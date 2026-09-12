//! The message input: a multi-line editor where Enter sends and Shift+Enter
//! breaks the line, with everything the next send will carry above it.

use iced::alignment::Vertical;
use iced::widget::text_editor::{Binding, KeyPress};
use iced::widget::{Id, Space, button, column, container, image, row, text, text_editor, tooltip};
use iced::{Element, Length, Padding, keyboard};
use vorcall_core::mentions::{self, PALETTE};
use vorcall_core::{Attachment, attachments, permissions};

use crate::app::message::{ChatMsg, Message};
use crate::app::state::chat::{COMPOSER_PALETTE, MESSAGE_MAX_CHARS};
use crate::app::state::rules::plain_text;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::message::glyph_of;
use crate::view::widgets::{self, Metrics};
use crate::view::{COMPOSER_ID, TEXT_BADGE, TEXT_BODY, TEXT_SECONDARY};
use crate::workers::images::ImageKey;

/// How tall the composer is before it grows with the text, and how many lines it
/// grows to before it scrolls instead.
const MIN_EDITOR_HEIGHT: f32 = 28.0;
const MAX_LINES: f32 = 6.0;
/// How tall one line of the editor is, as a share of its text size.
const LINE_HEIGHT: f32 = 1.4;
/// How many names the mention list offers before it stops being a shortcut.
const SUGGESTIONS: usize = 8;
/// How much of a quoted message the reply banner shows.
const EXCERPT_MAX: usize = 60;
/// The thumbnail a pending attachment is drawn as.
const THUMB: f32 = 40.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);
    let composer = &main.chat.composer;
    let channel = main.chat.current.channel_id();

    let allowed = |bit: u64| channel.is_some_and(|id| main.server.can(bit, Some(id)));
    let can_send = allowed(permissions::SEND_MESSAGES);
    let can_attach = allowed(permissions::ATTACH_FILES);
    let can_everyone = allowed(permissions::MENTION_EVERYONE);

    let mut panel = column![]
        .spacing(6)
        .padding(Padding::ZERO.left(16.0).right(16.0).bottom(8.0))
        .width(Length::Fill);

    if let Some(query) = &composer.mention_query
        && let Some(list) = mention_list(app, main, query, metrics)
    {
        panel = panel.push(list);
    }
    if let Some(banner) = banner(app, main, metrics) {
        panel = panel.push(banner);
    }
    if let Some(warning) = everyone_warning(app, main, can_everyone, metrics) {
        panel = panel.push(warning);
    }
    if !composer.attachments.is_empty() || composer.uploading > 0 {
        panel = panel.push(strip(app, main, metrics));
    }
    if main.chat.reacting == Some(COMPOSER_PALETTE) {
        panel = panel.push(emoji_palette(app, metrics));
    }
    panel = panel.push(input(app, main, can_send, can_attach, metrics));
    panel = panel.push(footer(app, main, can_send, metrics));

    container(panel)
        .width(Length::Fill)
        .style(styles::container::chat(tokens))
        .into()
}

/// The box itself: the attach button, the editor, and what cannot go in it.
fn input<'a>(
    app: &'a App,
    main: &'a MainState,
    can_send: bool,
    can_attach: bool,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let composer = &main.chat.composer;
    let size = metrics.text(TEXT_BODY);

    let mut editor = text_editor(&composer.content)
        .id(Id::new(COMPOSER_ID))
        .placeholder(placeholder(main, can_send))
        .min_height(metrics.height(MIN_EDITOR_HEIGHT))
        .max_height(size * LINE_HEIGHT * MAX_LINES)
        .padding(0.0)
        .size(size)
        .line_height(LINE_HEIGHT)
        .style(styles::text_editor_bare(tokens));
    // An editor with no action to report is a disabled one, which is what a
    // channel this account may not write in gets.
    if can_send {
        let editing = composer.editing;
        let replying = composer.reply_to.is_some();
        let empty = composer.content.text().trim().is_empty();
        let last_own = main
            .chat
            .current
            .channel_id()
            .and_then(|id| main.chat.channel(id))
            .and_then(|channel| channel.last_own_message_id(main.member_id));

        editor = editor
            .on_action(|action| Message::Chat(ChatMsg::Editor(action)))
            .key_binding(move |press| binding(press, editing, replying, empty, last_own));
    }

    let held = composer.attachments.len() + composer.uploading;
    let (attach, attach_tip) = if !can_attach {
        (None, "Requires Attach Files")
    } else if composer.editing.is_some() {
        // An edit carries no attachments of its own.
        (None, "An edit cannot carry an image")
    } else if held >= attachments::MAX_PER_MESSAGE {
        (None, "That is as many images as one message takes")
    } else {
        (
            Some(Message::Chat(ChatMsg::PickAttachment)),
            "Attach an image",
        )
    };

    // The same palette a message's reactions come from, into the text instead.
    let palette_open = main.chat.reacting == Some(COMPOSER_PALETTE);
    let emoji = widgets::icon_button(
        Icon::Smile,
        if palette_open { "Close" } else { "Emoji" },
        can_send.then(|| {
            Message::Chat(ChatMsg::OpenReactions(
                (!palette_open).then_some(COMPOSER_PALETTE),
            ))
        }),
        tokens,
    );

    let box_row = row![
        widgets::icon_button(Icon::Paperclip, attach_tip, attach, tokens),
        editor,
        counter(app, main, metrics),
        emoji,
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    container(box_row)
        .width(Length::Fill)
        .padding([8.0, 10.0])
        .style(styles::container::input(tokens))
        .into()
}

/// The emoji the composer offers, each one pressed into the text at the caret. It
/// stays open until it is closed: one press per emoji.
fn emoji_palette<'a>(app: &'a App, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut strip = row![].spacing(4).align_y(Vertical::Center);
    for emoji in PALETTE {
        strip = strip.push(widgets::tooltip_of(
            button(text(glyph_of(app, emoji)).size(metrics.text(TEXT_BODY)))
                .padding([2.0, 6.0])
                .style(styles::button::reaction(tokens, false))
                .on_press(Message::Chat(ChatMsg::InsertEmoji(emoji))),
            mentions::reaction_label(emoji),
            tooltip::Position::Top,
            tokens,
        ));
    }
    strip = strip.push(Space::new().width(Length::Fill));
    strip = strip.push(widgets::icon_button_in(
        Icon::Close,
        "Close",
        Some(Message::Chat(ChatMsg::OpenReactions(None))),
        tokens.text_secondary,
        widgets::ICON_MARK,
        tokens,
    ));

    container(strip)
        .width(Length::Fill)
        .padding(6)
        .style(styles::container::popover(tokens))
        .into()
}

/// Enter sends; Shift+Enter breaks the line; Escape unwinds a reply or an edit;
/// Up in an empty composer reaches for the last message of one's own. Everything
/// else is the editor's own.
fn binding(
    press: KeyPress,
    editing: Option<i64>,
    replying: bool,
    empty: bool,
    last_own: Option<i64>,
) -> Option<Binding<Message>> {
    use keyboard::key::Named;

    match &press.key {
        keyboard::Key::Named(Named::Enter) => {
            return if press.modifiers.shift() {
                Some(Binding::Insert('\n'))
            } else {
                Some(Binding::Custom(Message::Chat(ChatMsg::Send)))
            };
        }
        keyboard::Key::Named(Named::Escape) if editing.is_some() => {
            return Some(Binding::Custom(Message::Chat(ChatMsg::CancelEdit)));
        }
        keyboard::Key::Named(Named::Escape) if replying => {
            return Some(Binding::Custom(Message::Chat(ChatMsg::CancelReply)));
        }
        keyboard::Key::Named(Named::ArrowUp) if empty && editing.is_none() => {
            if let Some(id) = last_own {
                return Some(Binding::Custom(Message::Chat(ChatMsg::StartEdit(id))));
            }
        }
        _ => {}
    }
    Binding::from_key_press(press)
}

/// What the empty composer says, which names the channel it writes to.
fn placeholder(main: &MainState, can_send: bool) -> String {
    match main.chat.current.channel_id() {
        Some(_) if !can_send => "You cannot send messages here".to_owned(),
        Some(id) => format!("Message {}", main.server.channel_title(id)),
        None => "Pick a channel first".to_owned(),
    }
}

/// How close the text is to the limit the server keeps. Nothing is drawn until it
/// is worth saying.
fn counter<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let length = main.chat.composer.content.text().chars().count();
    if length <= MESSAGE_MAX_CHARS * 9 / 10 {
        return Space::new().into();
    }
    text(format!("{length}/{MESSAGE_MAX_CHARS}"))
        .size(metrics.text(TEXT_BADGE))
        .color(if length > MESSAGE_MAX_CHARS {
            tokens.danger
        } else {
            tokens.text_muted
        })
        .into()
}

/// What the next send will do, when it is not simply a new message.
fn banner<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let composer = &main.chat.composer;

    let (glyph, label, cancel) = if composer.editing.is_some() {
        (
            Icon::Edit,
            "Editing a message".to_owned(),
            ChatMsg::CancelEdit,
        )
    } else {
        let id = composer.reply_to?;
        let label = match main.chat.message(id) {
            Some(message) => format!(
                "Replying to {}: {}",
                message.author,
                excerpt(&message.text, &main.user_pairs)
            ),
            None => "Replying to a message".to_owned(),
        };
        (Icon::Reply, label, ChatMsg::CancelReply)
    };

    Some(
        container(
            row![
                icons::icon(glyph, widgets::ICON_MARK, tokens.text_secondary),
                text(label)
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.text_secondary)
                    .width(Length::Fill),
                widgets::icon_button_in(
                    Icon::Close,
                    "Cancel",
                    Some(Message::Chat(cancel)),
                    tokens.text_secondary,
                    widgets::ICON_MARK,
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        )
        .width(Length::Fill)
        .padding([2.0, 8.0])
        .style(styles::container::elevated(tokens))
        .into(),
    )
}

/// What a quoted message reads as: the names a token stands for, cut short.
fn excerpt(body: &str, users: &[(i64, String)]) -> String {
    let rendered = plain_text(body, users);
    if rendered.chars().count() <= EXCERPT_MAX {
        return rendered;
    }
    rendered.chars().take(EXCERPT_MAX).collect::<String>() + "…"
}

/// The note about an `@everyone` that will reach nobody.
fn everyone_warning<'a>(
    app: &'a App,
    main: &'a MainState,
    can_everyone: bool,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    if can_everyone {
        return None;
    }
    let body = main.chat.composer.content.text();
    let word = if mentions::mentions_everyone(&body) {
        mentions::EVERYONE
    } else if mentions::mentions_here(&body) {
        mentions::HERE
    } else {
        return None;
    };

    let tokens = &app.tokens;
    Some(
        container(
            row![
                icons::icon(Icon::Warning, widgets::ICON_MARK, tokens.warning),
                text(format!(
                    "{word} needs Mention Everyone — it will notify nobody"
                ))
                .size(metrics.text(TEXT_SECONDARY)),
            ]
            .spacing(6)
            .align_y(Vertical::Center),
        )
        .padding([2.0, 8.0])
        .style(styles::container::warning_chip(tokens))
        .into(),
    )
}

/// The names the mention list offers for what has been typed after the `@`.
fn mention_list<'a>(
    app: &'a App,
    main: &'a MainState,
    query: &str,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let names = suggestions(&main.user_pairs, query);
    if names.is_empty() {
        return None;
    }

    let mut list = column![].spacing(2).width(Length::Fill);
    for (user_id, username) in names {
        let line = row![
            widgets::member_avatar(main, user_id, widgets::AVATAR_OCCUPANT, tokens),
            text(main.server.display_name(user_id))
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_primary),
            text(format!("@{username}"))
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_y(Vertical::Center);

        list = list.push(
            button(line)
                .width(Length::Fill)
                .padding([metrics.row_padding(), 6.0])
                .style(styles::button::row(tokens))
                .on_press(Message::Chat(ChatMsg::MentionPick(username.to_owned()))),
        );
    }

    Some(
        container(list)
            .width(Length::Fill)
            .padding(6)
            .style(styles::container::popover(tokens))
            .into(),
    )
}

/// Everyone whose username starts with what has been typed, by name.
fn suggestions<'a>(pairs: &'a [(i64, String)], query: &str) -> Vec<(i64, &'a str)> {
    let query = query.to_lowercase();
    let mut names: Vec<(i64, &str)> = pairs
        .iter()
        .filter(|(_, name)| name.to_lowercase().starts_with(&query))
        .map(|(user_id, name)| (*user_id, name.as_str()))
        .collect();
    names.sort_by_cached_key(|(_, name)| name.to_lowercase());
    names.truncate(SUGGESTIONS);
    names
}

/// What is already uploaded for the next message, and what is still going up.
fn strip<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut line = row![].spacing(6).align_y(Vertical::Center);
    for attachment in &main.chat.composer.attachments {
        line = line.push(pending(app, main, attachment, metrics));
    }
    for _ in 0..main.chat.composer.uploading {
        line = line.push(
            container(
                text("Uploading…")
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.text_muted),
            )
            .padding([4.0, 8.0])
            .style(styles::container::chip(tokens)),
        );
    }
    line.into()
}

/// One image the next message will carry, with the way to take it back out.
fn pending<'a>(
    app: &'a App,
    main: &'a MainState,
    attachment: &Attachment,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let key = ImageKey::Attachment(attachment.id);
    let thumb: Element<'_, Message> = match widgets::image_handle(&main.chat, key) {
        Some(handle) => image(handle).width(THUMB).height(THUMB).into(),
        None => Space::new().width(THUMB).height(THUMB).into(),
    };

    container(
        row![
            thumb,
            text(attachment.file_name.clone())
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_secondary),
            widgets::icon_button_in(
                Icon::Close,
                "Remove",
                Some(Message::Chat(ChatMsg::RemovePendingAttachment(
                    attachment.id
                ))),
                tokens.text_secondary,
                widgets::ICON_MARK,
                tokens,
            ),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    )
    .padding([4.0, 6.0])
    .style(styles::container::chip(tokens))
    .into()
}

/// The line under the box: how to send, or why one cannot.
fn footer<'a>(
    app: &'a App,
    main: &'a MainState,
    can_send: bool,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    if main.chat.current.channel_id().is_none() {
        return Space::new().into();
    }
    if !can_send {
        return row![
            icons::icon(Icon::Ban, widgets::ICON_MARK, tokens.text_muted),
            text("You cannot send messages here")
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_muted),
        ]
        .spacing(6)
        .align_y(Vertical::Center)
        .into();
    }

    row![
        widgets::key_hint("Enter", tokens),
        text("sends")
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
        widgets::key_hint("Shift+Enter", tokens),
        text("for a new line")
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<(i64, String)> {
        [(1, "Bruno"), (2, "ana"), (3, "Ana Maria"), (4, "anders")]
            .into_iter()
            .map(|(id, name)| (id, name.to_owned()))
            .collect()
    }

    #[test]
    fn an_excerpt_names_the_user_a_token_stands_for() {
        assert_eq!(excerpt("hi <@2> there", &users()), "hi @ana there");
    }

    #[test]
    fn a_prefix_matches_whatever_its_case_and_is_offered_by_name() {
        let users = users();
        let names: Vec<&str> = suggestions(&users, "AN")
            .into_iter()
            .map(|(_, name)| name)
            .collect();

        assert_eq!(names, ["ana", "Ana Maria", "anders"]);
        assert!(suggestions(&users, "zz").is_empty());
        assert_eq!(suggestions(&users, "").len(), 4);
    }

    #[test]
    fn the_mention_list_stops_at_eight() {
        let many: Vec<(i64, String)> = (0..12).map(|id| (id, format!("a{id:02}"))).collect();

        assert_eq!(suggestions(&many, "a").len(), SUGGESTIONS);
    }

    #[test]
    fn a_long_excerpt_is_cut_short() {
        let body = "x".repeat(80);
        let cut = excerpt(&body, &[]);

        assert_eq!(cut.chars().count(), EXCERPT_MAX + 1);
        assert!(cut.ends_with('…'));
    }
}
