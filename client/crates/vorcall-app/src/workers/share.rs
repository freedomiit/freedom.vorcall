//! The two worker threads a screen share needs, both away from the interface.
//!
//! The sharer's pipeline is one thread: the capture backend hands it frames
//! and, where the platform can, this machine's own playout. It keeps nothing
//! but the newest frame, scales it into the preset's box, encodes it on its own
//! deadline rather than the capture's, and puts the access unit on the wire.
//! The share's audio rides along one stream over: converted to 48 kHz
//! interleaved stereo, cancelled against what Vorcall itself played (a loopback
//! capture carries the room's own voices straight back out otherwise) and sent
//! as 20 ms Opus frames.
//!
//! Opening the capture is a thread of its own, and a short-lived one: the
//! portal's picker keeps it for as long as the user takes to answer it, and a
//! `Stop` has to reach the pipeline thread while it does.
//!
//! The viewer's decode thread is the mirror of it: reassembled access units in,
//! pictures out, skipping to the next keyframe rather than falling behind.
//!
//! Both own things the interface must never touch — [`Capturer::start`] blocks
//! on the portal dialog, [`FrameSender::send_video`] paces itself in
//! milliseconds, and [`ShareCleanup`] is `!Send` — so the app only pushes a
//! command in and reads events back, exactly as it does for the audio thread.

use std::collections::VecDeque;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc as async_mpsc;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters};
use tokio::sync::mpsc::UnboundedReceiver;
use vorcall_screen::codec::{EncoderSettings, Picture, VideoDecoder, VideoEncoder};
use vorcall_screen::preset::Preset;
use vorcall_screen::scale::scale_bgra;
use vorcall_screen::{
    AudioChunk, AudioMode, CaptureEvent, CaptureRequest, Capturer, Unavailable, VideoFrame,
};
use vorcall_voice::cleanup::FAR_END_MAX_SAMPLES;
use vorcall_voice::{
    AccessUnit, FrameSender, SAMPLE_RATE, STEREO_FRAME_SAMPLES, ShareCleanup, StereoEncoder,
};

use crate::workers::voice::{Throttle, lock};

/// How often the share thread wakes when no command arrives. Well under one
/// frame even at 60 fps, so the encode deadline is never missed by much.
const TICK: Duration = Duration::from_millis(5);

/// The decode thread paces nothing, so it only wakes to notice a dropped handle
/// and to report.
const DECODE_TICK: Duration = Duration::from_millis(100);

/// A gap at least this long ends a talk spurt, so the next frame sent is marked
/// as the start of a new one.
const SPURT_GAP: Duration = Duration::from_millis(200);

const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Opus at 96 kbit/s over 20 ms stereo frames never comes near this.
const MAX_PACKET: usize = 1024;

/// However many cores the machine has, never more slice threads than this: the
/// rest of the client needs the processor too.
const MAX_ENCODER_THREADS: usize = 8;

/// Interleaved stereo, everywhere below.
const CHANNELS: usize = 2;

/// 200 ms of 48 kHz interleaved stereo. A stalled thread is allowed to lose
/// share audio, never to build up delay.
const AUDIO_QUEUE_MAX: usize = SAMPLE_RATE as usize * CHANNELS / 5;

/// Access units allowed to pile up in front of the decoder before it gives up
/// on them and restarts at a keyframe.
const MAX_BACKLOG: usize = 8;

/// How long the decode thread waits before trying to build a decoder again.
/// Whatever the platform was short of may well be there by the next watch.
const DECODER_RETRY: Duration = Duration::from_secs(1);

pub enum ShareCommand {
    Start {
        request: CaptureRequest,
        preset: Preset,
        sender: FrameSender,
        /// What the mixer played, in mono, for the loopback canceller; the audio
        /// thread owns it ([`crate::workers::voice::AudioHandle::share_far_end`]).
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    },
    /// Set while the server reports no watchers: capture carries on, encoding
    /// and sending stop, and resuming forces a keyframe. A share starts paused,
    /// so the first watcher is what opens the stream.
    SetPaused(bool),
    ForceKeyframe,
    /// Drops the capture and everything built on it. The thread stays alive for
    /// a later [`ShareCommand::Start`].
    Stop,
}

#[derive(Debug, Clone)]
pub struct ShareStats {
    pub capture_fps: f32,
    pub encode_fps: f32,
    pub kbps: u32,
    pub output: (u32, u32),
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub dropped_frames: u64,
    pub skipped_frames: u64,
    pub audio_frames: u64,
    /// Datagrams the socket gave up on over the last window, retries included:
    /// a share that keeps losing fragments is one the watchers see freeze.
    pub send_failures: u64,
}

#[derive(Debug, Clone)]
pub enum ShareEvent {
    /// What the backend actually gave, which is not always what was asked for,
    /// and the size it is encoded at.
    Started {
        width: u32,
        height: u32,
        output: (u32, u32),
        audio: Option<AudioMode>,
        backend: &'static str,
    },
    /// Once a second while a share runs.
    Stats(ShareStats),
    /// The share could not be started, or could not carry on; everything is
    /// torn down and the thread stays alive for a later start.
    Failed(String),
    /// The operating system ended the capture — the user revoked it, the window
    /// closed, the stream broke.
    Ended(String),
}

#[derive(Clone)]
pub struct ShareHandle {
    commands: Sender<ShareCommand>,
}

impl ShareHandle {
    /// Never blocks: the command channel is unbounded, and a thread that has
    /// already exited only costs a log line.
    pub fn send(&self, command: ShareCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("the share thread is gone, dropping the command");
        }
    }
}

/// Spawns the sharer's pipeline thread. The thread exits once every
/// [`ShareHandle`] has been dropped.
pub fn spawn_share_thread() -> (ShareHandle, async_mpsc::UnboundedReceiver<ShareEvent>) {
    let (commands, requests) = std::sync::mpsc::channel();
    let (events, updates) = async_mpsc::unbounded();

    let spawned = std::thread::Builder::new()
        .name("vorcall-share".to_string())
        .spawn(move || run(requests, events));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the share thread");
    }

    (ShareHandle { commands }, updates)
}

fn run(requests: Receiver<ShareCommand>, events: async_mpsc::UnboundedSender<ShareEvent>) {
    let mut state = ShareThread {
        events,
        pipeline: None,
        starting: None,
    };
    loop {
        match requests.recv_timeout(TICK) {
            Ok(command) => state.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        state.collect();
        state.pump();
    }
}

struct ShareThread {
    events: async_mpsc::UnboundedSender<ShareEvent>,
    pipeline: Option<Pipeline>,
    /// The capture being opened on the opener thread, while it is.
    starting: Option<Starting>,
}

/// A [`ShareCommand::Start`] whose capture is still being opened, and
/// everything its pipeline needs once it is.
struct Starting {
    /// The capture, or why there is none. Dropping this receiver is what cancels
    /// a start: the opener's send then fails, and the capturer it was handing
    /// over is dropped there, which stops the capture.
    opened: Receiver<Result<Capturer, Unavailable>>,
    frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
    preset: Preset,
    sender: FrameSender,
    share_far_end: Arc<Mutex<VecDeque<f32>>>,
    /// A pause that arrived while the capture was still opening. The app sends
    /// the share's first one right behind the start, and a pipeline that never
    /// hears it encodes nothing for the watchers it already has.
    paused: Option<bool>,
}

impl ShareThread {
    fn handle(&mut self, command: ShareCommand) {
        match command {
            ShareCommand::Start {
                request,
                preset,
                sender,
                share_far_end,
            } => {
                // One capture at a time, and the old backend has to be stopped
                // before another picker dialog opens.
                self.pipeline = None;
                self.starting = None;

                let (frames, events) = async_mpsc::unbounded();
                let (handles, opened) = std::sync::mpsc::channel();
                // `Capturer::start` blocks on the portal's own dialog for as
                // long as the user leaves it up, and this loop has to stay able
                // to read a `Stop` while it does.
                let spawned = std::thread::Builder::new()
                    .name("vorcall-opener".to_string())
                    .spawn(move || {
                        let _ = handles.send(Capturer::start(request, frames));
                    });
                match spawned {
                    Ok(_) => {
                        self.starting = Some(Starting {
                            opened,
                            frames: events,
                            preset,
                            sender,
                            share_far_end,
                            paused: None,
                        });
                    }
                    Err(error) => {
                        let failed = format!("cannot start the capture: {error}");
                        self.emit(ShareEvent::Failed(failed));
                    }
                }
            }
            ShareCommand::SetPaused(paused) => {
                if let Some(pipeline) = self.pipeline.as_mut() {
                    pipeline.set_paused(paused);
                } else if let Some(starting) = self.starting.as_mut() {
                    starting.paused = Some(paused);
                }
            }
            ShareCommand::ForceKeyframe => {
                // A capture still opening needs nothing: the first unit its
                // pipeline encodes is a keyframe anyway.
                if let Some(pipeline) = self.pipeline.as_mut() {
                    pipeline.keyframe_pending = true;
                }
            }
            ShareCommand::Stop => {
                self.pipeline = None;
                self.starting = None;
            }
        }
    }

    /// Takes over a capture the opener has finished with. A start this loop has
    /// cancelled since is not here to take it any more, and the capturer is
    /// dropped on the opener thread instead.
    fn collect(&mut self) {
        let Some(starting) = self.starting.take() else {
            return;
        };
        let opened = starting.opened.try_recv();
        match opened {
            Ok(Ok(capturer)) => self.start_pipeline(capturer, starting),
            Ok(Err(error)) => self.emit(ShareEvent::Failed(error.to_string())),
            Err(TryRecvError::Empty) => self.starting = Some(starting),
            Err(TryRecvError::Disconnected) => {
                // Nothing but a panic inside the backend ends that thread
                // without an answer.
                let lost = "the capture could not be started".to_string();
                self.emit(ShareEvent::Failed(lost));
            }
        }
    }

    /// Builds the pipeline on a capture that has just opened, carrying over what
    /// the app asked for while it was opening.
    fn start_pipeline(&mut self, capturer: Capturer, starting: Starting) {
        let paused = starting.paused;
        let mut pipeline = Pipeline::new(
            self.events.clone(),
            capturer,
            starting.frames,
            starting.preset,
            starting.sender,
            starting.share_far_end,
        );
        if let Some(paused) = paused {
            pipeline.set_paused(paused);
        }
        self.pipeline = Some(pipeline);
    }

    fn pump(&mut self) {
        let Some(pipeline) = self.pipeline.as_mut() else {
            return;
        };
        if !pipeline.pump(Instant::now()) {
            self.pipeline = None;
        }
    }

    fn emit(&self, event: ShareEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for share events");
        }
    }
}

/// Everything one running share owns. Built on `Start`, dropped whole on `Stop`
/// or when the capture ends; `!Send`, because [`ShareCleanup`] is.
struct Pipeline {
    events: async_mpsc::UnboundedSender<ShareEvent>,
    /// Only held so the capture keeps running: it stops the moment it is
    /// dropped, which must happen on this thread.
    capturer: Capturer,
    frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
    preset: Preset,
    sender: FrameSender,
    share_far_end: Arc<Mutex<VecDeque<f32>>>,

    /// `None` until the backend has said what it captures.
    encoder: Option<VideoEncoder>,
    /// The capture size the encoder was built for; a frame of any other size
    /// rebuilds it.
    source: Option<(u32, u32)>,
    output: (u32, u32),
    /// The newest captured frame, replaced rather than queued.
    latest: Option<VideoFrame>,
    /// Whether `latest` is a frame the encoder has not seen.
    fresh: bool,
    frame_id: u32,
    keyframe_pending: bool,
    next_encode: Instant,
    /// No watchers, so nothing is encoded and no audio is sent. True until the
    /// server has accepted the share and named one: a frame put on the wire
    /// before that is a datagram the relay drops as not sharing.
    paused: bool,

    audio: Option<ShareAudio>,

    unit: Vec<u8>,
    scaled: Vec<u8>,
    far_end_scratch: Vec<f32>,

    counters: Counters,
    /// The counters as of the last [`ShareEvent::Stats`], for the two rates.
    window: Counters,
    /// The engine's own send-failure total as of that same report; it counts
    /// the whole session, so only the difference belongs to this share.
    last_send_failures: u64,
    stats_at: Instant,
    warning: Throttle,
}

#[derive(Clone, Copy, Default)]
struct Counters {
    captured: u64,
    encoded: u64,
    /// Everything handed to the socket, video and share audio alike.
    bytes: u64,
    keyframes: u64,
    keyframe_requests: u64,
    dropped: u64,
    skipped: u64,
    audio_frames: u64,
}

impl Pipeline {
    fn new(
        events: async_mpsc::UnboundedSender<ShareEvent>,
        capturer: Capturer,
        frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
        preset: Preset,
        sender: FrameSender,
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    ) -> Self {
        let now = Instant::now();
        // Read before the sender is handed over, so a session that already had
        // failures does not report them all as this share's first second.
        let last_send_failures = sender.send_failures();
        Self {
            events,
            capturer,
            frames,
            preset,
            sender,
            share_far_end,
            encoder: None,
            source: None,
            output: (0, 0),
            latest: None,
            fresh: false,
            frame_id: 0,
            keyframe_pending: true,
            next_encode: now,
            paused: true,
            audio: None,
            unit: Vec::new(),
            scaled: Vec::new(),
            far_end_scratch: Vec::with_capacity(FAR_END_MAX_SAMPLES),
            counters: Counters::default(),
            window: Counters::default(),
            last_send_failures,
            stats_at: now,
            warning: Throttle::default(),
        }
    }

    /// One pass: everything the backend produced since the last one, then the
    /// encode deadline, the share audio and the report. `false` once the share
    /// is over, which is when it has emitted its own last event.
    fn pump(&mut self, now: Instant) -> bool {
        loop {
            match self.frames.try_recv() {
                Ok(CaptureEvent::Started {
                    width,
                    height,
                    audio,
                }) => {
                    if !self.on_started(width, height, audio) {
                        return false;
                    }
                }
                Ok(CaptureEvent::Video(frame)) => {
                    if !self.on_video(frame) {
                        return false;
                    }
                }
                Ok(CaptureEvent::Audio(chunk)) => {
                    if let Some(audio) = self.audio.as_mut()
                        && !self.paused
                    {
                        audio.framer.push(&chunk);
                    }
                }
                Ok(CaptureEvent::Ended(reason)) => {
                    self.emit(ShareEvent::Ended(reason));
                    return false;
                }
                Err(async_mpsc::TryRecvError::Empty) => break,
                // The backend dropped its sender without a word, which is the
                // same thing as ending.
                Err(async_mpsc::TryRecvError::Closed) => {
                    self.emit(ShareEvent::Ended("the capture stopped".to_string()));
                    return false;
                }
            }
        }

        self.encode_tick(now);
        self.audio_tick(now);
        self.stats_tick(now);
        true
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            // Nobody is watching, so the audio already captured is dropped
            // rather than played back late when someone arrives.
            if let Some(audio) = self.audio.as_mut() {
                audio.framer.clear();
            }
        } else {
            // Whoever just started watching can only begin at a keyframe.
            self.keyframe_pending = true;
            // What played while nobody watched is no reference for the audio
            // captured from here on; left in, the canceller would stay a
            // ring's length behind for the rest of the share.
            lock(&self.share_far_end).clear();
        }
    }

    fn on_started(&mut self, width: u32, height: u32, audio: Option<AudioMode>) -> bool {
        if !self.build_encoder((width, height)) {
            return false;
        }
        self.frame_id = 0;
        self.audio = audio.and_then(|mode| ShareAudio::new(mode, &self.share_far_end));

        self.emit(ShareEvent::Started {
            width,
            height,
            output: self.output,
            // What is really sent: a share whose encoder would not build sends
            // no audio at all.
            audio: self.audio.as_ref().map(|audio| audio.mode),
            backend: self.capturer.backend(),
        });
        true
    }

    fn on_video(&mut self, frame: VideoFrame) -> bool {
        if self.source != Some((frame.width, frame.height))
            && !self.build_encoder((frame.width, frame.height))
        {
            return false;
        }

        self.counters.captured += 1;
        // Paused, nothing is meant to be encoded, so replacing a frame is not
        // a frame lost.
        if self.fresh && !self.paused {
            self.counters.dropped += 1;
        }
        self.latest = Some(frame);
        self.fresh = true;
        true
    }

    /// Builds the encoder for a capture of `source`, at the size and bitrate the
    /// preset asks for. `false` once it has emitted [`ShareEvent::Failed`]: a
    /// share with no encoder has nothing to send.
    fn build_encoder(&mut self, source: (u32, u32)) -> bool {
        let output = self.preset.output_size(source);
        let threads = std::thread::available_parallelism()
            .map_or(1, |cores| cores.get() / 2)
            .clamp(1, MAX_ENCODER_THREADS) as u16;
        let settings = EncoderSettings {
            width: output.0,
            height: output.1,
            fps: self.preset.fps.hz(),
            bitrate_kbps: self.preset.bitrate_kbps(source),
            threads,
        };

        match VideoEncoder::new(settings) {
            Ok(encoder) => {
                tracing::info!(
                    source = ?source,
                    output = ?output,
                    bitrate_kbps = settings.bitrate_kbps,
                    threads = encoder.threads(),
                    "encoding a screen share"
                );
                self.encoder = Some(encoder);
                self.source = Some(source);
                self.output = output;
                // Nothing a viewer holds decodes against the new stream.
                self.keyframe_pending = true;
                self.scaled.clear();
                true
            }
            Err(error) => {
                self.emit(ShareEvent::Failed(format!(
                    "Cannot encode this screen: {error}"
                )));
                false
            }
        }
    }

    /// Encodes at most one frame, on the preset's cadence rather than the
    /// capture's.
    fn encode_tick(&mut self, now: Instant) {
        if now < self.next_encode {
            return;
        }
        let interval = self.preset.fps.interval();
        // Missed ticks are not made up for: this is live video, and a burst of
        // late frames only pushes the next ones later still.
        self.next_encode = if now.saturating_duration_since(self.next_encode) >= interval {
            now + interval
        } else {
            self.next_encode + interval
        };

        if self.paused {
            return;
        }
        if self.sender.take_keyframe_request() {
            self.counters.keyframe_requests += 1;
            self.keyframe_pending = true;
        }
        if !self.fresh && !self.keyframe_pending {
            return;
        }

        let Some(frame) = self.latest.as_ref() else {
            return;
        };
        let Some(encoder) = self.encoder.as_mut() else {
            return;
        };

        let (pixels, stride) = if (frame.width, frame.height) == self.output {
            (frame.bgra.as_slice(), frame.stride)
        } else {
            scale_bgra(
                &frame.bgra,
                frame.stride,
                (frame.width, frame.height),
                self.output,
                &mut self.scaled,
            );
            (self.scaled.as_slice(), self.output.0 as usize * 4)
        };

        let force = self.keyframe_pending;
        let encoded = encoder.encode(pixels, stride, force, &mut self.unit);
        self.fresh = false;

        let encoded = match encoded {
            Ok(encoded) => encoded,
            Err(error) => {
                if self.warning.allow(now) {
                    tracing::warn!(%error, "dropping a frame the encoder refused");
                }
                self.counters.dropped += 1;
                return;
            }
        };
        if encoded.skipped {
            // Rate control coded nothing, so a forced keyframe was not served
            // either and stays pending.
            self.counters.skipped += 1;
            return;
        }

        self.keyframe_pending = false;
        self.counters.encoded += 1;
        self.counters.bytes += self.unit.len() as u64;
        if encoded.keyframe {
            self.counters.keyframes += 1;
        }

        let frame_id = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);
        if let Err(error) = self
            .sender
            .send_video(frame_id, encoded.keyframe, &self.unit)
            && self.warning.allow(now)
        {
            tracing::debug!(%error, "dropping a frame the socket refused");
        }
    }

    fn audio_tick(&mut self, now: Instant) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if self.paused {
            return;
        }

        // The reference the canceller subtracts: everything the mixer played
        // since the last tick, drained in one go.
        if audio.cleanup.is_some() {
            {
                let mut ring = lock(&self.share_far_end);
                self.far_end_scratch.clear();
                self.far_end_scratch.extend(ring.drain(..));
            }
            audio.push_far_end(&self.far_end_scratch);
        }

        audio.pump(&self.sender, now, &mut self.counters);
    }

    fn stats_tick(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.stats_at);
        if elapsed < STATS_INTERVAL {
            return;
        }
        self.stats_at = now;
        let seconds = elapsed.as_secs_f64();
        let counters = self.counters;
        let rate = |current: u64, previous: u64| {
            (current.saturating_sub(previous) as f64 / seconds) as f32
        };

        let send_failures = self.sender.send_failures();
        let refused = send_failures.saturating_sub(self.last_send_failures);
        self.last_send_failures = send_failures;
        if refused > 0 && self.warning.allow(now) {
            tracing::warn!(
                refused,
                "share datagrams refused by the socket in the last second"
            );
        }

        let stats = ShareStats {
            capture_fps: rate(counters.captured, self.window.captured),
            encode_fps: rate(counters.encoded, self.window.encoded),
            kbps: (counters.bytes.saturating_sub(self.window.bytes) as f64 * 8.0
                / 1_000.0
                / seconds) as u32,
            output: self.output,
            keyframes: counters.keyframes,
            keyframe_requests: counters.keyframe_requests,
            dropped_frames: counters.dropped,
            skipped_frames: counters.skipped,
            audio_frames: counters.audio_frames,
            send_failures: refused,
        };
        self.window = counters;
        self.emit(ShareEvent::Stats(stats));
    }

    fn emit(&self, event: ShareEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for share events");
        }
    }
}

/// The share's own audio: whatever the backend captured, turned into the 20 ms
/// stereo Opus frames the wire takes.
struct ShareAudio {
    mode: AudioMode,
    encoder: StereoEncoder,
    framer: AudioFramer,
    /// `None` when the capture excludes this machine's playout, and after the
    /// chain has failed: there is nothing to subtract, or nothing left to
    /// subtract it with.
    cleanup: Option<ShareCleanup>,
    frame: [f32; STEREO_FRAME_SAMPLES],
    /// The frame as captured, restored if the chain fails mid-frame.
    raw: [f32; STEREO_FRAME_SAMPLES],
    packet: [u8; MAX_PACKET],
    last_sent: Option<Instant>,
    warning: Throttle,
}

impl ShareAudio {
    /// `None` when the encoder will not build, which is a share without audio
    /// rather than no share.
    fn new(mode: AudioMode, far_end: &Mutex<VecDeque<f32>>) -> Option<ShareAudio> {
        let encoder = match StereoEncoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::warn!(%error, "no share audio encoder, sharing the picture only");
                return None;
            }
        };

        // Only a capture that carries this machine's playout has Vorcall's own
        // voices in it.
        let cleanup = match mode {
            AudioMode::Excluded => None,
            AudioMode::IncludesOwnPlayout => match ShareCleanup::new() {
                Ok(cleanup) => {
                    // Whatever the mixer played before this capture existed is
                    // no reference for it.
                    lock(far_end).clear();
                    Some(cleanup)
                }
                Err(error) => {
                    tracing::warn!(%error, "no share echo canceller, sending the capture as it came");
                    None
                }
            },
        };

        Some(ShareAudio {
            mode,
            encoder,
            framer: AudioFramer::new(),
            cleanup,
            frame: [0.0; STEREO_FRAME_SAMPLES],
            raw: [0.0; STEREO_FRAME_SAMPLES],
            packet: [0; MAX_PACKET],
            last_sent: None,
            warning: Throttle::default(),
        })
    }

    fn push_far_end(&mut self, samples: &[f32]) {
        if let Some(cleanup) = self.cleanup.as_mut() {
            cleanup.push_far_end(samples);
        }
    }

    /// Sends every whole frame the framer has ready.
    fn pump(&mut self, sender: &FrameSender, now: Instant, counters: &mut Counters) {
        while self.framer.next_frame(&mut self.frame) {
            self.clean();

            let encoded = match self.encoder.encode(&self.frame, &mut self.packet) {
                Ok(written) => written,
                Err(error) => {
                    if self.warning.allow(now) {
                        tracing::warn!(%error, "dropping share audio the encoder refused");
                    }
                    continue;
                }
            };

            let marker = self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= SPURT_GAP);
            match sender.send_share_audio(&self.packet[..encoded], marker) {
                Ok(()) => {
                    self.last_sent = Some(now);
                    counters.audio_frames += 1;
                    counters.bytes += encoded as u64;
                }
                Err(error) => {
                    if self.warning.allow(now) {
                        tracing::debug!(%error, "dropping share audio the socket refused");
                    }
                }
            }
        }
    }

    /// Runs one frame through the canceller, in place. Guarded like the
    /// microphone's chain: a panic here would take the whole process down, and
    /// a share that echoes beats no share at all.
    fn clean(&mut self) {
        if self.cleanup.is_none() {
            return;
        }
        self.raw.copy_from_slice(&self.frame);

        let frame = &mut self.frame;
        let outcome = self
            .cleanup
            .as_mut()
            .map(|cleanup| catch_unwind(AssertUnwindSafe(|| cleanup.process(frame))));
        let reason = match outcome {
            Some(Ok(Ok(()))) | None => None,
            Some(Ok(Err(error))) => Some(error.to_string()),
            Some(Err(_)) => Some("the chain panicked".to_string()),
        };

        if let Some(reason) = reason {
            self.frame.copy_from_slice(&self.raw);
            self.cleanup = None;
            tracing::warn!(
                reason,
                "share echo cancellation stopped, sending the capture as it came"
            );
        }
    }
}

/// Whatever the capture backend hands over, cut into the 20 ms interleaved
/// stereo frames at 48 kHz the codec takes.
struct AudioFramer {
    /// The rate `conversion` was built for.
    rate: u32,
    conversion: Conversion,
    /// One chunk as interleaved stereo, still at the source's rate.
    stereo: Vec<f32>,
    /// 48 kHz interleaved stereo, waiting to be cut into frames.
    queue: VecDeque<f32>,
}

/// What the framer does with the source's rate.
enum Conversion {
    Native,
    /// Boxed: a resampler is far larger than the other two answers, and this
    /// enum is a field of every share's audio.
    Resampling(Box<Resample>),
    /// A rate the resampler refused; its audio is dropped until the format
    /// changes again.
    Unsupported,
}

impl AudioFramer {
    fn new() -> Self {
        Self {
            rate: SAMPLE_RATE,
            conversion: Conversion::Native,
            stereo: Vec::new(),
            queue: VecDeque::with_capacity(AUDIO_QUEUE_MAX),
        }
    }

    fn push(&mut self, chunk: &AudioChunk) {
        if chunk.sample_rate != self.rate {
            self.rate = chunk.sample_rate;
            self.conversion = Conversion::for_rate(chunk.sample_rate);
        }
        if matches!(self.conversion, Conversion::Unsupported) {
            return;
        }

        let channels = usize::from(chunk.channels.max(1));
        self.stereo.clear();
        match channels {
            1 => {
                for sample in &chunk.interleaved {
                    self.stereo.push(*sample);
                    self.stereo.push(*sample);
                }
            }
            CHANNELS => self.stereo.extend(
                chunk
                    .interleaved
                    .as_chunks::<CHANNELS>()
                    .0
                    .iter()
                    .flatten()
                    .copied(),
            ),
            // Surround and anything else: the mean of the channels in both,
            // which is all a stereo stream can honestly carry.
            _ => {
                let scale = 1.0 / channels as f32;
                for frame in chunk.interleaved.chunks_exact(channels) {
                    let mean = frame.iter().sum::<f32>() * scale;
                    self.stereo.push(mean);
                    self.stereo.push(mean);
                }
            }
        }

        match self.conversion {
            Conversion::Resampling(ref mut resample) => {
                resample.push(&self.stereo, &mut self.queue)
            }
            _ => self.queue.extend(self.stereo.iter().copied()),
        }

        let overflow = self.queue.len().saturating_sub(AUDIO_QUEUE_MAX);
        if overflow > 0 {
            // Whole frames only, so the two channels never swap places.
            let dropped = overflow.next_multiple_of(CHANNELS).min(self.queue.len());
            self.queue.drain(..dropped);
        }
    }

    /// Takes the oldest whole frame; `false` when there is not one yet.
    fn next_frame(&mut self, out: &mut [f32; STEREO_FRAME_SAMPLES]) -> bool {
        if self.queue.len() < STEREO_FRAME_SAMPLES {
            return false;
        }
        for (slot, sample) in out.iter_mut().zip(self.queue.drain(..STEREO_FRAME_SAMPLES)) {
            *slot = sample;
        }
        true
    }

    fn clear(&mut self) {
        self.queue.clear();
    }
}

impl Conversion {
    fn for_rate(rate: u32) -> Conversion {
        if rate == SAMPLE_RATE {
            return Conversion::Native;
        }
        match Resample::new(rate) {
            Ok(resample) => Conversion::Resampling(Box::new(resample)),
            Err(error) => {
                tracing::warn!(%error, rate, "cannot resample the share's audio, dropping it");
                Conversion::Unsupported
            }
        }
    }
}

/// A sinc resampler from the capture's rate to 48 kHz, interleaved stereo, fed
/// 20 ms of input at a time and producing a variable number of frames.
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
            CHANNELS,
            FixedAsync::Input,
        )?;
        let output = vec![0.0; inner.output_frames_max() * CHANNELS];
        Ok(Self {
            inner,
            pending: Vec::with_capacity(chunk * CHANNELS * 2),
            output,
        })
    }

    fn push(&mut self, samples: &[f32], out: &mut VecDeque<f32>) {
        self.pending.extend_from_slice(samples);
        loop {
            let frames_in = self.inner.input_frames_next();
            let needed = frames_in * CHANNELS;
            if self.pending.len() < needed {
                return;
            }

            let frames_out = self.output.len() / CHANNELS;
            let source = match InterleavedSlice::new(&self.pending[..needed], CHANNELS, frames_in) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(%error, "cannot wrap the share audio chunk");
                    self.pending.clear();
                    return;
                }
            };
            let mut target = match InterleavedSlice::new_mut(&mut self.output, CHANNELS, frames_out)
            {
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

            out.extend(self.output[..produced * CHANNELS].iter().copied());
            self.pending.drain(..needed);
        }
    }
}

#[derive(Clone)]
pub enum StageEvent {
    Picture {
        picture: Arc<Picture>,
        seq: u64,
    },
    /// Once a second while access units arrive.
    Stats {
        decode_fps: f32,
        pictures: u64,
        errors: u64,
        /// Access units thrown away undecoded: a backlog too deep to catch up
        /// with, or everything between a failure and the next keyframe.
        dropped: u64,
    },
    Failed(String),
}

impl fmt::Debug for StageEvent {
    /// Sizes only: a picture is somebody's screen.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageEvent::Picture { picture, seq } => f
                .debug_struct("Picture")
                .field("width", &picture.width)
                .field("height", &picture.height)
                .field("seq", seq)
                .finish(),
            StageEvent::Stats {
                decode_fps,
                pictures,
                errors,
                dropped,
            } => f
                .debug_struct("Stats")
                .field("decode_fps", decode_fps)
                .field("pictures", pictures)
                .field("errors", errors)
                .field("dropped", dropped)
                .finish(),
            StageEvent::Failed(reason) => f.debug_tuple("Failed").field(reason).finish(),
        }
    }
}

/// Dropping this stops the decode thread, within one [`DECODE_TICK`].
pub struct DecodeHandle {
    stop: Arc<AtomicBool>,
}

impl Drop for DecodeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Spawns the viewer's decode thread, reading the access units the media engine
/// reassembles.
///
/// The loop waits on the engine's channel through a current-thread tokio
/// runtime of its own: `blocking_recv` would leave a stalled stream's thread
/// asleep with nowhere to notice the handle going away, and the timeout is also
/// what paces the report.
///
/// A decoder that will not build is reported once and then tried again every
/// [`DECODER_RETRY`], the units arriving meanwhile thrown away: this thread
/// outlives any one watch, so ending it here would leave every later watch in
/// the session with no decoder and nothing to say about it.
pub fn spawn_decode_thread(
    units: UnboundedReceiver<AccessUnit>,
) -> (DecodeHandle, async_mpsc::UnboundedReceiver<StageEvent>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (events, updates) = async_mpsc::unbounded();

    let thread_stop = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("vorcall-decode".to_string())
        .spawn(move || decode(units, thread_stop, events));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the decode thread");
    }

    (DecodeHandle { stop }, updates)
}

fn decode(
    mut units: UnboundedReceiver<AccessUnit>,
    stop: Arc<AtomicBool>,
    events: async_mpsc::UnboundedSender<StageEvent>,
) {
    let emit = |event: StageEvent| {
        if events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for decoded pictures");
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emit(StageEvent::Failed(format!(
                "Cannot start the decode thread: {error}"
            )));
            return;
        }
    };

    // Built on the first pass, and again every DECODER_RETRY for as long as it
    // will not build.
    let mut decoder: Option<VideoDecoder> = None;
    let mut retry_at = Instant::now();
    let mut reported = false;

    let mut queue: VecDeque<AccessUnit> = VecDeque::new();
    let mut backlog: VecDeque<(bool, u32)> = VecDeque::new();
    // A stream can only be joined at a keyframe.
    let mut awaiting_keyframe = true;
    let mut seq = 0u64;
    let mut pictures = 0u64;
    let mut errors = 0u64;
    let mut dropped = 0u64;
    // Pictures as of the last report, for the rate.
    let mut window = 0u64;
    let mut stats_at = Instant::now();

    runtime.block_on(async {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            match tokio::time::timeout(DECODE_TICK, units.recv()).await {
                Ok(Some(unit)) => queue.push_back(unit),
                // The engine is gone; so is the stream.
                Ok(None) => break,
                Err(_) => {}
            }
            while let Ok(unit) = units.try_recv() {
                queue.push_back(unit);
            }

            let now = Instant::now();
            if decoder.is_none() && now >= retry_at {
                match VideoDecoder::new() {
                    Ok(built) => {
                        decoder = Some(built);
                        // A decoder that has seen none of the stream can only
                        // start at a keyframe, exactly as after an error.
                        awaiting_keyframe = true;
                    }
                    Err(error) => {
                        retry_at = now + DECODER_RETRY;
                        tracing::debug!(%error, "no video decoder yet");
                        if !reported {
                            reported = true;
                            emit(StageEvent::Failed(format!(
                                "Cannot start the video decoder: {error}"
                            )));
                        }
                    }
                }
            }

            if let Some(decoder) = decoder.as_mut() {
                backlog.clear();
                backlog.extend(queue.iter().map(|unit| (unit.keyframe, unit.frame_id)));
                let skip = units_to_skip(&backlog);
                for _ in 0..skip {
                    queue.pop_front();
                }
                dropped += skip as u64;
                if skip > 0 && queue.is_empty() {
                    awaiting_keyframe = true;
                }

                while let Some(unit) = queue.pop_front() {
                    if awaiting_keyframe && !unit.keyframe {
                        dropped += 1;
                        continue;
                    }
                    awaiting_keyframe = false;

                    match decoder.decode(&unit.data) {
                        Ok(Some(picture)) => {
                            seq += 1;
                            pictures += 1;
                            emit(StageEvent::Picture {
                                picture: Arc::new(picture),
                                seq,
                            });
                        }
                        // Not every access unit completes a picture.
                        Ok(None) => {}
                        Err(error) => {
                            tracing::debug!(%error, "dropping an undecodable access unit");
                            errors += 1;
                            awaiting_keyframe = true;
                        }
                    }
                }
            } else {
                // Nothing can be decoded, and keeping the units would only
                // spend memory on pictures nobody will ever see.
                dropped += queue.len() as u64;
                queue.clear();
            }

            let elapsed = now.saturating_duration_since(stats_at);
            if elapsed >= STATS_INTERVAL {
                stats_at = now;
                let decode_fps =
                    (pictures.saturating_sub(window) as f64 / elapsed.as_secs_f64()) as f32;
                window = pictures;
                emit(StageEvent::Stats {
                    decode_fps,
                    pictures,
                    errors,
                    dropped,
                });
            }
        }
    });
}

/// How many of the units waiting in front of the decoder to throw away, oldest
/// first.
///
/// A backlog this deep is a decoder that will not catch up by working through
/// it, so everything before the newest keyframe goes; with no keyframe among
/// them nothing there can be decoded after the gap either, and the caller waits
/// for the next one.
fn units_to_skip(queued: &VecDeque<(bool, u32)>) -> usize {
    if queued.len() <= MAX_BACKLOG {
        return 0;
    }
    queued
        .iter()
        .rposition(|(keyframe, _)| *keyframe)
        .unwrap_or(queued.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONE_HZ: f32 = 440.0;
    /// 20 ms in stereo frames, which is one sample per channel each.
    const FRAME_FRAMES: usize = STEREO_FRAME_SAMPLES / CHANNELS;

    fn tone(index: usize, rate: u32) -> f32 {
        (std::f32::consts::TAU * TONE_HZ * index as f32 / rate as f32).sin()
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
    fn a_44100_mono_chunk_becomes_48k_stereo_frames_without_gaps() {
        // One second of tone at 44.1 kHz, handed over in 10 ms chunks.
        let mut framer = AudioFramer::new();
        let mut frame = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut frames: Vec<[f32; STEREO_FRAME_SAMPLES]> = Vec::new();

        for chunk in 0..100 {
            let interleaved: Vec<f32> = (0..441)
                .map(|index| tone(chunk * 441 + index, 44_100))
                .collect();
            framer.push(&AudioChunk {
                sample_rate: 44_100,
                channels: 1,
                interleaved,
            });
            while framer.next_frame(&mut frame) {
                frames.push(frame);
            }
        }

        // 44 100 frames in is 48 000 out, which is 50 of them; the resampler's
        // own delay may hold the last one back.
        assert!(
            (49..=51).contains(&frames.len()),
            "one second became {} frames",
            frames.len()
        );

        let left: Vec<f32> = frames
            .iter()
            .flat_map(|frame| frame.as_chunks::<CHANNELS>().0.iter().map(|pair| pair[0]))
            .collect();
        for (index, frame) in frames.iter().enumerate() {
            for pair in frame.as_chunks::<CHANNELS>().0 {
                assert_eq!(pair[0], pair[1], "frame {index} is not centred");
            }
        }

        // A full-scale sine has an RMS of 1/sqrt(2), whatever it is resampled
        // to.
        let level = rms(left.iter().copied());
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (level - expected).abs() / expected < 0.2,
            "RMS was {level}, expected about {expected}"
        );

        // 440 Hz at 48 kHz moves by at most 0.058 between two samples; a lost
        // or repeated stretch anywhere, frame boundaries included, would jump.
        for (index, pair) in left.windows(2).enumerate() {
            assert!(
                (pair[1] - pair[0]).abs() <= 0.3,
                "a jump of {} at sample {index}",
                pair[1] - pair[0]
            );
        }
    }

    #[test]
    fn a_six_channel_chunk_is_downmixed_by_averaging() {
        let mut framer = AudioFramer::new();
        let channels = 6;
        let levels = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mean = levels.iter().sum::<f32>() / channels as f32;

        let interleaved: Vec<f32> = std::iter::repeat_n(levels, FRAME_FRAMES)
            .flatten()
            .collect();
        framer.push(&AudioChunk {
            sample_rate: 48_000,
            channels: channels as u16,
            interleaved,
        });

        let mut frame = [0.0f32; STEREO_FRAME_SAMPLES];
        assert!(framer.next_frame(&mut frame));
        for pair in frame.as_chunks::<CHANNELS>().0 {
            assert!((pair[0] - mean).abs() < 1e-6, "left was {}", pair[0]);
            assert!((pair[1] - mean).abs() < 1e-6, "right was {}", pair[1]);
        }
        assert!(!framer.next_frame(&mut frame), "one chunk is one frame");
    }

    #[test]
    fn a_48k_stereo_chunk_passes_through_untouched() {
        let mut framer = AudioFramer::new();
        let interleaved: Vec<f32> = (0..FRAME_FRAMES)
            .flat_map(|index| {
                let value = index as f32 / FRAME_FRAMES as f32;
                [value, -value]
            })
            .collect();
        framer.push(&AudioChunk {
            sample_rate: 48_000,
            channels: 2,
            interleaved: interleaved.clone(),
        });

        let mut frame = [0.0f32; STEREO_FRAME_SAMPLES];
        assert!(framer.next_frame(&mut frame));
        assert_eq!(frame.as_slice(), interleaved.as_slice());
    }

    #[test]
    fn a_backlog_over_eight_units_skips_to_the_next_keyframe() {
        // Nine units, the newest keyframe sitting at index 6.
        let queued: VecDeque<(bool, u32)> = [
            (true, 0),
            (false, 1),
            (false, 2),
            (false, 3),
            (true, 4),
            (false, 5),
            (true, 6),
            (false, 7),
            (false, 8),
        ]
        .into_iter()
        .collect();
        assert_eq!(units_to_skip(&queued), 6);

        // Nothing to restart from: all of them go and the caller waits.
        let no_keyframe: VecDeque<(bool, u32)> =
            (0..12).map(|frame_id| (false, frame_id)).collect();
        assert_eq!(units_to_skip(&no_keyframe), no_keyframe.len());
    }

    #[test]
    fn a_short_backlog_is_decoded_in_order() {
        let mut queued: VecDeque<(bool, u32)> = VecDeque::new();
        for frame_id in 0..=MAX_BACKLOG as u32 {
            queued.push_back((frame_id == 0, frame_id));
            if queued.len() <= MAX_BACKLOG {
                assert_eq!(units_to_skip(&queued), 0, "{} queued", queued.len());
            }
        }
        // One past the threshold, with the only keyframe still at the front:
        // there is nothing newer to skip to, so nothing is thrown away.
        assert_eq!(queued.len(), MAX_BACKLOG + 1);
        assert_eq!(units_to_skip(&queued), 0);
    }
}
