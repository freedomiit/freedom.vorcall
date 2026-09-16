//! The worker threads a screen share needs, all of them away from the
//! interface.
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
//! That audio has a thread of its own, and the capture's events are split
//! between the two the moment they arrive: a picture waits for the encoder,
//! which spends tens of milliseconds on a frame and more on a keyframe, and
//! audio waiting behind it is audio the watcher hears stutter.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc as async_mpsc;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters};
use tokio::sync::mpsc::UnboundedReceiver;
use vorcall_screen::codec::{Picture, VideoDecoder};
use vorcall_screen::preset::Preset;
use vorcall_screen::{
    AudioChunk, AudioMode, CaptureEvent, CaptureRequest, Capturer, Unavailable, VideoFrame,
};
use vorcall_voice::cleanup::FAR_END_MAX_SAMPLES;
use vorcall_voice::{
    AccessUnit, FRAME_MS, FrameSender, SAMPLE_RATE, STEREO_FRAME_SAMPLES, ShareCleanup,
    StereoEncoder,
};

use crate::workers::video::{STATS_INTERVAL, Track, VideoTrack};
use crate::workers::voice::Throttle;
use crate::workers::{Mailbox, lock};

/// How often the share thread wakes when no command arrives. Well under one
/// frame even at 60 fps, so the encode deadline is never missed by much.
const TICK: Duration = Duration::from_millis(5);

/// The decode thread paces nothing, so it only wakes to notice a dropped handle
/// and to report.
const DECODE_TICK: Duration = Duration::from_millis(100);

/// How long the share-audio thread waits for a chunk before looking at the
/// far-end reference again. It works when work arrives, so this is only what
/// keeps the canceller's reference moving through a quiet capture.
const AUDIO_WAIT: Duration = Duration::from_millis(20);

/// A stretch this long with no whole frame to send is a gap in the capture: the
/// next frame goes out marked, and the clock skips the silence.
const SPURT_GAP: Duration = Duration::from_millis(200);

/// Opus at 96 kbit/s over 20 ms stereo frames never comes near this.
const MAX_PACKET: usize = 1024;

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
    /// Blocks of share audio the echo canceller produced nothing for, which
    /// went out as they were captured.
    pub audio_passed_through: u64,
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
                    pipeline.video.force_keyframe();
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
        let built = Pipeline::new(
            self.events.clone(),
            capturer,
            starting.frames,
            starting.preset,
            starting.sender,
            starting.share_far_end,
        );
        let mut pipeline = match built {
            Ok(pipeline) => pipeline,
            Err(error) => {
                self.emit(ShareEvent::Failed(format!(
                    "cannot start the capture: {error}"
                )));
                return;
            }
        };
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
/// or when the capture ends, which stops the capture and joins the share-audio
/// thread with it.
struct Pipeline {
    events: async_mpsc::UnboundedSender<ShareEvent>,
    /// Only held so the capture keeps running: it stops the moment it is
    /// dropped, which must happen on this thread.
    capturer: Capturer,
    /// Everything the capture produced except its audio, which the splitter
    /// hands straight to the share-audio thread.
    frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
    sender: FrameSender,
    share_far_end: Arc<Mutex<VecDeque<f32>>>,

    /// The picture on its way out: the encoder, the frame waiting for it and the
    /// counters, all shared with the camera's pipeline.
    video: VideoTrack,
    /// No watchers, so nothing is encoded and no audio is sent. True until the
    /// server has accepted the share and named one: a frame put on the wire
    /// before that is a datagram the relay drops as not sharing.
    paused: bool,

    /// The share-audio thread, once the backend has said there is audio to
    /// send. Its end of the channel waits here until then.
    audio: Option<AudioThread>,
    audio_channel: Option<(Sender<AudioMessage>, Receiver<AudioMessage>)>,
    audio_counters: Arc<AudioCounters>,
}

/// What the share-audio thread has done, for the pipeline's report.
#[derive(Default)]
struct AudioCounters {
    frames: AtomicU64,
    bytes: AtomicU64,
    passed_through: AtomicU64,
}

impl Pipeline {
    /// `Err` when the splitter thread will not start, which is a share that
    /// would never see a frame.
    fn new(
        events: async_mpsc::UnboundedSender<ShareEvent>,
        capturer: Capturer,
        capture: async_mpsc::UnboundedReceiver<CaptureEvent>,
        preset: Preset,
        sender: FrameSender,
        share_far_end: Arc<Mutex<VecDeque<f32>>>,
    ) -> std::io::Result<Self> {
        let now = Instant::now();
        let (pictures, frames) = async_mpsc::unbounded();
        let (chunks, audio_inbox) = std::sync::mpsc::channel();
        spawn_splitter(capture, pictures, chunks.clone())?;

        Ok(Self {
            events,
            capturer,
            frames,
            video: VideoTrack::new(Track::Screen(preset), sender.clone(), now),
            sender,
            share_far_end,
            paused: true,
            audio: None,
            audio_channel: Some((chunks, audio_inbox)),
            audio_counters: Arc::new(AudioCounters::default()),
        })
    }

    /// One pass: every picture the backend produced since the last one, then
    /// the encode deadline and the report. `false` once the share is over,
    /// which is when it has emitted its own last event.
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
                // The splitter never sends audio this way: it belongs to the
                // share-audio thread, which is the whole point of the split.
                Ok(CaptureEvent::Audio(_)) => {}
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
        self.stats_tick(now);
        true
    }

    fn set_paused(&mut self, paused: bool) {
        if !self.video.set_paused(paused) {
            return;
        }
        self.paused = paused;
        // The audio thread clears its own queue and its own reference: doing
        // either from here would race whatever it is sending.
        if let Some(audio) = self.audio.as_ref() {
            audio.send(AudioMessage::Paused(paused));
        }
    }

    fn on_started(&mut self, width: u32, height: u32, audio: Option<AudioMode>) -> bool {
        if let Err(reason) = self.video.started((width, height)) {
            self.emit(ShareEvent::Failed(reason));
            return false;
        }
        self.audio = match (audio, self.audio_channel.take()) {
            (Some(mode), Some(channel)) => AudioThread::spawn(
                mode,
                self.paused,
                channel,
                self.sender.clone(),
                Arc::clone(&self.share_far_end),
                Arc::clone(&self.audio_counters),
            ),
            _ => None,
        };

        self.emit(ShareEvent::Started {
            width,
            height,
            output: self.video.output(),
            // What is really sent: a share whose encoder would not build sends
            // no audio at all.
            audio: self.audio.as_ref().map(|audio| audio.mode),
            backend: self.capturer.backend(),
        });
        true
    }

    fn on_video(&mut self, frame: VideoFrame) -> bool {
        if let Err(reason) = self.video.on_video(frame) {
            self.emit(ShareEvent::Failed(reason));
            return false;
        }
        true
    }

    fn encode_tick(&mut self, now: Instant) {
        self.video.encode_tick(now);
    }

    /// The picture's counters, with what the share-audio thread put on the wire
    /// folded into the bitrate.
    fn stats_tick(&mut self, now: Instant) {
        let audio_bytes = self.audio_counters.bytes.load(Ordering::Relaxed);
        let Some(report) = self.video.report(now, audio_bytes) else {
            return;
        };

        self.emit(ShareEvent::Stats(ShareStats {
            capture_fps: report.capture_fps,
            encode_fps: report.encode_fps,
            kbps: report.kbps,
            output: report.output,
            keyframes: report.keyframes,
            keyframe_requests: report.keyframe_requests,
            dropped_frames: report.dropped_frames,
            skipped_frames: report.skipped_frames,
            audio_frames: self.audio_counters.frames.load(Ordering::Relaxed),
            audio_passed_through: self.audio_counters.passed_through.load(Ordering::Relaxed),
            send_failures: report.send_failures,
        }));
    }

    fn emit(&self, event: ShareEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for share events");
        }
    }
}

/// Splits the capture's events the moment they arrive: audio goes to the
/// share-audio thread, everything else to the pipeline. The pipeline spends
/// tens of milliseconds inside one encode, and a chunk waiting behind that is
/// a chunk sent in a batch nobody can play back smoothly.
///
/// The thread ends with the capture, whose backend drops the sender when the
/// [`Capturer`] is dropped, or as soon as the pipeline has stopped listening.
fn spawn_splitter(
    mut capture: async_mpsc::UnboundedReceiver<CaptureEvent>,
    pictures: async_mpsc::UnboundedSender<CaptureEvent>,
    chunks: Sender<AudioMessage>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("vorcall-share-split".to_string())
        .spawn(move || {
            futures::executor::block_on(async move {
                while let Some(event) = capture.next().await {
                    let listening = match event {
                        // A share with no audio thread has nobody to take the
                        // chunk, which is not a reason to stop the picture.
                        CaptureEvent::Audio(chunk) => {
                            let _ = chunks.send(AudioMessage::Chunk(chunk));
                            !pictures.is_closed()
                        }
                        other => pictures.unbounded_send(other).is_ok(),
                    };
                    if !listening {
                        break;
                    }
                }
            });
        })?;
    Ok(())
}

/// What reaches the share-audio thread.
enum AudioMessage {
    Chunk(AudioChunk),
    Paused(bool),
    Stop,
}

/// The pipeline's end of the share-audio thread.
struct AudioThread {
    /// What the capture really gives, which is what the app is told about.
    mode: AudioMode,
    messages: Sender<AudioMessage>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioThread {
    /// Starts the thread and waits for it to say whether it has an encoder:
    /// a share whose encoder will not build sends no audio at all, and the
    /// `Started` event has to say so. `None` is exactly that case.
    fn spawn(
        mode: AudioMode,
        paused: bool,
        channel: (Sender<AudioMessage>, Receiver<AudioMessage>),
        sender: FrameSender,
        far_end: Arc<Mutex<VecDeque<f32>>>,
        counters: Arc<AudioCounters>,
    ) -> Option<AudioThread> {
        let (messages, inbox) = channel;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (built, ready) = std::sync::mpsc::channel();

        // `ShareCleanup` is `!Send`, so everything below is built, run and
        // dropped on that thread and nowhere else.
        let spawned = std::thread::Builder::new()
            .name("vorcall-share-audio".to_string())
            .spawn(move || {
                let Some(mut audio) = ShareAudio::new(mode, sender, far_end, counters) else {
                    let _ = built.send(false);
                    return;
                };
                let _ = built.send(true);
                audio.set_paused(paused);
                audio.run(&inbox, &thread_stop);
            });

        let handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(%error, "no share audio thread, sharing the picture only");
                return None;
            }
        };
        if !matches!(ready.recv(), Ok(true)) {
            let _ = handle.join();
            return None;
        }
        Some(AudioThread {
            mode,
            messages,
            stop,
            handle: Some(handle),
        })
    }

    fn send(&self, message: AudioMessage) {
        if self.messages.send(message).is_err() {
            tracing::debug!("the share audio thread is gone, dropping the message");
        }
    }
}

impl Drop for AudioThread {
    /// Stops the thread and waits for it, so the capture's audio never outlives
    /// the share it belongs to. The flag is what it stops on; the message is
    /// only there to wake it out of its wait.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.messages.send(AudioMessage::Stop);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::warn!("the share audio thread panicked");
        }
    }
}

/// The share's own audio, on its thread: whatever the backend captured, turned
/// into the 20 ms stereo Opus frames the wire takes and put on it as they come.
struct ShareAudio {
    sender: FrameSender,
    /// What the mixer played, for the canceller below; the audio thread fills
    /// it and this thread is what drains it.
    far_end: Arc<Mutex<VecDeque<f32>>>,
    counters: Arc<AudioCounters>,
    encoder: StereoEncoder,
    framer: AudioFramer,
    /// `None` when the capture excludes this machine's playout, and after the
    /// chain has failed: there is nothing to subtract, or nothing left to
    /// subtract it with.
    cleanup: Option<ShareCleanup>,
    spurt: Spurt,
    /// Nobody is watching: chunks are dropped rather than queued.
    paused: bool,
    frame: [f32; STEREO_FRAME_SAMPLES],
    /// The frame as captured, restored if the chain fails mid-frame.
    raw: [f32; STEREO_FRAME_SAMPLES],
    packet: [u8; MAX_PACKET],
    far_end_scratch: Vec<f32>,
    warning: Throttle,
}

impl ShareAudio {
    /// `None` when the encoder will not build, which is a share without audio
    /// rather than no share.
    fn new(
        mode: AudioMode,
        sender: FrameSender,
        far_end: Arc<Mutex<VecDeque<f32>>>,
        counters: Arc<AudioCounters>,
    ) -> Option<ShareAudio> {
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
                    lock(&far_end).clear();
                    Some(cleanup)
                }
                Err(error) => {
                    tracing::warn!(%error, "no share echo canceller, sending the capture as it came");
                    None
                }
            },
        };

        Some(ShareAudio {
            sender,
            far_end,
            counters,
            encoder,
            framer: AudioFramer::new(),
            cleanup,
            spurt: Spurt::default(),
            paused: true,
            frame: [0.0; STEREO_FRAME_SAMPLES],
            raw: [0.0; STEREO_FRAME_SAMPLES],
            packet: [0; MAX_PACKET],
            far_end_scratch: Vec::with_capacity(FAR_END_MAX_SAMPLES),
            warning: Throttle::default(),
        })
    }

    /// Waits for chunks and sends what they add up to, until the pipeline stops
    /// the thread or drops the channel.
    fn run(&mut self, messages: &Receiver<AudioMessage>, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match messages.recv_timeout(AUDIO_WAIT) {
                Ok(AudioMessage::Chunk(chunk)) => {
                    if !self.paused {
                        self.framer.push(&chunk);
                    }
                }
                Ok(AudioMessage::Paused(paused)) => self.set_paused(paused),
                Ok(AudioMessage::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.pump(Instant::now());
        }
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            // Nobody is watching, so the audio already captured is dropped
            // rather than played back late when someone arrives.
            self.framer.clear();
            return;
        }
        // What played while nobody watched is no reference for the audio
        // captured from here on; left in, the canceller would stay a ring's
        // length behind for the rest of the share.
        lock(&self.far_end).clear();
        // Every watcher joins a stream that begins here.
        self.sender.reset_share_audio_clock();
        self.spurt.restart();
    }

    /// Hands the canceller everything the mixer has played since the last pass.
    fn push_far_end(&mut self) {
        let Some(cleanup) = self.cleanup.as_mut() else {
            return;
        };
        self.far_end_scratch.clear();
        self.far_end_scratch.extend(lock(&self.far_end).drain(..));
        cleanup.push_far_end(&self.far_end_scratch);
    }

    /// Sends every whole frame the framer has ready, back to back: a batch is
    /// the thread having waited, not a gap in the capture, and it goes out
    /// under consecutive timestamps so the watcher plays it as one stretch.
    fn pump(&mut self, now: Instant) {
        if self.paused {
            return;
        }
        self.push_far_end();

        while self.framer.next_frame(&mut self.frame) {
            self.clean();
            let start = self.spurt.next(now);
            if start.skip > 0 {
                self.sender.skip_share_audio(start.skip);
            }

            let encoded = match self.encoder.encode(&self.frame, &mut self.packet) {
                Ok(written) => written,
                Err(error) => {
                    if self.warning.allow(now) {
                        tracing::warn!(%error, "dropping share audio the encoder refused");
                    }
                    // Its slot on the clock goes with it, so the watcher
                    // conceals the frame instead of hearing the next one early.
                    self.sender.skip_share_audio(1);
                    if start.marker {
                        self.spurt.restart();
                    }
                    continue;
                }
            };

            match self
                .sender
                .send_share_audio(&self.packet[..encoded], start.marker)
            {
                Ok(()) => {
                    self.counters.frames.fetch_add(1, Ordering::Relaxed);
                    self.counters
                        .bytes
                        .fetch_add(encoded as u64, Ordering::Relaxed);
                }
                Err(error) => {
                    if self.warning.allow(now) {
                        tracing::debug!(%error, "dropping share audio the socket refused");
                    }
                }
            }
        }

        if let Some(cleanup) = self.cleanup.as_ref() {
            self.counters
                .passed_through
                .store(cleanup.passed_through(), Ordering::Relaxed);
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

/// Where a run of share audio begins, judged on the share-audio thread's own
/// clock: a stretch with no whole frame to send is a gap in the capture, while
/// a batch of frames arriving together is only this thread having waited.
#[derive(Default)]
struct Spurt {
    /// The stream (re)started and nothing has gone out under it yet.
    starting: bool,
    /// When a whole frame was last there to send.
    last_frame_at: Option<Instant>,
}

/// How the next frame goes out: whether it opens a run, and how many frames of
/// silence the media clock skips before it.
#[derive(Debug, PartialEq, Eq)]
struct FrameStart {
    marker: bool,
    skip: u32,
}

impl Spurt {
    fn restart(&mut self) {
        self.starting = true;
        self.last_frame_at = None;
    }

    fn next(&mut self, now: Instant) -> FrameStart {
        let gap = gap_frames(self.last_frame_at, now);
        self.last_frame_at = Some(now);
        FrameStart {
            marker: std::mem::take(&mut self.starting) || gap.is_some(),
            skip: gap.unwrap_or(0),
        }
    }
}

/// The frames a gap in the capture swallowed before the one now going out, or
/// `None` when that frame simply follows the one before it.
fn gap_frames(last_frame_at: Option<Instant>, now: Instant) -> Option<u32> {
    let gap = now.saturating_duration_since(last_frame_at?);
    if gap < SPURT_GAP {
        return None;
    }
    let frames = gap.as_millis() / u128::from(FRAME_MS);
    Some(u32::try_from(frames.saturating_sub(1)).unwrap_or(u32::MAX))
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
    /// A picture is waiting in the decode handle's mailbox. The picture itself
    /// never travels, so at most one of these is ever queued however long the
    /// interface takes to read it, and what it reads is always the newest frame.
    Picture,
    /// Once a second while access units arrive.
    Stats {
        decode_fps: f32,
        pictures: u64,
        errors: u64,
        /// Frames thrown away: access units left undecoded — a backlog too deep
        /// to catch up with, or everything between a failure and the next
        /// keyframe — plus pictures overwritten in the mailbox before the
        /// interface could draw them.
        dropped: u64,
    },
    Failed(String),
}

impl fmt::Debug for StageEvent {
    /// Sizes only: a picture is somebody's screen.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageEvent::Picture => f.write_str("Picture"),
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
    /// The newest decoded picture behind [`StageEvent::Picture`].
    pub picture: Arc<Mailbox<(Arc<Picture>, u64)>>,
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
    let picture = Arc::new(Mailbox::new());

    let thread_stop = stop.clone();
    let decoded = picture.clone();
    let spawned = std::thread::Builder::new()
        .name("vorcall-decode".to_string())
        .spawn(move || decode(units, thread_stop, events, decoded));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the decode thread");
    }

    (DecodeHandle { stop, picture }, updates)
}

fn decode(
    mut units: UnboundedReceiver<AccessUnit>,
    stop: Arc<AtomicBool>,
    events: async_mpsc::UnboundedSender<StageEvent>,
    mailbox: Arc<Mailbox<(Arc<Picture>, u64)>>,
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
                            let posted = mailbox.post((Arc::new(picture), seq));
                            // A picture the interface never took is one it never
                            // drew, which is a dropped frame like any other.
                            if posted.replaced {
                                dropped += 1;
                            }
                            if posted.marker {
                                emit(StageEvent::Picture);
                            }
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

    #[test]
    fn only_a_real_silence_counts_as_a_gap() {
        let t0 = Instant::now();
        assert_eq!(
            gap_frames(None, t0),
            None,
            "the first frame follows nothing"
        );
        assert_eq!(gap_frames(Some(t0), t0), None, "a batch is not a gap");
        assert_eq!(gap_frames(Some(t0), t0 + Duration::from_millis(199)), None);
        // 200 ms is ten 20 ms frames, nine of them before the one going out.
        assert_eq!(
            gap_frames(Some(t0), t0 + Duration::from_millis(200)),
            Some(9)
        );
        assert_eq!(gap_frames(Some(t0), t0 + Duration::from_secs(1)), Some(49));
    }

    #[test]
    fn a_batch_of_frames_runs_on_and_a_gap_opens_a_new_run() {
        let t0 = Instant::now();
        let mut spurt = Spurt::default();
        spurt.restart();

        // The first frame of the share opens the run and skips nothing.
        assert_eq!(
            spurt.next(t0),
            FrameStart {
                marker: true,
                skip: 0
            }
        );
        // The rest of the batch is the same run, at consecutive timestamps.
        for late in [0, 0, 60] {
            assert_eq!(
                spurt.next(t0 + Duration::from_millis(late)),
                FrameStart {
                    marker: false,
                    skip: 0
                }
            );
        }

        // Half a second of nothing to send: 25 frames' worth, 24 of them before
        // the one that ends it.
        let after = t0 + Duration::from_millis(560);
        assert_eq!(
            spurt.next(after),
            FrameStart {
                marker: true,
                skip: 24
            }
        );
        assert_eq!(
            spurt.next(after),
            FrameStart {
                marker: false,
                skip: 0
            }
        );

        // A resume starts a run without pretending anything was lost.
        spurt.restart();
        assert_eq!(
            spurt.next(after + Duration::from_secs(30)),
            FrameStart {
                marker: true,
                skip: 0
            }
        );
    }
}
