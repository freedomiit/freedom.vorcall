//! The application state and the boot, dispatch, draw and subscribe entry points
//! `iced::daemon` calls.
//!
//! Networking stays in `vorcall-core`, reached through one subscription, so the
//! UI thread never waits on a socket. The worker threads own every device and
//! every blocking call. Drawing stays in [`crate::view`].

pub mod message;
pub mod state;
pub mod update;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use futures::Stream;
use futures::channel::mpsc;
use iced::widget::image::Handle;
use iced::widget::{Id, operation};
use iced::{Element, Size, Subscription, Task, Theme, keyboard, mouse, window};
use vorcall_core::connection::{self, Blob, Command};
use vorcall_core::update::{PublicKey, Ready, Version};
use vorcall_core::{Config, Endpoints, Event, Session, config, session};

use crate::brand;
use crate::theme::{self, ThemeTokens};
use crate::view;
use crate::workers::images::ImageKey;
use crate::workers::share::{ShareCommand, ShareHandle};
use crate::workers::voice::{AudioCommand, AudioHandle};
use crate::workers::{audio, images, notify, share, voice};

pub use message::Message;
use message::{ChatMsg, TickMsg, UiMsg, WindowMsg};
use state::chat::{ChatState, ImageState};
use state::server::ServerModel;
use state::settings::{AdminState, SettingsState};
use state::ui::UiState;
use state::update::UpdateState;
use state::voice::VoiceUi;

/// At most one notification and one chime per window, however many messages land
/// in it: a busy channel must not turn into a burst of popups.
pub const TOAST_INTERVAL: Duration = Duration::from_secs(5);
/// `PROTOCOL.md` § Client connection loop: at most one `MarkRead` per channel per
/// second, which is also how long a read waits before it is reported.
pub const MARK_READ_INTERVAL: Duration = Duration::from_secs(1);
/// How often the speaking indicators are refreshed while in voice.
pub const VOICE_TICK: Duration = Duration::from_millis(100);
/// A member counts as speaking while the local decoder got a packet this
/// recently.
pub const SPEAKING_WINDOW: Duration = Duration::from_millis(200);
/// Ticks between two reads of the media statistics: the status line moves far
/// slower than the speaking dots.
pub const STATS_EVERY: u32 = 10;
/// How often the toast stack is swept.
pub const TOAST_TICK: Duration = Duration::from_millis(500);
/// How often a connected client asks whether there is a new release.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// What the voice channel gets between the LeaveVoice and the swap: the frame has
/// to reach the server and the engine has to stop its tasks, and the process that
/// replaces this one waits for neither.
pub const RESTART_GRACE: Duration = Duration::from_millis(750);
/// The coarsest step a download reports, so a big release is not a message per
/// chunk. Below it, one percent is the step.
pub const PROGRESS_STEP: u64 = 1024 * 1024;
/// The pop-out stage: a 16:9 picture with its toolbar over it.
pub const STAGE_WINDOW: (f32, f32) = (960.0, 560.0);

/// The window this account signs in through, and everything behind it.
pub struct App {
    pub endpoints: Endpoints,
    pub session: Option<Session>,
    /// The connection subscription's identity, bumped by every sign-in and
    /// sign-out. The tokens themselves are not part of it: the loop rotates them
    /// while it runs, and that must not restart it.
    pub session_generation: u64,
    pub config: Config,
    pub tokens: ThemeTokens,
    /// The iced theme built from [`App::tokens`], kept so every frame does not
    /// rebuild its palette.
    theme: Theme,
    pub screen: Screen,
    pub ui: UiState,
    pub update: UpdateState,
    pub update_keys: Vec<PublicKey>,
    /// The release notes of the last download, kept for the settings page after
    /// the banner is gone.
    pub update_notes: Option<(Version, String)>,
    /// Whether this process has checked at all: the first connection is what
    /// starts it, and every reconnect after that must not.
    pub checked_on_connect: bool,
    /// What an earlier answer said about the update in flight. The download
    /// progress never says whether it is required, and `Restarting` carries no
    /// manifest of its own, so both read this instead.
    pub force_required: bool,
    pub pending_restart: Option<Ready>,
    /// The splash window while its entrance plays. It is closed before the main
    /// window opens, so the two never exist at once.
    pub splash: Option<window::Id>,
    pub main_window: Option<window::Id>,
    pub entrance: Option<brand::entrance::Entrance>,
    pub icon: Option<window::Icon>,
    pub loading: brand::loading::Loading,
    pub loading_elapsed: Duration,
    /// The chime's output device. Dropping the sink inside it silences the mixer,
    /// so it lives here for as long as the window does.
    pub audio: Option<audio::Audio>,
    pub audio_unavailable: bool,
    pub last_toast: Option<Instant>,
    pub workers: Workers,
}

/// The worker threads, started the first time something needs them and kept for
/// as long as the window lives.
#[derive(Default)]
pub struct Workers {
    pub audio: Option<AudioHandle>,
    pub share: Option<ShareHandle>,
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
    /// Boxed: the main state carries the voice session and dwarfs the two sign-in
    /// screens.
    Main(Box<MainState>),
}

impl Screen {
    /// A sign-in screen with the name that was last used already in it.
    pub fn login(username: String) -> Self {
        Self::Login {
            username,
            password: String::new(),
            error: None,
            busy: false,
        }
    }
}

/// Everything behind the shell: the server as this account sees it, the
/// conversations, the voice session and the two settings areas.
pub struct MainState {
    pub server: ServerModel,
    pub chat: ChatState,
    pub voice: VoiceUi,
    pub settings: SettingsState,
    pub admin: AdminState,
    pub status: Status,
    /// A transient server complaint shown beside the status.
    pub notice: Option<String>,
    /// The way back into the connection loop, handed over by `Event::Ready`.
    pub cmd: Option<mpsc::Sender<Command>>,
    pub member_id: i64,
    pub username: String,
    /// The handle the next transfer is asked for under; every answer carries it
    /// back.
    next_request: u64,
    pub pending_uploads: BTreeMap<u64, PendingUpload>,
    pub pending_fetches: BTreeMap<u64, ImageKey>,
    /// The user an `OpenDm` is out for, so its channel can be opened when it
    /// arrives.
    pub pending_dm: Option<i64>,
    /// The trimmed name a `CreateChannel` is out for, which is how its answer is
    /// told apart from every other channel the server describes.
    pub pending_channel: Option<String>,
    /// Every member as `vorcall_core::mentions` takes them, rebuilt only when the
    /// roster changes: a rendered row must not pay for a copy of it.
    pub user_pairs: Vec<(i64, String)>,
}

/// What an upload was asked for, so its answer can be dropped when the channel it
/// belongs to is no longer the one being written in.
pub struct PendingUpload {
    pub channel_id: i64,
    pub file_name: String,
}

#[derive(Debug, Clone)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting { in_secs: u64 },
    Unauthorized,
    Disconnected(String),
}

impl MainState {
    pub fn new(username: String) -> Self {
        Self {
            server: ServerModel::default(),
            chat: ChatState::new(),
            voice: VoiceUi::default(),
            settings: SettingsState::default(),
            admin: AdminState::default(),
            status: Status::Connecting,
            notice: None,
            cmd: None,
            member_id: 0,
            username,
            next_request: 0,
            pending_uploads: BTreeMap::new(),
            pending_fetches: BTreeMap::new(),
            pending_dm: None,
            pending_channel: None,
            user_pairs: Vec::new(),
        }
    }

    /// Sends one command, reporting whether the loop was there to take it.
    pub fn send_command(&mut self, command: Command) -> bool {
        let Some(cmd) = &mut self.cmd else {
            return false;
        };
        match cmd.try_send(command) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "cannot reach the connection loop");
                false
            }
        }
    }

    /// Sends one command, or says in the notice line why it could not.
    pub fn send_or_notice(&mut self, command: Command) {
        if !self.send_command(command) {
            self.notice = Some("Not connected".to_owned());
        }
    }

    /// The handle the next transfer is asked for under.
    pub fn next_request_id(&mut self) -> u64 {
        self.next_request = self.next_request.wrapping_add(1);
        self.next_request
    }

    /// Rebuilds the mention table from the member list.
    pub fn refresh_user_pairs(&mut self) {
        self.user_pairs = self.server.mention_pairs();
    }
}

impl App {
    pub fn new(
        endpoints: Endpoints,
        config: Config,
        session: Option<Session>,
        update_keys: Vec<PublicKey>,
        disabled: Option<String>,
    ) -> Self {
        let tokens = theme::file::resolve(&config.theme);
        let screen = match &session {
            Some(session) => Screen::Main(Box::new(MainState::new(session.username.clone()))),
            None => Screen::login(config.username.clone()),
        };
        let ui = UiState::new(&config);

        Self {
            endpoints,
            session,
            session_generation: 0,
            config,
            tokens,
            theme: tokens.iced_theme(),
            screen,
            ui,
            update: match disabled {
                Some(reason) => UpdateState::Disabled(reason),
                None => UpdateState::Idle,
            },
            update_keys,
            update_notes: None,
            checked_on_connect: false,
            force_required: false,
            pending_restart: None,
            splash: None,
            main_window: None,
            entrance: None,
            icon: None,
            loading: brand::loading::Loading::new(),
            loading_elapsed: Duration::ZERO,
            audio: None,
            audio_unavailable: false,
            last_toast: None,
            workers: Workers::default(),
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
        // `VORCALL_TEXT_REACTIONS` beats the stored preference, in both
        // directions, for a machine whose fonts decide the matter.
        match std::env::var("VORCALL_TEXT_REACTIONS").as_deref() {
            Ok("1") => app.config.text_reactions = true,
            Ok("0") => app.config.text_reactions = false,
            _ => {}
        }

        let task = match brand::entrance::choose(app.config.entrance) {
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

    pub fn update(&mut self, message: Message) -> Task<Message> {
        update::update(self, message)
    }

    pub fn view(&self, window: window::Id) -> Element<'_, Message> {
        view::view(self, window)
    }

    pub fn title(&self, window: window::Id) -> String {
        if self.splash == Some(window) {
            return "Vorcall".to_owned();
        }
        if let Screen::Main(main) = &self.screen
            && main.voice.watch.popped == Some(window)
        {
            return format!(
                "{} · Vorcall",
                state::rules::sharer_name(&main.voice, &main.server)
            );
        }
        let unread = match &self.screen {
            Screen::Main(main) => main.chat.total_unread(),
            _ => 0,
        };
        if unread > 0 {
            format!("({unread}) Vorcall")
        } else {
            "Vorcall".to_owned()
        }
    }

    /// The splash keeps the brand's own transparent theme; every other window is
    /// the token set.
    pub fn theme(&self, window: window::Id) -> Theme {
        if self.splash == Some(window) {
            brand::palette::splash_theme()
        } else {
            self.theme.clone()
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            iced::event::listen_with(ui_event),
            window::close_events().map(|id| Message::Window(WindowMsg::Closed(id))),
        ];
        // The splash is the only window while the entrance plays, so every frame
        // that arrives is one of its own.
        if self.entrance.is_some() {
            subscriptions
                .push(window::frames().map(|at| Message::Window(WindowMsg::SplashTick(at))));
        }
        if self.creature_visible() {
            subscriptions
                .push(window::frames().map(|at| Message::Window(WindowMsg::LoadingTick(at))));
        }

        if let Screen::Main(main) = &self.screen
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
            if main.voice.is_live() {
                subscriptions
                    .push(iced::time::every(VOICE_TICK).map(|_| Message::Tick(TickMsg::Voice)));
            }
            // Only while a read is waiting to be reported: the tick is what
            // debounces `MarkRead` to one per second.
            if main.chat.mark_read_due.is_some() {
                subscriptions.push(
                    iced::time::every(MARK_READ_INTERVAL).map(|_| Message::Tick(TickMsg::MarkRead)),
                );
            }
            // Nothing to ask while the socket is down, and nothing to ask at all
            // in a build that does not update itself.
            if matches!(main.status, Status::Connected)
                && !matches!(self.update, UpdateState::Disabled(_))
            {
                subscriptions.push(
                    iced::time::every(UPDATE_INTERVAL)
                        .map(|_| Message::Update(message::UpdateMsg::Tick)),
                );
            }
        }
        // Only while there is something to sweep.
        if !self.ui.toasts.is_empty() {
            subscriptions
                .push(iced::time::every(TOAST_TICK).map(|_| Message::Tick(TickMsg::Toasts)));
        }
        Subscription::batch(subscriptions)
    }

    /// The main state, when the window is past the sign-in screens.
    pub fn main(&self) -> Option<&MainState> {
        match &self.screen {
            Screen::Main(main) => Some(main),
            _ => None,
        }
    }

    pub fn main_mut(&mut self) -> Option<&mut MainState> {
        match &mut self.screen {
            Screen::Main(main) => Some(main),
            _ => None,
        }
    }

    /// Whatever is being typed into the open dialog.
    pub fn dialog_mut(&mut self) -> Option<&mut state::ui::Dialog> {
        self.ui.dialog.as_mut()
    }

    /// The name the window shows: the signed-in one, falling back to whoever
    /// signed in last.
    pub fn username(&self) -> &str {
        self.session
            .as_ref()
            .map_or(self.config.username.as_str(), |session| {
                session.username.as_str()
            })
    }

    /// Writes the preferences out. Every mutation site calls this: a setting that
    /// is not on disk is a setting that did not happen.
    pub fn save_config(&self) {
        if let Err(e) = config::save(&self.config) {
            tracing::warn!(error = %e, "cannot write the configuration");
        }
    }

    /// Rebuilds the theme after `config.theme` or a custom theme changed.
    pub fn reload_theme(&mut self) {
        self.tokens = theme::file::resolve(&self.config.theme);
        self.theme = self.tokens.iced_theme();
    }

    /// Raises one toast.
    pub fn toast(&mut self, kind: message::ToastKind, text: String) {
        self.ui.toast(kind, text, Instant::now());
    }

    /// The audio thread, started on the first use. The task is its event stream,
    /// which must only be wired once.
    pub fn ensure_audio(&mut self) -> Task<Message> {
        if self.workers.audio.is_some() {
            return Task::none();
        }
        let (handle, events) = voice::spawn_audio_thread();
        self.workers.audio = Some(handle);
        Task::run(events, |event| {
            Message::Voice(message::VoiceMsg::Audio(event))
        })
    }

    pub fn send_audio(&self, command: AudioCommand) {
        if let Some(handle) = &self.workers.audio {
            handle.send(command);
        }
    }

    /// The share thread, started on the first share and kept afterwards.
    pub fn ensure_share(&mut self) -> Task<Message> {
        if self.workers.share.is_some() {
            return Task::none();
        }
        let (handle, events) = share::spawn_share_thread();
        self.workers.share = Some(handle);
        Task::run(events, |event| {
            Message::Share(message::ShareMsg::Event(event))
        })
    }

    pub fn send_share(&self, command: ShareCommand) {
        if let Some(handle) = &self.workers.share {
            handle.send(command);
        }
    }

    /// Starts loading one image: from the cache when it is there, from the
    /// server otherwise. Doing nothing is the common case — the pixels are
    /// already held, or somebody else's request is in flight.
    pub fn ensure_image(&mut self, key: ImageKey) -> Task<Message> {
        let Some(main) = self.main_mut() else {
            return Task::none();
        };
        if !main.chat.ensure_image(key) {
            return Task::none();
        }

        if let Some(path) = images::cached_path(key)
            && path.is_file()
        {
            return decode_task(key, move || {
                std::fs::read(&path)
                    .map(Blob::from)
                    .map_err(|e| e.to_string())
            });
        }

        let request_id = main.next_request_id();
        main.pending_fetches.insert(request_id, key);
        let command = match key {
            ImageKey::Attachment(id) => Command::FetchAttachment { request_id, id },
            ImageKey::Image(id) => Command::FetchImage { request_id, id },
        };
        if !main.send_command(command) {
            main.pending_fetches.remove(&request_id);
            main.chat.images.insert(key, ImageState::Failed);
        }
        Task::none()
    }

    pub fn ensure_images(&mut self, keys: Vec<ImageKey>) -> Task<Message> {
        Task::batch(keys.into_iter().map(|key| self.ensure_image(key)))
    }

    /// One desktop notification and one chime for a message that landed away
    /// from the reader, at most one pair per [`TOAST_INTERVAL`].
    pub fn notify_once(&mut self, title: String, body: String) -> Task<Message> {
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
            Task::perform(notify::show(title, body), |()| Message::Noop)
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

    /// Whether the loading creature is on screen, which is what makes its
    /// per-frame clock worth running.
    pub fn creature_visible(&self) -> bool {
        matches!(self.screen, Screen::Main(_))
            && (self.update.shows_creature() || self.update.shows_required(self.force_required))
    }

    /// Everything one window message does. The splash's lifecycle is here because
    /// it is the window's, not an area's.
    pub fn window_message(&mut self, message: WindowMsg) -> Task<Message> {
        match message {
            WindowMsg::SplashTick(now) => {
                if let Some(entrance) = &mut self.entrance {
                    entrance.tick(now);
                    if entrance.finished() {
                        return self.close_splash();
                    }
                }
                Task::none()
            }
            WindowMsg::SplashSkip => self.close_splash(),
            WindowMsg::LoadingTick(now) => {
                self.loading_elapsed = self.loading.elapsed(now);
                Task::none()
            }
            WindowMsg::Closed(id) => {
                // The stage goes back into the main window; the watch itself
                // carries on.
                if let Screen::Main(main) = &mut self.screen {
                    let watch = &mut main.voice.watch;
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
        }
    }

    /// Signed in: the session is persisted, the screen becomes the shell, and the
    /// subscription restarts under the new identity.
    pub fn signed_in(&mut self, session: Session) -> Task<Message> {
        if let Err(e) = session::save(&session) {
            tracing::warn!(error = %e, "cannot persist the session");
        }
        self.config.username = session.username.clone();
        self.save_config();

        tracing::info!(user_id = session.user_id, "signed in");
        self.screen = Screen::Main(Box::new(MainState::new(session.username.clone())));
        self.session = Some(session);
        self.session_generation = self.session_generation.wrapping_add(1);
        self.ui.route = state::ui::Route::Main;
        self.ui.dialog = None;
        operation::focus(Id::new(view::COMPOSER_ID))
    }

    /// Drops the local session and goes back to the sign-in screen. The server is
    /// not told: the caller either already did, or holds tokens the server has
    /// refused.
    pub fn sign_out(&mut self, error: Option<String>) -> Task<Message> {
        // The media tasks outlive a dropped engine, so whatever is left of a
        // voice session is closed rather than dropped with the screen.
        let closing = self.close_voice();

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
        self.ui.dialog = None;
        self.ui.route = state::ui::Route::Main;
        Task::batch([closing, operation::focus(Id::new(view::USERNAME_ID))])
    }

    /// Ends the media path and everything drawn from it, for a window that is
    /// leaving the shell behind: a sign-out or a restart.
    ///
    /// The relay is told while there is a session to tell it about; without one
    /// this is the teardown a dropped connection gets, which also closes the
    /// engine, drops the hotkey listener and takes the stage windows down.
    pub fn close_voice(&mut self) -> Task<Message> {
        let joined = self
            .main()
            .is_some_and(|main| main.voice.intent || main.voice.is_live());
        let closing = if joined {
            update::voice::leave(self)
        } else {
            update::voice::close_session(self)
        };
        if let Some(main) = self.main_mut() {
            // Voice membership is per connection, in every channel.
            main.voice.clear_connection();
        }
        closing
    }

    fn open_main(&mut self) -> Task<Message> {
        let (id, task) = window::open(self.main_settings());
        self.main_window = Some(id);
        // The window's own `Focused` event is what flips this back: a window the
        // manager opens in the background must not pass for focused.
        self.ui.focused = false;
        task.discard()
    }

    /// Idempotent: a skip and the natural end in the same frame close once. The
    /// main window is opened first and the splash closed once it exists, because
    /// iced tears the compositor down while no window is left. `splash` itself is
    /// cleared only by [`WindowMsg::Closed`].
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

/// Turns one image's bytes into pixels on a blocking thread: an 8 MiB image is
/// milliseconds the UI thread does not have.
pub(crate) fn decode_task(
    key: ImageKey,
    read: impl FnOnce() -> Result<Blob, String> + Send + 'static,
) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || images::decode(&read()?, images::MAX_SIDE)),
        move |joined| {
            let decoded = joined.unwrap_or_else(|e| Err(e.to_string()));
            Message::Chat(ChatMsg::ImageDecoded(
                key,
                decoded.map(|(width, height, pixels)| Handle::from_rgba(width, height, pixels)),
            ))
        },
    )
}

/// Trims the image cache once per run, off the UI thread.
fn prune_image_cache() -> Task<Message> {
    Task::perform(
        async {
            if let Err(e) = tokio::task::spawn_blocking(|| images::prune(images::CACHE_LIMIT)).await
            {
                tracing::warn!(error = %e, "the image cache was not pruned");
            }
        },
        |()| Message::Noop,
    )
}

/// `listen_with` already drops redraw requests; everything this does not name is
/// dropped here, so no message reaches `update` for it.
fn ui_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Window(window::Event::Focused) => {
            Some(Message::Keys(message::KeyMsg::Focus(true)))
        }
        iced::Event::Window(window::Event::Unfocused) => {
            Some(Message::Keys(message::KeyMsg::Focus(false)))
        }
        iced::Event::Window(window::Event::Resized(size)) => {
            Some(Message::Ui(UiMsg::WindowResized(size)))
        }
        // A file dropped on the window goes the same way as one the dialog
        // picked.
        iced::Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::Keys(message::KeyMsg::FileDropped(path)))
        }
        // Push-to-talk needs both edges of every key, and the keybinding map is
        // what tells them apart, so nothing is dropped here.
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            Some(Message::Keys(message::KeyMsg::KeyDown(key, modifiers)))
        }
        iced::Event::Keyboard(keyboard::Event::KeyReleased { key, modifiers, .. }) => {
            Some(Message::Keys(message::KeyMsg::KeyUp(key, modifiers)))
        }
        // The context menu and the drag ghost follow the pointer.
        iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::Ui(UiMsg::CursorMoved(position)))
        }
        // Only the three buttons a binding may use: every left click would
        // otherwise wake `update` twice for nothing.
        iced::Event::Mouse(mouse::Event::ButtonPressed(button))
            if state::rules::binding_from_mouse(button).is_some() =>
        {
            Some(Message::Keys(message::KeyMsg::MouseDown(button)))
        }
        // The left release is the exception: a drag released away from a drop row
        // ends nowhere else.
        iced::Event::Mouse(mouse::Event::ButtonReleased(button))
            if button == mouse::Button::Left
                || state::rules::binding_from_mouse(button).is_some() =>
        {
            Some(Message::Keys(message::KeyMsg::MouseUp(button)))
        }
        _ => None,
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
