//! One message: what it says, what it quotes, what is attached to it, the
//! reactions under it and the actions that appear while the pointer is on it.

use std::sync::LazyLock;

use iced::alignment::Vertical;
use iced::widget::text::Span;
use iced::widget::{button, column, container, image, mouse_area, rich_text, row, span, text};
use iced::{Color, ContentFit, Element, Font, Length, border, font};
use vorcall_core::mentions::{self, PALETTE, Segment};
use vorcall_core::{Attachment, ChatMessage, Reaction, ReplyRef};

use crate::app::{ChatState, ImageState, Message};
use crate::brand::palette::{DEEP, INK, MUTED};

use super::{bold, format_time};

/// The largest an attachment is drawn at in the list; the dialog is what shows
/// the rest of it.
const ATTACHMENT_WIDTH: f32 = 320.0;
const ATTACHMENT_HEIGHT: f32 = 240.0;
/// A mention reads as a chip: the accent behind it, at a weight that leaves the
/// name on top of it legible.
const MENTION_TINT: Color = Color { a: 0.22, ..DEEP };

/// Whether emoji are replaced by their ASCII labels. Read once: this is about
/// the fonts on the machine, which do not change while the window is open.
static TEXT_REACTIONS: LazyLock<bool> =
    LazyLock::new(|| std::env::var("VORCALL_TEXT_REACTIONS").is_ok_and(|value| value == "1"));

pub(super) fn view<'a>(chat: &'a ChatState, message: &'a ChatMessage) -> Element<'a, Message> {
    let id = message.id;
    let mut body = column![heading(message)].spacing(4).width(Length::Fill);

    if message.deleted {
        body = body.push(text("Message deleted").font(italic()).color(MUTED));
        return hoverable(id, body.into());
    }

    if let Some(reply) = &message.reply_to {
        body = body.push(quote(reply));
    }
    body = body.push(body_text(chat, message));
    for attachment in &message.attachments {
        body = body.push(attachment_view(chat, attachment));
    }
    if !message.reactions.is_empty() {
        body = body.push(reactions(chat, message));
    }
    if chat.reacting == Some(id) {
        body = body.push(palette_row(id));
    }

    let mut line = row![body].spacing(8);
    if chat.hovered == Some(id) || chat.reacting == Some(id) || chat.confirm_delete == Some(id) {
        line = line.push(actions(chat, message));
    }

    hoverable(id, line.into())
}

/// The pointer is what decides whether a row shows its actions, so every row
/// reports its own comings and goings.
fn hoverable(id: i64, content: Element<'_, Message>) -> Element<'_, Message> {
    mouse_area(content)
        .on_enter(Message::Hover(id))
        .on_exit(Message::Unhover(id))
        .into()
}

fn heading(message: &ChatMessage) -> Element<'_, Message> {
    let mut line = row![
        text(format_time(message.sent_at_unix_ms)).color(MUTED),
        text(message.author.as_str()).font(bold()),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    if !message.deleted && message.edited_at_unix_ms > 0 {
        line = line.push(text("(edited)").size(11).color(MUTED));
    }
    line.into()
}

fn quote<'a>(reply: &ReplyRef) -> Element<'a, Message> {
    let label = match reply.deleted {
        true => "↩ deleted message".to_owned(),
        false => format!("↩ {}: {}", reply.author, reply.excerpt),
    };
    text(label).size(12).color(MUTED).into()
}

fn body_text<'a>(chat: &ChatState, message: &ChatMessage) -> Element<'a, Message> {
    let spans: Vec<Span<'a>> = mentions::segments(&message.text, chat.user_pairs())
        .into_iter()
        .map(|segment| match segment {
            Segment::Text(body) => span(body),
            Segment::Mention { user_id, username } => span(format!("@{username}"))
                .color(match user_id == chat.member_id {
                    true => DEEP,
                    false => INK,
                })
                .background(MENTION_TINT)
                .border(border::rounded(4))
                .padding([0, 3]),
        })
        .collect();

    rich_text(spans).width(Length::Fill).into()
}

fn attachment_view<'a>(chat: &ChatState, attachment: &Attachment) -> Element<'a, Message> {
    let id = attachment.id;
    let handle = match chat.images.get(&id) {
        Some(ImageState::Ready(handle)) => handle,
        Some(ImageState::Failed) => return placeholder("Image unavailable"),
        Some(ImageState::Loading) | None => return placeholder("Loading image…"),
    };

    button(
        container(image(handle.clone()).content_fit(ContentFit::Contain))
            .max_width(ATTACHMENT_WIDTH)
            .max_height(ATTACHMENT_HEIGHT),
    )
    .padding(0)
    .style(button::text)
    .on_press(Message::OpenImage(id))
    .into()
}

fn placeholder<'a>(label: &'a str) -> Element<'a, Message> {
    container(text(label).size(12).color(MUTED))
        .padding(24)
        .style(container::bordered_box)
        .into()
}

fn reactions<'a>(chat: &ChatState, message: &ChatMessage) -> Element<'a, Message> {
    let id = message.id;
    row(message
        .reactions
        .iter()
        .map(|reaction| chip(chat, id, reaction)))
    .spacing(4)
    .into()
}

fn chip<'a>(chat: &ChatState, message_id: i64, reaction: &Reaction) -> Element<'a, Message> {
    let mine = reaction.user_ids.contains(&chat.member_id);
    let label = format!(
        "{} {}",
        reaction_text(&reaction.emoji),
        reaction.user_ids.len()
    );

    let mut chip = button(text(label).size(12))
        .padding([2, 6])
        .style(match mine {
            true => button::primary,
            false => button::secondary,
        });
    // Only the fixed palette goes back to the server, so a reaction from
    // anywhere else is shown and left alone.
    if let Some(emoji) = palette_emoji(&reaction.emoji) {
        chip = chip.on_press(Message::React(message_id, emoji));
    }
    chip.into()
}

fn palette_row<'a>(message_id: i64) -> Element<'a, Message> {
    row(PALETTE.into_iter().map(|emoji| {
        button(text(reaction_text(emoji)).size(14))
            .padding([2, 6])
            .style(button::secondary)
            .on_press(Message::React(message_id, emoji))
            .into()
    }))
    .spacing(4)
    .into()
}

fn actions<'a>(chat: &ChatState, message: &ChatMessage) -> Element<'a, Message> {
    let id = message.id;
    let mut strip = row![
        action("Reply", Message::ReplyTo(id)),
        action("React", Message::OpenReactions(Some(id))),
    ]
    .spacing(4)
    .align_y(Vertical::Center);

    if message.author_id == chat.member_id {
        strip = strip.push(action("Edit", Message::StartEdit(id)));
        // The first press arms the delete; the label is what says so.
        strip = strip.push(match chat.confirm_delete == Some(id) {
            true => action("Delete?", Message::ConfirmDelete(id)),
            false => action("Delete", Message::Delete(id)),
        });
    }
    strip.into()
}

fn action<'a>(label: &'a str, message: Message) -> Element<'a, Message> {
    button(text(label).size(12))
        .padding([2, 6])
        .style(button::text)
        .on_press(message)
        .into()
}

fn reaction_text(emoji: &str) -> &str {
    reaction_glyph(emoji, *TEXT_REACTIONS)
}

/// The emoji itself, or its ASCII label where emoji do not render.
fn reaction_glyph(emoji: &str, as_text: bool) -> &str {
    match as_text {
        true => mentions::reaction_label(emoji),
        false => emoji,
    }
}

/// The palette entry a stored reaction is, which is the only form the server
/// takes back.
fn palette_emoji(emoji: &str) -> Option<&'static str> {
    PALETTE.into_iter().find(|entry| *entry == emoji)
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

    #[test]
    fn the_emoji_is_drawn_unless_labels_are_asked_for() {
        assert_eq!(reaction_glyph("👍", false), "👍");
        assert_eq!(reaction_glyph("👍", true), "+1");
    }

    #[test]
    fn a_reaction_outside_the_palette_keeps_its_own_glyph() {
        assert_eq!(reaction_glyph("🐧", false), "🐧");
        assert_eq!(reaction_glyph("🐧", true), "🐧");
    }

    #[test]
    fn a_palette_reaction_is_clickable() {
        assert_eq!(palette_emoji("🎉"), Some("🎉"));
        assert_eq!(palette_emoji("❤️"), Some("❤️"));
    }

    #[test]
    fn a_reaction_from_elsewhere_is_not() {
        assert_eq!(palette_emoji("🐧"), None);
        assert_eq!(palette_emoji(""), None);
    }
}
