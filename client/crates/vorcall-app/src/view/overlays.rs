//! Everything that sits in front of the page: the dialogs, the quick switcher,
//! the profile card, the context menu and the toasts.
//!
//! Z-order, bottom to top: the dialog takes the window, the quick switcher is
//! over it, then the card, then the menu, and the toasts are always on top — they
//! are the one layer that never blocks anything.
//!
//! A dialog *is* its form: every field rebuilds the [`Dialog`] value and hands it
//! back through `UiMsg::OpenDialog`, and the submit is a [`DialogAction::Submit`]
//! that `update::ui` turns into the command. No dialog keeps a draft anywhere
//! else, so closing one forgets it.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Id, Space, TextInput, button, column, container, image, mouse_area, opaque, progress_bar, row,
    scrollable, slider, stack, text, text_input, toggler,
};
use iced::{ContentFit, Element, Length, mouse};
use vorcall_core::ChannelKind;
use vorcall_core::images::ImagePurpose;
use vorcall_screen::{Source, SourceId, SourceKind};

use crate::app::message::{
    AdminMsg, AuthMsg, ChatMsg, CropMsg, Message, SettingsMsg, ShareMsg, UiMsg,
};
use crate::app::state::chat::ImageState;
use crate::app::state::crop::{self, CropState, FRAME_WIDTH, ZOOM_MAX, ZOOM_MIN};
use crate::app::state::rules::{format_bytes, progress_fraction};
use crate::app::state::ui::{
    Dialog, DialogAction, SourcesState, TransferSource, TransferState, validate_long, validate_name,
};
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::widgets;
use crate::view::{
    CURRENT_PASSWORD_ID, TEXT_BODY, TEXT_ROW, TEXT_SECTION, context_menu, profile_card,
    quick_switcher, toasts,
};
use crate::workers::images::ImageKey;

/// How wide a dialog is, and how wide the one that lists things is.
const DIALOG_WIDTH: f32 = 360.0;
const PICKER_WIDTH: f32 = 440.0;
/// How much of the picker the source list may take.
const SOURCES_HEIGHT: f32 = 220.0;
/// The bar a running transfer draws.
const TRANSFER_BAR_GIRTH: f32 = 6.0;

/// The stacked overlays, topmost last. Nothing is drawn when nothing is open.
pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let mut layers: Vec<Element<'_, Message>> = Vec::new();

    if let Some(dialog) = &app.ui.dialog {
        layers.push(modal(app, main, dialog));
    }
    if app.ui.quick_switcher.open {
        layers.push(quick_switcher::view(app, main));
    }
    if app.ui.profile_card.is_some() {
        layers.push(profile_card::view(app, main));
    }
    if app.ui.context_menu.is_some() {
        layers.push(context_menu::view(app, main));
    }
    if !app.ui.toasts.is_empty() {
        layers.push(toasts::view(app));
    }

    if layers.is_empty() {
        return Space::new().into();
    }
    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// An overlay's own action, wrapped so the overlay is down before it runs:
/// `UiMsg` has no message that does both.
pub fn perform(message: Message) -> Message {
    Message::Ui(UiMsg::OpenDialog(Dialog::Action(DialogAction::Perform(
        Box::new(message),
    ))))
}

/// An overlay item that opens a dialog.
pub fn opens(dialog: Dialog) -> Message {
    perform(Message::Ui(UiMsg::OpenDialog(dialog)))
}

/// An overlay item that puts one value on the clipboard.
pub fn copies(what: &'static str, value: String) -> Message {
    Message::Ui(UiMsg::OpenDialog(Dialog::Action(DialogAction::Copy {
        what,
        value,
    })))
}

/// What a dialog's own Enter and its action button send.
fn submit() -> Message {
    Message::Ui(UiMsg::OpenDialog(Dialog::Action(DialogAction::Submit)))
}

/// One dialog, over a backdrop that dismisses it.
fn modal<'a>(app: &'a App, main: &'a MainState, dialog: &'a Dialog) -> Element<'a, Message> {
    // The lightbox is its own shape: a picture over the backdrop, no card.
    if let Dialog::Image(id) = dialog {
        return lightbox(app, main, *id);
    }

    let tokens = &app.tokens;
    let body = match dialog {
        Dialog::ChangePassword {
            current,
            new,
            confirm,
            error,
            busy,
        } => change_password(app, current, new, confirm, error.as_deref(), *busy),
        Dialog::CreateChannel {
            category_id,
            kind,
            name,
            error,
        } => create_channel(app, *category_id, *kind, name, error.as_deref()),
        Dialog::CreateCategory { name, error } => create_category(app, name, error.as_deref()),
        Dialog::EditChannel {
            channel_id,
            name,
            topic,
        } => edit_channel(app, *channel_id, name, topic),
        Dialog::EditCategory { id, name } => edit_category(app, *id, name),
        Dialog::ConfirmDeleteChannel { channel_id } => confirm(
            app,
            format!("Delete #{}?", main.server.channel_title(*channel_id)),
            "Every message in it goes with it. This cannot be undone.",
            "Delete channel",
        ),
        Dialog::ConfirmDeleteCategory { id } => confirm(
            app,
            format!("Delete {}?", category_name(main, *id)),
            "Its channels are kept and move out of it.",
            "Delete category",
        ),
        Dialog::ConfirmDeleteRole { role_id } => confirm(
            app,
            format!("Delete {}?", role_name(main, *role_id)),
            "Everybody who has it loses it, and the permissions it granted.",
            "Delete role",
        ),
        Dialog::ConfirmDeleteMessage { .. } => confirm(
            app,
            "Delete this message?".to_owned(),
            "It is replaced by a tombstone for everybody.",
            "Delete",
        ),
        Dialog::ConfirmKick { user_id } => confirm(
            app,
            format!("Kick {}?", main.server.display_name(*user_id)),
            "They are removed from the server and have to be invited back.",
            "Kick",
        ),
        Dialog::ConfirmTransferOwnership { user_id } => confirm(
            app,
            format!("Hand the server to {}?", main.server.display_name(*user_id)),
            "The owner bypasses every permission check. After this you will not, and only they can hand it back.",
            "Transfer",
        ),
        Dialog::BanReason { user_id, reason } => {
            ban(app, main.server.display_name(*user_id), reason)
        }
        Dialog::Nickname { user_id, draft } => nickname(app, main, *user_id, draft),
        Dialog::CropImage {
            purpose,
            handle,
            source,
            crop,
            ..
        } => crop_image(app, *purpose, handle, *source, *crop),
        Dialog::SharePicker {
            sources,
            selected,
            audio,
        } => share_picker(app, sources, selected.as_ref(), *audio),
        Dialog::Transfer {
            source,
            request_id,
            file_name,
            received,
            total,
            state,
        } => transfer(
            app,
            *source,
            *request_id,
            file_name,
            *received,
            *total,
            state,
        ),
        Dialog::ThemeSaveAs { name } => theme_save_as(app, name),
        // The code is shown once: the server never sends it again.
        Dialog::InviteCreated { code } => invite_created(app, code),
        Dialog::CrashReport => crash_report(app),
        // Never stored: an action is performed and dropped.
        Dialog::Image(_) | Dialog::Action(_) => Space::new().into(),
    };

    let backdrop = mouse_area(
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(styles::container::backdrop(tokens)),
    )
    .on_press(Message::Ui(UiMsg::CloseDialog));

    opaque(
        stack![
            backdrop,
            // The card takes its own presses, so only what is outside it dismisses
            // the dialog.
            container(mouse_area(body).on_press(Message::Noop)).center(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
}

/// The frame every dialog shares.
fn frame<'a>(
    app: &'a App,
    title: &str,
    rows: Vec<Element<'a, Message>>,
    actions: Element<'a, Message>,
) -> Element<'a, Message> {
    sized_frame(app, title, rows, actions, DIALOG_WIDTH)
}

fn sized_frame<'a>(
    app: &'a App,
    title: &str,
    rows: Vec<Element<'a, Message>>,
    actions: Element<'a, Message>,
    width: f32,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut body = column![
        text(title.to_owned())
            .size(TEXT_SECTION)
            .color(tokens.text_primary),
    ]
    .spacing(12)
    .align_x(Horizontal::Left);
    for row in rows {
        body = body.push(row);
    }
    body = body.push(actions);

    container(body)
        .padding(20)
        .width(width)
        .style(styles::container::card(tokens))
        .into()
}

/// Cancel, and the one button that does the thing. A complaint the dialog can
/// work out itself is what disables it.
fn actions<'a>(app: &'a App, label: &'a str, ready: bool, danger: bool) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let mut action = button(text(label.to_owned()).size(TEXT_BODY)).padding([6.0, 14.0]);
    action = if danger {
        action.style(styles::button::danger(tokens))
    } else {
        action.style(styles::button::primary(tokens))
    };
    if ready {
        action = action.on_press(submit());
    }

    row![
        button(text("Cancel").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
        action,
    ]
    .spacing(8)
    .into()
}

/// One field of a dialog. Typing in it rebuilds the whole dialog, which is what
/// keeps the value in one place.
fn field<'a>(
    tokens: &'a ThemeTokens,
    placeholder: &'a str,
    value: &'a str,
    rebuild: impl Fn(String) -> Dialog + 'a,
) -> TextInput<'a, Message> {
    text_input(placeholder, value)
        .on_input(move |typed| Message::Ui(UiMsg::OpenDialog(rebuild(typed))))
        .on_submit(submit())
        .padding(10)
        .width(Length::Fill)
        .style(styles::text_input(tokens))
}

/// A line of explanation under a field.
fn note<'a>(tokens: &'a ThemeTokens, what: String) -> Element<'a, Message> {
    text(what)
        .size(TEXT_ROW)
        .color(tokens.text_secondary)
        .into()
}

/// What is wrong with what has been typed, or what the server refused.
fn complaint<'a>(tokens: &'a ThemeTokens, what: String) -> Element<'a, Message> {
    text(what).size(TEXT_ROW).color(tokens.danger).into()
}

/// Pushes whichever refusal there is: the server's first, then the local one.
fn push_error<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    tokens: &'a ThemeTokens,
    refused: Option<&str>,
    local: Option<String>,
) {
    if let Some(refused) = refused {
        rows.push(complaint(tokens, refused.to_owned()));
    } else if let Some(local) = local {
        rows.push(complaint(tokens, local));
    }
}

fn create_channel<'a>(
    app: &'a App,
    category_id: Option<i64>,
    kind: ChannelKind,
    name: &'a str,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let checked = validate_name(name, "channel name");

    let kinds = row![
        kind_button(
            app,
            "Text",
            Icon::Hash,
            kind == ChannelKind::Text,
            Dialog::CreateChannel {
                category_id,
                kind: ChannelKind::Text,
                name: name.to_owned(),
                error: None,
            },
        ),
        kind_button(
            app,
            "Voice",
            Icon::Speaker,
            kind == ChannelKind::Voice,
            Dialog::CreateChannel {
                category_id,
                kind: ChannelKind::Voice,
                name: name.to_owned(),
                error: None,
            },
        ),
    ]
    .spacing(8);

    let mut rows: Vec<Element<'_, Message>> = vec![
        kinds.into(),
        field(tokens, "Name", name, move |typed| Dialog::CreateChannel {
            category_id,
            kind,
            name: typed,
            error: None,
        })
        .into(),
    ];
    push_error(&mut rows, tokens, error, checked.clone().err());

    frame(
        app,
        "Create a channel",
        rows,
        actions(app, "Create", checked.is_ok(), false),
    )
}

/// The two kinds a new channel can be, as a pair of rows.
fn kind_button<'a>(
    app: &'a App,
    label: &'static str,
    glyph: Icon,
    selected: bool,
    next: Dialog,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    button(
        row![
            icons::icon(glyph, 16.0, tokens.text_secondary),
            text(label).size(TEXT_BODY),
        ]
        .spacing(6)
        .align_y(Vertical::Center),
    )
    .padding([6.0, 12.0])
    .style(styles::button::row_for(tokens, selected))
    .on_press(Message::Ui(UiMsg::OpenDialog(next)))
    .into()
}

fn create_category<'a>(
    app: &'a App,
    name: &'a str,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let checked = validate_name(name, "category name");

    let mut rows: Vec<Element<'_, Message>> = vec![
        field(tokens, "Name", name, |typed| Dialog::CreateCategory {
            name: typed,
            error: None,
        })
        .into(),
    ];
    push_error(&mut rows, tokens, error, checked.clone().err());

    frame(
        app,
        "Create a category",
        rows,
        actions(app, "Create", checked.is_ok(), false),
    )
}

fn edit_channel<'a>(
    app: &'a App,
    channel_id: i64,
    name: &'a str,
    topic: &'a str,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let checked = validate_name(name, "channel name");
    let topic_checked = validate_long(topic);

    let mut rows: Vec<Element<'_, Message>> = vec![
        field(tokens, "Name", name, move |typed| Dialog::EditChannel {
            channel_id,
            name: typed,
            topic: topic.to_owned(),
        })
        .into(),
        field(tokens, "Topic", topic, move |typed| Dialog::EditChannel {
            channel_id,
            name: name.to_owned(),
            topic: typed,
        })
        .into(),
    ];
    push_error(
        &mut rows,
        tokens,
        None,
        checked.clone().err().or(topic_checked.clone().err()),
    );

    frame(
        app,
        "Edit channel",
        rows,
        actions(app, "Save", checked.is_ok() && topic_checked.is_ok(), false),
    )
}

fn edit_category<'a>(app: &'a App, id: i64, name: &'a str) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let checked = validate_name(name, "category name");

    let mut rows: Vec<Element<'_, Message>> = vec![
        field(tokens, "Name", name, move |typed| Dialog::EditCategory {
            id,
            name: typed,
        })
        .into(),
    ];
    push_error(&mut rows, tokens, None, checked.clone().err());

    frame(
        app,
        "Rename category",
        rows,
        actions(app, "Save", checked.is_ok(), false),
    )
}

/// Anything destructive: what it is, what it costs, and the red button.
fn confirm<'a>(
    app: &'a App,
    question: String,
    consequence: &'a str,
    label: &'a str,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    frame(
        app,
        &question,
        vec![note(tokens, consequence.to_owned())],
        actions(app, label, true, true),
    )
}

/// One file coming down onto the disk. Alone among the dialogs this one is a
/// display rather than a form: the counters are written into it in place, so
/// nothing here rebuilds it.
fn transfer<'a>(
    app: &'a App,
    source: TransferSource,
    request_id: u64,
    file_name: &str,
    received: u64,
    total: u64,
    state: &'a TransferState,
) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let mut rows: Vec<Element<'_, Message>> = vec![
        text(file_name.to_owned())
            .size(TEXT_BODY)
            .color(tokens.text_primary)
            .into(),
    ];
    if matches!(source, TransferSource::Stream(_)) {
        rows.push(note(
            tokens,
            "Streamed from the sender's own computer.".to_owned(),
        ));
    }

    let (title, action) = match state {
        TransferState::Running => {
            rows.push(
                progress_bar(0.0..=1.0, progress_fraction(received, total))
                    .girth(TRANSFER_BAR_GIRTH)
                    .into(),
            );
            // A record that never said how large the file is counts up instead of
            // filling.
            rows.push(note(
                tokens,
                if total == 0 {
                    format_bytes(received)
                } else {
                    format!("{} of {}", format_bytes(received), format_bytes(total))
                },
            ));
            let cancel = button(text("Cancel").size(TEXT_BODY))
                .padding([6.0, 14.0])
                .style(styles::button::secondary(tokens))
                .on_press(Message::Chat(ChatMsg::CancelTransfer(request_id)));
            ("Saving a file", cancel.into())
        }
        TransferState::Done(path) => {
            rows.push(note(tokens, format!("Saved to {}", path.display())));
            ("File saved", close(app))
        }
        TransferState::Failed(reason) => {
            rows.push(complaint(tokens, reason.clone()));
            ("The file did not arrive", close(app))
        }
    };

    sized_frame(app, title, rows, action, DIALOG_WIDTH)
}

/// The one button a dialog that has nothing left to do carries.
fn close(app: &App) -> Element<'_, Message> {
    button(text("Close").size(TEXT_BODY))
        .padding([6.0, 14.0])
        .style(styles::button::secondary(&app.tokens))
        .on_press(Message::Ui(UiMsg::CloseDialog))
        .into()
}

/// The server settings own the ban flow — the bans page writes the same draft —
/// so the reason and the confirm go to it rather than rebuilding the dialog.
fn ban<'a>(app: &'a App, name: &str, reason: &'a str) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let checked = validate_long(reason);

    let mut rows: Vec<Element<'_, Message>> = vec![
        note(
            tokens,
            "They are removed from the server and cannot come back.".to_owned(),
        ),
        text_input("Reason (optional)", reason)
            .on_input(|typed| Message::Admin(AdminMsg::BanReason(typed)))
            .on_submit(Message::Admin(AdminMsg::BanConfirm))
            .padding(10)
            .width(Length::Fill)
            .style(styles::text_input(tokens))
            .into(),
    ];
    push_error(&mut rows, tokens, None, checked.clone().err());

    let mut confirm = button(text("Ban").size(TEXT_BODY))
        .padding([6.0, 14.0])
        .style(styles::button::danger(tokens));
    if checked.is_ok() {
        confirm = confirm.on_press(Message::Admin(AdminMsg::BanConfirm));
    }
    let actions = row![
        button(text("Cancel").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
        confirm,
    ]
    .spacing(8);

    frame(app, &format!("Ban {name}?"), rows, actions.into())
}

fn nickname<'a>(
    app: &'a App,
    main: &'a MainState,
    user_id: i64,
    draft: &'a str,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    // Empty clears it, which is a value the server takes.
    let checked = if draft.trim().is_empty() {
        Ok(String::new())
    } else {
        validate_name(draft, "nickname")
    };
    let username = main
        .server
        .members
        .get(&user_id)
        .map(|profile| profile.username.clone())
        .unwrap_or_default();

    let mut rows: Vec<Element<'_, Message>> = vec![
        field(tokens, "Nickname", draft, move |typed| Dialog::Nickname {
            user_id,
            draft: typed,
        })
        .into(),
        note(tokens, format!("Leave it empty to go back to @{username}.")),
    ];
    push_error(&mut rows, tokens, None, checked.clone().err());

    frame(
        app,
        "Change nickname",
        rows,
        actions(app, "Save", checked.is_ok(), false),
    )
}

fn theme_save_as<'a>(app: &'a App, name: &'a str) -> Element<'a, Message> {
    let tokens = &app.tokens;
    // The settings page owns the draft this writes into, so the field reports to
    // it rather than rebuilding the dialog.
    let rows: Vec<Element<'_, Message>> = vec![
        text_input("Theme name", name)
            .on_input(|typed| Message::Settings(SettingsMsg::ThemeEditorName(typed)))
            .on_submit(Message::Settings(SettingsMsg::ThemeSaveAsConfirm))
            .padding(10)
            .width(Length::Fill)
            .style(styles::text_input(tokens))
            .into(),
    ];

    let mut save = button(text("Save").size(TEXT_BODY))
        .padding([6.0, 14.0])
        .style(styles::button::primary(tokens));
    if !name.trim().is_empty() {
        save = save.on_press(Message::Settings(SettingsMsg::ThemeSaveAsConfirm));
    }
    let actions = row![
        button(text("Cancel").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
        save,
    ]
    .spacing(8);

    frame(app, "Save the theme as…", rows, actions.into())
}

/// The offer made once a run when the last one left a crash report behind. "Not
/// now" is not a Cancel: it answers the offer, which is what puts it away until
/// the next sign-in, and the files stay on disk for the Account page to send.
fn crash_report<'a>(app: &'a App) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let rows = vec![note(
        tokens,
        "Send the crash report and the tail of the log to the server?".to_owned(),
    )];

    let actions = row![
        button(text("Send").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::primary(tokens))
            .on_press(submit()),
        button(text("Not now").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Settings(SettingsMsg::DismissCrashReport)),
    ]
    .spacing(8);

    frame(app, "Vorcall crashed last time", rows, actions.into())
}

fn invite_created<'a>(app: &'a App, code: &'a str) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let rows: Vec<Element<'_, Message>> = vec![
        note(
            tokens,
            "This is the only time the code is shown.".to_owned(),
        ),
        container(
            text(code.to_owned())
                .size(TEXT_BODY)
                .color(tokens.text_primary),
        )
        .padding([8.0, 10.0])
        .width(Length::Fill)
        .style(styles::container::input(tokens))
        .into(),
    ];

    let actions = row![
        button(text("Copy").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::primary(tokens))
            .on_press(copies("Invite code", code.to_owned())),
        button(text("Close").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
    ]
    .spacing(8);

    frame(app, "Invite created", rows, actions.into())
}

/// What to share. Where the system owns the picker there is nothing to list: the
/// choice is made in its own dialog once the capture starts.
fn share_picker<'a>(
    app: &'a App,
    sources: &'a SourcesState,
    selected: Option<&'a SourceId>,
    audio: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let capabilities = vorcall_screen::capabilities();

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if capabilities.portal_picker {
        rows.push(note(
            tokens,
            "You will pick the screen or window in the system dialog.".to_owned(),
        ));
    } else {
        rows.push(source_list(app, sources, selected));
    }
    rows.push(
        toggler(audio)
            .label("Share audio")
            .text_size(TEXT_BODY)
            .on_toggle(|value| Message::Share(ShareMsg::SetPickerAudio(value)))
            .style(styles::toggler(tokens))
            .into(),
    );

    // The system's own picker answers for the source, so there is nothing left to
    // choose here first.
    let ready = capabilities.portal_picker || selected.is_some();
    let mut share = button(text("Share").size(TEXT_BODY))
        .padding([6.0, 14.0])
        .style(styles::button::primary(tokens));
    if ready {
        share = share.on_press(Message::Share(ShareMsg::Confirm));
    }
    let actions = row![
        button(text("Cancel").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
        share,
    ]
    .spacing(8);

    sized_frame(app, "Share a screen", rows, actions.into(), PICKER_WIDTH)
}

/// The crop adjuster: the picked picture under a frame of the shape it is going
/// to be drawn in, moved by dragging and tightened by the slider.
///
/// `Image::crop` scissors the preview as it renders rather than cutting any
/// pixels, so a drag costs a redraw and nothing is encoded until the action
/// button.
fn crop_image<'a>(
    app: &'a App,
    purpose: ImagePurpose,
    handle: &'a iced::widget::image::Handle,
    source: (u32, u32),
    state: CropState,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    // The aspect is the upload box: 512 by 512 is 1:1, 1600 by 600 is 8:3.
    let aspect = purpose.max_size();
    let frame_height = FRAME_WIDTH * aspect.1 as f32 / aspect.0 as f32;
    // A square frame is a circle at half its width, which is how the three round
    // purposes are drawn everywhere else.
    let radius = if aspect.0 == aspect.1 {
        FRAME_WIDTH / 2.0
    } else {
        styles::RADIUS_CARD
    };

    let preview = mouse_area(
        image(handle.clone())
            .crop(crop::region(source, aspect, state))
            .width(FRAME_WIDTH)
            .height(frame_height)
            .content_fit(ContentFit::Cover)
            .border_radius(radius),
    )
    .interaction(mouse::Interaction::Grab)
    .on_press(Message::Crop(CropMsg::PanStart))
    .on_move(|at| Message::Crop(CropMsg::PanMove(at)))
    .on_release(Message::Crop(CropMsg::PanEnd))
    // A release outside the frame never arrives, so leaving it ends the drag
    // rather than leaving one armed.
    .on_exit(Message::Crop(CropMsg::PanEnd));

    let tighten = slider(ZOOM_MIN..=ZOOM_MAX, state.zoom, |value| {
        Message::Crop(CropMsg::Zoom(value))
    })
    .step(0.01_f32)
    .width(Length::Fill)
    .style(styles::slider(tokens));

    let zoom = row![
        text("Zoom").size(TEXT_ROW).color(tokens.text_secondary),
        tighten,
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    let rows: Vec<Element<'_, Message>> = vec![
        container(preview).center_x(Length::Fill).into(),
        zoom.into(),
        note(
            tokens,
            "Drag the picture to choose what the frame keeps.".to_owned(),
        ),
    ];

    sized_frame(
        app,
        "Adjust the picture",
        rows,
        actions(app, "Use picture", true, false),
        PICKER_WIDTH,
    )
}

/// The displays first, then the windows: sharing a whole screen is the common
/// case, and a long window list must not push it off the top.
fn source_list<'a>(
    app: &'a App,
    sources: &'a SourcesState,
    selected: Option<&'a SourceId>,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let listed = match sources {
        SourcesState::Loading => {
            return note(tokens, "Looking for screens…".to_owned());
        }
        SourcesState::Failed(error) => return complaint(tokens, error.clone()),
        SourcesState::Ready(listed) => listed,
    };
    if listed.is_empty() {
        return note(tokens, "Nothing to share.".to_owned());
    }

    let mut rows = column![].spacing(4).width(Length::Fill);
    for kind in [SourceKind::Display, SourceKind::Window] {
        let group: Vec<&Source> = listed.iter().filter(|source| source.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        rows = rows.push(widgets::section_label(
            if kind == SourceKind::Display {
                "Screens"
            } else {
                "Windows"
            },
            tokens,
        ));
        for source in group {
            rows = rows.push(source_row(app, source, selected == Some(&source.id)));
        }
    }

    scrollable(rows)
        .height(SOURCES_HEIGHT)
        .style(styles::scrollable(tokens))
        .into()
}

fn source_row<'a>(app: &'a App, source: &'a Source, picked: bool) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let line = row![
        text(source.title.clone())
            .size(TEXT_ROW)
            .color(tokens.text_primary),
        Space::new().width(Length::Fill),
        text(format!("{}×{}", source.width, source.height))
            .size(TEXT_ROW)
            .color(tokens.text_muted),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    button(line)
        .width(Length::Fill)
        .padding([6.0, 8.0])
        .style(styles::button::row_for(tokens, picked))
        .on_press(Message::Share(ShareMsg::PickSource(source.id.clone())))
        .into()
}

/// One attachment at its own size, shrunk to fit the window.
fn lightbox<'a>(app: &'a App, main: &'a MainState, id: i64) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let picture: Element<'_, Message> = match main.chat.images.get(&ImageKey::Attachment(id)) {
        Some(ImageState::Ready(handle)) => image(handle.clone())
            .content_fit(ContentFit::ScaleDown)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        Some(ImageState::Failed) => text("Image unavailable")
            .size(TEXT_BODY)
            .color(tokens.text_muted)
            .into(),
        Some(ImageState::Loading) | None => text("Loading image…")
            .size(TEXT_BODY)
            .color(tokens.text_muted)
            .into(),
    };

    // The picture takes its own presses, so only the dimmed ground around it
    // closes the lightbox.
    opaque(
        mouse_area(
            container(mouse_area(picture).on_press(Message::Noop))
                .center(Length::Fill)
                .padding(24)
                .style(styles::container::backdrop(tokens)),
        )
        .on_press(Message::Ui(UiMsg::CloseDialog)),
    )
}

/// One password field of the dialog below.
fn secure_field<'a>(
    tokens: &'a ThemeTokens,
    placeholder: &'static str,
    value: &'a str,
) -> TextInput<'a, Message> {
    text_input(placeholder, value)
        .secure(true)
        .padding(10)
        .width(Length::Fill)
        .style(styles::text_input(tokens))
}

/// Change password, which `update::auth` drives end to end.
fn change_password<'a>(
    app: &'a App,
    current: &'a str,
    new: &'a str,
    confirm: &'a str,
    error: Option<&'a str>,
    busy: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let mut rows: Vec<Element<'_, Message>> = vec![
        secure_field(tokens, "Current password", current)
            .id(Id::new(CURRENT_PASSWORD_ID))
            .on_input(|value| Message::Auth(AuthMsg::DialogCurrentChanged(value)))
            .into(),
        secure_field(tokens, "New password", new)
            .on_input(|value| Message::Auth(AuthMsg::DialogNewChanged(value)))
            .into(),
        secure_field(tokens, "Confirm new password", confirm)
            .on_input(|value| Message::Auth(AuthMsg::DialogConfirmChanged(value)))
            .on_submit(Message::Auth(AuthMsg::ChangePasswordSubmit))
            .into(),
    ];
    if let Some(error) = error {
        rows.push(complaint(tokens, error.to_owned()));
    }

    let mut action = button(text("Change password").size(TEXT_BODY))
        .padding([6.0, 14.0])
        .style(styles::button::primary(tokens));
    if !busy {
        action = action.on_press(Message::Auth(AuthMsg::ChangePasswordSubmit));
    }

    let actions = row![
        button(text("Cancel").size(TEXT_BODY))
            .padding([6.0, 14.0])
            .style(styles::button::secondary(tokens))
            .on_press(Message::Ui(UiMsg::CloseDialog)),
        action,
    ]
    .spacing(8);

    frame(app, "Change password", rows, actions.into())
}

fn category_name(main: &MainState, id: i64) -> String {
    main.server.categories.get(&id).map_or_else(
        || "this category".to_owned(),
        |category| category.name.clone(),
    )
}

fn role_name(main: &MainState, id: i64) -> String {
    main.server
        .roles
        .get(&id)
        .map_or_else(|| "this role".to_owned(), |role| role.name.clone())
}
