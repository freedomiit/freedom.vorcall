#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The Vorcall desktop client. All networking lives in `vorcall-core`, reached
//! through one iced subscription, so the UI thread never waits on a socket.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use futures::Stream;
use futures::channel::mpsc;
use iced::alignment::{Horizontal, Vertical};
use iced::widget::scrollable::RelativeOffset;
use iced::widget::{Id, button, column, container, operation, row, scrollable, text, text_input};
use iced::{Color, Element, Font, Length, Size, Subscription, Task, Theme, font};
use tracing_subscriber::EnvFilter;
use vorcall_core::connection::{self, Command, DisconnectReason, Event};
use vorcall_core::{ChatMessage, Config, Endpoints};

/// How many messages stay in memory; nothing is persisted.
const MESSAGE_LIMIT: usize = 500;
const NICKNAME_MAX: usize = 32;
/// The server rejects anything longer with a non-fatal INVALID_MESSAGE.
const MESSAGE_MAX_CHARS: usize = 2000;

const MESSAGES_ID: &str = "vorcall-messages";
const INPUT_ID: &str = "vorcall-input";
const NICKNAME_ID: &str = "vorcall-nickname";

const OK: Color = Color::from_rgb(0.36, 0.78, 0.46);
const WARN: Color = Color::from_rgb(0.95, 0.72, 0.28);
const DANGER: Color = Color::from_rgb(0.92, 0.37, 0.37);
const MUTED: Color = Color::from_rgb(0.55, 0.57, 0.62);

fn main() -> iced::Result {
    // reqwest is built with `rustls-no-provider`; without this it panics on the
    // first request. An Err only means someone already installed a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let endpoints = match vorcall_core::endpoints::resolve() {
        Ok(endpoints) => endpoints,
        Err(e) => {
            eprintln!("vorcall: {e:#}");
            std::process::exit(2);
        }
    };
    if endpoints.is_dev_key() {
        tracing::warn!("running with the development server key");
    }
    tracing::info!(?endpoints, "resolved endpoints");

    let config = match vorcall_core::config::load() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(error = %e, "ignoring the stored configuration");
            None
        }
    };

    iced::application(
        move || App::new(endpoints.clone(), config.clone()),
        App::update,
        App::view,
    )
    .title("Vorcall")
    .theme(|_: &App| Theme::Dark)
    .subscription(App::subscription)
    .window_size(Size::new(900.0, 620.0))
    .run()
}

struct App {
    endpoints: Endpoints,
    screen: Screen,
}

enum Screen {
    Nickname {
        input: String,
        error: Option<String>,
    },
    Chat {
        nickname: String,
        /// Keyed by id: history and live frames overlap, and id is the only order.
        messages: BTreeMap<i64, ChatMessage>,
        input: String,
        status: Status,
        cmd: Option<mpsc::Sender<Command>>,
        latest_history_error: Option<String>,
        /// A transient server complaint shown next to the status.
        notice: Option<String>,
    },
}

#[derive(Debug, Clone)]
enum Status {
    Connecting,
    Connected,
    Reconnecting { in_secs: u64 },
    Unauthorized,
    Disconnected(String),
}

#[derive(Debug, Clone)]
enum Message {
    NicknameChanged(String),
    Join,
    InputChanged(String),
    Send,
    Conn(Event),
    ChangeName,
}

impl App {
    fn new(endpoints: Endpoints, config: Option<Config>) -> Self {
        let nickname = config.map(|config| config.nickname).unwrap_or_default();
        let screen = if validate_nickname(&nickname).is_ok() {
            Screen::chat(nickname)
        } else {
            Screen::Nickname {
                input: nickname,
                error: None,
            }
        };

        Self { endpoints, screen }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NicknameChanged(value) => {
                if let Screen::Nickname { input, .. } = &mut self.screen {
                    *input = value;
                }
                Task::none()
            }
            Message::Join => self.join(),
            Message::InputChanged(value) => {
                if let Screen::Chat { input, .. } = &mut self.screen {
                    *input = value;
                }
                Task::none()
            }
            Message::Send => self.send(),
            Message::Conn(event) => self.on_event(event),
            Message::ChangeName => {
                // Dropping the Chat screen drops `cmd`, which ends `connection::run`.
                if let Screen::Chat { nickname, .. } = &self.screen {
                    let input = nickname.clone();
                    self.screen = Screen::Nickname { input, error: None };
                    return operation::focus(Id::new(NICKNAME_ID));
                }
                Task::none()
            }
        }
    }

    fn join(&mut self) -> Task<Message> {
        let Screen::Nickname { input, error } = &mut self.screen else {
            return Task::none();
        };

        let nickname = input.trim().to_owned();
        if let Err(message) = validate_nickname(&nickname) {
            *error = Some(message);
            return Task::none();
        }

        if let Err(e) = vorcall_core::config::save(&Config {
            nickname: nickname.clone(),
        }) {
            tracing::warn!(error = %e, "cannot persist the nickname");
        }

        tracing::info!(%nickname, "joining");
        self.screen = Screen::chat(nickname);
        operation::focus(Id::new(INPUT_ID))
    }

    fn send(&mut self) -> Task<Message> {
        let Screen::Chat {
            input,
            status,
            cmd,
            notice,
            ..
        } = &mut self.screen
        else {
            return Task::none();
        };

        if !matches!(status, Status::Connected) {
            return Task::none();
        }
        let text = input.trim().to_owned();
        if text.is_empty() {
            return Task::none();
        }
        if text.chars().count() > MESSAGE_MAX_CHARS {
            *notice = Some(format!(
                "Message is too long (max {MESSAGE_MAX_CHARS} characters)"
            ));
            return operation::focus(Id::new(INPUT_ID));
        }
        let Some(sender) = cmd.as_mut() else {
            return Task::none();
        };

        match sender.try_send(Command::Send(text)) {
            Ok(()) => {
                input.clear();
                *notice = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot hand the message to the connection");
                *notice = Some("Message not sent".to_owned());
            }
        }

        operation::focus(Id::new(INPUT_ID))
    }

    fn on_event(&mut self, event: Event) -> Task<Message> {
        // A rejected nickname replaces the whole screen, so it is handled before
        // borrowing the chat fields.
        if let Event::Disconnected {
            reason: DisconnectReason::InvalidNickname,
            ..
        } = &event
        {
            let Screen::Chat {
                nickname, notice, ..
            } = &self.screen
            else {
                return Task::none();
            };
            let error = notice
                .clone()
                .unwrap_or_else(|| "The server rejected that nickname.".to_owned());
            self.screen = Screen::Nickname {
                input: nickname.clone(),
                error: Some(error),
            };
            return operation::focus(Id::new(NICKNAME_ID));
        }

        let Screen::Chat {
            messages,
            status,
            cmd,
            latest_history_error,
            notice,
            ..
        } = &mut self.screen
        else {
            return Task::none();
        };

        match event {
            Event::Ready(sender) => {
                *cmd = Some(sender);
                Task::none()
            }
            Event::Connecting => {
                *status = Status::Connecting;
                Task::none()
            }
            Event::Connected { .. } => {
                *status = Status::Connected;
                *notice = None;
                Task::none()
            }
            Event::Disconnected { reason, retry_in } => {
                *status = match reason {
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
            Event::History(history) => {
                *latest_history_error = None;
                for message in history {
                    messages.insert(message.id, message);
                }
                trim(messages);
                snap_to_end()
            }
            Event::Message(message) => {
                messages.insert(message.id, message);
                trim(messages);
                *notice = None;
                snap_to_end()
            }
            Event::HistoryFailed(detail) => {
                *latest_history_error = Some(detail);
                Task::none()
            }
            Event::ServerError { detail, .. } => {
                *notice = Some(detail);
                Task::none()
            }
            Event::SendDropped => {
                *notice = Some("Not connected".to_owned());
                Task::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.screen {
            Screen::Nickname { .. } => Subscription::none(),
            Screen::Chat { nickname, .. } => Subscription::run_with(
                ConnectionInput {
                    endpoints: self.endpoints.clone(),
                    nickname: nickname.clone(),
                },
                connect,
            )
            .map(Message::Conn),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        match &self.screen {
            Screen::Nickname { input, error } => nickname_view(input, error.as_deref()),
            Screen::Chat {
                messages,
                input,
                status,
                latest_history_error,
                notice,
                ..
            } => chat_view(
                messages,
                input,
                status,
                notice.as_deref(),
                latest_history_error.as_deref(),
            ),
        }
    }
}

impl Screen {
    fn chat(nickname: String) -> Self {
        Self::Chat {
            nickname,
            messages: BTreeMap::new(),
            input: String::new(),
            status: Status::Connecting,
            cmd: None,
            latest_history_error: None,
            notice: None,
        }
    }
}

/// The subscription's identity is the nickname alone: a new nickname must
/// restart the connection, while the endpoints never change while we run.
struct ConnectionInput {
    endpoints: Endpoints,
    nickname: String,
}

impl Hash for ConnectionInput {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.nickname.hash(state);
    }
}

// `Subscription::run_with` takes a plain fn pointer, so everything this stream
// needs is cloned out of `input` rather than captured.
fn connect(input: &ConnectionInput) -> impl Stream<Item = Event> + use<> {
    let endpoints = input.endpoints.clone();
    let nickname = input.nickname.clone();

    iced::stream::channel(64, async move |mut output| {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        if futures::SinkExt::send(&mut output, Event::Ready(cmd_tx))
            .await
            .is_err()
        {
            return;
        }
        connection::run(endpoints, nickname, cmd_rx, output).await;
    })
}

fn nickname_view<'a>(input: &str, error: Option<&'a str>) -> Element<'a, Message> {
    let mut content = column![
        text("Vorcall").size(34).font(bold()),
        text("Pick a name to join the room.").color(MUTED),
        text_input("Nickname", input)
            .id(Id::new(NICKNAME_ID))
            .on_input(Message::NicknameChanged)
            .on_submit(Message::Join)
            .padding(12)
            .width(320),
        button(text("Join")).on_press(Message::Join).padding(12),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Some(error) = error {
        content = content.push(text(error).color(DANGER));
    }

    container(content).center(Length::Fill).into()
}

fn chat_view<'a>(
    messages: &'a BTreeMap<i64, ChatMessage>,
    input: &str,
    status: &Status,
    notice: Option<&str>,
    history_error: Option<&str>,
) -> Element<'a, Message> {
    let (label, colour) = status_line(status, notice, history_error);

    let top = row![
        text("Vorcall").size(20).font(bold()),
        text(label)
            .color(colour)
            .width(Length::Fill)
            .align_x(Horizontal::Right),
        button(text("Change name")).on_press(Message::ChangeName),
    ]
    .spacing(12)
    .padding(12)
    .align_y(Vertical::Center);

    let list = column(messages.values().map(message_row))
        .spacing(6)
        .padding(12)
        .width(Length::Fill);

    let history = scrollable(list)
        .id(Id::new(MESSAGES_ID))
        .width(Length::Fill)
        .height(Length::Fill);

    let connected = matches!(status, Status::Connected);

    let mut field = text_input("Message…", input)
        .id(Id::new(INPUT_ID))
        .on_submit(Message::Send)
        .padding(12)
        .width(Length::Fill);
    let mut send = button(text("Send")).padding(12);
    if connected {
        field = field.on_input(Message::InputChanged);
        send = send.on_press(Message::Send);
    }

    let composer = row![field, send].spacing(8).padding(12);

    column![top, history, composer].into()
}

fn message_row(message: &ChatMessage) -> Element<'_, Message> {
    row![
        text(format_time(message.sent_at_unix_ms)).color(MUTED),
        text(message.author.as_str()).font(bold()),
        text(message.text.as_str()).width(Length::Fill),
    ]
    .spacing(8)
    .into()
}

fn status_line(
    status: &Status,
    notice: Option<&str>,
    history_error: Option<&str>,
) -> (String, Color) {
    let (label, colour) = match status {
        Status::Connecting => ("Connecting…".to_owned(), MUTED),
        Status::Connected => ("Connected".to_owned(), OK),
        Status::Reconnecting { in_secs } => (format!("Reconnecting in {in_secs}s"), WARN),
        Status::Unauthorized => (
            "Unauthorized: rebuild the client with the current key".to_owned(),
            DANGER,
        ),
        Status::Disconnected(reason) => (format!("Disconnected — {reason}"), DANGER),
    };

    if let Some(notice) = notice {
        return (format!("{label} · {notice}"), WARN);
    }
    if history_error.is_some() {
        return (format!("{label} · history unavailable"), WARN);
    }
    (label, colour)
}

fn snap_to_end() -> Task<Message> {
    operation::snap_to(Id::new(MESSAGES_ID), RelativeOffset::END)
}

fn trim(messages: &mut BTreeMap<i64, ChatMessage>) {
    while messages.len() > MESSAGE_LIMIT {
        if messages.pop_first().is_none() {
            break;
        }
    }
}

fn validate_nickname(nickname: &str) -> Result<(), String> {
    let length = nickname.chars().count();
    if length == 0 {
        return Err("Enter a nickname.".to_owned());
    }
    if length > NICKNAME_MAX {
        return Err(format!("At most {NICKNAME_MAX} characters."));
    }
    if nickname.chars().any(char::is_control) {
        return Err("No control characters.".to_owned());
    }
    Ok(())
}

fn format_time(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_owned())
}

fn bold() -> Font {
    Font {
        weight: font::Weight::Bold,
        ..Font::DEFAULT
    }
}
