//! The Roles page: the hierarchy on the left, one role's editor on the right.
//!
//! `PROTOCOL.md` § Hierarchy is what greys things out here: a role at or above
//! the reader's own highest cannot be touched, and a bit the reader does not hold
//! cannot be handed out. `@everyone` is the other exception — only its
//! permissions are editable, whoever asks.

use iced::alignment::Vertical;
use iced::widget::{Column, Row, Space, button, container, row, text, text_input, toggler};
use iced::{Background, Element, Length, Theme, border};
use vorcall_core::{Role, permissions};

use crate::app::message::{AdminMsg, DragItem, DragSlot, Message, RoleIconDraft, UiMsg};
use crate::app::state::settings::RoleDraft;
use crate::app::state::ui::Dialog;
use crate::app::update::{admin, drag};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{
    Kind, action, arrows, card, drag_banner, drop_slot, field, grip, hint,
};
use crate::view::widgets::{self, color_of};
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY};
use crate::workers::images::ImageKey;

/// How wide the hierarchy is.
const LIST_WIDTH: f32 = 260.0;
/// One colour square in the picker, and the dot beside a role's name.
const SWATCH: f32 = 18.0;
const DOT: f32 = 10.0;

/// The colours the picker offers. A role may also carry none at all, which is
/// what `0` means on the wire.
const PALETTE: [u32; 12] = [
    0x00_C8_10_2E,
    0x00_E0_53_3D,
    0x00_E8_A3_3D,
    0x00_F2_D2_4B,
    0x00_5C_C7_75,
    0x00_3D_BF_A0,
    0x00_3D_A5_E8,
    0x00_4C_6F_E0,
    0x00_7C_5C_E0,
    0x00_B0_5C_E0,
    0x00_E0_5C_9E,
    0x00_9A_A3_AE,
];

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    row![hierarchy(app, main), editor(app, main)]
        .spacing(16)
        .width(Length::Fill)
        .into()
}

/// Every role, highest first, with `@everyone` pinned under them all.
fn hierarchy<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let slot = app.ui.drag.and_then(|drag| drag.slot);
    let moving = matches!(app.ui.drag.map(|drag| drag.item), Some(DragItem::Role(_)));
    let order = drag::role_ids(&main.server);
    let full = main.server.roles.len() >= admin::MAX_ROLES;

    let mut list = Column::new().spacing(1).width(Length::Fill);
    for (index, id) in order.iter().enumerate() {
        let Some(role) = main.server.roles.get(id) else {
            continue;
        };
        if moving {
            let before = DragSlot::BeforeRole(*id);
            list = list.push(drop_slot(before, slot == Some(before), 0.0, tokens));
        }
        list = list.push(role_row(
            app,
            main,
            role,
            index > 0,
            index + 1 < order.len(),
        ));
    }
    if moving {
        list = list.push(drop_slot(
            DragSlot::EndOfRoles,
            slot == Some(DragSlot::EndOfRoles),
            0.0,
            tokens,
        ));
    }
    if let Some(everyone) = main.server.everyone() {
        list = list.push(role_row(app, main, everyone, false, false));
    }
    if moving && let Some(label) = dragged_label(app, main) {
        list = list.push(drag_banner(&label, tokens));
    }

    list = list.push(Space::new().height(8.0));
    list = list.push(action(
        Kind::Secondary,
        "＋ New role",
        (!full).then_some(Message::Admin(AdminMsg::RoleCreate)),
        &if full {
            format!("A server holds at most {} roles", admin::MAX_ROLES)
        } else {
            String::new()
        },
        tokens,
    ));
    list = list.push(hint(
        "Drag to reorder. A member takes the colour of their highest role; roles above your own are locked.",
        tokens,
    ));

    container(list)
        .padding(12)
        .width(LIST_WIDTH)
        .style(styles::container::card(tokens))
        .into()
}

fn dragged_label(app: &App, main: &MainState) -> Option<String> {
    let DragItem::Role(id) = app.ui.drag.as_ref()?.item else {
        return None;
    };
    main.server.roles.get(&id).map(|role| role.name.clone())
}

/// One role: its colour, its name, how many hold it, and the way to move it.
fn role_row<'a>(
    app: &'a App,
    main: &'a MainState,
    role: &'a Role,
    up: bool,
    down: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let id = role.id;
    let mine = main.server.can_manage_role(id);
    let selected = main.admin.role.id == Some(id);
    let count = if role.everyone {
        main.server.members.len()
    } else {
        main.server
            .members
            .values()
            .filter(|member| member.role_ids.contains(&id))
            .count()
    };

    let mut line = Row::new().spacing(6).align_y(Vertical::Center);
    if role.everyone {
        line = line.push(Space::new().width(SWATCH));
    } else {
        line = line.push(grip(
            DragItem::Role(id),
            mine,
            if mine {
                "Drag to reorder"
            } else {
                "This role outranks yours"
            },
            tokens,
        ));
    }
    line = line.push(
        button(
            row![
                dot(role.color, tokens),
                widgets::clipped_name(
                    text(role.name.clone()).size(TEXT_ROW).color(if selected {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    }),
                    &role.name,
                    tokens,
                ),
                text(count.to_string())
                    .size(TEXT_BADGE)
                    .color(tokens.text_muted),
            ]
            .spacing(6)
            .align_y(Vertical::Center),
        )
        .width(Length::Fill)
        .padding([3.0, 6.0])
        .style(styles::button::row_for(tokens, selected))
        .on_press(Message::Admin(AdminMsg::RoleSelect(id))),
    );
    if !role.everyone {
        line = line.push(arrows(DragItem::Role(id), up && mine, down && mine, tokens));
    }
    line.into()
}

/// A role's colour, or the hollow circle that says it has none.
fn dot<'a>(rgb: u32, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let color = if rgb == 0 {
        tokens.text_muted
    } else {
        color_of(rgb)
    };
    container(Space::new())
        .width(DOT)
        .height(DOT)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(color)),
            border: border::rounded(DOT / 2.0),
            ..container::Style::default()
        })
        .into()
}

/// The editor for whichever role is in the draft.
fn editor<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.admin.role;

    let existing = draft.id.and_then(|id| main.server.roles.get(&id));
    let everyone = existing.is_some_and(|role| role.everyone);
    // `@everyone` takes a permission change from any `MANAGE_ROLES` holder; every
    // other role has to be below the reader's own highest.
    let editable = match draft.id {
        Some(id) => everyone || main.server.can_manage_role(id),
        None => main.server.can(permissions::MANAGE_ROLES, None),
    };
    let locked_tip = if everyone {
        "Only @everyone's permissions can change"
    } else if editable {
        ""
    } else {
        "This role outranks yours"
    };
    let identity = editable && !everyone;

    let title = match existing {
        Some(role) => format!("Edit role — {}", role.name),
        None => "New role".to_owned(),
    };
    let ready = editable && (everyone || !draft.name.trim().is_empty());

    let mut rows = vec![
        field(
            "Name",
            text_input("Mods", &draft.name)
                .on_input_maybe(
                    identity.then_some(|value| Message::Admin(AdminMsg::RoleDraftName(value))),
                )
                .padding(8)
                .size(TEXT_BODY)
                .style(styles::text_input(tokens))
                .into(),
            tokens,
        ),
        field("Colour", colors(draft.color, identity, tokens), tokens),
        field("Icon", icon_row(app, main, draft, identity), tokens),
        hoist_row(draft.hoist, identity, locked_tip, tokens),
    ];
    if !locked_tip.is_empty() {
        rows.push(hint(locked_tip, tokens));
    }
    rows.push(permission_groups(app, main, draft, editable));
    rows.push(
        row![
            action(
                Kind::Primary,
                if draft.id.is_some() {
                    "Save changes"
                } else {
                    "Create role"
                },
                ready.then_some(Message::Admin(AdminMsg::RoleSave)),
                if ready { "" } else { locked_tip },
                tokens,
            ),
            Space::new().width(Length::Fill),
            action(
                Kind::Danger,
                "Delete role",
                draft.id.filter(|_| editable && !everyone).map(|role_id| {
                    Message::Ui(UiMsg::OpenDialog(Dialog::ConfirmDeleteRole { role_id }))
                }),
                if everyone {
                    "@everyone can never be deleted"
                } else {
                    locked_tip
                },
                tokens,
            ),
        ]
        .align_y(Vertical::Center)
        .into(),
    );

    container(card(&title, rows, tokens))
        .width(Length::Fill)
        .into()
}

/// The palette, plus the square that means "no colour of its own".
fn colors<'a>(current: u32, enabled: bool, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let mut squares = Row::new().spacing(4).align_y(Vertical::Center);
    squares = squares.push(action(
        Kind::Ghost,
        "None",
        (enabled && current != 0).then_some(Message::Admin(AdminMsg::RoleDraftColor(0))),
        "",
        tokens,
    ));
    for rgb in PALETTE {
        squares = squares.push(swatch(rgb, rgb == current, enabled, tokens));
    }
    Column::new()
        .push(squares)
        .push(
            text(if current == 0 {
                "No colour".to_owned()
            } else {
                format!("#{:06X}", current & 0x00FF_FFFF)
            })
            .size(TEXT_SECONDARY)
            .color(tokens.text_muted),
        )
        .spacing(4)
        .into()
}

fn swatch<'a>(
    rgb: u32,
    chosen: bool,
    enabled: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let color = color_of(rgb);
    let ring = if chosen {
        tokens.text_primary
    } else {
        tokens.border_subtle
    };
    button(Space::new().width(SWATCH).height(SWATCH))
        .style(move |_theme: &Theme, _status| iced::widget::button::Style {
            background: Some(Background::Color(color)),
            border: border::rounded(styles::RADIUS_CHIP)
                .width(if chosen { 2.0 } else { 1.0 })
                .color(ring),
            ..iced::widget::button::Style::default()
        })
        .on_press_maybe(enabled.then_some(Message::Admin(AdminMsg::RoleDraftColor(rgb))))
        .into()
}

/// None, an emoji, or an image the page uploads.
fn icon_row<'a>(
    app: &'a App,
    main: &'a MainState,
    draft: &'a RoleDraft,
    enabled: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut bar = Row::new().spacing(6).align_y(Vertical::Center);

    for (label, chosen, next) in [
        (
            "None",
            matches!(draft.icon, RoleIconDraft::None),
            RoleIconDraft::None,
        ),
        (
            "Emoji",
            matches!(draft.icon, RoleIconDraft::Emoji(_)),
            RoleIconDraft::Emoji(String::new()),
        ),
        (
            "Image",
            matches!(draft.icon, RoleIconDraft::Image(_)),
            RoleIconDraft::Image(0),
        ),
    ] {
        bar = bar.push(
            button(text(label).size(TEXT_BADGE))
                .padding([3.0, 9.0])
                .style(styles::button::row_for(tokens, chosen))
                .on_press_maybe(
                    (enabled && !chosen).then_some(Message::Admin(AdminMsg::RoleDraftIcon(next))),
                ),
        );
    }

    match &draft.icon {
        RoleIconDraft::Emoji(emoji) => {
            bar = bar.push(
                text_input("🛡", emoji)
                    .on_input_maybe(enabled.then_some(|value| {
                        Message::Admin(AdminMsg::RoleDraftIcon(RoleIconDraft::Emoji(value)))
                    }))
                    .width(80.0)
                    .padding(6)
                    .size(TEXT_BODY)
                    .style(styles::text_input(tokens)),
            );
        }
        RoleIconDraft::Image(id) => {
            if let Some(handle) = (*id != 0)
                .then(|| widgets::image_handle(&main.chat, ImageKey::Image(*id)))
                .flatten()
            {
                bar = bar.push(
                    iced::widget::image(handle)
                        .width(SWATCH)
                        .height(SWATCH)
                        .content_fit(iced::ContentFit::Cover)
                        .border_radius(SWATCH / 2.0),
                );
            } else {
                bar = bar.push(icons::icon(Icon::Image, SWATCH, tokens.text_muted));
            }
            bar = bar.push(action(
                Kind::Secondary,
                "Choose…",
                enabled.then_some(Message::Admin(AdminMsg::RolePickIcon)),
                "",
                tokens,
            ));
        }
        RoleIconDraft::None => {}
    }
    bar.into()
}

/// Whether the role's members are listed as their own group.
fn hoist_row<'a>(
    hoist: bool,
    enabled: bool,
    tip: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let switch = toggler(hoist)
        .on_toggle_maybe(enabled.then_some(|value| Message::Admin(AdminMsg::RoleDraftHoist(value))))
        .size(18.0)
        .style(styles::toggler(tokens));

    widgets::tooltip_of(
        row![
            Column::new()
                .push(
                    text("Show separately in the member list")
                        .size(TEXT_ROW)
                        .color(tokens.text_primary),
                )
                .push(
                    text("Members with this role get their own group.")
                        .size(TEXT_BADGE)
                        .color(tokens.text_muted),
                )
                .spacing(1)
                .width(Length::Fill),
            switch,
        ]
        .spacing(8)
        .align_y(Vertical::Center),
        tip,
        iced::widget::tooltip::Position::Left,
        tokens,
    )
}

/// Every bit, in the three groups a reader thinks in.
fn permission_groups<'a>(
    app: &'a App,
    main: &'a MainState,
    draft: &'a RoleDraft,
    editable: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let groups: [(&str, &[u64]); 3] = [
        (
            "Server permissions",
            &[
                permissions::MANAGE_SERVER,
                permissions::MANAGE_ROLES,
                permissions::MANAGE_MEMBERS,
                permissions::MANAGE_INVITES,
                permissions::KICK_MEMBERS,
                permissions::BAN_MEMBERS,
                permissions::CHANGE_NICKNAME,
            ],
        ),
        (
            "Channel permissions",
            &[
                permissions::MANAGE_CHANNELS,
                permissions::MANAGE_MESSAGES,
                permissions::VIEW_CHANNEL,
                permissions::SEND_MESSAGES,
                permissions::ATTACH_FILES,
                permissions::ADD_REACTIONS,
                permissions::MENTION_EVERYONE,
            ],
        ),
        (
            "Voice permissions",
            &[
                permissions::CONNECT,
                permissions::SPEAK,
                permissions::SHARE_SCREEN,
                permissions::MUTE_MEMBERS,
                permissions::DEAFEN_MEMBERS,
                permissions::MOVE_MEMBERS,
                permissions::PRIORITY_SPEAKER,
            ],
        ),
    ];

    let mut body = Column::new().spacing(12).width(Length::Fill);
    for (label, bits) in groups {
        let mut group = Column::new()
            .push(widgets::section_label(label, tokens))
            .spacing(6)
            .width(Length::Fill);
        for bit in bits {
            group = group.push(permission_row(
                main,
                draft.permissions,
                *bit,
                editable,
                tokens,
            ));
        }
        body = body.push(group);
    }
    body.into()
}

/// One switch. A bit the reader does not hold themselves cannot be handed out,
/// but taking one away is always theirs to do: `UpdateRole` asks for `CanGrant`
/// on the bits being added, never on the ones being dropped.
fn permission_row<'a>(
    main: &'a MainState,
    held: u64,
    bit: u64,
    editable: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let on = permissions::has(held, bit);
    let grantable = main.server.can_grant(bit);
    let enabled = editable && (on || grantable);
    let tip = match (grantable || !editable, on) {
        (true, _) => String::new(),
        (false, true) => format!(
            "You do not hold {}: you can take it away, but not put it back",
            permissions::name(bit).unwrap_or_default()
        ),
        (false, false) => format!(
            "You do not hold {}",
            permissions::name(bit).unwrap_or_default()
        ),
    };

    let switch = toggler(on)
        .on_toggle_maybe(
            enabled
                .then_some(move |value| Message::Admin(AdminMsg::RoleDraftPermission(bit, value))),
        )
        .size(18.0)
        .style(styles::toggler(tokens));

    widgets::tooltip_of(
        row![
            Column::new()
                .push(
                    text(permissions::label(bit))
                        .size(TEXT_ROW)
                        .color(tokens.text_primary),
                )
                .push(
                    text(permissions::describe(bit))
                        .size(TEXT_BADGE)
                        .color(tokens.text_muted),
                )
                .spacing(1)
                .width(Length::Fill),
            switch,
        ]
        .spacing(8)
        .align_y(Vertical::Center),
        &tip,
        iced::widget::tooltip::Position::Left,
        tokens,
    )
}
