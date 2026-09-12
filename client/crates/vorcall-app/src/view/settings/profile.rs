//! The Profile page: the form on the left, a live preview of the card other
//! people see on the right.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::image::Handle;
use iced::widget::{Space, button, column, container, image, row, text, text_input};
use iced::{Background, Color, Element, Length, Theme, border};
use vorcall_core::{Role, permissions};

use crate::app::message::{Message, SettingsMsg};
use crate::app::state::chat::ImageState;
use crate::app::state::settings::ProfileDraft;
use crate::app::{App, MainState};
use crate::theme::{ThemeTokens, styles, tokens};
use crate::view::settings::{counter, field, section};
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, bold};
use crate::workers::images::ImageKey;

/// How long the two profile fields may be, which is what the counters count to.
const NICKNAME_MAX: usize = 32;
const DESCRIPTION_MAX: usize = 256;

/// The preview card, its banner and the avatar over the banner's edge.
const CARD_WIDTH: f32 = 300.0;
const CARD_BANNER: f32 = 80.0;
const CARD_AVATAR: f32 = 64.0;
/// The avatar and the banner the form itself shows.
const FORM_AVATAR: f32 = 96.0;
const FORM_BANNER: f32 = 120.0;
/// One accent swatch.
const SWATCH: f32 = 28.0;

/// The accents the page offers with one press; anything else is typed as a hex.
const SWATCHES: [u32; 8] = [
    0x00_C8_10_2E,
    0x00_F2_B8_47,
    0x00_5C_C7_75,
    0x00_3F_7D_52,
    0x00_4F_A3_FF,
    0x00_8E_7C_FF,
    0x00_EB_5E_5E,
    0x00_A0_93_93,
];

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    row![form(app, main), preview(app, main)]
        .spacing(24)
        .width(Length::Fill)
        .into()
}

fn form<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.settings.profile;
    let may_rename = main.server.can(permissions::CHANGE_NICKNAME, None);

    let mut nickname = text_input("Your username", &draft.nickname)
        .padding(10)
        .width(Length::Fill)
        .style(styles::text_input(tokens));
    if may_rename {
        nickname =
            nickname.on_input(|value| Message::Settings(SettingsMsg::ProfileNickname(value)));
    }

    let description = text_input("A line about you", &draft.description)
        .padding(10)
        .width(Length::Fill)
        .style(styles::text_input(tokens))
        .on_input(|value| Message::Settings(SettingsMsg::ProfileDescription(value)));

    let dirty = main
        .server
        .members
        .get(&main.member_id)
        .is_some_and(|profile| draft.differs_from(profile));

    let actions = row![
        Space::new().width(Length::Fill),
        button(text("Reset").size(TEXT_ROW))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press_maybe(dirty.then_some(Message::Settings(SettingsMsg::ProfileReset))),
        button(text("Save changes").size(TEXT_ROW))
            .padding([6.0, 14.0])
            .style(styles::button::primary(tokens))
            .on_press_maybe(dirty.then_some(Message::Settings(SettingsMsg::ProfileSave))),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    column![
        section(
            "Pictures",
            tokens,
            vec![
                field(
                    "Banner",
                    banner_picker(app, main),
                    Some("Up to 1600×600. Larger images are resized before upload."),
                    tokens,
                ),
                field("Avatar", avatar_picker(app, main), None, tokens),
            ],
        ),
        section(
            "About you",
            tokens,
            vec![
                field(
                    "Nickname",
                    column![
                        nickname,
                        counter(draft.nickname.chars().count(), NICKNAME_MAX, tokens)
                    ]
                    .spacing(4),
                    Some(if may_rename {
                        "Shown instead of your username everywhere."
                    } else {
                        "Changing your nickname needs the Change nickname permission."
                    }),
                    tokens,
                ),
                field(
                    "About me",
                    column![
                        description,
                        counter(draft.description.chars().count(), DESCRIPTION_MAX, tokens),
                    ]
                    .spacing(4),
                    None,
                    tokens,
                ),
                field("Accent colour", accent(app, main), None, tokens),
            ],
        ),
        actions,
    ]
    .spacing(24)
    .width(Length::Fill)
    .into()
}

/// The banner as it stands, with the two things that can be done to it.
fn banner_picker<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.settings.profile;

    let ground: Element<'_, Message> = match picture(main, draft.banner_image_id) {
        Some(handle) => image(handle).width(Length::Fill).height(FORM_BANNER).into(),
        None => container(Space::new())
            .width(Length::Fill)
            .height(FORM_BANNER)
            .style(block(accent_of(draft, tokens.accent), styles::RADIUS_CARD))
            .into(),
    };

    column![
        ground,
        row![
            picker_button(
                "Change banner",
                Message::Settings(SettingsMsg::ProfilePickBanner),
                tokens,
            ),
            clear_button(
                Message::Settings(SettingsMsg::ProfileClearBanner),
                draft.banner_image_id != 0,
                tokens,
            ),
        ]
        .spacing(8),
    ]
    .spacing(8)
    .width(Length::Fill)
    .into()
}

fn avatar_picker<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.settings.profile;
    let profile = main.server.members.get(&main.member_id);

    row![
        widgets::avatar(
            profile,
            FORM_AVATAR,
            tokens,
            picture(main, draft.avatar_image_id)
        ),
        column![
            picker_button(
                "Change avatar",
                Message::Settings(SettingsMsg::ProfilePickAvatar),
                tokens,
            ),
            clear_button(
                Message::Settings(SettingsMsg::ProfileClearAvatar),
                draft.avatar_image_id != 0,
                tokens,
            ),
        ]
        .spacing(8),
    ]
    .spacing(16)
    .align_y(Vertical::Center)
    .into()
}

fn picker_button<'a>(
    label: &str,
    message: Message,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    button(text(label.to_owned()).size(TEXT_ROW))
        .padding([6.0, 14.0])
        .style(styles::button::secondary(tokens))
        .on_press(message)
        .into()
}

/// Removing a picture that is not there is nothing to offer.
fn clear_button<'a>(
    message: Message,
    enabled: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    button(text("Remove").size(TEXT_ROW))
        .padding([6.0, 14.0])
        .style(styles::button::ghost(tokens))
        .on_press_maybe(enabled.then_some(message))
        .into()
}

/// The accent: the eight the page offers, the one that means "none", and the hex
/// for anything else.
///
/// The field shows the colour itself, so a half-typed hex snaps back: the draft
/// holds a colour, not the text of one.
fn accent<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let chosen = main.settings.profile.accent_color;

    let typed = if chosen == 0 {
        String::new()
    } else {
        tokens::to_hex(widgets::color_of(chosen))
    };
    let hex = text_input("#RRGGBB", &typed)
        .padding(10)
        .width(120.0)
        .style(styles::text_input(tokens))
        .on_input(move |value| {
            Message::Settings(SettingsMsg::ProfileAccent(
                parse_accent(&value).unwrap_or(chosen),
            ))
        });

    // "No accent of my own", which falls back to the theme's.
    let none = button(text("None").size(TEXT_BADGE))
        .padding([4.0, 8.0])
        .on_press(Message::Settings(SettingsMsg::ProfileAccent(0)));
    let none: Element<'_, Message> = if chosen == 0 {
        none.style(styles::button::primary(tokens)).into()
    } else {
        none.style(styles::button::secondary(tokens)).into()
    };

    let mut swatches = row![none].spacing(8).align_y(Vertical::Center);
    for rgb in SWATCHES {
        swatches = swatches.push(
            button(Space::new().width(SWATCH).height(SWATCH))
                .padding(0.0)
                .style(swatch(
                    widgets::color_of(rgb),
                    rgb == chosen,
                    tokens.text_primary,
                ))
                .on_press(Message::Settings(SettingsMsg::ProfileAccent(rgb))),
        );
    }

    column![swatches, hex].spacing(8).into()
}

/// `#RRGGBB` as the wire spells a colour, with the `#` optional because a typed
/// field rarely starts with one.
fn parse_accent(raw: &str) -> Option<u32> {
    let body = raw.trim().trim_start_matches('#');
    if body.len() != 6 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(body, 16).ok()
}

/// The card other people see, drawn from the draft rather than from the server:
/// this is what Save would publish.
fn preview<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.settings.profile;
    let profile = main.server.members.get(&main.member_id);
    let accent = accent_of(draft, tokens.accent);

    let banner: Element<'_, Message> = match picture(main, draft.banner_image_id) {
        Some(handle) => image(handle).width(Length::Fill).height(CARD_BANNER).into(),
        None => container(Space::new())
            .width(Length::Fill)
            .height(CARD_BANNER)
            .style(block(accent, 0.0))
            .into(),
    };

    let name_color = match main.server.member_color(main.member_id) {
        0 => tokens.text_primary,
        rgb => widgets::color_of(rgb),
    };
    let mut body = column![
        widgets::avatar(
            profile,
            CARD_AVATAR,
            tokens,
            picture(main, draft.avatar_image_id),
        ),
        text(display_name(main, draft))
            .size(TEXT_SECTION)
            .font(bold())
            .color(name_color),
        text(format!("@{}", app.username()))
            .size(TEXT_SECONDARY)
            .color(tokens.text_secondary),
    ]
    .spacing(6);

    if !draft.description.trim().is_empty() {
        body = body.push(
            text(draft.description.clone())
                .size(TEXT_SECONDARY)
                .color(tokens.text_secondary),
        );
    }

    // The roles are the server's, not the draft's: nothing on this page changes
    // them.
    let roles: Vec<&Role> = profile
        .map(|profile| {
            let mut roles: Vec<&Role> = profile
                .role_ids
                .iter()
                .filter_map(|id| main.server.roles.get(id))
                .collect();
            roles.sort_by_key(|role| std::cmp::Reverse((role.position, role.id)));
            roles
        })
        .unwrap_or_default();
    if !roles.is_empty() {
        let mut chips = row![].spacing(4);
        for role in roles {
            chips = chips.push(widgets::role_chip(role, tokens));
        }
        body = body.push(chips);
    }

    column![
        widgets::section_label("Preview", tokens),
        container(column![banner, container(body).padding([12.0, 14.0])])
            .width(CARD_WIDTH)
            .style(styles::container::popover(tokens)),
    ]
    .spacing(8)
    .align_x(Horizontal::Left)
    .into()
}

/// The name the preview shows: what is being typed, falling back to the username.
fn display_name(main: &MainState, draft: &ProfileDraft) -> String {
    if draft.nickname.trim().is_empty() {
        main.server
            .members
            .get(&main.member_id)
            .map(|profile| profile.username.clone())
            .unwrap_or_default()
    } else {
        draft.nickname.clone()
    }
}

/// The accent the draft would publish, or the theme's where it has none.
fn accent_of(draft: &ProfileDraft, fallback: Color) -> Color {
    match draft.accent_color {
        0 => fallback,
        rgb => widgets::color_of(rgb),
    }
}

/// One of the pictures, if its bytes have already been fetched and decoded.
fn picture(main: &MainState, id: i64) -> Option<Handle> {
    if id == 0 {
        return None;
    }
    match main.chat.images.get(&ImageKey::Image(id)) {
        Some(ImageState::Ready(handle)) => Some(handle.clone()),
        _ => None,
    }
}

/// A flat block of colour, for a banner nobody has uploaded yet.
fn block(color: Color, radius: f32) -> impl Fn(&Theme) -> iced::widget::container::Style {
    move |_theme| iced::widget::container::Style {
        background: Some(Background::Color(color)),
        border: border::rounded(radius),
        ..iced::widget::container::Style::default()
    }
}

/// One accent swatch: the colour itself, ringed while it is the one in use.
fn swatch(
    color: Color,
    selected: bool,
    ring: Color,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    move |_theme, _status| iced::widget::button::Style {
        background: Some(Background::Color(color)),
        border: border::rounded(SWATCH / 2.0).width(2.0).color(if selected {
            ring
        } else {
            Color::TRANSPARENT
        }),
        ..iced::widget::button::Style::default()
    }
}
