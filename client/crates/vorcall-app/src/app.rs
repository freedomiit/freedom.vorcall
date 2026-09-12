//! The application state and everything that changes it.
//!
//! Networking stays in `vorcall-core`, reached through one iced subscription,
//! so the UI thread never waits on a socket. Drawing stays in [`crate::view`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::{Stream, StreamExt};
use iced::widget::image::Handle;
use iced::widget::scrollable::{RelativeOffset, Viewport};
use iced::widget::{Id, operation};
use iced::{Element, Size, Subscription, Task, Theme, keyboard, mouse, window};
use vorcall_core::config::{
    PeerAudio, SHARE_MAX_BITRATE_KBPS, SHARE_MIN_BITRATE_KBPS, TransmitMode, VAD_MAX_DB, VAD_MIN_DB,
};
use vorcall_core::connection::{
    self, Blob, Command, DisconnectReason, Event, GENERAL_ROOM, MediaKey,
};
use vorcall_core::mentions::Segment;
use vorcall_core::update::{self, Checker, Outcome, Progress, PublicKey, Ready, Version};
use vorcall_core::{
    ApiFailure, Attachment, ChatMessage, Config, Endpoints, ErrorCode, Member, Room, RoomEntry,
    RoomKind, Session, VoiceMember, attachments, auth, config, diagnostics, mentions, report,
    session,
};
use vorcall_hotkey::{Backend, Binding, Edge, Listener, MouseButton, Unavailable};
use vorcall_screen::codec::Picture;
use vorcall_screen::preset::{self, FrameRate, Preset, Resolution};
use vorcall_screen::{AudioMode, Capabilities, CaptureRequest, Source, SourceId};
use vorcall_voice::{CleanupSettings, MediaConfig, MediaEngine, Playout, Stats, VideoStats};

use crate::share::{
    self, DecodeHandle, ShareCommand, ShareEvent, ShareHandle, ShareStats, StageEvent,
};
use crate::view;
use crate::voice::{self, AudioCommand, AudioEvent, AudioHandle, AudioSettings, TransmitSettings};
use crate::{audio, brand, images, notify, update_ui};

/// How many messages stay in memory; nothing is persisted.
pub const MESSAGE_LIMIT: usize = 2000;
/// The server rejects anything longer with a non-fatal INVALID_MESSAGE.
pub const MESSAGE_MAX_CHARS: usize = 2000;
const USERNAME_MAX: usize = 32;
/// `PROTOCOL.md` § Limits: what the server accepts as a room name.
const ROOM_NAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 128;
/// At most one notification and one chime per window, however many messages
/// land in it: a busy room must not turn into a burst of popups.
const TOAST_INTERVAL: Duration = Duration::from_secs(5);
/// `PROTOCOL.md` § Client connection loop: at most one `MarkRead` per room per
/// second, which is also how long a read waits before it is reported.
const MARK_READ_INTERVAL: Duration = Duration::from_secs(1);
/// How much of the attachment cache survives a start.
const IMAGE_CACHE_LIMIT: u64 = 500 << 20;
/// How often the speaking indicators are refreshed while in a voice room.
const VOICE_TICK: Duration = Duration::from_millis(100);
/// A member counts as speaking while the local decoder got a packet this
/// recently.
const SPEAKING_WINDOW: Duration = Duration::from_millis(200);
/// Ticks between two reads of the media statistics: the status line moves far
/// slower than the speaking dots.
const STATS_EVERY: u32 = 10;

/// How often a connected client asks whether there is a new release.
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// What the voice room gets between the LeaveVoice and the swap: the frame has
/// to reach the server and the engine has to stop its tasks, and the process
/// that replaces this one waits for neither.
const RESTART_GRACE: Duration = Duration::from_millis(750);
/// The coarsest step a download reports, so a big release is not a message per
/// chunk. Below it, one percent is the step.
const PROGRESS_STEP: u64 = 1024 * 1024;

/// The pop-out stage: a 16:9 picture with its toolbar over it.
const STAGE_WINDOW: (f32, f32) = (960.0, 560.0);
/// The loudest a watched share can be played, which is the range the stage's
/// slider offers.
const SHARE_VOLUME_MAX: f32 = 2.0;
/// What [`vorcall_screen::capabilities`] calls a system that cannot capture.
pub const NO_CAPTURE: &str = "none";
/// Whose screen the stage shows while the roster has not caught up.
const UNKNOWN_SHARER: &str = "someone";

const UNEXPECTED: &str = "Unexpected server answer";
/// Both the guard that keeps a huge file out of memory and the answer to one
/// the server would refuse anyway.
const TOO_LARGE: &str = "Images must be 8 MiB or smaller";
const NOT_AN_IMAGE: &str = "Only PNG, JPEG, GIF and WebP images can be attached";

pub struct App {
    endpoints: Endpoints,
    config: Config,
    session: Option<Session>,
    /// The connection subscription's identity, bumped by every sign-in and
    /// sign-out. The tokens themselves are not part of it: the loop rotates
    /// them while it runs, and that must not restart it.
    session_generation: u64,
    screen: Screen,
    focused: bool,
    last_toast: Option<Instant>,
    audio: Option<audio::Audio>,
    audio_unavailable: bool,
    /// The splash window while its entrance plays. It is closed before the main
    /// window opens, so the two never exist at once.
    splash: Option<window::Id>,
    main_window: Option<window::Id>,
    entrance: Option<brand::entrance::Entrance>,
    icon: Option<window::Icon>,
    /// The last run left a crash report behind and nobody has been asked about
    /// it yet. Cleared by the offer, whichever way it is answered.
    crash_offer: bool,
    update: UpdateState,
    update_keys: Vec<PublicKey>,
    /// The release notes of the last download, kept for the settings page after
    /// the banner is gone.
    update_notes: Option<(Version, String)>,
    /// Whether this process has checked at all: the first connection is what
    /// starts it, and every reconnect after that must not.
    checked_on_connect: bool,
    /// What an earlier answer said about the update in flight. The download
    /// progress never says whether it is required, and `Restarting` carries no
    /// manifest of its own, so both read this instead.
    force_required: bool,
    pending_restart: Option<Ready>,
    loading: brand::loading::Loading,
    loading_elapsed: Duration,
}

pub enum Screen {
    Login {
        username: String,
        password: String,
        error: Option<String>,
        busy: bool,
    },
    Register {
        username: String,
        password: String,
        confirm: String,
        invite: String,
        error: Option<String>,
        busy: bool,
    },
    /// Boxed: the chat state carries the voice session and dwarfs the two
    /// sign-in screens.
    Chat(Box<ChatState>),
}

pub struct ChatState {
    /// Every room the account can see, keyed by room id.
    pub rooms: BTreeMap<String, RoomUi>,
    pub current_room: String,
    /// Every registered user, online or not.
    pub users: BTreeMap<i64, Member>,
    pub member_id: i64,
    pub input: String,
    pub status: Status,
    cmd: Option<mpsc::Sender<Command>>,
    /// A transient server complaint shown next to the status.
    pub notice: Option<String>,
    pub dialog: Option<Dialog>,
    /// The trimmed name a `CreateRoom` is out for, which is how its answer is
    /// told apart from every other room the server describes.
    pending_create: Option<String>,
    pub voice: VoiceUi,
    /// Who is in each room's voice channel, whichever one this client joined.
    pub voice_rosters: BTreeMap<String, VoiceRoster>,
    pub page: Page,
    pub composer: Composer,
    /// The message row under the pointer, which is what shows its actions.
    pub hovered: Option<i64>,
    /// The message whose reaction palette is open.
    pub reacting: Option<i64>,
    pub confirm_delete: Option<i64>,
    pub images: BTreeMap<i64, ImageState>,
    /// The handle the next transfer is asked for under; every answer carries it
    /// back.
    next_request: u64,
    pending_uploads: BTreeMap<u64, PendingUpload>,
    pending_fetches: BTreeMap<u64, i64>,
    /// The newest message of a room that has been read but not reported yet.
    mark_read_due: Option<(String, i64)>,
    /// What is being typed after an `@`, without the `@` itself.
    pub mention_query: Option<String>,
    /// The user an `OpenDm` is out for, so its room can be opened when it
    /// arrives.
    pending_dm: Option<i64>,
    /// Every user as [`mentions`] takes them, rebuilt only when the roster
    /// changes: a rendered row must not pay for a copy of it.
    user_pairs: Vec<(i64, String)>,
}

/// One room as the window holds it: the shared facts the server sends, this
/// reader's counters, and everything the view needs to draw it.
pub struct RoomUi {
    pub room: Room,
    pub last_message_id: i64,
    pub unread: u32,
    pub mentions: u32,
    /// Keyed by id: history, older pages and live frames overlap, and id is the
    /// only order.
    pub messages: BTreeMap<i64, ChatMessage>,
    /// Whether a `History` for this room has ever landed; history is per room
    /// and on demand.
    pub loaded: bool,
    pub loading: bool,
    pub has_older: bool,
    pub loading_older: bool,
    pub history_error: Option<String>,
    pub at_bottom: bool,
    pub pending_new: u32,
    pub online: BTreeSet<i64>,
    /// A DM the user closed. It stays in `rooms` — only its entry in the
    /// sidebar is gone, until the next message or a new `OpenDm`.
    pub hidden: bool,
}

/// Who is in one room's voice channel. The client can be in only one of them,
/// and [`VoiceUi`] is that one; this is every room's, drawn but not heard.
#[derive(Default)]
pub struct VoiceRoster {
    pub members: BTreeMap<i64, VoiceMember>,
    pub speaking: BTreeSet<i64>,
    /// Who is sharing their screen, and whether that share carries audio. Kept
    /// beside `members`, which only holds what the last frame about a member
    /// said and goes stale the moment somebody starts or stops sharing.
    pub sharing: BTreeMap<i64, bool>,
}

/// What the message being written carries besides its text.
#[derive(Default)]
pub struct Composer {
    pub reply_to: Option<i64>,
    pub editing: Option<i64>,
    /// Uploaded already, and linked to the message once it is sent.
    pub attachments: Vec<Attachment>,
    /// Uploads still in flight, which count against the per-message limit.
    pub uploading: usize,
}

/// Where one attachment's pixels are. `Failed` is not retried on its own: the
/// row draws a placeholder instead.
pub enum ImageState {
    Loading,
    Ready(Handle),
    Failed,
}

/// What an upload was asked for, so its answer can be dropped when the room it
/// belongs to is no longer the one being written in.
struct PendingUpload {
    room_id: String,
}

pub enum Dialog {
    ChangePassword {
        current: String,
        new: String,
        confirm: String,
        error: Option<String>,
        busy: bool,
    },
    NewRoom {
        name: String,
        error: Option<String>,
    },
    /// One attachment at full size.
    Image(i64),
    /// The offer made once at startup when the last run left a crash report.
    CrashReport,
    /// What to share, before any capture starts. A system whose own picker
    /// chooses the source has nothing to list here.
    SharePicker {
        sources: SourcesState,
        selected: Option<SourceId>,
        audio: bool,
    },
}

/// What the picker knows about this machine's screens and windows.
pub enum SourcesState {
    Loading,
    Ready(Vec<Source>),
    Failed(String),
}

/// Everything the voice room adds to the chat screen. `intent` is what makes a
/// reconnect rejoin; nothing below it survives one.
#[derive(Default)]
pub struct VoiceUi {
    /// Which room's voice channel this is. `Default` leaves it empty;
    /// [`ChatState::new`] is what starts it at `general`.
    pub room_id: String,
    pub intent: bool,
    /// `JoinVoice` is out and `VoiceReady` has not come back yet.
    pub joining: bool,
    pub members: BTreeMap<i64, VoiceMember>,
    /// The joined room's sharers, kept the way [`VoiceRoster::sharing`] is.
    pub sharing: BTreeMap<i64, bool>,
    pub share: ShareUi,
    pub watch: WatchUi,
    pub speaking_server: BTreeSet<i64>,
    /// Whoever the local decoder heard within [`SPEAKING_WINDOW`], which needs
    /// no help from the server.
    pub speaking_local: BTreeSet<i64>,
    pub muted: bool,
    pub deafened: bool,
    pub ptt_held: bool,
    /// What the audio thread says about its own microphone: under voice
    /// activation the gate decides this, not the key.
    pub transmitting: bool,
    /// The last level the audio thread reported, in dBFS, and whether the gate
    /// stood open — the settings meter, and nothing else.
    pub input_level: Option<(f32, bool)>,
    pub stats: Stats,
    pub session: Option<VoiceSession>,
    /// The system-wide push-to-talk listener. Dropping it stops it.
    hotkey: Option<Listener>,
    pub hotkey_status: HotkeyStatus,
    /// Which listener the room is on. Every start, edge stream and answer
    /// names the generation it belongs to, so a start still in flight when the
    /// next one begins can be told apart from it and dropped.
    hotkey_generation: u64,
    /// Whose volume and mute the sidebar has open.
    pub expanded_member: Option<i64>,
    /// A mirror of the configured tuning, so applying it needs no [`Config`].
    peer_audio: BTreeMap<i64, PeerAudio>,
    /// Whether a `VoiceState` for the joined room has landed since this
    /// session's media path was set up. Until one has, `sharing` still
    /// describes the session that was replaced.
    roster_seen: bool,
    /// The ssrc of the last `VoiceReady`, waiting for its engine.
    pending_ssrc: u32,
    by_ssrc: BTreeMap<u32, i64>,
    /// What un-deafening puts back.
    muted_before_deafen: bool,
    ticks: u32,
}

/// This client's own screen share. `intent` is what makes a reconnect start it
/// again; nothing else here survives one.
#[derive(Default)]
pub struct ShareUi {
    pub intent: Option<ShareIntent>,
    /// A capture is being started and the server has not answered for it yet.
    pub starting: bool,
    pub active: bool,
    /// The share thread, started the first time something is shared and kept
    /// for as long as the chat screen is open.
    handle: Option<ShareHandle>,
    pub watchers: u32,
    pub stats: Option<ShareStats>,
    pub backend: Option<&'static str>,
    /// What the backend really captured, which is not always what was asked
    /// for: `None` is a share without audio.
    pub audio: Option<AudioMode>,
}

/// What a share was started with, so a reconnect can start the same one again.
pub struct ShareIntent {
    request: CaptureRequest,
    preset: Preset,
}

impl ShareUi {
    /// The share thread, spawned on the first share and kept afterwards. The
    /// task is its event stream, which must only be wired once.
    fn ensure_thread(&mut self) -> Task<Message> {
        if self.handle.is_some() {
            return Task::none();
        }
        let (handle, events) = share::spawn_share_thread();
        self.handle = Some(handle);
        Task::run(events, Message::Share)
    }

    fn send(&self, command: ShareCommand) {
        if let Some(handle) = &self.handle {
            handle.send(command);
        }
    }

    /// Everything about a share that is over. The intent is the caller's: a
    /// reconnect keeps it, a failure gives it up.
    fn stopped(&mut self) {
        self.starting = false;
        self.active = false;
        self.stats = None;
        self.watchers = 0;
        self.backend = None;
        self.audio = None;
    }
}

/// The share being watched: the intent that survives a reconnect, and the
/// decoded picture the stage draws.
pub struct WatchUi {
    pub intent: Option<i64>,
    /// Whose stream the server has actually put this client on.
    pub state: Option<i64>,
    pub picture: Option<Arc<Picture>>,
    pub seq: u64,
    /// Dropping it stops the decode thread; one thread serves a whole session,
    /// because the access units are handed out once per engine.
    decoder: Option<DecodeHandle>,
    /// Decoded frames per second, pictures and errors, as the decode thread
    /// last reported them.
    pub stats: Option<(f32, u64, u64)>,
    pub video: VideoStats,
    pub popped: Option<window::Id>,
    /// The window the stage has taken over, which is not always the main one.
    pub fullscreen: Option<window::Id>,
    pub volume: f32,
    /// `video.bytes` as of the last report, for the rate below.
    last_bytes: u64,
    pub kbps: u32,
    /// Whether the next `VoiceState` for the joined room decides the watch a
    /// reconnect kept: it is only worth asking for again while that user shares.
    resume_pending: bool,
}

impl Default for WatchUi {
    fn default() -> Self {
        Self {
            intent: None,
            state: None,
            picture: None,
            seq: 0,
            decoder: None,
            stats: None,
            video: VideoStats::default(),
            popped: None,
            fullscreen: None,
            volume: 1.0,
            last_bytes: 0,
            kbps: 0,
            resume_pending: false,
        }
    }
}

/// Where push-to-talk edges come from. `Global` is a listener actually running;
/// `WindowOnly` carries why there is none, which the settings page shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HotkeyStatus {
    #[default]
    Off,
    Starting,
    Global {
        backend: Backend,
        trigger: Option<String>,
    },
    WindowOnly(String),
}

/// The media path of one voice session: the UDP engine and the audio thread
/// that feeds it.
pub struct VoiceSession {
    pub ssrc: u32,
    pub engine: MediaEngine,
    pub audio: AudioHandle,
    /// The devices the audio thread opened; no input means the microphone did
    /// not open and the session is listen-only.
    pub audio_input: Option<String>,
    pub audio_output: Option<String>,
}

pub enum Page {
    Chat,
    Settings(SettingsState),
}

#[derive(Default)]
pub struct SettingsState {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub capturing_ptt: bool,
    pub report: ReportState,
}

/// How far the problem report started from the Settings page has got.
#[derive(Debug, Clone, Default)]
pub enum ReportState {
    #[default]
    Idle,
    Sending,
    Sent(usize),
    Failed(String),
}

/// A [`MediaEngine`] is not `Clone` and every [`Message`] is, so the engine
/// travels in this instead: the handler that gets there first takes it out.
/// `ssrc` says which `VoiceReady` asked for it, so an engine the room has
/// already moved past can be told apart from the one being waited on.
#[derive(Clone)]
pub struct EngineHandoff {
    ssrc: u32,
    engine: Arc<Mutex<Option<MediaEngine>>>,
}

impl EngineHandoff {
    fn new(ssrc: u32, engine: MediaEngine) -> Self {
        Self {
            ssrc,
            engine: Arc::new(Mutex::new(Some(engine))),
        }
    }

    fn take(&self) -> Option<MediaEngine> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl fmt::Debug for EngineHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EngineHandoff")
    }
}

/// A [`Listener`] is not `Clone` and every [`Message`] is, so a started
/// listener travels the same way an engine does. `generation`, `ssrc` and
/// `binding` say what it was started for: an answer that outlived any of them
/// is dropped, which stops it, rather than becoming a second listener.
#[derive(Clone)]
pub struct HotkeyHandoff {
    generation: u64,
    ssrc: u32,
    binding: Binding,
    started: Arc<Mutex<Option<Result<Listener, Unavailable>>>>,
}

impl HotkeyHandoff {
    fn new(
        generation: u64,
        ssrc: u32,
        binding: Binding,
        started: Result<Listener, Unavailable>,
    ) -> Self {
        Self {
            generation,
            ssrc,
            binding,
            started: Arc::new(Mutex::new(Some(started))),
        }
    }

    fn take(&self) -> Option<Result<Listener, Unavailable>> {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl fmt::Debug for HotkeyHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HotkeyHandoff")
    }
}

#[derive(Debug, Clone)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting { in_secs: u64 },
    Unauthorized,
    Disconnected(String),
}

/// Where the self-updater is. `Disabled` is decided at boot and never leaves:
/// such a build neither checks nor swaps anything.
pub enum UpdateState {
    Disabled(String),
    Idle,
    Checking,
    UpToDate {
        at: Instant,
    },
    NoBuild {
        platform: String,
    },
    Downloading {
        version: Version,
        received: u64,
        total: u64,
        required: bool,
    },
    Ready {
        ready: Ready,
        dismissed: bool,
    },
    Failed {
        message: String,
        required: bool,
        at: Instant,
    },
    Restarting,
}

impl UpdateState {
    /// Whether another check would only get in the way of what is already going
    /// on — or of a build that does not update itself at all.
    pub fn busy(&self) -> bool {
        match self {
            Self::Disabled(_) | Self::Checking | Self::Downloading { .. } | Self::Restarting => {
                true
            }
            // "Later" only puts the banner away. The next check reuses the file
            // already on disk, and its result brings the banner back.
            Self::Ready { dismissed, .. } => !dismissed,
            _ => false,
        }
    }

    /// Whether the loading creature is on screen, which is what makes its
    /// per-frame clock worth running.
    pub fn shows_creature(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }

    /// Whether the update takes the whole window instead of a banner. `forced`
    /// is [`App::force_required`]: neither a check nor a restart in flight
    /// carries the manifest that made the update required, and the retry after
    /// a failed required update must not flash the chat back into view.
    pub fn shows_required(&self, forced: bool) -> bool {
        match self {
            Self::Downloading { required, .. } | Self::Failed { required, .. } => *required,
            Self::Ready { ready, .. } => ready.required,
            Self::Checking | Self::Restarting => forced,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
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
    OpenChangePassword,
    CloseDialog,
    DialogCurrentChanged(String),
    DialogNewChanged(String),
    DialogConfirmChanged(String),
    ChangePasswordSubmit,
    ChangePasswordResult(Result<(), ApiFailure>),
    InputChanged(String),
    Send,
    LoadOlder,
    Scrolled(Viewport),
    JumpToLatest,
    SetNotifications(bool),
    SetSound(bool),
    SelectRoom(String),
    OpenNewRoom,
    NewRoomNameChanged(String),
    CreateRoom,
    JoinRoom(String),
    LeaveRoom(String),
    OpenDm(i64),
    CloseDm(String),
    /// The message row the pointer entered, and the one it left. Two messages
    /// rather than one option: the enter of the next row arrives before the
    /// exit of the previous one.
    Hover(i64),
    Unhover(i64),
    ReplyTo(i64),
    CancelReply,
    StartEdit(i64),
    CancelEdit,
    Delete(i64),
    ConfirmDelete(i64),
    OpenReactions(Option<i64>),
    React(i64, &'static str),
    PickAttachment,
    /// Whatever the file dialog answered, or one file dropped on the window.
    FilesPicked(Vec<PathBuf>),
    /// One file read off the disk: its name and its bytes.
    FileRead(Result<(String, Blob), String>),
    RemovePendingAttachment(i64),
    OpenImage(i64),
    /// The pixels of one attachment, ready to draw.
    ImageDecoded(i64, Result<Handle, String>),
    MarkReadTick,
    /// The username picked from the mention list.
    MentionPick(String),
    JoinVoice,
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    /// Every press and release reaches `update`: push-to-talk needs both edges,
    /// and the settings page binds whichever key comes next.
    KeyDown(keyboard::Key),
    KeyUp(keyboard::Key),
    MediaConnected(Result<EngineHandoff, String>),
    Audio(AudioEvent),
    VoiceTick,
    OpenSettings,
    CloseSettings,
    DevicesListed(Vec<String>, Vec<String>),
    SetInputDevice(String),
    SetOutputDevice(String),
    StartPttCapture,
    /// A press or release the system-wide listener saw, wherever the focus was.
    Hotkey {
        generation: u64,
        edge: Edge,
    },
    HotkeyStarted(HotkeyHandoff),
    /// The listener's edge stream ended, which is the only sign a backend gives
    /// that it stopped on its own.
    HotkeyEnded {
        generation: u64,
    },
    RetryHotkey,
    /// Only the buttons a binding may use reach these; a left click must not
    /// wake `update` at every press.
    MouseDown(mouse::Button),
    MouseUp(mouse::Button),
    SetTransmitMode(TransmitMode),
    SetVadThreshold(f32),
    /// The end of a drag, which is what writes the threshold to disk.
    VadThresholdReleased,
    /// The input cleanup switches; each one saves and reaches the audio thread
    /// at once.
    SetNoiseSuppression(bool),
    SetEchoCancellation(bool),
    SetAutoGain(bool),
    ToggleMemberPanel(i64),
    SetPeerVolume(i64, f32),
    PeerVolumeReleased(i64),
    TogglePeerMute(i64),
    /// The screen-share picker, and what is chosen in it.
    OpenSharePicker,
    SourcesListed(Result<Vec<Source>, String>),
    PickSource(SourceId),
    SetPickerAudio(bool),
    ConfirmShare,
    StopShare,
    /// What the share thread reports about the capture it owns.
    Share(ShareEvent),
    WatchShare(i64),
    StopWatching,
    /// What the decode thread reports about the stream it reads. Never logged
    /// whole: a picture is somebody's screen.
    Stage(StageEvent),
    PopOutStage,
    PopInStage,
    ToggleFullscreen,
    SetShareVolume(f32),
    /// The end of a drag, which is what writes the volume to disk.
    ShareVolumeReleased,
    SetShareResolution(String),
    SetShareFps(u32),
    SetShareBitrateAuto(bool),
    SetShareBitrate(u32),
    ShareBitrateReleased,
    SetShareAudio(bool),
    /// One per frame while the splash is up.
    SplashTick(Instant),
    SplashSkip,
    /// The Settings button that sends the logs and the crash reports, and the
    /// count of files sent, or why none were.
    ReportProblem,
    ReportFinished(Result<usize, String>),
    /// The answers to the offer made after a crash.
    SendCrashReport,
    DismissCrashReport,
    /// A check somebody asked for: the settings button, and the retry on the
    /// required screen.
    CheckForUpdates,
    UpdateTick,
    UpdateProgress(Progress),
    /// [`Outcome`] is not `Clone` and every [`Message`] is, so the answer
    /// travels behind an [`Arc`]; the failure is already a sentence.
    UpdateResult(Result<Arc<Outcome>, String>),
    RestartForUpdate,
    ApplyUpdate,
    DismissUpdate,
    /// One per frame while the loading creature is on screen.
    LoadingTick(Instant),
    WindowClosed(window::Id),
    Focus(bool),
    Conn(Event),
    Noop,
}

impl App {
    pub fn new(
        endpoints: Endpoints,
        config: Config,
        session: Option<Session>,
        update_keys: Vec<PublicKey>,
        disabled: Option<String>,
    ) -> Self {
        let screen = if session.is_some() {
            chat_screen(&config)
        } else {
            Screen::login(config.username.clone())
        };

        let mut app = Self {
            endpoints,
            config,
            session,
            session_generation: 0,
            screen,
            focused: true,
            last_toast: None,
            audio: None,
            audio_unavailable: false,
            splash: None,
            main_window: None,
            entrance: None,
            icon: None,
            // Listing the directory is all this reads; the files themselves are
            // only ever read off the UI thread.
            crash_offer: !diagnostics::crash_reports().is_empty(),
            update: match disabled {
                Some(reason) => UpdateState::Disabled(reason),
                None => UpdateState::Idle,
            },
            update_keys,
            update_notes: None,
            checked_on_connect: false,
            force_required: false,
            pending_restart: None,
            loading: brand::loading::Loading::new(),
            loading_elapsed: Duration::ZERO,
        };
        app.offer_crash_report();
        app
    }

    /// Builds the state and asks for the first window: the splash, or the main
    /// window when the entrance is switched off.
    pub fn boot(
        endpoints: Endpoints,
        config: Config,
        session: Option<Session>,
        update_keys: Vec<PublicKey>,
        disabled: Option<String>,
    ) -> (Self, Task<Message>) {
        let mut app = Self::new(endpoints, config, session, update_keys, disabled);
        app.icon = brand::icon::window_icon();

        let task = match brand::entrance::choose() {
            Some(variant) => {
                app.entrance = Some(brand::entrance::Entrance::new(variant));
                let (id, task) = window::open(app.splash_settings());
                app.splash = Some(id);
                task.discard()
            }
            None => app.open_main(),
        };
        (app, Task::batch([task, prune_image_cache()]))
    }

    fn open_main(&mut self) -> Task<Message> {
        let (id, task) = window::open(self.main_settings());
        self.main_window = Some(id);
        // The window's own `Focused` event is what flips this back: a window the
        // manager opens in the background must not pass for focused.
        self.focused = false;
        task.discard()
    }

    fn splash_settings(&self) -> window::Settings {
        let size = Size::new(320.0, 320.0);
        window::Settings {
            size,
            min_size: Some(size),
            max_size: Some(size),
            position: window::Position::Centered,
            resizable: false,
            decorations: false,
            transparent: true,
            level: window::Level::AlwaysOnTop,
            icon: self.icon.clone(),
            platform_specific: platform_specific(true),
            ..window::Settings::default()
        }
    }

    fn main_settings(&self) -> window::Settings {
        window::Settings {
            size: Size::new(1100.0, 680.0),
            icon: self.icon.clone(),
            platform_specific: platform_specific(false),
            ..window::Settings::default()
        }
    }

    pub fn title(&self, window: window::Id) -> String {
        if self.splash == Some(window) {
            return "Vorcall".to_owned();
        }
        if let Screen::Chat(chat) = &self.screen
            && chat.voice.watch.popped == Some(window)
        {
            return format!("{} · Vorcall", sharer_name(chat));
        }
        let unread = match &self.screen {
            Screen::Chat(chat) => chat.total_unread(),
            _ => 0,
        };
        if unread > 0 {
            format!("({unread}) Vorcall")
        } else {
            "Vorcall".to_owned()
        }
    }

    pub fn theme(&self, window: window::Id) -> Theme {
        if self.splash == Some(window) {
            brand::palette::splash_theme()
        } else {
            brand::palette::theme()
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // While the entrance plays, a key or mouse press is its skip and
            // must not reach push-to-talk or the settings key capture. Once it
            // is over these flow again, so the main window's first `Focused`
            // still lands even while the splash is being torn down.
            Message::Focus(_)
            | Message::KeyDown(_)
            | Message::KeyUp(_)
            | Message::MouseDown(_)
            | Message::MouseUp(_)
                if self.entrance.is_some() =>
            {
                Task::none()
            }
            Message::UsernameChanged(value) => {
                match &mut self.screen {
                    Screen::Login { username, .. } | Screen::Register { username, .. } => {
                        *username = value;
                    }
                    Screen::Chat(_) => {}
                }
                Task::none()
            }
            Message::PasswordChanged(value) => {
                match &mut self.screen {
                    Screen::Login { password, .. } | Screen::Register { password, .. } => {
                        *password = value;
                    }
                    Screen::Chat(_) => {}
                }
                Task::none()
            }
            Message::ConfirmChanged(value) => {
                if let Screen::Register { confirm, .. } = &mut self.screen {
                    *confirm = value;
                }
                Task::none()
            }
            Message::InviteChanged(value) => {
                if let Screen::Register { invite, .. } = &mut self.screen {
                    *invite = value;
                }
                Task::none()
            }
            Message::ShowLogin => {
                let username = match &self.screen {
                    Screen::Register { username, .. } => username.clone(),
                    _ => self.config.username.clone(),
                };
                self.screen = Screen::login(username);
                operation::focus(Id::new(view::USERNAME_ID))
            }
            Message::ShowRegister => {
                let username = match &self.screen {
                    Screen::Login { username, .. } => username.clone(),
                    _ => self.config.username.clone(),
                };
                self.screen = Screen::Register {
                    username,
                    password: String::new(),
                    confirm: String::new(),
                    invite: String::new(),
                    error: None,
                    busy: false,
                };
                operation::focus(Id::new(view::USERNAME_ID))
            }
            Message::LoginSubmit => self.submit_login(),
            Message::RegisterSubmit => self.submit_register(),
            Message::LoginResult(Ok(session)) => self.signed_in(session),
            Message::LoginResult(Err(failure)) => {
                let detail = describe(&failure);
                match &mut self.screen {
                    Screen::Login { error, busy, .. } | Screen::Register { error, busy, .. } => {
                        *error = Some(detail);
                        *busy = false;
                    }
                    Screen::Chat(_) => {}
                }
                Task::none()
            }
            Message::Logout => self.logout(),
            Message::OpenChangePassword => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.dialog = Some(Dialog::ChangePassword {
                    current: String::new(),
                    new: String::new(),
                    confirm: String::new(),
                    error: None,
                    busy: false,
                });
                operation::focus(Id::new(view::CURRENT_PASSWORD_ID))
            }
            Message::CloseDialog => self.close_overlay(),
            Message::DialogCurrentChanged(value) => {
                if let Some(Dialog::ChangePassword { current, .. }) = self.dialog() {
                    *current = value;
                }
                Task::none()
            }
            Message::DialogNewChanged(value) => {
                if let Some(Dialog::ChangePassword { new, .. }) = self.dialog() {
                    *new = value;
                }
                Task::none()
            }
            Message::DialogConfirmChanged(value) => {
                if let Some(Dialog::ChangePassword { confirm, .. }) = self.dialog() {
                    *confirm = value;
                }
                Task::none()
            }
            Message::ChangePasswordSubmit => self.submit_change_password(),
            Message::ChangePasswordResult(result) => self.on_change_password(result),
            Message::InputChanged(value) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.mention_query = mention_query(&value);
                    chat.input = value;
                }
                Task::none()
            }
            Message::Send => self.send(),
            Message::LoadOlder => self.load_older(),
            Message::Scrolled(viewport) => {
                let focused = self.focused;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                // Under `Anchor::End` the offset is measured from the end of
                // the list, so zero is the bottom.
                let at_bottom = viewport.absolute_offset().y <= 1.0;
                if let Some(room) = chat.current_mut() {
                    room.at_bottom = at_bottom;
                    if at_bottom {
                        room.pending_new = 0;
                    }
                }
                if at_bottom && focused {
                    chat.schedule_mark_read();
                }
                Task::none()
            }
            Message::JumpToLatest => {
                let focused = self.focused;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                if let Some(room) = chat.current_mut() {
                    room.pending_new = 0;
                    room.at_bottom = true;
                }
                if focused {
                    chat.schedule_mark_read();
                }
                snap_to_bottom()
            }
            Message::SetNotifications(value) => {
                self.config.notifications = value;
                self.save_config();
                Task::none()
            }
            Message::SetSound(value) => {
                self.config.sound = value;
                self.save_config();
                Task::none()
            }
            Message::SelectRoom(room_id) => {
                let focused = self.focused;
                match &mut self.screen {
                    Screen::Chat(chat) => chat.select_room(room_id, focused),
                    _ => Task::none(),
                }
            }
            Message::OpenNewRoom => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.dialog = Some(Dialog::NewRoom {
                    name: String::new(),
                    error: None,
                });
                Task::none()
            }
            Message::NewRoomNameChanged(value) => {
                if let Some(Dialog::NewRoom { name, .. }) = self.dialog() {
                    *name = value;
                }
                Task::none()
            }
            Message::CreateRoom => self.create_room(),
            Message::JoinRoom(room_id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.send_or_notice(Command::JoinRoom { room_id });
                }
                Task::none()
            }
            Message::LeaveRoom(room_id) => self.leave_room(room_id),
            Message::OpenDm(user_id) => self.open_dm(user_id),
            Message::CloseDm(room_id) => {
                let focused = self.focused;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                if let Some(room) = chat.rooms.get_mut(&room_id) {
                    room.hidden = true;
                }
                if chat.current_room == room_id {
                    return chat.select_room(GENERAL_ROOM.to_owned(), focused);
                }
                Task::none()
            }
            Message::Hover(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.hovered = Some(id);
                }
                Task::none()
            }
            Message::Unhover(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.unhover(id);
                }
                Task::none()
            }
            Message::ReplyTo(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.composer.reply_to = Some(id);
                    chat.composer.editing = None;
                }
                operation::focus(Id::new(view::INPUT_ID))
            }
            Message::CancelReply => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.composer.reply_to = None;
                }
                Task::none()
            }
            Message::StartEdit(id) => self.start_edit(id),
            Message::CancelEdit => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.composer.editing = None;
                    chat.input.clear();
                    chat.mention_query = None;
                }
                operation::focus(Id::new(view::INPUT_ID))
            }
            Message::Delete(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.confirm_delete = Some(id);
                }
                Task::none()
            }
            Message::ConfirmDelete(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.confirm_delete = None;
                    chat.send_or_notice(Command::Delete { id });
                }
                Task::none()
            }
            Message::OpenReactions(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.reacting = id;
                }
                Task::none()
            }
            Message::React(message_id, emoji) => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                let remove = chat.reacted(message_id, emoji);
                chat.reacting = None;
                chat.send_or_notice(Command::React {
                    message_id,
                    emoji: emoji.to_owned(),
                    remove,
                });
                Task::none()
            }
            Message::PickAttachment => Task::perform(pick_images(), Message::FilesPicked),
            Message::FilesPicked(paths) => Task::batch(paths.into_iter().map(read_file)),
            Message::FileRead(result) => self.on_file_read(result),
            Message::RemovePendingAttachment(id) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.composer
                        .attachments
                        .retain(|attachment| attachment.id != id);
                }
                Task::none()
            }
            Message::OpenImage(id) => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.dialog = Some(Dialog::Image(id));
                chat.ensure_image(id)
            }
            Message::ImageDecoded(id, result) => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                match result {
                    Ok(handle) => {
                        chat.images.insert(id, ImageState::Ready(handle));
                    }
                    Err(error) => {
                        tracing::warn!(id, %error, "cannot decode an attachment");
                        chat.images.insert(id, ImageState::Failed);
                    }
                }
                Task::none()
            }
            Message::MarkReadTick => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.flush_mark_read();
                }
                Task::none()
            }
            Message::MentionPick(username) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.input = mention_replace(&chat.input, &username);
                    chat.mention_query = None;
                }
                operation::focus(Id::new(view::INPUT_ID))
            }
            Message::JoinVoice => self.join_voice(),
            Message::LeaveVoice => self.leave_voice(),
            Message::ToggleMute => self.toggle_mute(),
            Message::ToggleDeafen => self.toggle_deafen(),
            Message::KeyDown(key) => self.key_down(key),
            Message::KeyUp(key) => {
                if self.key_is_ptt(&key) {
                    self.set_ptt(false);
                }
                Task::none()
            }
            Message::MouseDown(button) => self.mouse_down(button),
            Message::MouseUp(button) => {
                if self.mouse_is_ptt(button) {
                    self.set_ptt(false);
                }
                Task::none()
            }
            Message::Hotkey { generation, edge } => {
                // In-window edges are ignored while this runs, so one press
                // never reaches push-to-talk twice.
                if self.hotkey_is_current(generation)
                    && self.hotkey_is_global()
                    && self.config.transmit_mode == TransmitMode::PushToTalk
                {
                    self.set_ptt(edge == Edge::Pressed);
                }
                Task::none()
            }
            Message::HotkeyStarted(handoff) => self.hotkey_started(handoff),
            Message::HotkeyEnded { generation } => self.hotkey_ended(generation),
            Message::RetryHotkey => self.start_hotkey(),
            Message::SetTransmitMode(mode) => self.set_transmit_mode(mode),
            Message::SetVadThreshold(threshold_db) => {
                self.config.vad_threshold_db = threshold_db.clamp(VAD_MIN_DB, VAD_MAX_DB);
                self.push_transmit();
                Task::none()
            }
            Message::VadThresholdReleased => {
                self.save_config();
                Task::none()
            }
            Message::SetNoiseSuppression(value) => {
                self.config.noise_suppression = value;
                self.save_config();
                self.push_cleanup();
                Task::none()
            }
            Message::SetEchoCancellation(value) => {
                self.config.echo_cancellation = value;
                self.save_config();
                self.push_cleanup();
                Task::none()
            }
            Message::SetAutoGain(value) => {
                self.config.auto_gain = value;
                self.save_config();
                self.push_cleanup();
                Task::none()
            }
            Message::ToggleMemberPanel(user_id) => {
                // Nothing to tune about oneself: the local mixer never plays
                // this member back.
                if let Screen::Chat(chat) = &mut self.screen
                    && user_id != chat.member_id
                {
                    let voice = &mut chat.voice;
                    voice.expanded_member =
                        (voice.expanded_member != Some(user_id)).then_some(user_id);
                }
                Task::none()
            }
            Message::SetPeerVolume(user_id, volume) => {
                self.update_peer_audio(user_id, |audio| audio.volume = volume);
                Task::none()
            }
            Message::PeerVolumeReleased(user_id) => {
                // Every step of the drag reached the mixer already; only its
                // end reaches the disk.
                let volume = self.config.peer_audio(user_id).volume;
                tracing::debug!(user_id, volume, "a peer's volume was set");
                self.save_config();
                Task::none()
            }
            Message::TogglePeerMute(user_id) => {
                self.update_peer_audio(user_id, |audio| audio.muted = !audio.muted);
                self.save_config();
                Task::none()
            }
            Message::OpenSharePicker => self.open_share_picker(),
            Message::SourcesListed(result) => {
                if let Screen::Chat(chat) = &mut self.screen
                    && let Some(Dialog::SharePicker { sources, .. }) = &mut chat.dialog
                {
                    *sources = match result {
                        Ok(listed) => SourcesState::Ready(listed),
                        Err(error) => SourcesState::Failed(error),
                    };
                }
                Task::none()
            }
            Message::PickSource(source_id) => {
                if let Screen::Chat(chat) = &mut self.screen
                    && let Some(Dialog::SharePicker { selected, .. }) = &mut chat.dialog
                {
                    *selected = Some(source_id);
                }
                Task::none()
            }
            Message::SetPickerAudio(value) => {
                if let Screen::Chat(chat) = &mut self.screen
                    && let Some(Dialog::SharePicker { audio, .. }) = &mut chat.dialog
                {
                    *audio = value;
                }
                Task::none()
            }
            Message::ConfirmShare => self.confirm_share(),
            Message::StopShare => self.stop_share(),
            Message::Share(event) => self.on_share_event(event),
            Message::WatchShare(user_id) => self.watch_share(user_id),
            Message::StopWatching => self.stop_watching(),
            Message::Stage(event) => self.on_stage_event(event),
            Message::PopOutStage => self.pop_out_stage(),
            Message::PopInStage => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                let watch = &mut chat.voice.watch;
                let Some(id) = watch.popped.take() else {
                    return Task::none();
                };
                if watch.fullscreen == Some(id) {
                    watch.fullscreen = None;
                }
                window::close(id)
            }
            Message::ToggleFullscreen => self.toggle_fullscreen(),
            Message::SetShareVolume(volume) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.voice.set_share_volume(volume);
                }
                Task::none()
            }
            Message::ShareVolumeReleased => {
                // Every step of the drag reached the mixer already; only its
                // end reaches the disk.
                if let Screen::Chat(chat) = &self.screen {
                    self.config.share_volume = chat.voice.watch.volume;
                }
                self.save_config();
                Task::none()
            }
            Message::SetShareResolution(resolution) => {
                self.config.share_resolution = resolution;
                self.save_config();
                Task::none()
            }
            Message::SetShareFps(fps) => {
                self.config.share_fps = fps;
                self.save_config();
                Task::none()
            }
            Message::SetShareBitrateAuto(auto) => {
                // Manual starts where automatic left off, so the slider does
                // not jump the moment it appears.
                self.config.share_bitrate_kbps = (!auto).then(|| auto_bitrate_kbps(&self.config));
                self.save_config();
                Task::none()
            }
            Message::SetShareBitrate(kbps) => {
                self.config.share_bitrate_kbps =
                    Some(kbps.clamp(SHARE_MIN_BITRATE_KBPS, SHARE_MAX_BITRATE_KBPS));
                Task::none()
            }
            Message::ShareBitrateReleased => {
                self.save_config();
                Task::none()
            }
            Message::SetShareAudio(value) => {
                self.config.share_audio = value;
                self.save_config();
                Task::none()
            }
            Message::MediaConnected(Ok(handoff)) => self.media_connected(handoff),
            Message::MediaConnected(Err(reason)) => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.voice.intent = false;
                chat.voice.joining = false;
                chat.notice = Some(format!("Voice: {reason}"));
                Task::none()
            }
            Message::Audio(event) => self.on_audio(event),
            Message::VoiceTick => self.voice_tick(),
            Message::OpenSettings => self.open_settings(),
            Message::CloseSettings => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.page = Page::Chat;
                }
                Task::none()
            }
            Message::DevicesListed(inputs, outputs) => {
                if let Some(settings) = self.settings() {
                    settings.inputs = inputs;
                    settings.outputs = outputs;
                }
                Task::none()
            }
            Message::SetInputDevice(name) => {
                self.config.input_device = device_choice(name);
                self.save_config();
                self.push_devices();
                Task::none()
            }
            Message::SetOutputDevice(name) => {
                self.config.output_device = device_choice(name);
                self.save_config();
                self.push_devices();
                Task::none()
            }
            Message::StartPttCapture => {
                if let Some(settings) = self.settings() {
                    settings.capturing_ptt = true;
                }
                Task::none()
            }
            Message::SplashTick(now) => {
                if let Some(entrance) = &mut self.entrance {
                    entrance.tick(now);
                    if entrance.finished() {
                        return self.close_splash();
                    }
                }
                Task::none()
            }
            Message::SplashSkip => self.close_splash(),
            Message::ReportProblem => self.report_problem(),
            Message::ReportFinished(result) => self.on_report_finished(result),
            Message::SendCrashReport => {
                self.close_crash_offer();
                self.report_problem()
            }
            Message::DismissCrashReport => {
                // The files stay on disk: the Settings button can still send
                // them later.
                self.close_crash_offer();
                Task::none()
            }
            Message::CheckForUpdates | Message::UpdateTick => self.check_for_updates(),
            Message::UpdateProgress(progress) => self.on_update_progress(progress),
            Message::UpdateResult(result) => self.on_update_result(result),
            Message::RestartForUpdate => self.restart_for_update(),
            Message::ApplyUpdate => self.apply_update(),
            Message::DismissUpdate => {
                if let UpdateState::Ready { dismissed, .. } = &mut self.update {
                    *dismissed = true;
                }
                Task::none()
            }
            Message::LoadingTick(now) => {
                self.loading_elapsed = self.loading.elapsed(now);
                Task::none()
            }
            Message::WindowClosed(id) => {
                // The stage goes back into the main window; the watch itself
                // carries on.
                if let Screen::Chat(chat) = &mut self.screen {
                    let watch = &mut chat.voice.watch;
                    if watch.popped == Some(id) {
                        watch.popped = None;
                    }
                    if watch.fullscreen == Some(id) {
                        watch.fullscreen = None;
                    }
                }
                if self.splash == Some(id) {
                    self.splash = None;
                    self.entrance = None;
                    // A splash closed from outside — Alt+F4 — never went through
                    // `close_splash`, so the main window is still to come.
                    if self.main_window.is_none() {
                        return self.open_main();
                    }
                }
                if self.main_window == Some(id) {
                    return iced::exit();
                }
                Task::none()
            }
            Message::Focus(focused) => {
                self.focused = focused;
                if focused {
                    // Whatever is on screen at the bottom has now been read.
                    if let Screen::Chat(chat) = &mut self.screen
                        && chat.current().is_some_and(|room| room.at_bottom)
                    {
                        chat.schedule_mark_read();
                    }
                } else if !self.hotkey_is_global() {
                    // A release that lands on another window never reaches us,
                    // so push-to-talk would stay held. The system-wide listener
                    // sees that release wherever it happens.
                    self.set_ptt(false);
                }
                Task::none()
            }
            Message::Conn(event) => self.on_event(event),
            Message::Noop => Task::none(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            iced::event::listen_with(ui_event),
            window::close_events().map(Message::WindowClosed),
        ];
        // The splash is the only window while the entrance plays, so every frame
        // that arrives is one of its own.
        if self.entrance.is_some() {
            subscriptions.push(window::frames().map(Message::SplashTick));
        }
        if self.creature_visible() {
            subscriptions.push(window::frames().map(Message::LoadingTick));
        }

        if let Screen::Chat(chat) = &self.screen
            && let Some(session) = self.session.clone()
        {
            subscriptions.push(
                Subscription::run_with(
                    ConnectionInput {
                        endpoints: self.endpoints.clone(),
                        session,
                        generation: self.session_generation,
                    },
                    connect,
                )
                .map(Message::Conn),
            );
            // Only while there is a media path to look at: the tick reads the
            // playout and, now and then, the engine statistics.
            if chat.voice.session.is_some() {
                subscriptions.push(iced::time::every(VOICE_TICK).map(|_| Message::VoiceTick));
            }
            // Only while a read is waiting to be reported: the tick is what
            // debounces `MarkRead` to one per second.
            if chat.mark_read_due.is_some() {
                subscriptions
                    .push(iced::time::every(MARK_READ_INTERVAL).map(|_| Message::MarkReadTick));
            }
            // Nothing to ask while the socket is down, and nothing to ask at all
            // in a build that does not update itself.
            if matches!(chat.status, Status::Connected)
                && !matches!(self.update, UpdateState::Disabled(_))
            {
                subscriptions.push(iced::time::every(UPDATE_INTERVAL).map(|_| Message::UpdateTick));
            }
        }
        Subscription::batch(subscriptions)
    }

    pub fn view(&self, window: window::Id) -> Element<'_, Message> {
        if self.splash == Some(window) {
            return match &self.entrance {
                Some(entrance) => entrance.view(Message::SplashSkip),
                None => iced::widget::Space::new().into(),
            };
        }

        if let Screen::Chat(chat) = &self.screen
            && chat.voice.watch.popped == Some(window)
        {
            return view::popped_stage(chat);
        }

        match &self.screen {
            Screen::Login {
                username,
                password,
                error,
                busy,
            } => view::login(username, password, error.as_deref(), *busy),
            Screen::Register {
                username,
                password,
                confirm,
                invite,
                error,
                busy,
            } => view::register(username, password, confirm, invite, error.as_deref(), *busy),
            Screen::Chat(chat) => {
                let update = update_ui::UpdateView {
                    state: &self.update,
                    notes: self.update_notes.as_ref(),
                    elapsed: self.loading_elapsed,
                };
                if self.update.shows_required(self.force_required) {
                    update_ui::required(update)
                } else {
                    view::chat(chat, &self.config, self.username(), update)
                }
            }
        }
    }

    /// Idempotent: a skip and the natural end in the same frame close once. The
    /// main window is opened first and the splash closed once it exists, because
    /// iced tears the compositor down while no window is left. `splash` itself is
    /// cleared only by [`Message::WindowClosed`].
    fn close_splash(&mut self) -> Task<Message> {
        if self.entrance.take().is_none() {
            return Task::none();
        }
        let open = self.open_main();
        match self.splash {
            Some(id) => open.chain(window::close(id)),
            None => open,
        }
    }

    /// The name the header shows: the signed-in one, falling back to whoever
    /// signed in last.
    fn username(&self) -> &str {
        self.session
            .as_ref()
            .map_or(self.config.username.as_str(), |session| {
                session.username.as_str()
            })
    }

    fn dialog(&mut self) -> Option<&mut Dialog> {
        match &mut self.screen {
            Screen::Chat(chat) => chat.dialog.as_mut(),
            _ => None,
        }
    }

    fn submit_login(&mut self) -> Task<Message> {
        let Screen::Login {
            username,
            password,
            error,
            busy,
        } = &mut self.screen
        else {
            return Task::none();
        };
        if *busy {
            return Task::none();
        }

        let username = username.trim().to_owned();
        if let Err(detail) = validate_username(&username) {
            *error = Some(detail);
            return Task::none();
        }
        if password.is_empty() {
            *error = Some("Enter your password.".to_owned());
            return Task::none();
        }

        let password = password.clone();
        *error = None;
        *busy = true;

        let endpoints = self.endpoints.clone();
        Task::perform(
            async move { auth::login(&endpoints, &username, &password).await },
            Message::LoginResult,
        )
    }

    fn submit_register(&mut self) -> Task<Message> {
        let Screen::Register {
            username,
            password,
            confirm,
            invite,
            error,
            busy,
        } = &mut self.screen
        else {
            return Task::none();
        };
        if *busy {
            return Task::none();
        }

        let username = username.trim().to_owned();
        let invite = invite.trim().to_owned();
        if let Err(detail) = validate_username(&username) {
            *error = Some(detail);
            return Task::none();
        }
        if let Err(detail) = validate_password(password) {
            *error = Some(detail);
            return Task::none();
        }
        if password != confirm {
            *error = Some("Passwords do not match".to_owned());
            return Task::none();
        }
        if invite.is_empty() {
            *error = Some("Enter your invite code.".to_owned());
            return Task::none();
        }

        let password = password.clone();
        *error = None;
        *busy = true;

        let endpoints = self.endpoints.clone();
        Task::perform(
            async move { auth::register(&endpoints, &username, &password, &invite).await },
            Message::LoginResult,
        )
    }

    fn signed_in(&mut self, session: Session) -> Task<Message> {
        if let Err(e) = session::save(&session) {
            tracing::warn!(error = %e, "cannot persist the session");
        }
        self.config.username = session.username.clone();
        self.save_config();

        tracing::info!(user_id = session.user_id, "signed in");
        self.session = Some(session);
        self.session_generation = self.session_generation.wrapping_add(1);
        self.screen = chat_screen(&self.config);
        self.offer_crash_report();
        operation::focus(Id::new(view::INPUT_ID))
    }

    fn logout(&mut self) -> Task<Message> {
        let leaving = self.leave_voice();
        let task = match self.session.take() {
            Some(session) => {
                let endpoints = self.endpoints.clone();
                let refresh_token = session.refresh_token;
                Task::perform(
                    async move {
                        if let Err(e) = auth::logout(&endpoints, &refresh_token).await {
                            tracing::warn!(error = %e, "the server did not confirm the sign-out");
                        }
                    },
                    |()| Message::Noop,
                )
            }
            None => Task::none(),
        };

        let closing = self.sign_out(None);
        Task::batch([leaving, task, closing])
    }

    /// Drops the local session and goes back to the sign-in screen. The server
    /// is not told: the caller either already did, or holds tokens the server
    /// has refused. Dropping [`Screen::Chat`] drops `cmd`, which ends
    /// `connection::run`.
    fn sign_out(&mut self, error: Option<String>) -> Task<Message> {
        // The media tasks outlive a dropped engine, so whatever is left of a
        // voice session is closed rather than dropped with the screen.
        let closing = match &mut self.screen {
            Screen::Chat(chat) => chat.voice.close_session(),
            _ => Task::none(),
        };

        if let Err(e) = session::delete() {
            tracing::warn!(error = %e, "cannot remove the stored session");
        }
        self.session = None;
        self.session_generation = self.session_generation.wrapping_add(1);
        self.screen = Screen::Login {
            username: self.config.username.clone(),
            password: String::new(),
            error,
            busy: false,
        };
        closing
    }

    fn submit_change_password(&mut self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let Some(Dialog::ChangePassword {
            current,
            new,
            confirm,
            error,
            busy,
        }) = self.dialog()
        else {
            return Task::none();
        };
        if *busy {
            return Task::none();
        }

        if current.is_empty() {
            *error = Some("Enter your current password.".to_owned());
            return Task::none();
        }
        if let Err(detail) = validate_password(new) {
            *error = Some(detail);
            return Task::none();
        }
        if new != confirm {
            *error = Some("Passwords do not match".to_owned());
            return Task::none();
        }

        let current = current.clone();
        let new = new.clone();
        *error = None;
        *busy = true;

        let endpoints = self.endpoints.clone();
        Task::perform(
            async move { auth::change_password(&endpoints, &session, &current, &new).await },
            Message::ChangePasswordResult,
        )
    }

    fn on_change_password(&mut self, result: Result<(), ApiFailure>) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let failure = match result {
            Ok(()) => {
                chat.dialog = None;
                chat.notice = Some("Password changed".to_owned());
                return operation::focus(Id::new(view::INPUT_ID));
            }
            Err(failure) => failure,
        };

        let detail = match &failure {
            ApiFailure::AuthChallenge(_) | ApiFailure::Status(401, _) => {
                "Current password is wrong".to_owned()
            }
            other => describe(other),
        };
        if let Some(Dialog::ChangePassword { error, busy, .. }) = &mut chat.dialog {
            *error = Some(detail);
            *busy = false;
        }
        Task::none()
    }

    fn send(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        if !matches!(chat.status, Status::Connected) {
            return Task::none();
        }

        // What goes on the wire is the token form, and that is what the
        // server measures against its own limit.
        let text = mentions::encode(chat.input.trim(), chat.user_pairs());
        if text.chars().count() > MESSAGE_MAX_CHARS {
            chat.notice = Some(format!(
                "Message is too long (max {MESSAGE_MAX_CHARS} characters)"
            ));
            return operation::focus(Id::new(view::INPUT_ID));
        }

        let editing = chat.composer.editing.is_some();
        let command = match chat.composer.editing {
            // An edit never empties a message; deleting it is its own action.
            Some(id) if !text.is_empty() => Command::Edit { id, text },
            Some(_) => return Task::none(),
            None if text.is_empty() && chat.composer.attachments.is_empty() => {
                return Task::none();
            }
            None => Command::Send {
                room_id: chat.current_room.clone(),
                text,
                reply_to_id: chat.composer.reply_to,
                attachment_ids: chat
                    .composer
                    .attachments
                    .iter()
                    .map(|attachment| attachment.id)
                    .collect(),
            },
        };

        if chat.send_command(command) {
            chat.input.clear();
            chat.mention_query = None;
            chat.composer.reply_to = None;
            chat.composer.editing = None;
            // An edit carries no attachments of its own, so whatever is held
            // still belongs to the message being written.
            if !editing {
                chat.composer.attachments.clear();
            }
            chat.notice = None;
        } else {
            chat.notice = Some("Message not sent".to_owned());
        }
        operation::focus(Id::new(view::INPUT_ID))
    }

    fn load_older(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let room_id = chat.current_room.clone();
        let Some(room) = chat.current_mut() else {
            return Task::none();
        };
        if !room.has_older || room.loading_older {
            return Task::none();
        }
        let Some(&before) = room.messages.keys().next() else {
            return Task::none();
        };

        if chat.send_command(Command::LoadOlder { room_id, before }) {
            if let Some(room) = chat.current_mut() {
                room.loading_older = true;
            }
        } else {
            chat.notice = Some("Not connected".to_owned());
        }
        Task::none()
    }

    /// Opens the dialog's room, leaving the dialog up: it closes when the
    /// `RoomUpdated` that created the room comes back.
    fn create_room(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let Some(Dialog::NewRoom { name, error }) = chat.dialog.as_mut() else {
            return Task::none();
        };

        let trimmed = name.trim().to_owned();
        if !(1..=ROOM_NAME_MAX).contains(&trimmed.chars().count()) {
            *error = Some(format!("A room name is 1 to {ROOM_NAME_MAX} characters."));
            return Task::none();
        }
        *error = None;

        chat.pending_create = Some(trimmed.clone());
        chat.send_or_notice(Command::CreateRoom { name: trimmed });
        Task::none()
    }

    /// Neither `general` nor a DM can be left; the server refuses both.
    fn leave_room(&mut self, room_id: String) -> Task<Message> {
        let Screen::Chat(chat) = &self.screen else {
            return Task::none();
        };
        if room_id == GENERAL_ROOM || chat.rooms.get(&room_id).is_some_and(RoomUi::is_dm) {
            return Task::none();
        }

        // `PROTOCOL.md` § Rooms and presence: the server drops the leaver's
        // voice slot with the membership, so the engine and the intent go
        // first and the relay is told why.
        let in_its_voice = chat.voice.intent && chat.voice.room_id == room_id;
        let closed = match in_its_voice {
            true => self.leave_voice(),
            false => Task::none(),
        };

        let Screen::Chat(chat) = &mut self.screen else {
            return closed;
        };
        chat.send_or_notice(Command::LeaveRoom { room_id });
        closed
    }

    /// A DM this client already knows opens straight away; otherwise the server
    /// is asked for it and [`ChatState::pending_dm`] waits for the answer.
    fn open_dm(&mut self, user_id: i64) -> Task<Message> {
        let focused = self.focused;
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        if let Some(room_id) = chat.dm_with(user_id) {
            return chat.select_room(room_id, focused);
        }

        chat.pending_dm = Some(user_id);
        chat.send_or_notice(Command::OpenDm { user_id });
        Task::none()
    }

    /// Only my own message, and never a tombstone. The stored tokens go back to
    /// the `@name` that was typed.
    fn start_edit(&mut self, id: i64) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let pairs = chat.user_pairs();
        let Some(message) = chat
            .message(id)
            .filter(|message| message.author_id == chat.member_id && !message.deleted)
        else {
            return Task::none();
        };

        let text = plain_text(&message.text, pairs);
        chat.input = text;
        chat.composer.editing = Some(id);
        chat.composer.reply_to = None;
        chat.mention_query = None;
        operation::focus(Id::new(view::INPUT_ID))
    }

    fn on_file_read(&mut self, result: Result<(String, Blob), String>) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let (file_name, bytes) = match result {
            Ok(read) => read,
            Err(error) => {
                chat.notice = Some(error);
                return Task::none();
            }
        };

        let Some(content_type) = attachments::sniff(&bytes) else {
            chat.notice = Some(NOT_AN_IMAGE.to_owned());
            return Task::none();
        };
        if bytes.len() > attachments::MAX_BYTES {
            chat.notice = Some(TOO_LARGE.to_owned());
            return Task::none();
        }
        let held = chat.composer.attachments.len() + chat.composer.uploading;
        if held >= attachments::MAX_PER_MESSAGE {
            chat.notice = Some(format!(
                "At most {} images per message",
                attachments::MAX_PER_MESSAGE
            ));
            return Task::none();
        }

        let request_id = chat.next_request_id();
        let room_id = chat.current_room.clone();
        tracing::debug!(request_id, %room_id, bytes = bytes.len(), content_type, "uploading an attachment");
        chat.pending_uploads.insert(
            request_id,
            PendingUpload {
                room_id: room_id.clone(),
            },
        );
        chat.composer.uploading += 1;

        if !chat.send_command(Command::UploadAttachment {
            request_id,
            room_id,
            file_name,
            content_type,
            bytes,
        }) {
            chat.pending_uploads.remove(&request_id);
            chat.composer.uploading -= 1;
            chat.notice = Some("Not connected".to_owned());
        }
        Task::none()
    }

    fn join_voice(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let room_id = chat.current_room.clone();
        chat.voice.room_id = room_id.clone();
        chat.voice.intent = true;
        chat.voice.joining = true;
        if !chat.send_command(Command::JoinVoice { room_id }) {
            chat.voice.joining = false;
            chat.notice = Some("Not connected".to_owned());
        }
        Task::none()
    }

    fn leave_voice(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        chat.voice.give_up_intents();
        let room_id = chat.voice.room_id.clone();
        if !chat.send_command(Command::LeaveVoice { room_id }) {
            // The intent stays cleared either way: the server drops the slot
            // when the socket ends.
            chat.notice = Some("Not connected".to_owned());
        }
        chat.voice.close_session()
    }

    fn on_voice_ready(
        &mut self,
        room_id: String,
        host: String,
        port: u16,
        key: MediaKey,
        ssrc: u32,
    ) -> Task<Message> {
        // An empty host means the relay lives wherever the WebSocket goes.
        let host = non_empty(&host).unwrap_or_else(|| self.endpoints.host());

        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        // The answer to a join this room has already moved past.
        if room_id != chat.voice.room_id {
            tracing::debug!(%room_id, joined = %chat.voice.room_id, "ignoring a VoiceReady for another room");
            return Task::none();
        }
        // Left before the answer came back; the server takes the slot away
        // again when it reaches the pending LeaveVoice.
        if !chat.voice.intent {
            chat.voice.joining = false;
            return Task::none();
        }

        // A second VoiceReady carries a new key; the engine holding the old one
        // goes.
        let closing = chat.voice.close_session();
        chat.voice.joining = true;
        chat.voice.pending_ssrc = ssrc;

        let config = MediaConfig {
            host,
            port,
            key: key.0,
            ssrc,
        };
        Task::batch([
            closing,
            Task::perform(
                async move {
                    MediaEngine::connect(config)
                        .await
                        .map_err(|error| error.to_string())
                },
                move |result| {
                    Message::MediaConnected(result.map(|engine| EngineHandoff::new(ssrc, engine)))
                },
            ),
        ])
    }

    fn media_connected(&mut self, handoff: EngineHandoff) -> Task<Message> {
        let settings = self.audio_settings();
        let transmit = self.transmit_settings();
        let cleanup = self.cleanup_settings();
        let peer_audio = self.peer_audio_map();
        // A duplicate of a message already handled carries an empty handoff.
        let Some(engine) = handoff.take() else {
            return Task::none();
        };

        let Screen::Chat(chat) = &mut self.screen else {
            return Task::perform(engine.close(), |()| Message::Noop);
        };
        // Left, or signed out, while the socket was opening.
        if !chat.voice.intent {
            chat.voice.joining = false;
            return Task::perform(engine.close(), |()| Message::Noop);
        }
        // A newer VoiceReady is already being answered, or the connection ended:
        // this engine holds a key the server has replaced.
        if handoff.ssrc != chat.voice.pending_ssrc {
            tracing::debug!(
                ssrc = handoff.ssrc,
                pending = chat.voice.pending_ssrc,
                "dropping a media engine the room moved past"
            );
            return Task::perform(engine.close(), |()| Message::Noop);
        }

        // One session at a time: anything still open goes before this one is
        // installed.
        let closing = chat.voice.close_session();

        let (audio, events) = voice::spawn_audio_thread();
        // Before `Open`, so the input opens straight into the user's chain
        // instead of building the default one first.
        audio.send(AudioCommand::SetCleanup(cleanup));
        audio.send(AudioCommand::Open {
            settings,
            sender: engine.sender(),
            playout: engine.playout(),
        });
        audio.send(AudioCommand::SetTransmit(transmit));
        audio.send(AudioCommand::SetMuted(chat.voice.muted));
        audio.send(AudioCommand::SetDeafened(chat.voice.deafened));
        audio.send(AudioCommand::SetPtt(chat.voice.ptt_held));

        chat.voice.session = Some(VoiceSession {
            ssrc: chat.voice.pending_ssrc,
            engine,
            audio,
            audio_input: None,
            audio_output: None,
        });
        chat.voice.joining = false;
        chat.voice.peer_audio = peer_audio;
        chat.voice.apply_all_peer_audio();

        // The share starts again on the new sender. The `VoiceState` behind
        // the `VoiceReady` normally lands before this, so the watch is usually
        // decided right here rather than left waiting for one.
        let share = chat.voice.resume_share();
        chat.resume_watch();

        let hotkey = self.start_hotkey();
        Task::batch([closing, Task::run(events, Message::Audio), share, hotkey])
    }

    /// Offers the picker. A system that cannot capture at all never gets here:
    /// the sidebar draws no button for it.
    fn open_share_picker(&mut self) -> Task<Message> {
        let capabilities = vorcall_screen::capabilities();
        let audio = self.config.share_audio;
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        if !can_share(&chat.voice, &capabilities) {
            return Task::none();
        }

        chat.dialog = Some(Dialog::SharePicker {
            sources: if capabilities.portal_picker {
                SourcesState::Ready(Vec::new())
            } else {
                SourcesState::Loading
            },
            selected: None,
            audio,
        });
        if capabilities.portal_picker {
            return Task::none();
        }

        // Listing the screens talks to the window server and blocks; on macOS
        // it is also what raises the screen-recording prompt.
        Task::perform(
            tokio::task::spawn_blocking(vorcall_screen::enumerate),
            |joined| {
                let listed = match joined {
                    Ok(listed) => listed.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                Message::SourcesListed(listed)
            },
        )
    }

    fn confirm_share(&mut self) -> Task<Message> {
        let preset = share_preset(&self.config);
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let Some(Dialog::SharePicker {
            selected, audio, ..
        }) = chat.dialog.take()
        else {
            return Task::none();
        };

        let request = CaptureRequest {
            source: selected,
            fps: preset.fps,
            cursor: true,
            audio,
            max_size: capture_box(preset.resolution),
        };
        let Some(session) = chat.voice.session.as_ref() else {
            chat.notice = Some("Join voice first".to_owned());
            return Task::none();
        };
        let sender = session.engine.sender();
        let share_far_end = session.audio.share_far_end();

        chat.voice.share.intent = Some(ShareIntent {
            request: request.clone(),
            preset,
        });
        chat.voice.share.starting = true;
        let thread = chat.voice.share.ensure_thread();
        chat.voice.share.send(ShareCommand::Start {
            request,
            preset,
            sender,
            share_far_end,
        });
        thread
    }

    fn stop_share(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let room_id = chat.voice.room_id.clone();
        chat.voice.share.intent = None;
        chat.voice.share.send(ShareCommand::Stop);
        chat.voice.share.stopped();
        chat.send_or_notice(Command::StopShare { room_id });
        Task::none()
    }

    fn on_share_event(&mut self, event: ShareEvent) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        match event {
            ShareEvent::Started {
                width,
                height,
                output,
                audio,
                backend,
            } => {
                tracing::info!(
                    width,
                    height,
                    output = ?output,
                    audio = ?audio,
                    backend,
                    "a screen share is running"
                );
                chat.voice.share.backend = Some(backend);
                chat.voice.share.audio = audio;
                let room_id = chat.voice.room_id.clone();
                chat.send_or_notice(Command::StartShare {
                    room_id,
                    audio: audio.is_some(),
                });
            }
            ShareEvent::Stats(stats) => {
                tracing::debug!(
                    capture_fps = stats.capture_fps,
                    encode_fps = stats.encode_fps,
                    kbps = stats.kbps,
                    output = ?stats.output,
                    keyframes = stats.keyframes,
                    keyframe_requests = stats.keyframe_requests,
                    dropped = stats.dropped_frames,
                    skipped = stats.skipped_frames,
                    audio_frames = stats.audio_frames,
                    "sharing a screen"
                );
                chat.voice.share.stats = Some(stats);
            }
            ShareEvent::Failed(reason) | ShareEvent::Ended(reason) => {
                // The server is only told about a share it was told about.
                if chat.voice.share.active {
                    let room_id = chat.voice.room_id.clone();
                    chat.send_command(Command::StopShare { room_id });
                }
                chat.voice.share.intent = None;
                chat.voice.share.stopped();
                chat.notice = Some(reason);
            }
        }
        Task::none()
    }

    fn watch_share(&mut self, user_id: i64) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let room_id = chat.voice.room_id.clone();
        if !can_watch(&chat.voice, chat.member_id, &room_id, user_id) {
            return Task::none();
        }
        chat.voice.watch.intent = Some(user_id);
        chat.send_or_notice(Command::WatchShare { room_id, user_id });
        Task::none()
    }

    /// Asks to come off the stream. What is on screen stays until the server
    /// answers with the `WatchState` that ends it.
    fn stop_watching(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let room_id = chat.voice.room_id.clone();
        chat.voice.watch.intent = None;
        chat.send_or_notice(Command::UnwatchShare { room_id });
        Task::none()
    }

    fn on_stage_event(&mut self, event: StageEvent) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        match event {
            StageEvent::Picture { picture, seq } => {
                chat.voice.watch.picture = Some(picture);
                chat.voice.watch.seq = seq;
            }
            StageEvent::Stats {
                decode_fps,
                pictures,
                errors,
                dropped,
            } => {
                tracing::debug!(
                    decode_fps,
                    pictures,
                    errors,
                    dropped,
                    "watching a shared screen"
                );
                chat.voice.watch.stats = Some((decode_fps, pictures, errors));
            }
            StageEvent::Failed(reason) => chat.notice = Some(reason),
        }
        Task::none()
    }

    fn pop_out_stage(&mut self) -> Task<Message> {
        let settings = self.stage_settings();
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let watch = &mut chat.voice.watch;
        if watch.state.is_none() || watch.popped.is_some() {
            return Task::none();
        }
        // The stage leaves the window it was in, so nothing there is fullscreen
        // for it any more.
        let restore = match watch.fullscreen.take() {
            Some(id) => window::set_mode(id, window::Mode::Windowed),
            None => Task::none(),
        };

        let (id, opening) = window::open(settings);
        watch.popped = Some(id);
        Task::batch([restore, opening.discard()])
    }

    fn stage_settings(&self) -> window::Settings {
        window::Settings {
            size: Size::new(STAGE_WINDOW.0, STAGE_WINDOW.1),
            icon: self.icon.clone(),
            platform_specific: platform_specific(false),
            ..window::Settings::default()
        }
    }

    /// The stage takes over whichever window shows it, and gives it back.
    fn toggle_fullscreen(&mut self) -> Task<Message> {
        let main = self.main_window;
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let watch = &mut chat.voice.watch;
        if let Some(id) = watch.fullscreen.take() {
            return window::set_mode(id, window::Mode::Windowed);
        }
        let Some(id) = watch.popped.or(main) else {
            return Task::none();
        };
        watch.fullscreen = Some(id);
        window::set_mode(id, window::Mode::Fullscreen)
    }

    fn in_fullscreen(&self) -> bool {
        match &self.screen {
            Screen::Chat(chat) => chat.voice.watch.fullscreen.is_some(),
            _ => false,
        }
    }

    fn on_audio(&mut self, event: AudioEvent) -> Task<Message> {
        if matches!(event, AudioEvent::Opened { .. }) {
            // An output device opened here, so the chime has one as well: its
            // earlier failure is worth another try.
            self.audio_unavailable = false;
        }

        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        match event {
            AudioEvent::Opened { input, output } => {
                if let Some(session) = &mut chat.voice.session {
                    session.audio_input = input;
                    session.audio_output = Some(output);
                }
            }
            AudioEvent::Failed(reason) => chat.notice = Some(format!("Audio: {reason}")),
            AudioEvent::Closed => {}
            AudioEvent::InputLevel { dbfs, gate_open } => {
                chat.voice.input_level = Some((dbfs, gate_open));
            }
            AudioEvent::Transmitting(transmitting) => chat.voice.transmitting = transmitting,
        }
        Task::none()
    }

    fn toggle_mute(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let voice = &mut chat.voice;
        voice.muted = !voice.muted;
        // Speaking again while deafened means hearing again too.
        if !voice.muted {
            voice.deafened = false;
        }
        voice.apply_flags();
        Task::none()
    }

    fn toggle_deafen(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let voice = &mut chat.voice;
        voice.deafened = !voice.deafened;
        if voice.deafened {
            voice.muted_before_deafen = voice.muted;
            voice.muted = true;
        } else {
            voice.muted = voice.muted_before_deafen;
        }
        voice.apply_flags();
        Task::none()
    }

    /// Key repeat sends a press per repeat, so only the edges reach the audio
    /// thread.
    fn set_ptt(&mut self, held: bool) {
        let Screen::Chat(chat) = &mut self.screen else {
            return;
        };
        if chat.voice.ptt_held == held || chat.voice.session.is_none() {
            return;
        }

        chat.voice.ptt_held = held;
        if let Some(session) = &chat.voice.session {
            session.audio.send(AudioCommand::SetPtt(held));
        }
    }

    fn key_down(&mut self, key: keyboard::Key) -> Task<Message> {
        let escape = matches!(key, keyboard::Key::Named(keyboard::key::Named::Escape));

        if self.capturing_ptt() {
            if escape {
                self.end_ptt_capture();
                return Task::none();
            }
            return self.bind_ptt(binding_from_key(&key));
        }
        if escape {
            // The stage owns Escape while it owns a whole window.
            if self.in_fullscreen() {
                return self.toggle_fullscreen();
            }
            return self.close_overlay();
        }
        if self.key_is_ptt(&key) {
            self.set_ptt(true);
        }
        Task::none()
    }

    fn mouse_down(&mut self, button: mouse::Button) -> Task<Message> {
        if self.capturing_ptt() {
            return self.bind_ptt(binding_from_mouse(button));
        }
        if self.mouse_is_ptt(button) {
            self.set_ptt(true);
        }
        Task::none()
    }

    /// Ends the capture on a new binding and moves the listener over to it. A
    /// key or button no backend can observe is refused instead, and the capture
    /// stays up for the next one.
    fn bind_ptt(&mut self, binding: Option<Binding>) -> Task<Message> {
        let Some(binding) = binding else {
            if let Screen::Chat(chat) = &mut self.screen {
                chat.notice = Some("That key can't be used for push-to-talk".to_owned());
            }
            return Task::none();
        };

        // The old binding is possibly held right now, and its release will no
        // longer match anything.
        self.set_ptt(false);
        self.config.ptt_key = binding.name();
        self.save_config();
        self.end_ptt_capture();

        // Whatever is running still observes the binding that was replaced.
        self.stop_hotkey();
        self.start_hotkey()
    }

    fn end_ptt_capture(&mut self) {
        if let Some(settings) = self.settings() {
            settings.capturing_ptt = false;
        }
    }

    fn key_is_ptt(&self, key: &keyboard::Key) -> bool {
        self.in_window_ptt() && key_matches(key, &self.config.ptt_key)
    }

    fn mouse_is_ptt(&self, button: mouse::Button) -> bool {
        self.in_window_ptt() && mouse_matches(button, &self.config.ptt_key)
    }

    /// Whether the window itself is what drives push-to-talk. It is not while
    /// the system-wide listener runs: that one reports the same press already.
    fn in_window_ptt(&self) -> bool {
        self.config.transmit_mode == TransmitMode::PushToTalk && !self.hotkey_is_global()
    }

    fn hotkey_is_global(&self) -> bool {
        match &self.screen {
            Screen::Chat(chat) => matches!(chat.voice.hotkey_status, HotkeyStatus::Global { .. }),
            _ => false,
        }
    }

    /// Whether a generation is the listener the room is on now. Anything older
    /// belongs to a start or a stream that has been replaced.
    fn hotkey_is_current(&self, generation: u64) -> bool {
        match &self.screen {
            Screen::Chat(chat) => chat.voice.hotkey_generation == generation,
            _ => false,
        }
    }

    /// Starts the system-wide listener for the session that is open. Doing
    /// nothing is the common case: no session, voice activation, or a listener
    /// already running or on its way.
    fn start_hotkey(&mut self) -> Task<Message> {
        if self.config.transmit_mode != TransmitMode::PushToTalk {
            return Task::none();
        }
        let binding = ptt_binding(&self.config);

        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let voice = &mut chat.voice;
        let Some(session) = &voice.session else {
            return Task::none();
        };
        if voice.hotkey.is_some() || voice.hotkey_status == HotkeyStatus::Starting {
            return Task::none();
        }
        let Some(binding) = binding else {
            // Such a key never starts a listener, and the window still holds it.
            voice.hotkey_status = HotkeyStatus::WindowOnly(format!(
                "'{}' works only while the window is focused; bind a listed key for system-wide capture",
                self.config.ptt_key
            ));
            return Task::none();
        };

        let ssrc = session.ssrc;
        voice.hotkey_status = HotkeyStatus::Starting;
        voice.hotkey_generation = voice.hotkey_generation.wrapping_add(1);
        let generation = voice.hotkey_generation;

        // Starting blocks — on Wayland for as long as the compositor keeps its
        // dialog up — so it never runs on this thread.
        let (edges, stream) = mpsc::unbounded();
        // A backend that stops on its own drops its sender, and the end of the
        // stream is the only word of it the app ever gets.
        let messages = stream
            .map(move |edge| Message::Hotkey { generation, edge })
            .chain(futures::stream::once(async move {
                Message::HotkeyEnded { generation }
            }));

        Task::batch([
            Task::perform(
                tokio::task::spawn_blocking(move || Listener::start(binding, edges)),
                move |joined| {
                    let started =
                        joined.unwrap_or_else(|e| Err(Unavailable::Failed(e.to_string())));
                    Message::HotkeyStarted(HotkeyHandoff::new(generation, ssrc, binding, started))
                },
            ),
            Task::stream(messages),
        ])
    }

    fn hotkey_started(&mut self, handoff: HotkeyHandoff) -> Task<Message> {
        // A duplicate of a message already handled carries an empty handoff.
        let Some(started) = handoff.take() else {
            return Task::none();
        };
        // Whatever this was started for is gone; dropping the answer stops the
        // listener it may carry, rather than making it a second one.
        let stale = self.config.transmit_mode != TransmitMode::PushToTalk
            || ptt_binding(&self.config) != Some(handoff.binding);

        let waiting = match &self.screen {
            Screen::Chat(chat) => {
                let voice = &chat.voice;
                voice.hotkey_generation == handoff.generation
                    && voice.hotkey_status == HotkeyStatus::Starting
                    && voice.hotkey.is_none()
                    && voice
                        .session
                        .as_ref()
                        .is_some_and(|session| session.ssrc == handoff.ssrc)
            }
            _ => false,
        };
        if stale || !waiting {
            return Task::none();
        }

        match started {
            Ok(listener) => {
                // The binding may have been pressed while this was starting:
                // the window saw that press, and the backend will drop the
                // release it never saw the press for.
                self.set_ptt(false);

                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.voice.hotkey_status = HotkeyStatus::Global {
                    backend: listener.backend(),
                    trigger: listener.trigger_description(),
                };
                chat.voice.hotkey = Some(listener);
            }
            Err(e) => {
                tracing::info!(error = %e, "no system-wide push-to-talk; the window keeps its own");
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.voice.hotkey_status = HotkeyStatus::WindowOnly(e.to_string());
                }
            }
        }
        Task::none()
    }

    /// A listener that stopped by itself — a session that ended under it, a
    /// portal the user revoked — leaves the window holding push-to-talk.
    fn hotkey_ended(&mut self, generation: u64) -> Task<Message> {
        let running = match &self.screen {
            Screen::Chat(chat) => {
                chat.voice.hotkey_generation == generation
                    && matches!(chat.voice.hotkey_status, HotkeyStatus::Global { .. })
            }
            _ => false,
        };
        if !running {
            return Task::none();
        }

        tracing::info!("the system-wide push-to-talk listener stopped; the window keeps its own");
        // Nothing will report the release of a binding held right now.
        self.set_ptt(false);
        if let Screen::Chat(chat) = &mut self.screen {
            chat.voice.hotkey = None;
            chat.voice.hotkey_status =
                HotkeyStatus::WindowOnly("global capture stopped".to_owned());
        }
        Task::none()
    }

    /// Stops the listener and invalidates a start still on its way.
    fn stop_hotkey(&mut self) {
        let Screen::Chat(chat) = &mut self.screen else {
            return;
        };
        chat.voice.drop_hotkey();
    }

    fn set_transmit_mode(&mut self, mode: TransmitMode) -> Task<Message> {
        if self.config.transmit_mode == mode {
            return Task::none();
        }
        self.config.transmit_mode = mode;
        self.save_config();
        self.push_transmit();

        match mode {
            TransmitMode::PushToTalk => self.start_hotkey(),
            TransmitMode::VoiceActivation => {
                // A binding held as the mode changed would never be released.
                self.set_ptt(false);
                self.stop_hotkey();
                Task::none()
            }
        }
    }

    fn transmit_settings(&self) -> TransmitSettings {
        TransmitSettings {
            mode: self.config.transmit_mode,
            threshold_db: self.config.vad_threshold_db,
        }
    }

    fn push_transmit(&self) {
        let transmit = self.transmit_settings();
        if let Screen::Chat(chat) = &self.screen
            && let Some(session) = &chat.voice.session
        {
            session.audio.send(AudioCommand::SetTransmit(transmit));
        }
    }

    fn cleanup_settings(&self) -> CleanupSettings {
        CleanupSettings {
            noise_suppression: self.config.noise_suppression,
            echo_cancellation: self.config.echo_cancellation,
            auto_gain: self.config.auto_gain,
        }
    }

    fn push_cleanup(&self) {
        let cleanup = self.cleanup_settings();
        if let Screen::Chat(chat) = &self.screen
            && let Some(session) = &chat.voice.session
        {
            session.audio.send(AudioCommand::SetCleanup(cleanup));
        }
    }

    /// The stored tuning, keyed the way the voice room needs it. A key that is
    /// not a user id can only come from a hand-edited file.
    fn peer_audio_map(&self) -> BTreeMap<i64, PeerAudio> {
        self.config
            .peer_audio
            .iter()
            .filter_map(|(user_id, audio)| Some((user_id.parse().ok()?, *audio)))
            .collect()
    }

    /// Keeps the three copies of one peer's tuning together: the configuration,
    /// the mirror the voice room applies from, and the mixer itself. Saving is
    /// the caller's, so a slider drag writes the file once, on release.
    fn update_peer_audio(&mut self, user_id: i64, edit: impl FnOnce(&mut PeerAudio)) {
        let mut audio = self.config.peer_audio(user_id);
        edit(&mut audio);
        self.config.set_peer_audio(user_id, audio);
        // Read back: the setter is what clamps the volume.
        let stored = self.config.peer_audio(user_id);

        if let Screen::Chat(chat) = &mut self.screen {
            chat.voice.peer_audio.insert(user_id, stored);
            chat.voice.apply_peer_audio(user_id);
        }
    }

    /// Escape, and a dialog's own Cancel: one layer at a time, outermost
    /// first, and the settings page once nothing is left over the chat.
    fn close_overlay(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        if chat.dialog.take().is_some() {
            // Whatever a `CreateRoom` still out answers, no dialog is waiting
            // for it now.
            chat.pending_create = None;
            return operation::focus(Id::new(view::INPUT_ID));
        }
        if chat.reacting.take().is_some() || chat.confirm_delete.take().is_some() {
            return Task::none();
        }
        if chat.composer.editing.take().is_some() {
            chat.input.clear();
            chat.mention_query = None;
            return operation::focus(Id::new(view::INPUT_ID));
        }
        if chat.composer.reply_to.take().is_some() {
            return Task::none();
        }
        if matches!(chat.page, Page::Settings(_)) {
            chat.page = Page::Chat;
        }
        Task::none()
    }

    fn voice_tick(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        let voice = &mut chat.voice;
        let Some(session) = voice.session.as_ref() else {
            return Task::none();
        };

        let playout = session.engine.playout();
        let speaking = playout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .speaking(SPEAKING_WINDOW);
        let report = voice.ticks % STATS_EVERY == 0;
        let stats = report.then(|| session.engine.stats());
        let video = session.engine.video_stats();

        voice.ticks = voice.ticks.wrapping_add(1);
        voice.speaking_local = speaking
            .into_iter()
            .filter_map(|ssrc| voice.by_ssrc.get(&ssrc).copied())
            .collect();
        if let Some(stats) = stats {
            voice.stats = stats;
        }

        voice.watch.video = video;
        if report {
            // The viewer measures no bitrate of its own; what the depacketizer
            // took in over the second between two reports is the rate.
            let bytes = voice.watch.video.bytes;
            let received = bytes.saturating_sub(voice.watch.last_bytes);
            voice.watch.last_bytes = bytes;
            voice.watch.kbps = (received as f64 * 8.0
                / 1_000.0
                / (f64::from(STATS_EVERY) * VOICE_TICK.as_secs_f64()))
                as u32;
        }
        Task::none()
    }

    fn open_settings(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        chat.page = Page::Settings(SettingsState::default());

        // Enumerating devices talks to the audio host and blocks.
        Task::perform(tokio::task::spawn_blocking(voice::list_devices), |listed| {
            let lists = listed.unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot list the audio devices");
                voice::DeviceLists {
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                }
            });
            Message::DevicesListed(lists.inputs, lists.outputs)
        })
    }

    fn settings(&mut self) -> Option<&mut SettingsState> {
        match &mut self.screen {
            Screen::Chat(chat) => match &mut chat.page {
                Page::Settings(settings) => Some(settings),
                Page::Chat => None,
            },
            _ => None,
        }
    }

    fn capturing_ptt(&self) -> bool {
        match &self.screen {
            Screen::Chat(chat) => match &chat.page {
                Page::Settings(settings) => settings.capturing_ptt,
                Page::Chat => false,
            },
            _ => false,
        }
    }

    fn audio_settings(&self) -> AudioSettings {
        AudioSettings {
            input: self.config.input_device.clone(),
            output: self.config.output_device.clone(),
        }
    }

    fn push_devices(&self) {
        let settings = self.audio_settings();
        if let Screen::Chat(chat) = &self.screen
            && let Some(session) = &chat.voice.session
        {
            session.audio.send(AudioCommand::SetDevices(settings));
        }
    }

    fn on_event(&mut self, event: Event) -> Task<Message> {
        if !matches!(self.screen, Screen::Chat(_)) {
            return Task::none();
        }

        // The two reasons that end the session for good replace the whole
        // screen, so they are handled before the chat state is borrowed.
        match event {
            Event::Disconnected {
                reason: DisconnectReason::AuthRequired(detail),
                ..
            } => {
                let detail =
                    non_empty(&detail).unwrap_or_else(|| "Sign in again, please.".to_owned());
                Task::batch([
                    self.sign_out(Some(detail)),
                    operation::focus(Id::new(view::USERNAME_ID)),
                ])
            }
            Event::Disconnected {
                reason: DisconnectReason::SessionReplaced,
                ..
            } => Task::batch([
                self.sign_out(Some(
                    "This account connected from another device.".to_owned(),
                )),
                operation::focus(Id::new(view::USERNAME_ID)),
            ]),
            // A kick ends the session and nothing else: the account is free to
            // sign in again, exactly as after a replacement.
            Event::Disconnected {
                reason: DisconnectReason::Kicked(_),
                ..
            } => Task::batch([
                self.sign_out(Some("Disconnected by the admin".to_owned())),
                operation::focus(Id::new(view::USERNAME_ID)),
            ]),
            Event::Disconnected {
                reason: DisconnectReason::Banned(_),
                ..
            } => Task::batch([
                self.sign_out(Some("This account is banned".to_owned())),
                operation::focus(Id::new(view::USERNAME_ID)),
            ]),
            // The first connection is the earliest moment there is a token to
            // check with; every later one is the timer's business.
            Event::Connected { .. } if !self.checked_on_connect => {
                self.checked_on_connect = true;
                let focused = self.focused;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                let applied = chat.apply(event, focused);
                Task::batch([applied, self.check_for_updates()])
            }
            Event::Message(message) => self.on_message(message),
            // Already persisted by the loop; the subscription must not restart,
            // so its identity does not include the tokens.
            Event::SessionUpdated(session) => {
                self.session = Some(session);
                Task::none()
            }
            // The media path needs the endpoints and outlives the chat state.
            Event::VoiceReady {
                room_id,
                host,
                port,
                key,
                ssrc,
            } => self.on_voice_ready(room_id, host, port, key, ssrc),
            other => {
                let focused = self.focused;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.apply(other, focused)
            }
        }
    }

    fn on_message(&mut self, message: ChatMessage) -> Task<Message> {
        let focused = self.focused;
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let id = message.id;
        let room_id = message.room_id.clone();
        let is_current = room_id == chat.current_room;
        let foreign = message.author_id != chat.member_id;
        let author = message.author.clone();
        let body = plain_text(&message.text, chat.user_pairs());
        let attachments: Vec<i64> = message
            .attachments
            .iter()
            .map(|attachment| attachment.id)
            .collect();
        let mentioned = mentions::mentions_me(&message.mention_ids, chat.member_id);
        // Handed back below; the room borrow and the user map cannot be held
        // at the same time.
        let users = std::mem::take(&mut chat.users);
        let member_id = chat.member_id;

        let room = chat.message_room(&room_id);
        let at_bottom = room.at_bottom;
        // A DM reads like a mention: it is addressed to this account and
        // nobody else.
        let mention = mentioned || room.is_dm();
        let title = match room.is_dm() {
            true => author.clone(),
            false => format!("{} · {author}", room.title(&users, member_id)),
        };
        let unread = unread_rule(foreign, is_current, at_bottom, focused);

        room.last_message_id = room.last_message_id.max(id);
        room.messages.insert(id, message);
        trim(&mut room.messages);
        // At the bottom the anchor already keeps the newest message in view.
        if foreign && !at_bottom {
            room.pending_new = room.pending_new.saturating_add(1);
        }
        if unread {
            room.unread = room.unread.saturating_add(1);
            if mention {
                room.mentions = room.mentions.saturating_add(1);
            }
        }

        chat.users = users;
        chat.notice = None;
        if !unread && is_current && at_bottom && focused {
            chat.schedule_mark_read();
        }

        let images = match is_current {
            true => Task::batch(attachments.iter().map(|id| chat.ensure_image(*id))),
            false => Task::none(),
        };

        let viewing = focused && is_current;
        if !foreign || !notification_rule(mention, focused, viewing) {
            return images;
        }
        // A message that is nothing but images still has to say something.
        let body = match body.trim().is_empty() && !attachments.is_empty() {
            true => "[image]".to_owned(),
            false => notify::preview(&body),
        };
        Task::batch([images, self.toast(title, body)])
    }

    fn toast(&mut self, author: String, preview: String) -> Task<Message> {
        if self
            .last_toast
            .is_some_and(|at| at.elapsed() < TOAST_INTERVAL)
        {
            return Task::none();
        }
        self.last_toast = Some(Instant::now());

        if self.config.sound {
            self.chime();
        }
        if self.config.notifications {
            Task::perform(notify::show(author, preview), |()| Message::Noop)
        } else {
            Task::none()
        }
    }

    /// Opens the output device on the first chime that needs it. A device that
    /// will not open is not retried: it does not come back mid-run, and the
    /// attempt blocks the UI thread.
    fn chime(&mut self) {
        if self.audio.is_none() && !self.audio_unavailable {
            match audio::open() {
                Ok(audio) => self.audio = Some(audio),
                Err(e) => {
                    tracing::warn!(error = %e, "no audio output; the chime is off for this run");
                    self.audio_unavailable = true;
                }
            }
        }
        if let Some(audio) = &self.audio {
            audio.chime();
        }
    }

    /// Opens the offer on entering the chat screen, never over another dialog,
    /// until it is answered; from then on the Settings button is the only way
    /// to send. Keeping the offer until an answer is what lets it survive a
    /// stale stored session, whose chat screen gives way to sign-in before
    /// anyone can act on it.
    fn offer_crash_report(&mut self) {
        if !self.crash_offer || self.session.is_none() {
            return;
        }
        let Screen::Chat(chat) = &mut self.screen else {
            return;
        };
        if chat.dialog.is_none() {
            chat.dialog = Some(Dialog::CrashReport);
        }
    }

    fn close_crash_offer(&mut self) {
        self.crash_offer = false;
        if let Screen::Chat(chat) = &mut self.screen
            && matches!(chat.dialog, Some(Dialog::CrashReport))
        {
            chat.dialog = None;
        }
    }

    /// Sends the log files and every crash report, one upload at a time; each
    /// crash report is deleted as soon as its own upload succeeds, so a report
    /// the server's hourly limit cut short resumes where it stopped on the
    /// next press. The token is a clone taken now: a 401 comes back as a
    /// failure the user can press again, the same way an update check does.
    fn report_problem(&mut self) -> Task<Message> {
        let Some(session) = &self.session else {
            return Task::none();
        };
        if self.reporting() {
            return Task::none();
        }

        let endpoints = self.endpoints.clone();
        let token = session.access_token.clone();
        if let Some(settings) = self.settings() {
            settings.report = ReportState::Sending;
        }

        Task::perform(send_report(endpoints, token), Message::ReportFinished)
    }

    fn reporting(&self) -> bool {
        match &self.screen {
            Screen::Chat(chat) => match &chat.page {
                Page::Settings(settings) => matches!(settings.report, ReportState::Sending),
                Page::Chat => false,
            },
            _ => false,
        }
    }

    fn on_report_finished(&mut self, result: Result<usize, String>) -> Task<Message> {
        let (state, notice) = report_outcome(result);
        if let Some(settings) = self.settings() {
            settings.report = state;
        }
        if let Screen::Chat(chat) = &mut self.screen {
            chat.notice = Some(notice);
        }
        Task::none()
    }

    /// Starts one check, unless one is already going or this build does not
    /// update itself. The token is a clone taken now: a 401 comes back as a
    /// failure and the next tick tries again with whatever the loop rotated to.
    fn check_for_updates(&mut self) -> Task<Message> {
        if self.update.busy() {
            return Task::none();
        }
        let Some(session) = &self.session else {
            return Task::none();
        };

        let token = session.access_token.clone();
        let checker = Checker {
            endpoints: self.endpoints.clone(),
            keys: self.update_keys.clone(),
            current: Version::current(),
            platform: update::platform(),
        };
        self.update = UpdateState::Checking;
        Task::run(check_stream(checker, token), std::convert::identity)
    }

    fn on_update_progress(&mut self, progress: Progress) -> Task<Message> {
        // Only what a live check reports: a message that arrives late must not
        // undo a restart already on its way.
        if !matches!(
            self.update,
            UpdateState::Checking | UpdateState::Downloading { .. }
        ) {
            return Task::none();
        }

        self.update = match progress {
            Progress::Checking => UpdateState::Checking,
            Progress::Downloading {
                version,
                received,
                total,
                required,
            } => {
                // `Failed` and `Restarting` carry no manifest of their own; this
                // is what they fall back on.
                self.force_required = required;
                UpdateState::Downloading {
                    version,
                    received,
                    total,
                    required,
                }
            }
        };
        Task::none()
    }

    fn on_update_result(&mut self, result: Result<Arc<Outcome>, String>) -> Task<Message> {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(message) => {
                tracing::warn!(%message, "the update check failed");
                self.update = UpdateState::Failed {
                    message,
                    required: self.force_required,
                    at: Instant::now(),
                };
                return Task::none();
            }
        };

        match &*outcome {
            Outcome::UpToDate { .. } => {
                self.force_required = false;
                self.update = UpdateState::UpToDate { at: Instant::now() };
                Task::none()
            }
            Outcome::NoBuildForPlatform { .. } => {
                self.force_required = false;
                self.update = UpdateState::NoBuild {
                    platform: update::platform(),
                };
                Task::none()
            }
            Outcome::Downloaded(ready) => {
                let ready = ready.clone();
                self.force_required = ready.required;
                if !ready.manifest.notes.trim().is_empty() {
                    self.update_notes =
                        Some((ready.manifest.version, ready.manifest.notes.clone()));
                }

                // A download that landed outside the install directory is the
                // one case a required update still waits for a person.
                let restart_now = ready.required && !ready.manual;
                self.update = UpdateState::Ready {
                    ready,
                    dismissed: false,
                };
                if restart_now {
                    return self.restart_for_update();
                }
                Task::none()
            }
        }
    }

    /// Leaves the voice room and closes the media engine before the binary is
    /// replaced: the swap ends this process without another chance to say so.
    fn restart_for_update(&mut self) -> Task<Message> {
        let UpdateState::Ready { ready, .. } = &self.update else {
            return Task::none();
        };
        if ready.manual {
            return Task::none();
        }

        self.pending_restart = Some(ready.clone());
        self.update = UpdateState::Restarting;

        let leaving = self.leave_voice();
        let closing = match &mut self.screen {
            Screen::Chat(chat) => chat.voice.close_session(),
            _ => Task::none(),
        };
        let swap = Task::perform(tokio::time::sleep(RESTART_GRACE), |()| Message::ApplyUpdate);
        Task::batch([leaving, closing]).chain(swap)
    }

    fn apply_update(&mut self) -> Task<Message> {
        // The swap only ever follows the restart above, which is what left the
        // voice room.
        if !matches!(self.update, UpdateState::Restarting) {
            return Task::none();
        }
        let Some(ready) = self.pending_restart.take() else {
            return Task::none();
        };

        let args: Vec<_> = std::env::args_os().skip(1).collect();
        match update::swap::apply_and_relaunch(&ready.file, &args) {
            // On unix the process image is already gone by now; this is Windows,
            // where the replacement is up and this one is in its way.
            Ok(update::swap::Relaunched::Spawned) => iced::exit(),
            Err(e) => {
                tracing::warn!(error = %e, "the update was not applied");
                self.update = UpdateState::Failed {
                    message: format!("restart by hand: {e}"),
                    required: self.force_required,
                    at: Instant::now(),
                };
                Task::none()
            }
        }
    }

    /// The creature is drawn while a check runs and on the required screen, and
    /// only the chat screen has room for either.
    fn creature_visible(&self) -> bool {
        matches!(self.screen, Screen::Chat(_))
            && (self.update.shows_creature() || self.update.shows_required(self.force_required))
    }

    fn save_config(&self) {
        if let Err(e) = config::save(&self.config) {
            tracing::warn!(error = %e, "cannot persist the configuration");
        }
    }
}

impl ChatState {
    fn new() -> Self {
        let mut rooms = BTreeMap::new();
        rooms.insert(GENERAL_ROOM.to_owned(), RoomUi::new(general_room()));

        Self {
            rooms,
            current_room: GENERAL_ROOM.to_owned(),
            users: BTreeMap::new(),
            member_id: 0,
            input: String::new(),
            status: Status::Connecting,
            cmd: None,
            notice: None,
            dialog: None,
            pending_create: None,
            voice: VoiceUi {
                room_id: GENERAL_ROOM.to_owned(),
                ..VoiceUi::default()
            },
            voice_rosters: BTreeMap::new(),
            page: Page::Chat,
            composer: Composer::default(),
            hovered: None,
            reacting: None,
            confirm_delete: None,
            images: BTreeMap::new(),
            next_request: 0,
            pending_uploads: BTreeMap::new(),
            pending_fetches: BTreeMap::new(),
            mark_read_due: None,
            mention_query: None,
            pending_dm: None,
            user_pairs: Vec::new(),
        }
    }

    /// The room in view, which is the one every message action is about.
    pub fn current(&self) -> Option<&RoomUi> {
        self.rooms.get(&self.current_room)
    }

    fn current_mut(&mut self) -> Option<&mut RoomUi> {
        self.rooms.get_mut(&self.current_room)
    }

    /// The public rooms this account belongs to, by name.
    pub fn joined_rooms(&self) -> impl Iterator<Item = &RoomUi> {
        let mut rooms: Vec<&RoomUi> = self
            .rooms
            .values()
            .filter(|room| !room.is_dm() && room.joined(self.member_id))
            .collect();
        rooms.sort_by_key(|room| room.room.name.to_lowercase());
        rooms.into_iter()
    }

    /// The open conversations, the one that spoke last at the top.
    pub fn dms(&self) -> impl Iterator<Item = &RoomUi> {
        let mut rooms: Vec<&RoomUi> = self
            .rooms
            .values()
            .filter(|room| room.is_dm() && !room.hidden)
            .collect();
        rooms.sort_by_key(|room| std::cmp::Reverse(room.last_message_id));
        rooms.into_iter()
    }

    /// The public rooms this account could join but has not.
    pub fn browsable(&self) -> impl Iterator<Item = &RoomUi> {
        let mut rooms: Vec<&RoomUi> = self
            .rooms
            .values()
            .filter(|room| room.is_public() && !room.joined(self.member_id))
            .collect();
        rooms.sort_by_key(|room| room.room.name.to_lowercase());
        rooms.into_iter()
    }

    /// What the window title counts.
    pub fn total_unread(&self) -> u32 {
        self.rooms
            .values()
            .fold(0u32, |total, room| total.saturating_add(room.unread))
    }

    /// Whether a member is online in the room in view.
    pub fn online(&self, user_id: i64) -> bool {
        self.current()
            .is_some_and(|room| room.online.contains(&user_id))
    }

    /// The speaking dot: what the server saw, what the local decoder hears, and
    /// this member while push-to-talk is going out.
    pub fn speaking(&self, user_id: i64) -> bool {
        self.voice.speaking_server.contains(&user_id)
            || self.voice.speaking_local.contains(&user_id)
            || (user_id == self.member_id && self.voice.transmitting)
    }

    /// One message of the room in view; every action is about one of those.
    fn message(&self, id: i64) -> Option<&ChatMessage> {
        self.current().and_then(|room| room.messages.get(&id))
    }

    /// Whether this account already reacted to that message with that emoji,
    /// which is what turns the same press into a removal.
    fn reacted(&self, message_id: i64, emoji: &str) -> bool {
        self.message(message_id).is_some_and(|message| {
            message.reactions.iter().any(|reaction| {
                reaction.emoji == emoji && reaction.user_ids.contains(&self.member_id)
            })
        })
    }

    /// The known DM with one user, whether or not it is hidden.
    fn dm_with(&self, user_id: i64) -> Option<String> {
        self.rooms
            .values()
            .find(|room| {
                room.is_dm()
                    && room.room.member_ids.contains(&user_id)
                    && room.room.member_ids.contains(&self.member_id)
            })
            .map(|room| room.room.room_id.clone())
    }

    /// Every user as [`mentions`] wants them, which is what every drawn row
    /// and every stored message is read through.
    pub fn user_pairs(&self) -> &[(i64, String)] {
        &self.user_pairs
    }

    /// Rebuilds that list; every write to `users` ends with this.
    fn refresh_user_pairs(&mut self) {
        self.user_pairs = self
            .users
            .iter()
            .map(|(user_id, member)| (*user_id, member.username.clone()))
            .collect();
    }

    /// The pointer left a row. An exit that arrives after the next row's enter
    /// must not take that row's actions away with it.
    fn unhover(&mut self, id: i64) {
        if self.hovered == Some(id) {
            self.hovered = None;
        }
    }

    /// Puts one room in view: its history is asked for the first time it is
    /// opened, and opening it is what reads it.
    fn select_room(&mut self, room_id: String, focused: bool) -> Task<Message> {
        self.current_room = room_id.clone();
        self.pending_dm = None;
        self.hovered = None;
        self.reacting = None;
        self.confirm_delete = None;
        // An edit owns the input as well: that text is another room's message.
        if self.composer.editing.is_some() {
            self.input.clear();
            self.mention_query = None;
        }
        self.composer = Composer::default();

        let mut loading = None;
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.hidden = false;
            room.at_bottom = true;
            room.pending_new = 0;
            if !room.loaded && !room.loading {
                room.loading = true;
                loading = Some(room_id.clone());
            }
        }
        if let Some(room_id) = loading
            && !self.send_command(Command::LoadHistory { room_id })
            && let Some(room) = self.current_mut()
        {
            room.loading = false;
        }

        if focused {
            self.schedule_mark_read();
        }
        let images = self.ensure_room_images();
        Task::batch([images, snap_to_bottom()])
    }

    /// Remembers that the room in view has been read up to its newest message.
    /// The tick is what reports it, at most once a second.
    fn schedule_mark_read(&mut self) {
        let room_id = self.current_room.clone();
        let Some(room) = self.rooms.get(&room_id) else {
            return;
        };
        let newest = room
            .messages
            .keys()
            .next_back()
            .copied()
            .unwrap_or_default()
            .max(room.last_message_id);
        if newest <= 0 {
            return;
        }

        match &mut self.mark_read_due {
            // A read of another room was still waiting: it was true when it was
            // taken, so it goes out now rather than being dropped.
            Some((waiting, _)) if *waiting != room_id => {
                self.flush_mark_read();
                self.mark_read_due = Some((room_id, newest));
            }
            Some((_, message_id)) => *message_id = (*message_id).max(newest),
            None => self.mark_read_due = Some((room_id, newest)),
        }
    }

    /// Reports the waiting read and clears the counters it covers.
    fn flush_mark_read(&mut self) {
        let Some((room_id, message_id)) = self.mark_read_due.take() else {
            return;
        };
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.unread = 0;
            room.mentions = 0;
        }
        self.send_command(Command::MarkRead {
            room_id,
            message_id,
        });
    }

    /// The handle one transfer is asked for under; the answer carries it back.
    fn next_request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    /// Starts loading one attachment's pixels: from the cache when they are
    /// there, from the server otherwise. Doing nothing is the common case.
    pub fn ensure_image(&mut self, id: i64) -> Task<Message> {
        if self.images.contains_key(&id) {
            return Task::none();
        }
        self.images.insert(id, ImageState::Loading);

        if let Some(path) = images::cached_path(id)
            && path.is_file()
        {
            return decode_task(id, move || {
                std::fs::read(&path)
                    .map(Blob::from)
                    .map_err(|e| e.to_string())
            });
        }

        let request_id = self.next_request_id();
        self.pending_fetches.insert(request_id, id);
        if !self.send_command(Command::FetchAttachment { request_id, id }) {
            self.pending_fetches.remove(&request_id);
            self.images.insert(id, ImageState::Failed);
        }
        Task::none()
    }

    /// Every attachment of the room in view, so a row is drawn with its image
    /// rather than fetching one once it is on screen.
    fn ensure_room_images(&mut self) -> Task<Message> {
        let Some(room) = self.current() else {
            return Task::none();
        };
        let ids: Vec<i64> = room
            .messages
            .values()
            .flat_map(|message| message.attachments.iter().map(|attachment| attachment.id))
            .collect();
        Task::batch(ids.into_iter().map(|id| self.ensure_image(id)))
    }

    /// Hands one command to the connection loop; `false` means there is no loop
    /// or its queue is full.
    fn send_command(&mut self, command: Command) -> bool {
        match self.cmd.as_mut() {
            Some(sender) => sender
                .try_send(command)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "cannot hand the command to the connection"),
                )
                .is_ok(),
            None => false,
        }
    }

    /// The same, saying so in the status line when the connection is gone.
    fn send_or_notice(&mut self, command: Command) {
        if !self.send_command(command) {
            self.notice = Some("Not connected".to_owned());
        }
    }

    /// The room a frame names, created empty when it is not known yet:
    /// `PROTOCOL.md` § Rooms and presence sends `RoomState` for every room
    /// before the `RoomList` that describes them.
    fn room_entry(&mut self, room_id: &str) -> &mut RoomUi {
        self.rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| RoomUi::new(placeholder_room(room_id)))
    }

    /// The room a live message lands in. A closed conversation comes back with
    /// it: a new message is exactly what reopens one.
    fn message_room(&mut self, room_id: &str) -> &mut RoomUi {
        let room = self.room_entry(room_id);
        room.hidden = false;
        room
    }

    /// Acts on the watch intent a reconnect kept, against the roster this
    /// session has. Without one yet the answer waits for the next `VoiceState`
    /// for the joined room, which is what asks again.
    fn resume_watch(&mut self) {
        let decision = watch_resume(
            self.voice.watch.intent,
            &self.voice.sharing,
            self.voice.roster_seen,
        );
        self.voice.watch.resume_pending = decision == WatchResume::Pending;

        match decision {
            WatchResume::Request(user_id) => {
                let room_id = self.voice.room_id.clone();
                self.send_command(Command::WatchShare { room_id, user_id });
            }
            WatchResume::Clear => self.voice.watch.intent = None,
            WatchResume::Pending | WatchResume::Nothing => {}
        }
    }

    /// Every connection event except the two that end the session and the live
    /// message, which both need more than the chat state.
    fn apply(&mut self, event: Event, focused: bool) -> Task<Message> {
        match event {
            Event::Ready(sender) => {
                self.cmd = Some(sender);
                Task::none()
            }
            Event::Connecting => {
                self.status = Status::Connecting;
                Task::none()
            }
            Event::Connected { member_id, .. } => {
                self.status = Status::Connected;
                self.member_id = member_id;
                self.notice = None;
                // The room membership did not survive the reconnect; the intent
                // did.
                if self.voice.intent {
                    self.voice.joining = true;
                    let room_id = self.voice.room_id.clone();
                    self.send_command(Command::JoinVoice { room_id });
                }
                // History is per room and on demand, so the room in view and
                // every room already loaded ask again: that is what fills the
                // gap the reconnect left.
                let asking: BTreeSet<String> = std::iter::once(self.current_room.clone())
                    .chain(
                        self.rooms
                            .iter()
                            .filter(|(_, room)| room.loaded)
                            .map(|(room_id, _)| room_id.clone()),
                    )
                    .collect();
                for room_id in asking {
                    if let Some(room) = self.rooms.get_mut(&room_id) {
                        room.loading = true;
                    }
                    self.send_command(Command::LoadHistory { room_id });
                }
                Task::none()
            }
            Event::Disconnected { reason, retry_in } => {
                for room in self.rooms.values_mut() {
                    room.online.clear();
                    room.loading = false;
                    room.loading_older = false;
                }
                self.status = match reason {
                    DisconnectReason::Unauthorized => Status::Unauthorized,
                    other => match retry_in {
                        Some(delay) => Status::Reconnecting {
                            in_secs: delay.as_secs().max(1),
                        },
                        None => Status::Disconnected(other.to_string()),
                    },
                };
                // The media key belongs to the session that just ended, and no
                // engine still on its way answers for it.
                self.voice.joining = false;
                self.voice.pending_ssrc = 0;
                self.voice.members.clear();
                self.voice.sharing.clear();
                self.voice.by_ssrc.clear();
                self.voice.speaking_server.clear();
                // Voice membership is per connection, in every room.
                self.voice_rosters.clear();
                self.voice.close_session()
            }
            Event::History {
                room_id,
                messages,
                has_more,
            } => {
                let current = room_id == self.current_room;
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    tracing::debug!(%room_id, "ignoring history for an unknown room");
                    return Task::none();
                };
                room.history_error = None;
                room.loaded = true;
                room.loading = false;
                room.merge(messages);
                trim(&mut room.messages);
                // A reconnect starts the page state over.
                room.loading_older = false;
                room.has_older = has_more;
                room.at_bottom = true;
                room.pending_new = 0;

                if !current {
                    return Task::none();
                }
                let images = self.ensure_room_images();
                if focused {
                    self.schedule_mark_read();
                }
                Task::batch([images, snap_to_bottom()])
            }
            Event::HistoryFailed { room_id, error } => {
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    tracing::debug!(%room_id, "ignoring a history failure for an unknown room");
                    return Task::none();
                };
                room.loading = false;
                room.history_error = Some(error);
                Task::none()
            }
            // No scroll operation: the bottom anchor keeps the viewport where
            // it is while the list grows upwards.
            Event::OlderPage {
                room_id,
                messages,
                has_more,
            } => {
                let current = room_id == self.current_room;
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    tracing::debug!(%room_id, "ignoring an older page for an unknown room");
                    return Task::none();
                };
                room.merge(messages);
                room.loading_older = false;
                room.has_older = has_more && room.messages.len() < MESSAGE_LIMIT;
                if current {
                    return self.ensure_room_images();
                }
                Task::none()
            }
            Event::OlderFailed { room_id, error } => {
                if let Some(room) = self.rooms.get_mut(&room_id) {
                    room.loading_older = false;
                }
                self.notice = Some(error);
                Task::none()
            }
            Event::Users(list) => {
                self.users = list
                    .into_iter()
                    .map(|member| (member.user_id, member))
                    .collect();
                self.refresh_user_pairs();
                Task::none()
            }
            Event::UsersFailed(detail) => {
                self.notice = Some(detail);
                Task::none()
            }
            Event::RoomList(entries) => {
                self.merge_room_list(entries);
                Task::none()
            }
            Event::RoomUpdated(room) => self.merge_room(room, focused),
            Event::RoomState { room_id, members } => {
                let room = self.room_entry(&room_id);
                room.online = members.iter().map(|member| member.user_id).collect();
                for member in members {
                    self.users.entry(member.user_id).or_insert(member);
                }
                self.refresh_user_pairs();
                self.select_pending_dm(&room_id, focused)
            }
            Event::MemberJoined { room_id, member } => {
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    tracing::debug!(%room_id, "ignoring presence for an unknown room");
                    return Task::none();
                };
                room.online.insert(member.user_id);
                self.users.insert(member.user_id, member);
                self.refresh_user_pairs();
                Task::none()
            }
            Event::MemberLeft { room_id, user_id } => {
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    tracing::debug!(%room_id, "ignoring presence for an unknown room");
                    return Task::none();
                };
                room.online.remove(&user_id);
                Task::none()
            }
            Event::MessageEdited(message) => {
                if let Some(room) = self.rooms.get_mut(&message.room_id)
                    && room.messages.contains_key(&message.id)
                {
                    room.messages.insert(message.id, message);
                }
                Task::none()
            }
            Event::MessageDeleted { room_id, id } => {
                // The tombstone stays in history and in pages, so the row keeps
                // its place and only loses its content.
                if let Some(message) = self
                    .rooms
                    .get_mut(&room_id)
                    .and_then(|room| room.messages.get_mut(&id))
                {
                    message.deleted = true;
                    message.text.clear();
                    message.mention_ids.clear();
                    message.reactions.clear();
                    message.attachments.clear();
                }
                if self.composer.editing == Some(id) {
                    self.composer.editing = None;
                    self.input.clear();
                }
                if self.confirm_delete == Some(id) {
                    self.confirm_delete = None;
                }
                Task::none()
            }
            Event::ReactionsChanged {
                room_id,
                message_id,
                reactions,
            } => {
                if let Some(message) = self
                    .rooms
                    .get_mut(&room_id)
                    .and_then(|room| room.messages.get_mut(&message_id))
                {
                    message.reactions = reactions;
                }
                Task::none()
            }
            Event::AttachmentUploaded {
                request_id,
                attachment,
            } => {
                let Some(pending) = self.pending_uploads.remove(&request_id) else {
                    tracing::debug!(request_id, "ignoring an upload nothing is waiting for");
                    return Task::none();
                };
                self.composer.uploading = self.composer.uploading.saturating_sub(1);
                if pending.room_id == self.current_room {
                    self.composer.attachments.push(attachment);
                } else {
                    // Never linked to a message, so the server sweeps it.
                    tracing::debug!(
                        id = attachment.id,
                        room_id = %pending.room_id,
                        "dropping an upload for a room no longer being written in"
                    );
                }
                Task::none()
            }
            Event::UploadFailed { request_id, error } => {
                if self.pending_uploads.remove(&request_id).is_some() {
                    self.composer.uploading = self.composer.uploading.saturating_sub(1);
                }
                self.notice = Some(error);
                Task::none()
            }
            Event::AttachmentFetched {
                request_id,
                id,
                bytes,
            } => {
                self.pending_fetches.remove(&request_id);
                decode_task(id, move || {
                    images::store(id, &bytes);
                    Ok(bytes)
                })
            }
            Event::FetchFailed {
                request_id,
                id,
                error,
            } => {
                self.pending_fetches.remove(&request_id);
                tracing::warn!(id, %error, "cannot fetch an attachment");
                self.images.insert(id, ImageState::Failed);
                Task::none()
            }
            Event::ServerError { code, detail, .. } => {
                // Rejoining on the next reconnect would only repeat these two.
                if self.voice.joining
                    && (code == ErrorCode::NotAMember as i32
                        || code == ErrorCode::VoiceUnavailable as i32)
                {
                    self.voice.intent = false;
                    self.voice.joining = false;
                }
                // What the New room dialog asked for is answered in it: the
                // status line behind a modal is not where a refused name goes.
                let refused_name = code == ErrorCode::RoomExists as i32
                    || code == ErrorCode::InvalidRoomName as i32;
                if let Some(Dialog::NewRoom { error, .. }) = &mut self.dialog
                    && refused_name
                {
                    *error = Some(detail);
                    self.pending_create = None;
                    return Task::none();
                }
                self.notice = Some(detail);
                Task::none()
            }
            Event::SendDropped => {
                self.notice = Some("Not connected".to_owned());
                Task::none()
            }
            Event::VoiceState { room_id, members } => {
                let roster = self.voice_rosters.entry(room_id.clone()).or_default();
                // Split so the retain reads one field while it writes the other.
                let VoiceRoster {
                    members: held,
                    speaking,
                    sharing,
                } = roster;
                *held = members
                    .iter()
                    .map(|member| (member.user_id, member.clone()))
                    .collect();
                *sharing = sharing_map(&members);
                speaking.retain(|user_id| held.contains_key(user_id));

                if room_id != self.voice.room_id {
                    return Task::none();
                }
                self.voice.reset_members(members);

                // The media path came up before this roster did, so the watch a
                // reconnect kept is still waiting to be judged.
                if self.voice.watch.resume_pending {
                    self.resume_watch();
                }
                Task::none()
            }
            Event::VoiceMemberJoined { room_id, member } => {
                let roster = self.voice_rosters.entry(room_id.clone()).or_default();
                roster.speaking.remove(&member.user_id);
                set_sharing(&mut roster.sharing, &member);
                roster.members.insert(member.user_id, member.clone());

                if room_id == self.voice.room_id {
                    self.voice.insert_member(member);
                }
                Task::none()
            }
            Event::VoiceMemberLeft { room_id, user_id } => {
                if let Some(roster) = self.voice_rosters.get_mut(&room_id) {
                    roster.members.remove(&user_id);
                    roster.speaking.remove(&user_id);
                    roster.sharing.remove(&user_id);
                }
                if room_id != self.voice.room_id {
                    return Task::none();
                }
                self.voice.remove_member(user_id);
                // The server dropped this client's own slot — a leave from
                // another device. Nothing is on the relay any more, so neither
                // the engine nor the sidebar may claim there is.
                if user_id == self.member_id && self.voice.intent {
                    self.voice.give_up_intents();
                    return self.voice.close_session();
                }
                Task::none()
            }
            Event::Speaking {
                room_id,
                user_id,
                speaking,
            } => {
                let roster = self.voice_rosters.entry(room_id.clone()).or_default();
                if speaking {
                    roster.speaking.insert(user_id);
                } else {
                    roster.speaking.remove(&user_id);
                }

                if room_id == self.voice.room_id {
                    if speaking {
                        self.voice.speaking_server.insert(user_id);
                    } else {
                        self.voice.speaking_server.remove(&user_id);
                    }
                }
                Task::none()
            }
            Event::ShareStarted {
                room_id,
                user_id,
                audio,
            } => {
                self.voice_rosters
                    .entry(room_id.clone())
                    .or_default()
                    .sharing
                    .insert(user_id, audio);
                if room_id != self.voice.room_id {
                    return Task::none();
                }
                self.voice.sharing.insert(user_id, audio);
                // The server has the share, so the capture running here is one
                // other people can now ask to watch.
                if user_id == self.member_id {
                    self.voice.share.active = true;
                    self.voice.share.starting = false;
                }
                Task::none()
            }
            Event::ShareStopped { room_id, user_id } => {
                if let Some(roster) = self.voice_rosters.get_mut(&room_id) {
                    roster.sharing.remove(&user_id);
                }
                if room_id != self.voice.room_id {
                    return Task::none();
                }
                self.voice.sharing.remove(&user_id);
                // The server ended this client's own share — the voice slot
                // went, or a limit was reached — so the capture goes with it.
                // A frame about a share already stopped from here is stale, and
                // would otherwise kill the one started right after it.
                if user_id == self.member_id && self.voice.share.active {
                    self.voice.share.send(ShareCommand::Stop);
                    self.voice.share.intent = None;
                    self.voice.share.stopped();
                }
                // Nothing to go back to once that screen is gone.
                if self.voice.watch.intent == Some(user_id) {
                    self.voice.watch.intent = None;
                }
                Task::none()
            }
            Event::WatchState { room_id, user_id } => {
                if room_id != self.voice.room_id {
                    return Task::none();
                }
                match user_id {
                    Some(user_id) => self.voice.start_watching(user_id),
                    None => self.voice.leave_stage(),
                }
            }
            Event::ShareWatchers { room_id, count } => {
                if room_id != self.voice.room_id {
                    return Task::none();
                }
                let (paused, keyframe) = pause_decision(self.voice.share.watchers, count);
                self.voice.share.watchers = count;
                self.voice.share.send(ShareCommand::SetPaused(paused));
                if keyframe {
                    self.voice.share.send(ShareCommand::ForceKeyframe);
                }
                Task::none()
            }
            // All three need more than the chat state, so [`App::on_event`]
            // takes them before this is reached.
            Event::Message(_) | Event::SessionUpdated(_) | Event::VoiceReady { .. } => Task::none(),
        }
    }

    /// Rebuilds the room list, keeping what this client knows about each room
    /// that survived: the counters and the newest id come from the server, the
    /// buffers and everything the reader did stay.
    fn merge_room_list(&mut self, entries: Vec<RoomEntry>) {
        let mut rooms = BTreeMap::new();
        for entry in entries {
            let Some(room) = entry.room else {
                continue;
            };
            let room_id = room.room_id.clone();
            let mut held = match self.rooms.remove(&room_id) {
                Some(mut held) => {
                    held.room = room;
                    held
                }
                None => RoomUi::new(room),
            };
            held.unread = counter(entry.unread);
            held.mentions = counter(entry.mentions);
            held.last_message_id = entry.last_message_id;
            rooms.insert(room_id, held);
        }
        self.rooms = rooms;

        // A room that is gone — one left from another device — takes the view
        // back to the room nobody can leave.
        if !self.rooms.contains_key(&self.current_room) {
            self.current_room = GENERAL_ROOM.to_owned();
        }
    }

    /// One room's shared facts, whether or not it was known.
    fn merge_room(&mut self, room: Room, focused: bool) -> Task<Message> {
        let room_id = room.room_id.clone();
        let created = created_by_me(self.pending_create.as_deref(), &room, self.member_id);

        match self.rooms.get_mut(&room_id) {
            Some(held) => held.room = room,
            None => {
                self.rooms.insert(room_id.clone(), RoomUi::new(room));
            }
        }
        let held = &self.rooms[&room_id];
        let joined = held.joined(self.member_id);
        let opened_dm = joined
            && held.is_dm()
            && self
                .pending_dm
                .is_some_and(|user_id| held.room.member_ids.contains(&user_id));

        // The room this account just asked for: the dialog is done, and the
        // room is what the window shows next.
        if created {
            self.pending_create = None;
            if matches!(self.dialog, Some(Dialog::NewRoom { .. })) {
                self.dialog = None;
            }
            return self.select_room(room_id, focused);
        }
        if opened_dm {
            return self.select_room(room_id, focused);
        }
        // No longer a member of the room in view — left from another device.
        if room_id == self.current_room && !joined {
            return self.select_room(GENERAL_ROOM.to_owned(), focused);
        }
        Task::none()
    }

    /// Opens the DM an `OpenDm` asked for once the server names it. A DM that
    /// already existed is answered with a resync and no `RoomUpdated`.
    fn select_pending_dm(&mut self, room_id: &str, focused: bool) -> Task<Message> {
        let Some(user_id) = self.pending_dm else {
            return Task::none();
        };
        let theirs = self
            .rooms
            .get(room_id)
            .is_some_and(|room| room.is_dm() && room.room.member_ids.contains(&user_id));
        if !theirs {
            return Task::none();
        }
        self.select_room(room_id.to_owned(), focused)
    }
}

impl RoomUi {
    fn new(room: Room) -> Self {
        Self {
            room,
            last_message_id: 0,
            unread: 0,
            mentions: 0,
            messages: BTreeMap::new(),
            loaded: false,
            loading: false,
            has_older: false,
            loading_older: false,
            history_error: None,
            at_bottom: true,
            pending_new: 0,
            online: BTreeSet::new(),
            hidden: false,
        }
    }

    /// Membership is the persistent list the server keeps, not who is online.
    pub fn joined(&self, me: i64) -> bool {
        self.room.member_ids.contains(&me)
    }

    pub fn is_dm(&self) -> bool {
        self.room.kind == RoomKind::Dm as i32
    }

    fn is_public(&self) -> bool {
        self.room.kind == RoomKind::Public as i32
    }

    /// What the room is called on screen: `#name` for a public room, `@other`
    /// for a conversation.
    pub fn title(&self, users: &BTreeMap<i64, Member>, me: i64) -> String {
        if !self.is_dm() {
            let name = match self.room.name.is_empty() {
                true => self.room.room_id.as_str(),
                false => self.room.name.as_str(),
            };
            return format!("#{name}");
        }

        let other = self
            .room
            .member_ids
            .iter()
            .copied()
            .find(|user_id| *user_id != me)
            .unwrap_or(me);
        let name = users
            .get(&other)
            .map_or("unknown", |member| member.username.as_str());
        format!("@{name}")
    }

    fn merge(&mut self, messages: Vec<ChatMessage>) {
        for message in messages {
            self.last_message_id = self.last_message_id.max(message.id);
            self.messages.insert(message.id, message);
        }
    }
}

impl VoiceUi {
    /// Drops the media path and leaves every intent alone: a reconnect rejoins,
    /// shares again and asks to watch again with them. Closing the engine is
    /// what stops its tasks; dropping it does not.
    fn close_session(&mut self) -> Task<Message> {
        self.ptt_held = false;
        self.transmitting = false;
        self.input_level = None;
        self.speaking_local.clear();
        self.stats = Stats::default();
        self.expanded_member = None;
        // Dropping the listener stops it: nothing outside a session observes
        // the binding.
        self.drop_hotkey();

        // Whatever `sharing` holds describes the session that is going.
        self.roster_seen = false;
        // The capture feeds the engine that is going, and the decoder reads a
        // stream that ends with it.
        self.share.send(ShareCommand::Stop);
        self.share.stopped();
        self.watch.decoder = None;
        self.forget_stream();
        let stage = self.close_stage_windows();

        let Some(session) = self.session.take() else {
            return stage;
        };
        tracing::debug!(
            ssrc = session.ssrc,
            input = ?session.audio_input,
            output = ?session.audio_output,
            "closing the voice session"
        );
        session.audio.send(AudioCommand::Close);
        Task::batch([
            stage,
            Task::perform(session.engine.close(), |()| Message::Noop),
        ])
    }

    /// Everything the stage shows about a stream that is over. The watch intent
    /// is not part of it.
    fn forget_stream(&mut self) {
        self.watch.state = None;
        self.watch.picture = None;
        self.watch.seq = 0;
        self.watch.stats = None;
        self.watch.video = VideoStats::default();
        self.watch.last_bytes = 0;
        self.watch.kbps = 0;
    }

    /// Puts the pop-out away and gives a window that went fullscreen for the
    /// stage its decorations back.
    fn close_stage_windows(&mut self) -> Task<Message> {
        let restore = match self.watch.fullscreen.take() {
            Some(id) => window::set_mode(id, window::Mode::Windowed),
            None => Task::none(),
        };
        let close = match self.watch.popped.take() {
            Some(id) => window::close(id),
            None => Task::none(),
        };
        Task::batch([restore, close])
    }

    /// What a leave gives up: the voice slot and both screen-share intents, so
    /// none of them is asserted again on the next connection.
    fn give_up_intents(&mut self) {
        self.intent = false;
        self.joining = false;
        self.share.intent = None;
        self.watch.intent = None;
    }

    /// Starts the share a reconnect kept, on the new engine's sender.
    fn resume_share(&mut self) -> Task<Message> {
        let Some(intent) = &self.share.intent else {
            return Task::none();
        };
        let (request, preset) = (intent.request.clone(), intent.preset);

        let Some(session) = &self.session else {
            return Task::none();
        };
        let sender = session.engine.sender();
        let share_far_end = session.audio.share_far_end();

        self.share.starting = true;
        let thread = self.share.ensure_thread();
        self.share.send(ShareCommand::Start {
            request,
            preset,
            sender,
            share_far_end,
        });
        thread
    }

    /// The server put this client on a sharer's stream. One decode thread
    /// serves a whole session: the engine hands its access units out once, so
    /// switching sharers only re-points the engine at another ssrc.
    fn start_watching(&mut self, user_id: i64) -> Task<Message> {
        let ssrc = self.members.get(&user_id).map(|member| member.ssrc);
        if ssrc.is_none() {
            tracing::debug!(user_id, "watching a sharer the roster does not name");
        }
        let volume = self.watch.volume;
        let first = self.watch.decoder.is_none();

        let Some(session) = &self.session else {
            return Task::none();
        };
        session.engine.watch(ssrc);
        {
            let playout = session.engine.playout();
            let mut playout = playout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            playout.set_share_gain(volume);
        }
        let units = first.then(|| session.engine.take_access_units()).flatten();

        self.forget_stream();
        self.watch.state = Some(user_id);

        let Some(units) = units else {
            if first {
                tracing::warn!("this session has no access units left to decode");
            }
            return Task::none();
        };
        let (decoder, events) = share::spawn_decode_thread(units);
        self.watch.decoder = Some(decoder);
        Task::run(events, Message::Stage)
    }

    /// The server took this client off every stream. The decoder stays: it is
    /// the session's, and it simply starves until the next watch.
    fn leave_stage(&mut self) -> Task<Message> {
        if let Some(session) = &self.session {
            session.engine.watch(None);
            let playout = session.engine.playout();
            playout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove_share();
        }
        self.forget_stream();
        self.close_stage_windows()
    }

    /// The watched share's volume, live: the mixer holds it, and only the end
    /// of a drag writes it to the configuration.
    fn set_share_volume(&mut self, volume: f32) {
        self.watch.volume = volume.clamp(0.0, SHARE_VOLUME_MAX);
        let volume = self.watch.volume;

        let Some(session) = &self.session else {
            return;
        };
        let playout = session.engine.playout();
        playout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_share_gain(volume);
    }

    /// Stops the listener and moves past it: no start still in flight, no
    /// edge stream and no answer from either is acted on afterwards.
    fn drop_hotkey(&mut self) {
        self.hotkey = None;
        self.hotkey_status = HotkeyStatus::Off;
        self.hotkey_generation = self.hotkey_generation.wrapping_add(1);
    }

    fn apply_flags(&self) {
        let Some(session) = &self.session else {
            return;
        };
        session.audio.send(AudioCommand::SetMuted(self.muted));
        session.audio.send(AudioCommand::SetDeafened(self.deafened));
    }

    fn reset_members(&mut self, members: Vec<VoiceMember>) {
        self.roster_seen = true;
        self.sharing = sharing_map(&members);
        self.by_ssrc = members
            .iter()
            .map(|member| (member.ssrc, member.user_id))
            .collect();
        self.members = members
            .into_iter()
            .map(|member| (member.user_id, member))
            .collect();
        self.speaking_server
            .retain(|user_id| self.members.contains_key(user_id));
        self.speaking_local
            .retain(|user_id| self.members.contains_key(user_id));
        self.apply_all_peer_audio();
    }

    fn insert_member(&mut self, member: VoiceMember) {
        // A Speaking(true) can outlive the member it was about; the rejoin
        // starts silent rather than lit up for good.
        let user_id = member.user_id;
        self.speaking_server.remove(&user_id);
        self.speaking_local.remove(&user_id);
        self.by_ssrc.insert(member.ssrc, user_id);
        set_sharing(&mut self.sharing, &member);
        self.members.insert(user_id, member);
        // A rejoin brings a new ssrc, so the tuning has to follow it.
        self.apply_peer_audio(user_id);
    }

    fn remove_member(&mut self, user_id: i64) {
        if let Some(member) = self.members.remove(&user_id) {
            self.by_ssrc.remove(&member.ssrc);
        }
        self.speaking_server.remove(&user_id);
        self.speaking_local.remove(&user_id);
        self.sharing.remove(&user_id);
        if self.expanded_member == Some(user_id) {
            self.expanded_member = None;
        }
    }

    /// The local volume and mute for one member, as the sidebar draws them.
    pub fn peer_audio(&self, user_id: i64) -> PeerAudio {
        self.peer_audio.get(&user_id).copied().unwrap_or_default()
    }

    /// Pushes one member's tuning into the mixer. The playout is shared with
    /// the audio thread, so the lock is held for one member only.
    fn apply_peer_audio(&self, user_id: i64) {
        let (Some(session), Some(member)) = (&self.session, self.members.get(&user_id)) else {
            return;
        };
        let playout = session.engine.playout();
        let mut playout = playout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        apply_tuning(&mut playout, member, self.peer_audio(user_id));
    }

    /// The same for everyone in the room, under one lock: a fresh session and a
    /// fresh member list both start from the stored tuning.
    fn apply_all_peer_audio(&self) {
        let Some(session) = &self.session else {
            return;
        };
        let playout = session.engine.playout();
        let mut playout = playout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for member in self.members.values() {
            apply_tuning(&mut playout, member, self.peer_audio(member.user_id));
        }
    }
}

/// One member's local volume and mute, as the mixer holds them.
fn apply_tuning(playout: &mut Playout, member: &VoiceMember, audio: PeerAudio) {
    playout.set_gain(member.ssrc, audio.volume);
    playout.set_muted(member.ssrc, audio.muted);
}

/// A fresh chat screen, with the preferences its state mirrors already in it.
fn chat_screen(config: &Config) -> Screen {
    let mut chat = ChatState::new();
    chat.voice.watch.volume = config.share_volume;
    Screen::Chat(Box::new(chat))
}

/// Who shares in a room, and whether that share carries audio.
fn sharing_map(members: &[VoiceMember]) -> BTreeMap<i64, bool> {
    members
        .iter()
        .filter(|member| member.sharing)
        .map(|member| (member.user_id, member.share_audio))
        .collect()
}

/// One member's share, as the frame that carried them describes it.
fn set_sharing(sharing: &mut BTreeMap<i64, bool>, member: &VoiceMember) {
    if member.sharing {
        sharing.insert(member.user_id, member.share_audio);
    } else {
        sharing.remove(&member.user_id);
    }
}

/// Whether a screen can be shared from here: a live voice session, nothing of
/// ours already on the wire, and a backend that can capture at all.
pub fn can_share(voice: &VoiceUi, capabilities: &Capabilities) -> bool {
    capabilities.backend != NO_CAPTURE
        && voice.session.is_some()
        && !voice.share.active
        && !voice.share.starting
}

/// Whether `user_id`'s share can be watched from the room `room_id` names.
pub fn can_watch(voice: &VoiceUi, me: i64, room_id: &str, user_id: i64) -> bool {
    watch_rule(
        voice.session.is_some() && voice.room_id == room_id,
        voice.sharing.contains_key(&user_id),
        user_id == me,
    )
}

/// Watching takes a live voice session in the room whose roster is on screen,
/// a peer sharing in it, and somebody other than oneself.
fn watch_rule(in_this_voice_room: bool, sharing: bool, is_me: bool) -> bool {
    in_this_voice_room && sharing && !is_me
}

/// What a fresh media session does about a watch intent a reconnect kept.
#[derive(Debug, PartialEq, Eq)]
enum WatchResume {
    /// That screen is still being shared: ask for the stream again.
    Request(i64),
    /// The roster is in and it is not: there is nothing to go back to.
    Clear,
    /// No roster for this session yet, so nothing can be judged.
    Pending,
    Nothing,
}

/// A reconnect keeps the watch intent; whether it is worth asking for again is
/// the fresh roster's word, and without one the answer has to wait for it.
fn watch_resume(
    intent: Option<i64>,
    sharing: &BTreeMap<i64, bool>,
    roster_seen: bool,
) -> WatchResume {
    let Some(user_id) = intent else {
        return WatchResume::Nothing;
    };
    if !roster_seen {
        return WatchResume::Pending;
    }
    if sharing.contains_key(&user_id) {
        WatchResume::Request(user_id)
    } else {
        WatchResume::Clear
    }
}

/// What a new watcher count means for the capture: nobody watching pauses the
/// encoder, and whoever arrives after a pause can only start at a keyframe.
fn pause_decision(previous: u32, now: u32) -> (bool, bool) {
    (now == 0, previous == 0 && now > 0)
}

/// Everyone sharing in the joined room, as the stage's picker lists them. This
/// client is never in it: its own screen is not watched here.
pub fn sharer_list(voice: &VoiceUi, me: i64) -> Vec<(i64, String)> {
    voice
        .sharing
        .keys()
        .filter(|user_id| **user_id != me)
        .filter_map(|user_id| {
            let member = voice.members.get(user_id)?;
            Some((*user_id, member.username.clone()))
        })
        .collect()
}

/// Whose screen the stage is showing.
pub fn sharer_name(chat: &ChatState) -> &str {
    let Some(user_id) = chat.voice.watch.state else {
        return UNKNOWN_SHARER;
    };
    chat.voice
        .members
        .get(&user_id)
        .map(|member| member.username.as_str())
        .or_else(|| {
            chat.users
                .get(&user_id)
                .map(|member| member.username.as_str())
        })
        .unwrap_or(UNKNOWN_SHARER)
}

/// The preset a share starts with. Anything the configuration cannot name is
/// the default rather than a refusal to share.
fn share_preset(config: &Config) -> Preset {
    Preset {
        resolution: config.share_resolution.parse().unwrap_or(Resolution::P720),
        fps: FrameRate::from_hz(config.share_fps).unwrap_or(FrameRate::F30),
        bitrate_kbps: config.share_bitrate_kbps,
    }
}

/// The frame the capture backend is asked to aim for where it can scale for us.
/// A share at the source's own resolution asks for nothing.
fn capture_box(resolution: Resolution) -> Option<(u32, u32)> {
    match resolution {
        Resolution::Source => None,
        Resolution::P720 => Some((1280, 720)),
        Resolution::P1080 => Some((1920, 1080)),
        Resolution::P1440 => Some((2560, 1440)),
        Resolution::P2160 => Some((3840, 2160)),
    }
}

/// What the preset would have asked the encoder for on its own, which is where
/// a manual bitrate starts.
fn auto_bitrate_kbps(config: &Config) -> u32 {
    let preset = Preset {
        bitrate_kbps: None,
        ..share_preset(config)
    };
    preset.bitrate_kbps(capture_box(preset.resolution).unwrap_or(preset::MAX_SOURCE))
}

/// What this machine can share, in the sentence under the settings. `running`
/// is the backend a live share actually got, which is the one worth naming.
pub fn share_sentence(capabilities: &Capabilities, running: Option<&'static str>) -> String {
    if capabilities.backend == NO_CAPTURE {
        return "This system cannot share a screen.".to_owned();
    }

    let mut parts = vec![
        format!("Capture: {}", running.unwrap_or(capabilities.backend)),
        if capabilities.windows {
            "whole screens and single windows".to_owned()
        } else {
            "whole screens only".to_owned()
        },
        if capabilities.audio {
            "shared audio carries what this machine plays, without Vorcall's own voices".to_owned()
        } else {
            "no audio with the share on this system".to_owned()
        },
    ];
    if capabilities.portal_picker {
        parts.push("the system dialog picks the screen or window".to_owned());
    }
    #[cfg(target_os = "macos")]
    parts.push(
        "Screen Recording permission is required, and re-granted after every update".to_owned(),
    );
    parts.join(" · ")
}

impl Screen {
    fn login(username: String) -> Self {
        Self::Login {
            username,
            password: String::new(),
            error: None,
            busy: false,
        }
    }
}

/// The identity of the connection subscription: who is signed in, and how many
/// times that has changed. A token rotation inside the loop leaves it alone.
struct ConnectionInput {
    endpoints: Endpoints,
    session: Session,
    generation: u64,
}

impl Hash for ConnectionInput {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session.user_id.hash(state);
        self.generation.hash(state);
    }
}

// `Subscription::run_with` takes a plain fn pointer, so everything this stream
// needs is cloned out of `input` rather than captured.
fn connect(input: &ConnectionInput) -> impl Stream<Item = Event> + use<> {
    let endpoints = input.endpoints.clone();
    let session = input.session.clone();

    iced::stream::channel(64, async move |mut output| {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        if futures::SinkExt::send(&mut output, Event::Ready(cmd_tx))
            .await
            .is_err()
        {
            return;
        }
        connection::run(endpoints, session, cmd_rx, output).await;
    })
}

/// One check as a stream of messages: what the core reports on the way, then
/// the outcome. [`Task::run`] turns it into the task [`App::check_for_updates`]
/// hands back.
fn check_stream(checker: Checker, token: String) -> impl Stream<Item = Message> {
    iced::stream::channel(16, async move |mut output| {
        let mut throttle = ProgressThrottle::default();
        let mut reports = output.clone();

        let result = update::check_and_download(&checker, &token, |progress| {
            if !throttle.admit(&progress) {
                return;
            }
            // A full queue only means the window is behind on a number the next
            // report replaces anyway.
            let _ = reports.try_send(Message::UpdateProgress(progress));
        })
        .await;

        let _ = futures::SinkExt::send(
            &mut output,
            Message::UpdateResult(result.map(Arc::new).map_err(|e| e.to_string())),
        )
        .await;
    })
}

/// The downloader reports every chunk it writes and every report redraws the
/// window, so only a step worth looking at is passed on: one percent of the
/// release, at most [`PROGRESS_STEP`], and the last chunk whatever its size.
#[derive(Default)]
struct ProgressThrottle {
    last: Option<u64>,
}

impl ProgressThrottle {
    fn admit(&mut self, progress: &Progress) -> bool {
        let Progress::Downloading {
            received, total, ..
        } = progress
        else {
            self.last = None;
            return true;
        };
        let (received, total) = (*received, *total);

        let step = (total / 100).clamp(1, PROGRESS_STEP);
        let done = total > 0 && received >= total;
        if let Some(last) = self.last
            && !done
            && received.saturating_sub(last) < step
        {
            return false;
        }

        self.last = Some(received);
        true
    }
}

/// Wayland reads the window icon from the desktop entry whose id this matches,
/// not from the pixels the window carries.
#[cfg(target_os = "linux")]
fn platform_specific(_splash: bool) -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        application_id: "vorcall".to_owned(),
        ..Default::default()
    }
}

/// The splash is not a window anyone switches to.
#[cfg(windows)]
fn platform_specific(splash: bool) -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        skip_taskbar: splash,
        ..Default::default()
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_specific(_splash: bool) -> window::settings::PlatformSpecific {
    Default::default()
}

/// `listen_with` already drops redraw requests; everything else this does not
/// name is dropped here, so no message reaches `update` for it.
fn ui_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Window(window::Event::Focused) => Some(Message::Focus(true)),
        iced::Event::Window(window::Event::Unfocused) => Some(Message::Focus(false)),
        // A file dropped on the window goes the same way as one the dialog
        // picked.
        iced::Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::FilesPicked(vec![path]))
        }
        // Push-to-talk needs both edges of every key, and Escape is told apart
        // in `update` so no key press is dropped here.
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => {
            Some(Message::KeyDown(key))
        }
        iced::Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
            Some(Message::KeyUp(key))
        }
        // Only the three buttons a binding may use: every left click would
        // otherwise wake `update` twice for nothing.
        iced::Event::Mouse(mouse::Event::ButtonPressed(button))
            if binding_from_mouse(button).is_some() =>
        {
            Some(Message::MouseDown(button))
        }
        iced::Event::Mouse(mouse::Event::ButtonReleased(button))
            if binding_from_mouse(button).is_some() =>
        {
            Some(Message::MouseUp(button))
        }
        _ => None,
    }
}

/// Under `Anchor::End` a relative offset of zero is the bottom of the list.
fn snap_to_bottom() -> Task<Message> {
    operation::snap_to(Id::new(view::MESSAGES_ID), RelativeOffset::START)
}

fn trim(messages: &mut BTreeMap<i64, ChatMessage>) {
    while messages.len() > MESSAGE_LIMIT {
        if messages.pop_first().is_none() {
            break;
        }
    }
}

/// The room every account is in, so the window has something to draw before the
/// first `RoomList` arrives.
fn general_room() -> Room {
    Room {
        room_id: GENERAL_ROOM.to_owned(),
        kind: RoomKind::Public as i32,
        name: GENERAL_ROOM.to_owned(),
        member_ids: Vec::new(),
        created_by: 0,
    }
}

/// A room named by a frame that arrived before its facts. Its kind stays
/// unspecified, so it is neither listed nor browsable until they do.
fn placeholder_room(room_id: &str) -> Room {
    Room {
        room_id: room_id.to_owned(),
        ..Room::default()
    }
}

/// A counter the server sends as an `i64`, as the window holds it.
fn counter(value: i64) -> u32 {
    value.clamp(0, i64::from(u32::MAX)) as u32
}

/// Whether a message that just landed leaves its room unread. Reading it takes
/// having that room in view, at the bottom, in a focused window.
/// Whether one `RoomUpdated` is the room the New room dialog just asked for.
/// The name is what matches it: `PROTOCOL.md` § Rooms and presence sends the
/// creator a `RoomState` first, which has already put a placeholder in `rooms`,
/// so the room being new is no sign at all.
fn created_by_me(pending: Option<&str>, room: &Room, me: i64) -> bool {
    room.created_by == me
        && room.member_ids.contains(&me)
        && room.kind != RoomKind::Dm as i32
        && pending == Some(room.name.as_str())
}

fn unread_rule(foreign: bool, is_current: bool, at_bottom: bool, focused: bool) -> bool {
    foreign && (!is_current || !at_bottom || !focused)
}

/// Whether a foreign message is worth a notification. Anything addressed to
/// this account interrupts unless it is already on screen; everything else only
/// while the window is away.
fn notification_rule(mention_or_dm: bool, focused: bool, viewing: bool) -> bool {
    if mention_or_dm { !viewing } else { !focused }
}

/// Where the `@…` being typed starts, which is the end of the last whitespace
/// before it.
fn fragment_start(input: &str) -> usize {
    input
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(at, character)| at + character.len_utf8())
}

/// The name being typed after an `@`, without the `@`. `None` as soon as the
/// fragment is finished or is not a mention at all.
fn mention_query(input: &str) -> Option<String> {
    let fragment = &input[fragment_start(input)..];
    let name = fragment.strip_prefix('@')?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Puts the picked name where the fragment was, ready for the next word.
fn mention_replace(input: &str, username: &str) -> String {
    format!("{}@{username} ", &input[..fragment_start(input)])
}

/// A stored text as a person reads and edits it: every `<@id>` token becomes
/// the name it stands for.
fn plain_text(text: &str, users: &[(i64, String)]) -> String {
    mentions::segments(text, users)
        .into_iter()
        .map(|segment| match segment {
            Segment::Text(text) => text,
            Segment::Mention { username, .. } => format!("@{username}"),
        })
        .collect()
}

/// Decoding runs on a blocking thread: an 8 MiB image is milliseconds the UI
/// thread does not have.
fn decode_task(
    id: i64,
    read: impl FnOnce() -> Result<Blob, String> + Send + 'static,
) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || images::decode(&read()?)),
        move |joined| {
            let decoded = joined.unwrap_or_else(|e| Err(e.to_string()));
            Message::ImageDecoded(
                id,
                decoded.map(|(width, height, pixels)| Handle::from_rgba(width, height, pixels)),
            )
        },
    )
}

/// Trims the attachment cache once per run, off the UI thread.
fn prune_image_cache() -> Task<Message> {
    Task::perform(
        async {
            if let Err(e) = tokio::task::spawn_blocking(|| images::prune(IMAGE_CACHE_LIMIT)).await {
                tracing::warn!(error = %e, "the attachment cache was not pruned");
            }
        },
        |()| Message::Noop,
    )
}

/// The native file dialog. Cancelling it picks nothing, which is not an error.
async fn pick_images() -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
        .pick_files()
        .await
        .map(|handles| {
            handles
                .iter()
                .map(|handle| handle.path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads one picked or dropped file off the disk, on a blocking thread.
fn read_file(path: PathBuf) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || read_image(&path)),
        |joined| {
            let read = joined.unwrap_or_else(|e| Err(e.to_string()));
            Message::FileRead(read.map(|(name, bytes)| (name, Blob::from(bytes))))
        },
    )
}

fn read_image(path: &Path) -> Result<(String, Vec<u8>), String> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("image")
        .to_owned();
    // Asked before the read: nothing the server would refuse belongs in memory.
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read that file: {e}"))?
        .len();
    if size > attachments::MAX_BYTES as u64 {
        return Err(TOO_LARGE.to_owned());
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read that file: {e}"))?;
    Ok((name, bytes))
}

/// The binding an in-window key event stands for. A `Named` key is stored
/// under its variant name, which is exactly what [`Binding::parse`] reads;
/// location is deliberately ignored, so either Control key holds the same talk.
fn binding_from_key(key: &keyboard::Key) -> Option<Binding> {
    match key {
        keyboard::Key::Named(named) => Binding::parse(&format!("{named:?}")),
        keyboard::Key::Character(character) => Binding::parse(character.as_str()),
        keyboard::Key::Unidentified => None,
    }
}

/// The primary and secondary buttons are deliberately absent: binding them
/// would make ordinary clicking transmit.
fn binding_from_mouse(button: mouse::Button) -> Option<Binding> {
    match button {
        mouse::Button::Back => Some(Binding::Mouse(MouseButton::Back)),
        mouse::Button::Forward => Some(Binding::Mouse(MouseButton::Forward)),
        mouse::Button::Middle => Some(Binding::Mouse(MouseButton::Middle)),
        _ => None,
    }
}

fn ptt_binding(config: &Config) -> Option<Binding> {
    Binding::parse(&config.ptt_key)
}

/// Whether an in-window key edge is the bound push-to-talk input.
///
/// A key stored by an older build can be outside the grammar the backends
/// understand — "Enter", "ArrowUp", "ç" — and those keep working by name: no
/// listener ever starts on one, so this is the only thing that can match them.
fn key_matches(key: &keyboard::Key, ptt: &str) -> bool {
    if let Some(binding) = Binding::parse(ptt) {
        return binding_from_key(key) == Some(binding);
    }
    match key {
        keyboard::Key::Named(named) => format!("{named:?}") == ptt,
        keyboard::Key::Character(character) => character.eq_ignore_ascii_case(ptt),
        keyboard::Key::Unidentified => false,
    }
}

fn mouse_matches(button: mouse::Button, ptt: &str) -> bool {
    binding_from_mouse(button).is_some_and(|binding| Binding::parse(ptt) == Some(binding))
}

/// What the settings page and the sidebar hint call the push-to-talk binding.
/// A key from an older build is spelled the way it was stored.
pub fn key_label(ptt: &str) -> String {
    Binding::parse(ptt).map_or_else(|| ptt.to_owned(), |binding| binding.label())
}

/// What the settings page says about where push-to-talk edges come from.
pub fn hotkey_sentence(status: &HotkeyStatus, mode: TransmitMode) -> String {
    if mode == TransmitMode::VoiceActivation {
        return "Off (voice activation)".to_owned();
    }
    match status {
        HotkeyStatus::Off => "Global capture starts when you join voice".to_owned(),
        HotkeyStatus::Starting => "Starting global capture…".to_owned(),
        HotkeyStatus::Global { backend, trigger } => {
            let mut sentence = match backend {
                Backend::WindowsHook => "Global (low-level hook)".to_owned(),
                Backend::MacEventTap => "Global (event tap)".to_owned(),
                Backend::X11Raw => "Global (X11)".to_owned(),
                // The compositor is what decided what it bound, and it does not
                // have to be what was asked for.
                Backend::WaylandPortal => "Global via portal".to_owned(),
            };
            if let Some(trigger) = trigger {
                sentence.push_str(": ");
                sentence.push_str(trigger);
            }
            sentence
        }
        HotkeyStatus::WindowOnly(reason) => format!("Window only: {reason}"),
    }
}

/// How far a download has come, for the progress bar. An unknown total reads as
/// nothing done rather than as finished.
pub fn progress_fraction(received: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (received as f64 / total as f64).clamp(0.0, 1.0) as f32
}

/// The pick list offers the system default as an entry; the configuration
/// spells it `None`.
fn device_choice(name: String) -> Option<String> {
    (name != view::SYSTEM_DEFAULT).then_some(name)
}

/// What a finished report leaves behind: the settings page's state, and the
/// line the chat shows either way.
fn report_outcome(result: Result<usize, String>) -> (ReportState, String) {
    match result {
        Ok(count) => (
            ReportState::Sent(count),
            format!("Report sent ({count} files)"),
        ),
        Err(error) => (
            ReportState::Failed(error.clone()),
            format!("Report failed: {error}"),
        ),
    }
}

/// One file of a report, as it goes over the wire.
struct ReportFile {
    kind: &'static str,
    name: String,
    path: PathBuf,
    body: Vec<u8>,
}

/// Collects the files off the UI thread and uploads them one at a time,
/// stopping at the first refusal: a half-sent report is still worth reading.
/// A crash file goes away as soon as its own upload lands, so a report the
/// server's hourly limit cut short picks up where it stopped on the next
/// press instead of replaying files already sent.
async fn send_report(endpoints: Endpoints, token: String) -> Result<usize, String> {
    let files = tokio::task::spawn_blocking(collect_report)
        .await
        .map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err("there is nothing to send".to_owned());
    }

    let count = files.len();
    let bytes: usize = files.iter().map(|file| file.body.len()).sum();
    tracing::info!(files = count, bytes, "sending a problem report");

    for file in files {
        report::upload(&endpoints, &token, file.kind, &file.name, file.body)
            .await
            .map_err(|failure| describe(&failure))?;
        if file.kind != "crash" {
            continue;
        }
        let removed = tokio::task::spawn_blocking(move || std::fs::remove_file(file.path))
            .await
            .map_err(|e| e.to_string())
            .and_then(|result| result.map_err(|e| e.to_string()));
        if let Err(error) = removed {
            tracing::warn!(error = %error, "could not delete a sent crash report");
        }
    }

    Ok(count)
}

/// The live log, the generation behind it and every crash report on disk. A
/// file that cannot be read is left out rather than losing the whole report.
fn collect_report() -> Vec<ReportFile> {
    let mut files = Vec::new();

    if let Some(live) = diagnostics::log_path() {
        let mut rotated = live.clone().into_os_string();
        rotated.push(".1");
        for path in [live, PathBuf::from(rotated)] {
            files.extend(read_report(&path, "log"));
        }
    }
    for path in diagnostics::crash_reports() {
        files.extend(read_report(&path, "crash"));
    }

    files
}

fn read_report(path: &Path, kind: &'static str) -> Option<ReportFile> {
    let body = std::fs::read(path).ok()?;
    let name = path.file_name().and_then(OsStr::to_str)?.to_owned();

    Some(ReportFile {
        kind,
        name,
        path: path.to_path_buf(),
        body: report::tail(body, report::MAX_BYTES),
    })
}

/// The one place an [`ApiFailure`] becomes something a person can act on.
fn describe(failure: &ApiFailure) -> String {
    match failure {
        ApiFailure::AuthChallenge(detail) | ApiFailure::Status(401, detail) => {
            non_empty(detail).unwrap_or_else(|| "Invalid username or password".to_owned())
        }
        ApiFailure::Status(403, _) => "Invite code is invalid, used or expired".to_owned(),
        ApiFailure::Status(409, _) => "That username is taken".to_owned(),
        ApiFailure::Throttled(secs) => format!("Too many attempts, try again in {secs}s"),
        ApiFailure::StaleKey => "Unauthorized: rebuild the client with the current key".to_owned(),
        ApiFailure::Transport(_) => "Cannot reach the server".to_owned(),
        ApiFailure::Malformed(_) => UNEXPECTED.to_owned(),
        ApiFailure::Status(_, detail) => non_empty(detail).unwrap_or_else(|| UNEXPECTED.to_owned()),
    }
}

fn non_empty(detail: &str) -> Option<String> {
    let detail = detail.trim();
    (!detail.is_empty()).then(|| detail.to_owned())
}

fn validate_username(username: &str) -> Result<(), String> {
    let length = username.chars().count();
    if length == 0 {
        return Err("Enter a username.".to_owned());
    }
    if length > USERNAME_MAX {
        return Err(format!("At most {USERNAME_MAX} characters."));
    }
    if username.chars().any(char::is_control) {
        return Err("No control characters.".to_owned());
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), String> {
    let length = password.chars().count();
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
        return Err(format!(
            "Password must be {PASSWORD_MIN} to {PASSWORD_MAX} characters."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use iced::keyboard::key::Named;
    use vorcall_core::update::{Asset, Manifest};
    use vorcall_hotkey::Key;

    use super::*;

    fn named(key: Named) -> keyboard::Key {
        keyboard::Key::Named(key)
    }

    fn typed(character: &str) -> keyboard::Key {
        keyboard::Key::Character(character.into())
    }

    #[test]
    fn a_named_key_binds_under_its_variant_name() {
        assert_eq!(
            binding_from_key(&named(Named::Control)),
            Some(Binding::Key(Key::Control))
        );
        assert_eq!(
            binding_from_key(&named(Named::F8)),
            Some(Binding::Key(Key::F(8)))
        );
    }

    #[test]
    fn a_typed_character_binds_lowercase() {
        assert_eq!(
            binding_from_key(&typed("a")),
            Some(Binding::Key(Key::Char('a')))
        );
        assert_eq!(
            binding_from_key(&typed("A")),
            Some(Binding::Key(Key::Char('a')))
        );
        assert_eq!(
            binding_from_key(&typed("7")),
            Some(Binding::Key(Key::Char('7')))
        );
    }

    /// Escape cancels the capture and can never be the binding; a key no
    /// backend can name is refused rather than stored.
    #[test]
    fn a_key_that_cannot_be_observed_binds_to_nothing() {
        assert_eq!(binding_from_key(&named(Named::Escape)), None);
        assert_eq!(binding_from_key(&typed("é")), None);
        assert_eq!(binding_from_key(&keyboard::Key::Unidentified), None);
    }

    #[test]
    fn only_the_three_secondary_mouse_buttons_bind() {
        assert_eq!(
            binding_from_mouse(mouse::Button::Back),
            Some(Binding::Mouse(MouseButton::Back))
        );
        assert_eq!(
            binding_from_mouse(mouse::Button::Middle),
            Some(Binding::Mouse(MouseButton::Middle))
        );
        assert_eq!(binding_from_mouse(mouse::Button::Left), None);
        assert_eq!(binding_from_mouse(mouse::Button::Right), None);
    }

    #[test]
    fn a_key_from_an_older_build_keeps_the_name_it_was_stored_under() {
        assert_eq!(key_label("Control"), "Ctrl");
        assert_eq!(key_label("MouseBack"), "Mouse back");
        assert_eq!(key_label("ArrowUp"), "ArrowUp");
    }

    #[test]
    fn a_bound_key_matches_by_binding() {
        assert!(key_matches(&named(Named::Control), "Control"));
        assert!(key_matches(&typed("A"), "a"));
        assert!(!key_matches(&named(Named::Shift), "Control"));
        assert!(!key_matches(&keyboard::Key::Unidentified, "Control"));
    }

    /// No backend can observe these, so the window is the only thing that ever
    /// sees them — and it has to keep working for whoever bound one.
    #[test]
    fn a_key_from_an_older_build_still_matches_in_the_window() {
        assert!(key_matches(&named(Named::ArrowUp), "ArrowUp"));
        assert!(key_matches(&typed("ç"), "ç"));
        assert!(!key_matches(&named(Named::ArrowDown), "ArrowUp"));
        assert_eq!(binding_from_key(&named(Named::ArrowUp)), None);
    }

    #[test]
    fn a_bound_mouse_button_matches_only_itself() {
        assert!(mouse_matches(mouse::Button::Back, "MouseBack"));
        assert!(!mouse_matches(mouse::Button::Forward, "MouseBack"));
        assert!(!mouse_matches(mouse::Button::Left, "MouseBack"));
        // A key binding is never a button, whatever the button is.
        assert!(!mouse_matches(mouse::Button::Middle, "Control"));
    }

    #[test]
    fn the_hotkey_sentence_names_the_backend() {
        let global = |backend, trigger: Option<&str>| {
            hotkey_sentence(
                &HotkeyStatus::Global {
                    backend,
                    trigger: trigger.map(str::to_owned),
                },
                TransmitMode::PushToTalk,
            )
        };

        assert_eq!(
            global(Backend::WindowsHook, None),
            "Global (low-level hook)"
        );
        assert_eq!(global(Backend::MacEventTap, None), "Global (event tap)");
        assert_eq!(global(Backend::X11Raw, None), "Global (X11)");
        assert_eq!(global(Backend::WaylandPortal, None), "Global via portal");
        assert_eq!(
            global(Backend::WaylandPortal, Some("CTRL")),
            "Global via portal: CTRL"
        );
    }

    #[test]
    fn the_hotkey_sentence_says_where_a_missing_listener_stands() {
        let ptt = |status| hotkey_sentence(status, TransmitMode::PushToTalk);

        assert_eq!(
            ptt(&HotkeyStatus::Off),
            "Global capture starts when you join voice"
        );
        assert_eq!(ptt(&HotkeyStatus::Starting), "Starting global capture…");
        assert_eq!(
            ptt(&HotkeyStatus::WindowOnly("no display".to_owned())),
            "Window only: no display"
        );
    }

    /// Voice activation needs no binding at all, whatever the listener was
    /// last doing.
    #[test]
    fn voice_activation_reads_as_off() {
        for status in [
            HotkeyStatus::Off,
            HotkeyStatus::Starting,
            HotkeyStatus::Global {
                backend: Backend::X11Raw,
                trigger: None,
            },
            HotkeyStatus::WindowOnly("no display".to_owned()),
        ] {
            assert_eq!(
                hotkey_sentence(&status, TransmitMode::VoiceActivation),
                "Off (voice activation)"
            );
        }
    }

    /// Two handlers can see the same message; the second must not get a
    /// listener of its own.
    #[test]
    fn a_hotkey_handoff_gives_its_answer_up_once() {
        let handoff = HotkeyHandoff::new(
            1,
            7,
            Binding::Key(Key::Control),
            Err(Unavailable::Unsupported("no display".to_owned())),
        );

        assert!(handoff.take().is_some());
        assert!(handoff.take().is_none());
    }

    fn voice_member(user_id: i64, username: &str) -> VoiceMember {
        VoiceMember {
            user_id,
            username: username.to_owned(),
            ssrc: user_id as u32,
            sharing: false,
            share_audio: false,
        }
    }

    /// A client that is sharing one screen and watching somebody else's.
    fn sharing_voice() -> VoiceUi {
        VoiceUi {
            room_id: GENERAL_ROOM.to_owned(),
            intent: true,
            sharing: [(9, true)].into_iter().collect(),
            share: ShareUi {
                intent: Some(ShareIntent {
                    request: CaptureRequest {
                        source: None,
                        fps: FrameRate::F30,
                        cursor: true,
                        audio: true,
                        max_size: Some((1280, 720)),
                    },
                    preset: share_preset(&Config::default()),
                }),
                starting: true,
                active: true,
                ..ShareUi::default()
            },
            watch: WatchUi {
                intent: Some(9),
                state: Some(9),
                seq: 12,
                ..WatchUi::default()
            },
            ..VoiceUi::default()
        }
    }

    /// A reconnect drops the media path and everything drawn from it. What was
    /// asked for is exactly what survives one, so the new session shares and
    /// watches again by itself.
    #[test]
    fn a_reconnect_keeps_the_share_intent_and_the_watch_intent() {
        let mut voice = sharing_voice();

        let _ = voice.close_session();

        assert!(voice.share.intent.is_some());
        assert_eq!(voice.watch.intent, Some(9));
        assert!(!voice.share.active);
        assert!(!voice.share.starting);
        assert_eq!(voice.watch.state, None);
        assert_eq!(voice.watch.seq, 0);
    }

    #[test]
    fn a_share_stopped_for_the_watched_user_drops_the_watch_intent() {
        let mut chat = ChatState::new();
        chat.member_id = 7;
        chat.voice.sharing.insert(9, true);
        chat.voice.watch.intent = Some(9);

        let _ = chat.apply(
            Event::ShareStopped {
                room_id: GENERAL_ROOM.to_owned(),
                user_id: 9,
            },
            false,
        );

        assert_eq!(chat.voice.watch.intent, None);
        assert!(!chat.voice.sharing.contains_key(&9));
    }

    #[test]
    fn leaving_voice_clears_both_intents() {
        let mut voice = sharing_voice();

        voice.give_up_intents();

        assert!(!voice.intent);
        assert!(!voice.joining);
        assert!(voice.share.intent.is_none());
        assert_eq!(voice.watch.intent, None);
    }

    #[test]
    fn watch_is_offered_only_in_voice_for_a_sharing_peer_that_is_not_me() {
        assert!(watch_rule(true, true, false));
        // Not in that room's voice channel, so there is no stream to ask for.
        assert!(!watch_rule(false, true, false));
        // In voice with them, but they are sharing nothing.
        assert!(!watch_rule(true, false, false));
        // One's own screen is not watched here.
        assert!(!watch_rule(true, true, true));
    }

    #[test]
    fn the_stage_sharer_list_excludes_me_and_keeps_usernames() {
        let voice = VoiceUi {
            members: [(7, "me"), (9, "bea"), (4, "ana")]
                .into_iter()
                .map(|(user_id, username)| (user_id, voice_member(user_id, username)))
                .collect(),
            sharing: [(4, false), (7, false), (9, true), (11, false)]
                .into_iter()
                .collect(),
            ..VoiceUi::default()
        };

        // 11 shares but is not in the roster, so there is no name to list it
        // under.
        assert_eq!(
            sharer_list(&voice, 7),
            vec![(4, "ana".to_owned()), (9, "bea".to_owned())]
        );
    }

    #[test]
    fn a_reconnect_re_requests_the_watch_when_the_sharer_is_still_there() {
        let sharing: BTreeMap<i64, bool> = [(9, true)].into_iter().collect();

        assert_eq!(
            watch_resume(Some(9), &sharing, true),
            WatchResume::Request(9)
        );
        // The roster is in and that screen is gone with it.
        assert_eq!(watch_resume(Some(4), &sharing, true), WatchResume::Clear);
        // The media path came up first: nothing can be judged until the roster
        // for this session lands.
        assert_eq!(
            watch_resume(Some(9), &BTreeMap::new(), false),
            WatchResume::Pending
        );
        assert_eq!(watch_resume(None, &sharing, true), WatchResume::Nothing);
    }

    #[test]
    fn a_watcher_count_rising_from_zero_forces_a_keyframe() {
        // Nobody watching: the encoder stops, and nothing has to be forced.
        assert_eq!(pause_decision(0, 0), (true, false));
        // The first watcher can only start at a keyframe.
        assert_eq!(pause_decision(0, 1), (false, true));
        // A second one joins a stream that is already running.
        assert_eq!(pause_decision(1, 2), (false, false));
        assert_eq!(pause_decision(2, 0), (true, false));
    }

    fn member(user_id: i64, username: &str) -> Member {
        Member {
            user_id,
            username: username.to_owned(),
        }
    }

    fn public_room(room_id: &str, name: &str) -> Room {
        Room {
            room_id: room_id.to_owned(),
            kind: RoomKind::Public as i32,
            name: name.to_owned(),
            member_ids: vec![7],
            created_by: 7,
        }
    }

    fn entry(room: Room, unread: i64, mentions: i64, last_message_id: i64) -> RoomEntry {
        RoomEntry {
            room: Some(room),
            unread,
            mentions,
            last_message_id,
        }
    }

    /// Everything but a message sitting on screen, in a focused window, in the
    /// room being read.
    #[test]
    fn a_foreign_message_is_unread_unless_it_is_being_read() {
        assert!(!unread_rule(true, true, true, true));

        assert!(unread_rule(true, false, true, true));
        assert!(unread_rule(true, true, false, true));
        assert!(unread_rule(true, true, true, false));
    }

    #[test]
    fn my_own_message_is_never_unread() {
        assert!(!unread_rule(false, false, false, false));
        assert!(!unread_rule(false, true, true, true));
    }

    /// A mention and a DM interrupt wherever the window is, unless they land
    /// where they can already be read.
    #[test]
    fn a_mention_notifies_unless_it_is_already_on_screen() {
        assert!(notification_rule(true, false, false));
        assert!(notification_rule(true, true, false));
        assert!(!notification_rule(true, true, true));
    }

    #[test]
    fn an_ordinary_message_notifies_only_an_unfocused_window() {
        assert!(notification_rule(false, false, false));
        assert!(!notification_rule(false, true, false));
        assert!(!notification_rule(false, true, true));
    }

    #[test]
    fn a_mention_query_is_the_unfinished_fragment() {
        assert_eq!(mention_query("hi @an").as_deref(), Some("an"));
        assert_eq!(mention_query("@a").as_deref(), Some("a"));
    }

    #[test]
    fn a_finished_or_empty_fragment_queries_nothing() {
        assert_eq!(mention_query("hi @an there"), None);
        assert_eq!(mention_query("@"), None);
        assert_eq!(mention_query("hi"), None);
        assert_eq!(mention_query(""), None);
    }

    #[test]
    fn picking_a_name_replaces_the_fragment() {
        assert_eq!(mention_replace("hi @an", "ana"), "hi @ana ");
        assert_eq!(mention_replace("@a", "ana"), "@ana ");
        assert_eq!(mention_replace("", "ana"), "@ana ");
    }

    /// The fragment starts after the whitespace, whatever its length in bytes.
    #[test]
    fn picking_a_name_keeps_what_was_written_before_it() {
        assert_eq!(mention_replace("olá @jo", "joão"), "olá @joão ");
    }

    #[test]
    fn a_public_room_is_titled_by_its_name() {
        let users = BTreeMap::from([(7, member(7, "ana"))]);

        let room = RoomUi::new(public_room("music", "Music"));
        assert_eq!(room.title(&users, 7), "#Music");

        // A room whose name never arrived is still worth naming.
        let bare = RoomUi::new(Room {
            name: String::new(),
            ..public_room("music", "")
        });
        assert_eq!(bare.title(&users, 7), "#music");
    }

    #[test]
    fn a_dm_is_titled_by_the_other_member() {
        let users = BTreeMap::from([(7, member(7, "ana")), (9, member(9, "bob"))]);
        let dm = |member_ids: Vec<i64>| {
            RoomUi::new(Room {
                room_id: "dm-7-9".to_owned(),
                kind: RoomKind::Dm as i32,
                name: String::new(),
                member_ids,
                created_by: 7,
            })
        };

        assert_eq!(dm(vec![7, 9]).title(&users, 7), "@bob");
        assert_eq!(dm(vec![7, 9]).title(&users, 9), "@ana");
        // A member this client has never heard of is not a raw id on screen.
        assert_eq!(dm(vec![7, 11]).title(&users, 7), "@unknown");
    }

    /// The counters and the newest id are the server's; the messages already
    /// loaded and everything the reader did are not.
    #[test]
    fn a_room_list_keeps_the_buffer_and_takes_the_counters() {
        let mut chat = ChatState::new();
        chat.member_id = 7;
        let general = chat
            .rooms
            .get_mut(GENERAL_ROOM)
            .expect("general is there from the start");
        general.messages.insert(3, ChatMessage::default());
        general.loaded = true;
        general.at_bottom = false;
        general.unread = 9;
        general.mentions = 4;

        chat.merge_room_list(vec![
            entry(public_room(GENERAL_ROOM, "general"), 2, 1, 12),
            entry(public_room("music", "Music"), 0, 0, 4),
        ]);

        let general = &chat.rooms[GENERAL_ROOM];
        assert!(general.loaded);
        assert!(!general.at_bottom);
        assert_eq!(general.messages.len(), 1);
        assert_eq!(general.unread, 2);
        assert_eq!(general.mentions, 1);
        assert_eq!(general.last_message_id, 12);
        assert!(chat.rooms.contains_key("music"));
    }

    /// A room left from another device is gone from the next list, and the room
    /// nobody can leave is what the window falls back to.
    #[test]
    fn a_room_list_without_the_room_in_view_falls_back_to_general() {
        let mut chat = ChatState::new();
        chat.member_id = 7;
        chat.current_room = "music".to_owned();

        chat.merge_room_list(vec![entry(public_room(GENERAL_ROOM, "general"), 0, 0, 0)]);

        assert_eq!(chat.current_room, GENERAL_ROOM);
        assert!(!chat.rooms.contains_key("music"));
    }

    fn dm_room(room_id: &str) -> Room {
        Room {
            room_id: room_id.to_owned(),
            kind: RoomKind::Dm as i32,
            name: String::new(),
            member_ids: vec![7, 9],
            created_by: 7,
        }
    }

    fn chat_holding(room: Room) -> ChatState {
        let mut chat = ChatState::new();
        chat.member_id = 7;
        chat.rooms.insert(room.room_id.clone(), RoomUi::new(room));
        chat
    }

    /// The `RoomState` that precedes the frame has already made a placeholder,
    /// so the name the dialog was given is the only thing that tells them apart.
    #[test]
    fn the_room_the_dialog_asked_for_is_the_one_it_named() {
        assert!(created_by_me(
            Some("Music"),
            &public_room("music", "Music"),
            7
        ));
    }

    #[test]
    fn no_other_room_answers_the_dialog() {
        let room = public_room("music", "Music");
        assert!(!created_by_me(None, &room, 7));
        assert!(!created_by_me(Some("Movies"), &room, 7));
        // Somebody else created it, whatever it is called.
        assert!(!created_by_me(Some("Music"), &room, 9));

        let mut named_dm = dm_room("dm-7-9");
        named_dm.name = "Music".to_owned();
        assert!(!created_by_me(Some("Music"), &named_dm, 7));

        let mut left = public_room("music", "Music");
        left.member_ids.clear();
        assert!(!created_by_me(Some("Music"), &left, 7));
    }

    /// The message being written belongs to the room it was written in.
    #[test]
    fn opening_another_room_starts_the_message_over() {
        let mut chat = chat_holding(public_room("music", "Music"));
        chat.composer.reply_to = Some(4);
        chat.composer.attachments.push(Attachment::default());
        chat.input = "half a sentence".to_owned();

        let _ = chat.select_room("music".to_owned(), false);

        assert_eq!(chat.current_room, "music");
        assert_eq!(chat.composer.reply_to, None);
        assert!(chat.composer.attachments.is_empty());
        assert_eq!(chat.input, "half a sentence");
    }

    /// An edit owns the input too: that text is the other room's message.
    #[test]
    fn opening_another_room_while_editing_drops_the_text_as_well() {
        let mut chat = chat_holding(public_room("music", "Music"));
        chat.composer.editing = Some(4);
        chat.input = "the other room's message".to_owned();
        chat.mention_query = Some("an".to_owned());

        let _ = chat.select_room("music".to_owned(), false);

        assert_eq!(chat.composer.editing, None);
        assert!(chat.input.is_empty());
        assert_eq!(chat.mention_query, None);
    }

    #[test]
    fn a_message_reopens_a_closed_conversation() {
        let mut chat = chat_holding(dm_room("dm-7-9"));
        chat.rooms.get_mut("dm-7-9").expect("just inserted").hidden = true;

        assert!(!chat.message_room("dm-7-9").hidden);
    }

    /// The next row's enter arrives before the previous row's exit.
    #[test]
    fn leaving_a_row_never_clears_the_one_the_pointer_moved_on_to() {
        let mut chat = ChatState::new();
        chat.hovered = Some(5);

        chat.unhover(4);
        assert_eq!(chat.hovered, Some(5));

        chat.unhover(5);
        assert_eq!(chat.hovered, None);
    }

    fn version(raw: &str) -> Version {
        raw.parse().expect("should parse")
    }

    fn ready(required: bool, manual: bool) -> Ready {
        Ready {
            manifest: Manifest {
                version: version("0.3.0"),
                notes: String::new(),
                published_at: "2026-09-10T18:00:00Z".to_owned(),
                min_version: version("0.3.0"),
                platforms: BTreeMap::new(),
            },
            asset: Asset {
                path: "vorcall".to_owned(),
                sha256: "0".repeat(64),
                size: 3,
            },
            file: PathBuf::from("/opt/vorcall/.vorcall-update-0.3.0"),
            manual,
            required,
        }
    }

    fn downloading(required: bool) -> UpdateState {
        UpdateState::Downloading {
            version: version("0.3.0"),
            received: 1,
            total: 2,
            required,
        }
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn a_fraction_without_a_total_is_zero() {
        assert!(close(progress_fraction(0, 0), 0.0));
        assert!(close(progress_fraction(512, 0), 0.0));
    }

    #[test]
    fn a_fraction_is_the_share_received() {
        assert!(close(progress_fraction(0, 400), 0.0));
        assert!(close(progress_fraction(100, 400), 0.25));
        assert!(close(progress_fraction(400, 400), 1.0));
    }

    #[test]
    fn a_fraction_never_passes_one() {
        assert!(close(progress_fraction(900, 400), 1.0));
    }

    #[test]
    fn a_check_waits_for_what_is_already_going_on() {
        assert!(!UpdateState::Idle.busy());
        assert!(!UpdateState::UpToDate { at: Instant::now() }.busy());
        assert!(
            !UpdateState::NoBuild {
                platform: "testos-testarch".to_owned()
            }
            .busy()
        );
        assert!(
            !UpdateState::Failed {
                message: "no".to_owned(),
                required: false,
                at: Instant::now(),
            }
            .busy()
        );

        assert!(UpdateState::Disabled("debug build".to_owned()).busy());
        assert!(UpdateState::Checking.busy());
        assert!(downloading(false).busy());
        assert!(
            UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false,
            }
            .busy()
        );
        assert!(UpdateState::Restarting.busy());
    }

    /// "Later" is only about the banner: the 6 h tick and the settings button
    /// keep working while an update sits dismissed.
    #[test]
    fn a_dismissed_update_does_not_hold_off_the_next_check() {
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: true,
            }
            .busy()
        );
    }

    #[test]
    fn only_a_required_update_takes_the_window() {
        assert!(downloading(true).shows_required(false));
        assert!(!downloading(false).shows_required(true));
        assert!(
            UpdateState::Ready {
                ready: ready(true, false),
                dismissed: false,
            }
            .shows_required(false)
        );
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false,
            }
            .shows_required(true)
        );
        assert!(
            UpdateState::Failed {
                message: "no".to_owned(),
                required: true,
                at: Instant::now(),
            }
            .shows_required(false)
        );
        assert!(!UpdateState::Idle.shows_required(true));
    }

    /// Neither carries a manifest, so what the app remembers is the only thing
    /// that keeps the required screen up while the swap — or the retry after a
    /// failed required update — runs.
    #[test]
    fn a_check_or_a_restart_takes_the_window_only_when_it_was_required() {
        assert!(UpdateState::Restarting.shows_required(true));
        assert!(!UpdateState::Restarting.shows_required(false));
        assert!(UpdateState::Checking.shows_required(true));
        assert!(!UpdateState::Checking.shows_required(false));
    }

    #[test]
    fn the_creature_runs_while_a_check_does() {
        assert!(UpdateState::Checking.shows_creature());
        assert!(downloading(false).shows_creature());
        assert!(!UpdateState::Idle.shows_creature());
        assert!(!UpdateState::Restarting.shows_creature());
        assert!(
            !UpdateState::Ready {
                ready: ready(false, false),
                dismissed: false,
            }
            .shows_creature()
        );
    }

    fn report(received: u64, total: u64) -> Progress {
        Progress::Downloading {
            version: version("0.3.0"),
            received,
            total,
            required: false,
        }
    }

    #[test]
    fn the_first_report_of_a_download_always_passes() {
        let mut throttle = ProgressThrottle::default();

        assert!(throttle.admit(&Progress::Checking));
        assert!(throttle.admit(&report(0, 10_000)));
    }

    /// One percent of 10 000 bytes is 100 of them.
    #[test]
    fn a_step_under_one_percent_is_dropped() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));

        assert!(!throttle.admit(&report(50, 10_000)));
        assert!(!throttle.admit(&report(99, 10_000)));
        assert!(throttle.admit(&report(100, 10_000)));
        assert!(!throttle.admit(&report(150, 10_000)));
    }

    /// One percent of ten gibibytes is far more than a mebibyte, and a
    /// mebibyte is already worth redrawing.
    #[test]
    fn a_huge_release_steps_by_a_mebibyte() {
        const TOTAL: u64 = 10 * 1024 * 1024 * 1024;
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, TOTAL)));

        assert!(!throttle.admit(&report(PROGRESS_STEP - 1, TOTAL)));
        assert!(throttle.admit(&report(PROGRESS_STEP, TOTAL)));
    }

    #[test]
    fn the_last_chunk_always_passes() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));
        assert!(throttle.admit(&report(9_999, 10_000)));

        assert!(throttle.admit(&report(10_000, 10_000)));
    }

    /// A second check starts over: its first report is not measured against
    /// what the last download had reached.
    #[test]
    fn checking_again_forgets_the_last_download() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));
        assert!(throttle.admit(&report(5_000, 10_000)));

        assert!(throttle.admit(&Progress::Checking));
        assert!(throttle.admit(&report(0, 10_000)));
    }

    /// A finished report leaves the settings page and the chat line saying the
    /// same thing.
    #[test]
    fn a_sent_report_counts_its_files() {
        let (state, notice) = report_outcome(Ok(3));

        assert!(matches!(state, ReportState::Sent(3)));
        assert_eq!(notice, "Report sent (3 files)");
    }

    #[test]
    fn a_refused_report_keeps_the_reason() {
        let (state, notice) = report_outcome(Err("Cannot reach the server".to_owned()));

        assert!(
            matches!(state, ReportState::Failed(reason) if reason == "Cannot reach the server")
        );
        assert_eq!(notice, "Report failed: Cannot reach the server");
    }
}
