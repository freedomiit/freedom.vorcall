//! The DM view: the conversation list on the left, and the other person on the
//! right.

use iced::alignment::Vertical;
use iced::widget::{
    Space, button, column, container, hover, image, mouse_area, row, rule, scrollable, stack, text,
    tooltip,
};
use iced::{Background, ContentFit, Element, Length, Padding, Theme, border};
use vorcall_core::{Channel, Role};

use crate::app::message::{ChannelsMsg, MenuTarget, Message, UiMsg, VoiceMsg};
use crate::app::state::rules::plain_text;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::widgets::{self, Metrics};
use crate::view::{
    HEADER_HEIGHT, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, TEXT_TITLE, bold,
};
use crate::workers::images::ImageKey;

/// How wide the pane on the right is, and how the profile in it is laid out.
const PROFILE_WIDTH: f32 = 300.0;
const BANNER_HEIGHT: f32 = 100.0;
const PROFILE_AVATAR: f32 = 80.0;
/// The ring of the pane's own colour around that avatar, and how far below the
/// banner it hangs.
const AVATAR_FRAME: f32 = 5.0;
const AVATAR_DROP: f32 = 36.0;
/// One conversation's row, which carries two lines.
const DM_ROW_HEIGHT: f32 = 48.0;
/// How much of the last message the list shows.
const PREVIEW_MAX: usize = 40;

/// The conversation list.
pub fn pane<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);

    let mut list = column![].spacing(2).padding(8).width(Length::Fill);
    let conversations = ordered(app, main);
    if conversations.is_empty() {
        list = list.push(widgets::empty_state(
            Icon::User,
            "No conversations",
            "Open somebody's profile to start one.",
            tokens,
        ));
    }
    for channel in conversations {
        list = list.push(conversation(app, main, channel, metrics));
    }

    container(
        column![
            container(
                text("Direct messages")
                    .size(metrics.text(TEXT_TITLE))
                    .font(bold())
                    .color(tokens.text_primary)
            )
            .width(Length::Fill)
            .padding([0.0, 16.0])
            .center_y(metrics.height(HEADER_HEIGHT)),
            rule::horizontal(1.0).style(styles::rule(tokens)),
            search(app, metrics),
            scrollable(list)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(styles::scrollable(tokens)),
            widgets::user_bar(app, main),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(app.config.sidebar_width)
    .height(Length::Fill)
    .style(styles::container::sidebar(tokens))
    .into()
}

/// The conversations still in the list, the one spoken in last at the top.
fn ordered<'a>(app: &'a App, main: &'a MainState) -> Vec<&'a Channel> {
    let mut conversations: Vec<&Channel> = main
        .server
        .dms()
        .into_iter()
        .filter(|channel| !app.config.hidden_dms.contains(&channel.id))
        .collect();
    conversations.sort_by_key(|channel| {
        let newest = main
            .chat
            .channel(channel.id)
            .map_or(0, |channel| channel.newest_seen);
        std::cmp::Reverse((newest, channel.id))
    });
    conversations
}

/// The box that opens the quick switcher, which is where a conversation is found.
fn search<'a>(app: &'a App, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    container(
        button(widgets::row_body(
            row![
                icons::icon(Icon::Search, widgets::ICON_MARK, tokens.text_muted),
                text("Find a conversation")
                    .size(metrics.text(TEXT_ROW))
                    .color(tokens.text_muted),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        ))
        .width(Length::Fill)
        .height(metrics.height(widgets::ROW_HEIGHT))
        .padding([0.0, 10.0])
        .style(styles::button::secondary(tokens))
        .on_press(Message::Ui(UiMsg::OpenQuickSwitcher)),
    )
    .width(Length::Fill)
    .padding(Padding::ZERO.top(12.0).bottom(4.0).left(8.0).right(8.0))
    .into()
}

/// One conversation: who it is with, what was said last, and what is waiting.
fn conversation<'a>(
    app: &'a App,
    main: &'a MainState,
    channel: &'a Channel,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let channel_id = channel.id;
    let partner = main.server.dm_partner(channel);
    let unread = main.chat.channel(channel_id).map_or(0, |ui| ui.unread);
    let selected = main.chat.current.is(channel_id);
    let color = match partner.map_or(0, |user_id| main.server.member_color(user_id)) {
        0 => tokens.text_primary,
        rgb => widgets::color_of(rgb),
    };

    let mut line = row![].spacing(10).align_y(Vertical::Center);
    if let Some(user_id) = partner {
        line = line.push(widgets::member_avatar_on(
            main,
            user_id,
            metrics.row_avatar(),
            tokens.bg_sidebar,
            tokens,
        ));
    }
    let title = main.server.channel_title(channel_id);
    let last = preview(main, channel_id, partner);
    line = line.push(
        column![
            widgets::clipped_name(
                text(title.clone())
                    .size(metrics.text(TEXT_BODY))
                    .font(bold())
                    .color(color),
                &title,
                tokens,
            ),
            widgets::clipped_name(
                text(last.clone())
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(if unread > 0 {
                        tokens.text_primary
                    } else {
                        tokens.text_secondary
                    }),
                &last,
                tokens,
            ),
        ]
        .width(Length::Fill),
    );
    line = line.push(widgets::badge(unread, tokens));

    let open = app
        .ui
        .context_menu
        .is_some_and(|menu| menu.target == MenuTarget::Channel(channel_id));
    let entry = button(widgets::row_body(line))
        .width(Length::Fill)
        .height(metrics.height(DM_ROW_HEIGHT))
        .padding([0.0, 8.0])
        .clip(true)
        .style(styles::button::row_state(tokens, selected, open))
        .on_press(Message::Channels(ChannelsMsg::Select(channel_id)));

    // A conversation is never left, only closed: a new message brings it back.
    let close = container(widgets::icon_button_in(
        Icon::Close,
        "Close this conversation",
        Some(Message::Channels(ChannelsMsg::HideDm(channel_id))),
        tokens.text_secondary,
        widgets::ICON_MARK,
        tokens,
    ))
    .align_right(Length::Fill)
    .center_y(Length::Fill)
    .padding(Padding::ZERO.right(8.0));

    mouse_area(hover(entry, close))
        .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Channel(
            channel_id,
        ))))
        .into()
}

/// What the row says under the name: the last message, or who it is with until
/// there is one.
fn preview(main: &MainState, channel_id: i64, partner: Option<i64>) -> String {
    let newest = main
        .chat
        .channel(channel_id)
        .and_then(|channel| channel.newest());
    let Some(message) = newest else {
        return partner
            .and_then(|user_id| main.server.members.get(&user_id))
            .map(|profile| format!("@{}", profile.username))
            .unwrap_or_default();
    };

    let body = if message.deleted {
        "Message deleted".to_owned()
    } else if message.text.is_empty() && !message.attachments.is_empty() {
        "[image]".to_owned()
    } else {
        plain_text(&message.text, &main.user_pairs)
    };
    let body = shorten(body, PREVIEW_MAX);
    if message.author_id == main.member_id {
        format!("You: {body}")
    } else {
        body
    }
}

/// One line of it, with an ellipsis where the rest was.
fn shorten(body: String, max: usize) -> String {
    if body.chars().count() <= max {
        return body;
    }
    body.chars().take(max).collect::<String>() + "…"
}

/// The right-hand pane of the DM view: who the conversation is with.
pub fn profile_pane<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);
    let channel = main
        .chat
        .current
        .channel_id()
        .and_then(|id| main.server.channel(id));
    let partner = channel.and_then(|channel| main.server.dm_partner(channel));

    let body: Element<'_, Message> = match (channel, partner) {
        (Some(channel), Some(user_id)) => profile(app, main, channel.id, user_id, metrics),
        _ => widgets::empty_state(
            Icon::User,
            "No conversation",
            "Pick one on the left.",
            tokens,
        ),
    };

    container(
        row![
            rule::vertical(1.0).style(styles::rule(tokens)),
            container(body).width(Length::Fill).height(Length::Fill),
        ]
        .height(Length::Fill),
    )
    .width(PROFILE_WIDTH)
    .height(Length::Fill)
    .style(styles::container::sidebar(tokens))
    .into()
}

/// The banner, the avatar over it, and everything known about the other person.
fn profile<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    user_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let profile = main.server.members.get(&user_id);
    let color = match main.server.member_color(user_id) {
        0 => tokens.text_primary,
        rgb => widgets::color_of(rgb),
    };
    let owner = user_id != 0 && user_id == main.server.server.owner_id;

    let online = main.server.is_online(user_id);
    let mut body = column![
        column![
            widgets::clipped_name(
                text(main.server.display_name(user_id))
                    .size(metrics.text(TEXT_SECTION))
                    .font(bold())
                    .color(color),
                main.server.display_name(user_id),
                tokens,
            ),
            text(match profile {
                Some(profile) if owner => format!("@{} · Owner", profile.username),
                Some(profile) => format!("@{}", profile.username),
                None => String::new(),
            })
            .size(metrics.text(TEXT_ROW))
            .color(tokens.text_secondary),
            row![
                widgets::presence_dot(online, tokens),
                text(if online { "Online" } else { "Offline" })
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.text_muted),
            ]
            .spacing(6)
            .align_y(Vertical::Center),
        ]
        .spacing(4)
    ]
    .spacing(12)
    .padding(Padding::ZERO.left(16.0).right(16.0).bottom(16.0))
    .width(Length::Fill);

    if let Some(card) = about(app, main, user_id, metrics) {
        body = body.push(card);
    }
    body = body.push(call_button(app, main, channel_id, metrics));
    if let Some(call) = in_this_call(app, main, channel_id, metrics) {
        body = body.push(call);
    }

    scrollable(
        column![banner(app, main, user_id), body]
            .width(Length::Fill)
            .spacing(0),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(styles::scrollable(tokens))
    .into()
}

/// The banner with the avatar straddling its lower edge, the way the design has
/// it.
fn banner<'a>(app: &'a App, main: &'a MainState, user_id: i64) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let profile = main.server.members.get(&user_id);

    let picture = profile
        .map(|profile| profile.banner_image_id)
        .filter(|id| *id != 0)
        .and_then(|id| widgets::image_handle(&main.chat, ImageKey::Image(id)));
    let band: Element<'_, Message> = match picture {
        Some(handle) => image(handle)
            .width(Length::Fill)
            .height(BANNER_HEIGHT)
            .content_fit(ContentFit::Cover)
            .into(),
        None => {
            let accent = profile
                .map(|profile| profile.accent_color)
                .filter(|rgb| *rgb != 0)
                .map_or(tokens.accent, widgets::color_of);
            container(Space::new())
                .width(Length::Fill)
                .height(BANNER_HEIGHT)
                .style(move |_theme: &Theme| container::Style {
                    background: Some(Background::Color(accent)),
                    ..container::Style::default()
                })
                .into()
        }
    };

    let ground = tokens.bg_sidebar;
    let radius = PROFILE_AVATAR / 2.0 + AVATAR_FRAME;
    let framed = container(widgets::member_avatar_on(
        main,
        user_id,
        PROFILE_AVATAR,
        ground,
        tokens,
    ))
    .padding(AVATAR_FRAME)
    .style(move |_theme: &Theme| container::Style {
        background: Some(Background::Color(ground)),
        border: border::rounded(radius),
        ..container::Style::default()
    });

    stack![
        // The first child is what the stack takes its size from, so the band and
        // the avatar both have the room they need.
        container(Space::new())
            .width(Length::Fill)
            .height(BANNER_HEIGHT + AVATAR_DROP),
        band,
        container(framed)
            .align_left(Length::Fill)
            .align_bottom(Length::Fill)
            .padding(Padding::ZERO.left(16.0)),
    ]
    .into()
}

/// What the other person says about themselves, and what they are.
fn about<'a>(
    app: &'a App,
    main: &'a MainState,
    user_id: i64,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let profile = main.server.members.get(&user_id)?;
    let roles: Vec<&Role> = profile
        .role_ids
        .iter()
        .filter_map(|id| main.server.roles.get(id))
        .collect();
    if profile.description.is_empty() && roles.is_empty() {
        return None;
    }

    let mut card = column![].spacing(10).width(Length::Fill);
    if !profile.description.is_empty() {
        card = card.push(
            column![
                widgets::group_label("About", None, metrics, tokens),
                text(profile.description.clone())
                    .size(metrics.text(TEXT_ROW))
                    .color(tokens.text_primary),
            ]
            .spacing(4),
        );
    }
    if !roles.is_empty() {
        let mut chips = row![].spacing(6);
        for role in roles {
            chips = chips.push(widgets::role_chip(role, tokens));
        }
        card = card
            .push(column![widgets::group_label("Roles", None, metrics, tokens), chips].spacing(6));
    }

    Some(
        container(card)
            .width(Length::Fill)
            .padding(12)
            .style(styles::container::card(tokens))
            .into(),
    )
}

/// Calling the other person, which is this DM's own voice channel.
fn call_button<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let joined = main.voice.intent && main.voice.channel_id == channel_id;

    let control = button(widgets::row_body(
        row![
            icons::icon(Icon::Speaker, widgets::ICON_MARK, tokens.text_on_accent),
            text("Call")
                .size(metrics.text(TEXT_ROW))
                .font(bold())
                .color(tokens.text_on_accent),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    ))
    .width(Length::Fill)
    .height(metrics.height(widgets::CONTROL_HEIGHT))
    .style(styles::button::primary(tokens))
    .on_press_maybe((!joined).then_some(Message::Voice(VoiceMsg::Join(channel_id))));

    widgets::tooltip_of(
        control,
        if joined {
            "You are in this call"
        } else {
            "Start a voice call in this conversation"
        },
        tooltip::Position::Top,
        tokens,
    )
}

/// Who is in this conversation's voice channel, when anybody is.
fn in_this_call<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let roster = main.voice.rosters.get(&channel_id)?;
    if roster.members.is_empty() {
        return None;
    }

    let mut list = column![widgets::group_label(
        "In this call",
        Some(roster.members.len()),
        metrics,
        tokens
    )]
    .spacing(4)
    .width(Length::Fill);
    for member in roster.members.values() {
        let user_id = member.user_id;
        list = list.push(
            row![
                widgets::speaking_avatar(
                    main,
                    user_id,
                    widgets::AVATAR_OCCUPANT,
                    roster.speaking.contains(&user_id),
                    tokens,
                ),
                widgets::clipped_name(
                    text(main.server.display_name(user_id))
                        .size(metrics.text(TEXT_SECONDARY))
                        .color(tokens.text_secondary),
                    main.server.display_name(user_id),
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        );
    }

    Some(container(list).width(Length::Fill).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preview is one line, however long the message was.
    #[test]
    fn a_preview_is_cut_short_on_a_character_boundary() {
        assert_eq!(shorten("short".to_owned(), 10), "short");
        assert_eq!(shorten("até onde vai".to_owned(), 4), "até …");
        let cut = shorten("y".repeat(PREVIEW_MAX + 10), PREVIEW_MAX);
        assert_eq!(cut.chars().count(), PREVIEW_MAX + 1);
        assert!(cut.ends_with('…'));
    }
}
