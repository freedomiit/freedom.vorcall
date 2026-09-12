//! The Members page: who is here, what they hold, and what can be done to them.
//!
//! `PROTOCOL.md` § Hierarchy decides the last part: the owner and the reader
//! themselves are never targets, and neither is anybody who outranks them.

use std::fmt;

use iced::alignment::Vertical;
use iced::widget::{Column, Row, Space, button, container, pick_list, row, text, text_input};
use iced::{Element, Length};
use vorcall_core::{Profile, permissions};

use crate::app::message::{AdminMsg, Message, UiMsg};
use crate::app::state::ui::Dialog;
use crate::app::{App, MainState};
use crate::icons::Icon;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, hint};
use crate::view::widgets::{self, color_of};
use crate::view::{AVATAR_SMALL, TEXT_BADGE, TEXT_BODY, TEXT_ROW};

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let query = main.admin.member_search.trim().to_lowercase();

    let mut shown: Vec<&Profile> = main
        .server
        .members
        .values()
        .filter(|member| matches(main, member, &query))
        .collect();
    shown.sort_by_key(|member| main.server.display_name(member.user_id).to_lowercase());

    let mut rows = Column::new().spacing(6).width(Length::Fill);
    rows = rows.push(
        row![
            text_input("Search members", &main.admin.member_search)
                .on_input(|value| Message::Admin(AdminMsg::MemberSearch(value)))
                .padding(8)
                .size(TEXT_BODY)
                .style(styles::text_input(tokens)),
            text(format!("{} of {}", shown.len(), main.server.members.len()))
                .size(TEXT_BADGE)
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    );
    if shown.is_empty() {
        rows = rows.push(hint("Nobody matches that.", tokens));
    }
    for member in shown {
        rows = rows.push(member_row(app, main, member));
    }

    card("Members", vec![rows.into()], tokens)
}

/// Whether one member is in what the search asked for.
fn matches(main: &MainState, member: &Profile, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    member.username.to_lowercase().contains(query)
        || main
            .server
            .display_name(member.user_id)
            .to_lowercase()
            .contains(query)
}

/// One member: who they are, the roles they hold, and the moderation for them.
fn member_row<'a>(app: &'a App, main: &'a MainState, member: &'a Profile) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let user_id = member.user_id;
    let owner = user_id == main.server.server.owner_id;
    let myself = user_id == main.server.me;
    // Neither the owner nor the reader themselves is ever a target, whatever the
    // permissions say.
    let target = !owner && !myself && main.server.can_target(user_id);
    // One's own nickname is the exception the hierarchy allows, with its own bit.
    let may_nickname = (myself && main.server.can(permissions::CHANGE_NICKNAME, None))
        || (target && main.server.can(permissions::MANAGE_MEMBERS, None));
    let color = match main.server.member_color(user_id) {
        0 => tokens.text_primary,
        rgb => color_of(rgb),
    };

    let mut head = row![
        widgets::member_avatar(main, user_id, AVATAR_SMALL, tokens),
        text(main.server.display_name(user_id).to_owned())
            .size(TEXT_ROW)
            .color(color),
        text(format!("@{}", member.username))
            .size(TEXT_BADGE)
            .color(tokens.text_muted),
    ]
    .spacing(6)
    .align_y(Vertical::Center);
    if owner {
        head = head.push(widgets::key_hint("owner", tokens));
    }
    head = head.push(Space::new().width(Length::Fill));
    head = head.push(nickname(app, main, user_id, member, may_nickname));
    head = head.push(action(
        Kind::Ghost,
        "Kick",
        (target && main.server.can(permissions::KICK_MEMBERS, None)).then_some(Message::Ui(
            UiMsg::OpenDialog(Dialog::ConfirmKick { user_id }),
        )),
        moderation_tip(owner, myself, permissions::KICK_MEMBERS),
        tokens,
    ));
    head = head.push(action(
        Kind::Danger,
        "Ban",
        (target && main.server.can(permissions::BAN_MEMBERS, None))
            .then_some(Message::Admin(AdminMsg::Ban(user_id))),
        moderation_tip(owner, myself, permissions::BAN_MEMBERS),
        tokens,
    ));

    container(
        Column::new()
            .push(head)
            .push(roles(app, main, member))
            .spacing(4)
            .width(Length::Fill),
    )
    .padding([6.0, 8.0])
    .width(Length::Fill)
    .style(styles::container::elevated(tokens))
    .into()
}

/// Why a moderation control is not offered.
fn moderation_tip(owner: bool, myself: bool, bit: u64) -> &'static str {
    if owner {
        "The owner cannot be moderated"
    } else if myself {
        "That is you"
    } else if bit == permissions::KICK_MEMBERS {
        "Needs Kick members, and a rank above theirs"
    } else {
        "Needs Ban members, and a rank above theirs"
    }
}

/// The roles one member holds, each with the way to take it off them, plus the
/// list of the ones that can still be added.
fn roles<'a>(app: &'a App, main: &'a MainState, member: &'a Profile) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let user_id = member.user_id;
    let may_assign = main.server.can(permissions::MANAGE_ROLES, None);

    let mut chips = Row::new().spacing(4).align_y(Vertical::Center);
    for role in member
        .role_ids
        .iter()
        .filter_map(|id| main.server.roles.get(id))
    {
        let removable = may_assign && main.server.can_manage_role(role.id);
        let color = if role.color == 0 {
            tokens.text_secondary
        } else {
            color_of(role.color)
        };
        chips = chips.push(
            container(
                row![
                    text(role.name.clone()).size(TEXT_BADGE).color(color),
                    button(text("✕").size(TEXT_BADGE))
                        .padding([0.0, 3.0])
                        .style(styles::button::icon(tokens))
                        .on_press_maybe(removable.then_some(Message::Admin(
                            AdminMsg::MemberRemoveRole(user_id, role.id)
                        ))),
                ]
                .spacing(3)
                .align_y(Vertical::Center),
            )
            .padding([1.0, 4.0])
            .style(styles::container::chip(tokens)),
        );
    }

    let addable: Vec<RoleOption> = main
        .server
        .roles
        .values()
        .filter(|role| !role.everyone && !member.role_ids.contains(&role.id))
        .filter(|role| may_assign && main.server.can_manage_role(role.id))
        .map(|role| RoleOption {
            id: role.id,
            name: role.name.clone(),
        })
        .collect();

    if addable.is_empty() {
        if member.role_ids.is_empty() {
            chips = chips.push(hint("No role of their own.", tokens));
        }
        return chips.into();
    }

    chips = chips.push(
        pick_list(addable, None::<RoleOption>, move |role| {
            Message::Admin(AdminMsg::MemberAddRole(user_id, role.id))
        })
        .placeholder("＋ role")
        .text_size(TEXT_BADGE)
        .padding([2.0, 6.0])
        .style(styles::pick_list(tokens))
        .menu_style(styles::menu(tokens)),
    );
    chips.into()
}

/// The nickname, typed in place. An entry in the draft is what an edit in flight
/// looks like.
fn nickname<'a>(
    app: &'a App,
    main: &'a MainState,
    user_id: i64,
    member: &'a Profile,
    allowed: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;

    match main.admin.nicknames.get(&user_id) {
        Some(typed) => row![
            text_input("Nickname", typed)
                .on_input(move |value| Message::Admin(AdminMsg::MemberNickname(user_id, value)))
                .on_submit(Message::Admin(AdminMsg::MemberNicknameSave(user_id)))
                .width(160.0)
                .padding(6)
                .size(TEXT_ROW)
                .style(styles::text_input(tokens)),
            widgets::icon_button(
                Icon::Check,
                "Save the nickname",
                Some(Message::Admin(AdminMsg::MemberNicknameSave(user_id))),
                tokens,
            ),
        ]
        .spacing(4)
        .align_y(Vertical::Center)
        .into(),
        None => widgets::icon_button(
            Icon::Edit,
            if allowed {
                "Change their nickname"
            } else {
                "Needs Manage members, and a rank above theirs"
            },
            allowed.then_some(Message::Admin(AdminMsg::MemberNickname(
                user_id,
                member.nickname.clone(),
            ))),
            tokens,
        ),
    }
}

/// A pick-list entry for a role: the list shows the name and hands back the id.
#[derive(Clone, PartialEq, Eq)]
struct RoleOption {
    id: i64,
    name: String,
}

impl fmt::Display for RoleOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}
