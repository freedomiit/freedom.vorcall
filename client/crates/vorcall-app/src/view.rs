//! Drawing. Every function here is a pure read of the state in [`crate::app`].

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Id, button, column, container, opaque, row, scrollable, stack, text, text_input, toggler,
};
use iced::{Color, Element, Font, Length, Theme, font};
use vorcall_core::{ChatMessage, Config, Member};

use crate::app::{ChatState, Dialog, MESSAGE_LIMIT, Message, Status};

pub const MESSAGES_ID: &str = "vorcall-messages";
pub const INPUT_ID: &str = "vorcall-input";
pub const USERNAME_ID: &str = "vorcall-username";
pub const CURRENT_PASSWORD_ID: &str = "vorcall-current-password";

const OK: Color = Color::from_rgb(0.36, 0.78, 0.46);
const WARN: Color = Color::from_rgb(0.95, 0.72, 0.28);
const DANGER: Color = Color::from_rgb(0.92, 0.37, 0.37);
const MUTED: Color = Color::from_rgb(0.55, 0.57, 0.62);

const SIDEBAR_WIDTH: f32 = 200.0;
const FIELD_WIDTH: f32 = 320.0;

pub fn login<'a>(
    username: &str,
    password: &str,
    error: Option<&'a str>,
    busy: bool,
) -> Element<'a, Message> {
    let mut sign_in = button(text("Sign in")).padding(12);
    if !busy {
        sign_in = sign_in.on_press(Message::LoginSubmit);
    }

    let mut content = column![
        text("Vorcall").size(34).font(bold()),
        text("Sign in").size(20).color(MUTED),
        text_input("Username", username)
            .id(Id::new(USERNAME_ID))
            .on_input(Message::UsernameChanged)
            .on_submit(Message::LoginSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        text_input("Password", password)
            .secure(true)
            .on_input(Message::PasswordChanged)
            .on_submit(Message::LoginSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        sign_in,
        button(text("Create account").color(MUTED))
            .on_press(Message::ShowRegister)
            .style(button::text),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Some(error) = error {
        content = content.push(text(error).color(DANGER));
    }

    container(content).center(Length::Fill).into()
}

pub fn register<'a>(
    username: &str,
    password: &str,
    confirm: &str,
    invite: &str,
    error: Option<&'a str>,
    busy: bool,
) -> Element<'a, Message> {
    let mut create = button(text("Create account")).padding(12);
    if !busy {
        create = create.on_press(Message::RegisterSubmit);
    }

    let mut content = column![
        text("Vorcall").size(34).font(bold()),
        text("Create account").size(20).color(MUTED),
        text_input("Username", username)
            .id(Id::new(USERNAME_ID))
            .on_input(Message::UsernameChanged)
            .on_submit(Message::RegisterSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        text_input("Password", password)
            .secure(true)
            .on_input(Message::PasswordChanged)
            .on_submit(Message::RegisterSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        text_input("Confirm password", confirm)
            .secure(true)
            .on_input(Message::ConfirmChanged)
            .on_submit(Message::RegisterSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        text_input("XXXXX-XXXXX-XXXXX-XXXXX", invite)
            .on_input(Message::InviteChanged)
            .on_submit(Message::RegisterSubmit)
            .padding(12)
            .width(FIELD_WIDTH),
        create,
        button(text("Back to sign in").color(MUTED))
            .on_press(Message::ShowLogin)
            .style(button::text),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Some(error) = error {
        content = content.push(text(error).color(DANGER));
    }

    container(content).center(Length::Fill).into()
}

pub fn chat<'a>(chat: &'a ChatState, config: &Config, username: &'a str) -> Element<'a, Message> {
    let content = column![
        header(chat, username),
        row![messages(chat), sidebar(chat, config)].height(Length::Fill),
        composer(chat),
    ];

    match &chat.dialog {
        Some(dialog) => stack![content, change_password(dialog)].into(),
        None => content.into(),
    }
}

fn header<'a>(chat: &ChatState, username: &'a str) -> Element<'a, Message> {
    let (label, colour) = status_line(
        &chat.status,
        chat.notice.as_deref(),
        chat.history_error.as_deref(),
    );

    row![
        text("Vorcall").size(20).font(bold()),
        text(label)
            .color(colour)
            .width(Length::Fill)
            .align_x(Horizontal::Right),
        text(username).color(MUTED),
        button(text("Change password")).on_press(Message::OpenChangePassword),
        button(text("Log out")).on_press(Message::Logout),
    ]
    .spacing(12)
    .padding(12)
    .align_y(Vertical::Center)
    .into()
}

fn messages(chat: &ChatState) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> = Vec::with_capacity(chat.messages.len() + 3);

    if chat.has_older && !chat.loading_older && chat.messages.len() < MESSAGE_LIMIT {
        rows.push(
            button(text("Load older messages"))
                .on_press(Message::LoadOlder)
                .into(),
        );
    }
    if chat.loading_older {
        rows.push(text("Loading…").color(MUTED).into());
    }
    if chat.messages.len() >= MESSAGE_LIMIT {
        rows.push(
            text(format!("Showing the last {MESSAGE_LIMIT} messages"))
                .color(MUTED)
                .into(),
        );
    }
    rows.extend(chat.messages.values().map(message_row));

    let list = column(rows).spacing(6).padding(12).width(Length::Fill);
    let scroller = scrollable(list)
        .id(Id::new(MESSAGES_ID))
        .anchor_bottom()
        .on_scroll(Message::Scrolled)
        .width(Length::Fill)
        .height(Length::Fill);

    if chat.pending_new == 0 {
        return scroller.into();
    }

    stack![
        scroller,
        container(
            button(text(format!("{} new messages ↓", chat.pending_new)))
                .on_press(Message::JumpToLatest)
        )
        .align_bottom(Length::Fill)
        .center_x(Length::Fill)
        .padding(8),
    ]
    .into()
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

fn sidebar<'a>(chat: &'a ChatState, config: &Config) -> Element<'a, Message> {
    let total = chat.users.len();
    let online = chat.online.len();

    let mut members: Vec<&Member> = chat.users.values().collect();
    members.sort_by_cached_key(|member| {
        (
            !chat.online.contains(&member.user_id),
            member.username.to_lowercase(),
        )
    });

    let roster = column(members.into_iter().map(|member| member_row(member, chat)))
        .spacing(6)
        .width(Length::Fill);

    container(
        column![
            text(format!("Members · {online}/{total}")).font(bold()),
            scrollable(roster).height(Length::Fill),
            toggler(config.notifications)
                .label("Notifications")
                .on_toggle(Message::SetNotifications),
            toggler(config.sound)
                .label("Sound")
                .on_toggle(Message::SetSound),
        ]
        .spacing(10)
        .padding(12),
    )
    .width(SIDEBAR_WIDTH)
    .height(Length::Fill)
    .into()
}

fn member_row<'a>(member: &'a Member, chat: &ChatState) -> Element<'a, Message> {
    let online = chat.online.contains(&member.user_id);

    let mut row = row![
        text("●").color(if online { OK } else { MUTED }),
        text(member.username.as_str()).font(if online { bold() } else { Font::DEFAULT }),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    if member.user_id == chat.member_id {
        row = row.push(text("(you)").color(MUTED));
    }
    row.into()
}

fn composer(chat: &ChatState) -> Element<'_, Message> {
    let connected = matches!(chat.status, Status::Connected);

    let mut field = text_input("Message…", &chat.input)
        .id(Id::new(INPUT_ID))
        .on_submit(Message::Send)
        .padding(12)
        .width(Length::Fill);
    let mut send = button(text("Send")).padding(12);
    if connected {
        field = field.on_input(Message::InputChanged);
        send = send.on_press(Message::Send);
    }

    row![field, send].spacing(8).padding(12).into()
}

fn change_password(dialog: &Dialog) -> Element<'_, Message> {
    let Dialog::ChangePassword {
        current,
        new,
        confirm,
        error,
        busy,
    } = dialog;

    let mut save = button(text("Save")).padding(10);
    if !busy {
        save = save.on_press(Message::ChangePasswordSubmit);
    }

    let mut form = column![
        text("Change password").size(20).font(bold()),
        text_input("Current password", current)
            .id(Id::new(CURRENT_PASSWORD_ID))
            .secure(true)
            .on_input(Message::DialogCurrentChanged)
            .on_submit(Message::ChangePasswordSubmit)
            .padding(10)
            .width(FIELD_WIDTH),
        text_input("New password", new)
            .secure(true)
            .on_input(Message::DialogNewChanged)
            .on_submit(Message::ChangePasswordSubmit)
            .padding(10)
            .width(FIELD_WIDTH),
        text_input("Confirm new password", confirm)
            .secure(true)
            .on_input(Message::DialogConfirmChanged)
            .on_submit(Message::ChangePasswordSubmit)
            .padding(10)
            .width(FIELD_WIDTH),
    ]
    .spacing(12);

    if let Some(error) = error {
        form = form.push(text(error.as_str()).color(DANGER));
    }
    form = form.push(
        row![
            save,
            button(text("Cancel"))
                .on_press(Message::CloseDialog)
                .padding(10),
        ]
        .spacing(8),
    );

    // `opaque` is what keeps the chat under the backdrop from taking the
    // clicks that miss the dialog.
    opaque(
        container(
            container(form)
                .padding(24)
                .style(container::bordered_box)
                .max_width(FIELD_WIDTH + 64.0),
        )
        .center(Length::Fill)
        .style(backdrop),
    )
}

/// Dark enough that the chat behind the dialog stops competing for attention,
/// and opaque enough that its text does not read through.
fn backdrop(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.8).into()),
        ..container::Style::default()
    }
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
