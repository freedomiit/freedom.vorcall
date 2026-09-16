//! The member pane: everyone who may see the channel in view, grouped by hoisted
//! role, then Online, then Offline.

use iced::alignment::Vertical;
use iced::widget::{
    Space, button, column, container, mouse_area, row, rule, scrollable, text, tooltip,
};
use iced::{Element, Length, Padding};

use crate::app::message::{MenuTarget, Message, UiMsg};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::widgets::{self, Metrics};
use crate::view::{TEXT_BODY, bold};

pub fn pane<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);

    let body: Element<'_, Message> = match main.chat.current.channel_id() {
        Some(channel_id) => list(app, main, channel_id, metrics),
        // Nothing is open, so there is no membership to list.
        None => Space::new().into(),
    };

    container(
        row![
            rule::vertical(1.0).style(styles::rule(tokens)),
            container(body).width(Length::Fill).height(Length::Fill),
        ]
        .height(Length::Fill),
    )
    .width(app.config.members_width)
    .height(Length::Fill)
    .style(styles::container::sidebar(tokens))
    .into()
}

/// The groups the model works out, each with its own heading.
fn list<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let groups = main.server.member_groups(channel_id);
    if groups.is_empty() {
        return widgets::empty_state(
            Icon::Users,
            "Nobody here",
            "Members who can see this channel are listed here.",
            tokens,
        );
    }

    let mut list = column![]
        .spacing(2)
        .padding([12.0, 8.0])
        .width(Length::Fill);
    for group in groups {
        list = list.push(
            container(widgets::group_label(
                &group.label,
                Some(group.members.len()),
                metrics,
                tokens,
            ))
            .padding(Padding::ZERO.top(10.0).bottom(4.0).left(4.0).right(4.0)),
        );
        for user_id in group.members {
            list = list.push(member_row(app, main, user_id, metrics));
        }
    }

    scrollable(list)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::scrollable(tokens))
        .into()
}

/// One member: their avatar with the presence dot, their name in its role's
/// colour, and what marks them.
fn member_row<'a>(
    app: &'a App,
    main: &'a MainState,
    user_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let online = main.server.is_online(user_id);
    // Offline reads quieter: the name goes grey rather than the row going
    // translucent, which iced has no way to draw.
    let color = match main.server.member_color(user_id) {
        _ if !online => tokens.text_muted,
        0 => tokens.text_secondary,
        rgb => widgets::color_of(rgb),
    };

    let name = main.server.display_name(user_id);
    let mut line = row![
        widgets::member_avatar_on(
            main,
            user_id,
            metrics.row_avatar(),
            tokens.bg_sidebar,
            tokens
        ),
        widgets::clipped_name(
            text(name)
                .size(metrics.text(TEXT_BODY))
                .font(bold())
                .color(color),
            name,
            tokens,
        ),
    ]
    .spacing(10)
    .align_y(Vertical::Center);

    if let Some(role) = main.server.member_badge_role(user_id) {
        line = line.push(widgets::tooltip_of(
            widgets::role_icon(main, role, widgets::ICON_MARK, tokens),
            &role.name,
            tooltip::Position::Left,
            tokens,
        ));
    }
    if user_id != 0 && user_id == main.server.server.owner_id {
        line = line.push(widgets::owner_crown(tokens));
    }

    // Whoever is in a voice channel anywhere is shown as such here.
    if let Some((voice_channel_id, member)) = main
        .voice
        .rosters
        .iter()
        .find_map(|(channel_id, roster)| Some((*channel_id, roster.members.get(&user_id)?)))
    {
        // One's own switches are known here before the server has echoed them, so
        // the icon flips on the press rather than on the round trip.
        let self_muted = if user_id == main.member_id {
            main.voice.muted
        } else {
            member.self_muted
        };
        let self_deafened = if user_id == main.member_id {
            main.voice.deafened
        } else {
            member.self_deafened
        };

        if let Some(mark) = widgets::voice_flag(
            Icon::MicOff,
            member.server_muted,
            self_muted,
            "Muted by a moderator",
            "Muted",
            tooltip::Position::Left,
            tokens,
        ) {
            line = line.push(mark);
        }
        if let Some(mark) = widgets::voice_flag(
            Icon::HeadphonesOff,
            member.server_deafened,
            self_deafened,
            "Deafened by a moderator",
            "Deafened",
            tooltip::Position::Left,
            tokens,
        ) {
            line = line.push(mark);
        }
        if member.camera {
            line = line.push(widgets::camera_badge(
                main,
                voice_channel_id,
                user_id,
                tokens,
            ));
        }
        if member.sharing {
            line = line.push(widgets::watch_badge(
                main,
                voice_channel_id,
                user_id,
                tokens,
            ));
        } else if !member.camera {
            line = line.push(icons::icon(
                Icon::Speaker,
                widgets::ICON_MARK,
                tokens.text_muted,
            ));
        }
    }

    let open = app
        .ui
        .context_menu
        .is_some_and(|menu| menu.target == MenuTarget::Member(user_id));
    let entry = button(widgets::row_body(line))
        .width(Length::Fill)
        .height(metrics.height(widgets::MEMBER_ROW_HEIGHT))
        .padding([0.0, 8.0])
        .clip(true)
        .style(styles::button::row_state(tokens, false, open))
        .on_press(Message::Ui(UiMsg::OpenProfileCard(user_id)));

    mouse_area(entry)
        .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Member(user_id))))
        .into()
}
