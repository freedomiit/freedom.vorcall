//! The chat column: the channel header, the message list and the composer — or
//! the stage over a slimmer list, while a share is being watched here.

use iced::alignment::Vertical;
use iced::widget::text::Wrapping;
use iced::widget::{Id, Space, button, column, container, row, rule, scrollable, stack, text};
use iced::{Element, Length, Padding};
use vorcall_core::ChannelKind;

use crate::app::message::{ChatMsg, Message, UiMsg, VoiceMsg};
use crate::app::state::chat::ChannelUi;
use crate::app::state::server::channel_kind;
use crate::app::{App, MainState, Status};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::widgets::{self, Metrics};
use crate::view::{
    AVATAR_SMALL, HEADER_HEIGHT, MESSAGES_ID, TEXT_BADGE, TEXT_ROW, TEXT_SECONDARY, TEXT_TITLE,
    bold,
};
use crate::view::{channels, composer, message, stage};

/// How many unread messages still get a divider. Further back than this it would
/// be off the screen anyway, and finding it would cost a walk of the whole
/// buffer on every row.
const UNREAD_DIVIDER_MAX: u32 = 50;
/// How the column is split while the stage is in it.
const STAGE_PORTION: u16 = 3;
const LIST_PORTION: u16 = 2;
/// The separator between a channel's name and its topic.
const SEPARATOR_HEIGHT: f32 = 20.0;

pub fn pane<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);
    let watching = watching_here(main);

    let body: Element<'_, Message> = match (watching, main.chat.current.channel_id()) {
        // Nothing to read beside it: the stage is the whole column.
        (true, None) => stage::in_chat(app, main),
        (true, Some(channel_id)) => column![
            container(stage::in_chat(app, main)).height(Length::FillPortion(STAGE_PORTION)),
            container(conversation(app, main, channel_id, metrics))
                .height(Length::FillPortion(LIST_PORTION)),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        (false, Some(channel_id)) => conversation(app, main, channel_id, metrics),
        (false, None) => channels::nothing_selected(app),
    };

    container(
        column![
            header(app, main, metrics),
            rule::horizontal(1.0).style(styles::rule(tokens)),
            body,
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(styles::container::chat(tokens))
    .into()
}

/// Whether the stage belongs in this column: something is being watched and the
/// pop-out window is not holding it.
pub fn watching_here(main: &MainState) -> bool {
    main.voice.watch.state.is_some() && main.voice.watch.popped.is_none()
}

/// Where the "new messages" divider goes: the oldest message of the unread run at
/// the end of the channel.
pub fn first_unread(channel: &ChannelUi) -> Option<i64> {
    if channel.unread == 0 || channel.unread > UNREAD_DIVIDER_MAX {
        return None;
    }
    let unread = channel.unread as usize;
    channel.messages.keys().rev().nth(unread - 1).copied()
}

/// The channel's name, its topic, and the switches that belong to the column.
fn header<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let dm = channels::is_dm(main);
    let channel = main.chat.current.channel_id();

    let mut bar = row![].spacing(10).align_y(Vertical::Center);
    match channel {
        Some(channel_id) if dm => {
            let partner = main
                .server
                .channel(channel_id)
                .and_then(|channel| main.server.dm_partner(channel));
            if let Some(user_id) = partner {
                bar = bar.push(widgets::member_avatar_on(
                    main,
                    user_id,
                    AVATAR_SMALL,
                    tokens.bg_chat,
                    tokens,
                ));
            }
            bar = bar.push(
                text(main.server.channel_title(channel_id))
                    .size(metrics.text(TEXT_TITLE))
                    .font(bold())
                    .color(tokens.text_primary)
                    .wrapping(Wrapping::None)
                    .width(Length::Fill),
            );
        }
        Some(channel_id) => {
            let kind = main.server.channel(channel_id).map(channel_kind);
            bar = bar.push(icons::icon(
                if kind == Some(ChannelKind::Voice) {
                    Icon::Speaker
                } else {
                    Icon::Hash
                },
                widgets::ICON_SIZE,
                tokens.text_secondary,
            ));
            bar = bar.push(
                text(main.server.channel_title(channel_id))
                    .size(metrics.text(TEXT_TITLE))
                    .font(bold())
                    .color(tokens.text_primary)
                    .wrapping(Wrapping::None),
            );
            let topic = main
                .server
                .channel(channel_id)
                .map(|channel| channel.topic.clone())
                .unwrap_or_default();
            if topic.is_empty() {
                bar = bar.push(Space::new().width(Length::Fill));
            } else {
                bar = bar.push(
                    container(rule::vertical(1.0).style(styles::rule(tokens)))
                        .height(SEPARATOR_HEIGHT),
                );
                bar = bar.push(
                    text(topic)
                        .size(metrics.text(TEXT_ROW))
                        .color(tokens.text_secondary)
                        .wrapping(Wrapping::None)
                        .width(Length::Fill),
                );
            }
        }
        None => bar = bar.push(Space::new().width(Length::Fill)),
    }

    if let Some(channel_id) = channel.filter(|_| dm) {
        let in_this_call = main.voice.intent && main.voice.channel_id == channel_id;
        bar = bar.push(widgets::icon_button(
            Icon::Speaker,
            if in_this_call {
                "You are in this call"
            } else {
                "Call"
            },
            (!in_this_call).then_some(Message::Voice(VoiceMsg::Join(channel_id))),
            tokens,
        ));
    }
    bar = bar.push(widgets::icon_button(
        Icon::Search,
        "Find a channel or a person",
        Some(Message::Ui(UiMsg::OpenQuickSwitcher)),
        tokens,
    ));
    if !dm {
        bar = bar.push(widgets::icon_button(
            Icon::Users,
            if app.ui.show_members {
                "Hide the member list"
            } else {
                "Show the member list"
            },
            Some(Message::Ui(UiMsg::ToggleMembers)),
            tokens,
        ));
    }

    container(bar)
        .width(Length::Fill)
        .padding([0.0, 16.0])
        .center_y(metrics.height(HEADER_HEIGHT))
        .style(styles::container::chat(tokens))
        .into()
}

/// The messages of one channel, whatever it is saying about itself, and the
/// composer under them.
fn conversation<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let Some(channel) = main.chat.channel(channel_id) else {
        return column![
            widgets::loading_state(app.loading_elapsed, tokens),
            status_bar(app, main, metrics),
            composer::view(app, main),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    };

    // Nothing has arrived yet, and nothing says it will not.
    let body: Element<'_, Message> = if channel.loading && !channel.loaded {
        widgets::loading_state(app.loading_elapsed, tokens)
    } else if channel.loaded && channel.messages.is_empty() {
        widgets::empty_state(
            Icon::Hash,
            &format!(
                "This is the beginning of {}",
                main.server.channel_title(channel_id)
            ),
            "Say something, and everyone who can see this channel sees it.",
            tokens,
        )
    } else {
        messages(app, main, channel, metrics)
    };

    column![
        body,
        status_bar(app, main, metrics),
        composer::view(app, main)
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The list itself, with the pill that takes a reader who has scrolled away back
/// to the bottom.
fn messages<'a>(
    app: &'a App,
    main: &'a MainState,
    channel: &'a ChannelUi,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut list = column![]
        .spacing(metrics.group_spacing())
        .padding(Padding::ZERO.top(16.0).bottom(8.0).left(16.0).right(16.0))
        .width(Length::Fill);

    if channel.loading_older {
        list = list.push(
            container(
                text("Loading older messages…")
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.text_muted),
            )
            .center_x(Length::Fill),
        );
    } else if channel.has_older {
        list = list.push(
            container(
                button(text("Load older messages").size(metrics.text(TEXT_SECONDARY)))
                    .padding([4.0, 12.0])
                    .style(styles::button::secondary(tokens))
                    .on_press(Message::Chat(ChatMsg::LoadOlder)),
            )
            .center_x(Length::Fill),
        );
    }
    if let Some(error) = &channel.history_error {
        list = list.push(
            container(
                text(error.clone())
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.danger),
            )
            .center_x(Length::Fill),
        );
    }

    let divider_at = first_unread(channel);
    let mut previous: Option<i64> = None;
    for entry in channel.messages.values() {
        if previous.is_none_or(|sent| !message::same_day(sent, entry.sent_at_unix_ms)) {
            list = list.push(day_divider(app, entry.sent_at_unix_ms, metrics));
        }
        if divider_at == Some(entry.id) {
            list = list.push(new_divider(app, metrics));
        }
        previous = Some(entry.sent_at_unix_ms);
        list = list.push(message::view(app, main, entry));
    }

    // Under `Anchor::End` a relative offset of zero is the bottom of the list.
    let scroller = scrollable(list)
        .id(Id::new(MESSAGES_ID))
        .anchor_y(scrollable::Anchor::End)
        .on_scroll(|viewport| Message::Chat(ChatMsg::Scrolled(viewport)))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::scrollable(tokens));

    // iced matches widget state by position and tag in a tree mirroring this one, so a
    // conditional wrapper around the scrollable would rebuild it with a fresh state and
    // snap the offset back to the bottom on every scroll: the shape has to stay constant,
    // only the pill's content may change.
    let overlay: Element<'a, Message> = if channel.at_bottom {
        Space::new().into()
    } else {
        let label = match channel.pending_new {
            0 => "Jump to the latest ↓".to_owned(),
            1 => "1 new message ↓".to_owned(),
            count => format!("{count} new messages ↓"),
        };
        button(text(label).size(metrics.text(TEXT_SECONDARY)))
            .padding([4.0, 12.0])
            .style(styles::button::primary(tokens))
            .on_press(Message::Chat(ChatMsg::JumpToLatest))
            .into()
    };
    stack![
        scroller,
        container(overlay)
            .center_x(Length::Fill)
            .align_bottom(Length::Fill)
            .padding(Padding::ZERO.bottom(8.0)),
    ]
    .into()
}

/// The day a run of messages was sent on.
fn day_divider<'a>(app: &'a App, sent_at_unix_ms: i64, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    container(
        container(
            text(message::day_label(sent_at_unix_ms))
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_secondary),
        )
        .padding([2.0, 8.0])
        .style(styles::container::pill(tokens)),
    )
    .center_x(Length::Fill)
    .into()
}

/// Where reading stopped last time.
fn new_divider<'a>(app: &'a App, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    row![
        container(Space::new())
            .width(Length::Fill)
            .height(1.0)
            .style(styles::container::mention_line(tokens)),
        text("NEW")
            .size(metrics.text(TEXT_BADGE))
            .font(bold())
            .color(tokens.mention),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

/// The slim line that says the connection is not what it should be. Nothing is
/// drawn while it is.
fn status_bar<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let sentence = match &main.status {
        Status::Connected => None,
        Status::Connecting => Some(("Connecting…".to_owned(), tokens.warning)),
        Status::Reconnecting { in_secs } => {
            Some((format!("Reconnecting in {in_secs}s…"), tokens.warning))
        }
        Status::Unauthorized => Some(("Signed out by the server".to_owned(), tokens.danger)),
        Status::Disconnected(error) => Some((error.clone(), tokens.danger)),
    };
    let (sentence, color) = match (sentence, &main.notice) {
        (Some(line), _) => line,
        // A server complaint is worth the same line while the connection is fine.
        (None, Some(notice)) => (notice.clone(), tokens.warning),
        (None, None) => return Space::new().into(),
    };

    container(
        row![
            icons::icon(Icon::Warning, widgets::ICON_MARK, color),
            text(sentence)
                .size(metrics.text(TEXT_SECONDARY))
                .color(color),
        ]
        .spacing(6)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .padding(Padding::ZERO.left(16.0).right(16.0).top(2.0).bottom(2.0))
    .into()
}
