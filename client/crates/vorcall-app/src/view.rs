//! Drawing. Every function here is a pure read of the state in [`crate::app`].

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Id, button, column, container, opaque, pick_list, row, scrollable, stack, text, text_input,
    toggler,
};
use iced::{Color, Element, Font, Length, Theme, font};
use vorcall_core::{ChatMessage, Config, Member, VoiceMember};
use vorcall_voice::{Link, Stats};

use crate::app::{
    ChatState, Dialog, MESSAGE_LIMIT, Message, Page, SettingsState, Status, VoiceUi, key_label,
};
use crate::brand::mark::mark;
use crate::brand::palette::{DANGER, MUTED, SUCCESS, WARNING};
use crate::update_ui::{self, UpdateView};

pub const MESSAGES_ID: &str = "vorcall-messages";
pub const INPUT_ID: &str = "vorcall-input";
pub const USERNAME_ID: &str = "vorcall-username";
pub const CURRENT_PASSWORD_ID: &str = "vorcall-current-password";
/// The pick list entry that means "whatever the system picks".
pub const SYSTEM_DEFAULT: &str = "System default";

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
        row![mark(40.0), text("Vorcall").size(34).font(bold())]
            .spacing(10)
            .align_y(Vertical::Center),
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
        row![mark(40.0), text("Vorcall").size(34).font(bold())]
            .spacing(10)
            .align_y(Vertical::Center),
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

pub fn chat<'a>(
    chat: &'a ChatState,
    config: &Config,
    username: &'a str,
    update: UpdateView<'a>,
) -> Element<'a, Message> {
    let mut content = column![header(chat, username)];
    // The banner belongs to the window, not to a page: it stays put while the
    // settings are open.
    if let Some(banner) = update_ui::banner(update) {
        content = content.push(banner);
    }
    let content = match &chat.page {
        Page::Chat => content
            .push(row![messages(chat), sidebar(chat, config)].height(Length::Fill))
            .push(composer(chat)),
        Page::Settings(state) => content.push(settings(state, config, update)),
    };

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
        chat.voice.session.is_some().then_some(&chat.voice),
    );

    row![
        row![mark(22.0), text("Vorcall").size(20).font(bold())]
            .spacing(8)
            .align_y(Vertical::Center),
        text(label)
            .color(colour)
            .width(Length::Fill)
            .align_x(Horizontal::Right),
        text(username).color(MUTED),
        button(text("Settings")).on_press(Message::OpenSettings),
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

    let mut panel = column![
        text(format!("Members · {online}/{total}")).font(bold()),
        scrollable(roster).height(Length::Fill),
        text("Voice · general").font(bold()),
        voice_controls(chat),
    ]
    .spacing(10)
    .padding(12);

    let mut speakers: Vec<&VoiceMember> = chat.voice.members.values().collect();
    speakers.sort_by_cached_key(|member| member.username.to_lowercase());
    for member in speakers {
        panel = panel.push(voice_member_row(member, chat));
    }

    if chat.voice.session.is_some() {
        let mut hint = format!("Hold {} to talk", key_label(&config.ptt_key));
        // Deafened already implies muted; saying both would only take room.
        if chat.voice.deafened {
            hint.push_str(" · deafened");
        } else if chat.voice.muted {
            hint.push_str(" · muted");
        }
        panel = panel.push(text(hint).color(MUTED));
    }
    if chat.voice.joining {
        panel = panel.push(text("Joining…").color(MUTED));
    }

    panel = panel.push(
        toggler(config.notifications)
            .label("Notifications")
            .on_toggle(Message::SetNotifications),
    );
    panel = panel.push(
        toggler(config.sound)
            .label("Sound")
            .on_toggle(Message::SetSound),
    );

    container(panel)
        .width(SIDEBAR_WIDTH)
        .height(Length::Fill)
        .into()
}

fn voice_controls(chat: &ChatState) -> Element<'_, Message> {
    if chat.voice.session.is_none() && !chat.voice.joining {
        // Joining goes through the connection, so it needs one.
        let connected = matches!(chat.status, Status::Connected);
        return button(text("Join voice"))
            .on_press_maybe(connected.then_some(Message::JoinVoice))
            .into();
    }

    // Two rows: the three buttons side by side do not fit the sidebar.
    column![
        button(text("Leave voice")).on_press(Message::LeaveVoice),
        row![
            button(text(if chat.voice.muted { "Unmute" } else { "Mute" }))
                .on_press(Message::ToggleMute),
            button(text(if chat.voice.deafened {
                "Undeafen"
            } else {
                "Deafen"
            }))
            .on_press(Message::ToggleDeafen),
        ]
        .spacing(6),
    ]
    .spacing(6)
    .into()
}

fn voice_member_row<'a>(member: &'a VoiceMember, chat: &ChatState) -> Element<'a, Message> {
    let speaking = chat.speaking(member.user_id);

    let mut row = row![
        text("●").color(if speaking { SUCCESS } else { MUTED }),
        text(member.username.as_str()).font(if speaking { bold() } else { Font::DEFAULT }),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    if member.user_id == chat.member_id {
        row = row.push(text("(you)").color(MUTED));
    }
    row.into()
}

fn settings<'a>(
    state: &SettingsState,
    config: &Config,
    update: UpdateView<'a>,
) -> Element<'a, Message> {
    let ptt: Element<'a, Message> = if state.capturing_ptt {
        text("Press a key… (Esc cancels)").color(WARNING).into()
    } else {
        button(text("Change"))
            .on_press(Message::StartPttCapture)
            .into()
    };

    column![
        text("Settings").size(20).font(bold()),
        text("Input device"),
        pick_list(
            device_options(&state.inputs),
            Some(device_selection(config.input_device.as_deref())),
            Message::SetInputDevice,
        ),
        text("Output device"),
        pick_list(
            device_options(&state.outputs),
            Some(device_selection(config.output_device.as_deref())),
            Message::SetOutputDevice,
        ),
        row![
            text(format!("Push-to-talk key: {}", key_label(&config.ptt_key))),
            ptt,
        ]
        .spacing(12)
        .align_y(Vertical::Center),
        button(text("Back")).on_press(Message::CloseSettings),
        update_ui::section(update),
    ]
    .spacing(12)
    .padding(16)
    .width(Length::Fill)
    .into()
}

fn device_options(names: &[String]) -> Vec<String> {
    std::iter::once(SYSTEM_DEFAULT.to_owned())
        .chain(names.iter().cloned())
        .collect()
}

fn device_selection(chosen: Option<&str>) -> String {
    chosen.unwrap_or(SYSTEM_DEFAULT).to_owned()
}

fn member_row<'a>(member: &'a Member, chat: &ChatState) -> Element<'a, Message> {
    let online = chat.online.contains(&member.user_id);

    let mut row = row![
        text("●").color(if online { SUCCESS } else { MUTED }),
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
    voice: Option<&VoiceUi>,
) -> (String, Color) {
    let (mut label, mut colour) = match status {
        Status::Connecting => ("Connecting…".to_owned(), MUTED),
        Status::Connected => ("Connected".to_owned(), SUCCESS),
        Status::Reconnecting { in_secs } => (format!("Reconnecting in {in_secs}s"), WARNING),
        Status::Unauthorized => (
            "Unauthorized: rebuild the client with the current key".to_owned(),
            DANGER,
        ),
        Status::Disconnected(reason) => (format!("Disconnected — {reason}"), DANGER),
    };

    if let Some(voice) = voice {
        match voice.stats.link {
            Link::Connecting => label.push_str(" · voice: connecting"),
            Link::Connected => {
                let rtt = voice
                    .stats
                    .rtt_last_ms
                    .map_or_else(|| "–".to_owned(), |ms| ms.round().to_string());
                label.push_str(&format!(
                    " · voice {rtt} ms · loss {:.1}%",
                    loss(&voice.stats)
                ));
            }
            // The socket is up and nothing comes back: a person can act on that.
            Link::NoMedia => {
                label.push_str(" · voice: no media");
                colour = WARNING;
            }
        }
    }

    if let Some(notice) = notice {
        return (format!("{label} · {notice}"), WARNING);
    }
    if history_error.is_some() {
        return (format!("{label} · history unavailable"), WARNING);
    }
    (label, colour)
}

/// What the jitter buffers never got, over every peer.
fn loss(stats: &Stats) -> f64 {
    let (received, lost) = stats
        .peers
        .iter()
        .fold((0u64, 0u64), |(received, lost), (_, peer)| {
            (received + peer.received, lost + peer.lost)
        });

    let total = received + lost;
    if total == 0 {
        return 0.0;
    }
    lost as f64 * 100.0 / total as f64
}

fn format_time(unix_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_owned())
}

pub(crate) fn bold() -> Font {
    Font {
        weight: font::Weight::Bold,
        ..Font::DEFAULT
    }
}
