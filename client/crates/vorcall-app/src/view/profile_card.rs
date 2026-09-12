//! The profile card, opened by clicking a name anywhere.
//!
//! The avatar overlaps the banner, which iced has no margin for: the card is two
//! stacked layers, and the body leaves the avatar's lower half room.

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, image, mouse_area, row, stack, text};
use iced::{Background, Color, Element, Length, Padding, Size, Theme, border};
use vorcall_core::Role;

use crate::app::message::{ChannelsMsg, Message, UiMsg};
use crate::app::{App, MainState};
use crate::theme::styles;
use crate::view::context_menu::anchored;
use crate::view::widgets::{self, color_of};
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, bold};
use crate::workers::images::ImageKey;

const CARD_WIDTH: f32 = 300.0;
const BANNER_HEIGHT: f32 = 100.0;
const CARD_AVATAR: f32 = 80.0;
/// Where the avatar sits, and how far the body starts below the banner so the
/// half of it that hangs over has room.
const AVATAR_LEFT: f32 = 14.0;
const BODY_TOP: f32 = CARD_AVATAR / 2.0 + 8.0;
/// What the card is assumed to cost in height while it is being placed.
const CARD_HEIGHT: f32 = 320.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let Some(card) = app.ui.profile_card else {
        return Space::new().into();
    };
    let tokens = &app.tokens;
    let user_id = card.user_id;
    let profile = main.server.members.get(&user_id);
    let me = user_id == main.member_id;
    let owner = user_id != 0 && user_id == main.server.server.owner_id;

    let mut head = row![
        text(main.server.display_name(user_id).to_owned())
            .size(TEXT_SECTION)
            .font(bold())
            .color(name_color(app, main, user_id)),
        widgets::presence_dot(main.server.is_online(user_id), tokens),
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if owner {
        head = head.push(widgets::owner_crown(tokens));
        head = head.push(text("Owner").size(TEXT_BADGE).color(tokens.warning));
    }

    let mut body = column![
        head,
        text(match profile {
            Some(profile) => format!("@{}", profile.username),
            None => "@unknown".to_owned(),
        })
        .size(TEXT_SECONDARY)
        .color(tokens.text_muted),
    ]
    .spacing(6)
    .width(Length::Fill);

    let description = profile
        .map(|profile| profile.description.clone())
        .unwrap_or_default();
    if !description.is_empty() {
        body = body.push(widgets::section_label("About", tokens));
        body = body.push(
            text(description)
                .size(TEXT_ROW)
                .color(tokens.text_secondary)
                .width(Length::Fill),
        );
    }

    let roles = roles_of(main, user_id);
    if !roles.is_empty() {
        body = body.push(widgets::section_label("Roles", tokens));
        let mut chips = row![].spacing(4);
        for role in roles {
            chips = chips.push(widgets::role_chip(role, tokens));
        }
        body = body.push(chips);
    }

    let mut open = button(text("Message").size(TEXT_BODY))
        .width(Length::Fill)
        .padding([6.0, 14.0])
        .style(styles::button::secondary(tokens));
    // There is no conversation with oneself.
    if !me {
        open = open.on_press(Message::Channels(ChannelsMsg::OpenDm(user_id)));
    }
    body = body.push(open);

    let card_body = container(body).padding(Padding {
        top: BODY_TOP,
        right: 14.0,
        bottom: 14.0,
        left: 14.0,
    });

    let picture = stack![
        column![banner(app, main, user_id), card_body],
        container(widgets::member_avatar(main, user_id, CARD_AVATAR, tokens)).padding(Padding {
            top: BANNER_HEIGHT - CARD_AVATAR / 2.0,
            right: 0.0,
            bottom: 0.0,
            left: AVATAR_LEFT,
        }),
    ];

    let shell = container(picture)
        .width(CARD_WIDTH)
        .style(styles::container::popover(tokens));
    // The card takes its own presses, so only what is outside it dismisses it.
    let held = mouse_area(shell).on_press(Message::Noop);

    let dismiss = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::Ui(UiMsg::CloseProfileCard));
    let at = app
        .ui
        .clamp_popover(card.at, Size::new(CARD_WIDTH, CARD_HEIGHT));

    stack![dismiss, anchored(held.into(), at)]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The banner: their own image, else a block of their accent.
fn banner<'a>(app: &'a App, main: &'a MainState, user_id: i64) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let profile = main.server.members.get(&user_id);
    let handle = profile
        .map(|profile| profile.banner_image_id)
        .filter(|id| *id != 0)
        .and_then(|id| widgets::image_handle(&main.chat, ImageKey::Image(id)));

    if let Some(handle) = handle {
        return image(handle)
            .width(CARD_WIDTH)
            .height(BANNER_HEIGHT)
            .content_fit(iced::ContentFit::Cover)
            .border_radius(border::top(styles::RADIUS_POPOVER))
            .into();
    }

    let accent = match profile.map(|profile| profile.accent_color) {
        Some(rgb) if rgb != 0 => color_of(rgb),
        _ => tokens.accent,
    };
    // Only the top corners: the card's own lower corners are rounded by the
    // popover under it, which does not clip what is drawn over it.
    container(Space::new())
        .width(CARD_WIDTH)
        .height(BANNER_HEIGHT)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(accent)),
            border: border::rounded(border::top(styles::RADIUS_POPOVER)),
            ..container::Style::default()
        })
        .into()
}

/// The colour a member's name is painted in on the card.
fn name_color(app: &App, main: &MainState, user_id: i64) -> Color {
    match main.server.member_color(user_id) {
        0 => app.tokens.text_primary,
        rgb => color_of(rgb),
    }
}

/// Their roles, highest first, the implicit everyone role left out.
fn roles_of(main: &MainState, user_id: i64) -> Vec<&Role> {
    let Some(profile) = main.server.members.get(&user_id) else {
        return Vec::new();
    };
    let mut roles: Vec<&Role> = profile
        .role_ids
        .iter()
        .filter_map(|id| main.server.roles.get(id))
        .filter(|role| !role.everyone)
        .collect();
    roles.sort_by_key(|role| std::cmp::Reverse((role.position, role.id)));
    roles
}
