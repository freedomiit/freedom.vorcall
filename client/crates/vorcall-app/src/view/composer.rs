//! The message input: a multi-line editor where Enter sends and Shift+Enter
//! breaks the line, with everything the next send will carry above it.

use iced::alignment::Vertical;
use iced::widget::text_editor::{Binding, KeyPress};
use iced::widget::{
    Id, Space, button, column, container, image, keyed, progress_bar, row, scrollable, text,
    text_editor, tooltip,
};
use iced::{Element, Length, Padding, keyboard};
use vorcall_core::mentions::{self, PALETTE};
use vorcall_core::{Attachment, attachments, permissions};

use crate::app::message::{ChatMsg, MenuTarget, Message, StickerMsg, UiMsg};
use crate::app::state::chat::{
    COMPOSER_PALETTE, ImageState, MESSAGE_MAX_CHARS, Mention, PendingStream, PendingTransfer,
    TransferKind,
};
use crate::app::state::rules::{
    MentionCandidate, format_bytes, mention_candidates, plain_text, progress_fraction,
};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::{ThemeTokens, styles};
use crate::view::message::glyph_of;
use crate::view::widgets::{self, Metrics};
use crate::view::{COMPOSER_ID, TEXT_BADGE, TEXT_BODY, TEXT_SECONDARY};
use crate::workers::images::ImageKey;

/// How tall the composer is before it grows with the text, and how many lines it
/// grows to before it scrolls instead.
const MIN_EDITOR_HEIGHT: f32 = 28.0;
const MAX_LINES: f32 = 6.0;
/// How tall one line of the editor is, as a share of its text size.
const LINE_HEIGHT: f32 = 1.4;
/// How much of a quoted message the reply banner shows.
const EXCERPT_MAX: usize = 60;
/// The thumbnail a pending attachment is drawn as.
const THUMB: f32 = 40.0;
/// How wide the route chevron is: no text of its own, only the handle.
const ROUTES_WIDTH: f32 = 22.0;
/// The sticker picker's grid: how many to a row, how large each one is drawn,
/// and how tall the whole of it grows before it scrolls.
const PICKER_COLUMNS: usize = 6;
const PICKER_THUMB: f32 = 56.0;
const PICKER_HEIGHT: f32 = 220.0;
/// The bar on a chip whose file is still going up.
const CHIP_BAR_LENGTH: f32 = 72.0;
const CHIP_BAR_GIRTH: f32 = 4.0;

/// Bound on a suggestion's display name, so the `@handle` beside it keeps its
/// place next to the name instead of being pushed to the popover's right edge.
/// Sixteen em is about thirty Latin characters, comfortably past the common
/// case without letting the longest of the 32 a name may carry take the row.
const MENTION_NAME_MAX: f32 = TEXT_SECONDARY * 16.0;

/// The panel's slots, in the order they are drawn. The column is keyed by these
/// because a positional `Column` rebuilds `text_editor::State` — and drops the
/// focus mid-word — whenever a row above the editor appears or disappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Mentions,
    Banner,
    Warning,
    Strip,
    Palette,
    Stickers,
    Input,
    Footer,
}

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);
    let composer = &main.chat.composer;
    let channel = main.chat.current.channel_id();

    let allowed = |bit: u64| channel.is_some_and(|id| main.server.can(bit, Some(id)));
    let can_send = allowed(permissions::SEND_MESSAGES);
    let can_attach = allowed(permissions::ATTACH_FILES);
    let can_everyone = allowed(permissions::MENTION_EVERYONE);

    let mentions = composer
        .mention
        .as_ref()
        .and_then(|mention| mention_list(app, main, mention, can_everyone, metrics));
    // The popup is open only while it has rows to draw, which is what the
    // composer's key bindings answer Enter and the arrows on.
    let mention_open = mentions.is_some();
    let attachment_strip = (composer.slots_used() > 0).then(|| strip(app, main, metrics));
    let palette =
        (main.chat.reacting == Some(COMPOSER_PALETTE)).then(|| emoji_palette(app, metrics));
    // Gated on the same permission the send path is: a channel this account may
    // not write in offers no stickers either.
    let stickers = (can_send && main.sticker.picker_open).then(|| sticker_picker(app, main));

    let panel = keyed::Column::new()
        .spacing(6)
        .padding(Padding::ZERO.left(16.0).right(16.0).bottom(8.0))
        .width(Length::Fill)
        .push_maybe(Slot::Mentions, mentions)
        .push_maybe(Slot::Banner, banner(app, main, metrics))
        .push_maybe(
            Slot::Warning,
            everyone_warning(app, main, can_everyone, metrics),
        )
        .push_maybe(Slot::Strip, attachment_strip)
        .push_maybe(Slot::Palette, palette)
        .push_maybe(Slot::Stickers, stickers)
        .push(
            Slot::Input,
            input(app, main, can_send, can_attach, mention_open, metrics),
        )
        .push(Slot::Footer, footer(app, main, can_send, metrics));

    container(panel)
        .width(Length::Fill)
        .style(styles::container::chat(tokens))
        .into()
}

/// The box itself: the attach button, the editor, and what cannot go in it.
fn input<'a>(
    app: &'a App,
    main: &'a MainState,
    can_send: bool,
    can_attach: bool,
    mention_open: bool,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let composer = &main.chat.composer;
    let size = metrics.text(TEXT_BODY);

    let mut editor = text_editor(&composer.content)
        .id(Id::new(COMPOSER_ID))
        .placeholder(placeholder(main, can_send))
        .min_height(metrics.height(MIN_EDITOR_HEIGHT))
        .max_height(size * LINE_HEIGHT * MAX_LINES)
        .padding(0.0)
        .size(size)
        .line_height(LINE_HEIGHT)
        .style(styles::text_editor_bare(tokens));
    // An editor with no action to report is a disabled one, which is what a
    // channel this account may not write in gets.
    if can_send {
        let editing = composer.editing;
        let replying = composer.reply_to.is_some();
        let empty = composer.content.text().trim().is_empty();
        let last_own = main
            .chat
            .current
            .channel_id()
            .and_then(|id| main.chat.channel(id))
            .and_then(|channel| channel.last_own_message_id(main.member_id));

        editor = editor
            .on_action(|action| Message::Chat(ChatMsg::Editor(action)))
            .key_binding(move |press| {
                binding(press, editing, replying, empty, last_own, mention_open)
            });
    }

    let (attach, attach_tip): (Option<Message>, &str) = if !can_attach {
        (None, "Requires Attach Files")
    } else if composer.editing.is_some() {
        // An edit carries no files of its own.
        (None, "An edit cannot carry a file")
    } else if composer.slots_used() >= attachments::MAX_PER_MESSAGE {
        (None, "That is as many files as one message takes")
    } else {
        (
            Some(Message::Chat(ChatMsg::PickAttachment)),
            "Attach a file",
        )
    };

    // The same palette a message's reactions come from, into the text instead.
    let palette_open = main.chat.reacting == Some(COMPOSER_PALETTE);
    let emoji = widgets::icon_button(
        Icon::Smile,
        if palette_open { "Close" } else { "Emoji" },
        can_send.then(|| {
            Message::Chat(ChatMsg::OpenReactions(
                (!palette_open).then_some(COMPOSER_PALETTE),
            ))
        }),
        tokens,
    );

    let picker_open = main.sticker.picker_open;
    let stickers = widgets::icon_button(
        Icon::Star,
        if picker_open { "Close" } else { "Stickers" },
        can_send.then_some(Message::Sticker(StickerMsg::TogglePicker)),
        tokens,
    );

    let mut box_row = row![widgets::icon_button(
        Icon::Paperclip,
        attach_tip,
        attach.clone(),
        tokens
    )]
    .spacing(8)
    .align_y(Vertical::Center);
    // The chevron is the choice between the two routes, so it is offered on
    // exactly the terms the paperclip is: whatever refuses one refuses both.
    if attach.is_some() {
        box_row = box_row.push(routes(tokens));
    }
    let box_row = box_row
        .push(editor)
        .push(counter(app, main, metrics))
        .push(emoji)
        .push(stickers);

    container(box_row)
        .width(Length::Fill)
        .padding([8.0, 10.0])
        .style(styles::container::input(tokens))
        .into()
}

/// The two ways one picked file travels: stored on the server, or served off
/// this disk for as long as the sender is online. A file above the stored
/// ceiling takes the second route whichever of these is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileRoute {
    Attach,
    Stream,
}

impl FileRoute {
    pub(crate) const ALL: [FileRoute; 2] = [FileRoute::Attach, FileRoute::Stream];

    /// Which dialog the entry opens. Both end in the same picker; only what the
    /// picked paths are routed to differs.
    pub(crate) fn message(self) -> Message {
        match self {
            FileRoute::Attach => Message::Chat(ChatMsg::PickAttachment),
            FileRoute::Stream => Message::Chat(ChatMsg::PickStream),
        }
    }
}

impl std::fmt::Display for FileRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FileRoute::Attach => "Attach file…",
            FileRoute::Stream => "Share as stream…",
        })
    }
}

/// The chevron beside the paperclip, which offers the route the one-click
/// paperclip does not. It opens the window's own anchored popover, keyed on
/// [`MenuTarget::FileRoutes`], so the rows are as wide as the menu rather than
/// as wide as the handle that opened them.
fn routes<'a>(tokens: &'a ThemeTokens) -> Element<'a, Message> {
    widgets::tooltip_of(
        button(icons::icon(
            Icon::ChevronDown,
            widgets::ICON_MARK,
            tokens.text_secondary,
        ))
        .width(ROUTES_WIDTH)
        .padding([6.0, 4.0])
        .style(styles::button::icon(tokens))
        .on_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::FileRoutes))),
        "Other ways to send a file",
        tooltip::Position::Top,
        tokens,
    )
}

/// The emoji the composer offers, each one pressed into the text at the caret. It
/// stays open until it is closed: one press per emoji.
fn emoji_palette<'a>(app: &'a App, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut strip = row![].spacing(4).align_y(Vertical::Center);
    for emoji in PALETTE {
        strip = strip.push(widgets::tooltip_of(
            button(text(glyph_of(app, emoji)).size(metrics.text(TEXT_BODY)))
                .padding([2.0, 6.0])
                .style(styles::button::reaction(tokens, false))
                .on_press(Message::Chat(ChatMsg::InsertEmoji(emoji))),
            mentions::reaction_label(emoji),
            tooltip::Position::Top,
            tokens,
        ));
    }
    strip = strip.push(Space::new().width(Length::Fill));
    strip = strip.push(widgets::icon_button_in(
        Icon::Close,
        "Close",
        Some(Message::Chat(ChatMsg::OpenReactions(None))),
        tokens.text_secondary,
        widgets::ICON_MARK,
        tokens,
    ));

    container(strip)
        .width(Length::Fill)
        .padding(6)
        .style(styles::container::popover(tokens))
        .into()
}

/// The library as a grid of thumbnails, each one a message of its own. It stays
/// open until it is closed or one is sent, like the emoji palette beside it.
///
/// The pictures come from the window's own image cache, asked for when the
/// picker was opened; a thumbnail still on its way leaves its cell blank rather
/// than moving the grid once it lands.
fn sticker_picker<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let stickers = main.sticker.ordered();

    let mut grid = column![].spacing(6).width(Length::Fill);
    if stickers.is_empty() {
        grid = grid.push(
            text("No stickers yet")
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }
    for chunk in stickers.chunks(PICKER_COLUMNS) {
        let mut line = row![].spacing(6).align_y(Vertical::Center);
        for sticker in chunk {
            let key = ImageKey::Sticker(sticker.id);
            let face: Element<'_, Message> = match main.chat.images.get(&key) {
                Some(ImageState::Ready(handle)) => image(handle.clone())
                    .content_fit(iced::ContentFit::Contain)
                    .width(PICKER_THUMB)
                    .height(PICKER_THUMB)
                    .into(),
                _ => Space::new().width(PICKER_THUMB).height(PICKER_THUMB).into(),
            };
            line = line.push(widgets::tooltip_of(
                button(face)
                    .padding(4.0)
                    .style(styles::button::icon(tokens))
                    .on_press(Message::Sticker(StickerMsg::Send(sticker.id))),
                &sticker.name,
                tooltip::Position::Top,
                tokens,
            ));
        }
        grid = grid.push(line);
    }

    let head = row![
        text("Stickers")
            .size(TEXT_SECONDARY)
            .color(tokens.text_secondary),
        Space::new().width(Length::Fill),
        widgets::icon_button_in(
            Icon::Close,
            "Close",
            Some(Message::Sticker(StickerMsg::ClosePicker)),
            tokens.text_secondary,
            widgets::ICON_MARK,
            tokens,
        ),
    ]
    .align_y(Vertical::Center);

    container(
        column![
            head,
            scrollable(grid)
                .height(Length::Shrink)
                .style(styles::scrollable(tokens)),
        ]
        .spacing(6),
    )
    .width(Length::Fill)
    .max_height(PICKER_HEIGHT)
    .padding(6)
    .style(styles::container::popover(tokens))
    .into()
}

/// Enter sends; Shift+Enter breaks the line; Escape unwinds a reply or an edit;
/// Up in an empty composer reaches for the last message of one's own; Ctrl+V is
/// the app's own paste. Everything else is the editor's own.
///
/// An open mention list takes Enter, the arrows and Escape ahead of all of that.
/// Tab is not here: iced yields no binding for it, so it is answered by the
/// window's own key handler instead.
fn binding(
    press: KeyPress,
    editing: Option<i64>,
    replying: bool,
    empty: bool,
    last_own: Option<i64>,
    mention_open: bool,
) -> Option<Binding<Message>> {
    use keyboard::key::Named;

    if mention_open {
        let taken = match &press.key {
            keyboard::Key::Named(Named::Enter) if !press.modifiers.shift() => {
                Some(ChatMsg::MentionAccept)
            }
            keyboard::Key::Named(Named::ArrowUp) => Some(ChatMsg::MentionMove(-1)),
            keyboard::Key::Named(Named::ArrowDown) => Some(ChatMsg::MentionMove(1)),
            keyboard::Key::Named(Named::Escape) => Some(ChatMsg::MentionDismiss),
            _ => None,
        };
        if let Some(message) = taken {
            return Some(Binding::Custom(Message::Chat(message)));
        }
    }
    plain_binding(press, editing, replying, empty, last_own)
}

/// The composer's own bindings, with no mention list in front of them.
fn plain_binding(
    press: KeyPress,
    editing: Option<i64>,
    replying: bool,
    empty: bool,
    last_own: Option<i64>,
) -> Option<Binding<Message>> {
    use keyboard::key::Named;

    match &press.key {
        keyboard::Key::Named(Named::Enter) => {
            return if press.modifiers.shift() {
                Some(Binding::Insert('\n'))
            } else {
                Some(Binding::Custom(Message::Chat(ChatMsg::Send)))
            };
        }
        keyboard::Key::Named(Named::Escape) if editing.is_some() => {
            return Some(Binding::Custom(Message::Chat(ChatMsg::CancelEdit)));
        }
        keyboard::Key::Named(Named::Escape) if replying => {
            return Some(Binding::Custom(Message::Chat(ChatMsg::CancelReply)));
        }
        keyboard::Key::Named(Named::ArrowUp) if empty && editing.is_none() => {
            if let Some(id) = last_own {
                return Some(Binding::Custom(Message::Chat(ChatMsg::StartEdit(id))));
            }
        }
        // The editor's own paste would take the text and drop everything else,
        // so the whole clipboard is read here instead. Text is not lost by that:
        // `update::chat` hands a text-only clipboard straight back to the editor
        // as the same `Edit::Paste` action this arm replaces.
        keyboard::Key::Character(character)
            if press.modifiers.command() && character.eq_ignore_ascii_case("v") =>
        {
            return Some(Binding::Custom(Message::Chat(ChatMsg::Paste)));
        }
        _ => {}
    }
    Binding::from_key_press(press)
}

/// What the empty composer says, which names the channel it writes to.
fn placeholder(main: &MainState, can_send: bool) -> String {
    match main.chat.current.channel_id() {
        Some(_) if !can_send => "You cannot send messages here".to_owned(),
        Some(id) => format!("Message {}", main.server.channel_title(id)),
        None => "Pick a channel first".to_owned(),
    }
}

/// How close the text is to the limit the server keeps. Nothing is drawn until it
/// is worth saying.
fn counter<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let length = main.chat.composer.content.text().chars().count();
    if length <= MESSAGE_MAX_CHARS * 9 / 10 {
        return Space::new().into();
    }
    text(format!("{length}/{MESSAGE_MAX_CHARS}"))
        .size(metrics.text(TEXT_BADGE))
        .color(if length > MESSAGE_MAX_CHARS {
            tokens.danger
        } else {
            tokens.text_muted
        })
        .into()
}

/// What the next send will do, when it is not simply a new message.
fn banner<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let composer = &main.chat.composer;

    let (glyph, label, cancel) = if composer.editing.is_some() {
        (
            Icon::Edit,
            "Editing a message".to_owned(),
            ChatMsg::CancelEdit,
        )
    } else {
        let id = composer.reply_to?;
        let label = match main.chat.message(id) {
            Some(message) => format!(
                "Replying to {}: {}",
                message.author,
                excerpt(&message.text, &main.user_pairs)
            ),
            None => "Replying to a message".to_owned(),
        };
        (Icon::Reply, label, ChatMsg::CancelReply)
    };

    Some(
        container(
            row![
                icons::icon(glyph, widgets::ICON_MARK, tokens.text_secondary),
                text(label)
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.text_secondary)
                    .width(Length::Fill),
                widgets::icon_button_in(
                    Icon::Close,
                    "Cancel",
                    Some(Message::Chat(cancel)),
                    tokens.text_secondary,
                    widgets::ICON_MARK,
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        )
        .width(Length::Fill)
        .padding([2.0, 8.0])
        .style(styles::container::elevated(tokens))
        .into(),
    )
}

/// What a quoted message reads as: the names a token stands for, cut short.
fn excerpt(body: &str, users: &[(i64, String)]) -> String {
    let rendered = plain_text(body, users);
    if rendered.chars().count() <= EXCERPT_MAX {
        return rendered;
    }
    rendered.chars().take(EXCERPT_MAX).collect::<String>() + "…"
}

/// The note about an `@everyone` that will reach nobody.
fn everyone_warning<'a>(
    app: &'a App,
    main: &'a MainState,
    can_everyone: bool,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    if can_everyone {
        return None;
    }
    let body = main.chat.composer.content.text();
    let word = if mentions::mentions_everyone(&body) {
        mentions::EVERYONE
    } else if mentions::mentions_here(&body) {
        mentions::HERE
    } else {
        return None;
    };

    let tokens = &app.tokens;
    Some(
        container(
            row![
                icons::icon(Icon::Warning, widgets::ICON_MARK, tokens.warning),
                text(format!(
                    "{word} needs Mention Everyone — it will notify nobody"
                ))
                .size(metrics.text(TEXT_SECONDARY)),
            ]
            .spacing(6)
            .align_y(Vertical::Center),
        )
        .padding([2.0, 8.0])
        .style(styles::container::warning_chip(tokens))
        .into(),
    )
}

/// What the mention list offers for what has been typed after the `@`, with the
/// highlighted row lit. Enter and Tab take that row; a press takes the row it
/// lands on.
fn mention_list<'a>(
    app: &'a App,
    main: &'a MainState,
    mention: &Mention,
    can_everyone: bool,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    let tokens = &app.tokens;
    let candidates = mention_candidates(&main.user_pairs, &mention.query, can_everyone);
    if candidates.is_empty() {
        return None;
    }
    // A query that grew keeps its highlight, and the list it was taken from can
    // have shrunk under it since.
    let selected = mention.selected.min(candidates.len() - 1);

    let mut list = column![].spacing(2).width(Length::Fill);
    for (index, candidate) in candidates.iter().enumerate() {
        let word = candidate.word().to_owned();
        let line = match candidate {
            MentionCandidate::Everyone => broadcast_row(
                mentions::EVERYONE,
                Icon::Users,
                "Notify everyone in the channel",
                tokens,
                metrics,
            ),
            MentionCandidate::Here => broadcast_row(
                mentions::HERE,
                Icon::Bell,
                "Notify online members",
                tokens,
                metrics,
            ),
            MentionCandidate::Member { user_id, username } => {
                member_row(main, *user_id, username, tokens, metrics)
            }
        };

        list = list.push(
            button(line)
                .width(Length::Fill)
                .padding([metrics.row_padding(), 6.0])
                .style(styles::button::row_for(tokens, index == selected))
                .on_press(Message::Chat(ChatMsg::MentionPick(word))),
        );
    }

    Some(
        container(list)
            .width(Length::Fill)
            .padding(6)
            .style(styles::container::popover(tokens))
            .into(),
    )
}

/// One member of the mention list: their picture, the name they go by, and the
/// handle a pick actually writes.
fn member_row<'a>(
    main: &'a MainState,
    user_id: i64,
    username: &str,
    tokens: &'a ThemeTokens,
    metrics: Metrics,
) -> Element<'a, Message> {
    let display = main.server.display_name(user_id);
    row![
        widgets::member_avatar(main, user_id, widgets::AVATAR_OCCUPANT, tokens),
        widgets::clipped_name_within(
            text(display)
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_primary),
            display,
            MENTION_NAME_MAX,
            tokens,
        ),
        text(format!("@{username}"))
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

/// `@everyone` or `@here`: a chip of the word itself, since neither has a face,
/// and one line on who it would reach.
fn broadcast_row<'a>(
    word: &'a str,
    glyph: Icon,
    hint: &'a str,
    tokens: &'a ThemeTokens,
    metrics: Metrics,
) -> Element<'a, Message> {
    let mark = icons::icon(glyph, widgets::ICON_MARK, tokens.text_secondary);
    let chip = text(word)
        .size(metrics.text(TEXT_SECONDARY))
        .color(tokens.text_primary);

    row![
        // The same footprint an avatar takes, so the rows line up whichever kind
        // they are.
        container(mark).center(widgets::AVATAR_OCCUPANT),
        container(chip)
            .padding([1.0, 6.0])
            .style(styles::container::chip(tokens)),
        text(hint)
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(8)
    .align_y(Vertical::Center)
    .into()
}

/// What is already held for the next message, and what is still going up: the
/// uploads that landed, the files offered as streams, and the transfers in
/// flight.
fn strip<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let composer = &main.chat.composer;
    let mut line = row![].spacing(6).align_y(Vertical::Center);
    for attachment in &composer.attachments {
        line = line.push(pending(app, main, attachment, metrics));
    }
    for offered in &composer.streams {
        line = line.push(pending_stream(app, offered, metrics));
    }
    for transfer in &composer.uploading {
        line = line.push(in_flight(app, transfer, metrics));
    }
    line.into()
}

/// One file the next message will carry, with the way to take it back out. A
/// picture shows its thumbnail; anything else shows the paperclip, because
/// nothing was decoded for it.
fn pending<'a>(
    app: &'a App,
    main: &'a MainState,
    attachment: &Attachment,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let key = ImageKey::Attachment(attachment.id);
    let thumb: Element<'_, Message> = match widgets::image_handle(&main.chat, key) {
        Some(handle) => image(handle).width(THUMB).height(THUMB).into(),
        None => container(icons::icon(
            Icon::Paperclip,
            widgets::ICON_SIZE,
            tokens.text_secondary,
        ))
        .center(THUMB)
        .into(),
    };

    chip(
        app,
        thumb,
        attachment.file_name.clone(),
        format_bytes(u64::try_from(attachment.size).unwrap_or(0)),
        None,
        (
            "Remove",
            Message::Chat(ChatMsg::RemovePendingAttachment(attachment.id)),
        ),
        metrics,
    )
}

/// One file the next message will point at without ever uploading it. The offer
/// stands on the server either way: taking it back out sends nothing.
fn pending_stream<'a>(
    app: &'a App,
    offered: &PendingStream,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let glyph = container(icons::icon(
        Icon::Link,
        widgets::ICON_SIZE,
        tokens.text_secondary,
    ))
    .center(THUMB)
    .into();

    chip(
        app,
        glyph,
        offered.file_name.clone(),
        format!(
            "{} · streamed from here",
            format_bytes(u64::try_from(offered.size).unwrap_or(0))
        ),
        None,
        (
            "Remove",
            Message::Chat(ChatMsg::RemovePendingStream(offered.id)),
        ),
        metrics,
    )
}

/// One upload or offer the composer is still waiting on, with how far it has
/// come and the way to give up on it.
fn in_flight<'a>(
    app: &'a App,
    transfer: &PendingTransfer,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let glyph = container(icons::icon(
        Icon::ArrowUp,
        widgets::ICON_SIZE,
        tokens.text_secondary,
    ))
    .center(THUMB)
    .into();

    // An offer moves no bytes, so there is no fraction worth drawing for it.
    let progress = match transfer.kind {
        TransferKind::Upload => Some(progress_fraction(transfer.done, transfer.total)),
        TransferKind::Offer => None,
    };
    let caption = match transfer.kind {
        TransferKind::Upload => format!(
            "{} of {}",
            format_bytes(transfer.done),
            format_bytes(transfer.total)
        ),
        TransferKind::Offer => "Offering…".to_owned(),
    };

    chip(
        app,
        glyph,
        transfer.file_name.clone(),
        caption,
        progress,
        (
            "Cancel",
            Message::Chat(ChatMsg::CancelUpload(transfer.request_id)),
        ),
        metrics,
    )
}

/// The chip every held file is drawn as: a mark, what it is called, a line
/// under it, an optional bar, and the button that takes it out — the tooltip
/// that button carries and what it sends travel together.
fn chip<'a>(
    app: &'a App,
    mark: Element<'a, Message>,
    file_name: String,
    caption: String,
    progress: Option<f32>,
    remove: (&str, Message),
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut details = column![
        text(file_name)
            .size(metrics.text(TEXT_SECONDARY))
            .color(tokens.text_secondary),
        text(caption)
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(2);
    if let Some(fraction) = progress {
        details = details.push(
            progress_bar(0.0..=1.0, fraction)
                .length(CHIP_BAR_LENGTH)
                .girth(CHIP_BAR_GIRTH),
        );
    }

    container(
        row![
            mark,
            details,
            widgets::icon_button_in(
                Icon::Close,
                remove.0,
                Some(remove.1),
                tokens.text_secondary,
                widgets::ICON_MARK,
                tokens,
            ),
        ]
        .spacing(8)
        .align_y(Vertical::Center),
    )
    .padding([4.0, 6.0])
    .style(styles::container::chip(tokens))
    .into()
}

/// The line under the box: how to send, or why one cannot.
fn footer<'a>(
    app: &'a App,
    main: &'a MainState,
    can_send: bool,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    if main.chat.current.channel_id().is_none() {
        return Space::new().into();
    }
    if !can_send {
        return row![
            icons::icon(Icon::Ban, widgets::ICON_MARK, tokens.text_muted),
            text("You cannot send messages here")
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_muted),
        ]
        .spacing(6)
        .align_y(Vertical::Center)
        .into();
    }

    row![
        widgets::key_hint("Enter", tokens),
        text("sends")
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
        widgets::key_hint("Shift+Enter", tokens),
        text("for a new line")
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(6)
    .align_y(Vertical::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<(i64, String)> {
        [(1, "Bruno"), (2, "ana"), (3, "Ana Maria"), (4, "anders")]
            .into_iter()
            .map(|(id, name)| (id, name.to_owned()))
            .collect()
    }

    #[test]
    fn an_excerpt_names_the_user_a_token_stands_for() {
        assert_eq!(excerpt("hi <@2> there", &users()), "hi @ana there");
    }

    #[test]
    fn each_route_opens_its_own_dialog_and_says_which_it_is() {
        assert!(matches!(
            FileRoute::Attach.message(),
            Message::Chat(ChatMsg::PickAttachment)
        ));
        assert!(matches!(
            FileRoute::Stream.message(),
            Message::Chat(ChatMsg::PickStream)
        ));
        assert_eq!(FileRoute::Attach.to_string(), "Attach file…");
        assert_eq!(FileRoute::Stream.to_string(), "Share as stream…");
    }

    #[test]
    fn a_long_excerpt_is_cut_short() {
        let body = "x".repeat(80);
        let cut = excerpt(&body, &[]);

        assert_eq!(cut.chars().count(), EXCERPT_MAX + 1);
        assert!(cut.ends_with('…'));
    }
}
