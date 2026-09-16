//! Turning this client's camera on, and watching other people's.
//!
//! Both halves are intents the server has to agree with, exactly as for the
//! screen share: the device is only opened once `CameraStarted` has come back,
//! because the relay accepts camera video only from a session it already knows
//! is on camera, and a tile is only drawn for a stream a `CameraWatchState` put
//! this client on.
//!
//! That watch state is the authority. The server sends the whole set every time
//! it changes, and [`watch_state`] makes the tiles match it — nothing here
//! assumes a watch it asked for was granted.

use iced::Task;
use vorcall_core::connection::Command;
use vorcall_core::{ErrorCode, Event, permissions};
use vorcall_screen::preset::FrameRate;

use crate::app::message::{CameraMsg, Message, ToastKind};
use crate::app::state::rules::{self, CameraResume};
use crate::app::state::voice::{CameraIntent, CameraTile, CameraTileId, tile_changes};
use crate::app::update::share;
use crate::app::{App, Status};
use crate::workers::camera::{CameraCommand, CameraEvent};
use crate::workers::share::{StageEvent, spawn_decode_thread};

pub fn update(app: &mut App, message: CameraMsg) -> Task<Message> {
    match message {
        CameraMsg::Toggle => toggle(app),
        CameraMsg::Event(event) => on_camera_event(app, event),
        CameraMsg::Tile(user_id, event) => on_tile_event(app, user_id, event),
        CameraMsg::Watch(user_id) => watch(app, user_id),
        CameraMsg::StopWatching(user_id) => stop_watching(app, user_id),
        CameraMsg::StopWatchingAll => stop_watching_all(app),
        CameraMsg::Feature(tile) => {
            if let Some(main) = app.main_mut() {
                let cameras = &mut main.voice.cameras;
                // A second press on the same tile puts it back in the row.
                cameras.featured = (cameras.featured() != Some(tile)).then_some(tile);
            }
            Task::none()
        }
        CameraMsg::DevicesListed(devices) => {
            // A device that has been unplugged since it was picked would leave
            // the list showing a name nothing can open.
            let unplugged = app
                .config
                .camera_device
                .as_ref()
                .is_some_and(|id| !devices.iter().any(|device| &device.id == id));
            if unplugged {
                app.config.camera_device = None;
                app.save_config();
            }
            if let Some(main) = app.main_mut() {
                main.settings.cameras = devices;
            }
            Task::none()
        }
        CameraMsg::SetDevice(device) => {
            app.config.camera_device = device.map(|device| device.id);
            app.save_config();
            Task::none()
        }
        CameraMsg::SetResolution(resolution) => {
            app.config.camera_resolution = resolution.to_string();
            app.save_config();
            Task::none()
        }
        CameraMsg::SetFps(hz) => {
            if FrameRate::from_hz(hz).is_some() {
                app.config.camera_fps = hz;
                app.save_config();
            }
            Task::none()
        }
    }
}

/// The camera frames. Everything else the connection reports belongs elsewhere.
pub fn on_event(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::CameraStarted {
            channel_id,
            user_id,
        } => camera_started(app, channel_id, user_id),
        Event::CameraStopped {
            channel_id,
            user_id,
        } => camera_stopped(app, channel_id, user_id),
        Event::CameraWatchState {
            channel_id,
            user_ids,
        } => watch_state(app, channel_id, &user_ids),
        Event::CameraWatchers { channel_id, count } => watchers(app, channel_id, count),
        _ => Task::none(),
    }
}

/// `PROTOCOL.md` § Camera: after the new `JoinVoice`/`VoiceReady`, a client that
/// was on camera asks again, and one that was watching asks for each of those
/// cameras again — the latter only once a roster says they are still on.
pub fn after_voice_ready(app: &mut App) -> Task<Message> {
    resume_camera(app);
    resume_watches(app)
}

/// Asks for the camera a reconnect kept. The device itself is opened on the
/// `CameraStarted` that answers this.
fn resume_camera(app: &mut App) {
    let Some(main) = app.main_mut() else {
        return;
    };
    if main.voice.camera.intent.is_none() {
        return;
    }
    let channel_id = main.voice.channel_id;
    main.voice.camera.starting = true;
    if !main.send_command(Command::StartCamera { channel_id }) {
        main.voice.camera.starting = false;
    }
}

/// What a fresh media session does about the watches a reconnect kept. Without a
/// roster for it yet the answer waits for the next `VoiceState`, which asks
/// again.
pub fn resume_watches(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let camera = main
        .voice
        .roster()
        .map(|roster| roster.camera.clone())
        .unwrap_or_default();
    let decision =
        rules::camera_watch_resume(&main.voice.cameras.intent, &camera, main.voice.roster_seen);
    main.voice.cameras.resume_pending = decision == CameraResume::Pending;

    let CameraResume::Request(user_ids) = decision else {
        return Task::none();
    };
    main.voice.cameras.intent = user_ids.iter().copied().collect();
    let channel_id = main.voice.channel_id;
    for user_id in user_ids {
        main.send_or_notice(Command::WatchCamera {
            channel_id,
            user_id,
        });
    }
    Task::none()
}

/// A camera the server refused. Without this the switch would stay behind a
/// `starting` that nothing ever answers.
pub fn on_server_error(app: &mut App, code: i32) {
    let toast = if code == ErrorCode::CameraUnavailable as i32 {
        Some("Cameras are off on this server")
    } else if code == ErrorCode::CameraLimit as i32 {
        Some("This channel already has the most cameras it allows")
    } else if code == ErrorCode::CameraWatchLimit as i32 {
        Some("You can watch at most four cameras at once")
    } else if code == ErrorCode::NotOnCamera as i32 {
        Some("That camera is off")
    } else {
        None
    };

    // The starting camera is given up on anything that could have refused it,
    // not only on the four camera codes: a `StartCamera` is also answered with
    // PERMISSION_DENIED or NOT_IN_VOICE.
    let refused = toast.is_some()
        || [
            ErrorCode::UnknownChannel,
            ErrorCode::NotInVoice,
            ErrorCode::PermissionDenied,
        ]
        .into_iter()
        .any(|known| known as i32 == code);

    if let Some(main) = app.main_mut()
        && refused
        && main.voice.camera.starting
    {
        main.voice.camera.intent = None;
        main.voice.camera.stopped();
    }
    if let Some(sentence) = toast {
        app.toast(ToastKind::Error, sentence.to_owned());
    }
}

/// The camera switch: start one, or stop the one that is running.
fn toggle(app: &mut App) -> Task<Message> {
    let running = app
        .main()
        .is_some_and(|main| main.voice.camera.active || main.voice.camera.starting);
    if running { stop(app) } else { start(app) }
}

fn start(app: &mut App) -> Task<Message> {
    let capabilities = vorcall_screen::camera_capabilities();
    let Some(prefs) = app
        .main()
        .map(|main| rules::camera_prefs(&app.config, &main.settings.cameras))
    else {
        return Task::none();
    };
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let has_video = main
        .server
        .can(permissions::VIDEO, Some(main.voice.channel_id));
    if !rules::can_camera(&main.voice, has_video, &capabilities) {
        return Task::none();
    }
    // Before the session check: a backed-off loop takes the command and drops it
    // without a frame, which would leave the camera waiting on an answer that is
    // never coming.
    if !matches!(main.status, Status::Connected) {
        main.notice = Some("Not connected".to_owned());
        return Task::none();
    }

    let channel_id = main.voice.channel_id;
    main.voice.camera.intent = Some(CameraIntent {
        request: rules::camera_request(&prefs),
        preset: rules::camera_preset(&prefs),
    });
    main.voice.camera.starting = true;
    if !main.send_command(Command::StartCamera { channel_id }) {
        main.voice.camera.intent = None;
        main.voice.camera.starting = false;
        main.notice = Some("Not connected".to_owned());
        return Task::none();
    }

    // The thread is started here rather than on the answer: opening a device can
    // raise a permission dialog, which must not wait for a thread as well.
    app.ensure_camera()
}

fn stop(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.camera.intent = None;
    main.voice.camera.stopped();
    main.send_or_notice(Command::StopCamera { channel_id });
    app.send_camera(CameraCommand::Stop);
    Task::none()
}

/// The server has the camera: whoever may see the channel now knows about it,
/// and this client's own device may be opened.
fn camera_started(app: &mut App, channel_id: i64, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.roster_mut(channel_id).set_camera(user_id, true);
    if channel_id != main.voice.channel_id || user_id != main.member_id {
        return Task::none();
    }
    main.voice.camera.active = true;
    main.voice.camera.starting = false;
    start_capture(app)
}

/// Opens the device for a camera the server has accepted, paused while nobody is
/// watching it.
fn start_capture(app: &mut App) -> Task<Message> {
    let thread = app.ensure_camera();
    let Some(main) = app.main() else {
        return thread;
    };
    let (Some(intent), Some(session)) = (&main.voice.camera.intent, &main.voice.session) else {
        return thread;
    };
    let command = CameraCommand::Start {
        request: intent.request.clone(),
        preset: intent.preset,
        sender: session.sender.clone(),
    };
    // A fresh pipeline starts paused, so the watcher count it already has is
    // what decides whether anything is encoded at all.
    let (paused, keyframe) = rules::pause_decision(0, main.voice.camera.watchers);

    app.send_camera(command);
    app.send_camera(CameraCommand::SetPaused(paused));
    if keyframe {
        app.send_camera(CameraCommand::ForceKeyframe);
    }
    thread
}

fn camera_stopped(app: &mut App, channel_id: i64, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if let Some(roster) = main.voice.rosters.get_mut(&channel_id) {
        roster.set_camera(user_id, false);
    }
    if channel_id != main.voice.channel_id {
        return Task::none();
    }

    if user_id != main.member_id {
        // A camera that has gone off has no stream left to decode. The server
        // sends a `CameraWatchState` for it too; dropping the tile here is what
        // keeps the stage from holding a frozen face until it arrives.
        main.voice.cameras.intent.remove(&user_id);
        drop_tile(app, user_id);
        return Task::none();
    }
    // The server ended this client's own camera — the voice session went, or a
    // limit was reached — so the device goes with it. A frame about a camera
    // already stopped from here is stale, and would otherwise kill the one
    // started right after it.
    if !(main.voice.camera.active || main.voice.camera.starting) {
        return Task::none();
    }
    main.voice.camera.intent = None;
    main.voice.camera.stopped();
    app.send_camera(CameraCommand::Stop);
    Task::none()
}

/// Nobody watching pauses the encoder — the preview carries on — and whoever
/// arrives after a pause can only start at a keyframe.
fn watchers(app: &mut App, channel_id: i64, count: u32) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if channel_id != main.voice.channel_id {
        return Task::none();
    }
    let (paused, keyframe) = rules::pause_decision(main.voice.camera.watchers, count);
    main.voice.camera.watchers = count;

    app.send_camera(CameraCommand::SetPaused(paused));
    if keyframe {
        app.send_camera(CameraCommand::ForceKeyframe);
    }
    Task::none()
}

fn watch(app: &mut App, user_id: i64) -> Task<Message> {
    let Some(main) = app.main() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    if main.voice.cameras.tiles.contains_key(&user_id) {
        return Task::none();
    }
    // The cap is the one refusal worth a sentence: every other reason is a
    // control that was not offered in the first place.
    let full = main.voice.cameras.tiles.len() >= rules::MAX_WATCHED_CAMERAS;
    let allowed = rules::can_watch_camera(&main.voice, main.member_id, channel_id, user_id);
    if full {
        app.toast(
            ToastKind::Error,
            "You can watch at most four cameras at once".to_owned(),
        );
        return Task::none();
    }
    if !allowed {
        return Task::none();
    }

    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.cameras.intent.insert(user_id);
    main.send_or_notice(Command::WatchCamera {
        channel_id,
        user_id,
    });
    Task::none()
}

/// Asks to come off one camera. Its tile stays until the server answers with the
/// `CameraWatchState` that ends it.
fn stop_watching(app: &mut App, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.cameras.intent.remove(&user_id);
    main.send_or_notice(Command::UnwatchCamera {
        channel_id,
        user_id,
    });
    Task::none()
}

/// Asks to come off every camera at once. `PROTOCOL.md` § Camera: user 0 is the
/// whole set. The tiles stay until the `CameraWatchState` that empties them.
fn stop_watching_all(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.cameras.intent.clear();
    main.send_or_notice(Command::UnwatchCamera {
        channel_id,
        user_id: 0,
    });
    Task::none()
}

/// The whole set of cameras this client is on, as the server has it. The tiles
/// are made to match: a stream that has arrived gets a decode thread, one that
/// has gone loses its tile and its thread with it.
fn watch_state(app: &mut App, channel_id: i64, watched: &[i64]) -> Task<Message> {
    let Some(main) = app.main() else {
        return Task::none();
    };
    if main.voice.channel_id != channel_id {
        return Task::none();
    }

    let (added, removed) = tile_changes(&main.voice.cameras.watched(), watched);
    for user_id in removed {
        drop_tile(app, user_id);
    }

    let mut tasks = Vec::new();
    for user_id in added {
        if let Some(task) = start_tile(app, user_id) {
            tasks.push(task);
        }
    }

    // The pop-out window holds the whole stage: with nothing left on it there is
    // nothing for that window to show.
    let empty = app.main().is_some_and(|main| !rules::on_stage(&main.voice));
    if empty {
        tasks.push(share::close_stage_windows(app));
    }
    Task::batch(tasks)
}

/// Starts decoding one camera. `None` when there is no session or no ssrc to
/// read it from, which is a tile that would never draw anything.
fn start_tile(app: &mut App, user_id: i64) -> Option<Task<Message>> {
    let main = app.main_mut()?;
    let Some(ssrc) = main.voice.ssrc_of(user_id) else {
        tracing::debug!(user_id, "watching a camera the roster does not name");
        return None;
    };
    let session = main.voice.session.as_ref()?;

    let units = session.engine.watch_camera(ssrc);
    let (decoder, events) = spawn_decode_thread(units);
    main.voice.cameras.tiles.insert(
        user_id,
        CameraTile {
            ssrc,
            decoder,
            picture: None,
            seq: 0,
            stats: None,
        },
    );
    Some(Task::run(events, move |event| {
        Message::Camera(CameraMsg::Tile(user_id, event))
    }))
}

/// Takes one camera off the stage: the engine stops reassembling it and dropping
/// the tile stops its decode thread.
fn drop_tile(app: &mut App, user_id: i64) {
    let Some(main) = app.main_mut() else {
        return;
    };
    let Some(tile) = main.voice.cameras.tiles.remove(&user_id) else {
        return;
    };
    if main.voice.cameras.featured == Some(CameraTileId::Peer(user_id)) {
        main.voice.cameras.featured = None;
    }
    if let Some(session) = &main.voice.session {
        session.engine.unwatch_camera(tile.ssrc);
    }
}

fn on_camera_event(app: &mut App, event: CameraEvent) -> Task<Message> {
    match event {
        CameraEvent::Started {
            width,
            height,
            output,
            backend,
        } => {
            tracing::info!(width, height, output = ?output, backend, "a camera is running");
            Task::none()
        }
        CameraEvent::Preview => {
            let taken = app
                .workers
                .camera
                .as_ref()
                .and_then(|handle| handle.preview.take());
            // A marker whose picture has already been read carries nothing: the
            // tile keeps the frame it is showing.
            if let (Some(main), Some((picture, seq))) = (app.main_mut(), taken) {
                main.voice.camera.preview = Some(picture);
                main.voice.camera.preview_seq = seq;
            }
            Task::none()
        }
        CameraEvent::Stats(stats) => {
            tracing::debug!(
                capture_fps = stats.capture_fps,
                encode_fps = stats.encode_fps,
                kbps = stats.kbps,
                output = ?stats.output,
                keyframes = stats.keyframes,
                keyframe_requests = stats.keyframe_requests,
                dropped = stats.dropped_frames,
                skipped = stats.skipped_frames,
                send_failures = stats.send_failures,
                "running a camera"
            );
            if let Some(main) = app.main_mut() {
                main.voice.camera.stats = Some(stats);
            }
            Task::none()
        }
        CameraEvent::Failed(reason) | CameraEvent::Ended(reason) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            // The server is only told about a camera it was told about.
            if main.voice.camera.active || main.voice.camera.starting {
                let channel_id = main.voice.channel_id;
                main.send_command(Command::StopCamera { channel_id });
            }
            main.voice.camera.intent = None;
            main.voice.camera.stopped();
            app.send_camera(CameraCommand::Stop);
            app.toast(ToastKind::Error, reason);
            Task::none()
        }
    }
}

/// What one watched camera's decode thread reports. A tile whose thread has died
/// keeps the last picture it drew and stops there.
fn on_tile_event(app: &mut App, user_id: i64, event: StageEvent) -> Task<Message> {
    match event {
        StageEvent::Picture => {
            if let Some(main) = app.main_mut()
                && let Some(tile) = main.voice.cameras.tiles.get_mut(&user_id)
                && let Some((picture, seq)) = tile.decoder.picture.take()
            {
                tile.picture = Some(picture);
                tile.seq = seq;
            }
        }
        StageEvent::Stats {
            decode_fps,
            pictures,
            errors,
            dropped,
        } => {
            tracing::debug!(
                user_id,
                decode_fps,
                pictures,
                errors,
                dropped,
                "watching a camera"
            );
            if let Some(main) = app.main_mut()
                && let Some(tile) = main.voice.cameras.tiles.get_mut(&user_id)
            {
                tile.stats = Some((decode_fps, pictures, errors));
            }
        }
        StageEvent::Failed(reason) => app.toast(ToastKind::Error, reason),
    }
    Task::none()
}

/// Everything a session that is going leaves behind: the device, the tiles and
/// every decode thread under them. The intents are the caller's, exactly as for
/// the share.
pub fn close(app: &mut App) {
    if let Some(main) = app.main_mut() {
        if let Some(session) = &main.voice.session {
            session.engine.unwatch_all_cameras();
        }
        main.voice.camera.stopped();
        main.voice.cameras.stopped();
    }
    app.send_camera(CameraCommand::Stop);
}
