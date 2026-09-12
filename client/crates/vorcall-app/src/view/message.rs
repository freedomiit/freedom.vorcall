//! One message in the list: who wrote it, what it quotes, what it says, what is
//! attached to it, the reactions under it and the actions the pointer brings out.
//!
//! Consecutive messages from one author read as one group — no second avatar, no
//! second name — as long as nothing comes between them: a reply, a gap of more
//! than [`GROUP_WINDOW_MS`], a day boundary or the unread divider all start a new
//! one.

use chrono::{Local, NaiveDate, TimeZone as _};
use iced::alignment::{Horizontal, Vertical};
use iced::widget::text::Span;
use iced::widget::{
    Space, button, column, container, image, mouse_area, rich_text, row, span, stack, text, tooltip,
};
use iced::{ContentFit, Element, Font, Length, border, font, mouse};
use vorcall_core::mentions::{self, PALETTE, Segment};
use vorcall_core::{Attachment, ChatMessage, Reaction, ReplyRef, permissions};

use crate::app::message::{ChatMsg, MenuTarget, Message, UiMsg};
use crate::app::state::chat::ImageState;
use crate::app::state::rules::plain_text;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::chat::first_unread;
use crate::view::widgets::{self, Metrics};
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_SECONDARY, bold};
use crate::workers::images::ImageKey;

/// How long after a message another one from the same author still belongs to the
/// same group.
pub const GROUP_WINDOW_MS: i64 = 7 * 60 * 1000;
/// The gap between the avatar column and the body, from the design.
const GUTTER_GAP: f32 = 14.0;
/// The largest an attachment is drawn at in the list; the dialog shows the rest
/// of it. A placeholder keeps the design's box until the pixels arrive.
const ATTACHMENT_WIDTH: f32 = 400.0;
const ATTACHMENT_HEIGHT: f32 = 300.0;
const PLACEHOLDER_WIDTH: f32 = 320.0;
const PLACEHOLDER_HEIGHT: f32 = 180.0;
const PLACEHOLDER_ICON: f32 = 28.0;
/// One button of the strip that appears over a hovered message.
const ACTION_ICON: f32 = 16.0;

pub fn view<'a>(app: &'a App, main: &'a MainState, chat: &ChatMessage) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);
    let id = chat.id;
    let reacting = main.chat.reacting == Some(id);
    let confirming = main.chat.confirm_delete == Some(id);
    let open = app
        .ui
        .context_menu
        .is_some_and(|menu| menu.target == MenuTarget::Message(id));
    let lit = main.chat.hovered == Some(id) || reacting || confirming || open;
    let grouped = continues(main, chat);

    let mut body = column![].spacing(4).width(Length::Fill);
    if let Some(reply) = &chat.reply_to {
        body = body.push(quote(app, main, reply, metrics));
    }
    if !grouped {
        body = body.push(head(app, main, chat, metrics));
    }
    body = body.push(said(app, main, chat, metrics));
    for attachment in &chat.attachments {
        body = body.push(attachment_view(app, main, attachment, metrics));
    }
    if !chat.reactions.is_empty() {
        body = body.push(reactions(app, main, chat, metrics));
    }
    if reacting {
        body = body.push(palette(app, id, metrics));
    }
    if confirming {
        body = body.push(confirm(app, id, metrics));
    }

    let mut line = row![].spacing(GUTTER_GAP).width(Length::Fill);
    if let Some(gutter) = gutter(app, main, chat, grouped, metrics) {
        line = line.push(gutter);
    }
    line = line.push(body);

    let ground = container(line)
        .width(Length::Fill)
        .padding([2.0, 8.0])
        .style(styles::container::message_row(tokens, lit));

    let layered: Element<'_, Message> = if lit {
        stack![
            ground,
            container(actions(app, main, chat, confirming))
                .align_right(Length::Fill)
                .align_top(Length::Fill),
        ]
        .into()
    } else {
        ground.into()
    };

    mouse_area(layered)
        .on_enter(Message::Chat(ChatMsg::Hover(id)))
        .on_exit(Message::Chat(ChatMsg::Unhover(id)))
        .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Message(id))))
        .into()
}

/// Whether this message belongs to the group above it.
fn continues(main: &MainState, chat: &ChatMessage) -> bool {
    // A reply carries a quote of its own, so it always starts a group.
    if chat.reply_to.is_some() || chat.author_id == 0 {
        return false;
    }
    let Some(channel) = main.chat.channel(chat.channel_id) else {
        return false;
    };
    // The divider the list draws is something between the two.
    if first_unread(channel) == Some(chat.id) {
        return false;
    }
    let Some((_, previous)) = channel.messages.range(..chat.id).next_back() else {
        return false;
    };
    previous.author_id == chat.author_id
        && chat.sent_at_unix_ms - previous.sent_at_unix_ms <= GROUP_WINDOW_MS
        && same_day(previous.sent_at_unix_ms, chat.sent_at_unix_ms)
}

/// The avatar column: the author's picture on a group's first line, and the time
/// the pointer brings out on every line after it. `None` is the compact density,
/// which has no column at all.
fn gutter<'a>(
    app: &'a App,
    main: &'a MainState,
    chat: &ChatMessage,
    grouped: bool,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let size = metrics.message_avatar()?;
    if !grouped {
        let author_id = chat.author_id;
        return Some(
            mouse_area(widgets::member_avatar(main, author_id, size, tokens))
                .interaction(mouse::Interaction::Pointer)
                .on_press(Message::Ui(UiMsg::OpenProfileCard(author_id)))
                .into(),
        );
    }

    // On a grouped line the gutter is empty until the pointer asks what time it
    // was sent.
    let stamp: Element<'_, Message> = if main.chat.hovered == Some(chat.id) {
        text(time_of(chat.sent_at_unix_ms))
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted)
            .into()
    } else {
        Space::new().into()
    };
    Some(container(stamp).align_right(size).into())
}

/// A group's first line: the name in its role's colour, what marks the author,
/// the time and whether it has been edited since.
fn head<'a>(
    app: &'a App,
    main: &'a MainState,
    chat: &ChatMessage,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let author_id = chat.author_id;
    let color = match main.server.member_color(author_id) {
        0 => tokens.text_primary,
        rgb => widgets::color_of(rgb),
    };
    let name = if main.server.members.contains_key(&author_id) {
        main.server.display_name(author_id).to_owned()
    } else {
        // A message older than the account that wrote it, or one from somebody
        // who has been banned: the author's name travelled with it.
        chat.author.clone()
    };

    let mut line = row![
        mouse_area(
            text(name)
                .size(metrics.text(TEXT_BODY))
                .font(bold())
                .color(color)
        )
        .interaction(mouse::Interaction::Pointer)
        .on_press(Message::Ui(UiMsg::OpenProfileCard(author_id))),
    ]
    .spacing(8)
    .align_y(Vertical::Bottom);

    if let Some(role) = main.server.member_badge_role(author_id) {
        line = line.push(widgets::tooltip_of(
            widgets::role_icon(main, role, widgets::ICON_MARK, tokens),
            &role.name,
            tooltip::Position::Bottom,
            tokens,
        ));
    }
    if author_id != 0 && author_id == main.server.server.owner_id {
        line = line.push(widgets::owner_crown(tokens));
    }
    line = line.push(
        text(time_of(chat.sent_at_unix_ms))
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    );
    if !chat.deleted && chat.edited_at_unix_ms > 0 {
        line = line.push(
            text("(edited)")
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_muted),
        );
    }
    line.into()
}

/// What the message quotes, above everything else it says.
fn quote<'a>(
    app: &'a App,
    main: &'a MainState,
    reply: &ReplyRef,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let label = if reply.deleted {
        "Original message was deleted".to_owned()
    } else {
        format!(
            "{}: {}",
            reply.author,
            plain_text(&reply.excerpt, &main.user_pairs)
        )
    };

    row![
        icons::icon(Icon::Reply, widgets::ICON_MARK, tokens.text_muted),
        text(label)
            .size(metrics.text(TEXT_SECONDARY))
            .color(tokens.text_muted),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
}

/// The text itself: plain runs, the mentions as chips, and the tombstone a
/// deleted message leaves behind.
fn said<'a>(
    app: &'a App,
    main: &'a MainState,
    chat: &ChatMessage,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    if chat.deleted {
        return text("Message deleted")
            .size(metrics.text(TEXT_BODY))
            .font(italic())
            .color(tokens.text_muted)
            .into();
    }

    let mut spans: Vec<Span<'a>> = Vec::new();
    for segment in mentions::segments(&chat.text, &main.user_pairs) {
        match segment {
            Segment::Text(body) => spans.push(span(body).color(tokens.text_primary)),
            Segment::Mention { user_id, username } => spans.push(chip_span(
                format!("@{username}"),
                user_id == main.member_id,
                tokens,
            )),
            // The two words are only a chip where the server honoured them; from
            // a sender without the permission they are ordinary text.
            Segment::Everyone => spans.push(if chat.mention_everyone {
                chip_span(mentions::EVERYONE.to_owned(), true, tokens)
            } else {
                span(mentions::EVERYONE).color(tokens.text_primary)
            }),
            Segment::Here => spans.push(if chat.mention_here {
                chip_span(mentions::HERE.to_owned(), true, tokens)
            } else {
                span(mentions::HERE).color(tokens.text_primary)
            }),
        }
    }

    rich_text(spans)
        .size(metrics.text(TEXT_BODY))
        .width(Length::Fill)
        .into()
}

/// One mention, as a chip inside the line of text. `mine` is a mention of this
/// account, which is painted in the accent rather than in the ink.
fn chip_span<'a>(body: String, mine: bool, tokens: &ThemeTokens) -> Span<'a> {
    span(body)
        .color(if mine {
            tokens.accent
        } else {
            tokens.text_primary
        })
        .background(tokens.accent_tint)
        .border(border::rounded(styles::RADIUS_CHIP))
        .padding([0.0, 4.0])
        .font(bold())
}

/// One attachment, as large as the list draws it.
fn attachment_view<'a>(
    app: &'a App,
    main: &'a MainState,
    attachment: &Attachment,
    metrics: Metrics,
) -> Element<'a, Message> {
    let id = attachment.id;
    let key = ImageKey::Attachment(id);
    match main.chat.images.get(&key) {
        Some(ImageState::Ready(handle)) => mouse_area(
            container(image(handle.clone()).content_fit(ContentFit::Contain))
                .max_width(ATTACHMENT_WIDTH)
                .max_height(ATTACHMENT_HEIGHT),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_press(Message::Chat(ChatMsg::OpenImage(id)))
        .into(),
        Some(ImageState::Failed) => placeholder(app, Icon::Warning, "Image unavailable", metrics),
        Some(ImageState::Loading) | None => {
            placeholder(app, Icon::Image, "Loading the image…", metrics)
        }
    }
}

/// The box an attachment keeps while its pixels are on the way, or once they are
/// not coming.
fn placeholder<'a>(
    app: &'a App,
    glyph: Icon,
    label: &str,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    container(
        column![
            icons::icon(glyph, PLACEHOLDER_ICON, tokens.text_secondary),
            text(label.to_owned())
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_x(Horizontal::Center),
    )
    .center_x(PLACEHOLDER_WIDTH)
    .center_y(PLACEHOLDER_HEIGHT)
    .style(styles::container::input(tokens))
    .into()
}

/// The reactions a message carries, each a chip that adds or takes mine back.
fn reactions<'a>(
    app: &'a App,
    main: &'a MainState,
    chat: &ChatMessage,
    metrics: Metrics,
) -> Element<'a, Message> {
    let id = chat.id;
    row(chat
        .reactions
        .iter()
        .map(|reaction| chip(app, main, id, reaction, metrics)))
    .spacing(6)
    .into()
}

fn chip<'a>(
    app: &'a App,
    main: &'a MainState,
    message_id: i64,
    reaction: &Reaction,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mine = main
        .chat
        .reacted(message_id, &reaction.emoji, main.member_id);
    let label = format!(
        "{} {}",
        glyph_of(app, &reaction.emoji),
        reaction.user_ids.len()
    );

    let mut control = button(text(label).size(metrics.text(TEXT_SECONDARY)))
        .padding([2.0, 8.0])
        .style(styles::button::reaction(tokens, mine));
    // Only the fixed palette goes back to the server, so a reaction from anywhere
    // else is shown and left alone.
    if let Some(emoji) = palette_emoji(&reaction.emoji) {
        control = control.on_press(Message::Chat(ChatMsg::React(message_id, emoji)));
    }

    widgets::tooltip_of(
        control,
        &who(main, reaction),
        tooltip::Position::Bottom,
        tokens,
    )
}

/// Who is in one reaction, which is what its tooltip says.
fn who(main: &MainState, reaction: &Reaction) -> String {
    let names: Vec<&str> = reaction
        .user_ids
        .iter()
        .map(|user_id| main.server.display_name(*user_id))
        .collect();
    names.join(", ")
}

/// The palette, once a row has been asked for it.
fn palette<'a>(app: &'a App, message_id: i64, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut strip = row![].spacing(4).align_y(Vertical::Center);
    for emoji in PALETTE {
        strip = strip.push(widgets::tooltip_of(
            button(text(glyph_of(app, emoji)).size(metrics.text(TEXT_BODY)))
                .padding([2.0, 6.0])
                .style(styles::button::reaction(tokens, false))
                .on_press(Message::Chat(ChatMsg::React(message_id, emoji))),
            mentions::reaction_label(emoji),
            tooltip::Position::Top,
            tokens,
        ));
    }
    strip = strip.push(widgets::icon_button_in(
        Icon::Close,
        "Close",
        Some(Message::Chat(ChatMsg::OpenReactions(None))),
        tokens.text_muted,
        widgets::ICON_MARK,
        tokens,
    ));
    strip.into()
}

/// The second press a delete takes, in the row itself.
fn confirm<'a>(app: &'a App, message_id: i64, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    row![
        container(
            text("This cannot be undone")
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.warning),
        )
        .padding([1.0, 6.0])
        .style(styles::container::warning_chip(tokens)),
        button(text("Delete").size(metrics.text(TEXT_SECONDARY)))
            .padding([2.0, 8.0])
            .style(styles::button::danger(tokens))
            .on_press(Message::Chat(ChatMsg::ConfirmDelete(message_id))),
        button(text("Cancel").size(metrics.text(TEXT_SECONDARY)))
            .padding([2.0, 8.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Chat(ChatMsg::CancelDelete)),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

/// The strip the pointer brings out over the top right of a row.
fn actions<'a>(
    app: &'a App,
    main: &'a MainState,
    chat: &ChatMessage,
    confirming: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let id = chat.id;
    let mine = chat.author_id != 0 && chat.author_id == main.member_id;
    let manage = main
        .server
        .can(permissions::MANAGE_MESSAGES, Some(chat.channel_id));

    let mut strip = row![].spacing(2).align_y(Vertical::Center);
    if !chat.deleted {
        if main
            .server
            .can(permissions::SEND_MESSAGES, Some(chat.channel_id))
        {
            strip = strip.push(action(
                Icon::Reply,
                "Reply",
                Some(Message::Chat(ChatMsg::ReplyTo(id))),
                tokens,
            ));
        }
        if main
            .server
            .can(permissions::ADD_REACTIONS, Some(chat.channel_id))
        {
            strip = strip.push(action(
                Icon::Smile,
                "Add a reaction",
                Some(Message::Chat(ChatMsg::OpenReactions(Some(id)))),
                tokens,
            ));
        }
        if mine {
            strip = strip.push(action(
                Icon::Edit,
                "Edit",
                Some(Message::Chat(ChatMsg::StartEdit(id))),
                tokens,
            ));
        }
        if mine || manage {
            // The first press arms the delete, and the row itself asks again.
            strip = strip.push(action(
                Icon::Trash,
                if confirming { "Delete?" } else { "Delete" },
                Some(Message::Chat(ChatMsg::Delete(id))),
                tokens,
            ));
        }
    }
    strip = strip.push(action(
        Icon::Dots,
        "More",
        Some(Message::Ui(UiMsg::ContextMenu(MenuTarget::Message(id)))),
        tokens,
    ));

    container(strip)
        .padding(2)
        .style(styles::container::hover_strip(tokens))
        .into()
}

fn action<'a>(
    glyph: Icon,
    tip: &str,
    press: Option<Message>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    widgets::icon_button_in(
        glyph,
        tip,
        press,
        tokens.text_secondary,
        ACTION_ICON,
        tokens,
    )
}

/// The emoji itself, or its ASCII label where the machine's fonts cannot draw
/// one.
pub fn glyph_of<'a>(app: &App, emoji: &'a str) -> &'a str {
    if app.config.text_reactions {
        mentions::reaction_label(emoji)
    } else {
        emoji
    }
}

/// The palette entry a stored reaction is, which is the only form the server
/// takes back.
fn palette_emoji(emoji: &str) -> Option<&'static str> {
    PALETTE.into_iter().find(|entry| *entry == emoji)
}

/// The local time a message was sent.
pub fn time_of(sent_at_unix_ms: i64) -> String {
    match Local.timestamp_millis_opt(sent_at_unix_ms).single() {
        Some(stamp) => stamp.format("%H:%M").to_string(),
        None => String::new(),
    }
}

/// The local day a message was sent, which is what the list divides on.
pub fn day_of(sent_at_unix_ms: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(sent_at_unix_ms)
        .single()
        .map(|stamp| stamp.date_naive())
}

pub fn same_day(a: i64, b: i64) -> bool {
    match (day_of(a), day_of(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// What the divider between two days says.
pub fn day_label(sent_at_unix_ms: i64) -> String {
    let Some(day) = day_of(sent_at_unix_ms) else {
        return String::new();
    };
    let today = Local::now().date_naive();
    match (today - day).num_days() {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => day.format("%-d %b %Y").to_string(),
    }
}

fn italic() -> Font {
    Font {
        style: font::Style::Italic,
        ..Font::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three days apart, in local time, so the labels do not depend on a zone.
    fn at(days: i64, hour: u32, minute: u32) -> i64 {
        let day = Local::now().date_naive() - chrono::Duration::days(days);
        let naive = day.and_hms_opt(hour, minute, 0).expect("a valid time");
        Local
            .from_local_datetime(&naive)
            .single()
            .expect("one local time")
            .timestamp_millis()
    }

    #[test]
    fn the_day_divider_names_today_and_yesterday() {
        assert_eq!(day_label(at(0, 12, 0)), "Today");
        assert_eq!(day_label(at(1, 12, 0)), "Yesterday");
        assert!(!day_label(at(30, 12, 0)).is_empty());
        assert_eq!(day_label(i64::MAX), String::new());
    }

    #[test]
    fn two_times_on_one_day_are_one_day() {
        assert!(same_day(at(0, 1, 0), at(0, 23, 0)));
        assert!(!same_day(at(0, 12, 0), at(1, 12, 0)));
    }

    #[test]
    fn only_the_palette_goes_back_to_the_server() {
        assert_eq!(palette_emoji("🔥"), Some("🔥"));
        assert_eq!(palette_emoji("🐧"), None);
    }

    /// The grouping window is the design's seven minutes, either side of it.
    #[test]
    fn the_group_window_is_seven_minutes() {
        assert_eq!(GROUP_WINDOW_MS, 420_000);
    }
}
