//! The Sounds page: the shared soundpad library, and the clip being added.
//!
//! The library is server-wide — there is nothing per channel or per member in it
//! — so this page is the whole of it. A reader without `MANAGE_SOUNDS` never
//! gets here: `ServerTab::Sounds` is not in their tab list and
//! `server_settings::view` shows them the door.

use iced::alignment::Vertical;
use iced::widget::{Column, container, row, text, text_input};
use iced::{Element, Length};
use vorcall_core::Sound;

use crate::app::message::{Message, SoundMsg, UiMsg};
use crate::app::state::rules::format_bytes;
use crate::app::state::sound::clock;
use crate::app::state::ui::{Dialog, validate_name};
use crate::app::{App, MainState};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, cells, heads, hint};
use crate::view::{TEXT_BADGE, TEXT_ROW};

/// The table's columns; the name field fills whatever is left.
const LENGTH: f32 = 70.0;
const SIZE: f32 = 80.0;
const ADDED_BY: f32 = 120.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    Column::new()
        .push(add(app))
        .push(list(app, main))
        .spacing(16)
        .width(Length::Fill)
        .into()
}

/// The button that opens the pick-trim-upload chain.
fn add<'a>(app: &'a App) -> Element<'a, Message> {
    let tokens = &app.tokens;
    card(
        "Add a clip",
        vec![
            action(
                Kind::Primary,
                "Choose a file…",
                Some(Message::Sound(SoundMsg::Pick)),
                "",
                tokens,
            ),
            hint(
                "Any audio file this machine can read. You pick the part of it that becomes the clip.",
                tokens,
            ),
        ],
        tokens,
    )
}

/// Every clip in the library.
fn list<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let clips = main.sound.ordered();

    let mut rows: Vec<Element<'_, Message>> = vec![
        text(format!("{} clips", clips.len()))
            .size(TEXT_BADGE)
            .color(tokens.text_muted)
            .into(),
    ];

    if clips.is_empty() {
        rows.push(hint("Nothing in the soundpad yet.", tokens));
    } else {
        rows.push(heads(
            vec![
                ("Name", 0.0),
                ("Length", LENGTH),
                ("Size", SIZE),
                ("Added by", ADDED_BY),
                ("", 0.0),
            ],
            tokens,
        ));
        for clip in clips {
            rows.push(clip_row(app, main, clip));
        }
    }

    card("Soundpad", rows, tokens)
}

/// One clip: its name in a field that renames it, what it is, and the way to
/// remove it.
fn clip_row<'a>(app: &'a App, main: &'a MainState, clip: &'a Sound) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let sound_id = clip.id;
    // No draft means nothing is being typed: the field shows the stored name.
    let draft = main
        .admin
        .sound_names
        .get(&sound_id)
        .map_or(clip.name.as_str(), String::as_str);
    let checked = validate_name(draft, "clip name");
    let renamed = checked.as_deref().is_ok_and(|name| name != clip.name);
    let refusal = checked.err().unwrap_or_default();

    let name = text_input("Name", draft)
        .on_input(move |typed| Message::Sound(SoundMsg::RenameDraft(sound_id, typed)))
        .on_submit(Message::Sound(SoundMsg::RenameSave(sound_id)))
        .size(TEXT_ROW)
        .padding([4.0, 8.0])
        .width(Length::Fill)
        .style(styles::text_input(tokens));

    let uploader = if clip.uploader_id == 0 {
        "—".to_owned()
    } else {
        main.server.display_name(clip.uploader_id).to_owned()
    };

    container(cells(vec![
        (name.into(), 0.0),
        (cell(&clock(clip.duration_ms), tokens), LENGTH),
        (cell(&format_bytes(clip.size.max(0) as u64), tokens), SIZE),
        (cell(&uploader, tokens), ADDED_BY),
        (
            row![
                action(
                    Kind::Secondary,
                    "Rename",
                    renamed.then_some(Message::Sound(SoundMsg::RenameSave(sound_id))),
                    &refusal,
                    tokens,
                ),
                action(
                    Kind::Danger,
                    "Delete",
                    Some(Message::Ui(UiMsg::OpenDialog(Dialog::ConfirmDeleteSound {
                        sound_id,
                    }))),
                    "",
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center)
            .into(),
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
