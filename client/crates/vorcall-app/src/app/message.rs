//! Every message the window can produce, nested by the area that handles it.
//!
//! One variant per interaction: `app::update` dispatches the outer enum to the
//! module that owns the area, and that module matches its own enum
//! exhaustively, so a variant added here has to be answered somewhere.
//!
//! `Message` is `Clone` because iced clones it, and `Debug` because a log line
//! is allowed to name it — which is why the payloads that are secrets
//! (passwords, invite codes) or bulk (image bytes, pictures) carry a `Debug` of
//! their own that prints neither.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use iced::widget::{scrollable, text_editor};
use iced::{Point, Size, keyboard, mouse, window};
use vorcall_core::config::{Density, Entrance, TransmitMode};
use vorcall_core::connection::Blob;
use vorcall_core::images::ImagePurpose;
use vorcall_core::update;
use vorcall_core::{ApiFailure, ChannelKind, Session};
use vorcall_hotkey::Edge;
use vorcall_screen::{Source, SourceId};

use crate::app::state::settings::{ServerTab, SettingsTab};
use crate::app::state::ui::Dialog;
use crate::app::state::voice::{EngineHandoff, HotkeyHandoff};
use crate::workers::share::{ShareEvent, StageEvent};
use crate::workers::voice::{AudioEvent, DeviceLists};

pub use crate::workers::images::ImageKey;

#[derive(Debug, Clone)]
pub enum Message {
    Auth(AuthMsg),
    /// Everything the connection loop reports.
    Conn(vorcall_core::Event),
    Chat(ChatMsg),
    Channels(ChannelsMsg),
    Voice(VoiceMsg),
    Share(ShareMsg),
    Settings(SettingsMsg),
    Admin(AdminMsg),
    Ui(UiMsg),
    Keys(KeyMsg),
    Update(UpdateMsg),
    Window(WindowMsg),
    Tick(TickMsg),
    /// The answer to a task whose result nothing reads.
    Noop,
}

/// Signing in, signing out and changing a password.
#[derive(Clone)]
pub enum AuthMsg {
    UsernameChanged(String),
    PasswordChanged(String),
    ConfirmChanged(String),
    InviteChanged(String),
    ShowLogin,
    ShowRegister,
    LoginSubmit,
    RegisterSubmit,
    /// Registration and sign-in end the same way: a session or a failure.
    LoginResult(Result<Session, ApiFailure>),
    Logout,
    ChangePasswordSubmit,
    ChangePasswordResult(Result<(), ApiFailure>),
    DialogCurrentChanged(String),
    DialogNewChanged(String),
    DialogConfirmChanged(String),
}

impl fmt::Debug for AuthMsg {
    /// A password and an invite code are credentials: the variant's name is all
    /// a log line gets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UsernameChanged(_) => f.write_str("UsernameChanged(<hidden>)"),
            Self::PasswordChanged(_) => f.write_str("PasswordChanged(<redacted>)"),
            Self::ConfirmChanged(_) => f.write_str("ConfirmChanged(<redacted>)"),
            Self::InviteChanged(_) => f.write_str("InviteChanged(<redacted>)"),
            Self::ShowLogin => f.write_str("ShowLogin"),
            Self::ShowRegister => f.write_str("ShowRegister"),
            Self::LoginSubmit => f.write_str("LoginSubmit"),
            Self::RegisterSubmit => f.write_str("RegisterSubmit"),
            Self::LoginResult(result) => match result {
                Ok(_) => f.write_str("LoginResult(Ok)"),
                Err(failure) => write!(f, "LoginResult(Err({failure:?}))"),
            },
            Self::Logout => f.write_str("Logout"),
            Self::ChangePasswordSubmit => f.write_str("ChangePasswordSubmit"),
            Self::ChangePasswordResult(result) => match result {
                Ok(()) => f.write_str("ChangePasswordResult(Ok)"),
                Err(failure) => write!(f, "ChangePasswordResult(Err({failure:?}))"),
            },
            Self::DialogCurrentChanged(_) => f.write_str("DialogCurrentChanged(<redacted>)"),
            Self::DialogNewChanged(_) => f.write_str("DialogNewChanged(<redacted>)"),
            Self::DialogConfirmChanged(_) => f.write_str("DialogConfirmChanged(<redacted>)"),
        }
    }
}

/// The message list and the composer.
#[derive(Debug, Clone)]
pub enum ChatMsg {
    Editor(text_editor::Action),
    Send,
    LoadOlder,
    Scrolled(scrollable::Viewport),
    JumpToLatest,
    /// The row the pointer entered, and the one it left. Two messages rather
    /// than one option: the enter of the next row arrives before the exit of the
    /// previous one.
    Hover(i64),
    Unhover(i64),
    ReplyTo(i64),
    CancelReply,
    StartEdit(i64),
    CancelEdit,
    Delete(i64),
    ConfirmDelete(i64),
    CancelDelete,
    /// `None` closes the palette; [`crate::app::state::chat::COMPOSER_PALETTE`] is
    /// the composer's own.
    OpenReactions(Option<i64>),
    React(i64, &'static str),
    /// One emoji of the palette, into the composer at the caret.
    InsertEmoji(&'static str),
    MentionPick(String),
    PickAttachment,
    /// Whatever the file dialog answered, or the files dropped on the window.
    FilesPicked(Vec<PathBuf>),
    /// One file read off the disk: its name and its bytes.
    FileRead(Result<(String, Blob), String>),
    RemovePendingAttachment(i64),
    OpenImage(i64),
    /// The pixels of one cached image, ready to draw.
    ImageDecoded(ImageKey, Result<iced::widget::image::Handle, String>),
    CopyText(i64),
    MarkChannelRead(i64),
}

/// Picking what is in view: channels, categories, DMs.
#[derive(Debug, Clone)]
pub enum ChannelsMsg {
    Select(i64),
    OpenDm(i64),
    HideDm(i64),
    ShowServer,
    ShowDms,
    ToggleCategory(i64),
    PrevChannel,
    NextChannel,
    NextUnread,
    PrevUnread,
    /// `None` is the "no category" group at the bottom of the list.
    CreateChannelIn(Option<i64>),
    CreateCategory,
    RenameChannel(i64),
    DeleteChannel(i64),
    MuteChannel(i64),
    UnmuteChannel(i64),
}

/// The voice channel: joining, the microphone, the peers.
#[derive(Debug, Clone)]
pub enum VoiceMsg {
    Join(i64),
    Leave,
    ToggleMute,
    ToggleDeafen,
    MediaConnected(Result<EngineHandoff, String>),
    Audio(AudioEvent),
    SetTransmitMode(TransmitMode),
    SetVadThreshold(f32),
    /// The end of a drag, which is what writes the threshold to disk.
    VadThresholdReleased,
    SetNoiseSuppression(bool),
    SetEchoCancellation(bool),
    SetAutoGain(bool),
    SetPeerVolume(i64, f32),
    PeerVolumeReleased(i64),
    TogglePeerMute(i64),
    /// A press or release the system-wide listener saw, wherever the focus was.
    Hotkey {
        generation: u64,
        action: u32,
        edge: Edge,
    },
    HotkeyStarted(HotkeyHandoff),
    /// The listener's edge stream ended, which is the only sign a backend gives
    /// that it stopped on its own.
    HotkeyEnded {
        generation: u64,
    },
    RetryHotkey,
    DevicesListed(DeviceLists),
    SetInputDevice(String),
    SetOutputDevice(String),
    /// Moderating somebody else's voice session. Each flag travels only when it
    /// is being set; `move_to` of `Some(0)` disconnects them.
    Moderate {
        user_id: i64,
        muted: Option<bool>,
        deafened: Option<bool>,
        move_to: Option<i64>,
    },
}

/// Sharing a screen, and watching somebody else's.
#[derive(Debug, Clone)]
pub enum ShareMsg {
    OpenPicker,
    SourcesListed(Result<Vec<Source>, String>),
    PickSource(SourceId),
    SetPickerAudio(bool),
    Confirm,
    Stop,
    Event(ShareEvent),
    Watch(i64),
    StopWatching,
    /// What the decode thread reports. Never logged whole: a picture is
    /// somebody's screen.
    Stage(StageEvent),
    PopOut,
    PopIn,
    ToggleFullscreen,
    SetVolume(f32),
    VolumeReleased,
    SetResolution(String),
    SetFps(u32),
    SetBitrateAuto(bool),
    SetBitrate(u32),
    BitrateReleased,
    SetShareAudio(bool),
}

/// The user settings pages.
#[derive(Debug, Clone)]
pub enum SettingsMsg {
    Open(SettingsTab),
    Close,
    Tab(SettingsTab),
    ServerTab(ServerTab),
    SetNotifications(bool),
    SetSound(bool),
    SetSuppressEveryone(bool),
    SetTheme(String),
    SetDensity(Density),
    SetFontScale(f32),
    FontScaleReleased,
    SetEntrance(Entrance),
    SetTextReactions(bool),
    /// One token of the theme being edited: its name, and the hex typed for it.
    ThemeEditorToken(String, String),
    ThemeEditorName(String),
    ThemeSave,
    ThemeSaveAs,
    ThemeSaveAsConfirm,
    ThemeExport,
    ThemeImport,
    /// What the file dialog did with a theme: the path on success.
    ThemeFileResult(Result<String, String>),
    ProfileNickname(String),
    ProfileDescription(String),
    ProfileAccent(u32),
    ProfilePickAvatar,
    ProfilePickBanner,
    ProfileImagePicked(ImagePurpose, Result<(String, Blob), String>),
    ProfileClearAvatar,
    ProfileClearBanner,
    ProfileSave,
    ProfileReset,
    /// The action whose next key press is its new binding.
    KeybindCapture(String),
    /// That action, and the binding captured for it.
    KeybindCaptured(String, String),
    KeybindReset(String),
    KeybindCancel,
}

/// The server settings pages. Everything here needs a permission the mirror
/// checks before the control is even drawn.
#[derive(Debug, Clone)]
pub enum AdminMsg {
    OverviewName(String),
    OverviewDescription(String),
    OverviewPickIcon,
    OverviewIconPicked(Result<(String, Blob), String>),
    OverviewSave,
    TransferOwnership(i64),
    ChannelDraftName(String),
    ChannelDraftTopic(String),
    ChannelDraftCategory(Option<i64>),
    ChannelDraftKind(ChannelKind),
    ChannelSave,
    ChannelDelete(i64),
    CategoryDraftName(String),
    CategorySave,
    CategoryDelete(i64),
    /// Which role or member the override editor of that channel is editing.
    OverrideTarget(i64, OverrideTargetKind, i64),
    /// One permission bit of one override: channel, target, bit, new state.
    OverrideSet(i64, OverrideTargetKind, i64, u64, TriState),
    OverrideRemove(i64, OverrideTargetKind, i64),
    RoleSelect(i64),
    RoleDraftName(String),
    RoleDraftColor(u32),
    RoleDraftIcon(RoleIconDraft),
    RoleDraftHoist(bool),
    RoleDraftPermission(u64, bool),
    RoleSave,
    RoleCreate,
    RoleDelete(i64),
    RolePickIcon,
    RoleIconPicked(Result<(String, Blob), String>),
    MemberSearch(String),
    MemberAddRole(i64, i64),
    MemberRemoveRole(i64, i64),
    MemberNickname(i64, String),
    MemberNicknameSave(i64),
    Ban(i64),
    BanReason(String),
    BanConfirm,
    Unban(i64),
    InvitesRefresh,
    InviteDays(u32),
    InviteCreate,
    InviteRevoke(i64),
    BansRefresh,
    MoveUp(DragItem),
    MoveDown(DragItem),
    Drag(DragMsg),
}

/// Dragging a channel, a category or a role into place.
// The `Drag` prefix stays: these variants are only ever written through
// `Message::Admin(AdminMsg::Drag(DragMsg::DragStart(..)))`, never glob-imported.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum DragMsg {
    DragStart(DragItem),
    DragOver(DragSlot),
    DragEnd,
    DragCancel,
}

/// Everything about the window itself: overlays, the pointer, the panes.
#[derive(Debug, Clone)]
pub enum UiMsg {
    CursorMoved(Point),
    WindowResized(Size),
    ContextMenu(MenuTarget),
    CloseContextMenu,
    OpenProfileCard(i64),
    CloseProfileCard,
    /// Whose per-peer volume the voice rows have open; `None` closes it.
    ExpandMember(Option<i64>),
    ToggleMembers,
    OpenQuickSwitcher,
    QuickSwitcherQuery(String),
    /// One step down (`1`) or up (`-1`) the list of matches.
    QuickSwitcherMove(i32),
    QuickSwitcherPick,
    CloseQuickSwitcher,
    OpenDialog(Dialog),
    CloseDialog,
    Toast(ToastKind, String),
    DismissToast(u64),
    FocusNext,
    FocusPrevious,
    /// Unwinds whatever is in front, one layer per press.
    Escape,
}

/// Raw input, before the keybinding map has said what it means.
#[derive(Debug, Clone)]
pub enum KeyMsg {
    KeyDown(keyboard::Key, keyboard::Modifiers),
    KeyUp(keyboard::Key, keyboard::Modifiers),
    MouseDown(mouse::Button),
    MouseUp(mouse::Button),
    Focus(bool),
    FileDropped(PathBuf),
}

/// The self-updater.
#[derive(Debug, Clone)]
pub enum UpdateMsg {
    Check,
    Tick,
    Progress(update::Progress),
    /// [`update::Outcome`] is not `Clone` and every message is, so the answer
    /// travels behind an [`Arc`]; the failure is already a sentence.
    Result(Result<Arc<update::Outcome>, String>),
    Restart,
    Apply,
    Dismiss,
}

#[derive(Debug, Clone)]
pub enum WindowMsg {
    Closed(window::Id),
    /// One per frame while the splash is up.
    SplashTick(Instant),
    SplashSkip,
    /// One per frame while the loading creature is on screen.
    LoadingTick(Instant),
}

/// The clocks. Each one only runs while there is something for it to do.
#[derive(Debug, Clone, Copy)]
pub enum TickMsg {
    Voice,
    MarkRead,
    Toasts,
}

/// How loud a toast is. An error stays until it is dismissed or pushed out;
/// everything else is a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToastKind {
    Info,
    Error,
}

/// What a right-click was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTarget {
    /// The server header, which has no id of its own: there is one server.
    Server,
    Message(i64),
    Member(i64),
    Channel(i64),
    Category(i64),
}

/// Whether an override belongs to a role or to one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideTargetKind {
    Role,
    Member,
}

/// One permission in an override: inherited, allowed or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriState {
    Inherit,
    Allow,
    Deny,
}

/// What a role is marked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleIconDraft {
    None,
    Emoji(String),
    Image(i64),
}

/// What is being dragged in the server settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragItem {
    Channel(i64),
    Category(i64),
    Role(i64),
}

/// Where a drag would drop it. The list reorders live as the pointer enters a
/// row, so a slot is always "before this one" or "at the end of that group".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragSlot {
    BeforeChannel(i64),
    EndOfCategory(Option<i64>),
    BeforeCategory(i64),
    EndOfCategories,
    BeforeRole(i64),
    EndOfRoles,
}
