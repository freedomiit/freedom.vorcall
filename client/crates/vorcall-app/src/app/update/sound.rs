//! The shared soundpad: playing a clip, the library behind it, and importing a
//! new one.
//!
//! Nothing is heard off the press. `PlaySound` asks, and the server's
//! `SoundPlayed` is what every client plays from — the presser included — so a
//! refusal is never contradicted by audio only this machine heard.
//!
//! The bytes are fetched once per clip per machine and cached on disk. Every
//! read, decode and encode here happens on a blocking task: the interface thread
//! neither touches the disk nor a sample.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;
use vorcall_core::connection::{Blob, Command};
use vorcall_core::sounds as api;
use vorcall_core::{Endpoints, Event, Sound};

use crate::app::App;
use crate::app::message::{Message, SoundMsg, ToastKind};
use crate::app::state::rules::describe;
use crate::app::state::sound::{TrimDrag, TrimEdge, TrimState, drag_to, span_ms};
use crate::app::state::ui::{Dialog, NAME_MAX, validate_name};
use crate::app::update::settings::CANCELLED;
use crate::workers::voice::AudioCommand;
use crate::workers::{clips, sounds};

/// What the file dialog offers, which is what `symphonia` is built to read.
const AUDIO_EXTENSIONS: [&str; 8] = ["mp3", "wav", "ogg", "oga", "flac", "m4a", "mp4", "aac"];

pub fn update(app: &mut App, message: SoundMsg) -> Task<Message> {
    match message {
        SoundMsg::OpenPopover(at) => {
            if let Some(main) = app.main_mut() {
                main.sound.popover = Some(at);
            }
            Task::none()
        }
        SoundMsg::ClosePopover => {
            close_popover(app);
            Task::none()
        }
        SoundMsg::Play(sound_id) => play(app, sound_id),
        SoundMsg::Stop => stop(app),
        SoundMsg::Ready(sound_id, result) => ready(app, sound_id, result),
        SoundMsg::Pick => Task::perform(pick_file(), |result| {
            Message::Sound(SoundMsg::Picked(result))
        }),
        SoundMsg::Picked(Ok(path)) => decode(path),
        SoundMsg::Picked(Err(error)) | SoundMsg::Decoded(Err(error)) => complain(app, error),
        SoundMsg::Decoded(Ok((name, source))) => {
            // A second pick replaces the dialog rather than stacking on it.
            app.ui.dialog = Some(Dialog::TrimSound {
                name,
                source,
                trim: TrimState::default(),
                drag: None,
            });
            Task::none()
        }
        SoundMsg::TrimStart(edge) => {
            if let Some(Dialog::TrimSound { trim, drag, .. }) = app.dialog_mut() {
                // The anchor is filled in by the first move: a press on a handle
                // carries no position of its own.
                *drag = Some(TrimDrag {
                    edge,
                    from: f32::NAN,
                    origin: match edge {
                        TrimEdge::Start => trim.start,
                        TrimEdge::End => trim.end,
                    },
                });
            }
            Task::none()
        }
        SoundMsg::TrimMove(at) => {
            if let Some(Dialog::TrimSound { trim, drag, .. }) = app.dialog_mut() {
                drag_edge(trim, drag, at);
            }
            Task::none()
        }
        SoundMsg::TrimEnd => {
            if let Some(Dialog::TrimSound { drag, .. }) = app.dialog_mut() {
                *drag = None;
            }
            Task::none()
        }
        SoundMsg::TrimApply => apply_trim(app),
        SoundMsg::Uploaded(Ok(sound)) => {
            // The library itself arrives as the `SoundUpserted` everybody gets.
            app.toast(ToastKind::Info, format!("Added {}", sound.name));
            Task::none()
        }
        SoundMsg::Uploaded(Err(error)) => complain(app, error),
        SoundMsg::RenameDraft(sound_id, name) => {
            if let Some(main) = app.main_mut() {
                main.admin.sound_names.insert(sound_id, name);
            }
            Task::none()
        }
        SoundMsg::RenameSave(sound_id) => rename(app, sound_id),
        SoundMsg::Delete(sound_id) => {
            app.ui.dialog = None;
            if let Some(main) = app.main_mut() {
                main.admin.sound_names.remove(&sound_id);
                main.send_or_notice(Command::DeleteSound { sound_id });
            }
            Task::none()
        }
    }
}

/// The soundpad's own frames. Everything else the connection reports belongs
/// elsewhere.
pub fn on_event(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::SoundUpserted { sound } => {
            if let Some(main) = app.main_mut() {
                main.sound.upsert(sound);
            }
            Task::none()
        }
        Event::SoundDeleted { sound_id } => {
            if let Some(main) = app.main_mut() {
                main.sound.remove(sound_id);
                main.admin.sound_names.remove(&sound_id);
            }
            Task::none()
        }
        Event::SoundPlayed {
            channel_id,
            user_id,
            sound_id,
        } => played(app, channel_id, user_id, sound_id),
        Event::SoundStopped { channel_id } => {
            if !in_joined_channel(app, channel_id) {
                return Task::none();
            }
            if let Some(main) = app.main_mut() {
                main.sound.stopped();
            }
            app.send_audio(AudioCommand::StopClip);
            Task::none()
        }
        _ => Task::none(),
    }
}

/// The library the snapshot carries, replacing whatever the last connection
/// left.
pub fn on_snapshot(app: &mut App, sounds: Vec<Sound>) {
    if let Some(main) = app.main_mut() {
        main.sound.apply_snapshot(sounds);
    }
}

/// Asks the server to play one clip. Nothing is heard here: the `SoundPlayed`
/// this earns is what plays it, on every client at once.
fn play(app: &mut App, sound_id: i64) -> Task<Message> {
    close_popover(app);
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.send_or_notice(Command::PlaySound {
        channel_id,
        sound_id,
    });
    Task::none()
}

/// Asks the server to cut whatever is playing. The `SoundStopped` is what
/// silences the mixer.
fn stop(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.send_or_notice(Command::StopSound { channel_id });
    Task::none()
}

fn rename(app: &mut App, sound_id: i64) -> Task<Message> {
    let draft = app
        .main()
        .and_then(|main| main.admin.sound_names.get(&sound_id).cloned())
        .unwrap_or_default();
    let name = match validate_name(&draft, "clip name") {
        Ok(name) => name,
        Err(complaint) => return complain(app, complaint),
    };

    if let Some(main) = app.main_mut() {
        main.admin.sound_names.remove(&sound_id);
        main.send_or_notice(Command::UpdateSound { sound_id, name });
    }
    Task::none()
}

/// One clip started in the joined channel. Only a member with a live session
/// there plays it — `PROTOCOL.md` § Sounds — so a frame for any other channel is
/// a fact this window has nothing to do with.
fn played(app: &mut App, channel_id: i64, user_id: i64, sound_id: i64) -> Task<Message> {
    if !in_joined_channel(app, channel_id) {
        return Task::none();
    }
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.sound.played(sound_id, user_id);

    // Already on its way: a second trigger of the same clip must not start a
    // second download of the same bytes.
    if !main.sound.fetching.insert(sound_id) {
        return Task::none();
    }

    let Some(session) = app.session.as_ref() else {
        // Nothing can be fetched without a bearer, so nothing is waited on
        // either.
        if let Some(main) = app.main_mut() {
            main.sound.fetching.remove(&sound_id);
        }
        return Task::none();
    };
    let token = session.access_token.clone();
    let endpoints = app.endpoints.clone();

    Task::perform(fetch_clip(endpoints, token, sound_id), move |result| {
        Message::Sound(SoundMsg::Ready(sound_id, result))
    })
}

/// The samples of one clip, from the cache when they are there and from the
/// server otherwise. Every step that reads a file or touches a sample runs on a
/// blocking task.
async fn fetch_clip(
    endpoints: Endpoints,
    token: String,
    sound_id: i64,
) -> Result<Arc<Vec<f32>>, String> {
    let cached = tokio::task::spawn_blocking(move || sounds::load(sound_id))
        .await
        .map_err(|e| e.to_string())?;

    let decoded = match cached {
        Some(bytes) => tokio::task::spawn_blocking(move || sounds::decode(&bytes)),
        None => {
            let downloaded = api::download(&endpoints, &token, sound_id)
                .await
                .map_err(|failure| describe(&failure))?;
            // Stored and decoded in the one hop: the bytes are megabytes, and
            // handing them over twice would copy them.
            tokio::task::spawn_blocking(move || {
                sounds::store(sound_id, &downloaded);
                sounds::decode(&downloaded)
            })
        }
    };

    decoded.await.map_err(|e| e.to_string())?.map(Arc::new)
}

/// The samples arrived. A clip the channel has already moved past is dropped
/// rather than played late.
fn ready(app: &mut App, sound_id: i64, result: Result<Arc<Vec<f32>>, String>) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        main.sound.fetching.remove(&sound_id);
    }
    let samples = match result {
        Ok(samples) => samples,
        Err(error) => {
            tracing::warn!(sound_id, %error, "a sound could not be played");
            return complain(app, error);
        }
    };

    let current = app.main().is_some_and(|main| {
        main.voice.is_live()
            && main
                .sound
                .playing
                .is_some_and(|clip| clip.sound_id == sound_id)
    });
    if current {
        app.send_audio(AudioCommand::PlayClip(samples));
    }
    Task::none()
}

/// The file dialog and nothing else: reading and decoding the picked file is the
/// next step's, on a blocking task of its own.
async fn pick_file() -> Result<PathBuf, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .add_filter("Audio", &AUDIO_EXTENSIONS)
        .pick_file()
        .await
    else {
        return Err(CANCELLED.to_owned());
    };
    Ok(handle.path().to_path_buf())
}

/// Decodes the picked file into the samples the trim view draws over.
fn decode(path: PathBuf) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || {
            let name = clip_name(&path);
            clips::decode(&path).map(|source| (name, Arc::new(source)))
        }),
        |joined| {
            Message::Sound(SoundMsg::Decoded(
                joined.unwrap_or_else(|e| Err(e.to_string())),
            ))
        },
    )
}

/// What a picked file's clip is called: its own name without the extension,
/// trimmed to what the server takes.
fn clip_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("clip");
    let name: String = stem
        .chars()
        .filter(|letter| !letter.is_control())
        .take(NAME_MAX)
        .collect();
    if name.trim().is_empty() {
        "clip".to_owned()
    } else {
        name.trim().to_owned()
    }
}

/// Moves the edge the pointer went down on. The first move is what anchors the
/// drag: a press on a handle carries no position.
fn drag_edge(trim: &mut TrimState, drag: &mut Option<TrimDrag>, at: f32) {
    let Some(anchor) = drag.as_mut() else {
        return;
    };
    if anchor.from.is_nan() {
        anchor.from = at;
        anchor.origin = match anchor.edge {
            TrimEdge::Start => trim.start,
            TrimEdge::End => trim.end,
        };
        return;
    }
    *trim = drag_to(*trim, *anchor, at);
}

/// Cuts the selection out and uploads it, neither on the interface thread. The
/// dialog goes away with the press: the span it held is in the cut now.
fn apply_trim(app: &mut App) -> Task<Message> {
    let Some(Dialog::TrimSound {
        name, source, trim, ..
    }) = app.ui.dialog.clone()
    else {
        return Task::none();
    };
    let Some(session) = app.session.as_ref() else {
        return Task::none();
    };
    app.ui.dialog = None;

    let (start_ms, end_ms) = span_ms(trim, source.duration_ms);
    let token = session.access_token.clone();
    let endpoints = app.endpoints.clone();

    Task::perform(
        async move {
            let bytes =
                tokio::task::spawn_blocking(move || clips::encode_clip(&source, start_ms, end_ms))
                    .await
                    .map_err(|e| e.to_string())??;

            api::upload(&endpoints, &token, &name, Blob::from(bytes))
                .await
                .map_err(|failure| describe(&failure))
        },
        |result| Message::Sound(SoundMsg::Uploaded(result)),
    )
}

/// Whether one frame's channel is the voice session this client is actually in.
fn in_joined_channel(app: &App, channel_id: i64) -> bool {
    app.main()
        .is_some_and(|main| main.voice.is_live() && main.voice.channel_id == channel_id)
}

fn close_popover(app: &mut App) {
    if let Some(main) = app.main_mut() {
        main.sound.popover = None;
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
