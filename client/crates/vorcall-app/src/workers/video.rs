//! The half of a sending video pipeline the screen share and the camera have in
//! common: keep the newest captured frame, encode it on the preset's own
//! deadline rather than the capture's, and put the access unit on the wire.
//!
//! The two differ only in what they encode to and which packet type they leave
//! on, which is all [`Track`] carries. Everything else — the pause while nobody
//! watches, the forced keyframes, the scale, the counters — is one body here, so
//! there is no second copy for the share's behaviour to drift from.

use std::time::{Duration, Instant};

use vorcall_screen::VideoFrame;
use vorcall_screen::codec::{EncoderSettings, Usage, VideoEncoder};
use vorcall_screen::preset::{CameraPreset, FrameRate, Preset};
use vorcall_screen::scale::scale_bgra;
use vorcall_voice::FrameSender;

use crate::workers::voice::Throttle;

/// How often a running pipeline reports.
pub const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// However many cores the machine has, never more slice threads than this: the
/// rest of the client needs the processor too.
const MAX_ENCODER_THREADS: usize = 8;

/// Which stream a pipeline carries, and the preset it is encoded to.
#[derive(Clone, Copy, Debug)]
pub enum Track {
    Screen(Preset),
    Camera(CameraPreset),
}

impl Track {
    /// What this track is called in a log line.
    pub fn label(self) -> &'static str {
        match self {
            Track::Screen(_) => "screen share",
            Track::Camera(_) => "camera",
        }
    }

    /// What a failure sentence calls the thing that will not encode.
    fn subject(self) -> &'static str {
        match self {
            Track::Screen(_) => "screen",
            Track::Camera(_) => "camera",
        }
    }

    pub fn fps(self) -> FrameRate {
        match self {
            Track::Screen(preset) => preset.fps,
            Track::Camera(preset) => preset.fps,
        }
    }

    fn output_size(self, source: (u32, u32)) -> (u32, u32) {
        match self {
            Track::Screen(preset) => preset.output_size(source),
            Track::Camera(preset) => preset.output_size(source),
        }
    }

    fn bitrate_kbps(self, source: (u32, u32)) -> u32 {
        match self {
            Track::Screen(preset) => preset.bitrate_kbps(source),
            Track::Camera(preset) => preset.bitrate_kbps(),
        }
    }

    fn usage(self) -> Usage {
        match self {
            Track::Screen(_) => Usage::Screen,
            Track::Camera(_) => Usage::Camera,
        }
    }

    /// The error is turned into its sentence here: `EngineError` is private to
    /// `vorcall-voice`, and a caller only ever logs it.
    fn send(
        self,
        sender: &FrameSender,
        frame_id: u32,
        keyframe: bool,
        unit: &[u8],
    ) -> Result<(), String> {
        match self {
            Track::Screen(_) => sender.send_video(frame_id, keyframe, unit),
            Track::Camera(_) => sender.send_camera_video(frame_id, keyframe, unit),
        }
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    /// The two streams are encoded separately, so each has a keyframe flag of
    /// its own.
    fn take_keyframe_request(self, sender: &FrameSender) -> bool {
        match self {
            Track::Screen(_) => sender.take_keyframe_request(),
            Track::Camera(_) => sender.take_camera_keyframe_request(),
        }
    }
}

/// What one running pipeline has done, once a second.
#[derive(Debug, Clone, Copy)]
pub struct VideoReport {
    pub capture_fps: f32,
    pub encode_fps: f32,
    pub kbps: u32,
    pub output: (u32, u32),
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub dropped_frames: u64,
    pub skipped_frames: u64,
    /// Datagrams the socket gave up on over the last window, retries included.
    /// The engine counts them for the whole session, so with a share and a
    /// camera running at once each report carries whatever the other lost too.
    pub send_failures: u64,
}

#[derive(Clone, Copy, Default)]
struct Counters {
    captured: u64,
    encoded: u64,
    bytes: u64,
    keyframes: u64,
    keyframe_requests: u64,
    dropped: u64,
    skipped: u64,
}

/// One video stream on its way out: the encoder, the frame waiting for it, and
/// what the pair have done.
pub struct VideoTrack {
    track: Track,
    sender: FrameSender,
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
    /// Nobody is watching, so nothing is encoded. True until the server has
    /// accepted the stream and named a watcher: a frame put on the wire before
    /// that is a datagram the relay drops.
    paused: bool,
    unit: Vec<u8>,
    scaled: Vec<u8>,
    counters: Counters,
    /// The counters as of the last report, for the two rates.
    window: Counters,
    /// The engine's own send-failure total as of that same report; it counts the
    /// whole session, so only the difference belongs to this stream.
    last_send_failures: u64,
    stats_at: Instant,
    warning: Throttle,
}

impl VideoTrack {
    pub fn new(track: Track, sender: FrameSender, now: Instant) -> Self {
        // Read before anything is sent, so a session that already had failures
        // does not report them all as this stream's first second.
        let last_send_failures = sender.send_failures();
        Self {
            track,
            sender,
            encoder: None,
            source: None,
            output: (0, 0),
            latest: None,
            fresh: false,
            frame_id: 0,
            keyframe_pending: true,
            next_encode: now,
            paused: true,
            unit: Vec::new(),
            scaled: Vec::new(),
            counters: Counters::default(),
            window: Counters::default(),
            last_send_failures,
            stats_at: now,
            warning: Throttle::default(),
        }
    }

    /// The size the frames go out at, once the encoder has been built.
    pub fn output(&self) -> (u32, u32) {
        self.output
    }

    /// Whether the pause actually changed, which is what tells a caller with
    /// more to pause that it has something to do.
    pub fn set_paused(&mut self, paused: bool) -> bool {
        if paused == self.paused {
            return false;
        }
        self.paused = paused;
        if !paused {
            // Whoever just started watching can only begin at a keyframe.
            self.keyframe_pending = true;
        }
        true
    }

    pub fn force_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    /// The backend has said what it captures. `Err` carries the sentence the
    /// caller reports: a stream with no encoder has nothing to send.
    pub fn started(&mut self, source: (u32, u32)) -> Result<(), String> {
        self.build_encoder(source)?;
        self.frame_id = 0;
        Ok(())
    }

    /// One captured frame, replacing whatever the encoder has not taken yet.
    pub fn on_video(&mut self, frame: VideoFrame) -> Result<(), String> {
        if self.source != Some((frame.width, frame.height)) {
            self.build_encoder((frame.width, frame.height))?;
        }

        self.counters.captured += 1;
        // Paused, nothing is meant to be encoded, so replacing a frame is not a
        // frame lost.
        if self.fresh && !self.paused {
            self.counters.dropped += 1;
        }
        self.latest = Some(frame);
        self.fresh = true;
        Ok(())
    }

    /// The newest captured frame, for a caller that draws it locally.
    pub fn latest(&self) -> Option<&VideoFrame> {
        self.latest.as_ref()
    }

    /// Builds the encoder for a capture of `source`, at the size and bitrate the
    /// preset asks for.
    fn build_encoder(&mut self, source: (u32, u32)) -> Result<(), String> {
        let output = self.track.output_size(source);
        let threads = std::thread::available_parallelism()
            .map_or(1, |cores| cores.get() / 2)
            .clamp(1, MAX_ENCODER_THREADS) as u16;
        let settings = EncoderSettings {
            width: output.0,
            height: output.1,
            fps: self.track.fps().hz(),
            bitrate_kbps: self.track.bitrate_kbps(source),
            threads,
            usage: self.track.usage(),
        };

        let encoder = VideoEncoder::new(settings)
            .map_err(|error| format!("Cannot encode this {}: {error}", self.track.subject()))?;
        tracing::info!(
            track = self.track.label(),
            source = ?source,
            output = ?output,
            bitrate_kbps = settings.bitrate_kbps,
            threads = encoder.threads(),
            "encoding a video track"
        );
        self.encoder = Some(encoder);
        self.source = Some(source);
        self.output = output;
        // Nothing a viewer holds decodes against the new stream.
        self.keyframe_pending = true;
        self.scaled.clear();
        Ok(())
    }

    /// Encodes at most one frame, on the preset's cadence rather than the
    /// capture's.
    pub fn encode_tick(&mut self, now: Instant) {
        if now < self.next_encode {
            return;
        }
        let interval = self.track.fps().interval();
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
        if self.track.take_keyframe_request(&self.sender) {
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
                    tracing::warn!(
                        %error,
                        track = self.track.label(),
                        "dropping a frame the encoder refused"
                    );
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
            .track
            .send(&self.sender, frame_id, encoded.keyframe, &self.unit)
            && self.warning.allow(now)
        {
            tracing::debug!(
                %error,
                track = self.track.label(),
                "dropping a frame the socket refused"
            );
        }
    }

    /// The report for the window that has just closed, or `None` while it is
    /// still open. `extra_bytes` is what a caller sending more over the same
    /// stream — the share's audio — has put on the wire in total.
    pub fn report(&mut self, now: Instant, extra_bytes: u64) -> Option<VideoReport> {
        let elapsed = now.saturating_duration_since(self.stats_at);
        if elapsed < STATS_INTERVAL {
            return None;
        }
        self.stats_at = now;
        let seconds = elapsed.as_secs_f64();
        let rate = |current: u64, previous: u64| {
            (current.saturating_sub(previous) as f64 / seconds) as f32
        };

        let send_failures = self.sender.send_failures();
        let refused = send_failures.saturating_sub(self.last_send_failures);
        self.last_send_failures = send_failures;
        if refused > 0 && self.warning.allow(now) {
            tracing::warn!(
                refused,
                track = self.track.label(),
                "datagrams refused by the socket in the last second"
            );
        }

        let bytes = self.counters.bytes.saturating_add(extra_bytes);
        let report = VideoReport {
            capture_fps: rate(self.counters.captured, self.window.captured),
            encode_fps: rate(self.counters.encoded, self.window.encoded),
            kbps: (bytes.saturating_sub(self.window.bytes) as f64 * 8.0 / 1_000.0 / seconds) as u32,
            output: self.output,
            keyframes: self.counters.keyframes,
            keyframe_requests: self.counters.keyframe_requests,
            dropped_frames: self.counters.dropped,
            skipped_frames: self.counters.skipped,
            send_failures: refused,
        };
        self.window = Counters {
            bytes,
            ..self.counters
        };
        Some(report)
    }
}
