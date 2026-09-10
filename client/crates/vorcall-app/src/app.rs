//! The application state and everything that changes it.
//!
//! Networking stays in `vorcall-core`, reached through one iced subscription,
//! so the UI thread never waits on a socket. Drawing stays in [`crate::view`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::Stream;
use futures::channel::mpsc;
use iced::widget::scrollable::{RelativeOffset, Viewport};
use iced::widget::{Id, operation};
use iced::{Element, Size, Subscription, Task, Theme, keyboard, window};
use vorcall_core::connection::{self, Command, DisconnectReason, Event, GENERAL_ROOM, MediaKey};
use vorcall_core::update::{self, Checker, Outcome, Progress, PublicKey, Ready, Version};
use vorcall_core::{
    ApiFailure, ChatMessage, Config, Endpoints, ErrorCode, Member, Session, VoiceMember, auth,
    config, session,
};
use vorcall_voice::{MediaConfig, MediaEngine, Stats};

use crate::view;
use crate::voice::{self, AudioCommand, AudioEvent, AudioHandle, AudioSettings};
use crate::{audio, brand, notify, update_ui};

/// How many messages stay in memory; nothing is persisted.
pub const MESSAGE_LIMIT: usize = 2000;
/// The server rejects anything longer with a non-fatal INVALID_MESSAGE.
pub const MESSAGE_MAX_CHARS: usize = 2000;
const USERNAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 128;
/// At most one notification and one chime per window, however many messages
/// land in it: a busy room must not turn into a burst of popups.
const TOAST_INTERVAL: Duration = Duration::from_secs(5);
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

const UNEXPECTED: &str = "Unexpected server answer";

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
    unread: u32,
    last_toast: Option<Instant>,
    audio: Option<audio::Audio>,
    audio_unavailable: bool,
    /// The splash window while its entrance plays. It is closed before the main
    /// window opens, so the two never exist at once.
    splash: Option<window::Id>,
    main_window: Option<window::Id>,
    entrance: Option<brand::entrance::Entrance>,
    icon: Option<window::Icon>,
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
    /// Keyed by id: history, older pages and live frames overlap, and id is the
    /// only order.
    pub messages: BTreeMap<i64, ChatMessage>,
    pub has_older: bool,
    pub loading_older: bool,
    /// Every registered user, online or not.
    pub users: BTreeMap<i64, Member>,
    pub online: BTreeSet<i64>,
    pub member_id: i64,
    pub input: String,
    pub status: Status,
    cmd: Option<mpsc::Sender<Command>>,
    /// A transient server complaint shown next to the status.
    pub notice: Option<String>,
    pub history_error: Option<String>,
    pub at_bottom: bool,
    pub pending_new: u32,
    pub dialog: Option<Dialog>,
    pub voice: VoiceUi,
    pub page: Page,
}

pub enum Dialog {
    ChangePassword {
        current: String,
        new: String,
        confirm: String,
        error: Option<String>,
        busy: bool,
    },
}

/// Everything the voice room adds to the chat screen. `intent` is what makes a
/// reconnect rejoin; nothing below it survives one.
#[derive(Default)]
pub struct VoiceUi {
    pub intent: bool,
    /// `JoinVoice` is out and `VoiceReady` has not come back yet.
    pub joining: bool,
    pub members: BTreeMap<i64, VoiceMember>,
    pub speaking_server: BTreeSet<i64>,
    /// Whoever the local decoder heard within [`SPEAKING_WINDOW`], which needs
    /// no help from the server.
    pub speaking_local: BTreeSet<i64>,
    pub muted: bool,
    pub deafened: bool,
    pub ptt_held: bool,
    pub stats: Stats,
    pub session: Option<VoiceSession>,
    /// The ssrc of the last `VoiceReady`, waiting for its engine.
    pending_ssrc: u32,
    by_ssrc: BTreeMap<u32, i64>,
    /// What un-deafening puts back.
    muted_before_deafen: bool,
    ticks: u32,
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
    /// One per frame while the splash is up.
    SplashTick(Instant),
    SplashSkip,
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
            Screen::Chat(Box::new(ChatState::new()))
        } else {
            Screen::login(config.username.clone())
        };

        Self {
            endpoints,
            config,
            session,
            session_generation: 0,
            screen,
            focused: true,
            unread: 0,
            last_toast: None,
            audio: None,
            audio_unavailable: false,
            splash: None,
            main_window: None,
            entrance: None,
            icon: None,
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
        }
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
        (app, task)
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
        if self.unread > 0 {
            format!("({}) Vorcall", self.unread)
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
            Message::Focus(_) | Message::KeyDown(_) | Message::KeyUp(_)
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
                    chat.input = value;
                }
                Task::none()
            }
            Message::Send => self.send(),
            Message::LoadOlder => self.load_older(),
            Message::Scrolled(viewport) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    // Under `Anchor::End` the offset is measured from the end of
                    // the list, so zero is the bottom.
                    chat.at_bottom = viewport.absolute_offset().y <= 1.0;
                    if chat.at_bottom {
                        chat.pending_new = 0;
                    }
                }
                Task::none()
            }
            Message::JumpToLatest => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.pending_new = 0;
                chat.at_bottom = true;
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
            Message::JoinVoice => self.join_voice(),
            Message::LeaveVoice => self.leave_voice(),
            Message::ToggleMute => self.toggle_mute(),
            Message::ToggleDeafen => self.toggle_deafen(),
            Message::KeyDown(key) => self.key_down(key),
            Message::KeyUp(key) => {
                if key_matches(&key, &self.config.ptt_key) {
                    self.set_ptt(false);
                }
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
                    self.unread = 0;
                } else {
                    // A release that lands on another window never reaches us,
                    // so push-to-talk would stay held.
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
        self.screen = Screen::Chat(Box::new(ChatState::new()));
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
        self.unread = 0;
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

        let text = chat.input.trim().to_owned();
        if text.is_empty() {
            return Task::none();
        }
        if text.chars().count() > MESSAGE_MAX_CHARS {
            chat.notice = Some(format!(
                "Message is too long (max {MESSAGE_MAX_CHARS} characters)"
            ));
            return operation::focus(Id::new(view::INPUT_ID));
        }

        let sent = match chat.cmd.as_mut() {
            Some(sender) => sender
                .try_send(Command::Send {
                    room_id: GENERAL_ROOM.to_owned(),
                    text,
                })
                .inspect_err(
                    |e| tracing::warn!(error = %e, "cannot hand the message to the connection"),
                )
                .is_ok(),
            None => return Task::none(),
        };

        if sent {
            chat.input.clear();
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
        if !chat.has_older || chat.loading_older {
            return Task::none();
        }
        let Some(&before) = chat.messages.keys().next() else {
            return Task::none();
        };

        let asked = match chat.cmd.as_mut() {
            Some(sender) => sender
                .try_send(Command::LoadOlder {
                    room_id: GENERAL_ROOM.to_owned(),
                    before,
                })
                .inspect_err(|e| tracing::warn!(error = %e, "cannot ask for an older page"))
                .is_ok(),
            None => false,
        };

        if asked {
            chat.loading_older = true;
        } else {
            chat.notice = Some("Not connected".to_owned());
        }
        Task::none()
    }

    fn join_voice(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        chat.voice.intent = true;
        chat.voice.joining = true;
        if !chat.send_command(Command::JoinVoice {
            room_id: GENERAL_ROOM.to_owned(),
        }) {
            chat.voice.joining = false;
            chat.notice = Some("Not connected".to_owned());
        }
        Task::none()
    }

    fn leave_voice(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        chat.voice.intent = false;
        chat.voice.joining = false;
        if !chat.send_command(Command::LeaveVoice {
            room_id: GENERAL_ROOM.to_owned(),
        }) {
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
        if ignoring_room(&room_id) {
            return Task::none();
        }
        // An empty host means the relay lives wherever the WebSocket goes.
        let host = non_empty(&host).unwrap_or_else(|| self.endpoints.host());

        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
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
        audio.send(AudioCommand::Open {
            settings,
            sender: engine.sender(),
            playout: engine.playout(),
        });
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
        Task::batch([closing, Task::run(events, Message::Audio)])
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
            if !escape {
                // The old key is possibly held right now, and its release will
                // no longer match anything.
                self.set_ptt(false);
                self.config.ptt_key = key_name(&key);
                self.save_config();
            }
            if let Some(settings) = self.settings() {
                settings.capturing_ptt = false;
            }
            return Task::none();
        }
        if escape {
            return self.close_overlay();
        }
        if key_matches(&key, &self.config.ptt_key) {
            self.set_ptt(true);
        }
        Task::none()
    }

    /// Escape, and the dialog's own Cancel: the dialog first, then the settings
    /// page.
    fn close_overlay(&mut self) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };
        if chat.dialog.take().is_some() {
            return operation::focus(Id::new(view::INPUT_ID));
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
        let stats = (voice.ticks % STATS_EVERY == 0).then(|| session.engine.stats());

        voice.ticks = voice.ticks.wrapping_add(1);
        voice.speaking_local = speaking
            .into_iter()
            .filter_map(|ssrc| voice.by_ssrc.get(&ssrc).copied())
            .collect();
        if let Some(stats) = stats {
            voice.stats = stats;
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
            // The first connection is the earliest moment there is a token to
            // check with; every later one is the timer's business.
            Event::Connected { .. } if !self.checked_on_connect => {
                self.checked_on_connect = true;
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                let applied = chat.apply(event);
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
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                chat.apply(other)
            }
        }
    }

    fn on_message(&mut self, message: ChatMessage) -> Task<Message> {
        let Screen::Chat(chat) = &mut self.screen else {
            return Task::none();
        };

        let foreign = message.author_id != chat.member_id;
        let author = message.author.clone();
        let preview = notify::preview(&message.text);

        chat.messages.insert(message.id, message);
        trim(&mut chat.messages);
        chat.notice = None;

        if !foreign {
            return Task::none();
        }
        // At the bottom the anchor already keeps the newest message in view.
        if !chat.at_bottom {
            chat.pending_new = chat.pending_new.saturating_add(1);
        }

        if self.focused {
            return Task::none();
        }
        self.unread = self.unread.saturating_add(1);
        self.toast(author, preview)
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
        Self {
            messages: BTreeMap::new(),
            has_older: false,
            loading_older: false,
            users: BTreeMap::new(),
            online: BTreeSet::new(),
            member_id: 0,
            input: String::new(),
            status: Status::Connecting,
            cmd: None,
            notice: None,
            history_error: None,
            at_bottom: true,
            pending_new: 0,
            dialog: None,
            voice: VoiceUi::default(),
            page: Page::Chat,
        }
    }

    /// The speaking dot: what the server saw, what the local decoder hears, and
    /// this member while push-to-talk is going out.
    pub fn speaking(&self, user_id: i64) -> bool {
        self.voice.speaking_server.contains(&user_id)
            || self.voice.speaking_local.contains(&user_id)
            || (user_id == self.member_id && self.voice.transmitting())
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

    /// Every connection event except the two that end the session and the live
    /// message, which both need more than the chat state.
    fn apply(&mut self, event: Event) -> Task<Message> {
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
                    self.send_command(Command::JoinVoice {
                        room_id: GENERAL_ROOM.to_owned(),
                    });
                }
                Task::none()
            }
            Event::Disconnected { reason, retry_in } => {
                self.online.clear();
                self.loading_older = false;
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
                self.voice.by_ssrc.clear();
                self.voice.speaking_server.clear();
                self.voice.close_session()
            }
            Event::History { messages, has_more } => {
                self.history_error = None;
                self.merge(messages);
                trim(&mut self.messages);
                // A reconnect starts the page state over.
                self.loading_older = false;
                self.has_older = has_more;
                self.at_bottom = true;
                self.pending_new = 0;
                snap_to_bottom()
            }
            Event::HistoryFailed(detail) => {
                self.history_error = Some(detail);
                Task::none()
            }
            // No scroll operation: the bottom anchor keeps the viewport where
            // it is while the list grows upwards.
            Event::OlderPage { messages, has_more } => {
                self.merge(messages);
                self.loading_older = false;
                self.has_older = has_more && self.messages.len() < MESSAGE_LIMIT;
                Task::none()
            }
            Event::OlderFailed(detail) => {
                self.loading_older = false;
                self.notice = Some(detail);
                Task::none()
            }
            Event::Users(list) => {
                self.users = list
                    .into_iter()
                    .map(|member| (member.user_id, member))
                    .collect();
                Task::none()
            }
            Event::UsersFailed(detail) => {
                self.notice = Some(detail);
                Task::none()
            }
            Event::RoomState { room_id, members } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.online = members.iter().map(|member| member.user_id).collect();
                for member in members {
                    self.users.entry(member.user_id).or_insert(member);
                }
                Task::none()
            }
            Event::MemberJoined { room_id, member } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.online.insert(member.user_id);
                self.users.insert(member.user_id, member);
                Task::none()
            }
            Event::MemberLeft { room_id, user_id } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.online.remove(&user_id);
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
                self.notice = Some(detail);
                Task::none()
            }
            Event::SendDropped => {
                self.notice = Some("Not connected".to_owned());
                Task::none()
            }
            Event::VoiceState { room_id, members } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.voice.reset_members(members);
                Task::none()
            }
            Event::VoiceMemberJoined { room_id, member } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.voice.insert_member(member);
                Task::none()
            }
            Event::VoiceMemberLeft { room_id, user_id } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                self.voice.remove_member(user_id);
                Task::none()
            }
            Event::Speaking {
                room_id,
                user_id,
                speaking,
            } => {
                if ignoring_room(&room_id) {
                    return Task::none();
                }
                if speaking {
                    self.voice.speaking_server.insert(user_id);
                } else {
                    self.voice.speaking_server.remove(&user_id);
                }
                Task::none()
            }
            // All three need more than the chat state, so [`App::on_event`]
            // takes them before this is reached.
            Event::Message(_) | Event::SessionUpdated(_) | Event::VoiceReady { .. } => Task::none(),
        }
    }

    fn merge(&mut self, messages: Vec<ChatMessage>) {
        for message in messages {
            self.messages.insert(message.id, message);
        }
    }
}

impl VoiceUi {
    /// Drops the media path and leaves `intent` alone: a reconnect rejoins with
    /// it. Closing the engine is what stops its tasks; dropping it does not.
    fn close_session(&mut self) -> Task<Message> {
        self.ptt_held = false;
        self.speaking_local.clear();
        self.stats = Stats::default();

        let Some(session) = self.session.take() else {
            return Task::none();
        };
        tracing::debug!(
            ssrc = session.ssrc,
            input = ?session.audio_input,
            output = ?session.audio_output,
            "closing the voice session"
        );
        session.audio.send(AudioCommand::Close);
        Task::perform(session.engine.close(), |()| Message::Noop)
    }

    fn apply_flags(&self) {
        let Some(session) = &self.session else {
            return;
        };
        session.audio.send(AudioCommand::SetMuted(self.muted));
        session.audio.send(AudioCommand::SetDeafened(self.deafened));
    }

    fn reset_members(&mut self, members: Vec<VoiceMember>) {
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
    }

    fn insert_member(&mut self, member: VoiceMember) {
        // A Speaking(true) can outlive the member it was about; the rejoin
        // starts silent rather than lit up for good.
        self.speaking_server.remove(&member.user_id);
        self.speaking_local.remove(&member.user_id);
        self.by_ssrc.insert(member.ssrc, member.user_id);
        self.members.insert(member.user_id, member);
    }

    fn remove_member(&mut self, user_id: i64) {
        if let Some(member) = self.members.remove(&user_id) {
            self.by_ssrc.remove(&member.ssrc);
        }
        self.speaking_server.remove(&user_id);
        self.speaking_local.remove(&user_id);
    }

    /// True while this member's own microphone is going out.
    fn transmitting(&self) -> bool {
        self.ptt_held && !self.muted && !self.deafened && self.session.is_some()
    }
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
        // Push-to-talk needs both edges of every key, and Escape is told apart
        // in `update` so no key press is dropped here.
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => {
            Some(Message::KeyDown(key))
        }
        iced::Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
            Some(Message::KeyUp(key))
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

/// Presence for any other room is not an error: `PROTOCOL.md` lets the server
/// add rooms without a version bump, and `general` is the only one drawn today.
fn ignoring_room(room_id: &str) -> bool {
    if room_id == GENERAL_ROOM {
        return false;
    }
    tracing::debug!(%room_id, "ignoring presence for another room");
    true
}

/// How a key is written in the configuration: the name of its `Named` variant,
/// or the character it produced.
fn key_name(key: &keyboard::Key) -> String {
    match key {
        keyboard::Key::Named(named) => format!("{named:?}"),
        keyboard::Key::Character(character) => character.to_string(),
        keyboard::Key::Unidentified => config::DEFAULT_PTT_KEY.to_owned(),
    }
}

/// Location is deliberately ignored: either Control key holds the same talk.
fn key_matches(key: &keyboard::Key, ptt: &str) -> bool {
    match key {
        keyboard::Key::Named(named) => format!("{named:?}") == ptt,
        keyboard::Key::Character(character) => character.eq_ignore_ascii_case(ptt),
        keyboard::Key::Unidentified => false,
    }
}

/// What the settings page and the sidebar hint call the push-to-talk key. Every
/// other name is already what a person would write.
pub fn key_label(ptt: &str) -> String {
    match ptt {
        "Control" => "Ctrl".to_owned(),
        other => other.to_owned(),
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

    use vorcall_core::update::{Asset, Manifest};

    use super::*;

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
}
