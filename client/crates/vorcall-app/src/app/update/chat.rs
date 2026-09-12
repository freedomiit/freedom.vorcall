//! The message list and the composer: sending, editing, replying, reacting,
//! attaching, and the read cursor.
//!
//! Nothing here reads a file or decodes an image on the UI thread: both go
//! through a blocking task and come back as a message.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;
use iced::widget::{Id, operation, scrollable, text_editor};
use vorcall_core::connection::Blob;
use vorcall_core::{ChannelKind, Command, attachments, mentions};

use crate::app::message::{ChatMsg, Message, ToastKind};
use crate::app::state::chat::{ImageState, MESSAGE_MAX_CHARS};
use crate::app::state::rules::{
    NOT_AN_IMAGE, TOO_LARGE, mention_query, mention_replace, plain_text,
};
use crate::app::state::server::channel_kind;
use crate::app::state::ui::Dialog;
use crate::app::{App, MainState, PendingUpload, Status};
use crate::view;
use crate::workers::images::ImageKey;

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
                main.chat
                    .composer
                    .content
                    .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                        Arc::new(emoji.to_owned()),
                    )));
                let text = main.chat.composer.content.text();
                main.chat.composer.mention_query = mention_query(text.trim_end());
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
        ChatMsg::PickAttachment => Task::perform(pick_images(), |paths| {
            Message::Chat(ChatMsg::FilesPicked(paths))
        }),
        ChatMsg::FilesPicked(paths) => Task::batch(paths.into_iter().map(read_file)),
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
        ChatMsg::CopyText(id) => {
            let Some(main) = app.main() else {
                return Task::none();
            };
            let Some(message) = main.chat.message(id) else {
                return Task::none();
            };
            iced::clipboard::write(plain_text(&message.text, &main.user_pairs))
        }
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

/// Uploads one picked or dropped file, once it is in memory.
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
    if let Err(complaint) = upload(main, file_name, bytes) {
        app.toast(ToastKind::Error, complaint);
    }
    Task::none()
}

/// Starts one attachment upload for the channel in view. `Err` is what to say
/// about a file that cannot go.
fn upload(main: &mut MainState, file_name: String, bytes: Blob) -> Result<(), String> {
    let Some(channel_id) = main.chat.current.channel_id() else {
        return Ok(());
    };
    let Some(content_type) = attachments::sniff(&bytes) else {
        return Err(NOT_AN_IMAGE.to_owned());
    };
    if bytes.len() > attachments::MAX_BYTES {
        return Err(TOO_LARGE.to_owned());
    }
    let held = main.chat.composer.attachments.len() + main.chat.composer.uploading;
    if held >= attachments::MAX_PER_MESSAGE {
        return Err(format!(
            "At most {} images per message",
            attachments::MAX_PER_MESSAGE
        ));
    }

    let request_id = main.next_request_id();
    tracing::debug!(
        request_id,
        channel_id,
        bytes = bytes.len(),
        content_type,
        "uploading an attachment"
    );
    main.pending_uploads.insert(
        request_id,
        PendingUpload {
            channel_id,
            file_name: file_name.clone(),
        },
    );
    main.chat.composer.uploading += 1;

    if !main.send_command(Command::UploadAttachment {
        request_id,
        channel_id,
        file_name,
        content_type,
        bytes,
    }) {
        main.pending_uploads.remove(&request_id);
        main.chat.composer.uploading -= 1;
        main.notice = Some("Not connected".to_owned());
    }
    Ok(())
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

/// The native file dialog. Cancelling it picks nothing, which is not an error.
async fn pick_images() -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
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

/// Reads one picked or dropped file off the disk, on a blocking thread.
fn read_file(path: PathBuf) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || read_image(&path)),
        |joined| {
            let read = joined.unwrap_or_else(|e| Err(e.to_string()));
            Message::Chat(ChatMsg::FileRead(
                read.map(|(name, bytes)| (name, Blob::from(bytes))),
            ))
        },
    )
}

fn read_image(path: &Path) -> Result<(String, Vec<u8>), String> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("image")
        .to_owned();
    // Asked before the read: nothing the server would refuse belongs in memory.
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read that file: {e}"))?
        .len();
    if size > attachments::MAX_BYTES as u64 {
        return Err(TOO_LARGE.to_owned());
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read that file: {e}"))?;
    Ok((name, bytes))
}
