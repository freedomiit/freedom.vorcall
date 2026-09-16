//! The camera's own capture → encode → send pipeline, beside the screen
//! share's rather than instead of it.
//!
//! It is the share's pipeline with the audio half taken off and a local preview
//! put on: [`crate::workers::video::VideoTrack`] is the shared body, and what is
//! left here is opening the device, the preview, and the events the interface
//! reads.
//!
//! The preview never leaves this machine. The frame the encoder is about to take
//! is downscaled to a tile and turned into the same I420 [`Picture`] a decoder
//! would have produced, so the stage draws the local tile through exactly the
//! shader it draws everybody else's through — and the conversion happens here,
//! on this thread, with only a marker travelling to the interface.
//!
//! Like the share's, everything here blocks: [`start_camera`] waits on the
//! platform's permission dialog and [`vorcall_voice::FrameSender`] paces itself
//! in milliseconds, so the interface only pushes a command in and reads events
//! back.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use futures::channel::mpsc as async_mpsc;
use vorcall_screen::codec::Picture;
use vorcall_screen::preset::CameraPreset;
use vorcall_screen::scale::scale_bgra;
use vorcall_screen::{
    CameraRequest, CaptureEvent, Capturer, Unavailable, VideoFrame, start_camera,
};
use vorcall_voice::FrameSender;

use crate::workers::Mailbox;
use crate::workers::video::{Track, VideoTrack};

/// How often the camera thread wakes when no command arrives, the same tick the
/// share's pipeline keeps.
const TICK: Duration = Duration::from_millis(5);

/// The box the local preview is fitted into. A tile beside the conversation is
/// a few hundred pixels wide however large the device's own frame is, and this
/// conversion runs on the pipeline thread in front of the encoder.
const PREVIEW_BOX: (u32, u32) = (320, 180);

/// How often that preview is rebuilt. Well under any capture rate: the tile is
/// small and nobody watches their own face for motion detail.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(66);

/// BT.709 limited range, the range the encoder writes and the stage's shader
/// inverts. Anything else here and the local tile would not match the peers'.
const Y_MIN: f32 = 16.0;
const Y_RANGE: f32 = 219.0;
const C_MID: f32 = 128.0;
const C_RANGE: f32 = 224.0;
const K_R: f32 = 0.2126;
const K_G: f32 = 0.7152;
const K_B: f32 = 0.0722;

pub enum CameraCommand {
    Start {
        request: CameraRequest,
        preset: CameraPreset,
        sender: FrameSender,
    },
    /// Set while the server reports no watchers: the device stays open and the
    /// preview carries on, encoding and sending stop, and resuming forces a
    /// keyframe. A camera starts paused, so the first watcher is what opens the
    /// stream.
    SetPaused(bool),
    ForceKeyframe,
    /// Drops the device and everything built on it. The thread stays alive for a
    /// later [`CameraCommand::Start`].
    Stop,
}

#[derive(Debug, Clone)]
pub struct CameraStats {
    pub capture_fps: f32,
    pub encode_fps: f32,
    pub kbps: u32,
    pub output: (u32, u32),
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub dropped_frames: u64,
    pub skipped_frames: u64,
    pub send_failures: u64,
}

#[derive(Debug, Clone)]
pub enum CameraEvent {
    /// What the device actually gave, which is not always what was asked for,
    /// and the size it is encoded at.
    Started {
        width: u32,
        height: u32,
        output: (u32, u32),
        backend: &'static str,
    },
    /// A preview picture is waiting in [`CameraHandle::preview`]. The picture
    /// itself never travels, so at most one of these is ever queued and what the
    /// interface reads is always the newest.
    Preview,
    /// Once a second while the camera runs.
    Stats(CameraStats),
    /// The camera could not be started, or could not carry on; everything is
    /// torn down and the thread stays alive for a later start.
    Failed(String),
    /// The operating system ended the capture — the device was unplugged, the
    /// permission revoked.
    Ended(String),
}

#[derive(Clone)]
pub struct CameraHandle {
    commands: std::sync::mpsc::Sender<CameraCommand>,
    /// The newest preview picture behind [`CameraEvent::Preview`].
    pub preview: Arc<Mailbox<(Arc<Picture>, u64)>>,
}

impl CameraHandle {
    /// Never blocks: the command channel is unbounded, and a thread that has
    /// already exited only costs a log line.
    pub fn send(&self, command: CameraCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("the camera thread is gone, dropping the command");
        }
    }
}

/// Spawns the camera's pipeline thread. The thread exits once every
/// [`CameraHandle`] has been dropped.
pub fn spawn_camera_thread() -> (CameraHandle, async_mpsc::UnboundedReceiver<CameraEvent>) {
    let (commands, requests) = std::sync::mpsc::channel();
    let (events, updates) = async_mpsc::unbounded();
    let preview = Arc::new(Mailbox::new());

    let posted = Arc::clone(&preview);
    let spawned = std::thread::Builder::new()
        .name("vorcall-camera".to_string())
        .spawn(move || run(requests, events, posted));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the camera thread");
    }

    (CameraHandle { commands, preview }, updates)
}

fn run(
    requests: Receiver<CameraCommand>,
    events: async_mpsc::UnboundedSender<CameraEvent>,
    preview: Arc<Mailbox<(Arc<Picture>, u64)>>,
) {
    let mut state = CameraThread {
        events,
        preview,
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

struct CameraThread {
    events: async_mpsc::UnboundedSender<CameraEvent>,
    preview: Arc<Mailbox<(Arc<Picture>, u64)>>,
    pipeline: Option<Pipeline>,
    /// The device being opened on the opener thread, while it is.
    starting: Option<Starting>,
}

/// A [`CameraCommand::Start`] whose device is still being opened, and everything
/// its pipeline needs once it is.
struct Starting {
    /// The device, or why there is none. Dropping this receiver is what cancels
    /// a start: the opener's send then fails, and the capturer it was handing
    /// over is dropped there, which closes the device.
    opened: Receiver<Result<Capturer, Unavailable>>,
    frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
    preset: CameraPreset,
    sender: FrameSender,
    /// A pause that arrived while the device was still opening. The app sends
    /// the camera's first one right behind the start, and a pipeline that never
    /// hears it encodes nothing for the watchers it already has.
    paused: Option<bool>,
}

impl CameraThread {
    fn handle(&mut self, command: CameraCommand) {
        match command {
            CameraCommand::Start {
                request,
                preset,
                sender,
            } => {
                // One device at a time, and the old one has to be closed before
                // another permission dialog opens.
                self.pipeline = None;
                self.starting = None;

                let (frames, events) = async_mpsc::unbounded();
                let (handles, opened) = std::sync::mpsc::channel();
                // `start_camera` blocks on the platform's own permission dialog
                // for as long as the user leaves it up, and this loop has to
                // stay able to read a `Stop` while it does.
                let spawned = std::thread::Builder::new()
                    .name("vorcall-camera-open".to_string())
                    .spawn(move || {
                        let _ = handles.send(start_camera(request, frames));
                    });
                match spawned {
                    Ok(_) => {
                        self.starting = Some(Starting {
                            opened,
                            frames: events,
                            preset,
                            sender,
                            paused: None,
                        });
                    }
                    Err(error) => {
                        let failed = format!("cannot start the camera: {error}");
                        self.emit(CameraEvent::Failed(failed));
                    }
                }
            }
            CameraCommand::SetPaused(paused) => {
                if let Some(pipeline) = self.pipeline.as_mut() {
                    pipeline.video.set_paused(paused);
                } else if let Some(starting) = self.starting.as_mut() {
                    starting.paused = Some(paused);
                }
            }
            CameraCommand::ForceKeyframe => {
                // A device still opening needs nothing: the first unit its
                // pipeline encodes is a keyframe anyway.
                if let Some(pipeline) = self.pipeline.as_mut() {
                    pipeline.video.force_keyframe();
                }
            }
            CameraCommand::Stop => {
                self.pipeline = None;
                self.starting = None;
                // The tile is drawn from the mailbox, and a stopped camera must
                // not leave its own face on the stage.
                self.preview.take();
            }
        }
    }

    /// Takes over a device the opener has finished with. A start this loop has
    /// cancelled since is not here to take it any more, and the capturer is
    /// dropped on the opener thread instead.
    fn collect(&mut self) {
        let Some(starting) = self.starting.take() else {
            return;
        };
        match starting.opened.try_recv() {
            Ok(Ok(capturer)) => {
                let paused = starting.paused;
                let mut pipeline = Pipeline::new(
                    self.events.clone(),
                    Arc::clone(&self.preview),
                    capturer,
                    starting.frames,
                    starting.preset,
                    starting.sender,
                );
                if let Some(paused) = paused {
                    pipeline.video.set_paused(paused);
                }
                self.pipeline = Some(pipeline);
            }
            Ok(Err(error)) => self.emit(CameraEvent::Failed(error.to_string())),
            Err(TryRecvError::Empty) => self.starting = Some(starting),
            Err(TryRecvError::Disconnected) => {
                // Nothing but a panic inside the backend ends that thread
                // without an answer.
                self.emit(CameraEvent::Failed(
                    "the camera could not be started".to_string(),
                ));
            }
        }
    }

    fn pump(&mut self) {
        let Some(pipeline) = self.pipeline.as_mut() else {
            return;
        };
        if !pipeline.pump(Instant::now()) {
            self.pipeline = None;
            self.preview.take();
        }
    }

    fn emit(&self, event: CameraEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for camera events");
        }
    }
}

/// Everything one running camera owns. Built on `Start`, dropped whole on `Stop`
/// or when the device ends, which closes it.
struct Pipeline {
    events: async_mpsc::UnboundedSender<CameraEvent>,
    preview: Arc<Mailbox<(Arc<Picture>, u64)>>,
    /// Only held so the device stays open: it closes the moment it is dropped,
    /// which must happen on this thread.
    capturer: Capturer,
    frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
    video: VideoTrack,
    /// Whether a frame has arrived since the preview was last built.
    preview_fresh: bool,
    next_preview: Instant,
    preview_seq: u64,
    scaled: Vec<u8>,
}

impl Pipeline {
    fn new(
        events: async_mpsc::UnboundedSender<CameraEvent>,
        preview: Arc<Mailbox<(Arc<Picture>, u64)>>,
        capturer: Capturer,
        frames: async_mpsc::UnboundedReceiver<CaptureEvent>,
        preset: CameraPreset,
        sender: FrameSender,
    ) -> Self {
        let now = Instant::now();
        Self {
            events,
            preview,
            capturer,
            frames,
            video: VideoTrack::new(Track::Camera(preset), sender, now),
            preview_fresh: false,
            next_preview: now,
            preview_seq: 0,
            scaled: Vec::new(),
        }
    }

    /// One pass: every picture the device produced since the last one, then the
    /// encode deadline, the preview and the report. `false` once the camera is
    /// over, which is when it has emitted its own last event.
    fn pump(&mut self, now: Instant) -> bool {
        loop {
            match self.frames.try_recv() {
                Ok(CaptureEvent::Started { width, height, .. }) => {
                    if let Err(reason) = self.video.started((width, height)) {
                        self.emit(CameraEvent::Failed(reason));
                        return false;
                    }
                    self.emit(CameraEvent::Started {
                        width,
                        height,
                        output: self.video.output(),
                        backend: self.capturer.backend(),
                    });
                }
                Ok(CaptureEvent::Video(frame)) => {
                    if !self.on_video(frame) {
                        return false;
                    }
                }
                // A camera never captures audio: the microphone is the voice
                // session's business.
                Ok(CaptureEvent::Audio(_)) => {}
                Ok(CaptureEvent::Ended(reason)) => {
                    self.emit(CameraEvent::Ended(reason));
                    return false;
                }
                Err(async_mpsc::TryRecvError::Empty) => break,
                // The backend dropped its sender without a word, which is the
                // same thing as ending.
                Err(async_mpsc::TryRecvError::Closed) => {
                    self.emit(CameraEvent::Ended("the camera stopped".to_string()));
                    return false;
                }
            }
        }

        self.video.encode_tick(now);
        self.preview_tick(now);
        if let Some(report) = self.video.report(now, 0) {
            self.emit(CameraEvent::Stats(CameraStats {
                capture_fps: report.capture_fps,
                encode_fps: report.encode_fps,
                kbps: report.kbps,
                output: report.output,
                keyframes: report.keyframes,
                keyframe_requests: report.keyframe_requests,
                dropped_frames: report.dropped_frames,
                skipped_frames: report.skipped_frames,
                send_failures: report.send_failures,
            }));
        }
        true
    }

    fn on_video(&mut self, frame: VideoFrame) -> bool {
        self.preview_fresh = true;
        if let Err(reason) = self.video.on_video(frame) {
            self.emit(CameraEvent::Failed(reason));
            return false;
        }
        true
    }

    /// Rebuilds the local tile from the newest captured frame. It runs while the
    /// camera is paused as well: nobody watching is no reason to stop showing
    /// the user their own face.
    fn preview_tick(&mut self, now: Instant) {
        if now < self.next_preview {
            return;
        }
        // Missed ticks are not made up for, exactly as for the encode deadline.
        self.next_preview = if now.saturating_duration_since(self.next_preview) >= PREVIEW_INTERVAL
        {
            now + PREVIEW_INTERVAL
        } else {
            self.next_preview + PREVIEW_INTERVAL
        };
        if !self.preview_fresh {
            return;
        }

        let Some(frame) = self.video.latest() else {
            return;
        };
        let size = preview_size((frame.width, frame.height));
        let (pixels, stride) = if size == (frame.width, frame.height) {
            (frame.bgra.as_slice(), frame.stride)
        } else {
            scale_bgra(
                &frame.bgra,
                frame.stride,
                (frame.width, frame.height),
                size,
                &mut self.scaled,
            );
            (self.scaled.as_slice(), size.0 as usize * 4)
        };

        let Some(picture) = bgra_to_i420(pixels, stride, size) else {
            return;
        };
        self.preview_fresh = false;
        self.preview_seq += 1;
        let posted = self.preview.post((Arc::new(picture), self.preview_seq));
        if posted.marker {
            self.emit(CameraEvent::Preview);
        }
    }

    fn emit(&self, event: CameraEvent) {
        if self.events.unbounded_send(event).is_err() {
            tracing::debug!("nobody is listening for camera events");
        }
    }
}

/// `source` fitted inside [`PREVIEW_BOX`], never magnified, both dimensions even
/// so the chroma planes are exactly half.
fn preview_size(source: (u32, u32)) -> (u32, u32) {
    let even = |value: u32| (value & !1).max(2);
    let (width, height) = (source.0.max(1), source.1.max(1));
    if width <= PREVIEW_BOX.0 && height <= PREVIEW_BOX.1 {
        return (even(width), even(height));
    }
    let scale = (f64::from(PREVIEW_BOX.0) / f64::from(width))
        .min(f64::from(PREVIEW_BOX.1) / f64::from(height));
    let fitted = |value: u32| even((value as f64 * scale).round() as u32);
    (fitted(width), fitted(height))
}

/// One tightly packed BGRA frame as the I420 [`Picture`] the stage draws,
/// BT.709 limited range — the very conversion the shader inverts.
///
/// `None` when the slice is too short for the size it claims, or when either
/// dimension is odd: the chroma planes are exactly half of each, and there is
/// no half pixel to average.
fn bgra_to_i420(bgra: &[u8], stride: usize, size: (u32, u32)) -> Option<Picture> {
    let (width, height) = (size.0 as usize, size.1 as usize);
    if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
        return None;
    }
    let row = width * 4;
    if stride < row || bgra.len() < stride * (height - 1) + row {
        return None;
    }

    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let mut y = vec![0u8; width * height];
    let mut u = vec![0u8; chroma_width * chroma_height];
    let mut v = vec![0u8; chroma_width * chroma_height];

    // The luma of one pixel, kept alongside its blue and red so the chroma of
    // the 2x2 block above it can be averaged without reading the frame twice.
    let mut luma = vec![0.0f32; width * 2];
    let mut blue = vec![0.0f32; width * 2];
    let mut red = vec![0.0f32; width * 2];

    for pair in 0..chroma_height {
        for half in 0..2 {
            let line = pair * 2 + half;
            let source = &bgra[line * stride..line * stride + row];
            let target = &mut luma[half * width..(half + 1) * width];
            let blues = &mut blue[half * width..(half + 1) * width];
            let reds = &mut red[half * width..(half + 1) * width];

            for (column, pixel) in source.as_chunks::<4>().0.iter().enumerate() {
                let b = f32::from(pixel[0]) / 255.0;
                let g = f32::from(pixel[1]) / 255.0;
                let r = f32::from(pixel[2]) / 255.0;
                let value = K_R * r + K_G * g + K_B * b;
                target[column] = value;
                blues[column] = b;
                reds[column] = r;
                y[line * width + column] = quantize(Y_MIN + Y_RANGE * value);
            }
        }

        for column in 0..chroma_width {
            let mut cb = 0.0f32;
            let mut cr = 0.0f32;
            for half in 0..2 {
                for step in 0..2 {
                    let at = half * width + column * 2 + step;
                    cb += (blue[at] - luma[at]) / (2.0 * (1.0 - K_B));
                    cr += (red[at] - luma[at]) / (2.0 * (1.0 - K_R));
                }
            }
            let at = pair * chroma_width + column;
            u[at] = quantize(C_MID + C_RANGE * cb / 4.0);
            v[at] = quantize(C_MID + C_RANGE * cr / 4.0);
        }
    }

    Some(Picture {
        width: size.0,
        height: size.1,
        y_stride: width,
        uv_stride: chroma_width,
        y,
        u,
        v,
    })
}

fn quantize(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stage's fragment stage, in Rust: BT.709 limited range, the same
    /// constants `view::stage` formats its WGSL from.
    fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [f32; 3] {
        let luma = (f32::from(y) - Y_MIN) / Y_RANGE;
        let cb = (f32::from(u) - C_MID) / C_RANGE;
        let cr = (f32::from(v) - C_MID) / C_RANGE;
        [
            luma + 2.0 * (1.0 - K_R) * cr,
            luma - 2.0 * (1.0 - K_B) * K_B / K_G * cb - 2.0 * (1.0 - K_R) * K_R / K_G * cr,
            luma + 2.0 * (1.0 - K_B) * cb,
        ]
    }

    /// One BGRA frame of a single colour, tightly packed.
    fn flat(size: (u32, u32), pixel: [u8; 4]) -> Vec<u8> {
        std::iter::repeat_n(pixel, (size.0 * size.1) as usize)
            .flatten()
            .collect()
    }

    #[test]
    fn a_flat_colour_survives_the_round_trip() {
        let size = (4, 4);
        // Blue, green, red, alpha, as BGRA orders them.
        for (pixel, expected) in [
            ([0u8, 0, 0, 255], [0.0f32, 0.0, 0.0]),
            ([255, 255, 255, 255], [1.0, 1.0, 1.0]),
            ([0, 0, 255, 255], [1.0, 0.0, 0.0]),
            ([0, 255, 0, 255], [0.0, 1.0, 0.0]),
            ([255, 0, 0, 255], [0.0, 0.0, 1.0]),
        ] {
            let bgra = flat(size, pixel);
            let picture = bgra_to_i420(&bgra, 16, size).expect("a picture");
            assert_eq!(picture.width, 4);
            assert_eq!(picture.y.len(), 16);
            assert_eq!(picture.u.len(), 4);

            let back = yuv_to_rgb(picture.y[5], picture.u[1], picture.v[1]);
            for (channel, wanted) in back.iter().zip(expected) {
                assert!(
                    (channel - wanted).abs() < 0.02,
                    "{pixel:?} came back as {back:?}"
                );
            }
        }
    }

    /// Black sits at 16 and white at 235 on the luma plane, neutral chroma at
    /// 128: the three numbers BT.709 limited range tabulates.
    #[test]
    fn black_and_white_land_on_the_limited_range_ends() {
        let black = bgra_to_i420(&flat((2, 2), [0, 0, 0, 255]), 8, (2, 2)).expect("a picture");
        assert_eq!(black.y[0], 16);
        assert_eq!(black.u[0], 128);
        assert_eq!(black.v[0], 128);

        let white =
            bgra_to_i420(&flat((2, 2), [255, 255, 255, 255]), 8, (2, 2)).expect("a picture");
        assert_eq!(white.y[0], 235);
        assert_eq!(white.u[0], 128);
        assert_eq!(white.v[0], 128);
    }

    /// A padded stride is what a capture backend normally hands over, and the
    /// padding is never part of the picture.
    #[test]
    fn a_padded_row_is_read_without_its_padding() {
        let size = (2, 2);
        let stride = 16;
        let mut bgra = vec![0u8; stride * 2];
        for line in 0..2 {
            // Two white pixels, then padding left black.
            for pixel in 0..2 {
                let at = line * stride + pixel * 4;
                bgra[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }

        let picture = bgra_to_i420(&bgra, stride, size).expect("a picture");
        assert_eq!(picture.y, vec![235; 4]);
    }

    #[test]
    fn a_short_or_odd_frame_yields_nothing() {
        assert!(bgra_to_i420(&flat((2, 2), [0, 0, 0, 255]), 8, (4, 4)).is_none());
        assert!(bgra_to_i420(&flat((4, 4), [0, 0, 0, 255]), 16, (3, 2)).is_none());
        assert!(bgra_to_i420(&[], 0, (0, 0)).is_none());
    }

    #[test]
    fn a_preview_fits_its_box_keeping_aspect_and_never_grows() {
        assert_eq!(preview_size((1280, 720)), (320, 180));
        assert_eq!(preview_size((640, 480)), (240, 180));
        // Smaller than the box is left alone, only evened off.
        assert_eq!(preview_size((161, 91)), (160, 90));
        assert_eq!(preview_size((0, 0)), (2, 2));
    }
}
