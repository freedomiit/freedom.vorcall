//! The Bans page: who is shut out, why, and the way back in.

use chrono::{Local, TimeZone as _};
use iced::alignment::Vertical;
use iced::widget::{container, row, text};
use iced::{Element, Length};
use vorcall_core::{Ban, permissions};

use crate::app::message::{AdminMsg, Message};
use crate::app::{App, MainState};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, cells, heads, rest_status};
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_ROW};

/// The table's columns.
const WHO: f32 = 140.0;
const BY: f32 = 120.0;
const WHEN: f32 = 140.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let bans = &main.admin.bans;

    let mut rows = vec![
        row![
            action(
                Kind::Secondary,
                "Refresh",
                Some(Message::Admin(AdminMsg::BansRefresh)),
                "",
                tokens,
            ),
            text(format!("{} banned", bans.rows().len()))
                .size(TEXT_BADGE)
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_y(Vertical::Center)
        .into(),
    ];

    if let Some(status) = rest_status(bans, "Nobody is banned.", tokens) {
        rows.push(status);
    } else {
        rows.push(heads(
            vec![
                ("Account", WHO),
                ("Banned by", BY),
                ("When", WHEN),
                ("Reason", 0.0),
                ("", 0.0),
            ],
            tokens,
        ));
        for ban in bans.rows() {
            rows.push(ban_row(app, main, ban));
        }
    }

    card("Bans", rows, tokens)
}

/// One ban, with the unban beside it.
fn ban_row<'a>(app: &'a App, main: &'a MainState, ban: &'a Ban) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let banned_by = if ban.banned_by == 0 {
        // `PROTOCOL.md` § REST: 0 is a ban the admin CLI made.
        "the CLI".to_owned()
    } else {
        main.server.display_name(ban.banned_by).to_owned()
    };
    let reason = if ban.reason.trim().is_empty() {
        "—".to_owned()
    } else {
        ban.reason.clone()
    };
    let may_unban = main.server.can(permissions::BAN_MEMBERS, None);

    container(cells(vec![
        (
            widgets::clipped_name(
                text(ban.username.clone())
                    .size(TEXT_ROW)
                    .color(tokens.text_primary),
                &ban.username,
                tokens,
            ),
            WHO,
        ),
        (cell(&banned_by, tokens), BY),
        (cell(&stamp(ban.banned_at_unix_ms), tokens), WHEN),
        (cell(&reason, tokens), 0.0),
        (
            action(
                Kind::Ghost,
                "Unban",
                may_unban.then_some(Message::Admin(AdminMsg::Unban(ban.user_id))),
                if may_unban { "" } else { "Needs Ban members" },
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
