//! The shared sticker library: sending one, and adding, renaming and removing
//! the pictures behind them.
//!
//! The bytes are fetched once per sticker per machine and go through the same
//! on-disk cache every other picture does — `App::ensure_image` under
//! [`ImageKey::Sticker`] — so nothing here decodes anything itself and the
//! interface thread never reads a file.

use std::path::Path;

use iced::Task;
use vorcall_core::connection::{Blob, Command};
use vorcall_core::stickers as api;
use vorcall_core::{ApiFailure, Event, Sticker, attachments};

use crate::app::App;
use crate::app::message::{Message, StickerMsg, ToastKind};
use crate::app::state::rules::{describe, format_bytes};
use crate::app::state::sticker;
use crate::app::state::ui::{NAME_MAX, validate_name};
use crate::app::update::settings::CANCELLED;
use crate::workers::images::ImageKey;

/// What the file dialog offers, which is the four types the endpoint takes.
const STICKER_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// What a picked file with no name of its own is called. One scalar at least and
/// well under the 32 the server's name grammar allows, so it is never refused.
const DEFAULT_NAME: &str = "sticker";

pub fn update(app: &mut App, message: StickerMsg) -> Task<Message> {
    match message {
        StickerMsg::TogglePicker => {
            let opened = match app.main_mut() {
                Some(main) => {
                    main.sticker.picker_open = !main.sticker.picker_open;
                    main.sticker.picker_open
                }
                None => false,
            };
            // The thumbnails are only worth their bytes once somebody is looking
            // at them: opening the picker is what asks for them.
            if opened {
                return ensure_thumbnails(app);
            }
            Task::none()
        }
        StickerMsg::ClosePicker => {
            close_picker(app);
            Task::none()
        }
        StickerMsg::Send(sticker_id) => send(app, sticker_id),
        StickerMsg::PickFile => Task::perform(pick_file(), |result| {
            Message::Sticker(StickerMsg::Picked(result))
        }),
        StickerMsg::Picked(Ok((name, content_type, bytes))) => {
            upload(app, name, content_type, bytes)
        }
        StickerMsg::Picked(Err(error)) => complain(app, error),
        StickerMsg::Uploaded(result) => uploaded(app, result),
        StickerMsg::RenameDraft(sticker_id, name) => {
            if let Some(main) = app.main_mut() {
                main.admin.sticker_names.insert(sticker_id, name);
            }
            Task::none()
        }
        StickerMsg::RenameSave(sticker_id) => rename(app, sticker_id),
        StickerMsg::Delete(sticker_id) => {
            app.ui.dialog = None;
            if let Some(main) = app.main_mut() {
                main.admin.sticker_names.remove(&sticker_id);
                main.send_or_notice(Command::DeleteSticker { sticker_id });
            }
            Task::none()
        }
    }
}

/// The library's own frames. Everything else the connection reports belongs
/// elsewhere.
pub fn on_event(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::StickerUpserted { sticker } => {
            let id = sticker.id;
            let looking = match app.main_mut() {
                Some(main) => {
                    main.sticker.upsert(sticker);
                    main.sticker.picker_open
                }
                None => false,
            };
            if looking {
                return app.ensure_image(ImageKey::Sticker(id));
            }
            Task::none()
        }
        Event::StickerDeleted { sticker_id } => {
            if let Some(main) = app.main_mut() {
                main.sticker.remove(sticker_id);
                main.admin.sticker_names.remove(&sticker_id);
            }
            Task::none()
        }
        _ => Task::none(),
    }
}

/// The library the snapshot carries, replacing whatever the last connection
/// left.
pub fn on_snapshot(app: &mut App, stickers: Vec<Sticker>) {
    if let Some(main) = app.main_mut() {
        main.sticker.apply_snapshot(stickers);
    }
}

/// Starts loading every thumbnail the picker and the settings page draw. Each
/// one is at most [`api::MAX_BYTES`], and a key already held costs nothing.
pub fn ensure_thumbnails(app: &mut App) -> Task<Message> {
    let keys: Vec<ImageKey> = app
        .main()
        .map(|main| {
            main.sticker
                .library
                .keys()
                .map(|id| ImageKey::Sticker(*id))
                .collect()
        })
        .unwrap_or_default();
    app.ensure_images(keys)
}

/// Sends one sticker as a message of its own. The draft is left alone — only the
/// reply the composer was holding travels with it, and goes with it.
fn send(app: &mut App, sticker_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.sticker.picker_open = false;

    let Some(channel_id) = main.chat.current.channel_id() else {
        return Task::none();
    };
    let reply_to_id = main.chat.composer.reply_to;
    if main.send_command(sticker::send(channel_id, sticker_id, reply_to_id)) {
        main.chat.composer.reply_to = None;
        main.notice = None;
    } else {
        main.notice = Some("Message not sent".to_owned());
    }
    Task::none()
}

/// Puts one picked picture on the server. The library itself arrives as the
/// `StickerUpserted` everybody gets.
fn upload(app: &mut App, name: String, content_type: String, bytes: Blob) -> Task<Message> {
    let Some(session) = app.session.as_ref() else {
        return complain(app, "Not signed in".to_owned());
    };
    let token = session.access_token.clone();
    let endpoints = app.endpoints.clone();
    if let Some(main) = app.main_mut() {
        main.sticker.uploading = true;
    }

    Task::perform(
        async move {
            api::upload(&endpoints, &token, &name, &content_type, bytes)
                .await
                .map_err(|failure| upload_failure(&failure))
        },
        |result| Message::Sticker(StickerMsg::Uploaded(result)),
    )
}

fn uploaded(app: &mut App, result: Result<Sticker, String>) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        main.sticker.uploading = false;
    }
    match result {
        Ok(sticker) => {
            app.toast(ToastKind::Info, format!("Added {}", sticker.name));
            Task::none()
        }
        Err(error) => complain(app, error),
    }
}

fn rename(app: &mut App, sticker_id: i64) -> Task<Message> {
    let draft = app
        .main()
        .and_then(|main| main.admin.sticker_names.get(&sticker_id).cloned())
        .unwrap_or_default();
    let name = match validate_name(&draft, "sticker name") {
        Ok(name) => name,
        Err(complaint) => return complain(app, complaint),
    };

    if let Some(main) = app.main_mut() {
        main.admin.sticker_names.remove(&sticker_id);
        main.send_or_notice(Command::UpdateSticker { sticker_id, name });
    }
    Task::none()
}

/// The file dialog, then the bytes off the disk — neither on the interface
/// thread. What the server would refuse is refused here, so a megabyte of
/// somebody's wallpaper is never uploaded to be told no.
async fn pick_file() -> Result<(String, String, Blob), String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .add_filter("Images", &STICKER_EXTENSIONS)
        .pick_file()
        .await
    else {
        return Err(CANCELLED.to_owned());
    };

    let path = handle.path().to_path_buf();
    tokio::task::spawn_blocking(move || read_sticker(&path))
        .await
        .map_err(|e| e.to_string())?
}

fn read_sticker(path: &Path) -> Result<(String, String, Blob), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;

    let size = bytes.len() as u64;
    if size > api::MAX_BYTES {
        return Err(format!(
            "That picture is {} — a sticker may be at most {}",
            format_bytes(size),
            format_bytes(api::MAX_BYTES)
        ));
    }

    // The magic number rather than the extension: a `.png` that is really
    // something else would be refused by the server, which sniffs the same way.
    let Some(content_type) =
        attachments::sniff_image(&bytes).filter(|found| api::is_accepted(found))
    else {
        return Err("A sticker must be a PNG, JPEG, GIF or WebP".to_owned());
    };

    Ok((
        sticker_name(path),
        content_type.to_owned(),
        Blob::from(bytes),
    ))
}

/// What a picked file's sticker is called: its own name without the extension,
/// trimmed to what the server's name grammar takes.
///
/// Not [`Path::file_stem`]: that answers a dotfile's whole name, so a file
/// called `.png` would be named after its extension. Cutting at the last dot
/// instead leaves nothing for such a file, which is what [`DEFAULT_NAME`] is
/// for — as it is for a path that names no file at all.
fn sticker_name(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let stem = match file_name.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => file_name,
    };

    let name: String = stem
        .chars()
        .filter(|letter| !letter.is_control())
        .take(NAME_MAX)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        DEFAULT_NAME.to_owned()
    } else {
        name.to_owned()
    }
}

/// What the upload endpoint's refusals mean here. The shared [`describe`] reads
/// a 409 as a taken username, which is another endpoint's answer entirely.
fn upload_failure(failure: &ApiFailure) -> String {
    match failure {
        ApiFailure::Status(409, _) => "The server's sticker library is full".to_owned(),
        ApiFailure::Status(413, _) => format!(
            "That picture is too large — a sticker may be at most {}",
            format_bytes(api::MAX_BYTES)
        ),
        ApiFailure::Status(415, _) => "A sticker must be a PNG, JPEG, GIF or WebP".to_owned(),
        other => describe(other),
    }
}

pub fn close_picker(app: &mut App) {
    if let Some(main) = app.main_mut() {
        main.sticker.picker_open = false;
    }
}

/// A cancelled dialog picks nothing, which is not a failure anybody needs to be
/// told about.
fn complain(app: &mut App, error: String) -> Task<Message> {
    if error != CANCELLED {
        app.toast(ToastKind::Error, error);
    }
    Task::none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picked_files_own_name_becomes_the_stickers() {
        assert_eq!(sticker_name(Path::new("/tmp/wave.png")), "wave");
        assert_eq!(sticker_name(Path::new("/tmp/big wave.webp")), "big wave");
        // Only the last dot is the extension's.
        assert_eq!(sticker_name(Path::new("/tmp/v1.2.gif")), "v1.2");
        // A file that carries no extension still carries a name.
        assert_eq!(sticker_name(Path::new("/tmp/wave")), "wave");
    }

    /// A file whose name is all extension, a dotfile, and a path that names no
    /// file at all: each would leave the sticker nameless, and the server's name
    /// grammar takes nothing empty.
    #[test]
    fn a_file_with_no_name_of_its_own_falls_back() {
        for path in ["/tmp/.png", "/tmp/.gitignore", "/tmp/  .png", "/"] {
            let name = sticker_name(Path::new(path));
            assert_eq!(name, DEFAULT_NAME, "{path} should fall back");
            assert!(!name.trim().is_empty());
            assert!(name.chars().count() <= NAME_MAX);
        }
    }

    /// The server takes 32 scalars; a longer file name is cut rather than
    /// refused.
    #[test]
    fn a_long_file_name_is_cut_to_the_name_grammars_ceiling() {
        let long = format!("/tmp/{}.png", "a".repeat(NAME_MAX + 10));
        assert_eq!(sticker_name(Path::new(&long)).chars().count(), NAME_MAX);
    }

    /// A full library and an oversized picture are this endpoint's own answers;
    /// the shared wording belongs to the account endpoints.
    #[test]
    fn the_upload_refusals_read_as_stickers_own() {
        assert_eq!(
            upload_failure(&ApiFailure::Status(409, String::new())),
            "The server's sticker library is full"
        );
        assert!(
            upload_failure(&ApiFailure::Status(413, String::new())).contains("too large"),
            "a 413 says the picture is too large"
        );
        assert_eq!(
            upload_failure(&ApiFailure::Status(415, String::new())),
            "A sticker must be a PNG, JPEG, GIF or WebP"
        );
    }
}
