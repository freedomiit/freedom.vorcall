//! The message list and the composer: sending, editing, replying, reacting,
//! attaching, pasting, saving and the read cursor.
//!
//! Nothing here touches the disk or the clipboard on the UI thread: a file is
//! measured, read, encoded or written on a blocking thread and comes back as a
//! message. A file the server will keep streams straight off the disk into the
//! upload, and one too large for that is offered instead — either way, nothing
//! that will not fit in memory is ever read into it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;
use iced::widget::{Id, operation, scrollable, text_editor};
use vorcall_clipboard::Pasted;
use vorcall_core::connection::Blob;
use vorcall_core::{ChannelKind, Command, Endpoints, attachments, mentions};

use crate::app::message::{ChatMsg, Message, ToastKind, UiMsg};
use crate::app::state::chat::{ImageState, MESSAGE_MAX_CHARS, PendingTransfer, TransferKind};
use crate::app::state::rules::{
    CLIPBOARD_UNAVAILABLE, FILE_TOO_LARGE, NOTHING_TO_PASTE, describe, mention_query,
    mention_replace, plain_text,
};
use crate::app::state::server::channel_kind;
use crate::app::state::ui::{Dialog, TransferSource, TransferState};
use crate::app::{App, MainState, PendingUpload, Status};
use crate::view;
use crate::workers::images::{self, ImageKey};

pub fn update(app: &mut App, message: ChatMsg) -> Task<Message> {
    match message {
        // Every key press in the composer arrives as one of these, and the editor
        // owns its own buffer.
        ChatMsg::Editor(action) => {
            if let Some(main) = app.main_mut() {
                main.chat.composer.content.perform(action);
                let text = main.chat.composer.content.text();
                main.chat.composer.mention_query = mention_query(text.trim_end());
            }
            Task::none()
        }
        ChatMsg::Send => send(app),
        ChatMsg::LoadOlder => load_older(app),
        // Under `Anchor::End` the offset is measured from the end of the list, so
        // zero is the bottom.
        ChatMsg::Scrolled(viewport) => {
            let at_bottom = viewport.absolute_offset().y <= 1.0;
            // TEMPORARY: diagnostics for the scroll investigation, remove once resolved.
            tracing::debug!(
                offset_y = viewport.absolute_offset().y,
                viewport_h = viewport.bounds().height,
                content_h = viewport.content_bounds().height,
                at_bottom,
                "the message list scrolled"
            );
            scrolled(app, at_bottom)
        }
        ChatMsg::JumpToLatest => Task::batch([
            scrolled(app, true),
            operation::snap_to(
                Id::new(view::MESSAGES_ID),
                scrollable::RelativeOffset::START,
            ),
        ]),
        // The hover state is what shows a row's actions.
        ChatMsg::Hover(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.hovered = Some(id);
            }
            Task::none()
        }
        ChatMsg::Unhover(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.unhover(id);
            }
            Task::none()
        }
        ChatMsg::ReplyTo(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.composer.reply_to = Some(id);
                main.chat.composer.editing = None;
            }
            focus_composer()
        }
        ChatMsg::CancelReply => {
            if let Some(main) = app.main_mut() {
                main.chat.composer.reply_to = None;
            }
            Task::none()
        }
        ChatMsg::StartEdit(id) => start_edit(app, id),
        ChatMsg::CancelEdit => {
            if let Some(main) = app.main_mut() {
                main.chat.composer.finish_edit();
            }
            focus_composer()
        }
        ChatMsg::Delete(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.confirm_delete = Some(id);
            }
            Task::none()
        }
        ChatMsg::ConfirmDelete(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.confirm_delete = None;
                main.send_or_notice(Command::Delete { id });
            }
            Task::none()
        }
        ChatMsg::CancelDelete => {
            if let Some(main) = app.main_mut() {
                main.chat.confirm_delete = None;
            }
            Task::none()
        }
        ChatMsg::OpenReactions(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.reacting = id;
            }
            Task::none()
        }
        ChatMsg::React(message_id, emoji) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            let remove = main.chat.reacted(message_id, emoji, main.member_id);
            main.chat.reacting = None;
            main.send_or_notice(Command::React {
                message_id,
                emoji: emoji.to_owned(),
                remove,
            });
            Task::none()
        }
        // The palette stays open: one press per emoji, and the Close button or
        // Escape is what puts it away.
        ChatMsg::InsertEmoji(emoji) => {
            if let Some(main) = app.main_mut() {
                insert(main, emoji.to_owned());
            }
            focus_composer()
        }
        ChatMsg::MentionPick(username) => {
            if let Some(main) = app.main_mut() {
                let text = main.chat.composer.text();
                main.chat
                    .composer
                    .set_text(&mention_replace(text.trim_end(), &username));
                main.chat.composer.mention_query = None;
            }
            focus_composer()
        }
        ChatMsg::PickAttachment => Task::perform(pick_files(), |paths| {
            Message::Chat(ChatMsg::FilesPicked(paths))
        }),
        ChatMsg::FilesPicked(paths) => measure_all(paths, false),
        // The same dialog as the paperclip's, routed to the offer instead of
        // the upload: one message per file, so a multiple pick still works.
        ChatMsg::PickStream => Task::future(pick_files()).then(|paths| {
            Task::batch(
                paths
                    .into_iter()
                    .map(|path| Task::done(Message::Chat(ChatMsg::OfferFile(path)))),
            )
        }),
        // A file the reader chose to serve from this disk, whatever its size.
        ChatMsg::OfferFile(path) => measure(path, true),
        ChatMsg::FileMeasured {
            path,
            size,
            content_type,
            offer,
        } => on_measured(app, path, size, content_type, offer),
        ChatMsg::FileRead(result) => on_file_read(app, result),
        ChatMsg::RemovePendingAttachment(id) => {
            if let Some(main) = app.main_mut() {
                main.chat
                    .composer
                    .attachments
                    .retain(|attachment| attachment.id != id);
            }
            Task::none()
        }
        // The offer stands on the server; it is swept unlinked, and nothing was
        // uploaded to take back.
        ChatMsg::RemovePendingStream(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.composer.streams.retain(|file| file.id != id);
            }
            Task::none()
        }
        ChatMsg::CancelUpload(request_id) => {
            if let Some(main) = app.main_mut() {
                main.pending_uploads.remove(&request_id);
                main.chat.composer.finish_transfer(request_id);
                main.send_command(Command::CancelTransfer { request_id });
            }
            Task::none()
        }
        ChatMsg::OpenImage(id) => {
            app.ui.dialog = Some(Dialog::Image(id));
            app.ensure_image(ImageKey::Attachment(id))
        }
        ChatMsg::ImageDecoded(key, result) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            match result {
                Ok(handle) => {
                    main.chat.images.insert(key, ImageState::Ready(handle));
                }
                Err(error) => {
                    tracing::warn!(?key, %error, "cannot decode an image");
                    main.chat.images.insert(key, ImageState::Failed);
                }
            }
            Task::none()
        }
        ChatMsg::SaveFile(source) => save_file(app, source),
        ChatMsg::SaveDestination(source, Some(path)) => start_save(app, source, path),
        // A dialog the reader dismissed: nothing was asked for.
        ChatMsg::SaveDestination(_, None) => Task::none(),
        ChatMsg::SaveProgress {
            request_id,
            received,
            total,
        } => {
            advance_dialog(app, request_id, received, total);
            Task::none()
        }
        ChatMsg::SaveFinished { request_id, result } => on_saved(app, request_id, result),
        ChatMsg::CancelTransfer(request_id) => cancel_transfer(app, request_id),
        ChatMsg::ServeStream {
            stream_id,
            transfer_id,
            offset,
            length,
            path,
        } => {
            if let Some(main) = app.main_mut() {
                main.send_command(Command::ServeStream {
                    stream_id,
                    transfer_id,
                    offset,
                    length,
                    path,
                });
            }
            Task::none()
        }
        ChatMsg::CopyText(id) => {
            let Some(main) = app.main() else {
                return Task::none();
            };
            let Some(message) = main.chat.message(id) else {
                return Task::none();
            };
            iced::clipboard::write(plain_text(&message.text, &main.user_pairs))
        }
        ChatMsg::Paste => paste(app),
        ChatMsg::PasteRead(result) => on_pasted(app, result),
        // One row owns the selection at a time: the press that starts one takes
        // it away from whichever row had it.
        ChatMsg::StartSelection(id) => {
            if let Some(main) = app.main_mut() {
                main.chat.selecting = Some(id);
            }
            Task::none()
        }
        ChatMsg::OpenLink(url) => open_link(url),
        ChatMsg::MarkChannelRead(channel_id) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            if let Some(message_id) = main.chat.read_cursor(channel_id) {
                main.send_command(Command::MarkRead {
                    channel_id,
                    message_id,
                });
            }
            Task::none()
        }
    }
}

/// Sends what the composer holds, or edits the message it was started on.
fn send(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    match compose(main) {
        Ok(()) => focus_composer(),
        // A press with nothing behind it, and nothing to say about it.
        Err(None) => Task::none(),
        Err(Some(complaint)) => {
            app.toast(ToastKind::Error, complaint);
            focus_composer()
        }
    }
}

/// Hands the composer's message to the connection and empties it. `Err(Some)` is
/// what to say instead.
fn compose(main: &mut MainState) -> Result<(), Option<String>> {
    if !matches!(main.status, Status::Connected) {
        return Err(None);
    }
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Err(None);
    };
    // A voice channel is joined, never written in: the server refuses a `Send` to
    // one with INVALID_ARGUMENT.
    if main
        .server
        .channel(channel_id)
        .is_none_or(|channel| channel_kind(channel) == ChannelKind::Voice)
    {
        return Err(None);
    }

    // What goes on the wire is the token form, and that is what the server
    // measures against its own limit.
    let text = mentions::encode(main.chat.composer.text().trim(), &main.user_pairs);
    if text.chars().count() > MESSAGE_MAX_CHARS {
        return Err(Some(format!(
            "Message is too long (max {MESSAGE_MAX_CHARS} characters)"
        )));
    }

    let editing = main.chat.composer.editing;
    let command = match editing {
        // An edit never empties a message; deleting it is its own action.
        Some(id) if !text.is_empty() => Command::Edit { id, text },
        Some(_) => return Err(None),
        None if main.chat.composer.is_empty() => return Err(None),
        None => Command::Send {
            channel_id,
            text,
            reply_to_id: main.chat.composer.reply_to,
            attachment_ids: main
                .chat
                .composer
                .attachments
                .iter()
                .map(|attachment| attachment.id)
                .collect(),
            // The offers are already on the server; this is what links them.
            streamed_file_ids: main
                .chat
                .composer
                .streams
                .iter()
                .map(|file| file.id)
                .collect(),
        },
    };

    if !main.send_command(command) {
        main.notice = Some("Message not sent".to_owned());
        return Ok(());
    }
    if editing.is_some() {
        main.chat.composer.finish_edit();
    } else {
        main.chat.composer.clear();
    }
    main.notice = None;
    Ok(())
}

/// Asks for the page before the oldest message held.
fn load_older(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Task::none();
    };
    let before = {
        let Some(channel) = main.chat.channels.get(&channel_id) else {
            return Task::none();
        };
        if !channel.has_older || channel.loading_older {
            return Task::none();
        }
        match channel.oldest_id() {
            Some(before) => before,
            None => return Task::none(),
        }
    };

    if main.send_command(Command::LoadOlder { channel_id, before }) {
        if let Some(channel) = main.chat.channels.get_mut(&channel_id) {
            channel.loading_older = true;
        }
    } else {
        main.notice = Some("Not connected".to_owned());
    }
    Task::none()
}

/// Where the list now stands. Reaching the bottom is what reads the channel.
fn scrolled(app: &mut App, at_bottom: bool) -> Task<Message> {
    let focused = app.ui.focused;
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if let Some(channel) = main.chat.current_mut() {
        channel.at_bottom = at_bottom;
        if at_bottom {
            channel.pending_new = 0;
        }
    }
    if at_bottom && focused {
        main.chat.schedule_mark_read();
    }
    Task::none()
}

/// Puts one of my own messages back in the composer. Never a tombstone, and the
/// stored tokens go back to the `@name` that was typed.
fn start_edit(app: &mut App, id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let me = main.member_id;
    let Some(text) = main
        .chat
        .message(id)
        .filter(|message| message.author_id == me && !message.deleted)
        .map(|message| plain_text(&message.text, &main.user_pairs))
    else {
        return Task::none();
    };

    main.chat.composer.set_text(&text);
    main.chat.composer.editing = Some(id);
    main.chat.composer.reply_to = None;
    main.chat.composer.mention_query = None;
    focus_composer()
}

/// Puts `text` in the composer at the caret, the way the editor's own paste
/// would.
fn insert(main: &mut MainState, text: String) {
    main.chat
        .composer
        .content
        .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
            Arc::new(text),
        )));
    let text = main.chat.composer.content.text();
    main.chat.composer.mention_query = mention_query(text.trim_end());
}

/// Whether a file of this size is one the server keeps. Above the ceiling it is
/// offered from this disk instead, which is what makes a file of any size
/// sendable at all.
fn stored(size: u64) -> bool {
    size <= attachments::MAX_BYTES
}

fn measure_all(paths: Vec<PathBuf>, offer: bool) -> Task<Message> {
    Task::batch(paths.into_iter().map(|path| measure(path, offer)))
}

/// Reads one picked, dropped or pasted file's length off the disk, which is what
/// decides how it travels. Only the length: a file this large must never be read
/// into memory to be looked at.
fn measure(path: PathBuf, offer: bool) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || {
            let metadata =
                std::fs::metadata(&path).map_err(|e| format!("cannot read that file: {e}"))?;
            // A folder can be dropped on the window as easily as a file, and
            // it has a length of its own that says nothing.
            if !metadata.is_file() {
                return Err(format!("{} is not a file", file_name_of(&path)));
            }
            let content_type = attachments::guess_type(&path, None);
            Ok::<_, String>((path, metadata.len(), content_type))
        }),
        move |joined| match joined.unwrap_or_else(|e| Err(e.to_string())) {
            Ok((path, size, content_type)) => Message::Chat(ChatMsg::FileMeasured {
                path,
                size,
                content_type,
                offer,
            }),
            Err(error) => Message::Ui(UiMsg::Toast(ToastKind::Error, error)),
        },
    )
}

/// One measured file, on its way: stored on the server, or offered from here.
fn on_measured(
    app: &mut App,
    path: PathBuf,
    size: u64,
    content_type: String,
    offer: bool,
) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let outcome = if offer || !stored(size) {
        offer_file(main, path, size, content_type)
    } else {
        upload_file(main, path, size, content_type)
    };
    if let Err(complaint) = outcome {
        app.toast(ToastKind::Error, complaint);
    }
    Task::none()
}

/// Uploads one pasted picture, once it is in memory.
fn on_file_read(app: &mut App, result: Result<(String, Blob), String>) -> Task<Message> {
    let (file_name, bytes) = match result {
        Ok(read) => read,
        Err(error) => {
            app.toast(ToastKind::Error, error);
            return Task::none();
        }
    };
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // The name carries the extension, and the bytes carry the magic number:
    // between them there is nothing left to ask the file system.
    let content_type = attachments::guess_type(Path::new(&file_name), Some(&bytes));
    if let Err(complaint) = upload_bytes(main, file_name, content_type, bytes) {
        app.toast(ToastKind::Error, complaint);
    }
    Task::none()
}

/// Streams one file up from the disk. `Err` is what to say about a file that
/// cannot go.
fn upload_file(
    main: &mut MainState,
    path: PathBuf,
    size: u64,
    content_type: String,
) -> Result<(), String> {
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Ok(());
    };
    free_slot(main)?;

    let request_id = begin(
        main,
        channel_id,
        file_name_of(&path),
        size,
        TransferKind::Upload,
        None,
    );
    tracing::debug!(
        request_id,
        channel_id,
        size,
        content_type,
        "streaming an attachment up from disk"
    );
    if !main.send_command(Command::UploadAttachmentFile {
        request_id,
        channel_id,
        path,
        content_type,
    }) {
        rollback(main, request_id);
        main.notice = Some("Not connected".to_owned());
    }
    Ok(())
}

/// Uploads bytes already in hand: a pasted picture, which has no file of its
/// own.
fn upload_bytes(
    main: &mut MainState,
    file_name: String,
    content_type: String,
    bytes: Blob,
) -> Result<(), String> {
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Ok(());
    };
    let size = bytes.len() as u64;
    if !stored(size) {
        return Err(FILE_TOO_LARGE.to_owned());
    }
    free_slot(main)?;

    let request_id = begin(
        main,
        channel_id,
        file_name.clone(),
        size,
        TransferKind::Upload,
        None,
    );
    tracing::debug!(
        request_id,
        channel_id,
        size,
        content_type,
        "uploading a pasted picture"
    );
    if !main.send_command(Command::UploadAttachment {
        request_id,
        channel_id,
        file_name,
        content_type,
        bytes,
    }) {
        rollback(main, request_id);
        main.notice = Some("Not connected".to_owned());
    }
    Ok(())
}

/// Offers one local file: the server keeps the record, this client keeps the
/// bytes and serves them on demand.
fn offer_file(
    main: &mut MainState,
    path: PathBuf,
    size: u64,
    content_type: String,
) -> Result<(), String> {
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Ok(());
    };
    // The record states the size as an `i64`; nothing in the protocol can name a
    // file larger than that.
    if i64::try_from(size).is_err() {
        return Err(FILE_TOO_LARGE.to_owned());
    }
    free_slot(main)?;

    let request_id = begin(
        main,
        channel_id,
        file_name_of(&path),
        size,
        TransferKind::Offer,
        Some(path.clone()),
    );
    tracing::debug!(
        request_id,
        channel_id,
        size,
        content_type,
        "offering a file from this disk"
    );
    if !main.send_command(Command::OfferStream {
        request_id,
        channel_id,
        path,
        content_type,
        size,
    }) {
        rollback(main, request_id);
        main.notice = Some("Not connected".to_owned());
    }
    Ok(())
}

/// Whether the message being written can carry one more file. What is still on
/// its way counts: a fifth file is refused while the fourth is uploading.
fn free_slot(main: &MainState) -> Result<(), String> {
    if main.chat.composer.slots_used() >= attachments::MAX_PER_MESSAGE {
        return Err(format!(
            "At most {} files per message",
            attachments::MAX_PER_MESSAGE
        ));
    }
    Ok(())
}

/// Books the slot one transfer is about to take: the chip the composer draws,
/// the record its answer is matched against, and the handle it travels under.
fn begin(
    main: &mut MainState,
    channel_id: i64,
    file_name: String,
    total: u64,
    kind: TransferKind,
    path: Option<PathBuf>,
) -> u64 {
    let request_id = main.next_request_id();
    main.pending_uploads.insert(
        request_id,
        PendingUpload {
            channel_id,
            file_name: file_name.clone(),
            path,
        },
    );
    main.chat.composer.uploading.push(PendingTransfer {
        request_id,
        file_name,
        total,
        done: 0,
        kind,
    });
    request_id
}

/// Gives that slot back, for a command the loop would not take.
fn rollback(main: &mut MainState, request_id: u64) {
    main.pending_uploads.remove(&request_id);
    main.chat.composer.finish_transfer(request_id);
}

/// Reports the read cursor the debounce has been holding.
pub fn flush_mark_read(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let Some((channel_id, message_id)) = main.chat.mark_read_due.take() else {
        return Task::none();
    };
    main.send_command(Command::MarkRead {
        channel_id,
        message_id,
    });
    Task::none()
}

fn focus_composer() -> Task<Message> {
    operation::focus(Id::new(view::COMPOSER_ID))
}

/// The native file dialog, unfiltered: any file of any type is an attachment
/// now. Cancelling it picks nothing, which is not an error.
async fn pick_files() -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .pick_files()
        .await
        .map(|handles| {
            handles
                .iter()
                .map(|handle| handle.path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

/// The name a file travels under. It is decoration: a path with nothing usable
/// in it is not worth refusing an upload over.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("file")
        .to_owned()
}

/// Reads the clipboard on a blocking thread: every backend waits on another
/// application. A session this client cannot read falls back to iced's own text
/// clipboard, so an ordinary paste keeps working.
fn paste(app: &mut App) -> Task<Message> {
    let Some(clipboard) = app.ensure_clipboard() else {
        // A window whose display has not been read yet cannot open one; asking
        // now is what makes the next paste a whole one.
        return Task::batch([app.probe_display(), paste_text()]);
    };
    Task::perform(
        tokio::task::spawn_blocking(move || {
            // A read that panicked poisoned the lock; the handle behind it is
            // still the one way in.
            let clipboard = clipboard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            clipboard.read().map_err(|e| e.to_string())
        }),
        |joined| {
            Message::Chat(ChatMsg::PasteRead(
                joined.unwrap_or_else(|e| Err(e.to_string())),
            ))
        },
    )
}

/// iced's own clipboard, which is text and nothing else.
fn paste_text() -> Task<Message> {
    iced::clipboard::read().map(|text| match text {
        Some(text) if !text.is_empty() => Message::Chat(ChatMsg::PasteRead(Ok(Pasted::Text(text)))),
        _ => Message::Ui(UiMsg::Toast(ToastKind::Info, NOTHING_TO_PASTE.to_owned())),
    })
}

/// What the clipboard held. Files travel like picked ones, a picture is
/// uploaded, and everything else is text the editor would have pasted itself.
fn on_pasted(app: &mut App, result: Result<Pasted, String>) -> Task<Message> {
    let pasted = match result {
        Ok(pasted) => pasted,
        Err(error) => {
            tracing::warn!(%error, "cannot read the clipboard");
            app.toast(ToastKind::Error, CLIPBOARD_UNAVAILABLE.to_owned());
            return paste_text();
        }
    };
    tracing::debug!(?pasted, "pasted");

    match pasted {
        Pasted::Files(paths) => measure_all(paths, false),
        // A screenshot has no name of its own; the extension is what the type is
        // read back from.
        Pasted::Png(bytes) => Task::done(Message::Chat(ChatMsg::FileRead(Ok((
            pasted_name(),
            Blob::from(bytes),
        ))))),
        Pasted::Rgba {
            width,
            height,
            data,
        } => encode_pasted(width, height, data),
        Pasted::Text(text) => {
            if let Some(main) = app.main_mut() {
                insert(main, text);
            }
            focus_composer()
        }
        Pasted::Nothing => paste_text(),
    }
}

/// Encodes a raw pasted bitmap as a PNG, off the UI thread: a screen's worth of
/// pixels is megabytes of work.
fn encode_pasted(width: u32, height: u32, data: Vec<u8>) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || {
            images::encode_png(width, height, &data).map(|png| (pasted_name(), Blob::from(png)))
        }),
        |joined| {
            Message::Chat(ChatMsg::FileRead(
                joined.unwrap_or_else(|e| Err(e.to_string())),
            ))
        },
    )
}

/// What a pasted picture is called, since a clipboard carries no name.
fn pasted_name() -> String {
    format!(
        "pasted-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

/// Asks where to put one file before a byte of it moves.
fn save_file(app: &mut App, source: TransferSource) -> Task<Message> {
    let file_name = app
        .main()
        .and_then(|main| described(main, source))
        .map(|(file_name, _)| file_name)
        .unwrap_or_default();
    Task::perform(save_dialog(file_name), move |path| {
        Message::Chat(ChatMsg::SaveDestination(source, path))
    })
}

/// The native save dialog. Dismissing it puts nothing anywhere, which is not an
/// error.
async fn save_dialog(file_name: String) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_file_name(file_name)
        .save_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// The name and the length one message says a file has.
fn described(main: &MainState, source: TransferSource) -> Option<(String, u64)> {
    let messages = main
        .chat
        .channels
        .values()
        .flat_map(|channel| channel.messages.values());
    match source {
        TransferSource::Attachment(id) => messages
            .flat_map(|message| message.attachments.iter())
            .find(|attachment| attachment.id == id)
            .map(|attachment| (attachment.file_name.clone(), size_of(attachment.size))),
        TransferSource::Stream(id) => messages
            .flat_map(|message| message.streamed_files.iter())
            .find(|file| file.id == id)
            .map(|file| (file.file_name.clone(), size_of(file.size))),
    }
}

/// A length the wire states as an `i64`, as the window counts it.
fn size_of(size: i64) -> u64 {
    u64::try_from(size).unwrap_or_default()
}

/// Starts one download onto the disk and puts the dialog that follows it up.
fn start_save(app: &mut App, source: TransferSource, path: PathBuf) -> Task<Message> {
    let (file_name, total) = app
        .main()
        .and_then(|main| described(main, source))
        .unwrap_or_else(|| (file_name_of(&path), 0));

    let (request_id, task) = match source {
        TransferSource::Stream(id) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            let request_id = main.next_request_id();
            if !main.send_command(Command::FetchStream {
                request_id,
                id,
                save_to: path,
            }) {
                app.toast(ToastKind::Error, "Not connected".to_owned());
                return Task::none();
            }
            (request_id, Task::none())
        }
        // A stored attachment has no download-to-disk command in the connection
        // loop, so the window runs this one itself, straight onto the disk.
        TransferSource::Attachment(id) => {
            let Some(token) = app
                .session
                .as_ref()
                .map(|session| session.access_token.clone())
            else {
                app.toast(ToastKind::Error, "Sign in again to download".to_owned());
                return Task::none();
            };
            let endpoints = app.endpoints.clone();
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            let request_id = main.next_request_id();
            let (task, handle) = download(endpoints, token, request_id, id, path).abortable();
            main.pending_saves.insert(request_id, handle);
            (request_id, task)
        }
    };

    app.ui.dialog = Some(Dialog::Transfer {
        source,
        request_id,
        file_name,
        received: 0,
        total,
        state: TransferState::Running,
    });
    task
}

/// One attachment onto the disk, run in the window. The bytes never pass
/// through memory whole: a stored file may be gigabytes.
///
/// The token is a clone taken now, like a problem report's: a challenge comes
/// back as a failure the reader can ask for again.
fn download(
    endpoints: Endpoints,
    token: String,
    request_id: u64,
    id: i64,
    path: PathBuf,
) -> Task<Message> {
    Task::run(
        iced::stream::channel(16, async move |mut output| {
            let mut reports = output.clone();
            let result = attachments::download_to_path(
                &endpoints,
                &token,
                id,
                &path,
                move |received, total| {
                    // A full queue only means the window is behind on a number
                    // the next report replaces anyway.
                    let _ = reports.try_send(Message::Chat(ChatMsg::SaveProgress {
                        request_id,
                        received,
                        total,
                    }));
                },
            )
            .await;

            let _ = futures::SinkExt::send(
                &mut output,
                Message::Chat(ChatMsg::SaveFinished {
                    request_id,
                    result: result.map(|()| path).map_err(|failure| describe(&failure)),
                }),
            )
            .await;
        }),
        std::convert::identity,
    )
}

fn on_saved(app: &mut App, request_id: u64, result: Result<PathBuf, String>) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        main.pending_saves.remove(&request_id);
    }
    match result {
        Ok(path) => {
            tracing::info!(request_id, path = %path.display(), "an attachment was saved");
            finish_dialog(app, request_id, TransferState::Done(path));
        }
        Err(error) => {
            tracing::warn!(request_id, %error, "an attachment was not saved");
            finish_dialog(app, request_id, TransferState::Failed(error));
        }
    }
    Task::none()
}

/// Gives up on the download the transfer dialog is following.
fn cancel_transfer(app: &mut App, request_id: u64) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        match main.pending_saves.remove(&request_id) {
            // The window is running this one: dropping the task is what stops
            // it, and the part-file it leaves is what a retry resumes from.
            Some(handle) => handle.abort(),
            None => {
                main.send_command(Command::CancelTransfer { request_id });
            }
        }
    }
    if matches!(&app.ui.dialog, Some(Dialog::Transfer { request_id: open, .. }) if *open == request_id)
    {
        app.ui.dialog = None;
    }
    Task::none()
}

/// Counts one report into the transfer dialog in place: a download can run for
/// hours, and reopening the dialog would take whatever the reader was doing
/// with it away every megabyte.
pub fn advance_dialog(app: &mut App, request_id: u64, received: u64, total: u64) {
    if let Some(Dialog::Transfer {
        request_id: open,
        received: counted,
        total: stated,
        ..
    }) = app.dialog_mut()
        && *open == request_id
    {
        *counted = received;
        // The record's length is what the dialog opened with; a transfer that
        // knows better says so, and one that does not says zero.
        if total > 0 {
            *stated = total;
        }
    }
}

/// Where a download ended, in the dialog that was following it. Both terminal
/// states stay up: a transfer that took an hour is worth a sentence.
pub fn finish_dialog(app: &mut App, request_id: u64, ended: TransferState) {
    if let Some(Dialog::Transfer {
        request_id: open,
        received,
        total,
        state,
        ..
    }) = app.dialog_mut()
        && *open == request_id
    {
        if matches!(ended, TransferState::Done(_)) && *total > 0 {
            *received = *total;
        }
        *state = ended;
    }
}

/// Hands one link to the system browser, off the UI thread.
///
/// Message text is written by other people, so only the two web schemes ever
/// leave here: a `file:` or a `javascript:` would be pointed at the reader's own
/// machine.
fn open_link(url: String) -> Task<Message> {
    if !is_web_link(&url) {
        tracing::debug!("ignoring a link that is neither http nor https");
        return Task::none();
    }
    Task::perform(
        tokio::task::spawn_blocking(move || open_in_browser(&url)),
        |joined| {
            match joined.unwrap_or_else(|e| Err(std::io::Error::other(e.to_string()))) {
                Ok(()) => (),
                Err(e) => tracing::warn!(error = %e, "cannot open a link"),
            }
            Message::Noop
        },
    )
}

/// Whether a URL may be opened: `http` or `https`, with something after the
/// scheme.
fn is_web_link(url: &str) -> bool {
    ["http://", "https://"].into_iter().any(|scheme| {
        url.len() > scheme.len()
            && url
                .get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    opener().arg(url).status().map(|_| ())
}

#[cfg(target_os = "linux")]
fn opener() -> std::process::Command {
    std::process::Command::new("xdg-open")
}

#[cfg(target_os = "macos")]
fn opener() -> std::process::Command {
    std::process::Command::new("open")
}

/// Not `cmd /c start`: that goes through the shell, which eats the `&`s of a
/// query string. The protocol handler takes the whole address as one argument.
#[cfg(windows)]
fn opener() -> std::process::Command {
    let mut command = std::process::Command::new("rundll32");
    command.arg("url.dll,FileProtocolHandler");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PROTOCOL.md` § Limits: the server stores up to 2 GiB, and a file above
    /// that is offered from the sender's own disk instead.
    #[test]
    fn the_size_is_what_decides_how_a_file_travels() {
        assert_eq!(attachments::MAX_BYTES, 2 * 1024 * 1024 * 1024);

        assert!(stored(0));
        assert!(stored(1));
        assert!(stored(8 * 1024 * 1024));
        assert!(stored(attachments::MAX_BYTES - 1));
        assert!(stored(attachments::MAX_BYTES));

        assert!(!stored(attachments::MAX_BYTES + 1));
        assert!(!stored(u64::MAX));
    }

    #[test]
    fn only_the_two_web_schemes_are_opened() {
        assert!(is_web_link("http://example.com"));
        assert!(is_web_link("https://example.com/a?b=1&c=2"));
        // A scheme is not spelled in any particular case.
        assert!(is_web_link("HTTPS://example.com"));
        assert!(is_web_link("HtTp://example.com"));
    }

    #[test]
    fn anything_that_is_not_a_web_link_is_ignored() {
        for refused in [
            "",
            "http://",
            "https://",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com",
            "data:text/html,<script>",
            "vorcall://channel/1",
            "xhttps://example.com",
            " https://example.com",
            "example.com",
        ] {
            assert!(!is_web_link(refused), "{refused} must not be opened");
        }
    }
}
