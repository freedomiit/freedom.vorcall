//! The Notifications page: the three switches, and the channels that are silent.

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, row, text};
use iced::{Element, Length};

use crate::app::message::{Message, SettingsMsg};
use crate::app::update::settings::{muted_channels, unmute};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::settings::{section, toggle_row};
use crate::view::{TEXT_ROW, TEXT_SECONDARY};

/// The bell beside a muted row.
const ROW_ICON: f32 = 14.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let config = &app.config;

    column![
        section(
            "Notifications",
            tokens,
            vec![
                toggle_row(
                    "Desktop notifications",
                    "A popup for a mention or a direct message while you are looking elsewhere.",
                    config.notifications,
                    |on| Message::Settings(SettingsMsg::SetNotifications(on)),
                    tokens,
                ),
                toggle_row(
                    "Notification sound",
                    "A chime with the popup.",
                    config.sound,
                    |on| Message::Settings(SettingsMsg::SetSound(on)),
                    tokens,
                ),
                toggle_row(
                    "Ignore @everyone and @here",
                    "Only a mention of you by name raises a notification.",
                    config.suppress_everyone,
                    |on| Message::Settings(SettingsMsg::SetSuppressEveryone(on)),
                    tokens,
                ),
            ],
        ),
        section("Muted channels", tokens, vec![muted(app, main)]),
    ]
    .spacing(24)
    .width(Length::Fill)
    .into()
}

/// Every channel whose messages raise nothing, each with the way back.
fn muted<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let rows = muted_channels(app, main);
    if rows.is_empty() {
        return text("No channel is muted.")
            .size(TEXT_SECONDARY)
            .color(tokens.text_muted)
            .into();
    }

    let mut list = column![].spacing(4).width(Length::Fill);
    for (channel_id, title) in rows {
        list = list.push(
            container(
                row![
                    icons::icon(Icon::BellOff, ROW_ICON, tokens.text_muted),
                    text(title).size(TEXT_ROW).color(tokens.text_primary),
                    Space::new().width(Length::Fill),
                    button(text("Unmute").size(TEXT_ROW))
                        .padding([4.0, 10.0])
                        .style(styles::button::secondary(tokens))
                        .on_press(unmute(channel_id)),
                ]
                .spacing(8)
                .align_y(Vertical::Center),
            )
            .padding([6.0, 10.0])
            .width(Length::Fill)
            .style(styles::container::card(tokens)),
        );
    }
    list.into()
}
