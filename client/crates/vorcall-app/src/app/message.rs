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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use iced::widget::{scrollable, text_editor};
use iced::{Point, Size, keyboard, mouse, window};
use vorcall_clipboard::Pasted;
use vorcall_core::config::{Density, Entrance, TransmitMode};
use vorcall_core::connection::Blob;
use vorcall_core::images::ImagePurpose;
use vorcall_core::update;
use vorcall_core::{ApiFailure, ChannelKind, Session, Sound, Sticker};
use vorcall_hotkey::Edge;
use vorcall_screen::preset::CameraResolution;
use vorcall_screen::{CameraSource, Source, SourceId};

use crate::app::state::settings::{ServerTab, SettingsTab};
use crate::app::state::sound::TrimEdge;
use crate::app::state::ui::{Dialog, TransferSource};
use crate::app::state::voice::{CameraTileId, EngineHandoff, HotkeyHandoff};
// `vorcall_screen::Source` is a screen to capture; this one is a decoded audio
// file waiting to be trimmed.
use crate::workers::camera::CameraEvent;
use crate::workers::clips::Source as ClipSource;
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
    /// This client's camera, and the ones it watches.
    Camera(CameraMsg),
    /// The interface motifs and the shared soundpad.
    Sound(SoundMsg),
    /// The shared sticker library.
    Sticker(StickerMsg),
    Settings(SettingsMsg),
    Admin(AdminMsg),
    /// Framing a picked picture before it is uploaded.
    Crop(CropMsg),
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
    /// Fold the sign-in screen's "Server" section open or shut.
    ServerToggle,
    ServerUrlChanged(String),
    ServerKeyChanged(String),
    /// Point this client at the typed server, saving it in `config.toml`.
    ServerSave,
    /// Forget the saved server and go back to the one the build carries.
    ServerReset,
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
            Self::ServerToggle => f.write_str("ServerToggle"),
            Self::ServerUrlChanged(_) => f.write_str("ServerUrlChanged(<hidden>)"),
            Self::ServerKeyChanged(_) => f.write_str("ServerKeyChanged(<redacted>)"),
            Self::ServerSave => f.write_str("ServerSave"),
            Self::ServerReset => f.write_str("ServerReset"),
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
    /// The word one row of the mention list writes after the `@`: a username,
    /// or `everyone` or `here`.
    MentionPick(String),
    /// One step through that list, wrapping at both ends.
    MentionMove(i32),
    /// Take whichever row is highlighted.
    MentionAccept,
    /// Put the list away. The next edit that leaves a fragment at the caret
    /// opens it again, so nothing remembers the dismissal.
    MentionDismiss,
    PickAttachment,
    /// Whatever the file dialog answered, or the files dropped on the window.
    FilesPicked(Vec<PathBuf>),
    /// One picked, dropped or pasted file, measured off the interface thread:
    /// its size is what decides whether it is stored on the server or offered
    /// from this disk, and `offer` is that choice already made by hand.
    FileMeasured {
        path: PathBuf,
        size: u64,
        content_type: String,
        offer: bool,
    },
    /// Bytes to send as a file, with the name they travel under: a pasted
    /// picture has no file of its own to stream from.
    FileRead(Result<(String, Blob), String>),
    RemovePendingAttachment(i64),
    /// One streamed file out of the message being written. The offer stands on
    /// the server either way: nothing was uploaded to take back.
    RemovePendingStream(i64),
    /// Give up on one upload or offer the composer is still waiting on, named by
    /// the request id it was started under.
    CancelUpload(u64),
    OpenImage(i64),
    /// The pixels of one cached image, ready to draw.
    ImageDecoded(ImageKey, Result<iced::widget::image::Handle, String>),
    /// The download button on one attachment or one streamed file: asks where
    /// to put it before a byte moves.
    SaveFile(TransferSource),
    /// Where that dialog said to put it, which is what starts the transfer;
    /// `None` is a dialog the user dismissed.
    SaveDestination(TransferSource, Option<PathBuf>),
    /// Give up on the download the transfer dialog is following, named by the
    /// request id it was started under.
    CancelTransfer(u64),
    /// How far the attachment being saved has come. A stored attachment has no
    /// download-to-disk command in the connection loop, so that one transfer
    /// runs in the window and reports here rather than as an `Event`.
    SaveProgress {
        request_id: u64,
        received: u64,
        total: u64,
    },
    /// Where it landed, or why it did not.
    SaveFinished {
        request_id: u64,
        result: Result<PathBuf, String>,
    },
    /// One range of a file this client offered, resolved against the registry:
    /// the local file is still the one that was offered, so it may be read.
    ServeStream {
        stream_id: i64,
        transfer_id: i64,
        offset: i64,
        length: i64,
        path: PathBuf,
    },
    CopyText(i64),
    /// Paste into the composer: files copied in a file manager, a screenshot, or
    /// text — whichever the clipboard is offering.
    Paste,
    /// What the clipboard held, or the sentence saying why it could not be read.
    /// [`Pasted`] prints itself summarised, so the derived `Debug` puts neither
    /// a pasted picture nor the pasted text in the log.
    PasteRead(Result<Pasted, String>),
    /// Ask for the files to offer as streamed ones, which is the chevron beside
    /// the paperclip rather than the paperclip itself.
    PickStream,
    /// Offer one local file as a streamed file: above the stored-attachment
    /// ceiling the server keeps only the record, and this client serves the
    /// bytes for as long as it is online.
    OfferFile(PathBuf),
    /// The press that starts selecting text inside one message, which takes the
    /// selection away from whatever row held it.
    StartSelection(i64),
    /// One `http`/`https` URL out of a message, handed to the system browser.
    OpenLink(String),
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
    SetPriorityDucking(bool),
    /// Whether somebody joining or leaving this client's voice channel is worth
    /// a motif, and whether its own two switches are.
    SetVoiceSounds(bool),
    SetSelfSounds(bool),
    SetSoundVolume(f32),
    SetSoundpadVolume(f32),
    /// The end of either volume drag, which is what writes it to disk.
    SoundVolumeReleased,
    SoundpadVolumeReleased,
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

/// This client's camera, and the ones it watches.
#[derive(Debug, Clone)]
pub enum CameraMsg {
    /// The camera switch in the voice card: on when it is off, off when it is
    /// on.
    Toggle,
    /// What the camera thread reports. Never carries a picture: the preview
    /// travels through the handle's mailbox.
    Event(CameraEvent),
    /// What one watched camera's decode thread reports.
    Tile(i64, StageEvent),
    Watch(i64),
    StopWatching(i64),
    /// Come off every camera at once, which is what the stage's close button
    /// does when there is no share on it.
    StopWatchingAll,
    /// A press on one tile: into the large picture, or back out of it.
    Feature(CameraTileId),
    /// Every camera this machine can name, listed off the interface thread when
    /// the Voice settings page opens.
    DevicesListed(Vec<CameraSource>),
    /// `None` is the system's default device.
    SetDevice(Option<CameraSource>),
    SetResolution(CameraResolution),
    SetFps(u32),
}

/// The shared soundpad: playing a clip, and the library behind it.
#[derive(Clone)]
pub enum SoundMsg {
    /// The popover over the voice bar, anchored where the pointer was.
    OpenPopover(Point),
    ClosePopover,
    /// Ask the server to play one clip into the joined channel. Nothing is
    /// heard until its `SoundPlayed` comes back.
    Play(i64),
    Stop,
    /// The bytes of one clip, decoded and ready for the mixer, or why they are
    /// not.
    Ready(i64, Result<Arc<Vec<f32>>, String>),
    /// Add a clip: the file dialog, off the interface thread.
    Pick,
    /// What the dialog picked. An empty error is a dismissal rather than a
    /// failure, the same convention the picture pickers use.
    Picked(Result<PathBuf, String>),
    /// The picked file, decoded into the samples the trim view draws over.
    Decoded(Result<(String, Arc<ClipSource>), String>),
    /// A press on one of the two edge handles.
    TrimStart(TrimEdge),
    /// Where the pointer is inside the trim frame, in its own pixels.
    TrimMove(f32),
    TrimEnd,
    /// Cut the selection out and upload it, off the interface thread.
    TrimApply,
    Uploaded(Result<Sound, String>),
    /// One clip's name as it is being typed on the sounds page.
    RenameDraft(i64, String),
    RenameSave(i64),
    Delete(i64),
}

impl fmt::Debug for SoundMsg {
    /// A decoded source and a decoded clip are megabytes of samples, and a
    /// debug file log always exists: how many, never which. A clip's name is
    /// somebody's text and never reaches the log either.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenPopover(at) => write!(f, "OpenPopover({at:?})"),
            Self::ClosePopover => f.write_str("ClosePopover"),
            Self::Play(id) => write!(f, "Play({id})"),
            Self::Stop => f.write_str("Stop"),
            Self::Ready(id, result) => match result {
                Ok(samples) => write!(f, "Ready({id}, {} samples)", samples.len()),
                Err(error) => write!(f, "Ready({id}, Err({error}))"),
            },
            Self::Pick => f.write_str("Pick"),
            Self::Picked(result) => match result {
                // The file's own name, never the path it sits at.
                Ok(path) => {
                    let name = Path::new(path.file_name().unwrap_or_default());
                    write!(f, "Picked({})", name.display())
                }
                Err(error) => write!(f, "Picked(Err({error}))"),
            },
            Self::Decoded(result) => match result {
                Ok((_, source)) => write!(
                    f,
                    "Decoded(<hidden>, {} ms, {} samples)",
                    source.duration_ms,
                    source.pcm.len()
                ),
                Err(error) => write!(f, "Decoded(Err({error}))"),
            },
            Self::TrimStart(edge) => write!(f, "TrimStart({edge:?})"),
            Self::TrimMove(at) => write!(f, "TrimMove({at})"),
            Self::TrimEnd => f.write_str("TrimEnd"),
            Self::TrimApply => f.write_str("TrimApply"),
            Self::Uploaded(result) => match result {
                Ok(sound) => write!(f, "Uploaded({}, {} bytes)", sound.id, sound.size),
                Err(error) => write!(f, "Uploaded(Err({error}))"),
            },
            Self::RenameDraft(id, _) => write!(f, "RenameDraft({id}, <hidden>)"),
            Self::RenameSave(id) => write!(f, "RenameSave({id})"),
            Self::Delete(id) => write!(f, "Delete({id})"),
        }
    }
}

/// The shared sticker library: picking one to send, and managing the library.
#[derive(Clone)]
pub enum StickerMsg {
    /// The composer's picker, which one press opens and the next closes.
    TogglePicker,
    ClosePicker,
    /// Send one sticker as a message of its own into the channel in view.
    Send(i64),
    /// Add a sticker: the file dialog, off the interface thread.
    PickFile,
    /// The picked file: its name, the type it travels as, and its bytes. An
    /// empty error is a dismissal rather than a failure, the same convention the
    /// picture pickers use.
    Picked(Result<(String, String, Blob), String>),
    Uploaded(Result<Sticker, String>),
    /// One sticker's name as it is being typed on the stickers page.
    RenameDraft(i64, String),
    RenameSave(i64),
    Delete(i64),
}

impl fmt::Debug for StickerMsg {
    /// A picked file is a picture and a sticker's name is somebody's text; a
    /// debug file log always exists, so neither ever reaches one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TogglePicker => f.write_str("TogglePicker"),
            Self::ClosePicker => f.write_str("ClosePicker"),
            Self::Send(id) => write!(f, "Send({id})"),
            Self::PickFile => f.write_str("PickFile"),
            Self::Picked(result) => match result {
                Ok((_, content_type, bytes)) => {
                    write!(f, "Picked(<hidden>, {content_type}, {} bytes)", bytes.len())
                }
                Err(error) => write!(f, "Picked(Err({error}))"),
            },
            Self::Uploaded(result) => match result {
                Ok(sticker) => write!(f, "Uploaded({}, {} bytes)", sticker.id, sticker.size),
                Err(error) => write!(f, "Uploaded(Err({error}))"),
            },
            Self::RenameDraft(id, _) => write!(f, "RenameDraft({id}, <hidden>)"),
            Self::RenameSave(id) => write!(f, "RenameSave({id})"),
            Self::Delete(id) => write!(f, "Delete({id})"),
        }
    }
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
    /// The diagnostics button that hands the log and every crash report over.
    ReportProblem,
    /// How many files reached the server, or why none did.
    ReportFinished(Result<usize, String>),
    /// The two answers to the offer made after a crash.
    SendCrashReport,
    DismissCrashReport,
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

/// The crop adjuster every picked picture goes through, whichever page picked
/// it.
#[derive(Clone)]
pub enum CropMsg {
    /// A file was read: decode it for the preview, off the UI thread.
    Open {
        purpose: ImagePurpose,
        bytes: Blob,
    },
    /// The preview, decoded in the source's own pixels: open the dialog.
    Ready {
        purpose: ImagePurpose,
        bytes: Blob,
        handle: iced::widget::image::Handle,
        source: (u32, u32),
    },
    /// The decode or the crop did not work out.
    Failed(String),
    /// A press inside the frame, which anchors the drag.
    PanStart,
    /// Where the pointer is inside the frame.
    PanMove(Point),
    PanEnd,
    Zoom(f32),
    /// Cut the crop out and scale it, off the UI thread.
    Apply,
    /// What the page that picked the file is to upload.
    Applied {
        purpose: ImagePurpose,
        content_type: &'static str,
        bytes: Blob,
    },
}

impl fmt::Debug for CropMsg {
    /// A picked picture is somebody's own and a debug file log always exists:
    /// the variant and a byte count are all a log line gets, never the bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { purpose, bytes } => {
                write!(f, "Open({purpose:?}, {} bytes)", bytes.len())
            }
            Self::Ready {
                purpose,
                bytes,
                source,
                ..
            } => write!(
                f,
                "Ready({purpose:?}, {} bytes, {} * {})",
                bytes.len(),
                source.0,
                source.1
            ),
            Self::Failed(error) => write!(f, "Failed({error})"),
            Self::PanStart => f.write_str("PanStart"),
            Self::PanMove(at) => write!(f, "PanMove({at:?})"),
            Self::PanEnd => f.write_str("PanEnd"),
            Self::Zoom(zoom) => write!(f, "Zoom({zoom})"),
            Self::Apply => f.write_str("Apply"),
            Self::Applied {
                purpose,
                content_type,
                bytes,
            } => write!(
                f,
                "Applied({purpose:?}, {content_type}, {} bytes)",
                bytes.len()
            ),
        }
    }
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
    /// The chevron beside the paperclip: the routes the one-click paperclip
    /// does not take.
    FileRoutes,
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
