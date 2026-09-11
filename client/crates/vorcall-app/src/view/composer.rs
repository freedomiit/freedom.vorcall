//! What is under the message list: the banners saying what the next send will
//! do, the mention list, the input itself and whatever is attached to it.

use iced::alignment::Vertical;
use iced::widget::{Id, button, column, container, row, text, text_input};
use iced::{Element, Length};
use vorcall_core::mentions::{self, Segment};
use vorcall_core::{Attachment, attachments};

use crate::app::{ChatState, Message, Status};
use crate::brand::palette::MUTED;

use super::INPUT_ID;

/// How many names the mention list offers before it stops being a shortcut.
const SUGGESTIONS: usize = 6;
/// How much of a quoted message the reply banner shows.
const EXCERPT_MAX: usize = 60;

pub(super) fn view(chat: &ChatState) -> Element<'_, Message> {
    let connected = matches!(chat.status, Status::Connected);
    let mut panel = column![].spacing(6).padding(12).width(Length::Fill);

    if let Some(banner) = reply_banner(chat) {
        panel = panel.push(banner);
    }
    if chat.composer.editing.is_some() {
        panel = panel.push(edit_banner());
    }
    if let Some(query) = &chat.mention_query
        && let Some(list) = mention_list(chat, query)
    {
        panel = panel.push(list);
    }

    let mut field = text_input("Message…", &chat.input)
        .id(Id::new(INPUT_ID))
        .on_submit(Message::Send)
        .padding(12)
        .width(Length::Fill);
    let mut attach = button(text("Attach")).padding(12);
    let mut send = button(text("Send")).padding(12);

    let held = chat.composer.attachments.len() + chat.composer.uploading;
    // An edit never empties a message, but it also never carries an attachment,
    // so it is the one case an empty-looking composer still has something to say.
    let empty = chat.input.trim().is_empty() && chat.composer.attachments.is_empty();
    if connected {
        field = field.on_input(Message::InputChanged);
        // An edit carries no attachments, so nothing may be picked for one.
        if held < attachments::MAX_PER_MESSAGE && chat.composer.editing.is_none() {
            attach = attach.on_press(Message::PickAttachment);
        }
        if !empty || chat.composer.editing.is_some() {
            send = send.on_press(Message::Send);
        }
    }

    panel = panel.push(
        row![field, attach, send]
            .spacing(8)
            .align_y(Vertical::Center),
    );

    if !chat.composer.attachments.is_empty() || chat.composer.uploading > 0 {
        panel = panel.push(strip(chat));
    }
    panel.into()
}

fn reply_banner(chat: &ChatState) -> Option<Element<'_, Message>> {
    let id = chat.composer.reply_to?;
    let label = match chat.current().and_then(|room| room.messages.get(&id)) {
        Some(message) => format!(
            "↩ Replying to {}: {}",
            message.author,
            excerpt(&message.text, chat.user_pairs())
        ),
        None => "Replying to a message".to_owned(),
    };
    Some(banner(label, Message::CancelReply))
}

fn edit_banner<'a>() -> Element<'a, Message> {
    banner("Editing your message".to_owned(), Message::CancelEdit)
}

fn banner<'a>(label: String, cancel: Message) -> Element<'a, Message> {
    row![
        text(label).size(12).color(MUTED).width(Length::Fill),
        button(text("Cancel").size(12))
            .padding([2, 6])
            .style(button::text)
            .on_press(cancel),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

fn mention_list<'a>(chat: &'a ChatState, query: &str) -> Option<Element<'a, Message>> {
    let names = suggestions(
        chat.users.values().map(|member| member.username.as_str()),
        query,
    );
    if names.is_empty() {
        return None;
    }

    Some(
        row(names.into_iter().map(|name| {
            button(text(name).size(12))
                .padding([2, 6])
                .style(button::secondary)
                .on_press(Message::MentionPick(name.to_owned()))
                .into()
        }))
        .spacing(4)
        .into(),
    )
}

fn strip(chat: &ChatState) -> Element<'_, Message> {
    let mut line = row![].spacing(6).align_y(Vertical::Center);
    for attachment in &chat.composer.attachments {
        line = line.push(pending(attachment));
    }
    for _ in 0..chat.composer.uploading {
        line = line.push(container(text("Uploading…").size(12).color(MUTED)).padding([2, 6]));
    }
    line.into()
}

fn pending<'a>(attachment: &Attachment) -> Element<'a, Message> {
    button(text(format!("{} ✕", attachment.file_name)).size(12))
        .padding([2, 6])
        .style(button::secondary)
        .on_press(Message::RemovePendingAttachment(attachment.id))
        .into()
}

/// The names the mention list offers for what has been typed after the `@`.
fn suggestions<'a>(usernames: impl IntoIterator<Item = &'a str>, query: &str) -> Vec<&'a str> {
    let query = query.to_lowercase();
    let mut names: Vec<&str> = usernames
        .into_iter()
        .filter(|name| name.to_lowercase().starts_with(&query))
        .collect();
    names.sort_by_cached_key(|name| name.to_lowercase());
    names.truncate(SUGGESTIONS);
    names
}

/// What a quoted message reads as in the banner: mentions as the names they
/// were typed as, rather than the `<@id>` tokens they are stored as.
fn excerpt(text: &str, users: &[(i64, String)]) -> String {
    let rendered: String = mentions::segments(text, users)
        .into_iter()
        .map(|segment| match segment {
            Segment::Text(body) => body,
            Segment::Mention { username, .. } => format!("@{username}"),
        })
        .collect();

    if rendered.chars().count() <= EXCERPT_MAX {
        return rendered;
    }
    rendered.chars().take(EXCERPT_MAX).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 4] = ["Bruno", "ana", "Ana Maria", "anders"];

    #[test]
    fn a_prefix_matches_whatever_its_case() {
        assert_eq!(suggestions(NAMES, "an"), vec!["ana", "Ana Maria", "anders"]);
        assert_eq!(suggestions(NAMES, "AN"), vec!["ana", "Ana Maria", "anders"]);
    }

    #[test]
    fn an_empty_query_offers_everyone_sorted() {
        assert_eq!(
            suggestions(NAMES, ""),
            vec!["ana", "Ana Maria", "anders", "Bruno"]
        );
    }

    #[test]
    fn nothing_matching_offers_nothing() {
        assert!(suggestions(NAMES, "zz").is_empty());
    }

    #[test]
    fn the_list_stops_at_six() {
        let many = ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"];
        assert_eq!(suggestions(many, "a").len(), SUGGESTIONS);
        assert_eq!(
            suggestions(many, "a"),
            vec!["a1", "a2", "a3", "a4", "a5", "a6"]
        );
    }

    #[test]
    fn an_excerpt_names_the_user_a_token_stands_for() {
        let users = vec![(7, "ana".to_owned())];
        assert_eq!(excerpt("hi <@7> there", &users), "hi @ana there");
    }

    #[test]
    fn a_long_excerpt_is_cut_short() {
        let body = "x".repeat(80);
        let cut = excerpt(&body, &[]);
        assert_eq!(cut.chars().count(), EXCERPT_MAX + 1);
        assert!(cut.ends_with('…'));
    }
}
