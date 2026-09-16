//! Sharing a screen, and watching somebody else's.
//!
//! Both halves are intents the server has to agree with. The capture only starts
//! once `ShareStarted` has come back, because the relay accepts video and share
//! audio only from a session it already knows is sharing; the stage only ever
//! draws the stream a `WatchState` put this client on.

use iced::{Size, Task, window};
use vorcall_core::config::{SHARE_MAX_BITRATE_KBPS, SHARE_MIN_BITRATE_KBPS};
use vorcall_core::connection::Command;
use vorcall_core::{ErrorCode, Event};
use vorcall_screen::CaptureRequest;

use crate::app::message::{Message, ShareMsg, ToastKind};
use crate::app::state::rules::{self, WatchResume};
use crate::app::state::ui::{Dialog, SourcesState};
use crate::app::state::voice::{ShareIntent, watch_intent_after_stop};
use crate::app::{App, STAGE_WINDOW, Status};
use crate::workers::lock;
use crate::workers::share::{ShareCommand, ShareEvent, StageEvent, spawn_decode_thread};

pub fn update(app: &mut App, message: ShareMsg) -> Task<Message> {
    match message {
        ShareMsg::OpenPicker => open_picker(app),
        ShareMsg::SourcesListed(result) => {
            if let Some(Dialog::SharePicker { sources, .. }) = &mut app.ui.dialog {
                *sources = match result {
                    Ok(listed) => SourcesState::Ready(listed),
                    Err(error) => SourcesState::Failed(error),
                };
            }
            Task::none()
        }
        ShareMsg::PickSource(source_id) => {
            if let Some(Dialog::SharePicker { selected, .. }) = &mut app.ui.dialog {
                *selected = Some(source_id);
            }
            Task::none()
        }
        ShareMsg::SetPickerAudio(value) => {
            if let Some(Dialog::SharePicker { audio, .. }) = &mut app.ui.dialog {
                *audio = value;
            }
            Task::none()
        }
        ShareMsg::Confirm => confirm(app),
        ShareMsg::Stop => stop(app),
        ShareMsg::Event(event) => on_share_event(app, event),
        ShareMsg::Watch(user_id) => watch(app, user_id),
        ShareMsg::StopWatching => stop_watching(app),
        ShareMsg::Stage(event) => on_stage_event(app, event),
        ShareMsg::PopOut => pop_out(app),
        ShareMsg::PopIn => pop_in(app),
        ShareMsg::ToggleFullscreen => toggle_fullscreen(app),
        ShareMsg::SetVolume(volume) => set_volume(app, volume),
        // Every step of the drag reached the mixer already; only its end reaches
        // the disk.
        ShareMsg::VolumeReleased => {
            app.save_config();
            Task::none()
        }
        ShareMsg::SetResolution(resolution) => {
            app.config.share_resolution = resolution;
            app.save_config();
            Task::none()
        }
        ShareMsg::SetFps(fps) => {
            app.config.share_fps = fps;
            app.save_config();
            Task::none()
        }
        ShareMsg::SetBitrateAuto(auto) => {
            // Manual starts where automatic left off, so the slider does not jump
            // the moment it appears.
            app.config.share_bitrate_kbps = (!auto).then(|| rules::auto_bitrate_kbps(&app.config));
            app.save_config();
            Task::none()
        }
        ShareMsg::SetBitrate(kbps) => {
            app.config.share_bitrate_kbps =
                Some(kbps.clamp(SHARE_MIN_BITRATE_KBPS, SHARE_MAX_BITRATE_KBPS));
            Task::none()
        }
        ShareMsg::BitrateReleased => {
            app.save_config();
            Task::none()
        }
        ShareMsg::SetShareAudio(value) => {
            app.config.share_audio = value;
            app.save_config();
            Task::none()
        }
    }
}

/// The share frames. Everything else the connection reports belongs elsewhere.
pub fn on_event(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::ShareStarted {
            channel_id,
            user_id,
            audio,
        } => share_started(app, channel_id, user_id, audio),
        Event::ShareStopped {
            channel_id,
            user_id,
        } => share_stopped(app, channel_id, user_id),
        Event::WatchState {
            channel_id,
            user_id,
        } => {
            if app
                .main()
                .is_some_and(|main| main.voice.channel_id != channel_id)
            {
                return Task::none();
            }
            match user_id {
                Some(user_id) => start_watching(app, user_id),
                None => leave_stage(app),
            }
        }
        Event::ShareWatchers { channel_id, count } => watchers(app, channel_id, count),
        _ => Task::none(),
    }
}

/// `PROTOCOL.md` § Screen share: after the new `JoinVoice`/`VoiceReady`, a client
/// that was sharing asks again, and a client that was watching asks again as well —
/// the latter only once a roster says that screen is still being shared.
pub fn after_voice_ready(app: &mut App) -> Task<Message> {
    let resumed = resume_share(app);
    Task::batch([resumed, resume_watch(app)])
}

/// Asks for the share a reconnect kept. The capture itself starts on the
/// `ShareStarted` that answers this.
fn resume_share(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let Some(intent) = &main.voice.share.intent else {
        return Task::none();
    };
    let audio = intent.request.audio;
    let channel_id = main.voice.channel_id;
    main.voice.share.starting = true;
    if !main.send_command(Command::StartShare { channel_id, audio }) {
        main.voice.share.starting = false;
    }
    Task::none()
}

/// What a fresh media session does about a watch intent a reconnect kept. Without a
/// roster for it yet the answer waits for the next `VoiceState`, which asks again.
pub fn resume_watch(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let sharing = main
        .voice
        .roster()
        .map(|roster| roster.sharing.clone())
        .unwrap_or_default();
    let decision = rules::watch_resume(main.voice.watch.intent, &sharing, main.voice.roster_seen);
    main.voice.watch.resume_pending = decision == WatchResume::Pending;

    match decision {
        WatchResume::Request(user_id) => {
            let channel_id = main.voice.channel_id;
            main.send_or_notice(Command::WatchShare {
                channel_id,
                user_id,
            });
        }
        WatchResume::Clear => main.voice.watch.intent = None,
        WatchResume::Pending | WatchResume::Nothing => {}
    }
    Task::none()
}

/// A share the server refused. Without this the picker would stay shut behind a
/// `starting` that nothing ever answers.
pub fn on_server_error(app: &mut App, code: i32) {
    let refused = [
        ErrorCode::UnknownChannel,
        ErrorCode::NotInVoice,
        ErrorCode::NotSharing,
        ErrorCode::ShareLimit,
        ErrorCode::ShareUnavailable,
        ErrorCode::PermissionDenied,
    ]
    .into_iter()
    .any(|known| known as i32 == code);

    let Some(main) = app.main_mut() else {
        return;
    };
    if !refused || !main.voice.share.starting {
        return;
    }
    main.voice.share.intent = None;
    main.voice.share.stopped();
}

/// Offers the picker. A system that cannot capture at all never gets here: the
/// voice card draws no button for it.
fn open_picker(app: &mut App) -> Task<Message> {
    let capabilities = vorcall_screen::capabilities();
    let audio = app.config.share_audio;
    let allowed = app
        .main()
        .is_some_and(|main| rules::can_share(&main.voice, &capabilities));
    if !allowed {
        return Task::none();
    }

    app.ui.dialog = Some(Dialog::SharePicker {
        sources: if capabilities.portal_picker {
            SourcesState::Ready(Vec::new())
        } else {
            SourcesState::Loading
        },
        selected: None,
        audio,
    });

    // The pipeline thread is started here rather than on the confirmation: a
    // portal picker is raised by the capture itself, which must not wait for a
    // thread as well.
    let thread = app.ensure_share();
    if capabilities.portal_picker {
        return thread;
    }

    // Listing the screens talks to the window server and blocks; on macOS it is
    // also what raises the screen-recording prompt.
    let listing = Task::perform(
        tokio::task::spawn_blocking(vorcall_screen::enumerate),
        |joined| {
            let listed = match joined {
                Ok(listed) => listed.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            Message::Share(ShareMsg::SourcesListed(listed))
        },
    );
    Task::batch([thread, listing])
}

fn confirm(app: &mut App) -> Task<Message> {
    let preset = rules::share_preset(&app.config);
    let Some(Dialog::SharePicker {
        selected, audio, ..
    }) = app.ui.dialog.take()
    else {
        return Task::none();
    };

    let request = CaptureRequest {
        source: selected,
        fps: preset.fps,
        cursor: true,
        audio,
        max_size: rules::capture_box(preset.resolution),
    };

    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // Before the session check: a backed-off loop takes the command and drops it
    // without a frame, which would leave the share waiting on an answer that is
    // never coming.
    if !matches!(main.status, Status::Connected) {
        main.notice = Some("Not connected".to_owned());
        return Task::none();
    }
    if !main.voice.is_live() {
        main.notice = Some("Join voice first".to_owned());
        return Task::none();
    }
    let channel_id = main.voice.channel_id;
    main.voice.share.intent = Some(ShareIntent { request, preset });
    main.voice.share.starting = true;
    if !main.send_command(Command::StartShare { channel_id, audio }) {
        main.voice.share.intent = None;
        main.voice.share.starting = false;
        main.notice = Some("Not connected".to_owned());
    }
    Task::none()
}

fn stop(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.share.intent = None;
    main.voice.share.stopped();
    main.send_or_notice(Command::StopShare { channel_id });
    app.send_share(ShareCommand::Stop);
    Task::none()
}

/// The server has the share: whoever may see the channel now knows about it, and
/// this client's own capture may start.
fn share_started(app: &mut App, channel_id: i64, user_id: i64, audio: bool) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice
        .roster_mut(channel_id)
        .sharing
        .insert(user_id, audio);
    if channel_id != main.voice.channel_id || user_id != main.member_id {
        return Task::none();
    }
    main.voice.share.active = true;
    main.voice.share.starting = false;
    start_capture(app)
}

/// Starts the capture for a share the server has accepted, paused while nobody is
/// watching it.
fn start_capture(app: &mut App) -> Task<Message> {
    let thread = app.ensure_share();
    let Some(share_far_end) = app
        .workers
        .audio
        .as_ref()
        .map(|handle| handle.share_far_end())
    else {
        tracing::warn!("no audio thread to take the share's far-end reference from");
        return thread;
    };

    let Some(main) = app.main() else {
        return thread;
    };
    let (Some(intent), Some(session)) = (&main.voice.share.intent, &main.voice.session) else {
        return thread;
    };
    let command = ShareCommand::Start {
        request: intent.request.clone(),
        preset: intent.preset,
        sender: session.sender.clone(),
        share_far_end,
    };
    // A fresh pipeline starts paused, so the watcher count it already has is what
    // decides whether anything is encoded at all.
    let (paused, keyframe) = rules::pause_decision(0, main.voice.share.watchers);

    app.send_share(command);
    app.send_share(ShareCommand::SetPaused(paused));
    if keyframe {
        app.send_share(ShareCommand::ForceKeyframe);
    }
    thread
}

fn share_stopped(app: &mut App, channel_id: i64, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if let Some(roster) = main.voice.rosters.get_mut(&channel_id) {
        roster.sharing.remove(&user_id);
    }
    if channel_id != main.voice.channel_id {
        return Task::none();
    }
    // The `WatchState` that follows clears the stage itself.
    main.voice.watch.intent = watch_intent_after_stop(main.voice.watch.intent, user_id);
    // The server ended this client's own share — the voice session went, or a limit
    // was reached — so the capture goes with it. A frame about a share already
    // stopped from here is stale, and would otherwise kill the one started right
    // after it.
    if user_id != main.member_id || !(main.voice.share.active || main.voice.share.starting) {
        return Task::none();
    }
    main.voice.share.intent = None;
    main.voice.share.stopped();
    app.send_share(ShareCommand::Stop);
    Task::none()
}

/// Nobody watching pauses the encoder, and whoever arrives after a pause can only
/// start at a keyframe.
fn watchers(app: &mut App, channel_id: i64, count: u32) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if channel_id != main.voice.channel_id {
        return Task::none();
    }
    let (paused, keyframe) = rules::pause_decision(main.voice.share.watchers, count);
    main.voice.share.watchers = count;

    app.send_share(ShareCommand::SetPaused(paused));
    if keyframe {
        app.send_share(ShareCommand::ForceKeyframe);
    }
    Task::none()
}

fn on_share_event(app: &mut App, event: ShareEvent) -> Task<Message> {
    match event {
        ShareEvent::Started {
            width,
            height,
            output,
            audio,
            backend,
        } => {
            tracing::info!(
                width,
                height,
                output = ?output,
                audio = ?audio,
                backend,
                "a screen share is running"
            );
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            main.voice.share.backend = Some(backend);
            main.voice.share.audio = audio;

            // What the backend really captured is not always what was asked for,
            // and the relay drops share audio from a share that was not announced
            // with it. `StartShare` is idempotent, so the flag is simply corrected.
            let announced = main
                .voice
                .share
                .intent
                .as_ref()
                .is_some_and(|intent| intent.request.audio);
            if announced && audio.is_none() {
                let channel_id = main.voice.channel_id;
                main.send_command(Command::StartShare {
                    channel_id,
                    audio: false,
                });
            }
            Task::none()
        }
        ShareEvent::Stats(stats) => {
            tracing::debug!(
                capture_fps = stats.capture_fps,
                encode_fps = stats.encode_fps,
                kbps = stats.kbps,
                output = ?stats.output,
                keyframes = stats.keyframes,
                keyframe_requests = stats.keyframe_requests,
                dropped = stats.dropped_frames,
                skipped = stats.skipped_frames,
                audio_frames = stats.audio_frames,
                audio_passed_through = stats.audio_passed_through,
                "sharing a screen"
            );
            if let Some(main) = app.main_mut() {
                main.voice.share.stats = Some(stats);
            }
            Task::none()
        }
        ShareEvent::Failed(reason) | ShareEvent::Ended(reason) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            // The server is only told about a share it was told about.
            if main.voice.share.active || main.voice.share.starting {
                let channel_id = main.voice.channel_id;
                main.send_command(Command::StopShare { channel_id });
            }
            main.voice.share.intent = None;
            main.voice.share.stopped();
            app.send_share(ShareCommand::Stop);
            app.toast(ToastKind::Error, reason);
            Task::none()
        }
    }
}

fn watch(app: &mut App, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    if !rules::can_watch(&main.voice, main.member_id, channel_id, user_id) {
        return Task::none();
    }
    main.voice.watch.intent = Some(user_id);
    main.send_or_notice(Command::WatchShare {
        channel_id,
        user_id,
    });
    Task::none()
}

/// Asks to come off the stream. What is on screen stays until the server answers
/// with the `WatchState` that ends it.
fn stop_watching(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.watch.intent = None;
    main.send_or_notice(Command::UnwatchShare { channel_id });
    Task::none()
}

/// The server put this client on a sharer's stream. One decode thread serves a
/// whole session: the engine hands its access units out once, so switching sharers
/// only re-points the engine at another ssrc.
fn start_watching(app: &mut App, user_id: i64) -> Task<Message> {
    let volume = app.config.share_volume;
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let ssrc = main.voice.ssrc_of(user_id);
    if ssrc.is_none() {
        tracing::debug!(user_id, "watching a sharer the roster does not name");
    }
    let first = main.voice.watch.decoder.is_none();

    let Some(session) = &main.voice.session else {
        return Task::none();
    };
    session.engine.watch(ssrc);
    lock(&session.playout).set_share_gain(volume);
    let units = first.then(|| session.engine.take_access_units()).flatten();

    main.voice.watch.stopped();
    main.voice.watch.state = Some(user_id);
    main.voice.watch.volume = volume;

    let Some(units) = units else {
        if first {
            tracing::warn!("this session has no access units left to decode");
        }
        return Task::none();
    };
    let (decoder, events) = spawn_decode_thread(units);
    main.voice.watch.decoder = Some(decoder);
    Task::run(events, |event| Message::Share(ShareMsg::Stage(event)))
}

/// The server took this client off every stream. The decoder stays: it is the
/// session's, and it simply starves until the next watch.
fn leave_stage(app: &mut App) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        if let Some(session) = &main.voice.session {
            session.engine.watch(None);
            lock(&session.playout).remove_share();
        }
        main.voice.watch.stopped();
    }
    close_stage_windows(app)
}

fn on_stage_event(app: &mut App, event: StageEvent) -> Task<Message> {
    match event {
        StageEvent::Picture => {
            if let Some(main) = app.main_mut() {
                let taken = main
                    .voice
                    .watch
                    .decoder
                    .as_ref()
                    .and_then(|decoder| decoder.picture.take());
                // A marker whose picture has already been read carries nothing:
                // the stage keeps the frame it is showing.
                if let Some((picture, seq)) = taken {
                    main.voice.watch.picture = Some(picture);
                    main.voice.watch.seq = seq;
                }
            }
        }
        StageEvent::Stats {
            decode_fps,
            pictures,
            errors,
            dropped,
        } => {
            tracing::debug!(
                decode_fps,
                pictures,
                errors,
                dropped,
                "watching a shared screen"
            );
            if let Some(main) = app.main_mut() {
                main.voice.watch.stats = Some((decode_fps, pictures, errors));
            }
        }
        StageEvent::Failed(reason) => app.toast(ToastKind::Error, reason),
    }
    Task::none()
}

fn pop_out(app: &mut App) -> Task<Message> {
    let settings = stage_settings(app);
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // The pop-out carries the whole stage, cameras included, so it opens for
    // anything on it rather than for a share alone.
    if !rules::on_stage(&main.voice) {
        return Task::none();
    }
    let watch = &mut main.voice.watch;
    if watch.popped.is_some() {
        return Task::none();
    }
    // The stage leaves the window it was in, so nothing there is fullscreen for it
    // any more.
    let restore = match watch.fullscreen.take() {
        Some(id) => window::set_mode(id, window::Mode::Windowed),
        None => Task::none(),
    };

    let (id, opening) = window::open(settings);
    watch.popped = Some(id);
    Task::batch([restore, opening.discard()])
}

fn pop_in(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let watch = &mut main.voice.watch;
    let Some(id) = watch.popped.take() else {
        return Task::none();
    };
    if watch.fullscreen == Some(id) {
        watch.fullscreen = None;
    }
    window::close(id)
}

/// The stage takes over whichever window shows it, and gives it back.
fn toggle_fullscreen(app: &mut App) -> Task<Message> {
    let main_window = app.main_window;
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let watch = &mut main.voice.watch;
    if let Some(id) = watch.fullscreen.take() {
        return window::set_mode(id, window::Mode::Windowed);
    }
    let Some(id) = watch.popped.or(main_window) else {
        return Task::none();
    };
    watch.fullscreen = Some(id);
    window::set_mode(id, window::Mode::Fullscreen)
}

/// Puts the pop-out away and gives a window that went fullscreen for the stage its
/// decorations back.
pub fn close_stage_windows(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let watch = &mut main.voice.watch;
    let restore = match watch.fullscreen.take() {
        Some(id) => window::set_mode(id, window::Mode::Windowed),
        None => Task::none(),
    };
    let close = match watch.popped.take() {
        Some(id) => window::close(id),
        None => Task::none(),
    };
    Task::batch([restore, close])
}

/// The watched share's volume, live: the mixer holds it, and only the end of a drag
/// writes it to the configuration.
fn set_volume(app: &mut App, volume: f32) -> Task<Message> {
    let volume = volume.clamp(0.0, rules::SHARE_VOLUME_MAX);
    app.config.share_volume = volume;

    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.watch.volume = volume;
    if let Some(session) = &main.voice.session {
        lock(&session.playout).set_share_gain(volume);
    }
    Task::none()
}

fn stage_settings(app: &App) -> window::Settings {
    window::Settings {
        size: Size::new(STAGE_WINDOW.0, STAGE_WINDOW.1),
        icon: app.icon.clone(),
        platform_specific: crate::app::platform_specific(false),
        ..window::Settings::default()
    }
}
