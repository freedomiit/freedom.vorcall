//! The user settings area: the tab list on the left, one page on the right.
//!
//! The frame, the tab list and the way out are here, along with the row shapes
//! every page is built from — a labelled field, a toggle with its explanation and
//! a row of exclusive choices.

pub mod account;
pub mod appearance;
pub mod keybinds;
pub mod notifications;
pub mod profile;
pub mod voice;

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, row, scrollable, text, toggler};
use iced::{Element, Length};

use crate::app::message::{AuthMsg, Message, SettingsMsg};
use crate::app::state::settings::{ServerTab, SettingsTab};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::{ThemeTokens, styles};
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_PAGE, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, bold};

/// How wide the tab list is, and how wide a page's own content may grow.
const TABS_WIDTH: f32 = 220.0;
pub const CONTENT_WIDTH: f32 = 720.0;
/// The icon beside one tab.
const TAB_ICON: f32 = 16.0;

pub fn view<'a>(app: &'a App, main: &'a MainState, tab: SettingsTab) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let page = match tab {
        SettingsTab::Account => account::view(app, main),
        SettingsTab::Profile => profile::view(app, main),
        SettingsTab::Voice => voice::view(app, main),
        SettingsTab::Notifications => notifications::view(app, main),
        SettingsTab::Appearance => appearance::view(app, main),
        SettingsTab::Keybinds => keybinds::view(app, main),
    };

    row![
        tabs(app, main, tab),
        container(
            scrollable(
                column![
                    row![
                        text(tab.label())
                            .size(TEXT_PAGE)
                            .font(bold())
                            .color(tokens.text_primary),
                        Space::new().width(Length::Fill),
                        widgets::key_hint("Esc", tokens),
                        widgets::icon_button(
                            Icon::Close,
                            "Close",
                            Some(Message::Settings(SettingsMsg::Close)),
                            tokens,
                        ),
                    ]
                    .spacing(8)
                    .align_y(Vertical::Center),
                    page,
                ]
                .spacing(24)
                .padding(24)
                .max_width(CONTENT_WIDTH),
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

/// The tab list: the user's own pages, the server's, and the way out.
fn tabs<'a>(app: &'a App, main: &'a MainState, current: SettingsTab) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let mut list = column![widgets::section_label("User settings", tokens)]
        .spacing(2)
        .padding(12);
    for tab in SettingsTab::ALL {
        list = list.push(entry(
            tokens,
            tab_icon(tab),
            tab.label(),
            tab == current,
            Message::Settings(SettingsMsg::Tab(tab)),
        ));
    }

    // A page nobody may open is not drawn at all, so the group can be empty.
    let perms = main.server.resolve(main.server.me, None);
    let server: Vec<ServerTab> = ServerTab::ALL
        .into_iter()
        .filter(|tab| tab.allowed(perms))
        .collect();
    if !server.is_empty() {
        list = list.push(Space::new().height(12.0));
        list = list.push(widgets::section_label("Server settings", tokens));
        for tab in server {
            list = list.push(entry(
                tokens,
                server_tab_icon(tab),
                tab.label(),
                false,
                Message::Settings(SettingsMsg::ServerTab(tab)),
            ));
        }
    }

    list = list.push(Space::new().height(Length::Fill));
    list = list.push(entry(
        tokens,
        Icon::Logout,
        "Log out",
        false,
        Message::Auth(AuthMsg::Logout),
    ));

    container(list)
        .width(TABS_WIDTH)
        .height(Length::Fill)
        .style(styles::container::sidebar(tokens))
        .into()
}

fn entry<'a>(
    tokens: &'a ThemeTokens,
    glyph: Icon,
    label: &str,
    selected: bool,
    message: Message,
) -> Element<'a, Message> {
    let color = if selected {
        tokens.text_primary
    } else {
        tokens.text_secondary
    };
    button(
        row![
            icons::icon(glyph, TAB_ICON, color),
            text(label.to_owned()).size(TEXT_ROW),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    )
    .width(Length::Fill)
    .padding([5.0, 8.0])
    .style(styles::button::row_for(tokens, selected))
    .on_press(message)
    .into()
}

fn tab_icon(tab: SettingsTab) -> Icon {
    match tab {
        SettingsTab::Account => Icon::Gear,
        SettingsTab::Profile => Icon::User,
        SettingsTab::Voice => Icon::Mic,
        SettingsTab::Notifications => Icon::Bell,
        SettingsTab::Appearance => Icon::Palette,
        SettingsTab::Keybinds => Icon::Keyboard,
    }
}

fn server_tab_icon(tab: ServerTab) -> Icon {
    match tab {
        ServerTab::Overview => Icon::Gear,
        ServerTab::Channels => Icon::Hash,
        ServerTab::Roles => Icon::Shield,
        ServerTab::Members => Icon::Users,
        ServerTab::Invites => Icon::Link,
        ServerTab::Bans => Icon::Ban,
    }
}

/// One group of a page: its heading and the rows under it.
pub fn section<'a>(
    title: &str,
    tokens: &'a ThemeTokens,
    rows: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut group = column![
        text(title.to_owned())
            .size(TEXT_SECTION)
            .font(bold())
            .color(tokens.text_primary),
    ]
    .spacing(12)
    .width(Length::Fill);
    for row in rows {
        group = group.push(row);
    }
    group.into()
}

/// A control under its label, with the sentence that explains it underneath.
pub fn field<'a>(
    label: &str,
    control: impl Into<Element<'a, Message>>,
    help: Option<&str>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let mut stack = column![widgets::section_label(label, tokens), control.into(),]
        .spacing(6)
        .width(Length::Fill);
    if let Some(help) = help {
        stack = stack.push(
            text(help.to_owned())
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }
    stack.into()
}

/// A preference that is on or off: what it is and what it does on the left, the
/// switch on the right.
pub fn toggle_row<'a>(
    label: &str,
    help: &str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let mut explained = column![
        text(label.to_owned())
            .size(TEXT_ROW)
            .color(tokens.text_primary),
    ]
    .spacing(2);
    if !help.is_empty() {
        explained = explained.push(
            text(help.to_owned())
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }

    row![
        explained.width(Length::Fill),
        toggler(value)
            .on_toggle(on_toggle)
            .style(styles::toggler(tokens)),
    ]
    .spacing(12)
    .align_y(Vertical::Center)
    .into()
}

/// A row of exclusive choices, as the pressed one against the rest. A pick list
/// would hide three options behind a click.
pub fn choices<'a, T: PartialEq + Copy + 'a>(
    options: &[(T, &str)],
    selected: T,
    on_pick: impl Fn(T) -> Message,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let mut group = row![].spacing(8).align_y(Vertical::Center);
    for (value, label) in options {
        let chosen = *value == selected;
        let control = button(text((*label).to_owned()).size(TEXT_ROW)).padding([6.0, 12.0]);
        group = group.push(if chosen {
            control.style(styles::button::primary(tokens))
        } else {
            control
                .style(styles::button::secondary(tokens))
                .on_press(on_pick(*value))
        });
    }
    group.into()
}

/// The counter under a field with a limit, which turns into a warning as the
/// limit is reached.
pub fn counter<'a>(used: usize, max: usize, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let color = if used >= max {
        tokens.warning
    } else {
        tokens.text_muted
    };
    text(format!("{used}/{max}"))
        .size(TEXT_BADGE)
        .color(color)
        .into()
}
