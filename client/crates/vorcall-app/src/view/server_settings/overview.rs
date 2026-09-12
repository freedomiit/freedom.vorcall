//! The Overview page: the server's name, description and icon, who owns it, and
//! what it is made of.

use std::fmt;

use iced::alignment::Vertical;
use iced::widget::{Column, Space, container, image, pick_list, row, text, text_input};
use iced::{Element, Length, Theme, border};
use vorcall_core::ChannelKind;

use crate::app::message::{AdminMsg, Message};
use crate::app::state::server::channel_kind;
use crate::app::state::settings::OverviewDraft;
use crate::app::update::admin;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, field, hint};
use crate::view::widgets;
use crate::view::{TEXT_BODY, TEXT_ROW, TEXT_SECONDARY};
use crate::workers::images::ImageKey;

/// How large the icon is drawn in the form.
const ICON: f32 = 64.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let draft = admin::overview_draft(main);

    let mut page = Column::new()
        .push(identity(app, main, &draft))
        .push(icon_card(app, main, draft.icon_image_id))
        .spacing(16)
        .width(Length::Fill);
    if main.server.is_owner() {
        page = page.push(ownership(app, main));
    }
    page.push(facts(app, main)).into()
}

/// The name and the description, with the Save that sends both.
fn identity<'a>(app: &'a App, main: &'a MainState, draft: &OverviewDraft) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let ready = !draft.name.trim().is_empty();

    card(
        "Identity",
        vec![
            field(
                "Name",
                text_input("Vorcall", &draft.name)
                    .on_input(|value| Message::Admin(AdminMsg::OverviewName(value)))
                    .on_submit(Message::Admin(AdminMsg::OverviewSave))
                    .padding(8)
                    .size(TEXT_BODY)
                    .style(styles::text_input(tokens))
                    .into(),
                tokens,
            ),
            field(
                "Description",
                text_input("What this server is for", &draft.description)
                    .on_input(|value| Message::Admin(AdminMsg::OverviewDescription(value)))
                    .padding(8)
                    .size(TEXT_BODY)
                    .style(styles::text_input(tokens))
                    .into(),
                tokens,
            ),
            hint(
                "1 to 32 characters for the name, up to 256 for the description.",
                tokens,
            ),
            row![
                action(
                    Kind::Primary,
                    "Save changes",
                    ready.then_some(Message::Admin(AdminMsg::OverviewSave)),
                    if ready {
                        ""
                    } else {
                        "A server name is required"
                    },
                    tokens,
                ),
                Space::new().width(Length::Fill),
                text(format!(
                    "Owned by {}",
                    main.server.display_name(main.server.server.owner_id)
                ))
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
            ]
            .align_y(Vertical::Center)
            .into(),
        ],
        tokens,
    )
}

/// The icon, the picker and the way to take it away again.
fn icon_card<'a>(app: &'a App, main: &'a MainState, icon_image_id: i64) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let picture = (icon_image_id != 0)
        .then(|| widgets::image_handle(&main.chat, ImageKey::Image(icon_image_id)))
        .flatten();

    card(
        "Icon",
        vec![
            row![
                preview(picture, tokens),
                Column::new()
                    .push(action(
                        Kind::Secondary,
                        "Change…",
                        Some(Message::Admin(AdminMsg::OverviewPickIcon)),
                        "",
                        tokens,
                    ))
                    .push(action(
                        Kind::Ghost,
                        "Remove",
                        (icon_image_id != 0).then(admin::clear_icon),
                        if icon_image_id == 0 {
                            "There is no icon to remove"
                        } else {
                            ""
                        },
                        tokens,
                    ))
                    .spacing(6),
            ]
            .spacing(12)
            .align_y(Vertical::Center)
            .into(),
            hint(
                "Scaled to 512 pixels on this machine before it is uploaded; Save is what puts it on the server.",
                tokens,
            ),
        ],
        tokens,
    )
}

/// The icon as it stands, or the square that says there is none.
fn preview<'a>(
    picture: Option<iced::widget::image::Handle>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let inner: Element<'_, Message> = match picture {
        Some(handle) => image(handle).width(ICON).height(ICON).into(),
        None => icons::icon(Icon::Image, ICON / 2.0, tokens.text_muted),
    };
    let border_color = tokens.border_subtle;
    container(inner)
        .center(ICON)
        .style(move |_theme: &Theme| container::Style {
            border: border::rounded(styles::RADIUS_CARD)
                .width(1.0)
                .color(border_color),
            ..container::Style::default()
        })
        .into()
}

/// Handing the server over, which only its owner may do.
fn ownership<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let candidates: Vec<Member> = main
        .server
        .members
        .values()
        .filter(|member| member.user_id != main.server.server.owner_id)
        .map(|member| Member {
            id: member.user_id,
            name: main.server.display_name(member.user_id).to_owned(),
        })
        .collect();

    let control: Element<'_, Message> = if candidates.is_empty() {
        hint("There is nobody else to hand it to.", tokens)
    } else {
        pick_list(candidates, None::<Member>, |member| {
            Message::Admin(AdminMsg::TransferOwnership(member.id))
        })
        .placeholder("Transfer ownership to…")
        .text_size(TEXT_ROW)
        .padding([6.0, 10.0])
        .style(styles::pick_list(tokens))
        .menu_style(styles::menu(tokens))
        .into()
    };

    card(
        "Ownership",
        vec![
            row![
                icons::icon(Icon::Crown, 16.0, tokens.accent),
                text("You own this server.")
                    .size(TEXT_BODY)
                    .color(tokens.text_primary),
            ]
            .spacing(8)
            .align_y(Vertical::Center)
            .into(),
            control,
            text("Handing it over cannot be undone: the owner bypasses every permission check, and after this you will not.")
                .size(TEXT_SECONDARY)
                .color(tokens.danger)
                .into(),
        ],
        tokens,
    )
}

/// What the server is made of, which is the page's own reason to exist.
fn facts<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let channels = main
        .server
        .channels
        .values()
        .filter(|channel| channel_kind(channel) != ChannelKind::Dm)
        .count();
    let general = main.server.server.general_channel_id;

    card(
        "This server",
        vec![
            fact("Channels", &channels.to_string(), tokens),
            fact(
                "Categories",
                &main.server.categories.len().to_string(),
                tokens,
            ),
            fact("Roles", &main.server.roles.len().to_string(), tokens),
            fact("Members", &main.server.members.len().to_string(), tokens),
            fact(
                "Default channel",
                &main.server.channel_title(general),
                tokens,
            ),
        ],
        tokens,
    )
}

fn fact<'a>(label: &str, value: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    row![
        text(label.to_owned())
            .size(TEXT_ROW)
            .color(tokens.text_secondary),
        Space::new().width(Length::Fill),
        text(value.to_owned())
            .size(TEXT_ROW)
            .color(tokens.text_primary),
    ]
    .align_y(Vertical::Center)
    .into()
}

/// A pick-list entry: the list shows the name and hands back the id.
#[derive(Clone, PartialEq, Eq)]
struct Member {
    id: i64,
    name: String,
}

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}
