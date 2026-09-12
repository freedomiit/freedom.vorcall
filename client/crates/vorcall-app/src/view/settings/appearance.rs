//! The Appearance page: the theme picker, the three layout preferences, and the
//! editor that writes a theme of one's own.
//!
//! The editor draws from the draft, not from the window's tokens: nothing it is
//! doing reaches the window until Save.

use std::fmt;

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, pick_list, row, slider, text, text_input};
use iced::{Background, Border, Color, Element, Length, Theme, border};
use vorcall_core::config::{Density, Entrance, FONT_SCALE_MAX, FONT_SCALE_MIN};

use crate::app::message::{Message, SettingsMsg};
use crate::app::state::settings::{ThemeDraft, ThemeEntry};
use crate::app::update::settings::theme_name;
use crate::app::{App, MainState};
use crate::theme::{ThemeTokens, styles, tokens};
use crate::view::settings::{choices, field, section, toggle_row};
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, bold};

/// One theme card, and the strip of colours across the top of it.
const CARD_WIDTH: f32 = 160.0;
const CARD_PREVIEW: f32 = 72.0;
/// The font-scale slider.
const SLIDER_WIDTH: f32 = 240.0;
/// One token row: its swatch, and the field beside it.
const TOKEN_SWATCH: f32 = 22.0;
const TOKEN_FIELD: f32 = 110.0;
/// The editor panel.
const EDITOR_WIDTH: f32 = 400.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let config = &app.config;

    let scale = row![
        slider(
            FONT_SCALE_MIN..=FONT_SCALE_MAX,
            config.font_scale,
            |value| { Message::Settings(SettingsMsg::SetFontScale(value)) }
        )
        .step(0.05_f32)
        .on_release(Message::Settings(SettingsMsg::FontScaleReleased))
        .width(SLIDER_WIDTH)
        .style(styles::slider(tokens)),
        text(format!("{:.0}%", config.font_scale * 100.0))
            .size(TEXT_ROW)
            .color(tokens.text_secondary),
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    let entrance = pick_list(
        EntranceChoice::ALL.to_vec(),
        Some(EntranceChoice(config.entrance)),
        |choice| Message::Settings(SettingsMsg::SetEntrance(choice.0)),
    )
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens));

    let page = column![
        section("Theme", tokens, vec![themes(app, main)]),
        section(
            "Layout",
            tokens,
            vec![
                field(
                    "Message density",
                    choices(
                        &[(Density::Cosy, "Cosy"), (Density::Compact, "Compact")],
                        config.density,
                        |density| Message::Settings(SettingsMsg::SetDensity(density)),
                        tokens,
                    ),
                    None,
                    tokens,
                ),
                field(
                    "Font scale",
                    scale,
                    Some("Scales the whole interface, text included."),
                    tokens,
                ),
                field("Entrance animation", entrance, None, tokens),
                toggle_row(
                    "Reactions as text",
                    "Shows +1, fire and eyes instead of emoji, for a machine whose fonts cannot draw them.",
                    config.text_reactions,
                    |on| Message::Settings(SettingsMsg::SetTextReactions(on)),
                    tokens,
                ),
            ],
        ),
    ]
    .spacing(24)
    .width(Length::Fill);

    row![page, editor(app, main)]
        .spacing(24)
        .width(Length::Fill)
        .into()
}

/// The two presets, then every custom theme that reads as one. The theme in force
/// carries the accent border.
fn themes<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let mut cards = row![].spacing(12);
    for entry in &main.settings.themes {
        cards = cards.push(card(app, entry));
    }
    // A friend with several themes of their own gets a second line rather than a
    // row running off the page.
    cards.wrap().into()
}

/// One theme as a card: four of its surfaces, its name, and what it is.
fn card<'a>(app: &'a App, entry: &'a ThemeEntry) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let selected = app.config.theme == entry.theme;

    let strip = row![
        stripe(entry.tokens.bg_sidebar),
        stripe(entry.tokens.bg_chat),
        stripe(entry.tokens.accent),
        stripe(entry.tokens.text_primary),
    ]
    .height(CARD_PREVIEW);

    let name = row![
        text(entry.name.as_str())
            .size(TEXT_ROW)
            .color(tokens.text_primary),
        Space::new().width(Length::Fill),
        text(entry.detail.as_str())
            .size(TEXT_BADGE)
            .color(tokens.text_muted),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    button(
        column![strip, container(name).padding([8.0, 10.0])]
            .width(CARD_WIDTH)
            .clip(true),
    )
    .padding(0.0)
    .style(outlined(*tokens, selected))
    .on_press(Message::Settings(SettingsMsg::SetTheme(
        entry.theme.clone(),
    )))
    .into()
}

fn stripe(color: Color) -> Element<'static, Message> {
    container(Space::new())
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme: &Theme| iced::widget::container::Style {
            background: Some(Background::Color(color)),
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// The card's own frame: the accent while it is the theme in force.
fn outlined(
    tokens: ThemeTokens,
    selected: bool,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    move |_theme, status| iced::widget::button::Style {
        background: Some(Background::Color(tokens.bg_elevated)),
        text_color: tokens.text_primary,
        border: Border {
            color: match (selected, status) {
                (true, _) => tokens.accent,
                (false, iced::widget::button::Status::Hovered) => tokens.border_strong,
                (false, _) => tokens.border_subtle,
            },
            width: 2.0,
            radius: styles::RADIUS_CARD.into(),
        },
        ..iced::widget::button::Style::default()
    }
}

/// The theme editor: every token of the draft, grouped as the token table is, with
/// a live card above them and the four things that can be done with the result.
fn editor<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.settings.theme;

    let name = text_input("Theme name", &draft.name)
        .padding(8)
        .width(Length::Fill)
        .style(styles::text_input(tokens))
        .on_input(|value| Message::Settings(SettingsMsg::ThemeEditorName(value)));

    let mut panel = column![
        text(format!("Theme editor · {}", theme_name(&draft.base)))
            .size(TEXT_SECTION)
            .font(bold())
            .color(tokens.text_primary),
        text("Every colour the window uses. Save puts the result in force.")
            .size(TEXT_SECONDARY)
            .color(tokens.text_muted),
        sample(draft),
        name,
    ]
    .spacing(12)
    .width(Length::Fill);

    let mut group = "";
    for (heading, token) in tokens::TOKEN_NAMES {
        if heading != group {
            group = heading;
            panel = panel.push(widgets::section_label(heading, tokens));
        }
        panel = panel.push(token_row(app, draft, token));
    }

    if let Some(error) = &draft.error {
        panel = panel.push(
            text(error.clone())
                .size(TEXT_SECONDARY)
                .color(tokens.danger),
        );
    }

    panel = panel.push(
        row![
            action("Save", SettingsMsg::ThemeSave, true, tokens),
            action("Save as…", SettingsMsg::ThemeSaveAs, false, tokens),
            action("Export", SettingsMsg::ThemeExport, false, tokens),
            action("Import", SettingsMsg::ThemeImport, false, tokens),
        ]
        .spacing(8),
    );

    container(panel)
        .width(EDITOR_WIDTH)
        .padding(16)
        .style(styles::container::card(tokens))
        .into()
}

fn action<'a>(
    label: &str,
    message: SettingsMsg,
    primary: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let control = button(text(label.to_owned()).size(TEXT_ROW))
        .padding([6.0, 12.0])
        .on_press(Message::Settings(message));
    if primary {
        control.style(styles::button::primary(tokens)).into()
    } else {
        control.style(styles::button::secondary(tokens)).into()
    }
}

/// One token: its name, its colour, and the hex that is being typed for it.
fn token_row<'a>(app: &'a App, draft: &'a ThemeDraft, token: &'static str) -> Element<'a, Message> {
    let theme = &app.tokens;
    let invalid = draft.invalid(token);
    let color = draft.tokens.field(token).unwrap_or(Color::TRANSPARENT);

    let swatch = container(Space::new())
        .width(TOKEN_SWATCH)
        .height(TOKEN_SWATCH)
        .style(move |_theme: &Theme| iced::widget::container::Style {
            background: Some(Background::Color(color)),
            border: border::rounded(styles::RADIUS_CHIP)
                .width(1.0)
                .color(theme.border_strong),
            ..iced::widget::container::Style::default()
        });

    // An invalid hex outlines the field in danger; the field styles from this copy
    // either way, so both branches hand `text_input` the same kind of tokens.
    let field_tokens = if invalid {
        ThemeTokens {
            border_subtle: theme.danger,
            border_strong: theme.danger,
            accent: theme.danger,
            ..*theme
        }
    } else {
        *theme
    };
    let hex = text_input("#RRGGBB", draft.typed(token))
        .padding([4.0, 8.0])
        .width(TOKEN_FIELD)
        .size(TEXT_SECONDARY)
        .style(styles::text_input(&field_tokens))
        .on_input(move |value| {
            Message::Settings(SettingsMsg::ThemeEditorToken(token.to_owned(), value))
        });

    row![
        text(token).size(TEXT_SECONDARY).color(theme.text_secondary),
        Space::new().width(Length::Fill),
        swatch,
        hex,
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

/// What the draft looks like, in the shapes the window is made of.
fn sample(draft: &ThemeDraft) -> Element<'_, Message> {
    let theme = draft.tokens;

    let rows = column![
        text("Channel title")
            .size(TEXT_BODY)
            .font(bold())
            .color(theme.text_primary),
        text("A message, as somebody else wrote it.")
            .size(TEXT_SECONDARY)
            .color(theme.text_secondary),
        row![
            container(text("Accent").size(TEXT_BADGE).color(theme.text_on_accent))
                .padding([2.0, 8.0])
                .style(move |_theme: &Theme| iced::widget::container::Style {
                    background: Some(Background::Color(theme.accent)),
                    border: border::rounded(styles::RADIUS_CHIP),
                    ..iced::widget::container::Style::default()
                }),
            container(text("Online").size(TEXT_BADGE).color(theme.text_on_accent))
                .padding([2.0, 8.0])
                .style(move |_theme: &Theme| iced::widget::container::Style {
                    background: Some(Background::Color(theme.online)),
                    border: border::rounded(styles::RADIUS_CHIP),
                    ..iced::widget::container::Style::default()
                }),
        ]
        .spacing(6),
    ]
    .spacing(6);

    container(
        row![
            container(Space::new())
                .width(36.0)
                .height(Length::Fill)
                .style(move |_theme: &Theme| iced::widget::container::Style {
                    background: Some(Background::Color(theme.bg_sidebar)),
                    ..iced::widget::container::Style::default()
                }),
            container(rows).padding(10).width(Length::Fill),
        ]
        .height(92.0),
    )
    .width(Length::Fill)
    .clip(true)
    .style(move |_theme: &Theme| iced::widget::container::Style {
        background: Some(Background::Color(theme.bg_chat)),
        border: border::rounded(styles::RADIUS_CARD)
            .width(1.0)
            .color(theme.border_subtle),
        ..iced::widget::container::Style::default()
    })
    .into()
}

/// The entrance animations, as the pick list spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntranceChoice(Entrance);

impl EntranceChoice {
    const ALL: [Self; 4] = [
        Self(Entrance::Random),
        Self(Entrance::Wink),
        Self(Entrance::Rare),
        Self(Entrance::Off),
    ];
}

impl fmt::Display for EntranceChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Entrance::Random => "Random",
            Entrance::Wink => "Wink",
            Entrance::Rare => "The rare one",
            Entrance::Off => "Off",
        })
    }
}
