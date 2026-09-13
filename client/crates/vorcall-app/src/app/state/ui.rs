//! What the window is showing on top of the chat: the route, the overlays and
//! the pointer.
//!
//! None of this is persisted except the two mirrors the configuration owns —
//! `collapsed` and `show_members`, which are written back as they change.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::{Point, Size};
use vorcall_core::connection::Blob;
use vorcall_core::images::ImagePurpose;
use vorcall_core::permissions;
use vorcall_core::{ChannelKind, Config};
use vorcall_screen::{Source, SourceId};

use crate::app::MainState;
use crate::app::message::{DragItem, DragSlot, MenuTarget, Message, ToastKind};
use crate::app::state::crop::{CropDrag, CropState};
use crate::app::state::settings::{ServerTab, SettingsTab};

/// How long a toast stays up, and how many are stacked at once.
pub const TOAST_LIFE: Duration = Duration::from_secs(5);
pub const TOAST_STACK: usize = 3;

/// How far a popover stays from the window's edges.
pub const POPOVER_MARGIN: f32 = 8.0;

/// How many matches the quick switcher offers.
pub const SWITCHER_MAX: usize = 10;

/// The name and long-text grammars of `PROTOCOL.md` § Limits, counted in Unicode
/// scalars after trimming. Mirrored here so a refusal is said in the dialog
/// rather than travelling to the server and back.
pub const NAME_MAX: usize = 32;
pub const LONG_MAX: usize = 256;

/// Which page the window is on. Settings and server settings take the whole
/// area to the right of the rail; the chat underneath is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Main,
    Settings(SettingsTab),
    ServerSettings(ServerTab),
}

impl Route {
    pub fn is_main(self) -> bool {
        matches!(self, Self::Main)
    }
}

#[derive(Debug)]
pub struct UiState {
    pub route: Route,
    pub dialog: Option<Dialog>,
    pub context_menu: Option<ContextMenu>,
    pub profile_card: Option<ProfileCard>,
    pub quick_switcher: QuickSwitcher,
    pub toasts: VecDeque<Toast>,
    /// The handle the next toast is raised under, so a dismissal names one.
    pub next_toast: u64,
    pub cursor: Point,
    pub window_size: Size,
    pub focused: bool,
    pub drag: Option<DragState>,
    /// Mirrors `Config::collapsed_categories`.
    pub collapsed: BTreeSet<i64>,
    /// Mirrors `Config::show_members`.
    pub show_members: bool,
}

impl UiState {
    /// The state a window starts in, with the preferences it mirrors already in
    /// place.
    pub fn new(config: &Config) -> Self {
        Self {
            route: Route::Main,
            dialog: None,
            context_menu: None,
            profile_card: None,
            quick_switcher: QuickSwitcher::default(),
            toasts: VecDeque::new(),
            next_toast: 0,
            cursor: Point::ORIGIN,
            window_size: Size::ZERO,
            focused: false,
            drag: None,
            collapsed: config.collapsed_categories.clone(),
            show_members: config.show_members,
        }
    }

    /// Raises one toast, pushing the oldest out once the stack is full.
    pub fn toast(&mut self, kind: ToastKind, text: String, now: Instant) {
        self.next_toast = self.next_toast.wrapping_add(1);
        self.toasts.push_back(Toast {
            id: self.next_toast,
            kind,
            text,
            until: now + TOAST_LIFE,
        });
        while self.toasts.len() > TOAST_STACK {
            self.toasts.pop_front();
        }
    }

    /// Drops every toast whose time is up.
    pub fn expire_toasts(&mut self, now: Instant) {
        self.toasts.retain(|toast| toast.until > now);
    }

    /// Where a popover of `size` is drawn so that all of it stays inside the
    /// window. A window whose size is not known yet — nothing has resized it —
    /// clamps nothing: a menu at the pointer beats one in the corner.
    pub fn clamp_popover(&self, at: Point, size: Size) -> Point {
        let limit = |edge: f32, extent: f32, window: f32| {
            let furthest = window - extent - POPOVER_MARGIN;
            if furthest > POPOVER_MARGIN {
                edge.clamp(POPOVER_MARGIN, furthest)
            } else {
                edge.max(0.0)
            }
        };
        Point::new(
            limit(at.x, size.width, self.window_size.width),
            limit(at.y, size.height, self.window_size.height),
        )
    }
}

/// One modal. Whatever is being typed into it lives here: a dialog is closed by
/// dropping it, and nothing it held survives.
#[derive(Clone)]
pub enum Dialog {
    ChangePassword {
        current: String,
        new: String,
        confirm: String,
        error: Option<String>,
        busy: bool,
    },
    CreateChannel {
        category_id: Option<i64>,
        kind: ChannelKind,
        name: String,
        error: Option<String>,
    },
    CreateCategory {
        name: String,
        error: Option<String>,
    },
    EditChannel {
        channel_id: i64,
        name: String,
        topic: String,
    },
    EditCategory {
        id: i64,
        name: String,
    },
    ConfirmDeleteChannel {
        channel_id: i64,
    },
    ConfirmDeleteCategory {
        id: i64,
    },
    ConfirmDeleteRole {
        role_id: i64,
    },
    ConfirmDeleteMessage {
        message_id: i64,
    },
    ConfirmKick {
        user_id: i64,
    },
    /// Handing the server over, which the owner cannot take back.
    ConfirmTransferOwnership {
        user_id: i64,
    },
    BanReason {
        user_id: i64,
        reason: String,
    },
    /// Somebody's nickname: theirs, or one's own.
    Nickname {
        user_id: i64,
        draft: String,
    },
    /// One attachment at full size.
    Image(i64),
    /// One file coming down onto the disk. Unlike every other dialog this one is
    /// not a form: a transfer can run for hours, so the bytes are counted into
    /// it in place through [`crate::app::App::dialog_mut`] rather than by
    /// reopening it on every report.
    Transfer {
        source: TransferSource,
        /// The handle the transfer was started under, which is what a cancel
        /// names and what every progress report carries back.
        request_id: u64,
        file_name: String,
        received: u64,
        /// What the record says the file is; zero when nothing said.
        total: u64,
        state: TransferState,
    },
    /// The crop adjuster a picked picture goes through before it is uploaded.
    CropImage {
        purpose: ImagePurpose,
        /// The file exactly as it was picked: the crop is cut out of these
        /// bytes, never out of the preview.
        bytes: Blob,
        handle: iced::widget::image::Handle,
        /// The preview's size, which is the source's own — the rectangle the
        /// frame is scissored with is in these pixels.
        source: (u32, u32),
        crop: CropState,
        /// The pan in flight, while the pointer is down on the frame.
        drag: Option<CropDrag>,
    },
    /// What to share, before any capture starts. A system whose own picker
    /// chooses the source has nothing to list here.
    SharePicker {
        sources: SourcesState,
        selected: Option<SourceId>,
        audio: bool,
    },
    ThemeSaveAs {
        name: String,
    },
    /// An invite code, shown once: the server never sends it again.
    InviteCreated {
        code: String,
    },
    /// The offer made once a run when the last one left a crash report behind.
    /// Answering it either way is what puts it away.
    CrashReport,
    /// Not a modal: what a control in an overlay asks for. `UiMsg::OpenDialog` is
    /// the only message that carries a [`Dialog`] into `update`, and `UiMsg` has
    /// no submit or clipboard of its own, so those travel as one of these.
    /// `update::ui` performs it and never stores it.
    Action(DialogAction),
}

impl fmt::Debug for Dialog {
    /// A password and a fresh invite code are credentials; the dialog they are
    /// being typed into is allowed in a log line, their contents are not.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChangePassword { busy, .. } => f
                .debug_struct("ChangePassword")
                .field("busy", busy)
                .finish_non_exhaustive(),
            Self::CreateChannel {
                category_id, kind, ..
            } => f
                .debug_struct("CreateChannel")
                .field("category_id", category_id)
                .field("kind", kind)
                .finish_non_exhaustive(),
            Self::CreateCategory { .. } => f.write_str("CreateCategory"),
            Self::EditChannel { channel_id, .. } => f
                .debug_struct("EditChannel")
                .field("channel_id", channel_id)
                .finish_non_exhaustive(),
            Self::EditCategory { id, .. } => f
                .debug_struct("EditCategory")
                .field("id", id)
                .finish_non_exhaustive(),
            Self::ConfirmDeleteChannel { channel_id } => f
                .debug_struct("ConfirmDeleteChannel")
                .field("channel_id", channel_id)
                .finish(),
            Self::ConfirmDeleteCategory { id } => f
                .debug_struct("ConfirmDeleteCategory")
                .field("id", id)
                .finish(),
            Self::ConfirmDeleteRole { role_id } => f
                .debug_struct("ConfirmDeleteRole")
                .field("role_id", role_id)
                .finish(),
            Self::ConfirmDeleteMessage { message_id } => f
                .debug_struct("ConfirmDeleteMessage")
                .field("message_id", message_id)
                .finish(),
            Self::ConfirmKick { user_id } => f
                .debug_struct("ConfirmKick")
                .field("user_id", user_id)
                .finish(),
            Self::ConfirmTransferOwnership { user_id } => f
                .debug_struct("ConfirmTransferOwnership")
                .field("user_id", user_id)
                .finish(),
            Self::BanReason { user_id, .. } => f
                .debug_struct("BanReason")
                .field("user_id", user_id)
                .finish_non_exhaustive(),
            Self::Nickname { user_id, .. } => f
                .debug_struct("Nickname")
                .field("user_id", user_id)
                .finish_non_exhaustive(),
            Self::Image(id) => f.debug_tuple("Image").field(id).finish(),
            Self::Transfer {
                source,
                request_id,
                received,
                total,
                state,
                ..
            } => f
                .debug_struct("Transfer")
                .field("source", source)
                .field("request_id", request_id)
                .field("received", received)
                .field("total", total)
                .field("state", state)
                .finish_non_exhaustive(),
            // A picked picture is somebody's own: its size, never its pixels.
            Self::CropImage {
                purpose,
                bytes,
                source,
                crop,
                ..
            } => f
                .debug_struct("CropImage")
                .field("purpose", purpose)
                .field("bytes", &bytes.len())
                .field("source", source)
                .field("crop", crop)
                .finish_non_exhaustive(),
            Self::SharePicker {
                selected, audio, ..
            } => f
                .debug_struct("SharePicker")
                .field("selected", selected)
                .field("audio", audio)
                .finish_non_exhaustive(),
            Self::ThemeSaveAs { name } => {
                f.debug_struct("ThemeSaveAs").field("name", name).finish()
            }
            Self::InviteCreated { .. } => f.write_str("InviteCreated { code: <redacted> }"),
            Self::CrashReport => f.write_str("CrashReport"),
            Self::Action(action) => f.debug_tuple("Action").field(action).finish(),
        }
    }
}

/// Which of the two kinds of file a transfer is moving. They are fetched over
/// different endpoints and fail in different ways, so the id alone would not
/// say what to ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferSource {
    Attachment(i64),
    Stream(i64),
}

/// How far a transfer has got. Both terminal states are held rather than closing
/// the dialog: a download that took an hour is worth saying something about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    Running,
    /// On the disk, at this path.
    Done(PathBuf),
    /// Already a sentence a person can read.
    Failed(String),
}

/// What a control in an overlay asks `update::ui` to do. None of these is a
/// state: each is performed once and dropped.
#[derive(Clone)]
pub enum DialogAction {
    /// Submit the dialog that is open; its own fields say what that means.
    Submit,
    /// Put one value on the clipboard, saying what was copied.
    Copy { what: &'static str, value: String },
    /// Close the menu or the card the control belongs to, then send this.
    Perform(Box<Message>),
}

impl fmt::Debug for DialogAction {
    /// What is being copied can be an invite code; the label is all a log line
    /// gets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit => f.write_str("Submit"),
            Self::Copy { what, .. } => write!(f, "Copy({what})"),
            Self::Perform(message) => write!(f, "Perform({message:?})"),
        }
    }
}

/// What the share picker knows about this machine's screens and windows.
#[derive(Debug, Clone)]
pub enum SourcesState {
    Loading,
    Ready(Vec<Source>),
    Failed(String),
}

/// A right-click menu, anchored where the pointer was.
#[derive(Debug, Clone, Copy)]
pub struct ContextMenu {
    pub target: MenuTarget,
    pub at: Point,
}

/// Somebody's profile, opened by clicking their name.
#[derive(Debug, Clone, Copy)]
pub struct ProfileCard {
    pub user_id: i64,
    pub at: Point,
}

#[derive(Debug, Clone, Default)]
pub struct QuickSwitcher {
    pub open: bool,
    pub query: String,
    /// Which match the keyboard is on.
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub text: String,
    pub until: Instant,
}

/// A drag in flight. `slot` is where the list would put it if the pointer were
/// released now, which is also what the live reorder draws.
#[derive(Debug, Clone, Copy)]
pub struct DragState {
    pub item: DragItem,
    pub slot: Option<DragSlot>,
    pub at: Point,
}

/// What one quick-switcher row opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchTarget {
    Channel(i64),
    Voice(i64),
    Dm(i64),
    /// Somebody with no conversation open yet: picking them starts one.
    Member(i64),
}

/// One candidate the quick switcher ranks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchEntry {
    pub target: SwitchTarget,
    /// What the query is matched against, and what the row shows.
    pub name: String,
    /// The line beside it: the category, "voice · 2 inside", "direct message".
    pub detail: String,
    pub unread: u32,
}

/// How well one name answers a query. A prefix is what the typist most likely
/// meant, a word start next, a scattered subsequence last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Subsequence,
    WordStart,
    Prefix,
}

/// Everything the quick switcher can open: the text and voice channels this
/// account may see, the conversations still in the list, and the members there is
/// no conversation with yet.
pub fn switcher_entries(main: &MainState, hidden: &BTreeSet<i64>) -> Vec<SwitchEntry> {
    let mut entries = Vec::new();
    let category_of = |category_id: i64| -> String {
        main.server
            .categories
            .get(&category_id)
            .map(|category| category.name.clone())
            .unwrap_or_default()
    };

    for channel in main.server.channels_of_kind(ChannelKind::Text) {
        if !main.server.can(permissions::VIEW_CHANNEL, Some(channel.id)) {
            continue;
        }
        entries.push(SwitchEntry {
            target: SwitchTarget::Channel(channel.id),
            name: channel.name.clone(),
            detail: category_of(channel.category_id),
            unread: main.chat.channel(channel.id).map_or(0, |ui| ui.unread),
        });
    }

    for channel in main.server.channels_of_kind(ChannelKind::Voice) {
        if !main.server.can(permissions::VIEW_CHANNEL, Some(channel.id)) {
            continue;
        }
        let inside = main
            .voice
            .rosters
            .get(&channel.id)
            .map_or(0, |roster| roster.members.len());
        let mut detail = vec![category_of(channel.category_id), "voice".to_owned()];
        if inside > 0 {
            detail.push(format!("{inside} inside"));
        }
        detail.retain(|part| !part.is_empty());
        entries.push(SwitchEntry {
            target: SwitchTarget::Voice(channel.id),
            name: channel.name.clone(),
            detail: detail.join(" · "),
            unread: 0,
        });
    }

    // Whoever there is already a conversation with is offered as that
    // conversation, never twice.
    let mut in_conversation = BTreeSet::new();
    for channel in main.server.dms() {
        if hidden.contains(&channel.id) {
            continue;
        }
        let Some(partner) = main.server.dm_partner(channel) else {
            continue;
        };
        in_conversation.insert(partner);
        entries.push(SwitchEntry {
            target: SwitchTarget::Dm(channel.id),
            name: main.server.display_name(partner).to_owned(),
            detail: "direct message".to_owned(),
            unread: main.chat.channel(channel.id).map_or(0, |ui| ui.unread),
        });
    }

    for user_id in main.server.members.keys().copied() {
        if user_id == main.member_id || in_conversation.contains(&user_id) {
            continue;
        }
        entries.push(SwitchEntry {
            target: SwitchTarget::Member(user_id),
            name: main.server.display_name(user_id).to_owned(),
            detail: "open a direct message".to_owned(),
            unread: 0,
        });
    }

    entries
}

/// The matches for one query, best first, at most [`SWITCHER_MAX`]. Ties are
/// broken by what is unread and then by name, so the list never reorders itself
/// between two keystrokes that match equally well.
pub fn rank_switcher(entries: Vec<SwitchEntry>, query: &str) -> Vec<SwitchEntry> {
    let needle = query.trim().to_lowercase();
    let mut scored: Vec<(Rank, String, SwitchEntry)> = entries
        .into_iter()
        .filter_map(|entry| {
            let folded = entry.name.to_lowercase();
            let rank = rank_of(&folded, &needle)?;
            Some((rank, folded, entry))
        })
        .collect();

    scored.sort_by(
        |(left_rank, left_name, left), (right_rank, right_name, right)| {
            right_rank
                .cmp(left_rank)
                .then(right.unread.cmp(&left.unread))
                .then(left_name.cmp(right_name))
        },
    );
    scored
        .into_iter()
        .take(SWITCHER_MAX)
        .map(|(_, _, entry)| entry)
        .collect()
}

/// How well `folded` — a name already lowercased — answers `needle`, which is
/// lowercased too. `None` is no match at all.
fn rank_of(folded: &str, needle: &str) -> Option<Rank> {
    if needle.is_empty() {
        return Some(Rank::Subsequence);
    }
    if folded.starts_with(needle) {
        return Some(Rank::Prefix);
    }
    if folded
        .split(|letter: char| !letter.is_alphanumeric())
        .any(|word| word.starts_with(needle))
    {
        return Some(Rank::WordStart);
    }
    subsequence(folded, needle).then_some(Rank::Subsequence)
}

/// Whether every character of `needle` appears in `folded`, in order.
fn subsequence(folded: &str, needle: &str) -> bool {
    let mut haystack = folded.chars();
    needle
        .chars()
        .all(|wanted| haystack.any(|letter| letter == wanted))
}

/// One step through a list that wraps at both ends. An empty list stays at zero.
pub fn wrap_index(selected: usize, delta: i32, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as i64;
    let at = (selected as i64).min(count - 1);
    (at + i64::from(delta)).rem_euclid(count) as usize
}

/// A name as the server takes one: trimmed, 1 to [`NAME_MAX`] scalars, no
/// control characters. `what` is what the dialog calls the field it refuses.
pub fn validate_name(raw: &str, what: &str) -> Result<String, String> {
    let name = raw.trim();
    let length = name.chars().count();
    if length == 0 {
        return Err(format!("Enter a {what}."));
    }
    if length > NAME_MAX {
        return Err(format!("At most {NAME_MAX} characters."));
    }
    if name.chars().any(char::is_control) {
        return Err("No control characters.".to_owned());
    }
    Ok(name.to_owned())
}

/// A topic, a description or a ban reason: trimmed, up to [`LONG_MAX`] scalars,
/// no control characters. Empty is a value, not a refusal.
pub fn validate_long(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.chars().count() > LONG_MAX {
        return Err(format!("At most {LONG_MAX} characters."));
    }
    if text.chars().any(char::is_control) {
        return Err("No control characters.".to_owned());
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> UiState {
        UiState::new(&Config::default())
    }

    fn entry(name: &str, unread: u32) -> SwitchEntry {
        SwitchEntry {
            target: SwitchTarget::Channel(1),
            name: name.to_owned(),
            detail: String::new(),
            unread,
        }
    }

    #[test]
    fn a_toast_stack_keeps_the_newest_three() {
        let mut ui = state();
        let now = Instant::now();

        for index in 0..5 {
            ui.toast(ToastKind::Info, format!("note {index}"), now);
        }

        assert_eq!(ui.toasts.len(), TOAST_STACK);
        let texts: Vec<&str> = ui.toasts.iter().map(|toast| toast.text.as_str()).collect();
        assert_eq!(texts, ["note 2", "note 3", "note 4"]);
        // Every toast has its own handle, so a dismissal names one of them.
        assert_eq!(ui.toasts.front().map(|toast| toast.id), Some(3));
    }

    #[test]
    fn a_toast_is_gone_once_its_time_is_up() {
        let mut ui = state();
        let now = Instant::now();
        ui.toast(ToastKind::Error, "broken".to_owned(), now);

        ui.expire_toasts(now + TOAST_LIFE - Duration::from_millis(1));
        assert_eq!(ui.toasts.len(), 1);
        ui.expire_toasts(now + TOAST_LIFE);
        assert!(ui.toasts.is_empty());
    }

    #[test]
    fn a_popover_stays_inside_the_window() {
        let mut ui = state();
        ui.window_size = Size::new(1000.0, 700.0);
        let menu = Size::new(200.0, 300.0);

        // Far enough from both edges: where the pointer was.
        assert_eq!(
            ui.clamp_popover(Point::new(100.0, 120.0), menu),
            Point::new(100.0, 120.0)
        );
        // Past the right and bottom edges: pulled back by the margin.
        assert_eq!(
            ui.clamp_popover(Point::new(950.0, 650.0), menu),
            Point::new(792.0, 392.0)
        );
        // Against the top left corner: the margin is the nearest it gets.
        assert_eq!(
            ui.clamp_popover(Point::new(1.0, 2.0), menu),
            Point::new(8.0, 8.0)
        );
    }

    /// The first frame is drawn before any resize event, so the size can be zero.
    #[test]
    fn a_window_of_unknown_size_clamps_nothing() {
        let ui = state();

        assert_eq!(
            ui.clamp_popover(Point::new(400.0, 300.0), Size::new(200.0, 300.0)),
            Point::new(400.0, 300.0)
        );
    }

    #[test]
    fn a_prefix_beats_a_word_start_beats_a_scattered_match() {
        let matches = rank_switcher(
            vec![entry("lava", 0), entry("new va", 0), entry("valorant", 0)],
            "va",
        );

        let names: Vec<&str> = matches.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["valorant", "new va", "lava"]);
    }

    #[test]
    fn a_name_that_does_not_answer_the_query_is_left_out() {
        let matches = rank_switcher(vec![entry("builds", 0), entry("valorant", 0)], "val");

        let names: Vec<&str> = matches.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["valorant"]);
    }

    #[test]
    fn matching_equally_well_is_broken_by_unread_then_by_name() {
        let matches = rank_switcher(
            vec![entry("valley", 0), entry("valorant", 0), entry("value", 2)],
            "val",
        );

        let names: Vec<&str> = matches.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["value", "valley", "valorant"]);
    }

    #[test]
    fn the_query_and_the_name_are_matched_case_blind() {
        let matches = rank_switcher(vec![entry("Valorant", 0)], "VAL");

        assert_eq!(matches.len(), 1);
    }

    /// An empty query offers everything there is room for.
    #[test]
    fn nothing_typed_offers_the_first_ten() {
        let entries: Vec<SwitchEntry> = (0..14)
            .map(|index| entry(&format!("c{index:02}"), 0))
            .collect();

        let matches = rank_switcher(entries, "");

        assert_eq!(matches.len(), SWITCHER_MAX);
        assert_eq!(matches[0].name, "c00");
    }

    #[test]
    fn the_selection_wraps_at_both_ends() {
        assert_eq!(wrap_index(0, 1, 3), 1);
        assert_eq!(wrap_index(2, 1, 3), 0);
        assert_eq!(wrap_index(0, -1, 3), 2);
        // A selection left over from a longer list lands on the last row.
        assert_eq!(wrap_index(9, 1, 3), 0);
        assert_eq!(wrap_index(4, 0, 0), 0);
    }

    #[test]
    fn a_name_is_trimmed_and_capped_the_way_the_server_does_it() {
        assert_eq!(
            validate_name("  general  ", "name"),
            Ok("general".to_owned())
        );
        assert!(validate_name("   ", "name").is_err());
        assert!(validate_name(&"a".repeat(NAME_MAX), "name").is_ok());
        assert!(validate_name(&"a".repeat(NAME_MAX + 1), "name").is_err());
        assert!(validate_name("new\nline", "name").is_err());
    }

    #[test]
    fn a_topic_may_be_empty_but_not_endless() {
        assert_eq!(
            validate_long("  what we play  "),
            Ok("what we play".to_owned())
        );
        assert_eq!(validate_long("   "), Ok(String::new()));
        assert!(validate_long(&"a".repeat(LONG_MAX)).is_ok());
        assert!(validate_long(&"a".repeat(LONG_MAX + 1)).is_err());
        assert!(validate_long("tab\there").is_err());
    }
}
