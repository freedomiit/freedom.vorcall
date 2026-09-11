//! The pane on the left: the public rooms this account belongs to, its open
//! conversations, and the rooms it could join but has not.

use iced::alignment::Vertical;
use iced::widget::{Button, button, column, container, row, scrollable, text};
use iced::{Color, Element, Length, Theme, border};

use crate::app::{ChatState, Message, RoomUi};
use crate::brand::palette::{DANGER, INK, MUTED};

use super::bold;

pub(super) const PANE_WIDTH: f32 = 200.0;
/// Above this a counter stops being a number the pane has room for.
const BADGE_MAX: u32 = 99;

pub(super) fn pane(chat: &ChatState) -> Element<'_, Message> {
    let mut list = column![heading()].spacing(4).width(Length::Fill);

    for room in chat.joined_rooms() {
        list = list.push(room_row(chat, room));
    }

    list = list.push(section("Direct messages"));
    for room in chat.dms() {
        list = list.push(dm_row(chat, room));
    }

    let mut browsable = chat.browsable().peekable();
    if browsable.peek().is_some() {
        list = list.push(section("Browse"));
        for room in browsable {
            list = list.push(browse_row(chat, room));
        }
    }

    container(scrollable(list).height(Length::Fill))
        .width(PANE_WIDTH)
        .height(Length::Fill)
        .padding(12)
        .into()
}

fn heading<'a>() -> Element<'a, Message> {
    row![
        text("Rooms").font(bold()).width(Length::Fill),
        button(text("+"))
            .padding([0, 6])
            .style(button::text)
            .on_press(Message::OpenNewRoom),
    ]
    .spacing(4)
    .align_y(Vertical::Center)
    .into()
}

fn section<'a>(label: &'a str) -> Element<'a, Message> {
    text(label).size(12).color(MUTED).font(bold()).into()
}

fn room_row<'a>(chat: &ChatState, room: &RoomUi) -> Element<'a, Message> {
    open_button(chat, room).into()
}

/// A conversation can also be put away, which a room cannot.
fn dm_row<'a>(chat: &ChatState, room: &RoomUi) -> Element<'a, Message> {
    let room_id = room.room.room_id.clone();
    row![
        open_button(chat, room),
        button(text("✕").size(12))
            .padding([0, 4])
            .style(button::text)
            .on_press(Message::CloseDm(room_id)),
    ]
    .spacing(2)
    .align_y(Vertical::Center)
    .into()
}

fn browse_row<'a>(chat: &ChatState, room: &RoomUi) -> Element<'a, Message> {
    let room_id = room.room.room_id.clone();
    row![
        text(room.title(&chat.users, chat.member_id))
            .color(MUTED)
            .width(Length::Fill),
        button(text("Join").size(12))
            .padding([2, 6])
            .on_press(Message::JoinRoom(room_id)),
    ]
    .spacing(4)
    .align_y(Vertical::Center)
    .into()
}

/// The row that puts a room in view, with whatever it is holding for this
/// reader on the right.
fn open_button<'a>(chat: &ChatState, room: &RoomUi) -> Button<'a, Message> {
    let room_id = room.room.room_id.clone();
    let current = room_id == chat.current_room;

    let mut line = row![text(room.title(&chat.users, chat.member_id)).width(Length::Fill)]
        .spacing(4)
        .align_y(Vertical::Center);

    if room.unread > 0 {
        line = line.push(badge(room.unread, MUTED.scale_alpha(0.35)));
    }
    if room.mentions > 0 {
        line = line.push(badge(room.mentions, DANGER));
    }

    button(line)
        .width(Length::Fill)
        .padding([4, 8])
        .style(match current {
            true => button::primary,
            false => button::text,
        })
        .on_press(Message::SelectRoom(room_id))
}

fn badge<'a>(count: u32, background: Color) -> Element<'a, Message> {
    container(text(badge_label(count)).size(11).color(INK))
        .padding([1, 5])
        .style(move |_: &Theme| container::Style {
            background: Some(background.into()),
            border: border::rounded(8),
            ..container::Style::default()
        })
        .into()
}

fn badge_label(count: u32) -> String {
    match count > BADGE_MAX {
        true => format!("{BADGE_MAX}+"),
        false => count.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_count_is_its_own_digits() {
        assert_eq!(badge_label(1), "1");
        assert_eq!(badge_label(99), "99");
    }

    #[test]
    fn a_count_past_the_cap_is_abbreviated() {
        assert_eq!(badge_label(100), "99+");
        assert_eq!(badge_label(4321), "99+");
    }
}
