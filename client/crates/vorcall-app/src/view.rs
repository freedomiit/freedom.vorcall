//! Drawing. Every function here is a pure read of the state in [`crate::app`].

mod composer;
mod message;
mod rooms;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Id, Space, button, column, container, image, mouse_area, opaque, pick_list, progress_bar,
    radio, row, scrollable, slider, stack, text, text_input, toggler,
};
use iced::{Color, ContentFit, Element, Font, Length, Theme, font};
use vorcall_core::config::{TransmitMode, VAD_MAX_DB, VAD_MIN_DB};
use vorcall_core::connection::GENERAL_ROOM;
use vorcall_core::{Config, Member, VoiceMember};
use vorcall_voice::{Link, Stats};

use crate::app::{
    ChatState, Dialog, HotkeyStatus, ImageState, MESSAGE_LIMIT, Message, Page, RoomUi,
    SettingsState, Status, VoiceUi, hotkey_sentence, key_label,
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

const SIDEBAR_WIDTH: f32 = 220.0;
const FIELD_WIDTH: f32 = 320.0;
/// The threshold slider and the level meter share a width, so the gate and the
/// level it is measured against line up.
const METER_WIDTH: f32 = 220.0;

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
    let mut content = column![header(chat, config.transmit_mode, username)];
    // The banner belongs to the window, not to a page: it stays put while the
    // settings are open.
    if let Some(banner) = update_ui::banner(update) {
        content = content.push(banner);
    }
    let content = match &chat.page {
        // The composer belongs to the message column: neither pane beside it
        // has anything to write in.
        Page::Chat => content.push(
            row![
                rooms::pane(chat),
                column![messages(chat), composer::view(chat)]
                    .width(Length::Fill)
                    .height(Length::Fill),
                sidebar(chat, config),
            ]
            .height(Length::Fill),
        ),
        Page::Settings(state) => content.push(settings(state, config, &chat.voice, update)),
    };

    match &chat.dialog {
        Some(dialog) => stack![content, overlay(dialog, chat)].into(),
        None => content.into(),
    }
}

fn header<'a>(chat: &ChatState, mode: TransmitMode, username: &'a str) -> Element<'a, Message> {
    let (label, colour) = status_line(
        &chat.status,
        chat.notice.as_deref(),
        chat.current()
            .and_then(|room| room.history_error.as_deref()),
        chat.voice.session.is_some().then_some(&chat.voice),
        mode,
    );

    let mut left = row![mark(22.0), text("Vorcall").size(20).font(bold())]
        .spacing(8)
        .align_y(Vertical::Center);

    if let Some(room) = chat.current() {
        left = left.push(
            text(room.title(&chat.users, chat.member_id))
                .size(18)
                .font(bold()),
        );
        // A conversation has exactly two members, which its title already names.
        if !room.is_dm() {
            left = left.push(text(format!("{} members", room.room.member_ids.len())).color(MUTED));
        }
        left = left.push(room_action(room));
    }

    row![
        left,
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

/// What a room can be done with from its own header. `general` is the one room
/// nobody may leave; the server refuses it too.
fn room_action<'a>(room: &RoomUi) -> Element<'a, Message> {
    let room_id = room.room.room_id.clone();
    if room.is_dm() {
        return button(text("Close"))
            .on_press(Message::CloseDm(room_id))
            .into();
    }
    if room_id == GENERAL_ROOM {
        return Space::new().into();
    }
    button(text("Leave"))
        .on_press(Message::LeaveRoom(room_id))
        .into()
}

fn messages(chat: &ChatState) -> Element<'_, Message> {
    let Some(room) = chat.current() else {
        return Space::new().into();
    };
    let mut rows: Vec<Element<'_, Message>> = Vec::with_capacity(room.messages.len() + 3);

    if room.has_older && !room.loading_older && room.messages.len() < MESSAGE_LIMIT {
        rows.push(
            button(text("Load older messages"))
                .on_press(Message::LoadOlder)
                .into(),
        );
    }
    if room.loading_older {
        rows.push(text("Loading…").color(MUTED).into());
    }
    if room.messages.len() >= MESSAGE_LIMIT {
        rows.push(
            text(format!("Showing the last {MESSAGE_LIMIT} messages"))
                .color(MUTED)
                .into(),
        );
    }
    rows.extend(
        room.messages
            .values()
            .map(|entry| message::view(chat, entry)),
    );

    let list = column(rows).spacing(6).padding(12).width(Length::Fill);
    let scroller = scrollable(list)
        .id(Id::new(MESSAGES_ID))
        .anchor_bottom()
        .on_scroll(Message::Scrolled)
        .width(Length::Fill)
        .height(Length::Fill);

    if room.pending_new == 0 {
        return scroller.into();
    }

    stack![
        scroller,
        container(
            button(text(format!("{} new messages ↓", room.pending_new)))
                .on_press(Message::JumpToLatest)
        )
        .align_bottom(Length::Fill)
        .center_x(Length::Fill)
        .padding(8),
    ]
    .into()
}

fn sidebar<'a>(chat: &'a ChatState, config: &Config) -> Element<'a, Message> {
    // The sidebar is about the room in view, so its roster is that room's
    // membership rather than every account the server knows.
    let mut members: Vec<&Member> = chat
        .current()
        .map(|room| {
            room.room
                .member_ids
                .iter()
                .filter_map(|user_id| chat.users.get(user_id))
                .collect()
        })
        .unwrap_or_default();
    members.sort_by_cached_key(|member| {
        (!chat.online(member.user_id), member.username.to_lowercase())
    });

    let total = members.len();
    let online = members
        .iter()
        .filter(|member| chat.online(member.user_id))
        .count();
    let roster = column(members.into_iter().map(|member| member_row(member, chat)))
        .spacing(6)
        .width(Length::Fill);

    let room_title = chat
        .current()
        .map_or_else(String::new, |room| room.title(&chat.users, chat.member_id));

    let mut panel = column![
        text(format!("Members · {online}/{total}")).font(bold()),
        scrollable(roster).height(Length::Fill),
        text(format!("Voice · {room_title}")).font(bold()),
    ]
    .spacing(10)
    .padding(12);

    let mut speakers: Vec<&VoiceMember> = chat
        .voice_rosters
        .get(&chat.current_room)
        .map(|roster| roster.members.values().collect())
        .unwrap_or_default();
    speakers.sort_by_cached_key(|member| member.username.to_lowercase());
    for member in speakers {
        panel = panel.push(voice_member_row(member, chat));
    }
    panel = panel.push(voice_controls(chat));

    if chat.voice.session.is_some() {
        let mut hint = match config.transmit_mode {
            TransmitMode::PushToTalk => format!("Hold {} to talk", key_label(&config.ptt_key)),
            TransmitMode::VoiceActivation => "Voice activation on".to_owned(),
        };
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
    // One voice channel at a time: from any other room the only thing left to
    // do about it is leave it.
    if chat.voice.intent && chat.voice.room_id != chat.current_room {
        let title = chat.rooms.get(&chat.voice.room_id).map_or_else(
            || chat.voice.room_id.clone(),
            |room| room.title(&chat.users, chat.member_id),
        );
        return column![
            text(format!("In voice in {title}")).color(MUTED),
            button(text("Leave voice")).on_press(Message::LeaveVoice),
        ]
        .spacing(6)
        .into();
    }

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
    let user_id = member.user_id;
    let audio = chat.voice.peer_audio(user_id);
    let speaking = chat.speaking(user_id);

    let mut line = row![
        // Nothing of a locally muted member is heard, however loudly they talk.
        text("●").color(if speaking && !audio.muted {
            SUCCESS
        } else {
            MUTED
        }),
        text(member.username.as_str()).font(if speaking { bold() } else { Font::DEFAULT }),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    if user_id == chat.member_id {
        return line.push(text("(you)").color(MUTED)).into();
    }
    if audio.muted {
        line = line.push(text("(muted)").color(MUTED));
    }

    // The whole row opens the panel: this sidebar has no room for a control of
    // its own next to the name.
    let head = mouse_area(line).on_press(Message::ToggleMemberPanel(user_id));
    if chat.voice.expanded_member != Some(user_id) {
        return head.into();
    }

    column![
        head,
        row![
            slider(0.0..=2.0, audio.volume, move |volume| {
                Message::SetPeerVolume(user_id, volume)
            })
            .step(0.05_f32)
            .on_release(Message::PeerVolumeReleased(user_id)),
            text(format!("{:.0}%", audio.volume * 100.0)).color(MUTED),
        ]
        .spacing(6)
        .align_y(Vertical::Center),
        button(text(if audio.muted { "Unmute" } else { "Mute" }))
            .on_press(Message::TogglePeerMute(user_id)),
    ]
    .spacing(4)
    .into()
}

fn settings<'a>(
    state: &SettingsState,
    config: &Config,
    voice: &VoiceUi,
    update: UpdateView<'a>,
) -> Element<'a, Message> {
    let ptt: Element<'a, Message> = if state.capturing_ptt {
        text("Press a key or mouse button… (Esc cancels)")
            .color(WARNING)
            .into()
    } else {
        button(text("Change"))
            .on_press(Message::StartPttCapture)
            .into()
    };

    let window_only = matches!(voice.hotkey_status, HotkeyStatus::WindowOnly(_));
    let mut hotkey =
        row![
            text(hotkey_sentence(&voice.hotkey_status, config.transmit_mode))
                .color(if window_only { WARNING } else { MUTED }),
        ]
        .spacing(12)
        .align_y(Vertical::Center);
    // Retrying only makes sense while there is a session to listen for.
    if window_only && voice.session.is_some() {
        hotkey = hotkey.push(button(text("Retry")).on_press(Message::RetryHotkey));
    }

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
        text("Transmit"),
        row![
            radio(
                "Push to talk",
                TransmitMode::PushToTalk,
                Some(config.transmit_mode),
                Message::SetTransmitMode,
            ),
            radio(
                "Voice activation",
                TransmitMode::VoiceActivation,
                Some(config.transmit_mode),
                Message::SetTransmitMode,
            ),
        ]
        .spacing(16)
        .align_y(Vertical::Center),
        row![
            text("Threshold"),
            slider(
                VAD_MIN_DB..=VAD_MAX_DB,
                config.vad_threshold_db,
                Message::SetVadThreshold,
            )
            .step(1.0_f32)
            .on_release(Message::VadThresholdReleased)
            .width(METER_WIDTH),
            text(format!("{:.0} dB", config.vad_threshold_db)),
        ]
        .spacing(12)
        .align_y(Vertical::Center),
        input_meter(voice),
        text("Input cleanup"),
        row![
            toggler(config.noise_suppression)
                .label("Noise suppression")
                .on_toggle(Message::SetNoiseSuppression),
            toggler(config.echo_cancellation)
                .label("Echo cancellation")
                .on_toggle(Message::SetEchoCancellation),
            toggler(config.auto_gain)
                .label("Automatic gain")
                .on_toggle(Message::SetAutoGain),
        ]
        .spacing(16)
        .align_y(Vertical::Center),
        text(
            "Runs before the meter and the threshold. Echo cancellation removes what Vorcall plays from the voice channel; other apps' audio still comes through."
        )
        .color(MUTED),
        row![
            text(format!("Push-to-talk key: {}", key_label(&config.ptt_key))),
            ptt,
        ]
        .spacing(12)
        .align_y(Vertical::Center),
        hotkey,
        button(text("Back")).on_press(Message::CloseSettings),
        update_ui::section(update),
    ]
    .spacing(12)
    .padding(16)
    .width(Length::Fill)
    .into()
}

/// The microphone level the audio thread last reported, against the same scale
/// as the threshold slider. The bar turns green while the gate stands open.
fn input_meter<'a>(voice: &VoiceUi) -> Element<'a, Message> {
    let (level, gate_open) = match voice.input_level {
        Some((dbfs, gate_open)) => (dbfs.clamp(VAD_MIN_DB, 0.0), gate_open),
        None => (VAD_MIN_DB, false),
    };

    let mut meter = row![
        text("Input level"),
        progress_bar(VAD_MIN_DB..=0.0, level)
            .length(METER_WIDTH)
            .girth(10.0)
            .style(move |theme: &Theme| progress_bar::Style {
                bar: if gate_open {
                    SUCCESS.into()
                } else {
                    MUTED.into()
                },
                ..progress_bar::primary(theme)
            }),
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    if voice.input_level.is_none() {
        meter = meter.push(text("Join voice to see the input level").color(MUTED));
    }
    meter.into()
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
    let online = chat.online(member.user_id);

    let mut row = row![
        text("●").color(if online { SUCCESS } else { MUTED }),
        text(member.username.as_str())
            .font(if online { bold() } else { Font::DEFAULT })
            .color_maybe((!online).then_some(MUTED))
            .width(Length::Fill),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    if member.user_id == chat.member_id {
        return row.push(text("(you)").color(MUTED)).into();
    }
    row = row.push(
        button(text("DM").size(11))
            .padding([1, 5])
            .style(button::text)
            .on_press(Message::OpenDm(member.user_id)),
    );
    row.into()
}

fn overlay<'a>(dialog: &'a Dialog, chat: &'a ChatState) -> Element<'a, Message> {
    match dialog {
        Dialog::ChangePassword { .. } => change_password(dialog),
        Dialog::NewRoom { name, error } => new_room(name, error.as_deref()),
        Dialog::Image(id) => picture(chat, *id),
    }
}

fn new_room<'a>(name: &'a str, error: Option<&'a str>) -> Element<'a, Message> {
    let mut form = column![
        text("New room").size(20).font(bold()),
        text_input("Room name", name)
            .on_input(Message::NewRoomNameChanged)
            .on_submit(Message::CreateRoom)
            .padding(10)
            .width(FIELD_WIDTH),
    ]
    .spacing(12);

    if let Some(error) = error {
        form = form.push(text(error).color(WARNING));
    }
    form = form.push(
        row![
            button(text("Create"))
                .on_press(Message::CreateRoom)
                .padding(10),
            button(text("Cancel"))
                .on_press(Message::CloseDialog)
                .padding(10),
        ]
        .spacing(8),
    );

    form_dialog(form.into())
}

/// One attachment as large as the window allows.
fn picture(chat: &ChatState, id: i64) -> Element<'_, Message> {
    let full: Element<'_, Message> = match chat.images.get(&id) {
        // The handle is already capped at 1600 px on its longest side, so
        // `Contain` only ever shrinks it further.
        Some(ImageState::Ready(handle)) => image(handle.clone())
            .content_fit(ContentFit::Contain)
            .height(Length::Fill)
            .into(),
        Some(ImageState::Failed) => text("Image unavailable").color(MUTED).into(),
        Some(ImageState::Loading) | None => text("Loading image…").color(MUTED).into(),
    };

    let mut card = column![full].spacing(12).align_x(Horizontal::Center);
    if let Some(name) = attachment_name(chat, id) {
        card = card.push(text(name).color(MUTED));
    }
    card = card.push(
        button(text("Close"))
            .on_press(Message::CloseDialog)
            .padding(10),
    );

    // The card captures its own presses, so only what is truly outside the
    // picture reaches the backdrop and closes it.
    opaque(
        mouse_area(
            container(mouse_area(card).on_press(Message::Noop))
                .center(Length::Fill)
                .padding(24)
                .style(backdrop),
        )
        .on_press(Message::CloseDialog),
    )
}

/// The file name an attachment was uploaded under, when the room in view still
/// holds the message that carries it.
fn attachment_name(chat: &ChatState, id: i64) -> Option<&str> {
    chat.current()?
        .messages
        .values()
        .flat_map(|message| message.attachments.iter())
        .find(|attachment| attachment.id == id)
        .map(|attachment| attachment.file_name.as_str())
}

/// A form over the darkened chat. A press outside it is ignored on purpose:
/// half a filled-in form is not worth a stray click.
fn form_dialog(content: Element<'_, Message>) -> Element<'_, Message> {
    // `opaque` is what keeps the chat under the backdrop from taking the
    // clicks that miss the dialog.
    opaque(
        container(
            container(content)
                .padding(24)
                .style(container::bordered_box)
                .max_width(FIELD_WIDTH + 64.0),
        )
        .center(Length::Fill)
        .style(backdrop),
    )
}

fn change_password(dialog: &Dialog) -> Element<'_, Message> {
    let Dialog::ChangePassword {
        current,
        new,
        confirm,
        error,
        busy,
    } = dialog
    else {
        return Space::new().into();
    };

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

    form_dialog(form.into())
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
    mode: TransmitMode,
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
        // Push-to-talk still works, but only while this window has the focus.
        if mode == TransmitMode::PushToTalk
            && matches!(voice.hotkey_status, HotkeyStatus::WindowOnly(_))
        {
            label.push_str(" · PTT: window only");
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
