//! The settings and server-settings pages: which tab is open, and every form
//! being filled in.
//!
//! A draft is always a copy: nothing here is sent until the page's Save is
//! pressed, and Reset is a matter of rebuilding the draft from the model.

use std::collections::BTreeMap;
use std::fmt;

use vorcall_core::connection::RestKind;
use vorcall_core::images::ImagePurpose;
use vorcall_core::{Ban, ChannelKind, Config, Invite, Profile, Role, Server};
use vorcall_screen::CameraSource;

use crate::app::message::{OverrideTargetKind, RoleIconDraft};
use crate::theme::ThemeTokens;
use crate::theme::tokens;

/// How long a fresh invite lasts unless the page says otherwise.
pub const DEFAULT_INVITE_DAYS: u32 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Account,
    Profile,
    Voice,
    Notifications,
    Appearance,
    Keybinds,
}

impl SettingsTab {
    pub const ALL: [Self; 6] = [
        Self::Account,
        Self::Profile,
        Self::Voice,
        Self::Notifications,
        Self::Appearance,
        Self::Keybinds,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Profile => "Profile",
            Self::Voice => "Voice",
            Self::Notifications => "Notifications",
            Self::Appearance => "Appearance",
            Self::Keybinds => "Keybinds",
        }
    }
}

impl fmt::Display for SettingsTab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerTab {
    #[default]
    Overview,
    Channels,
    Roles,
    Members,
    Sounds,
    Stickers,
    Invites,
    Bans,
}

impl ServerTab {
    pub const ALL: [Self; 8] = [
        Self::Overview,
        Self::Channels,
        Self::Roles,
        Self::Members,
        Self::Sounds,
        Self::Stickers,
        Self::Invites,
        Self::Bans,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Channels => "Channels",
            Self::Roles => "Roles",
            Self::Members => "Members",
            Self::Sounds => "Sounds",
            Self::Stickers => "Stickers",
            Self::Invites => "Invites",
            Self::Bans => "Bans",
        }
    }

    /// The permissions that open the page, any one of which is enough. Without
    /// one of them the entry is not drawn at all.
    pub fn permissions(self) -> u64 {
        use vorcall_core::permissions as perms;
        match self {
            Self::Overview => perms::MANAGE_SERVER,
            Self::Channels => perms::MANAGE_CHANNELS,
            Self::Roles => perms::MANAGE_ROLES,
            // A member's roles are assigned from this page, so whoever manages
            // roles needs it as much as whoever manages members.
            Self::Members => perms::MANAGE_MEMBERS | perms::MANAGE_ROLES,
            Self::Sounds => perms::MANAGE_SOUNDS,
            Self::Stickers => perms::MANAGE_STICKERS,
            Self::Invites => perms::MANAGE_INVITES,
            Self::Bans => perms::BAN_MEMBERS,
        }
    }

    /// Whether a member whose server-wide permissions are `perms` may open the
    /// page.
    pub fn allowed(self, perms: u64) -> bool {
        perms & self.permissions() != 0
    }
}

impl fmt::Display for ServerTab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The user settings pages.
#[derive(Debug, Default)]
pub struct SettingsState {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Every camera this machine can name, read when the Voice page opens. Empty
    /// on a backend that does not enumerate them, which is a list with nothing
    /// to pick from rather than a machine without a camera.
    pub cameras: Vec<CameraSource>,
    pub profile: ProfileDraft,
    pub theme: ThemeDraft,
    /// Every theme the appearance page offers, read when the page opens: a card
    /// draws a theme's own colours, and a frame must not read the disk for them.
    pub themes: Vec<ThemeEntry>,
    /// The action whose next key press becomes its binding.
    pub capturing: Option<String>,
    /// The image uploads the profile page has out, by request id. An answer for
    /// an id that is not here belongs to another page.
    pub pending: BTreeMap<u64, ImagePurpose>,
    /// How far the problem report the account page offers has got.
    pub report: ReportState,
}

/// What the diagnostics section says about the report it last sent. `Sent` counts
/// the files that reached the server, which is what the reader can quote back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReportState {
    #[default]
    Idle,
    Sending,
    Sent(usize),
    Failed(String),
}

/// The profile page's draft, which is what Save sends.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProfileDraft {
    pub nickname: String,
    pub description: String,
    /// `0xRRGGBB`; 0 is "no accent of my own".
    pub accent_color: u32,
    pub avatar_image_id: i64,
    pub banner_image_id: i64,
}

impl ProfileDraft {
    /// The draft as the member's own profile reads today.
    pub fn from_profile(profile: &Profile) -> Self {
        Self {
            nickname: profile.nickname.clone(),
            description: profile.description.clone(),
            accent_color: profile.accent_color,
            avatar_image_id: profile.avatar_image_id,
            banner_image_id: profile.banner_image_id,
        }
    }

    /// Whether Save has anything to send.
    pub fn differs_from(&self, profile: &Profile) -> bool {
        *self != Self::from_profile(profile)
    }
}

/// What `UpdateProfile` carries for one image field: `0` keeps whatever the
/// server holds, `-1` clears it, anything else is the image to put there.
pub fn image_sentinel(draft: i64, current: i64) -> i64 {
    if draft == current {
        0
    } else if draft == 0 {
        -1
    } else {
        draft
    }
}

/// Every action caught in a binding clash, with the actions it clashes with, so
/// both rows of every pair can say so.
pub fn conflict_notes(config: &Config) -> BTreeMap<String, Vec<String>> {
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (first, second) in config.keybind_conflicts() {
        notes.entry(first.clone()).or_default().push(second.clone());
        notes.entry(second).or_default().push(first);
    }
    notes
}

/// One theme the appearance page lists.
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    /// The theme as `Config::theme` spells it: a preset's slug, or
    /// `custom:<slug>`.
    pub theme: String,
    pub name: String,
    /// What the card says it is: a preset, or the file it is stored as.
    pub detail: String,
    pub tokens: ThemeTokens,
}

/// The appearance page's theme editor. `text` holds what is typed per token, so
/// a half-written hex can sit in the field without repainting the window.
#[derive(Debug, Clone)]
pub struct ThemeDraft {
    /// The theme the editor started from, as `Config::theme` spells it.
    pub base: String,
    pub name: String,
    pub tokens: ThemeTokens,
    pub text: BTreeMap<String, String>,
    pub error: Option<String>,
}

impl ThemeDraft {
    /// An editor opened on one theme.
    pub fn new(base: String, name: String, tokens: ThemeTokens) -> Self {
        let text = tokens::TOKEN_NAMES
            .iter()
            .filter_map(|(_, name)| {
                let color = tokens.field(name)?;
                Some(((*name).to_owned(), tokens::to_hex(color)))
            })
            .collect();
        Self {
            base,
            name,
            tokens,
            text,
            error: None,
        }
    }

    /// What the field for one token shows.
    pub fn typed(&self, token: &str) -> &str {
        self.text.get(token).map_or("", String::as_str)
    }

    /// Types one token. The text is kept whatever it says, so a half-written hex
    /// can sit in the field; the colour behind it only moves when it parses.
    pub fn set_token(&mut self, token: &str, typed: String) {
        if let Ok(color) = tokens::parse_hex(&typed) {
            self.tokens.set_field(token, color);
        }
        self.text.insert(token.to_owned(), typed);
        self.error = self
            .text
            .iter()
            .find(|(_, typed)| tokens::parse_hex(typed.as_str()).is_err())
            .map(|(name, _)| format!("{name} is not a colour (#RRGGBB or #RRGGBBAA)"));
    }

    /// Whether one field shows something that is not a colour, which is what
    /// paints its outline red.
    pub fn invalid(&self, token: &str) -> bool {
        self.text
            .get(token)
            .is_some_and(|typed| tokens::parse_hex(typed).is_err())
    }

    /// The file this draft is saved as: the custom theme it was opened on, else
    /// the name typed for it.
    pub fn slug(&self) -> String {
        self.base
            .strip_prefix(crate::theme::file::CUSTOM_PREFIX)
            .map_or_else(|| crate::theme::file::slugify(&self.name), str::to_owned)
    }
}

impl Default for ThemeDraft {
    fn default() -> Self {
        Self::new(
            crate::theme::presets::DARK_SLUG.to_owned(),
            "Vorcall Dark".to_owned(),
            crate::theme::VORCALL_DARK,
        )
    }
}

/// The server settings pages.
#[derive(Debug, Default)]
pub struct AdminState {
    pub overview: OverviewDraft,
    pub channel: ChannelDraft,
    pub category: CategoryDraft,
    pub role: RoleDraft,
    pub selected_role: Option<i64>,
    /// Whose override the channel page is editing: channel, kind, target id.
    pub override_target: Option<(i64, OverrideTargetKind, i64)>,
    pub member_search: String,
    /// Nicknames being typed, by user id; an entry only exists while it is being
    /// edited.
    pub nicknames: BTreeMap<i64, String>,
    pub ban_reason: String,
    /// Clip names being typed on the sounds page, by clip id; an entry only
    /// exists while one is being edited.
    pub sound_names: BTreeMap<i64, String>,
    /// Sticker names being typed on the stickers page, by sticker id; an entry
    /// only exists while one is being edited.
    pub sticker_names: BTreeMap<i64, String>,
    pub invite_days: u32,
    pub invites: RestList<Invite>,
    pub bans: RestList<Ban>,
    /// The REST requests this page is waiting on, by request id.
    pub pending: BTreeMap<u64, RestKind>,
    /// The image uploads these pages have out, by request id.
    pub pending_images: BTreeMap<u64, ImagePurpose>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct OverviewDraft {
    pub name: String,
    pub description: String,
    pub icon_image_id: i64,
}

impl OverviewDraft {
    pub fn from_server(server: &Server) -> Self {
        Self {
            name: server.name.clone(),
            description: server.description.clone(),
            icon_image_id: server.icon_image_id,
        }
    }
}

/// The channel being created or edited. `id` of `None` is a creation.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDraft {
    pub id: Option<i64>,
    pub kind: ChannelKind,
    pub name: String,
    pub topic: String,
    pub category_id: Option<i64>,
}

impl Default for ChannelDraft {
    fn default() -> Self {
        Self {
            id: None,
            kind: ChannelKind::Text,
            name: String::new(),
            topic: String::new(),
            category_id: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CategoryDraft {
    pub id: Option<i64>,
    pub name: String,
}

/// The role being created or edited.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleDraft {
    pub id: Option<i64>,
    pub name: String,
    pub color: u32,
    pub icon: RoleIconDraft,
    pub hoist: bool,
    pub permissions: u64,
}

impl Default for RoleDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            color: 0,
            icon: RoleIconDraft::None,
            hoist: false,
            permissions: 0,
        }
    }
}

impl RoleDraft {
    /// The draft as one role reads today.
    pub fn from_role(role: &Role) -> Self {
        let icon = if role.icon_image_id != 0 {
            RoleIconDraft::Image(role.icon_image_id)
        } else if role.icon_emoji.is_empty() {
            RoleIconDraft::None
        } else {
            RoleIconDraft::Emoji(role.icon_emoji.clone())
        };
        Self {
            id: Some(role.id),
            name: role.name.clone(),
            color: role.color,
            icon,
            hoist: role.hoist,
            permissions: role.permissions,
        }
    }
}

/// One list the server settings fetch over REST.
#[derive(Debug, Clone, Default)]
pub enum RestList<T> {
    #[default]
    Idle,
    Loading,
    Ready(Vec<T>),
    Failed(String),
}

impl<T> RestList<T> {
    /// The rows, or nothing while there are none to draw.
    pub fn rows(&self) -> &[T] {
        match self {
            Self::Ready(rows) => rows,
            _ => &[],
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(detail) => Some(detail),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_draft_starts_with_every_token_spelled_out() {
        let draft = ThemeDraft::default();

        assert_eq!(draft.text.len(), tokens::TOKEN_NAMES.len());
        assert_eq!(draft.typed("accent"), "#C8102E");
        assert_eq!(draft.typed("not_a_token"), "");
    }

    #[test]
    fn a_role_draft_reads_the_icon_the_role_carries() {
        let mut role = Role {
            id: 4,
            name: "mods".to_owned(),
            color: 0x00_C8_10_2E,
            icon_emoji: String::new(),
            icon_image_id: 0,
            position: 2,
            permissions: 3,
            hoist: true,
            everyone: false,
        };
        assert_eq!(RoleDraft::from_role(&role).icon, RoleIconDraft::None);

        role.icon_emoji = "🛠".to_owned();
        assert_eq!(
            RoleDraft::from_role(&role).icon,
            RoleIconDraft::Emoji("🛠".to_owned())
        );

        // An image wins: the server keeps whichever was set last, and the image
        // is the richer of the two.
        role.icon_image_id = 9;
        assert_eq!(RoleDraft::from_role(&role).icon, RoleIconDraft::Image(9));
    }

    /// The dirty check is what enables Save, so it has to see each field.
    #[test]
    fn a_profile_draft_is_dirty_once_any_field_moves() {
        let profile = Profile {
            user_id: 7,
            username: "ana".to_owned(),
            nickname: "Ana".to_owned(),
            description: "Night owl.".to_owned(),
            accent_color: 0x00_5C_C7_75,
            avatar_image_id: 4,
            banner_image_id: 9,
            ..Profile::default()
        };

        let draft = ProfileDraft::from_profile(&profile);
        assert!(!draft.differs_from(&profile));

        let mutations: [fn(&mut ProfileDraft); 5] = [
            |draft| draft.nickname.push('!'),
            |draft| draft.description.clear(),
            |draft| draft.accent_color = 0,
            |draft| draft.avatar_image_id = 0,
            |draft| draft.banner_image_id = 11,
        ];
        for mutate in mutations {
            let mut moved = ProfileDraft::from_profile(&profile);
            mutate(&mut moved);
            assert!(moved.differs_from(&profile));
        }
    }

    /// `proto/vorcall.proto` § `UpdateProfile`: 0 keeps the stored image, -1
    /// clears it.
    #[test]
    fn an_unchanged_image_keeps_and_a_dropped_one_clears() {
        assert_eq!(image_sentinel(4, 4), 0);
        assert_eq!(image_sentinel(0, 0), 0);
        assert_eq!(image_sentinel(0, 4), -1);
        assert_eq!(image_sentinel(9, 4), 9);
        assert_eq!(image_sentinel(9, 0), 9);
    }

    #[test]
    fn the_theme_editor_keeps_a_half_written_hex_without_moving_the_colour() {
        let mut draft = ThemeDraft::default();
        let before = draft.tokens.accent;

        draft.set_token("accent", "#C81".to_owned());
        assert_eq!(draft.typed("accent"), "#C81");
        assert_eq!(draft.tokens.accent, before);
        assert!(draft.invalid("accent"));
        assert!(draft.error.is_some());

        draft.set_token("accent", "#00FF00".to_owned());
        assert_eq!(
            draft.tokens.accent,
            tokens::parse_hex("#00FF00").expect("a colour")
        );
        assert!(!draft.invalid("accent"));
        assert_eq!(draft.error, None);
    }

    /// Save writes the custom theme it was opened on; a preset's editor saves
    /// under the name typed for it instead.
    #[test]
    fn a_draft_saves_under_its_own_slug() {
        let mut draft = ThemeDraft::new(
            "custom:midnight-oil".to_owned(),
            "Renamed".to_owned(),
            crate::theme::VORCALL_DARK,
        );
        assert_eq!(draft.slug(), "midnight-oil");

        draft.base = crate::theme::presets::DARK_SLUG.to_owned();
        draft.name = "Night Shift".to_owned();
        assert_eq!(draft.slug(), "night-shift");
    }

    /// Both sides of a clash are warned, which is why the notes are keyed by
    /// action rather than listed as pairs.
    #[test]
    fn a_binding_clash_is_reported_on_both_rows() {
        let mut config = Config::default();
        assert!(conflict_notes(&config).is_empty());

        config.set_keybind("toggle_mute", "Ctrl+K");
        let notes = conflict_notes(&config);

        assert_eq!(
            notes.get("toggle_mute").map(Vec::as_slice),
            Some(["quick_switcher".to_owned()].as_slice())
        );
        assert_eq!(
            notes.get("quick_switcher").map(Vec::as_slice),
            Some(["toggle_mute".to_owned()].as_slice())
        );
        assert_eq!(notes.get("toggle_deafen"), None);
    }

    #[test]
    fn a_rest_list_has_no_rows_until_it_is_ready() {
        let mut list: RestList<Invite> = RestList::Idle;
        assert!(list.rows().is_empty());
        assert!(!list.is_loading());

        list = RestList::Loading;
        assert!(list.is_loading());

        list = RestList::Failed("no".to_owned());
        assert_eq!(list.error(), Some("no"));

        list = RestList::Ready(vec![Invite::default()]);
        assert_eq!(list.rows().len(), 1);
        assert_eq!(list.error(), None);
    }
}
