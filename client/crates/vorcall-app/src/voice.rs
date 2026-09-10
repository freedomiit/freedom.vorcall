//! The audio devices behind a voice room, on a thread of their own.
//!
//! Opening a device blocks for as long as the host wants, and a cpal stream is
//! `!Send` on some hosts, so both streams are created, owned and dropped on the
//! thread [`spawn_audio_thread`] starts. The UI only ever pushes an
//! [`AudioCommand`] into a channel and reads [`AudioEvent`]s back.
//!
//! Capture runs one way: the cpal callback appends interleaved samples to a
//! ring, the thread downmixes them to mono, resamples to 48 kHz when the device
//! runs at another rate, cuts 20 ms frames, and hands the Opus packets to the
//! media engine while push-to-talk is held. Playback runs the other way: a
//! single infinite [`VoiceSource`] sits in the rodio mixer and pulls mixed
//! frames from [`Playout`], which the engine's receive task keeps fed.

use std::collections::VecDeque;
use std::error::Error;
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::channel::mpsc as async_mpsc;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters};
use vorcall_voice::codec::Encoder;
use vorcall_voice::{FRAME_SAMPLES, FrameSender, Playout, SAMPLE_RATE};

/// How often the thread wakes up to cut frames when no command arrives. Well
/// under the 20 ms a frame lasts, so the capture ring never runs long.
const TICK: Duration = Duration::from_millis(5);

/// Neither the capture ring nor the mono queue may hold more than this: a
/// stalled thread is allowed to lose audio, never to build up delay.
const MAX_QUEUED_MS: usize = 200;

/// A silence at least this long ends a talk spurt, so the next frame sent is
/// marked as the start of a new one.
const SPURT_GAP: Duration = Duration::from_millis(200);

/// Opus at 48 kbit/s over 20 ms frames never comes near this.
const MAX_PACKET: usize = 512;

/// A device that fails does so on every frame; the log must not become the
/// problem.
const WARN_INTERVAL: Duration = Duration::from_secs(1);

/// 200 ms of 48 kHz mono.
const MONO_QUEUE_MAX: usize = SAMPLE_RATE as usize * MAX_QUEUED_MS / 1000;

/// Shown for a device the host refuses to name; it is still usable.
const UNNAMED_DEVICE: &str = "Unknown device";

pub struct DeviceLists {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// The devices the settings screen offers. Enumerating them talks to the audio
/// host and blocks, so the caller runs this off the UI thread.
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

pub enum AudioCommand {
    Open {
        settings: AudioSettings,
        sender: FrameSender,
        playout: Arc<Mutex<Playout>>,
    },
    /// Reopens both streams, unless nothing changed.
    SetDevices(AudioSettings),
    SetPtt(bool),
    SetMuted(bool),
    SetDeafened(bool),
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
}

#[derive(Clone)]
pub struct AudioHandle {
    commands: Sender<AudioCommand>,
}

impl AudioHandle {
    /// Never blocks: the command channel is unbounded, and a thread that has
    /// already exited only costs a log line.
    pub fn send(&self, command: AudioCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("the audio thread is gone, dropping the command");
        }
    }
}

/// Spawns the dedicated audio thread. The thread exits once every
/// [`AudioHandle`] has been dropped.
pub fn spawn_audio_thread() -> (AudioHandle, async_mpsc::UnboundedReceiver<AudioEvent>) {
    let (commands, requests) = std::sync::mpsc::channel();
    let (events, updates) = async_mpsc::unbounded();

    let spawned = std::thread::Builder::new()
        .name("vorcall-audio".to_string())
        .spawn(move || run(requests, events));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the audio thread");
    }

    (AudioHandle { commands }, updates)
}

fn run(requests: Receiver<AudioCommand>, events: async_mpsc::UnboundedSender<AudioEvent>) {
    let mut state = AudioThread::new(events);
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

    ptt: bool,
    muted: bool,
    /// Shared with the [`VoiceSource`] in the mixer, which keeps pulling frames
    /// while deafened so the jitter buffers do not back up.
    deafened: Arc<AtomicBool>,

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

/// The microphone stream and everything that turns its samples into 48 kHz
/// mono.
struct Capture {
    /// Only held so the stream keeps running; it stops the moment it is
    /// dropped, which must happen on this thread.
    _stream: cpal::Stream,
    ring: Arc<Mutex<VecDeque<f32>>>,
    channels: usize,
    /// `None` when the device already runs at 48 kHz.
    resampler: Option<Resample>,
}

impl AudioThread {
    fn new(events: async_mpsc::UnboundedSender<AudioEvent>) -> Self {
        Self {
            events,
            settings: AudioSettings::default(),
            sender: None,
            playout: None,
            output: None,
            capture: None,
            encoder: None,
            ptt: false,
            muted: false,
            deafened: Arc::new(AtomicBool::new(false)),
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
            AudioCommand::SetPtt(held) => self.ptt = held,
            AudioCommand::SetMuted(muted) => self.muted = muted,
            AudioCommand::SetDeafened(deafened) => self.deafened.store(deafened, Ordering::Relaxed),
            AudioCommand::Close => {
                self.close_streams();
                self.sender = None;
                self.playout = None;
                self.emit(AudioEvent::Closed);
            }
        }
    }

    /// Playback comes first: a room is still worth joining with a broken
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
        sink.mixer()
            .add(VoiceSource::new(playout, self.deafened.clone()));

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
        Ok(name)
    }

    fn close_streams(&mut self) {
        self.capture = None;
        self.output = None;
        self.encoder = None;
        self.queue.clear();
        self.last_sent = None;
    }

    /// Moves everything the microphone captured since the last tick through the
    /// pipeline. Runs even while muted, so the queue cannot grow behind a
    /// released push-to-talk key.
    fn pump(&mut self) {
        let Some(capture) = self.capture.as_mut() else {
            return;
        };

        {
            let mut ring = lock(&capture.ring);
            self.raw.clear();
            self.raw.reserve(ring.len());
            self.raw.extend(ring.drain(..));
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
        let transmitting = self.ptt && !self.muted && !self.deafened.load(Ordering::Relaxed);

        let mut frame = [0.0f32; FRAME_SAMPLES];
        while self.queue.len() >= FRAME_SAMPLES {
            for (slot, sample) in frame.iter_mut().zip(self.queue.drain(..FRAME_SAMPLES)) {
                *slot = sample;
            }
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

            let marker = self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= SPURT_GAP);
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

    fn emit(&self, event: AudioEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for audio events");
        }
    }
}

/// One infinite mono source in the rodio mixer; rodio resamples it to whatever
/// the speakers run at.
struct VoiceSource {
    playout: Arc<Mutex<Playout>>,
    deafened: Arc<AtomicBool>,
    frame: [f32; FRAME_SAMPLES],
    cursor: usize,
}

impl VoiceSource {
    fn new(playout: Arc<Mutex<Playout>>, deafened: Arc<AtomicBool>) -> Self {
        Self {
            playout,
            deafened,
            frame: [0.0; FRAME_SAMPLES],
            // Past the end, so the first sample pulls a frame.
            cursor: FRAME_SAMPLES,
        }
    }
}

impl Iterator for VoiceSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.cursor >= FRAME_SAMPLES {
            lock(&self.playout).next_frame(&mut self.frame);
            self.cursor = 0;
        }
        let sample = self.frame[self.cursor];
        self.cursor += 1;

        // Frames are still pulled while deafened: skipping them would let the
        // jitter buffers fill up and turn undeafening into a burst of stale
        // audio.
        Some(if self.deafened.load(Ordering::Relaxed) {
            0.0
        } else {
            sample
        })
    }
}

impl rodio::Source for VoiceSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        const MONO: rodio::ChannelCount = NonZero::new(1).unwrap();
        MONO
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        const RATE: rodio::SampleRate = NonZero::new(SAMPLE_RATE).unwrap();
        RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// A sinc resampler from the microphone's rate up or down to 48 kHz, fed 20 ms
/// of input at a time and producing a variable number of samples.
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
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn allow(&mut self, now: Instant) -> bool {
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

/// A poisoned lock still holds a usable ring or playout, and losing the call
/// over it would be worse than carrying on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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
    let devices = match devices {
        Ok(devices) => devices,
        Err(error) => {
            tracing::warn!(%error, "cannot list the audio devices");
            return Vec::new();
        }
    };

    let mut names: Vec<String> = Vec::new();
    for device in devices {
        // Several backends expose the same card more than once; the settings
        // screen only needs it once, in the order the host gave it.
        if let Ok(description) = device.description()
            && !names.iter().any(|name| name == description.name())
        {
            names.push(description.name().to_string());
        }
    }
    names
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
/// always asked for as `f32`; cpal converts for devices that speak anything
/// else.
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
