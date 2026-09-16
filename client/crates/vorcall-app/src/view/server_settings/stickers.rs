//! The Stickers page: the shared library, and the picture being added.
//!
//! The library is server-wide — there is nothing per channel or per member in it
//! — so this page is the whole of it. A reader without `MANAGE_STICKERS` never
//! gets here: `ServerTab::Stickers` is not in their tab list and
//! `server_settings::view` shows them the door.

use iced::alignment::Vertical;
use iced::widget::{Column, Space, container, image, row, text, text_input};
use iced::{ContentFit, Element, Length};
use vorcall_core::Sticker;

use crate::app::message::{Message, StickerMsg, UiMsg};
use crate::app::state::chat::ImageState;
use crate::app::state::rules::format_bytes;
use crate::app::state::ui::{Dialog, validate_name};
use crate::app::{App, MainState};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{Kind, action, card, cells, heads, hint};
use crate::view::{TEXT_BADGE, TEXT_ROW};
use crate::workers::images::ImageKey;

/// The table's columns; the name field fills whatever is left.
const THUMB: f32 = 40.0;
const SIZE: f32 = 80.0;
const ADDED_BY: f32 = 120.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    Column::new()
        .push(add(app, main))
        .push(list(app, main))
        .spacing(16)
        .width(Length::Fill)
        .into()
}

/// The button that opens the pick-and-upload chain.
fn add<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let busy = main.sticker.uploading;
    card(
        "Add a sticker",
        vec![
            action(
                Kind::Primary,
                if busy {
                    "Uploading…"
                } else {
                    "Choose a file…"
                },
                (!busy).then_some(Message::Sticker(StickerMsg::PickFile)),
                if busy { "One at a time" } else { "" },
                tokens,
            ),
            hint(
                "A PNG, JPEG, GIF or WebP of at most 1 MiB. It is drawn at 160 pixels in a message.",
                tokens,
            ),
        ],
        tokens,
    )
}

/// Every sticker in the library.
fn list<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let stickers = main.sticker.ordered();

    let mut rows: Vec<Element<'_, Message>> = vec![
        text(format!("{} stickers", stickers.len()))
            .size(TEXT_BADGE)
            .color(tokens.text_muted)
            .into(),
    ];

    if stickers.is_empty() {
        rows.push(hint("Nothing in the library yet.", tokens));
    } else {
        rows.push(heads(
            vec![
                ("", THUMB),
                ("Name", 0.0),
                ("Size", SIZE),
                ("Added by", ADDED_BY),
                ("", 0.0),
            ],
            tokens,
        ));
        for sticker in stickers {
            rows.push(sticker_row(app, main, sticker));
        }
    }

    card("Stickers", rows, tokens)
}

/// One sticker: what it looks like, its name in a field that renames it, and the
/// way to remove it.
fn sticker_row<'a>(
    app: &'a App,
    main: &'a MainState,
    sticker: &'a Sticker,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let sticker_id = sticker.id;
    // No draft means nothing is being typed: the field shows the stored name.
    let draft = main
        .admin
        .sticker_names
        .get(&sticker_id)
        .map_or(sticker.name.as_str(), String::as_str);
    let checked = validate_name(draft, "sticker name");
    let renamed = checked.as_deref().is_ok_and(|name| name != sticker.name);
    let refusal = checked.err().unwrap_or_default();

    let name = text_input("Name", draft)
        .on_input(move |typed| Message::Sticker(StickerMsg::RenameDraft(sticker_id, typed)))
        .on_submit(Message::Sticker(StickerMsg::RenameSave(sticker_id)))
        .size(TEXT_ROW)
        .padding([4.0, 8.0])
        .width(Length::Fill)
        .style(styles::text_input(tokens));

    let uploader = if sticker.uploader_id == 0 {
        "—".to_owned()
    } else {
        main.server.display_name(sticker.uploader_id).to_owned()
    };

    container(cells(vec![
        (thumbnail(main, sticker_id), THUMB),
        (name.into(), 0.0),
        (
            cell(&format_bytes(sticker.size.max(0) as u64), tokens),
            SIZE,
        ),
        (cell(&uploader, tokens), ADDED_BY),
        (
            row![
                action(
                    Kind::Secondary,
                    "Rename",
                    renamed.then_some(Message::Sticker(StickerMsg::RenameSave(sticker_id))),
                    &refusal,
                    tokens,
                ),
                action(
                    Kind::Danger,
                    "Delete",
                    Some(Message::Ui(UiMsg::OpenDialog(
                        Dialog::ConfirmDeleteSticker { sticker_id },
                    ))),
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

/// The picture itself, from the window's own image cache. A row whose bytes have
/// not landed keeps the cell's size rather than letting the table jump.
fn thumbnail<'a>(main: &'a MainState, sticker_id: i64) -> Element<'a, Message> {
    match main.chat.images.get(&ImageKey::Sticker(sticker_id)) {
        Some(ImageState::Ready(handle)) => image(handle.clone())
            .content_fit(ContentFit::Contain)
            .width(THUMB)
            .height(THUMB)
            .into(),
        _ => Space::new().width(THUMB).height(THUMB).into(),
    }
}

fn cell<'a>(value: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text(value.to_owned())
        .size(TEXT_BADGE)
        .color(tokens.text_secondary)
        .into()
}
