//! Joining voice, the microphone, the peers and the hotkey listener.
//!
//! The media path, the roster and the ssrc map belong to one connection;
//! `VoiceUi::intent` is what outlives one. A reconnect therefore asks for the
//! session again instead of repairing what is left of it, and every answer that
//! belongs to a session already moved past is dropped rather than installed —
//! which is what `pending_ssrc` and the hotkey generation are for.

use std::collections::BTreeMap;

use futures::StreamExt;
use futures::channel::mpsc;
use iced::Task;
use vorcall_core::config::{Config, PeerAudio, TransmitMode, VAD_MAX_DB, VAD_MIN_DB};
use vorcall_core::connection::{AdminCommand, Command, MediaKey};
use vorcall_core::{Event, VoiceMember};
use vorcall_hotkey::{ActionId, Binding, Edge, Listener, Shortcut, Unavailable};
use vorcall_voice::{CleanupSettings, MediaConfig, MediaEngine};

use crate::app::message::{Message, ToastKind, VoiceMsg};
use crate::app::state::rules::{
    self, GLOBAL_ACTIONS, PUSH_TO_TALK, TOGGLE_DEAFEN, TOGGLE_MUTE, WINDOW_GENERATION,
};
use crate::app::state::voice::{EngineHandoff, HotkeyHandoff, HotkeyStatus, MediaSession, VoiceUi};
use crate::app::update::share;
use crate::app::{App, SPEAKING_WINDOW, STATS_EVERY, VOICE_TICK};
use crate::workers::share::ShareCommand;
use crate::workers::voice::{AudioCommand, AudioEvent, AudioSettings, TransmitSettings, lock};

pub fn update(app: &mut App, message: VoiceMsg) -> Task<Message> {
    match message {
        VoiceMsg::Join(channel_id) => join(app, channel_id),
        VoiceMsg::Leave => leave(app),
        VoiceMsg::ToggleMute => toggle_mute(app),
        VoiceMsg::ToggleDeafen => toggle_deafen(app),
        VoiceMsg::MediaConnected(Ok(handoff)) => media_connected(app, handoff),
        VoiceMsg::MediaConnected(Err(reason)) => media_failed(app, reason),
        VoiceMsg::Audio(event) => on_audio(app, event),
        VoiceMsg::SetTransmitMode(mode) => set_transmit_mode(app, mode),
        VoiceMsg::SetVadThreshold(threshold_db) => {
            app.config.vad_threshold_db = threshold_db.clamp(VAD_MIN_DB, VAD_MAX_DB);
            push_transmit(app);
            Task::none()
        }
        // Every step of the drag reached the audio thread already; only its end
        // reaches the disk.
        VoiceMsg::VadThresholdReleased => {
            app.save_config();
            Task::none()
        }
        VoiceMsg::SetNoiseSuppression(value) => {
            app.config.noise_suppression = value;
            app.save_config();
            push_cleanup(app);
            Task::none()
        }
        VoiceMsg::SetEchoCancellation(value) => {
            app.config.echo_cancellation = value;
            app.save_config();
            push_cleanup(app);
            Task::none()
        }
        VoiceMsg::SetAutoGain(value) => {
            app.config.auto_gain = value;
            app.save_config();
            push_cleanup(app);
            Task::none()
        }
        VoiceMsg::SetPeerVolume(user_id, volume) => {
            set_peer_audio(app, user_id, |audio| audio.volume = volume);
            Task::none()
        }
        VoiceMsg::PeerVolumeReleased(user_id) => {
            let volume = app.config.peer_audio(user_id).volume;
            tracing::debug!(user_id, volume, "a peer's volume was set");
            app.save_config();
            Task::none()
        }
        VoiceMsg::TogglePeerMute(user_id) => {
            set_peer_audio(app, user_id, |audio| audio.muted = !audio.muted);
            app.save_config();
            Task::none()
        }
        VoiceMsg::Hotkey {
            generation,
            action,
            edge,
        } => hotkey(app, generation, action, edge),
        VoiceMsg::HotkeyStarted(handoff) => hotkey_started(app, handoff),
        VoiceMsg::HotkeyEnded { generation } => hotkey_ended(app, generation),
        VoiceMsg::RetryHotkey => restart_hotkey(app),
        VoiceMsg::DevicesListed(lists) => {
            if let Some(main) = app.main_mut() {
                main.settings.inputs = lists.inputs;
                main.settings.outputs = lists.outputs;
            }
            Task::none()
        }
        VoiceMsg::SetInputDevice(name) => {
            app.config.input_device = rules::device_choice(name);
            app.save_config();
            push_devices(app);
            Task::none()
        }
        VoiceMsg::SetOutputDevice(name) => {
            app.config.output_device = rules::device_choice(name);
            app.save_config();
            push_devices(app);
            Task::none()
        }
        VoiceMsg::Moderate {
            user_id,
            muted,
            deafened,
            move_to,
        } => moderate(app, user_id, muted, deafened, move_to),
    }
}

/// The 100 ms tick while a media path is live: the speaking indicators, and every
/// tenth tick the engine's statistics.
pub fn tick(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let voice = &mut main.voice;
    let Some(session) = voice.session.as_ref() else {
        return Task::none();
    };

    let heard = lock(&session.playout).speaking(SPEAKING_WINDOW);
    let report = voice.ticks % STATS_EVERY == 0;
    let stats = report.then(|| session.engine.stats());
    let video = session.engine.video_stats();

    voice.ticks = voice.ticks.wrapping_add(1);
    let heard: Vec<i64> = heard
        .into_iter()
        .filter_map(|ssrc| voice.by_ssrc.get(&ssrc).copied())
        .collect();
    voice.refresh_speaking(&heard);
    if let Some(stats) = stats {
        voice.stats = stats;
    }

    voice.watch.video = video;
    if report {
        // The viewer measures no bitrate of its own; what the depacketizer took in
        // over the second between two reports is the rate.
        let bytes = voice.watch.video.bytes;
        let received = bytes.saturating_sub(voice.watch.last_bytes);
        voice.watch.last_bytes = bytes;
        voice.watch.kbps =
            (received as f64 * 8.0 / 1_000.0 / (f64::from(STATS_EVERY) * VOICE_TICK.as_secs_f64()))
                as u32;
    }
    push_ducking(app);
    Task::none()
}

/// The voice frames. Everything else the connection reports belongs elsewhere.
pub fn on_event(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::VoiceReady {
            channel_id,
            host,
            port,
            key,
            ssrc,
        } => voice_ready(app, channel_id, host, port, key, ssrc),
        Event::VoiceState {
            channel_id,
            members,
        } => voice_state(app, channel_id, members),
        Event::VoiceMemberJoined { channel_id, member } => member_joined(app, channel_id, member),
        Event::VoiceMemberLeft {
            channel_id,
            user_id,
        } => member_left(app, channel_id, user_id),
        Event::VoiceMoved { channel_id } => moved(app, channel_id),
        Event::Speaking {
            channel_id,
            user_id,
            speaking,
        } => {
            if let Some(main) = app.main_mut() {
                main.voice.set_speaking(channel_id, user_id, speaking);
            }
            push_ducking(app);
            Task::none()
        }
        _ => Task::none(),
    }
}

/// The voice session did not survive the reconnect; the intent did.
pub fn on_connected(app: &mut App) -> Task<Message> {
    let Some(main) = app.main() else {
        return Task::none();
    };
    if !main.voice.intent || main.voice.is_live() {
        return Task::none();
    }
    let channel_id = main.voice.channel_id;
    request_join(app, channel_id);
    Task::none()
}

/// Asks for one channel's voice session. `joining` follows the frame: a client that
/// could not send one is not waiting for an answer.
fn request_join(app: &mut App, channel_id: i64) -> bool {
    let Some(main) = app.main_mut() else {
        return false;
    };
    main.voice.joining = true;
    if main.send_command(Command::JoinVoice { channel_id }) {
        return true;
    }
    main.voice.joining = false;
    false
}

/// Drops the media path and leaves every intent alone: the next connection
/// rejoins, shares again and asks to watch again with them.
pub fn on_disconnected(app: &mut App) -> Task<Message> {
    let closing = close_session(app);
    if let Some(main) = app.main_mut() {
        // Voice membership is per connection, in every channel.
        main.voice.rosters.clear();
        main.voice.joining = false;
    }
    closing
}

/// Leaves for good: the voice session and both screen-share intents are given up,
/// so none of them is asserted again on the next connection. The listener belongs
/// to the session and goes with it.
pub fn leave(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let channel_id = main.voice.channel_id;
    main.voice.give_up_intents();
    // The intent is cleared either way: the server drops the session when the
    // socket ends.
    if !main.send_command(Command::LeaveVoice { channel_id }) {
        main.notice = Some("Not connected".to_owned());
    }
    close_session(app)
}

/// Ends the media path and everything drawn from it, keeping the intents. The
/// engine's tasks outlive a dropped engine, so it is closed rather than dropped.
pub fn close_session(app: &mut App) -> Task<Message> {
    let stage = share::close_stage_windows(app);
    let Some(main) = app.main_mut() else {
        return stage;
    };
    let voice = &mut main.voice;
    // Dropping the listener stops it: nothing outside a session observes the
    // bindings.
    drop_hotkey(voice);
    // The capture feeds the engine that is going, and the decoder reads a stream
    // that ends with it.
    voice.share.stopped();
    voice.watch.decoder = None;
    voice.watch.stopped();

    let Some(session) = voice.take_session() else {
        return stage;
    };
    tracing::debug!(
        ssrc = session.ssrc,
        input = ?session.audio_input,
        output = ?session.audio_output,
        "closing the voice session"
    );
    app.send_audio(AudioCommand::Close);
    app.send_share(ShareCommand::Stop);
    Task::batch([
        stage,
        Task::perform(session.engine.close(), |()| Message::Noop),
    ])
}

/// Picking a voice channel. Another one while already in voice is a move: the
/// session being left is given up first, so the server is never asked to hold two.
fn join(app: &mut App, channel_id: i64) -> Task<Message> {
    let switching = app
        .main()
        .is_some_and(|main| main.voice.intent && main.voice.channel_id != channel_id);

    let mut tasks = Vec::new();
    if switching {
        if let Some(main) = app.main_mut() {
            let leaving = main.voice.channel_id;
            main.send_command(Command::LeaveVoice {
                channel_id: leaving,
            });
            // The share and the watch belonged to the session being left.
            main.voice.switch_to(channel_id);
        }
        tasks.push(close_session(app));
    }

    if let Some(main) = app.main_mut() {
        main.voice.intent = true;
        main.voice.channel_id = channel_id;
    }
    if !request_join(app, channel_id)
        && let Some(main) = app.main_mut()
    {
        main.notice = Some("Not connected".to_owned());
    }
    Task::batch(tasks)
}

/// A moderator moved this account. Channel 0 is a disconnect; anything else is a
/// session this client now asks for itself.
fn moved(app: &mut App, channel_id: i64) -> Task<Message> {
    if channel_id == 0 {
        if let Some(main) = app.main_mut() {
            main.voice.give_up_intents();
        }
        let closing = close_session(app);
        // The server sends the very same frame when the channel is deleted or the
        // right to be in it goes away, so nothing here may blame a moderator.
        app.toast(ToastKind::Info, "Removed from voice".to_owned());
        return closing;
    }

    let name = app
        .main()
        .map(|main| main.server.channel_title(channel_id))
        .unwrap_or_default();
    if let Some(main) = app.main_mut() {
        main.voice.switch_to(channel_id);
    }
    // The server already took the session away; only the new one is asked for.
    let closing = close_session(app);
    request_join(app, channel_id);
    app.toast(ToastKind::Info, format!("Moved to {name}"));
    closing
}

/// Where to send media and with which key. The frame carries the session key, so
/// nothing here logs it.
fn voice_ready(
    app: &mut App,
    channel_id: i64,
    host: String,
    port: u16,
    key: MediaKey,
    ssrc: u32,
) -> Task<Message> {
    // An empty host means the relay lives wherever the WebSocket goes.
    let host = if host.trim().is_empty() {
        app.endpoints.host()
    } else {
        host
    };

    let Some(main) = app.main() else {
        return Task::none();
    };
    // The answer to a join this client has already moved past.
    if channel_id != main.voice.channel_id {
        tracing::debug!(
            channel_id,
            joined = main.voice.channel_id,
            "ignoring a VoiceReady for another channel"
        );
        return Task::none();
    }
    // Left before the answer came back; the server takes the session away again
    // when it reaches the pending LeaveVoice.
    if !main.voice.intent {
        if let Some(main) = app.main_mut() {
            main.voice.joining = false;
        }
        return Task::none();
    }

    // A second VoiceReady carries a new key; the engine holding the old one goes.
    let closing = close_session(app);
    if let Some(main) = app.main_mut() {
        main.voice.joining = true;
        main.voice.pending_ssrc = ssrc;
    }

    let config = MediaConfig {
        host,
        port,
        key: key.0,
        ssrc,
    };
    Task::batch([
        closing,
        Task::perform(
            async move {
                MediaEngine::connect(config)
                    .await
                    .map_err(|error| error.to_string())
            },
            move |result| {
                Message::Voice(VoiceMsg::MediaConnected(
                    result.map(|engine| EngineHandoff::new(ssrc, engine)),
                ))
            },
        ),
    ])
}

/// The media path is up: the audio thread opens the devices on it, and the share
/// and the watch are asked for again.
fn media_connected(app: &mut App, handoff: EngineHandoff) -> Task<Message> {
    let settings = audio_settings(&app.config);
    let transmit = transmit_settings(&app.config);
    let cleanup = cleanup_settings(&app.config);
    let peer_audio = peer_audio_map(&app.config);
    // A duplicate of a message already handled carries an empty handoff.
    let Some(engine) = handoff.take() else {
        return Task::none();
    };

    let Some(main) = app.main() else {
        return Task::perform(engine.close(), |()| Message::Noop);
    };
    // Left, or signed out, while the socket was opening.
    if !main.voice.intent {
        if let Some(main) = app.main_mut() {
            main.voice.joining = false;
        }
        return Task::perform(engine.close(), |()| Message::Noop);
    }
    // A newer VoiceReady is already being answered, or the connection ended: this
    // engine holds a key the server has replaced.
    if handoff.ssrc != main.voice.pending_ssrc {
        tracing::debug!(
            ssrc = handoff.ssrc,
            pending = main.voice.pending_ssrc,
            "dropping a media engine the session moved past"
        );
        return Task::perform(engine.close(), |()| Message::Noop);
    }

    // One session at a time: anything still open goes before this one is
    // installed.
    let closing = close_session(app);
    let events = app.ensure_audio();
    let sender = engine.sender();
    let playout = engine.playout();

    // Before `Open`, so the input opens straight into the user's chain instead of
    // building the default one first.
    app.send_audio(AudioCommand::SetCleanup(cleanup));
    app.send_audio(AudioCommand::Open {
        settings,
        sender: sender.clone(),
        playout: playout.clone(),
    });
    app.send_audio(AudioCommand::SetTransmit(transmit));

    let Some(main) = app.main_mut() else {
        return Task::batch([
            closing,
            events,
            Task::perform(engine.close(), |()| Message::Noop),
        ]);
    };
    let (muted, deafened, ptt_held) = (main.voice.muted, main.voice.deafened, main.voice.ptt_held);
    main.voice.session = Some(MediaSession {
        // The handoff's own ssrc, which the check above found to be the one being
        // waited for: `pending_ssrc` is the media path's and goes with it.
        ssrc: handoff.ssrc,
        engine,
        playout,
        sender,
        audio_input: None,
        audio_output: None,
    });
    main.voice.joining = false;
    main.voice.peer_audio = peer_audio;

    // The thread outlives a session, so every flag it keeps is set again here.
    app.send_audio(AudioCommand::SetMuted(muted));
    app.send_audio(AudioCommand::SetDeafened(deafened));
    app.send_audio(AudioCommand::SetPtt(ptt_held));
    // `Open` forgets every peer, so the stored tuning follows it rather than
    // preceding it.
    push_peers(app);
    push_ducking(app);

    // The share starts again on the new sender. The `VoiceState` behind the
    // `VoiceReady` normally lands before this, so the watch is usually decided
    // right here rather than left waiting for one.
    let share = share::after_voice_ready(app);
    let hotkey = start_hotkey(app);
    Task::batch([closing, events, share, hotkey])
}

fn media_failed(app: &mut App, reason: String) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.intent = false;
    main.voice.joining = false;
    app.toast(ToastKind::Error, format!("Voice: {reason}"));
    Task::none()
}

fn on_audio(app: &mut App, event: AudioEvent) -> Task<Message> {
    if matches!(event, AudioEvent::Opened { .. }) {
        // An output device opened here, so the chime has one as well: its earlier
        // failure is worth another try.
        app.audio_unavailable = false;
    }

    match event {
        AudioEvent::Opened { input, output } => {
            if let Some(main) = app.main_mut()
                && let Some(session) = &mut main.voice.session
            {
                session.audio_input = input;
                session.audio_output = Some(output);
            }
        }
        AudioEvent::Failed(reason) => app.toast(ToastKind::Error, format!("Audio: {reason}")),
        AudioEvent::Closed => {}
        AudioEvent::InputLevel { dbfs, gate_open } => {
            if let Some(main) = app.main_mut() {
                main.voice.input_level = Some((dbfs, gate_open));
            }
        }
        AudioEvent::Transmitting(transmitting) => {
            if let Some(main) = app.main_mut() {
                main.voice.transmitting = transmitting;
            }
        }
    }
    Task::none()
}

/// Who is in one channel's voice session, as the server last described it.
fn voice_state(app: &mut App, channel_id: i64, members: Vec<VoiceMember>) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.set_roster(channel_id, members);
    if channel_id != main.voice.channel_id {
        return Task::none();
    }
    // A rejoin brings new ssrcs, so the stored tuning has to follow them.
    push_peers(app);
    push_ducking(app);

    // The media path came up before this roster did, so the watch a reconnect kept
    // is still waiting to be judged.
    let pending = app
        .main()
        .is_some_and(|main| main.voice.watch.resume_pending);
    if pending {
        return share::resume_watch(app);
    }
    Task::none()
}

fn member_joined(app: &mut App, channel_id: i64, member: VoiceMember) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let joined = channel_id == main.voice.channel_id;
    let peer = joined.then(|| (member.ssrc, main.voice.peer_audio(member.user_id)));
    main.voice.insert_member(channel_id, member);

    // A rejoin brings a new ssrc, so the tuning has to follow it.
    if let Some((ssrc, audio)) = peer {
        send_peer(app, ssrc, audio);
        push_ducking(app);
    }
    Task::none()
}

fn member_left(app: &mut App, channel_id: i64, user_id: i64) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    main.voice.remove_member(channel_id, user_id);
    if channel_id != main.voice.channel_id {
        return Task::none();
    }
    // The server dropped this client's own session — a leave from another device, a
    // permission that is gone. Nothing is on the relay any more, so the media path
    // goes with it; the intent stays, and the next connection asks again.
    if user_id == main.member_id && main.voice.intent {
        return close_session(app);
    }
    push_ducking(app);
    Task::none()
}

fn moderate(
    app: &mut App,
    user_id: i64,
    muted: Option<bool>,
    deafened: Option<bool>,
    move_to: Option<i64>,
) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // The session being moderated is the one the target is in, which is not always
    // the one this client joined.
    let channel_id = main
        .voice
        .channel_of(user_id)
        .unwrap_or(main.voice.channel_id);
    main.send_or_notice(Command::Admin(AdminCommand::VoiceModerate {
        user_id,
        channel_id,
        muted,
        deafened,
        move_to,
    }));
    Task::none()
}

fn toggle_mute(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let voice = &mut main.voice;
    voice.muted = !voice.muted;
    // Speaking again while deafened means hearing again too.
    if !voice.muted {
        voice.deafened = false;
    }
    let (muted, deafened) = (voice.muted, voice.deafened);
    push_flags(app, muted, deafened);
    Task::none()
}

fn toggle_deafen(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let voice = &mut main.voice;
    voice.deafened = !voice.deafened;
    if voice.deafened {
        voice.muted_before_deafen = voice.muted;
        voice.muted = true;
    } else {
        voice.muted = voice.muted_before_deafen;
    }
    let (muted, deafened) = (voice.muted, voice.deafened);
    push_flags(app, muted, deafened);
    Task::none()
}

fn set_transmit_mode(app: &mut App, mode: TransmitMode) -> Task<Message> {
    if app.config.transmit_mode == mode {
        return Task::none();
    }
    app.config.transmit_mode = mode;
    app.save_config();
    push_transmit(app);
    // Push to talk is only observed in one of the modes, so the listener's
    // shortcut set changes with it — and a binding held as the mode changed would
    // otherwise never be released.
    restart_hotkey(app)
}

/// One edge of a bound input, wherever it came from: the system-wide listener, or
/// the window itself when there is none.
fn hotkey(app: &mut App, generation: u64, action: ActionId, edge: Edge) -> Task<Message> {
    if !hotkey_is_current(app, generation) {
        return Task::none();
    }
    match (action, edge) {
        // Key repeat sends a press per repeat, so only the edges reach the audio
        // thread.
        (PUSH_TO_TALK, edge) => {
            set_ptt(app, edge == Edge::Pressed);
            Task::none()
        }
        (TOGGLE_MUTE, Edge::Pressed) => toggle_mute(app),
        (TOGGLE_DEAFEN, Edge::Pressed) => toggle_deafen(app),
        _ => Task::none(),
    }
}

/// Whether a generation is the listener this session is on. Anything older belongs
/// to a start or an edge stream that has been replaced; the window's own edges
/// belong to no listener at all.
fn hotkey_is_current(app: &App, generation: u64) -> bool {
    generation == WINDOW_GENERATION
        || app
            .main()
            .is_some_and(|main| main.voice.hotkey_generation == generation)
}

/// Whether the system-wide listener is what drives one bound input. While it is,
/// the window must not act on it as well: the listener reports the same press. A
/// binding the backend could not observe is not the listener's, so the window
/// keeps exactly that one.
pub fn hotkey_observes(app: &App, action: ActionId) -> bool {
    app.main()
        .is_some_and(|main| main.voice.hotkey_status.observes(action))
}

/// Starts the system-wide listener for the session that is open. Doing nothing is
/// the common case: no session, or a listener already running or on its way.
fn start_hotkey(app: &mut App) -> Task<Message> {
    let shortcuts = shortcuts(&app.config);
    let bindings: Vec<Binding> = shortcuts.iter().map(|shortcut| shortcut.binding).collect();
    let unbindable = unbindable(&app.config);

    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let voice = &mut main.voice;
    let Some(session) = &voice.session else {
        return Task::none();
    };
    let ssrc = session.ssrc;
    if voice.hotkey.is_some() || voice.hotkey_status == HotkeyStatus::Starting {
        return Task::none();
    }
    if shortcuts.is_empty() {
        // Such a binding never starts a listener, and the window still holds it.
        voice.hotkey_status = HotkeyStatus::WindowOnly(format!(
            "'{unbindable}' works only while the window is focused; bind a listed key for system-wide capture"
        ));
        return Task::none();
    }

    voice.hotkey_status = HotkeyStatus::Starting;
    voice.hotkey_generation = voice.hotkey_generation.wrapping_add(1);
    let generation = voice.hotkey_generation;

    // Starting blocks — on Wayland for as long as the compositor keeps its dialog
    // up — so it never runs on this thread.
    let (edges, stream) = mpsc::unbounded();
    // A backend that stops on its own drops its sender, and the end of the stream
    // is the only word of it the window ever gets.
    let messages = stream
        .map(move |(action, edge)| {
            Message::Voice(VoiceMsg::Hotkey {
                generation,
                action,
                edge,
            })
        })
        .chain(futures::stream::once(async move {
            Message::Voice(VoiceMsg::HotkeyEnded { generation })
        }));

    Task::batch([
        Task::perform(
            tokio::task::spawn_blocking(move || Listener::start(shortcuts, edges)),
            move |joined| {
                let started = joined.unwrap_or_else(|e| Err(Unavailable::Failed(e.to_string())));
                Message::Voice(VoiceMsg::HotkeyStarted(HotkeyHandoff::new(
                    generation, ssrc, bindings, started,
                )))
            },
        ),
        Task::stream(messages),
    ])
}

fn hotkey_started(app: &mut App, handoff: HotkeyHandoff) -> Task<Message> {
    // A duplicate of a message already handled carries an empty handoff.
    let Some(started) = handoff.take() else {
        return Task::none();
    };
    // Whatever this was started for is gone; dropping the answer stops the listener
    // it may carry, rather than making it a second one.
    let wanted: Vec<Binding> = shortcuts(&app.config)
        .iter()
        .map(|shortcut| shortcut.binding)
        .collect();
    let waiting = app.main().is_some_and(|main| {
        let voice = &main.voice;
        voice.hotkey_generation == handoff.generation
            && voice.hotkey_status == HotkeyStatus::Starting
            && voice.hotkey.is_none()
            && voice
                .session
                .as_ref()
                .is_some_and(|session| session.ssrc == handoff.ssrc)
    });
    if wanted != handoff.bindings || !waiting {
        return Task::none();
    }

    match started {
        Ok(listener) => {
            // A binding may have been pressed while this was starting: the window
            // saw that press, and the backend will drop the release it never saw
            // the press for.
            set_ptt(app, false);

            // A binding this backend cannot observe at all costs only itself: the
            // window keeps that one action while the listener holds the rest.
            let window_only: Vec<(ActionId, String)> = listener
                .unavailable()
                .iter()
                .map(|(action, reason)| (*action, reason.to_string()))
                .collect();
            let note = window_only_note(&window_only);
            let status = HotkeyStatus::Global {
                backend: listener.backend(),
                trigger: listener.trigger_description(PUSH_TO_TALK),
                window_only,
            };
            if let Some(main) = app.main_mut() {
                main.voice.hotkey_status = status;
                main.voice.hotkey = Some(listener);
            }
            if let Some(note) = note {
                tracing::info!(note = %note, "some bindings are the window's own");
                app.toast(ToastKind::Info, note);
            }
        }
        Err(e) => {
            tracing::info!(error = %e, "no system-wide capture; the window keeps its own");
            if let Some(main) = app.main_mut() {
                main.voice.hotkey_status = HotkeyStatus::WindowOnly(e.to_string());
            }
        }
    }
    Task::none()
}

/// A listener that stopped by itself — a session that ended under it, a portal the
/// user revoked — leaves the window holding the bound inputs.
fn hotkey_ended(app: &mut App, generation: u64) -> Task<Message> {
    let running = app.main().is_some_and(|main| {
        main.voice.hotkey_generation == generation
            && matches!(main.voice.hotkey_status, HotkeyStatus::Global { .. })
    });
    if !running {
        return Task::none();
    }

    tracing::info!("the system-wide listener stopped; the window keeps its own");
    // Nothing will report the release of a binding held right now.
    set_ptt(app, false);
    if let Some(main) = app.main_mut() {
        main.voice.hotkey = None;
        main.voice.hotkey_status = HotkeyStatus::WindowOnly("global capture stopped".to_owned());
    }
    Task::none()
}

/// Moves the session onto a fresh listener: a rebound key, or a retry from the
/// settings page.
pub fn restart_hotkey(app: &mut App) -> Task<Message> {
    // The binding that was replaced is possibly held right now, and its release
    // will no longer match anything.
    set_ptt(app, false);
    if let Some(main) = app.main_mut() {
        drop_hotkey(&mut main.voice);
    }
    start_hotkey(app)
}

/// Stops the listener and moves past it: no start still in flight, no edge stream
/// and no answer from either is acted on afterwards.
fn drop_hotkey(voice: &mut VoiceUi) {
    voice.hotkey = None;
    voice.hotkey_status = HotkeyStatus::Off;
    voice.hotkey_generation = voice.hotkey_generation.wrapping_add(1);
}

/// The global actions this mode observes: push to talk is only watched for in the
/// mode that uses it.
fn wanted_actions(config: &Config) -> impl Iterator<Item = (ActionId, &'static str, &'static str)> {
    let push_to_talk = config.transmit_mode == TransmitMode::PushToTalk;
    GLOBAL_ACTIONS
        .into_iter()
        .filter(move |(action, _, _)| *action != PUSH_TO_TALK || push_to_talk)
}

/// The shortcuts a listener is asked for: whatever of the wanted actions this
/// configuration can actually bind.
fn shortcuts(config: &Config) -> Vec<Shortcut> {
    wanted_actions(config)
        .filter_map(|(action, name, description)| {
            Some(Shortcut {
                action,
                binding: Binding::parse(config.keybind(name))?,
                description: description.to_owned(),
            })
        })
        .collect()
}

/// The one note a listener's skipped bindings are worth: which action stayed the
/// window's, and what the backend said about it. `None` when it bound everything.
fn window_only_note(window_only: &[(ActionId, String)]) -> Option<String> {
    if window_only.is_empty() {
        return None;
    }
    let lines: Vec<String> = window_only
        .iter()
        .map(|(action, reason)| {
            format!(
                "{} works only while the window is focused: {reason}",
                action_label(*action)
            )
        })
        .collect();
    Some(lines.join("; "))
}

/// What a system-wide action is called in a note to the user, which is the same
/// wording a Wayland compositor is asked to show.
fn action_label(action: ActionId) -> &'static str {
    GLOBAL_ACTIONS
        .iter()
        .find(|(id, _, _)| *id == action)
        .map_or("That binding", |(_, _, description)| *description)
}

/// What the settings sentence names when nothing could be bound: the bindings no
/// backend can observe, as the interface spells them.
fn unbindable(config: &Config) -> String {
    let names: Vec<String> = wanted_actions(config)
        .map(|(_, name, _)| config.keybind(name))
        .filter(|binding| Binding::parse(binding).is_none())
        .map(rules::key_label)
        .collect();
    names.join("', '")
}

fn set_ptt(app: &mut App, held: bool) {
    let Some(main) = app.main_mut() else {
        return;
    };
    if main.voice.ptt_held == held || !main.voice.is_live() {
        return;
    }
    main.voice.ptt_held = held;
    app.send_audio(AudioCommand::SetPtt(held));
}

fn push_flags(app: &App, muted: bool, deafened: bool) {
    if !app.main().is_some_and(|main| main.voice.is_live()) {
        return;
    }
    app.send_audio(AudioCommand::SetMuted(muted));
    app.send_audio(AudioCommand::SetDeafened(deafened));
}

fn push_transmit(app: &App) {
    app.send_audio(AudioCommand::SetTransmit(transmit_settings(&app.config)));
}

fn push_cleanup(app: &App) {
    app.send_audio(AudioCommand::SetCleanup(cleanup_settings(&app.config)));
}

fn push_devices(app: &App) {
    app.send_audio(AudioCommand::SetDevices(audio_settings(&app.config)));
}

/// Every other member's stored volume and mute. An ssrc belongs to one session, so
/// this runs again whenever the roster brings new ones.
fn push_peers(app: &App) {
    let Some(main) = app.main() else {
        return;
    };
    if !main.voice.is_live() {
        return;
    }
    for (ssrc, audio) in main.voice.peers(main.member_id) {
        send_peer(app, ssrc, audio);
    }
}

/// The audio thread is the only writer of a peer's gain: ducking multiplies into
/// the very same number, so two writers would undo each other.
fn send_peer(app: &App, ssrc: u32, audio: PeerAudio) {
    app.send_audio(AudioCommand::SetPeerVolume {
        ssrc,
        volume: audio.volume,
    });
    app.send_audio(AudioCommand::SetPeerMuted {
        ssrc,
        muted: audio.muted,
    });
}

/// Quietens the room while a priority speaker talks, and only when that changes.
fn push_ducking(app: &mut App) {
    let Some(main) = app.main_mut() else {
        return;
    };
    let voice = &mut main.voice;
    if !voice.is_live() {
        return;
    }
    let wanted = voice
        .roster()
        .map(|roster| roster.ducking())
        .unwrap_or_default();
    if wanted == voice.ducking {
        return;
    }
    voice.ducking = wanted.clone();
    app.send_audio(AudioCommand::SetDucking {
        active: wanted.active,
        exempt: wanted.exempt,
    });
}

/// Keeps the three copies of one peer's tuning together: the configuration, the
/// mirror the roster applies from, and the mixer itself. Saving is the caller's, so
/// a slider drag writes the file once, on release.
fn set_peer_audio(app: &mut App, user_id: i64, edit: impl FnOnce(&mut PeerAudio)) {
    let mut audio = app.config.peer_audio(user_id);
    edit(&mut audio);
    app.config.set_peer_audio(user_id, audio);
    // Read back: the setter is what clamps the volume.
    let stored = app.config.peer_audio(user_id);

    let Some(main) = app.main_mut() else {
        return;
    };
    main.voice.peer_audio.insert(user_id, stored);
    let ssrc = main.voice.ssrc_of(user_id);
    if let Some(ssrc) = ssrc {
        send_peer(app, ssrc, stored);
    }
}

fn audio_settings(config: &Config) -> AudioSettings {
    AudioSettings {
        input: config.input_device.clone(),
        output: config.output_device.clone(),
    }
}

fn transmit_settings(config: &Config) -> TransmitSettings {
    TransmitSettings {
        mode: config.transmit_mode,
        threshold_db: config.vad_threshold_db,
    }
}

fn cleanup_settings(config: &Config) -> CleanupSettings {
    CleanupSettings {
        noise_suppression: config.noise_suppression,
        echo_cancellation: config.echo_cancellation,
        auto_gain: config.auto_gain,
    }
}

/// The stored tuning, keyed the way the roster needs it. A key that is not a user
/// id can only come from a hand-edited file.
fn peer_audio_map(config: &Config) -> BTreeMap<i64, PeerAudio> {
    config
        .peer_audio
        .iter()
        .filter_map(|(user_id, audio)| Some((user_id.parse().ok()?, *audio)))
        .collect()
}
