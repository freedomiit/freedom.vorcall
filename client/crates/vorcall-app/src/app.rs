//! The application state and everything that changes it.
//!
//! Networking stays in `vorcall-core`, reached through one iced subscription,
//! so the UI thread never waits on a socket. Drawing stays in [`crate::view`].

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use futures::Stream;
use futures::channel::mpsc;
use iced::widget::scrollable::{RelativeOffset, Viewport};
use iced::widget::{Id, operation};
use iced::{Element, Subscription, Task, keyboard, window};
use vorcall_core::connection::{self, Command, DisconnectReason, Event, GENERAL_ROOM};
use vorcall_core::{
    ApiFailure, ChatMessage, Config, Endpoints, Member, Session, auth, config, session,
};

use crate::view;
use crate::{audio, notify};

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
    Chat(ChatState),
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

#[derive(Debug, Clone)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting { in_secs: u64 },
    Unauthorized,
    Disconnected(String),
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
    Focus(bool),
    Conn(Event),
    Noop,
}

impl App {
    pub fn new(endpoints: Endpoints, config: Config, session: Option<Session>) -> Self {
        let screen = if session.is_some() {
            Screen::Chat(ChatState::new())
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
        }
    }

    pub fn title(&self) -> String {
        if self.unread > 0 {
            format!("({}) Vorcall", self.unread)
        } else {
            "Vorcall".to_owned()
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
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
            Message::CloseDialog => {
                let Screen::Chat(chat) = &mut self.screen else {
                    return Task::none();
                };
                if chat.dialog.take().is_none() {
                    return Task::none();
                }
                operation::focus(Id::new(view::INPUT_ID))
            }
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
            Message::Focus(focused) => {
                self.focused = focused;
                if focused {
                    self.unread = 0;
                }
                Task::none()
            }
            Message::Conn(event) => self.on_event(event),
            Message::Noop => Task::none(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let ui = iced::event::listen_with(ui_event);

        if !matches!(self.screen, Screen::Chat(_)) {
            return ui;
        }
        let Some(session) = self.session.clone() else {
            return ui;
        };

        Subscription::batch([
            ui,
            Subscription::run_with(
                ConnectionInput {
                    endpoints: self.endpoints.clone(),
                    session,
                    generation: self.session_generation,
                },
                connect,
            )
            .map(Message::Conn),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
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
            Screen::Chat(chat) => view::chat(chat, &self.config, self.username()),
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
        self.screen = Screen::Chat(ChatState::new());
        operation::focus(Id::new(view::INPUT_ID))
    }

    fn logout(&mut self) -> Task<Message> {
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

        self.sign_out(None);
        task
    }

    /// Drops the local session and goes back to the sign-in screen. The server
    /// is not told: the caller either already did, or holds tokens the server
    /// has refused. Dropping [`Screen::Chat`] drops `cmd`, which ends
    /// `connection::run`.
    fn sign_out(&mut self, error: Option<String>) {
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
                self.sign_out(Some(detail));
                operation::focus(Id::new(view::USERNAME_ID))
            }
            Event::Disconnected {
                reason: DisconnectReason::SessionReplaced,
                ..
            } => {
                self.sign_out(Some(
                    "This account connected from another device.".to_owned(),
                ));
                operation::focus(Id::new(view::USERNAME_ID))
            }
            Event::Message(message) => self.on_message(message),
            // Already persisted by the loop; the subscription must not restart,
            // so its identity does not include the tokens.
            Event::SessionUpdated(session) => {
                self.session = Some(session);
                Task::none()
            }
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
                Task::none()
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
            Event::ServerError { detail, .. } => {
                self.notice = Some(detail);
                Task::none()
            }
            Event::SendDropped => {
                self.notice = Some("Not connected".to_owned());
                Task::none()
            }
            // Both need more than the chat state, so [`App::on_event`] takes
            // them before this is reached.
            Event::Message(_) | Event::SessionUpdated(_) => Task::none(),
        }
    }

    fn merge(&mut self, messages: Vec<ChatMessage>) {
        for message in messages {
            self.messages.insert(message.id, message);
        }
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
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            ..
        }) => Some(Message::CloseDialog),
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
