//! The Invites page: every invite ever made, and the one being made now.
//!
//! A fresh code exists once, in the dialog the answer opens: the server keeps
//! only its hash, and nothing here logs it or puts it in a toast.

use std::fmt;

use chrono::{Local, TimeZone as _};
use iced::alignment::Vertical;
use iced::widget::{Column, container, pick_list, row, text};
use iced::{Element, Length};
use vorcall_core::Invite;

use crate::app::message::{AdminMsg, Message};
use crate::app::update::admin;
use crate::app::{App, MainState};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, cells, heads, hint, rest_status};
use crate::view::{TEXT_BADGE, TEXT_ROW};

/// How long an invite may be asked to last, as the page offers it.
const DAYS: [u32; 6] = [1, 7, 14, 30, 90, 365];
/// The table's columns.
const CREATED: f32 = 140.0;
const EXPIRES: f32 = 140.0;
const USED_BY: f32 = 120.0;
const MADE_BY: f32 = 120.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    Column::new()
        .push(create(app, main))
        .push(list(app, main))
        .spacing(16)
        .width(Length::Fill)
        .into()
}

/// The form that asks the server for one code.
fn create<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let days = admin::invite_days(main.admin.invite_days);
    let options: Vec<Days> = DAYS.into_iter().map(Days).collect();

    card(
        "Create an invite",
        vec![
            row![
                pick_list(options, Some(Days(days)), |chosen| Message::Admin(
                    AdminMsg::InviteDays(chosen.0)
                ))
                .text_size(TEXT_ROW)
                .padding([6.0, 10.0])
                .style(styles::pick_list(tokens))
                .menu_style(styles::menu(tokens)),
                action(
                    Kind::Primary,
                    "Create invite",
                    Some(Message::Admin(AdminMsg::InviteCreate)),
                    "",
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center)
            .into(),
            hint(
                "The code is shown once, when it arrives. Nobody can read it back afterwards.",
                tokens,
            ),
        ],
        tokens,
    )
}

/// Every invite the server knows about.
fn list<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let invites = &main.admin.invites;

    let mut rows = vec![
        row![
            action(
                Kind::Secondary,
                "Refresh",
                Some(Message::Admin(AdminMsg::InvitesRefresh)),
                "",
                tokens,
            ),
            text(format!("{} invites", invites.rows().len()))
                .size(TEXT_BADGE)
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_y(Vertical::Center)
        .into(),
    ];

    if let Some(status) = rest_status(invites, "Press Refresh to read the list.", tokens) {
        rows.push(status);
    } else {
        rows.push(heads(
            vec![
                ("Created", CREATED),
                ("Expires", EXPIRES),
                ("Used by", USED_BY),
                ("Made by", MADE_BY),
                ("", 0.0),
            ],
            tokens,
        ));
        for invite in invites.rows() {
            rows.push(invite_row(app, main, invite));
        }
    }

    card("Invites", rows, tokens)
}

/// One invite: when it was made, when it dies, and who used it.
fn invite_row<'a>(app: &'a App, main: &'a MainState, invite: &'a Invite) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let used = invite.used_by != 0;
    let used_by = if used {
        if invite.used_by_username.is_empty() {
            main.server.display_name(invite.used_by).to_owned()
        } else {
            invite.used_by_username.clone()
        }
    } else {
        "—".to_owned()
    };
    let made_by = if invite.created_by == 0 {
        // `PROTOCOL.md` § REST: 0 is an invite the admin CLI made.
        "the CLI".to_owned()
    } else {
        main.server.display_name(invite.created_by).to_owned()
    };

    container(cells(vec![
        (cell(&stamp(invite.created_at_unix_ms), tokens), CREATED),
        (cell(&stamp(invite.expires_at_unix_ms), tokens), EXPIRES),
        (cell(&used_by, tokens), USED_BY),
        (cell(&made_by, tokens), MADE_BY),
        (
            action(
                Kind::Ghost,
                "Revoke",
                (!used).then_some(Message::Admin(AdminMsg::InviteRevoke(invite.id))),
                if used { "Already used" } else { "" },
                tokens,
            ),
            0.0,
        ),
    ]))
    .padding([3.0, 4.0])
    .width(Length::Fill)
    .style(styles::container::elevated(tokens))
    .into()
}

fn cell<'a>(value: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text(value.to_owned())
        .size(TEXT_BADGE)
        .color(tokens.text_secondary)
        .into()
}

/// One wire timestamp as a local date and time; `0` is no time at all.
fn stamp(unix_ms: i64) -> String {
    if unix_ms == 0 {
        return "—".to_owned();
    }
    match Local.timestamp_millis_opt(unix_ms).single() {
        Some(at) => at.format("%d %b %Y %H:%M").to_string(),
        None => "—".to_owned(),
    }
}

/// A pick-list entry for how long an invite lasts.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Days(u32);

impl fmt::Display for Days {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            1 => f.write_str("1 day"),
            days => write!(f, "{days} days"),
        }
    }
}
