//! The quick switcher: one field, and whatever it matches.
//!
//! The matches are worked out the same way `update::ui` works them out, so the
//! row the keyboard is on is the row that is drawn. A row's own action is wrapped
//! in [`overlays::perform`], which closes the switcher before it runs.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Id, Space, button, column, container, mouse_area, opaque, row, stack, text, text_input,
};
use iced::{Element, Length};

use crate::app::message::{ChannelsMsg, Message, UiMsg, VoiceMsg};
use crate::app::state::ui::{SwitchEntry, SwitchTarget, rank_switcher, switcher_entries};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::widgets;
use crate::view::{QUICK_SWITCHER_ID, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, overlays};

const SWITCHER_WIDTH: f32 = 560.0;
/// How far down the window the field sits.
const SWITCHER_TOP: f32 = 80.0;
const ROW_ICON: f32 = 14.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let switcher = &app.ui.quick_switcher;
    let matches = rank_switcher(
        switcher_entries(main, &app.config.hidden_dms),
        &switcher.query,
    );
    let selected = switcher.selected.min(matches.len().saturating_sub(1));

    let field = row![
        icons::icon(Icon::Search, 16.0, tokens.text_muted),
        text_input("Where to?", &switcher.query)
            .id(Id::new(QUICK_SWITCHER_ID))
            .on_input(|value| Message::Ui(UiMsg::QuickSwitcherQuery(value)))
            .on_submit(Message::Ui(UiMsg::QuickSwitcherPick))
            .padding(10)
            .size(TEXT_BODY)
            .style(styles::text_input(tokens)),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    let mut body = column![field].spacing(10).width(Length::Fill);
    if matches.is_empty() {
        body = body.push(
            text("Nothing matches")
                .size(TEXT_ROW)
                .color(tokens.text_muted),
        );
    } else {
        let mut rows = column![].spacing(2).width(Length::Fill);
        for (index, entry) in matches.iter().enumerate() {
            rows = rows.push(row_of(tokens, entry, index == selected));
        }
        body = body.push(rows);
    }
    body = body.push(hints(tokens));

    let card = container(body)
        .width(SWITCHER_WIDTH)
        .padding(16)
        .style(styles::container::popover(tokens));

    let backdrop = mouse_area(
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(styles::container::backdrop(tokens)),
    )
    .on_press(Message::Ui(UiMsg::CloseQuickSwitcher));

    opaque(
        stack![
            backdrop,
            container(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Horizontal::Center)
                .padding([SWITCHER_TOP, 0.0]),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
}

/// One match: what it is, what it is called, and where it lives.
fn row_of<'a>(
    tokens: &'a ThemeTokens,
    entry: &SwitchEntry,
    selected: bool,
) -> Element<'a, Message> {
    let (icon, message) = match entry.target {
        SwitchTarget::Channel(id) => (Icon::Hash, Message::Channels(ChannelsMsg::Select(id))),
        SwitchTarget::Voice(id) => (Icon::Speaker, Message::Voice(VoiceMsg::Join(id))),
        SwitchTarget::Dm(id) => (Icon::User, Message::Channels(ChannelsMsg::Select(id))),
        SwitchTarget::Member(user_id) => {
            (Icon::User, Message::Channels(ChannelsMsg::OpenDm(user_id)))
        }
    };

    let mut line = row![
        icons::icon(icon, ROW_ICON, tokens.text_muted),
        text(entry.name.clone())
            .size(TEXT_ROW)
            .color(tokens.text_primary),
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if !entry.detail.is_empty() {
        line = line.push(
            text(entry.detail.clone())
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }
    line = line.push(Space::new().width(Length::Fill));
    line = line.push(widgets::badge(entry.unread, tokens));
    // The hint marks the row Enter would take, which is the row the keyboard is
    // on.
    if selected {
        line = line.push(widgets::key_hint("↵", tokens));
    }

    button(line)
        .width(Length::Fill)
        .padding([6.0, 8.0])
        .style(styles::button::row_for(tokens, selected))
        .on_press(overlays::perform(message))
        .into()
}

/// What the keyboard does here, spelled out along the bottom.
fn hints<'a>(tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let pair = |keys: Vec<&'static str>, what: &'static str| {
        let mut line = row![].spacing(3).align_y(Vertical::Center);
        for key in keys {
            line = line.push(widgets::key_hint(key, tokens));
        }
        line.push(text(what).size(TEXT_SECONDARY).color(tokens.text_muted))
    };

    row![
        pair(vec!["↑", "↓"], "navigate"),
        pair(vec!["Enter"], "open"),
        pair(vec!["Esc"], "close"),
    ]
    .spacing(12)
    .align_y(Vertical::Center)
    .into()
}
