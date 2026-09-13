//! The audio devices behind a voice channel, on a thread of their own.
//!
//! Opening a device blocks for as long as the host wants, and a cpal stream is
//! `!Send` on some hosts, so both streams are created, owned and dropped on the
//! thread [`spawn_audio_thread`] starts. The UI only ever pushes an
//! [`AudioCommand`] into a channel and reads [`AudioEvent`]s back.
//!
//! Capture runs one way: the cpal callback appends interleaved samples to a ring,
//! the thread downmixes them to mono, resamples to 48 kHz when the device runs at
//! another rate, cuts 20 ms frames, and hands the Opus packets to the media
//! engine while the frame is allowed out: in push-to-talk mode while the key is
//! held, in voice-activation mode while the noise gate stands open. Every cut
//! frame first passes through the [`InputCleanup`] chain, so the gate, the level
//! meter and the encoder all see the cleaned audio.
//! Playback runs the other way: a single infinite [`VoiceSource`] sits in the
//! rodio mixer and pulls mixed stereo frames from [`Playout`], which the engine's
//! receive task keeps fed; that source also keeps the mono downmix of what it
//! played in two rings, the far-end reference the microphone's echo canceller
//! subtracts here and the one a screen share's own canceller subtracts on its
//! thread. Two more sounds ride that same output: a soundpad clip, which the
//! mixer carries like a voice and a deafen silences with them, and the interface
//! motifs, mixed in past the deafen so the deafen pair still confirms the press.
//!
//! This thread is also the only writer of per-peer volume and mute. Quietening
//! the room for a priority speaker has to multiply into the same single gain the
//! listener set, so the listener's own value is kept here and the mixer is only
//! ever told the product.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::num::NonZero;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::channel::mpsc as async_mpsc;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters};
use vorcall_core::config::{TransmitMode, VAD_DEFAULT_DB};
use vorcall_voice::cleanup::FAR_END_MAX_SAMPLES;
use vorcall_voice::codec::Encoder;
use vorcall_voice::{
    CleanupSettings, FRAME_SAMPLES, FrameSender, GateDecision, InputCleanup, NoiseGate, Playout,
    SAMPLE_RATE, STEREO_FRAME_SAMPLES, Sfx,
};

/// How often the thread wakes up to cut frames when no command arrives. Well
/// under the 20 ms a frame lasts, so the capture ring never runs long.
const TICK: Duration = Duration::from_millis(5);

/// Neither the capture ring nor the mono queue may hold more than this: a stalled
/// thread is allowed to lose audio, never to build up delay.
const MAX_QUEUED_MS: usize = 200;

/// A silence at least this long ends a talk spurt, so the next frame sent is
/// marked as the start of a new one.
const SPURT_GAP: Duration = Duration::from_millis(200);

/// Frames between two input-level events: five 20 ms frames make the 10 Hz the
/// settings meter is drawn at.
const LEVEL_EVERY_FRAMES: u8 = 5;

/// Opus at 48 kbit/s over 20 ms frames never comes near this.
const MAX_PACKET: usize = 512;

/// A device that fails does so on every frame; the log must not become the
/// problem.
const WARN_INTERVAL: Duration = Duration::from_secs(1);

/// How often the canceller's delay and return-loss figures go to the debug log
/// while echo cancellation runs.
const METRICS_EVERY: Duration = Duration::from_secs(10);

/// 200 ms of 48 kHz mono.
const MONO_QUEUE_MAX: usize = SAMPLE_RATE as usize * MAX_QUEUED_MS / 1000;

/// How much quieter a peer gets while a priority speaker talks: −12 dB, low
/// enough to be talked over and high enough to keep the room audible.
const DUCK_FACTOR: f32 = 0.25;

/// How often a running duck looks for speakers it has no entry for yet. Most
/// peers are never touched in the interface, so without this pass a duck would
/// only quieten the few the listener happens to have a slider on.
const DUCK_SWEEP: Duration = Duration::from_millis(100);

/// How many interface motifs may sound at once; a fifth drops the oldest. They
/// last 50-150 ms, so the bound is never reached in practice and is only here so
/// a stuck caller cannot grow the list without end.
const MAX_SFX: usize = 4;

/// The ceiling the motif volume shares with every other volume in the mixer.
const MAX_SFX_GAIN: f32 = 2.0;

/// Shown for a device the host refuses to name; it is still usable.
const UNNAMED_DEVICE: &str = "Unknown device";

/// The devices the settings screen offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceLists {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// Enumerating devices talks to the audio host and blocks, so the caller runs
/// this off the UI thread.
pub fn list_devices() -> DeviceLists {
    let host = cpal::default_host();
    DeviceLists {
        inputs: device_names(host.input_devices()),
        outputs: device_names(host.output_devices()),
    }
}

/// `None` means the system default. A name that no longer matches any device
/// falls back to the default rather than failing: hardware comes and goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioSettings {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// How the thread decides that a frame leaves the machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransmitSettings {
    pub mode: TransmitMode,
    pub threshold_db: f32,
}

impl Default for TransmitSettings {
    fn default() -> Self {
        Self {
            mode: TransmitMode::PushToTalk,
            threshold_db: VAD_DEFAULT_DB,
        }
    }
}

pub enum AudioCommand {
    Open {
        settings: AudioSettings,
        sender: FrameSender,
        playout: Arc<Mutex<Playout>>,
    },
    /// Reopens both streams, unless nothing changed.
    SetDevices(AudioSettings),
    /// Mode and gate threshold; takes effect on the next frame.
    SetTransmit(TransmitSettings),
    /// Which cleanup stages run; rebuilds the chain, so the canceller starts from
    /// scratch.
    SetCleanup(CleanupSettings),
    SetPtt(bool),
    SetMuted(bool),
    SetDeafened(bool),
    /// The volume the listener chose for one peer, by ssrc. It comes through here
    /// rather than straight into the mixer because ducking multiplies into that
    /// same gain: two writers would undo each other.
    SetPeerVolume {
        ssrc: u32,
        volume: f32,
    },
    /// The mute the listener chose for one peer, by ssrc; here for the same reason
    /// as [`AudioCommand::SetPeerVolume`], so one path owns a peer's tuning.
    SetPeerMuted {
        ssrc: u32,
        muted: bool,
    },
    /// While a priority speaker is talking every other peer is quieter.
    /// `exempt` is the ssrcs that keep their own gain.
    SetDucking {
        active: bool,
        exempt: Vec<u32>,
    },
    /// One of the interface motifs, played locally and never sent anywhere.
    PlaySfx(Sfx),
    /// A soundpad clip, already decoded to interleaved stereo 48 kHz. Replaces
    /// whatever clip was playing: one is heard at a time.
    PlayClip(Arc<Vec<f32>>),
    StopClip,
    /// The listener's own volume for the motifs, 0.0..=2.0.
    SetSfxVolume(f32),
    /// The listener's own volume for soundpad clips, 0.0..=2.0.
    SetClipVolume(f32),
    /// Drops both streams. The thread stays alive for a later `Open`.
    Close,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    /// `input` is None when the microphone could not be opened: playback still
    /// works (listen-only) and `Failed` was sent with the reason.
    Opened {
        input: Option<String>,
        output: String,
    },
    Failed(String),
    Closed,
    /// Every 100 ms while a microphone is open: the last frame's level in dBFS
    /// and whether the gate is open.
    InputLevel {
        dbfs: f32,
        gate_open: bool,
    },
    /// On every change of the effective "audio is leaving this machine" state.
    Transmitting(bool),
}

#[derive(Clone)]
pub struct AudioHandle {
    commands: Sender<AudioCommand>,
    share_far_end: Arc<Mutex<VecDeque<f32>>>,
}

impl AudioHandle {
    /// Never blocks: the command channel is unbounded, and a thread that has
    /// already exited only costs a log line.
    pub fn send(&self, command: AudioCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("the audio thread is gone, dropping the command");
        }
    }

    /// The mono downmix of everything the mixer plays, for a screen share's own
    /// echo canceller. It is the audio thread that fills it, so the ring outlives
    /// any one share and the share thread only ever drains it.
    pub fn share_far_end(&self) -> Arc<Mutex<VecDeque<f32>>> {
        self.share_far_end.clone()
    }
}

/// Spawns the dedicated audio thread. The thread exits once every
/// [`AudioHandle`] has been dropped.
pub fn spawn_audio_thread() -> (AudioHandle, async_mpsc::UnboundedReceiver<AudioEvent>) {
    let (commands, requests) = std::sync::mpsc::channel();
    let (events, updates) = async_mpsc::unbounded();
    let share_far_end = Arc::new(Mutex::new(VecDeque::with_capacity(FAR_END_MAX_SAMPLES)));

    let played = share_far_end.clone();
    let spawned = std::thread::Builder::new()
        .name("vorcall-audio".to_string())
        .spawn(move || run(requests, events, played));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the audio thread");
    }

    (
        AudioHandle {
            commands,
            share_far_end,
        },
        updates,
    )
}

fn run(
    requests: Receiver<AudioCommand>,
    events: async_mpsc::UnboundedSender<AudioEvent>,
    share_far_end: Arc<Mutex<VecDeque<f32>>>,
) {
    let mut state = AudioThread::new(events, share_far_end);
    loop {
        match requests.recv_timeout(TICK) {
            Ok(command) => state.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        state.pump();
    }
    state.close_streams();
}

struct AudioThread {
    events: async_mpsc::UnboundedSender<AudioEvent>,
    settings: AudioSettings,
    sender: Option<FrameSender>,
    playout: Option<Arc<Mutex<Playout>>>,

    output: Option<rodio::MixerDeviceSink>,
    capture: Option<Capture>,
    encoder: Option<Encoder>,

    transmit: TransmitSettings,
    gate: NoiseGate,
    /// The last [`AudioEvent::Transmitting`] sent.
    transmitting: bool,
    frames_since_level: u8,

    ptt: bool,
    muted: bool,
    /// Shared with the [`VoiceSource`] in the mixer, which keeps pulling frames
    /// while deafened so the jitter buffers do not back up.
    deafened: Arc<AtomicBool>,
    /// The interface motifs, shared with that same [`VoiceSource`]. It belongs to
    /// the thread rather than to a session: a motif is local, so it is played
    /// whether or not a voice channel is open.
    sfx: Arc<Mutex<SfxMixer>>,

    /// What the listener chose per peer, by ssrc. The mixer is given the product
    /// of it and the ducking factor, so the chosen value survives a duck.
    peers: HashMap<u32, PeerTuning>,
    ducking: Ducking,
    /// When the running duck last took in the speakers it did not know about.
    duck_swept_at: Option<Instant>,

    cleanup_settings: CleanupSettings,
    cleanup: Option<InputCleanup>,
    /// Set once the chain failed to build or crashed; cleared by the next
    /// `SetCleanup` or `Close`, so a broken chain is not rebuilt on every frame.
    cleanup_failed: bool,
    /// What the mixer played, pushed by the [`VoiceSource`] on rodio's thread and
    /// drained here into the canceller. Bounded like the capture ring: a stalled
    /// thread loses reference audio, never builds up delay.
    far_end: Arc<Mutex<VecDeque<f32>>>,
    /// The same reference for a screen share's loopback canceller, drained on the
    /// share thread instead of here: two consumers, two rings.
    share_far_end: Arc<Mutex<VecDeque<f32>>>,
    far_end_scratch: Vec<f32>,
    /// The frame as captured, restored if the chain fails mid-frame.
    raw_frame: [f32; FRAME_SAMPLES],
    /// When the canceller's metrics last went to the log.
    metrics_at: Option<Instant>,

    /// Interleaved samples taken from the capture ring, then their downmix.
    raw: Vec<f32>,
    mono: Vec<f32>,
    /// 48 kHz mono, waiting to be cut into frames.
    queue: VecDeque<f32>,
    packet: [u8; MAX_PACKET],
    last_sent: Option<Instant>,
    encode_warning: Throttle,
    send_warning: Throttle,
}

/// The microphone stream and everything that turns its samples into 48 kHz mono.
struct Capture {
    /// Only held so the stream keeps running; it stops the moment it is dropped,
    /// which must happen on this thread.
    _stream: cpal::Stream,
    ring: Arc<Mutex<VecDeque<f32>>>,
    channels: usize,
    /// `None` when the device already runs at 48 kHz.
    resampler: Option<Resample>,
}

/// What the listener did to one peer, before any ducking.
#[derive(Clone, Copy)]
struct PeerTuning {
    volume: f32,
    muted: bool,
}

impl Default for PeerTuning {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

/// Whether a priority speaker is talking, and who is exempt from being quietened
/// for them.
#[derive(Default)]
struct Ducking {
    active: bool,
    exempt: Vec<u32>,
}

impl Ducking {
    /// What this peer's chosen volume is multiplied by.
    fn factor(&self, ssrc: u32) -> f32 {
        if self.active && !self.exempt.contains(&ssrc) {
            DUCK_FACTOR
        } else {
            1.0
        }
    }

    /// The gain the mixer is given for a peer the listener set to `volume`.
    fn gain(&self, ssrc: u32, volume: f32) -> f32 {
        volume * self.factor(ssrc)
    }
}

/// The interface motifs waiting to be heard. Several may overlap — a join and a
/// mute can land in the same frame — so this is a list rather than a slot.
struct SfxMixer {
    playing: Vec<SfxPlayback>,
    gain: f32,
}

/// One motif part-way through.
struct SfxPlayback {
    /// Mono 48 kHz.
    samples: Vec<f32>,
    cursor: usize,
    bypass_deafen: bool,
}

impl SfxMixer {
    fn new() -> Self {
        Self {
            playing: Vec::new(),
            gain: 1.0,
        }
    }

    /// Starts one motif, dropping the oldest rather than growing past [`MAX_SFX`].
    ///
    /// The samples come in already synthesized: generating them costs thousands
    /// of `sin()` calls, and this mutex is the one the output callback takes on
    /// every frame.
    fn play(&mut self, samples: Vec<f32>, bypass_deafen: bool) {
        if self.playing.len() >= MAX_SFX {
            self.playing.remove(0);
        }
        self.playing.push(SfxPlayback {
            samples,
            cursor: 0,
            bypass_deafen,
        });
    }

    /// Drops everything queued. Motifs that piled up while there was no output
    /// stream must not all be heard at once when one opens.
    fn clear(&mut self) {
        self.playing.clear();
    }

    fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, MAX_SFX_GAIN);
    }

    /// Sums the next mono frame of everything playing into `out`. While
    /// `deafened` only the motifs that bypass it are heard, but every one of them
    /// still advances and retires: a motif held back must not resume mid-way when
    /// the listener undeafens.
    fn next_frame(&mut self, out: &mut [f32; FRAME_SAMPLES], deafened: bool) {
        out.fill(0.0);
        let gain = self.gain;
        self.playing.retain_mut(|playback| {
            let remaining = &playback.samples[playback.cursor..];
            let taken = remaining.len().min(FRAME_SAMPLES);
            if !deafened || playback.bypass_deafen {
                for (sum, sample) in out.iter_mut().zip(&remaining[..taken]) {
                    *sum += *sample * gain;
                }
            }
            playback.cursor += taken;
            playback.cursor < playback.samples.len()
        });
    }
}

impl AudioThread {
    fn new(
        events: async_mpsc::UnboundedSender<AudioEvent>,
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    ) -> Self {
        let transmit = TransmitSettings::default();
        Self {
            events,
            settings: AudioSettings::default(),
            sender: None,
            playout: None,
            output: None,
            capture: None,
            encoder: None,
            gate: NoiseGate::new(transmit.threshold_db),
            transmit,
            transmitting: false,
            frames_since_level: 0,
            ptt: false,
            muted: false,
            deafened: Arc::new(AtomicBool::new(false)),
            sfx: Arc::new(Mutex::new(SfxMixer::new())),
            peers: HashMap::new(),
            ducking: Ducking::default(),
            duck_swept_at: None,
            cleanup_settings: CleanupSettings::default(),
            cleanup: None,
            cleanup_failed: false,
            far_end: Arc::new(Mutex::new(VecDeque::with_capacity(FAR_END_MAX_SAMPLES))),
            share_far_end,
            far_end_scratch: Vec::with_capacity(FAR_END_MAX_SAMPLES),
            raw_frame: [0.0; FRAME_SAMPLES],
            metrics_at: None,
            raw: Vec::new(),
            mono: Vec::new(),
            queue: VecDeque::with_capacity(MONO_QUEUE_MAX),
            packet: [0; MAX_PACKET],
            last_sent: None,
            encode_warning: Throttle::default(),
            send_warning: Throttle::default(),
        }
    }

    fn handle(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::Open {
                settings,
                sender,
                playout,
            } => {
                self.close_streams();
                self.settings = settings;
                self.sender = Some(sender);
                self.playout = Some(playout);
                // A new session means new ssrcs, and the new playout carries no
                // tuning at all: the app pushes every peer's in again after this.
                self.forget_peers();
                self.start();
            }
            AudioCommand::SetDevices(settings) => {
                if settings == self.settings {
                    return;
                }
                self.settings = settings;
                if self.playout.is_some() {
                    self.close_streams();
                    self.start();
                }
            }
            AudioCommand::SetTransmit(transmit) => {
                self.transmit = transmit;
                self.gate.set_threshold(transmit.threshold_db);
            }
            AudioCommand::SetCleanup(settings) => {
                if settings == self.cleanup_settings {
                    return;
                }
                self.cleanup_settings = settings;
                self.cleanup_failed = false;
                if self.capture.is_some() {
                    self.build_cleanup();
                }
            }
            AudioCommand::SetPtt(held) => self.ptt = held,
            AudioCommand::SetMuted(muted) => self.muted = muted,
            AudioCommand::SetDeafened(deafened) => self.deafened.store(deafened, Ordering::Relaxed),
            AudioCommand::SetPeerVolume { ssrc, volume } => {
                self.peers.entry(ssrc).or_default().volume = volume;
                self.apply_peer(ssrc);
            }
            AudioCommand::SetPeerMuted { ssrc, muted } => {
                self.peers.entry(ssrc).or_default().muted = muted;
                self.apply_peer(ssrc);
            }
            AudioCommand::SetDucking { active, exempt } => self.set_ducking(active, exempt),
            AudioCommand::PlaySfx(sfx) => {
                // Synthesized before the lock, never under it.
                let samples = sfx.samples();
                lock(&self.sfx).play(samples, sfx.bypass_deafen());
            }
            AudioCommand::PlayClip(samples) => {
                // Unlike a motif, a clip only means anything inside a channel.
                let Some(playout) = self.playout.as_ref() else {
                    tracing::debug!("no voice session, dropping a soundpad clip");
                    return;
                };
                lock(playout).set_clip(samples);
            }
            AudioCommand::StopClip => {
                if let Some(playout) = self.playout.as_ref() {
                    lock(playout).stop_clip();
                }
            }
            AudioCommand::SetSfxVolume(volume) => lock(&self.sfx).set_gain(volume),
            AudioCommand::SetClipVolume(volume) => {
                if let Some(playout) = self.playout.as_ref() {
                    lock(playout).set_clip_gain(volume);
                }
            }
            AudioCommand::Close => {
                self.close_streams();
                self.sender = None;
                self.playout = None;
                self.forget_peers();
                self.emit(AudioEvent::Closed);
            }
        }
    }

    /// Playback comes first: a channel is still worth joining with a broken
    /// microphone, but not with no output at all.
    fn start(&mut self) {
        let Some(playout) = self.playout.clone() else {
            return;
        };

        let output = match self.open_output(playout) {
            Ok(name) => name,
            Err(reason) => {
                self.emit(AudioEvent::Failed(reason));
                return;
            }
        };

        match self.open_input() {
            Ok(input) => self.emit(AudioEvent::Opened {
                input: Some(input),
                output,
            }),
            Err(reason) => {
                self.emit(AudioEvent::Failed(reason));
                self.emit(AudioEvent::Opened {
                    input: None,
                    output,
                });
            }
        }
    }

    fn open_output(&mut self, playout: Arc<Mutex<Playout>>) -> Result<String, String> {
        let host = cpal::default_host();
        let device = pick_device(
            self.settings.output.as_deref(),
            host.output_devices().ok(),
            || host.default_output_device(),
            "output",
        )
        .ok_or_else(|| "No audio output device is available.".to_string())?;

        let name = device_name(&device);
        let mut sink = rodio::DeviceSinkBuilder::from_device(device)
            .and_then(|builder| builder.open_sink_or_fallback())
            .map_err(|error| format!("Cannot open the speakers \"{name}\": {}", chain(&error)))?;
        // Left on, rodio prints to stderr when the sink is dropped.
        sink.log_on_drop(false);
        sink.mixer().add(VoiceSource::new(
            playout,
            self.deafened.clone(),
            self.sfx.clone(),
            self.far_end.clone(),
            self.share_far_end.clone(),
        ));

        self.output = Some(sink);
        Ok(name)
    }

    fn open_input(&mut self) -> Result<String, String> {
        let host = cpal::default_host();
        let device = pick_device(
            self.settings.input.as_deref(),
            host.input_devices().ok(),
            || host.default_input_device(),
            "input",
        )
        .ok_or_else(|| "No microphone is available.".to_string())?;

        let name = device_name(&device);
        let config = input_config(&device)
            .map_err(|reason| format!("Cannot use the microphone \"{name}\": {reason}"))?;
        let channels = usize::from(config.channels.max(1));
        let rate = config.sample_rate;

        let encoder = Encoder::new()
            .map_err(|error| format!("Cannot start the voice encoder: {}", chain(&error)))?;
        let resampler = if rate == SAMPLE_RATE {
            None
        } else {
            Some(Resample::new(rate).map_err(|error| {
                format!(
                    "Cannot resample {rate} Hz to {SAMPLE_RATE} Hz: {}",
                    chain(&error)
                )
            })?)
        };

        // Whole frames only, so draining the ring can never cut one in half.
        let capacity = (rate as usize * MAX_QUEUED_MS / 1000) * channels;
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let callback_ring = ring.clone();

        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut ring = lock(&callback_ring);
                    let overflow = (ring.len() + data.len()).saturating_sub(capacity);
                    // Rounded up to a whole frame: the drain downstream assumes
                    // the ring always holds an exact number of them.
                    let dropped = (overflow.div_ceil(channels) * channels).min(ring.len());
                    ring.drain(..dropped);
                    ring.extend(data.iter().copied());
                },
                |error| tracing::warn!(%error, "microphone stream error"),
                None,
            )
            .map_err(|error| format!("Cannot open the microphone \"{name}\": {}", chain(&error)))?;
        stream.play().map_err(|error| {
            format!("Cannot start the microphone \"{name}\": {}", chain(&error))
        })?;

        self.encoder = Some(encoder);
        self.capture = Some(Capture {
            _stream: stream,
            ring,
            channels,
            resampler,
        });
        // Never fails the open: a microphone with no cleanup still works. It also
        // drops the reference the output has been pushing since it opened, so the
        // canceller starts aligned with this capture.
        self.build_cleanup();
        Ok(name)
    }

    /// Replaces the chain with one for the current settings. A chain that has
    /// already failed stays gone until the user changes a setting or rejoins.
    fn build_cleanup(&mut self) {
        self.cleanup = None;
        lock(&self.far_end).clear();
        self.metrics_at = None;
        if !self.cleanup_settings.any() || self.cleanup_failed {
            return;
        }

        match InputCleanup::new(self.cleanup_settings) {
            Ok(cleanup) => self.cleanup = Some(cleanup),
            Err(error) => {
                tracing::warn!(%error, "input cleanup unavailable, capturing raw");
                self.cleanup_failed = true;
                self.emit(AudioEvent::Failed(format!(
                    "Input cleanup is unavailable: {error}"
                )));
            }
        }
    }

    fn close_streams(&mut self) {
        self.capture = None;
        self.output = None;
        self.encoder = None;
        self.cleanup = None;
        self.cleanup_failed = false;
        self.metrics_at = None;
        lock(&self.far_end).clear();
        // Nothing is playing, so neither canceller has a reference any more.
        lock(&self.share_far_end).clear();
        self.queue.clear();
        self.last_sent = None;
        self.gate = NoiseGate::new(self.transmit.threshold_db);
        self.frames_since_level = 0;
        // Nothing queued here outlives the stream it was queued for.
        lock(&self.sfx).clear();
        self.set_transmitting(false);
    }

    /// Moves everything the microphone captured since the last tick through the
    /// pipeline. Runs even while muted, so the queue cannot grow behind a released
    /// push-to-talk key.
    fn pump(&mut self) {
        // Ahead of the capture, because a listener with no microphone at all
        // still has a room to quieten.
        self.sweep_ducked_peers();

        let Some(capture) = self.capture.as_mut() else {
            return;
        };

        {
            let mut ring = lock(&capture.ring);
            self.raw.clear();
            self.raw.reserve(ring.len());
            self.raw.extend(ring.drain(..));
        }

        // Drained whether or not a chain exists, so it never holds stale audio.
        {
            let mut far_end = lock(&self.far_end);
            self.far_end_scratch.clear();
            self.far_end_scratch.extend(far_end.drain(..));
        }
        if let Some(cleanup) = self.cleanup.as_mut() {
            cleanup.push_far_end(&self.far_end_scratch);
        }

        if self.raw.is_empty() {
            return;
        }

        self.mono.clear();
        let channels = capture.channels;
        let scale = 1.0 / channels as f32;
        self.mono.extend(
            self.raw
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() * scale),
        );

        match capture.resampler.as_mut() {
            Some(resampler) => resampler.push(&self.mono, &mut self.queue),
            None => self.queue.extend(self.mono.iter().copied()),
        }

        let overflow = self.queue.len().saturating_sub(MONO_QUEUE_MAX);
        if overflow > 0 {
            self.queue.drain(..overflow);
        }

        self.send_frames();
    }

    fn send_frames(&mut self) {
        let now = Instant::now();

        let mut frame = [0.0f32; FRAME_SAMPLES];
        while self.queue.len() >= FRAME_SAMPLES {
            for (slot, sample) in frame.iter_mut().zip(self.queue.drain(..FRAME_SAMPLES)) {
                *slot = sample;
            }

            // Ahead of the gate, the meter and the encoder: they all work on what
            // actually leaves the machine, so voice activation triggers on speech
            // rather than on the fan.
            self.clean(&mut frame, now);

            // Advanced in both modes and while muted, so the meter stays live and
            // a mode switch never starts from a stale gate.
            let decision = self.gate.process(&frame, now);

            self.frames_since_level += 1;
            if self.frames_since_level >= LEVEL_EVERY_FRAMES {
                self.frames_since_level = 0;
                self.emit(AudioEvent::InputLevel {
                    dbfs: NoiseGate::level(&frame),
                    gate_open: self.gate.is_open(),
                });
            }

            let wants = wants_to_send(self.transmit.mode, self.ptt, decision);
            let transmitting = wants && !self.muted && !self.deafened.load(Ordering::Relaxed);
            self.set_transmitting(transmitting);

            if !transmitting {
                continue;
            }
            let (Some(encoder), Some(sender)) = (self.encoder.as_mut(), self.sender.as_ref())
            else {
                continue;
            };

            let encoded = match encoder.encode(&frame, &mut self.packet) {
                Ok(written) => written,
                Err(error) => {
                    if self.encode_warning.allow(now) {
                        tracing::warn!(%error, "dropping a frame the encoder refused");
                    }
                    continue;
                }
            };

            let idle = self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= SPURT_GAP);
            let marker = starts_spurt(self.transmit.mode, decision, idle);
            match sender.send_audio(&self.packet[..encoded], marker) {
                Ok(()) => self.last_sent = Some(now),
                Err(error) => {
                    if self.send_warning.allow(now) {
                        tracing::debug!(%error, "dropping a frame the socket refused");
                    }
                }
            }
        }
    }

    /// Runs one frame through the chain, in place. Guarded like the codec: a panic
    /// on the audio thread would take the whole process down, and the raw
    /// microphone is better than no call at all.
    fn clean(&mut self, frame: &mut [f32; FRAME_SAMPLES], now: Instant) {
        let Some(cleanup) = self.cleanup.as_mut() else {
            return;
        };

        self.raw_frame.copy_from_slice(frame);
        let outcome = catch_unwind(AssertUnwindSafe(|| cleanup.process(frame)));
        let reason = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("the chain panicked".to_string()),
        };
        if let Some(reason) = reason {
            frame.copy_from_slice(&self.raw_frame);
            self.disable_cleanup(&reason);
            return;
        }

        if self.cleanup_settings.echo_cancellation
            && self
                .metrics_at
                .is_none_or(|at| now.saturating_duration_since(at) >= METRICS_EVERY)
        {
            self.metrics_at = Some(now);
            if let Some(metrics) = self.cleanup.as_ref().and_then(InputCleanup::metrics) {
                tracing::debug!(
                    delay_ms = metrics.delay_ms,
                    erl_db = metrics.echo_return_loss_db,
                    erle_db = metrics.echo_return_loss_enhancement_db,
                    "echo canceller"
                );
            }
        }
    }

    fn disable_cleanup(&mut self, reason: &str) {
        self.cleanup = None;
        self.cleanup_failed = true;
        tracing::warn!(reason, "input cleanup stopped, sending the raw microphone");
        self.emit(AudioEvent::Failed(format!(
            "Input cleanup stopped ({reason}); the raw microphone is sent until the next join."
        )));
    }

    /// Stores who is exempt and pushes every known peer's gain again, so starting
    /// and ending a duck are the same one pass over the mixer.
    fn set_ducking(&mut self, active: bool, exempt: Vec<u32>) {
        if self.ducking.active == active && self.ducking.exempt == exempt {
            return;
        }
        self.ducking = Ducking { active, exempt };
        // The sweep below is what reaches the rest of the room, and a duck that
        // just started should not wait out an interval for it.
        self.duck_swept_at = None;

        let Some(playout) = self.playout.as_ref() else {
            return;
        };
        let mut playout = lock(playout);
        for (ssrc, tuning) in &self.peers {
            apply_tuning(&mut playout, *ssrc, *tuning, &self.ducking);
        }
    }

    /// Takes in the speakers the interface never set a volume for: they have to
    /// be quietened by a duck as well, and one can start talking at any moment.
    /// Giving them an entry at their default volume is also what lets the duck
    /// end — every peer the mixer was told about has something to restore.
    fn sweep_ducked_peers(&mut self) {
        if !self.ducking.active {
            return;
        }
        let now = Instant::now();
        if self
            .duck_swept_at
            .is_some_and(|at| now.saturating_duration_since(at) < DUCK_SWEEP)
        {
            return;
        }
        self.duck_swept_at = Some(now);

        let Some(playout) = self.playout.clone() else {
            return;
        };
        let mut playout = lock(&playout);
        for (ssrc, _) in playout.stats() {
            if self.peers.contains_key(&ssrc) {
                continue;
            }
            let tuning = PeerTuning::default();
            self.peers.insert(ssrc, tuning);
            apply_tuning(&mut playout, ssrc, tuning, &self.ducking);
        }
    }

    /// Pushes one peer's tuning into the mixer, ducked as things stand.
    fn apply_peer(&self, ssrc: u32) {
        let Some(playout) = self.playout.as_ref() else {
            return;
        };
        let tuning = self.peers.get(&ssrc).copied().unwrap_or_default();
        apply_tuning(&mut lock(playout), ssrc, tuning, &self.ducking);
    }

    /// An ssrc belongs to one voice session, so none of this outlives the session
    /// it was collected in.
    fn forget_peers(&mut self) {
        self.peers.clear();
        self.ducking = Ducking::default();
        self.duck_swept_at = None;
    }

    fn set_transmitting(&mut self, transmitting: bool) {
        if transmitting == self.transmitting {
            return;
        }
        self.transmitting = transmitting;
        self.emit(AudioEvent::Transmitting(transmitting));
    }

    fn emit(&self, event: AudioEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for audio events");
        }
    }
}

/// Whether the frame in hand is allowed out of the machine at all.
fn wants_to_send(mode: TransmitMode, ptt: bool, decision: GateDecision) -> bool {
    match mode {
        TransmitMode::PushToTalk => ptt,
        TransmitMode::VoiceActivation => decision != GateDecision::Closed,
    }
}

/// Whether this frame opens a talk spurt. Only voice activation lets the gate
/// open one: under push-to-talk the key decides, and a gate that opens
/// mid-sentence would restart the receiver's spurt for nothing.
fn starts_spurt(mode: TransmitMode, decision: GateDecision, idle: bool) -> bool {
    idle || (mode == TransmitMode::VoiceActivation && decision == GateDecision::Opened)
}

/// The mixer has one gain per peer, so the chosen volume and the ducking factor
/// reach it multiplied together; mute is the listener's alone.
fn apply_tuning(playout: &mut Playout, ssrc: u32, tuning: PeerTuning, ducking: &Ducking) {
    playout.set_gain(ssrc, ducking.gain(ssrc, tuning.volume));
    playout.set_muted(ssrc, tuning.muted);
}

/// One infinite stereo source in the rodio mixer; rodio resamples it to whatever
/// the speakers run at. Stereo because a watched screen share's audio keeps its
/// own left and right, while the voices sit in the middle.
struct VoiceSource {
    playout: Arc<Mutex<Playout>>,
    deafened: Arc<AtomicBool>,
    /// The interface motifs, mixed in past the deafen so the deafen pair is
    /// still heard.
    sfx: Arc<Mutex<SfxMixer>>,
    /// What the mixer was given, for the echo canceller on the audio thread to
    /// subtract from what the microphone hears.
    far_end: Arc<Mutex<VecDeque<f32>>>,
    /// The same, for the canceller on the share thread: two consumers on two
    /// threads, so one of them draining must never starve the other.
    share_far_end: Arc<Mutex<VecDeque<f32>>>,
    frame: [f32; STEREO_FRAME_SAMPLES],
    /// `frame` in mono, which is what both cancellers take as a reference.
    downmix: [f32; FRAME_SAMPLES],
    /// The motifs' own mono frame, before they are folded into `frame`.
    sfx_frame: [f32; FRAME_SAMPLES],
    cursor: usize,
}

impl VoiceSource {
    fn new(
        playout: Arc<Mutex<Playout>>,
        deafened: Arc<AtomicBool>,
        sfx: Arc<Mutex<SfxMixer>>,
        far_end: Arc<Mutex<VecDeque<f32>>>,
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    ) -> Self {
        Self {
            playout,
            deafened,
            sfx,
            far_end,
            share_far_end,
            frame: [0.0; STEREO_FRAME_SAMPLES],
            downmix: [0.0; FRAME_SAMPLES],
            sfx_frame: [0.0; FRAME_SAMPLES],
            // Past the end, so the first sample pulls a frame.
            cursor: STEREO_FRAME_SAMPLES,
        }
    }
}

impl Iterator for VoiceSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.cursor >= STEREO_FRAME_SAMPLES {
            lock(&self.playout).next_stereo_frame(&mut self.frame);
            self.cursor = 0;

            // Deafened silences everything the mixer holds alike: the voices, a
            // watched share and a soundpad clip.
            let deafened = self.deafened.load(Ordering::Relaxed);
            if deafened {
                self.frame.fill(0.0);
            }

            // The motifs come in past that, so the deafen pair is still heard:
            // it is the confirmation of the press itself.
            lock(&self.sfx).next_frame(&mut self.sfx_frame, deafened);
            let (pairs, _) = self.frame.as_chunks_mut::<2>();
            for (pair, motif) in pairs.iter_mut().zip(&self.sfx_frame) {
                pair[0] = (pair[0] + motif).clamp(-1.0, 1.0);
                pair[1] = (pair[1] + motif).clamp(-1.0, 1.0);
            }

            // The reference has to be what the speakers are given, motifs
            // included: the microphone picks one up, so a canceller that did not
            // have it would let everyone else hear it back.
            downmix(&self.frame, &mut self.downmix);
            push_far_end(&self.far_end, &self.downmix);
            push_far_end(&self.share_far_end, &self.downmix);
        }
        let sample = self.frame[self.cursor];
        self.cursor += 1;

        // Frames are still pulled while deafened: skipping them would let the
        // jitter buffers fill up and turn undeafening into a burst of stale audio.
        Some(sample)
    }
}

impl rodio::Source for VoiceSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        const STEREO: rodio::ChannelCount = NonZero::new(2).unwrap();
        STEREO
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        const RATE: rodio::SampleRate = NonZero::new(SAMPLE_RATE).unwrap();
        RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Folds one played stereo frame into the mono reference both echo cancellers
/// subtract.
fn downmix(frame: &[f32; STEREO_FRAME_SAMPLES], out: &mut [f32; FRAME_SAMPLES]) {
    for (mono, pair) in out.iter_mut().zip(frame.as_chunks::<2>().0) {
        *mono = (pair[0] + pair[1]) / 2.0;
    }
}

/// Appends one played frame to a far-end ring, keeping only the newest
/// [`FAR_END_MAX_SAMPLES`]: a consumer that stalls loses reference audio rather
/// than letting the ring grow.
fn push_far_end(ring: &Mutex<VecDeque<f32>>, samples: &[f32]) {
    let mut ring = lock(ring);
    let overflow = (ring.len() + samples.len()).saturating_sub(FAR_END_MAX_SAMPLES);
    let dropped = overflow.min(ring.len());
    ring.drain(..dropped);
    ring.extend(samples.iter().copied());
}

/// A sinc resampler from the microphone's rate up or down to 48 kHz, fed 20 ms of
/// input at a time and producing a variable number of samples.
struct Resample {
    inner: Async<f32>,
    /// Input frames still waiting for a full chunk.
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Resample {
    fn new(rate: u32) -> Result<Self, rubato::ResamplerConstructionError> {
        let chunk = (rate as usize / 50).max(1);
        let inner = Async::<f32>::new_sinc(
            f64::from(SAMPLE_RATE) / f64::from(rate),
            1.0,
            &SincInterpolationParameters::default(),
            chunk,
            1,
            FixedAsync::Input,
        )?;
        let output = vec![0.0; inner.output_frames_max()];
        Ok(Self {
            inner,
            pending: Vec::with_capacity(chunk * 2),
            output,
        })
    }

    fn push(&mut self, samples: &[f32], out: &mut VecDeque<f32>) {
        self.pending.extend_from_slice(samples);
        loop {
            let needed = self.inner.input_frames_next();
            if self.pending.len() < needed {
                return;
            }

            let frames = self.output.len();
            let source = match InterleavedSlice::new(&self.pending[..needed], 1, needed) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(%error, "cannot wrap the capture chunk");
                    self.pending.clear();
                    return;
                }
            };
            let mut target = match InterleavedSlice::new_mut(&mut self.output, 1, frames) {
                Ok(target) => target,
                Err(error) => {
                    tracing::warn!(%error, "cannot wrap the resampler output");
                    self.pending.clear();
                    return;
                }
            };

            let produced = match self.inner.process_into_buffer(&source, &mut target, None) {
                Ok((_, produced)) => produced,
                Err(error) => {
                    tracing::warn!(%error, "dropping a chunk the resampler refused");
                    0
                }
            };

            out.extend(self.output[..produced].iter().copied());
            self.pending.drain(..needed);
        }
    }
}

/// The last time something was logged, so a failure on every frame still only
/// costs one line a second.
#[derive(Default)]
pub struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    pub fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= WARN_INTERVAL)
        {
            self.last = Some(now);
            return true;
        }
        false
    }
}

/// A poisoned lock still holds a usable ring or playout, and losing the call over
/// it would be worse than carrying on.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pick_device<I, F>(
    wanted: Option<&str>,
    devices: Option<I>,
    default: F,
    direction: &str,
) -> Option<cpal::Device>
where
    I: Iterator<Item = cpal::Device>,
    F: FnOnce() -> Option<cpal::Device>,
{
    if let Some(wanted) = wanted {
        if let Some(device) =
            devices.and_then(|mut devices| devices.find(|device| device_name(device) == wanted))
        {
            return Some(device);
        }
        tracing::info!(
            device = wanted,
            direction,
            "device is gone, using the default"
        );
    }
    default()
}

fn device_names<I>(devices: Result<I, cpal::DevicesError>) -> Vec<String>
where
    I: Iterator<Item = cpal::Device>,
{
    match devices {
        Ok(devices) => unique_in_order(devices.map(|device| device_name(&device))),
        Err(error) => {
            tracing::warn!(%error, "cannot list the audio devices");
            Vec::new()
        }
    }
}

/// Several backends expose the same card more than once; the settings screen
/// only needs it once, in the order the host gave it.
fn unique_in_order(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for name in names {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

fn device_name(device: &cpal::Device) -> String {
    match device.description() {
        Ok(description) => description.name().to_string(),
        Err(error) => {
            tracing::debug!(%error, "the host will not name one of its devices");
            UNNAMED_DEVICE.to_string()
        }
    }
}

/// 48 kHz mono if the device offers it, else 48 kHz stereo, else whatever the
/// device calls its default and the resampler sorts it out. The samples are
/// always asked for as `f32`; cpal converts for devices that speak anything else.
fn input_config(device: &cpal::Device) -> Result<cpal::StreamConfig, String> {
    if let Ok(supported) = device.supported_input_configs() {
        let ranges: Vec<cpal::SupportedStreamConfigRange> = supported.collect();
        for channels in [1u16, 2] {
            let native = ranges
                .iter()
                .filter(|range| range.channels() == channels)
                .find_map(|range| (*range).try_with_sample_rate(SAMPLE_RATE));
            if let Some(native) = native {
                return Ok(native.config());
            }
        }
    }

    device
        .default_input_config()
        .map(|config| config.config())
        .map_err(|error| chain(&error))
}

/// `Display` on an error only prints its own line; the cause is where the
/// operating system's reason lives.
fn chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source as _;
    use vorcall_voice::jitter::Incoming;

    /// A peer the listener turned down, as a plain number the arithmetic below
    /// can be read against.
    const QUIET_PEER: f32 = 0.8;

    /// A motif is at most 150 ms, so eight 20 ms frames outlast every one of them.
    const MOTIF_FRAMES: usize = 8;

    struct Mixed {
        source: VoiceSource,
        deafened: Arc<AtomicBool>,
        playout: Arc<Mutex<Playout>>,
        sfx: Arc<Mutex<SfxMixer>>,
        far_end: Arc<Mutex<VecDeque<f32>>>,
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    }

    fn mixed() -> Mixed {
        mixed_over(
            Arc::new(Mutex::new(Playout::new())),
            Arc::new(Mutex::new(SfxMixer::new())),
        )
    }

    /// The same source over a playout and a motif mixer the caller keeps, so a
    /// test can drive it through the commands the app will send.
    fn mixed_over(playout: Arc<Mutex<Playout>>, sfx: Arc<Mutex<SfxMixer>>) -> Mixed {
        let deafened = Arc::new(AtomicBool::new(false));
        let far_end = Arc::new(Mutex::new(VecDeque::new()));
        let share_far_end = Arc::new(Mutex::new(VecDeque::new()));
        Mixed {
            source: VoiceSource::new(
                playout.clone(),
                deafened.clone(),
                sfx.clone(),
                far_end.clone(),
                share_far_end.clone(),
            ),
            deafened,
            playout,
            sfx,
            far_end,
            share_far_end,
        }
    }

    /// Starting a motif the way the audio thread does: synthesized first, then
    /// handed to the mixer under its lock.
    fn play_sfx(mixer: &Arc<Mutex<SfxMixer>>, sfx: Sfx) {
        let samples = sfx.samples();
        lock(mixer).play(samples, sfx.bypass_deafen());
    }

    /// One 20 ms frame of playout, as rodio pulls it: two samples per frame.
    fn pull(source: &mut VoiceSource, frames: usize) -> Vec<f32> {
        (0..frames * STEREO_FRAME_SAMPLES)
            .map(|_| source.next().expect("the source is infinite"))
            .collect()
    }

    fn tone(index: usize, rate: u32) -> f32 {
        (std::f32::consts::TAU * 440.0 * index as f32 / rate as f32).sin()
    }

    fn rms(samples: impl Iterator<Item = f32>) -> f32 {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for sample in samples {
            sum += f64::from(sample) * f64::from(sample);
            count += 1;
        }
        (sum / count.max(1) as f64).sqrt() as f32
    }

    #[test]
    fn the_mixer_source_is_interleaved_stereo_at_48_khz() {
        let mixed = mixed();
        assert_eq!(mixed.source.channels().get(), 2);
        assert_eq!(mixed.source.sample_rate().get(), SAMPLE_RATE);
    }

    #[test]
    fn a_played_frame_is_averaged_into_one_mono_reference() {
        let mut frame = [0.0f32; STEREO_FRAME_SAMPLES];
        for (index, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            // Left and right are deliberately different, and opposite at the
            // start: a reference that took one channel only would show it.
            *pair = if index == 0 { [1.0, -1.0] } else { [0.5, 0.25] };
        }

        let mut mono = [0.0f32; FRAME_SAMPLES];
        downmix(&frame, &mut mono);

        assert_eq!(mono[0], 0.0);
        for sample in &mono[1..] {
            assert_eq!(*sample, 0.375);
        }
    }

    #[test]
    fn every_played_frame_reaches_both_far_end_rings() {
        let mut mixed = mixed();
        pull(&mut mixed.source, 2);

        // One mono sample per stereo frame played, in each ring.
        assert_eq!(lock(&mixed.far_end).len(), 2 * FRAME_SAMPLES);
        assert_eq!(lock(&mixed.share_far_end).len(), 2 * FRAME_SAMPLES);

        // Past 200 ms both rings hold the newest reference and nothing older, so
        // neither consumer's backlog can grow without bound.
        pull(&mut mixed.source, 12);
        assert_eq!(lock(&mixed.far_end).len(), FAR_END_MAX_SAMPLES);
        assert_eq!(lock(&mixed.share_far_end).len(), FAR_END_MAX_SAMPLES);
    }

    #[test]
    fn a_deafened_listener_hears_zeros_and_the_playout_still_drains() {
        let mut mixed = mixed();
        mixed.deafened.store(true, Ordering::Relaxed);

        let played = pull(&mut mixed.source, 2);
        assert!(played.iter().all(|sample| *sample == 0.0));

        // Pulled all the same, or the jitter buffers would fill up and
        // undeafening would start with a burst of stale audio.
        assert_eq!(lock(&mixed.far_end).len(), 2 * FRAME_SAMPLES);
        assert_eq!(lock(&mixed.share_far_end).len(), 2 * FRAME_SAMPLES);
    }

    #[test]
    fn an_undeafened_listener_hears_a_motif() {
        let mut mixed = mixed();
        play_sfx(&mixed.sfx, Sfx::Join);

        let level = rms(pull(&mut mixed.source, 1).into_iter());
        assert!(level > 0.01, "the motif is silent: {level}");
    }

    #[test]
    fn a_deafened_listener_hears_the_deafen_pair_and_nothing_else() {
        // The confirmation of the press itself, so it survives the deafen.
        for sfx in [Sfx::Deafen, Sfx::Undeafen] {
            let mut mixed = mixed();
            mixed.deafened.store(true, Ordering::Relaxed);
            play_sfx(&mixed.sfx, sfx);

            let level = rms(pull(&mut mixed.source, 1).into_iter());
            assert!(level > 0.01, "{sfx:?} was silenced by the deafen");
        }

        for sfx in [Sfx::Join, Sfx::Leave, Sfx::Mute, Sfx::Unmute] {
            let mut mixed = mixed();
            mixed.deafened.store(true, Ordering::Relaxed);
            play_sfx(&mixed.sfx, sfx);

            let played = pull(&mut mixed.source, MOTIF_FRAMES);
            assert!(
                played.iter().all(|sample| *sample == 0.0),
                "{sfx:?} was heard through the deafen"
            );
        }
    }

    #[test]
    fn a_motif_played_while_deafened_still_reaches_both_far_end_rings() {
        let mut mixed = mixed();
        mixed.deafened.store(true, Ordering::Relaxed);
        play_sfx(&mixed.sfx, Sfx::Deafen);
        pull(&mut mixed.source, 1);

        // The microphone hears a motif whatever the deafen does, so a canceller
        // without it in its reference would send it back to the room.
        for ring in [&mixed.far_end, &mixed.share_far_end] {
            let level = rms(lock(ring).iter().copied());
            assert!(level > 0.01, "a far-end ring only holds {level}");
        }
    }

    #[test]
    fn a_motif_advances_while_deafened_so_undeafening_does_not_replay_it() {
        let mut mixed = mixed();
        mixed.deafened.store(true, Ordering::Relaxed);
        play_sfx(&mixed.sfx, Sfx::Join);

        // Long enough for the whole motif to have run out unheard.
        let played = pull(&mut mixed.source, MOTIF_FRAMES);
        assert!(played.iter().all(|sample| *sample == 0.0));

        mixed.deafened.store(false, Ordering::Relaxed);
        let after = pull(&mut mixed.source, MOTIF_FRAMES);
        assert!(
            after.iter().all(|sample| *sample == 0.0),
            "undeafening replayed a motif that had already run out"
        );
    }

    /// The level of a motif's first frame at one volume, set the way the app
    /// sets it.
    fn motif_level(volume: f32) -> f32 {
        let mut room = listening();
        let mut mixed = mixed_over(room.playout.clone(), room.audio.sfx.clone());
        room.audio.handle(AudioCommand::SetSfxVolume(volume));
        room.audio.handle(AudioCommand::PlaySfx(Sfx::Join));
        rms(pull(&mut mixed.source, 1).into_iter())
    }

    #[test]
    fn the_motif_volume_scales_and_clamps_at_both_ends() {
        let full = motif_level(1.0);
        assert!(full > 0.01, "the reference motif is silent: {full}");

        let half = motif_level(0.5) / full;
        assert!((half - 0.5).abs() < 0.01, "half gave {half}");

        // Clamped to 0.0..=2.0, like every other volume the mixer takes.
        assert_eq!(motif_level(-1.0), 0.0);
        let boosted = motif_level(5.0) / full;
        assert!((boosted - 2.0).abs() < 0.01, "5.0 gave {boosted}");
    }

    #[test]
    fn a_fifth_motif_drops_the_oldest_rather_than_growing_the_list() {
        let mut mixed = mixed();
        // The deafen pair is the only one a deafen lets through, and here it is
        // the oldest: keeping it would be heard below.
        for sfx in [Sfx::Deafen, Sfx::Join, Sfx::Leave, Sfx::Mute, Sfx::Unmute] {
            play_sfx(&mixed.sfx, sfx);
        }
        assert_eq!(lock(&mixed.sfx).playing.len(), MAX_SFX);

        mixed.deafened.store(true, Ordering::Relaxed);
        let played = pull(&mut mixed.source, MOTIF_FRAMES);
        assert!(
            played.iter().all(|sample| *sample == 0.0),
            "the oldest motif outlived the newest"
        );
    }

    #[test]
    fn a_deafened_listener_hears_nothing_of_a_soundpad_clip() {
        let samples = Arc::new(vec![0.5f32; STEREO_FRAME_SAMPLES]);

        let mut plain = mixed();
        lock(&plain.playout).set_clip(Arc::clone(&samples));
        let level = rms(pull(&mut plain.source, 1).into_iter());
        assert!(level > 0.01, "the reference clip is silent: {level}");

        let mut silent = mixed();
        silent.deafened.store(true, Ordering::Relaxed);
        lock(&silent.playout).set_clip(samples);
        let played = pull(&mut silent.source, 1);
        assert!(played.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_motif_on_an_already_loud_frame_stays_in_range() {
        let mut mixed = mixed();
        // Full scale out of the playout, which clamps its own mix there.
        lock(&mixed.playout).set_clip(Arc::new(vec![1.0f32; STEREO_FRAME_SAMPLES]));
        play_sfx(&mixed.sfx, Sfx::Join);

        let played = pull(&mut mixed.source, 1);
        assert!(
            played.contains(&1.0),
            "the frame never reached full scale to begin with"
        );
        assert!(
            played.iter().all(|sample| (-1.0..=1.0).contains(sample)),
            "a motif pushed the mix out of range"
        );
    }

    #[test]
    fn a_duck_is_twelve_decibels_down() {
        // The feature is specified in decibels; the code carries the factor.
        let decibels = 20.0 * f64::from(DUCK_FACTOR).log10();
        assert!(
            (decibels + 12.0).abs() < 0.1,
            "the duck factor is {decibels} dB"
        );
    }

    #[test]
    fn ducking_quietens_everyone_but_the_priority_speakers() {
        let ducking = Ducking {
            active: true,
            exempt: vec![7],
        };

        // The priority speaker keeps whatever the listener chose for them.
        assert_eq!(ducking.gain(7, 1.0), 1.0);
        assert_eq!(ducking.gain(7, QUIET_PEER), QUIET_PEER);
        // Everyone else is a quarter of it.
        assert_eq!(ducking.gain(9, 1.0), 0.25);
        assert_eq!(ducking.gain(9, QUIET_PEER), 0.2);
    }

    #[test]
    fn the_chosen_volume_comes_back_when_the_duck_ends() {
        let ducking = Ducking::default();

        assert_eq!(ducking.gain(9, 1.0), 1.0);
        assert_eq!(ducking.gain(9, QUIET_PEER), QUIET_PEER);

        // An exempt list left over from the last duck changes nothing while it is
        // inactive.
        let stale = Ducking {
            active: false,
            exempt: vec![7],
        };
        assert_eq!(stale.gain(9, QUIET_PEER), QUIET_PEER);
    }

    /// The thread as it stands before any device is opened, which is all the
    /// per-peer path needs: it only ever talks to the playout.
    struct Listening {
        audio: AudioThread,
        playout: Arc<Mutex<Playout>>,
        _events: async_mpsc::UnboundedReceiver<AudioEvent>,
    }

    fn listening() -> Listening {
        let (events, updates) = async_mpsc::unbounded();
        let playout = Arc::new(Mutex::new(Playout::new()));
        let mut audio = AudioThread::new(events, Arc::new(Mutex::new(VecDeque::new())));
        audio.playout = Some(playout.clone());
        Listening {
            audio,
            playout,
            _events: updates,
        }
    }

    /// A packet is what makes the playout know a speaker at all; one is enough.
    fn heard(playout: &Mutex<Playout>, ssrc: u32) {
        lock(playout).push(
            ssrc,
            Incoming {
                seq: 1,
                ts: 0,
                marker: true,
                payload: vec![0],
            },
        );
    }

    #[test]
    fn a_volume_chosen_while_ducking_reaches_the_mixer_ducked() {
        let mut room = listening();
        room.audio.handle(AudioCommand::SetDucking {
            active: true,
            exempt: vec![7],
        });

        room.audio.handle(AudioCommand::SetPeerVolume {
            ssrc: 9,
            volume: QUIET_PEER,
        });
        assert_eq!(lock(&room.playout).tuning(9).0, 0.2);

        // Ending the duck gives the listener back exactly what they chose, not
        // the ducked value.
        room.audio.handle(AudioCommand::SetDucking {
            active: false,
            exempt: Vec::new(),
        });
        assert_eq!(lock(&room.playout).tuning(9).0, QUIET_PEER);
    }

    #[test]
    fn a_mute_chosen_while_ducking_is_the_listeners_alone() {
        let mut room = listening();
        room.audio.handle(AudioCommand::SetDucking {
            active: true,
            exempt: Vec::new(),
        });
        room.audio.handle(AudioCommand::SetPeerMuted {
            ssrc: 9,
            muted: true,
        });

        // Ducked gain, and the mute the listener asked for either way.
        assert_eq!(lock(&room.playout).tuning(9), (DUCK_FACTOR, true));
        room.audio.handle(AudioCommand::SetDucking {
            active: false,
            exempt: Vec::new(),
        });
        assert_eq!(lock(&room.playout).tuning(9), (1.0, true));
    }

    #[test]
    fn a_duck_reaches_a_peer_the_interface_never_touched() {
        let mut room = listening();
        heard(&room.playout, 7);
        heard(&room.playout, 9);

        room.audio.handle(AudioCommand::SetDucking {
            active: true,
            exempt: vec![7],
        });
        // The sweep rides on the tick, not on the command, and runs with no
        // microphone open.
        room.audio.pump();

        assert_eq!(lock(&room.playout).tuning(7), (1.0, false));
        assert_eq!(lock(&room.playout).tuning(9), (DUCK_FACTOR, false));

        room.audio.handle(AudioCommand::SetDucking {
            active: false,
            exempt: Vec::new(),
        });
        assert_eq!(lock(&room.playout).tuning(9), (1.0, false));
    }

    #[test]
    fn a_peer_heard_for_the_first_time_mid_duck_is_quietened_too() {
        let mut room = listening();
        room.audio.handle(AudioCommand::SetDucking {
            active: true,
            exempt: Vec::new(),
        });
        room.audio.pump();

        heard(&room.playout, 9);
        // The sweep is rate-limited, so the speaker is picked up by the first
        // tick an interval later rather than the very next one.
        room.audio.duck_swept_at = room
            .audio
            .duck_swept_at
            .and_then(|at| at.checked_sub(DUCK_SWEEP));
        room.audio.pump();

        assert_eq!(lock(&room.playout).tuning(9), (DUCK_FACTOR, false));
    }

    #[test]
    fn leaving_the_channel_forgets_every_peer_and_the_duck() {
        let mut room = listening();
        heard(&room.playout, 9);
        room.audio.handle(AudioCommand::SetDucking {
            active: true,
            exempt: Vec::new(),
        });
        room.audio.pump();
        assert_eq!(room.audio.peers.len(), 1);

        room.audio.handle(AudioCommand::Close);
        assert!(room.audio.peers.is_empty());
        assert!(!room.audio.ducking.active);
    }

    #[test]
    fn the_same_card_is_offered_once_in_the_order_the_host_gave_it() {
        let names = unique_in_order(
            ["Built-in", "USB mic", "Built-in", UNNAMED_DEVICE]
                .into_iter()
                .map(String::from),
        );

        assert_eq!(names, ["Built-in", "USB mic", UNNAMED_DEVICE]);
    }

    #[test]
    fn push_to_talk_sends_on_the_key_and_voice_activation_on_the_gate() {
        use TransmitMode::{PushToTalk, VoiceActivation};

        assert!(wants_to_send(PushToTalk, true, GateDecision::Closed));
        assert!(!wants_to_send(PushToTalk, false, GateDecision::Open));
        assert!(wants_to_send(VoiceActivation, false, GateDecision::Opened));
        assert!(wants_to_send(VoiceActivation, false, GateDecision::Open));
        assert!(!wants_to_send(VoiceActivation, true, GateDecision::Closed));
    }

    #[test]
    fn only_voice_activation_lets_the_gate_start_a_spurt() {
        use TransmitMode::{PushToTalk, VoiceActivation};

        // A gap long enough to have ended the last spurt always starts one.
        assert!(starts_spurt(PushToTalk, GateDecision::Open, true));
        assert!(starts_spurt(VoiceActivation, GateDecision::Open, true));

        // Mid-spurt the gate opening marks a new one only under voice activation:
        // under push-to-talk the key decides.
        assert!(starts_spurt(VoiceActivation, GateDecision::Opened, false));
        assert!(!starts_spurt(PushToTalk, GateDecision::Opened, false));
        assert!(!starts_spurt(VoiceActivation, GateDecision::Open, false));
    }

    #[test]
    fn a_44100_microphone_is_resampled_to_48k_without_gaps() {
        // One second of tone at 44.1 kHz, handed over in 10 ms chunks.
        let mut resample = Resample::new(44_100).expect("44.1 kHz resamples");
        let mut queue: VecDeque<f32> = VecDeque::new();
        let mut frames: Vec<[f32; FRAME_SAMPLES]> = Vec::new();
        let mut frame = [0.0f32; FRAME_SAMPLES];

        for chunk in 0..100usize {
            let samples: Vec<f32> = (0..441)
                .map(|index| tone(chunk * 441 + index, 44_100))
                .collect();
            resample.push(&samples, &mut queue);
            while queue.len() >= FRAME_SAMPLES {
                for (slot, sample) in frame.iter_mut().zip(queue.drain(..FRAME_SAMPLES)) {
                    *slot = sample;
                }
                frames.push(frame);
            }
        }

        // 44 100 samples in is 48 000 out, which is 50 frames of 20 ms; the
        // resampler's own delay may hold the last one back.
        assert!(
            (49..=51).contains(&frames.len()),
            "one second became {} frames",
            frames.len()
        );

        let out: Vec<f32> = frames.iter().flatten().copied().collect();

        // A full-scale sine has an RMS of 1/sqrt(2), whatever it is resampled to.
        let level = rms(out.iter().copied());
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (level - expected).abs() / expected < 0.2,
            "RMS was {level}, expected about {expected}"
        );

        // 440 Hz at 48 kHz moves by at most 0.058 between two samples; a lost or
        // repeated stretch anywhere, frame boundaries included, would jump.
        for (index, pair) in out.windows(2).enumerate() {
            assert!(
                (pair[1] - pair[0]).abs() <= 0.3,
                "a jump of {} at sample {index}",
                pair[1] - pair[0]
            );
        }
    }

    #[test]
    fn a_throttle_lets_one_line_through_per_interval() {
        let mut throttle = Throttle::default();
        let now = Instant::now();

        assert!(throttle.allow(now));
        assert!(!throttle.allow(now));
        assert!(!throttle.allow(now + WARN_INTERVAL - Duration::from_millis(1)));
        assert!(throttle.allow(now + WARN_INTERVAL));
    }

    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let mutex = Arc::new(Mutex::new(1u8));
        let poisoner = mutex.clone();
        let _ = std::thread::spawn(move || {
            let _guard = lock(&poisoner);
            panic!("poison it");
        })
        .join();

        assert_eq!(*lock(&mutex), 1);
    }
}
