//! The far-left rail: the mark, which is the server view, and the DMs entry.
//!
//! Nothing else ever goes here — there is one server.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Space, button, column, container, row, stack, tooltip};
use iced::{Background, Element, Length, Theme, border};

use crate::app::message::{ChannelsMsg, Message};
use crate::app::state::chat::MainView;
use crate::app::{App, MainState};
use crate::brand::mark::mark;
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::RAIL_WIDTH;
use crate::view::widgets;

/// How large the mark and the DMs icon are drawn.
const MARK_SIZE: f32 = 32.0;
const ICON_SIZE: f32 = 22.0;
/// The square an entry is, and the pill beside the one in front.
const SQUARE: f32 = 40.0;
const PILL_WIDTH: f32 = 3.0;
const PILL_HEIGHT: f32 = 16.0;
const PILL_GAP: f32 = 5.0;
/// The rail's horizontal padding: what leaves the pill, the gap and the square
/// room inside `RAIL_WIDTH`, since iced clamps a fixed width to the limits it is
/// given and would otherwise narrow the square.
const RAIL_PADDING_X: f32 = 4.0;
const _: () = assert!(RAIL_WIDTH - 2.0 * RAIL_PADDING_X >= PILL_WIDTH + PILL_GAP + SQUARE);

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let on_server = main.chat.view == MainView::Server;

    let server = entry(
        mark(MARK_SIZE),
        server_name(main),
        Message::Channels(ChannelsMsg::ShowServer),
        on_server,
        None,
        tokens,
    );

    // A conversation with something in it is a dot rather than a count: the list
    // itself is where the counts belong.
    let waiting = main
        .server
        .dms()
        .iter()
        .filter(|channel| !app.config.hidden_dms.contains(&channel.id))
        .any(|channel| {
            main.chat
                .channel(channel.id)
                .is_some_and(|channel| channel.unread > 0)
        });

    let dms = entry(
        icons::icon(
            Icon::User,
            ICON_SIZE,
            if on_server {
                tokens.text_secondary
            } else {
                tokens.text_primary
            },
        ),
        "Direct messages",
        Message::Channels(ChannelsMsg::ShowDms),
        !on_server,
        waiting.then(|| widgets::alert_dot(tokens.bg_rail, tokens)),
        tokens,
    );

    container(
        column![server, dms]
            .spacing(8)
            .padding([10.0, RAIL_PADDING_X])
            .align_x(Horizontal::Center),
    )
    .width(RAIL_WIDTH)
    .height(Length::Fill)
    .style(styles::container::rail(tokens))
    .into()
}

/// One rail entry: the pill that says it is the view in front, and the square
/// that switches to it.
fn entry<'a>(
    glyph: Element<'a, Message>,
    tip: &'a str,
    message: Message,
    selected: bool,
    badge: Option<Element<'a, Message>>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let face: Element<'_, Message> = match badge {
        Some(dot) => stack![
            container(glyph).center(Length::Fill),
            container(dot)
                .align_right(Length::Fill)
                .align_top(Length::Fill),
        ]
        .into(),
        None => container(glyph).center(Length::Fill).into(),
    };

    let square = button(face)
        .width(SQUARE)
        .height(SQUARE)
        .padding(0)
        .style(styles::button::rail(tokens, selected))
        .on_press(message);

    row![
        marker(selected, tokens),
        widgets::tooltip_of(square, tip, tooltip::Position::Right, tokens),
    ]
    .spacing(PILL_GAP)
    .align_y(Vertical::Center)
    .into()
}

/// The accent pill next to the entry in front; the space it takes is held either
/// way, so switching views never shifts the rail.
fn marker<'a>(selected: bool, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    if !selected {
        return Space::new().width(PILL_WIDTH).into();
    }
    let accent = tokens.accent;
    container(Space::new())
        .width(PILL_WIDTH)
        .height(PILL_HEIGHT)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(accent)),
            border: border::rounded(PILL_WIDTH / 2.0),
            ..container::Style::default()
        })
        .into()
}

/// What the mark's tooltip says, which is the server's own name.
fn server_name(main: &MainState) -> &str {
    if main.server.server.name.is_empty() {
        "Vorcall"
    } else {
        main.server.server.name.as_str()
    }
}
