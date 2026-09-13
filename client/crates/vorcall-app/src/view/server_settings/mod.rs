//! The server settings area. Same shape as the user settings: a tab list on the
//! left, one page on the right.
//!
//! A page whose permission the mirror does not resolve is not drawn at all, and
//! its entry is not in the list either — the server refuses the frames anyway,
//! so hiding is the only thing the client owes the reader. Inside a page nothing
//! is hidden: a control the reader may not use is drawn disabled, with the
//! permission it would need in its tooltip.

pub mod bans;
pub mod channels;
pub mod invites;
pub mod members;
pub mod overview;
pub mod roles;
pub mod sounds;

use iced::alignment::Vertical;
use iced::widget::{
    Column, Row, Space, button, container, mouse_area, row, scrollable, text, tooltip,
};
use iced::{Background, Element, Length, Theme, border};

use crate::app::message::{AdminMsg, DragItem, DragMsg, DragSlot, Message, SettingsMsg};
use crate::app::state::settings::{RestList, ServerTab, SettingsTab};
use crate::app::update::admin;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_PAGE, TEXT_ROW, TEXT_SECONDARY};

const TABS_WIDTH: f32 = 200.0;
/// How wide a page's own content is allowed to grow.
const CONTENT_WIDTH: f32 = 760.0;
/// The accent line a hovered drop slot draws, and the gap around it.
const SLOT_LINE: f32 = 2.0;
const SLOT_GAP: f32 = 3.0;
/// The drag handle beside a row.
const GRIP: f32 = 14.0;

pub fn view<'a>(app: &'a App, main: &'a MainState, tab: ServerTab) -> Element<'a, Message> {
    let tokens = &app.tokens;

    // A reader who loses the permission while the page is open is shown the door
    // rather than a half-drawn form.
    if !tab.allowed(main.server.resolve(main.server.me, None)) {
        return container(widgets::empty_state(
            Icon::Shield,
            "Not for you",
            "This page needs a permission you do not hold.",
            tokens,
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::container::chat(tokens))
        .into();
    }

    let page = match tab {
        ServerTab::Overview => overview::view(app, main),
        ServerTab::Channels => channels::view(app, main),
        ServerTab::Roles => roles::view(app, main),
        ServerTab::Members => members::view(app, main),
        ServerTab::Sounds => sounds::view(app, main),
        ServerTab::Invites => invites::view(app, main),
        ServerTab::Bans => bans::view(app, main),
    };

    row![
        tabs(app, main, tab),
        container(
            scrollable(
                Column::new()
                    .push(
                        row![
                            text(tab.label()).size(TEXT_PAGE).color(tokens.text_primary),
                            Space::new().width(Length::Fill),
                            widgets::icon_button(
                                Icon::Close,
                                "Close (Esc)",
                                Some(Message::Settings(SettingsMsg::Close)),
                                tokens,
                            ),
                        ]
                        .align_y(Vertical::Center),
                    )
                    .push(container(page).max_width(CONTENT_WIDTH))
                    .spacing(16)
                    .padding(24)
                    .width(Length::Fill),
            )
            .height(Length::Fill)
            .style(styles::scrollable(tokens)),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::container::chat(tokens)),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The tab list: the server's pages the reader may open, then the way back.
fn tabs<'a>(app: &'a App, main: &'a MainState, current: ServerTab) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let mut list = Column::new()
        .push(widgets::section_label("Server settings", tokens))
        .spacing(2)
        .padding(12);
    let perms = main.server.resolve(main.server.me, None);
    for tab in ServerTab::ALL {
        if !tab.allowed(perms) {
            continue;
        }
        let selected = tab == current;
        list = list.push(
            button(
                row![
                    icons::icon(glyph(tab), GRIP, tokens.text_secondary),
                    text(tab.label()).size(TEXT_ROW),
                ]
                .spacing(8)
                .align_y(Vertical::Center),
            )
            .width(Length::Fill)
            .padding([5.0, 8.0])
            .style(styles::button::row_for(tokens, selected))
            .on_press(Message::Settings(SettingsMsg::ServerTab(tab))),
        );
    }

    list = list.push(Space::new().height(Length::Fill));
    list = list.push(
        button(text("User settings").size(TEXT_ROW))
            .width(Length::Fill)
            .padding([5.0, 8.0])
            .style(styles::button::row(tokens))
            .on_press(Message::Settings(SettingsMsg::Tab(SettingsTab::Account))),
    );

    container(list)
        .width(TABS_WIDTH)
        .height(Length::Fill)
        .style(styles::container::sidebar(tokens))
        .into()
}

/// The icon beside one tab's name.
fn glyph(tab: ServerTab) -> Icon {
    match tab {
        ServerTab::Overview => Icon::Gear,
        ServerTab::Channels => Icon::Hash,
        ServerTab::Roles => Icon::Shield,
        ServerTab::Members => Icon::Users,
        ServerTab::Sounds => Icon::Speaker,
        ServerTab::Invites => Icon::Link,
        ServerTab::Bans => Icon::Ban,
    }
}

/// Which ground a page's button draws on.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Ghost,
}

/// One labelled button. `on_press` of `None` draws it disabled, which is how a
/// missing permission or an unfinished form reads; `tip` says why.
pub fn action<'a>(
    kind: Kind,
    label: &str,
    on_press: Option<Message>,
    tip: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let control = button(text(label.to_owned()).size(TEXT_BODY))
        .padding([6.0, 14.0])
        .on_press_maybe(on_press);
    let control = match kind {
        Kind::Primary => control.style(styles::button::primary(tokens)),
        Kind::Secondary => control.style(styles::button::secondary(tokens)),
        Kind::Danger => control.style(styles::button::danger(tokens)),
        Kind::Ghost => control.style(styles::button::ghost(tokens)),
    };
    widgets::tooltip_of(control, tip, tooltip::Position::Top, tokens)
}

/// A titled card, which is how every form on these pages is grouped.
pub fn card<'a>(
    title: &str,
    rows: Vec<Element<'a, Message>>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let mut body = Column::new()
        .push(
            text(title.to_owned())
                .size(TEXT_BODY)
                .color(tokens.text_primary),
        )
        .spacing(10)
        .width(Length::Fill);
    for row in rows {
        body = body.push(row);
    }
    container(body)
        .padding(16)
        .width(Length::Fill)
        .style(styles::container::card(tokens))
        .into()
}

/// One labelled control inside a card.
pub fn field<'a>(
    label: &str,
    control: Element<'a, Message>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    Column::new()
        .push(widgets::section_label(label, tokens))
        .push(control)
        .spacing(4)
        .width(Length::Fill)
        .into()
}

/// The quiet line under a control that explains it.
pub fn hint<'a>(line: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text(line.to_owned())
        .size(TEXT_SECONDARY)
        .color(tokens.text_muted)
        .into()
}

/// What a list says while it has no rows to draw: why, or that there are none.
pub fn rest_status<'a, T>(
    list: &'a RestList<T>,
    empty: &str,
    tokens: &'a ThemeTokens,
) -> Option<Element<'a, Message>> {
    if list.is_loading() {
        return Some(hint("Loading…", tokens));
    }
    if let Some(error) = list.error() {
        return Some(
            text(error.to_owned())
                .size(TEXT_SECONDARY)
                .color(tokens.danger)
                .into(),
        );
    }
    if list.rows().is_empty() {
        return Some(hint(empty, tokens));
    }
    None
}

/// One row of a table: each cell as wide as its column, the last one filling.
pub fn cells<'a>(columns: Vec<(Element<'a, Message>, f32)>) -> Element<'a, Message> {
    let mut line = Row::new().spacing(8).align_y(Vertical::Center);
    for (cell, width) in columns {
        line = if width > 0.0 {
            line.push(container(cell).width(Length::Fixed(width)))
        } else {
            line.push(container(cell).width(Length::Fill))
        };
    }
    line.into()
}

/// The headings over one table, in the same columns its rows use.
pub fn heads<'a>(columns: Vec<(&str, f32)>, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    cells(
        columns
            .into_iter()
            .map(|(label, width)| (widgets::section_label(label, tokens), width))
            .collect(),
    )
}

/// The handle a row is dragged by. Disabled when the reader may not move that
/// row, which is what a role above their own highest reads as.
pub fn grip<'a>(
    item: DragItem,
    enabled: bool,
    tip: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let color = if enabled {
        tokens.text_muted
    } else {
        tokens.divider
    };
    let handle = container(icons::icon(Icon::Drag, GRIP, color)).padding(2.0);
    if !enabled {
        return widgets::tooltip_of(handle, tip, tooltip::Position::Right, tokens);
    }
    widgets::tooltip_of(
        mouse_area(handle).on_press(Message::Admin(AdminMsg::Drag(DragMsg::DragStart(item)))),
        tip,
        tooltip::Position::Right,
        tokens,
    )
}

/// The two arrows that do what a drag does, from the keyboard.
pub fn arrows<'a>(
    item: DragItem,
    up: bool,
    down: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    row![
        widgets::icon_button(
            Icon::ArrowUp,
            "Move up",
            up.then(|| Message::Admin(admin::move_item(item, true))),
            tokens,
        ),
        widgets::icon_button(
            Icon::ArrowDown,
            "Move down",
            down.then(|| Message::Admin(admin::move_item(item, false))),
            tokens,
        ),
    ]
    .spacing(2)
    .align_y(Vertical::Center)
    .into()
}

/// Where a drag would drop what it is holding. Only drawn while something is in
/// flight: the hovered one is a line in the accent, the rest are the gap between
/// two rows.
pub fn drop_slot<'a>(
    slot: DragSlot,
    hovered: bool,
    indent: f32,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let accent = tokens.accent;
    let line = container(Space::new())
        .width(Length::Fill)
        .height(if hovered { SLOT_LINE } else { 0.0 })
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(accent)),
            border: border::rounded(SLOT_LINE / 2.0),
            ..container::Style::default()
        });

    mouse_area(
        container(row![Space::new().width(indent), line])
            .padding([SLOT_GAP, 0.0])
            .width(Length::Fill),
    )
    .on_enter(Message::Admin(AdminMsg::Drag(DragMsg::DragOver(slot))))
    .on_release(Message::Admin(AdminMsg::Drag(DragMsg::DragEnd)))
    .into()
}

/// The ghost of what is being dragged. It sits under the list rather than over
/// the pointer: a page nested in this scrollable does not know where on the
/// window it starts, and a banner above the rows would shift them out from under
/// the pointer mid-drag.
pub fn drag_banner<'a>(label: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    container(
        row![
            icons::icon(Icon::Drag, GRIP, tokens.accent),
            text(format!("Moving {label} — drop it on a line, Esc cancels"))
                .size(TEXT_BADGE)
                .color(tokens.text_secondary),
        ]
        .spacing(6)
        .align_y(Vertical::Center),
    )
    .padding([4.0, 10.0])
    .width(Length::Fill)
    .style(styles::container::chip(tokens))
    .into()
}
