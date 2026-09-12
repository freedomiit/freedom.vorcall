//! One `SCStream` on a thread of its own, from the source lookup to the stop.
//!
//! The thread exists because nothing ScreenCaptureKit hands out is `Send` in
//! the bindings: the stream, its filter, its configuration and the output
//! object are created, started, stopped and dropped on this one thread, inside
//! one autorelease pool, and only the events leave it. Sample callbacks arrive
//! on two serial dispatch queues, one per output so that audio never waits
//! behind a frame copy, and touch nothing but the output object's ivars, which
//! are all thread-safe.

use std::fmt;
use std::mem::offset_of;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    CMTimeFlags, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamErrorCode, SCStreamErrorDomain, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType,
};

use super::content::{self, Target};
use super::permission;
use crate::{
    AudioChunk, AudioMode, CaptureEvent, CaptureRequest, Capturer, Stop, Unavailable, VideoFrame,
};

/// How long `startCaptureWithCompletionHandler` may take.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the stop waits for ScreenCaptureKit's acknowledgement; what is
/// left of the 500 ms budget goes to joining the thread.
const STOP_TIMEOUT: Duration = Duration::from_millis(400);

/// A backstop over the content and start timeouts, so the caller never waits
/// forever on a thread that has stopped answering.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// ScreenCaptureKit's own timing jitters around `minimumFrameInterval`, so a
/// frame only counts as an extra when it comes this early.
const PACE_FLOOR: f64 = 0.75;

/// How often the running counters go to the log.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// What the user is told when the stream ends from the macOS side.
const USER_STOPPED: &str = "the share was stopped from the macOS menu bar";
const SYSTEM_STOPPED: &str = "macOS stopped the share (Screen Recording permission changed, or \
                              the system alert was dismissed)";

const QUEUE_DEPTH: isize = 3;
const AUDIO_SAMPLE_RATE: isize = 48_000;
const AUDIO_CHANNELS: isize = 2;

/// `u64`s of room for the `AudioBufferList` a sample is unpacked into: 8-byte
/// aligned for the data pointers, and enough for eleven buffers when stereo
/// comes as one buffer per channel.
const AUDIO_LIST_WORDS: usize = 24;

/// One sample's attachments: a dictionary per sample, keyed by frame-info
/// strings.
type Attachments = NSArray<NSDictionary<NSString, AnyObject>>;

struct Ivars {
    events: UnboundedSender<CaptureEvent>,
    /// Frames are held back until `Started` went out, so it is always first.
    started: AtomicBool,
    /// Set once a send fails: the consumer is gone and nothing else matters.
    closed: AtomicBool,
    /// The least time between two forwarded frames.
    pace_floor: Duration,
    last_frame: Mutex<Option<Instant>>,
    /// What the log lines date themselves from.
    started_at: Instant,
    last_report: Mutex<Instant>,
    /// What ScreenCaptureKit delivered and what of it went out; written only
    /// on the sample queues, read by the log lines.
    video_samples: AtomicU64,
    video_complete: AtomicU64,
    video_forwarded: AtomicU64,
    audio_samples: AtomicU64,
    audio_forwarded: AtomicU64,
    audio_rejected: AtomicU64,
    /// Set once the first audio sample's layout went to the log.
    audio_described: AtomicBool,
    /// Set once the first dropped audio sample went to the log.
    audio_drop_described: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Output has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "VorcallStreamOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            let ivars = self.ivars();
            if kind == SCStreamOutputType::Screen {
                ivars.video_samples.fetch_add(1, Ordering::Relaxed);
            } else if kind == SCStreamOutputType::Audio {
                ivars.audio_samples.fetch_add(1, Ordering::Relaxed);
            } else {
                return;
            }
            self.report_progress();
            if !self.forwarding() {
                return;
            }
            if kind == SCStreamOutputType::Screen {
                self.video(sample);
            } else {
                self.audio(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let domain = error.domain();
            let code = error.code();
            let description = error.localizedDescription().to_string();
            tracing::warn!(
                domain = %*domain,
                code,
                description = %description,
                "ScreenCaptureKit stopped the stream"
            );
            if self.forwarding() {
                let reason = stop_reason(&domain, code, description);
                self.send(CaptureEvent::Ended(reason));
            }
        }

        #[unsafe(method(streamDidBecomeActive:))]
        fn did_become_active(&self, _stream: &SCStream) {
            tracing::debug!("ScreenCaptureKit stream became active");
        }

        #[unsafe(method(streamDidBecomeInactive:))]
        fn did_become_inactive(&self, _stream: &SCStream) {
            tracing::debug!("ScreenCaptureKit stream became inactive");
        }
    }
);

impl Output {
    fn new(ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's `init` takes no arguments; the ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    /// Lets the callbacks through, with `started` going out first.
    fn arm(&self, started: CaptureEvent) {
        self.send(started);
        self.ivars().started.store(true, Ordering::Release);
    }

    fn forwarding(&self) -> bool {
        let ivars = self.ivars();
        ivars.started.load(Ordering::Acquire) && !ivars.closed.load(Ordering::Acquire)
    }

    fn send(&self, event: CaptureEvent) {
        if self.ivars().events.unbounded_send(event).is_err() {
            self.ivars().closed.store(true, Ordering::Release);
        }
    }

    fn video(&self, sample: &CMSampleBuffer) {
        if !is_complete(sample) {
            return;
        }
        let ivars = self.ivars();
        ivars.video_complete.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        if !self.due(now) {
            return;
        }
        // SAFETY: reads the sample's image buffer, which a screen sample has.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        if let Some(frame) = copy_bgra(&image, now) {
            ivars.video_forwarded.fetch_add(1, Ordering::Relaxed);
            self.send(CaptureEvent::Video(frame));
        }
    }

    fn audio(&self, sample: &CMSampleBuffer) {
        let ivars = self.ivars();
        match read_audio(sample) {
            Ok((chunk, layout)) => {
                if !ivars.audio_described.swap(true, Ordering::Relaxed) {
                    tracing::debug!(%layout, "ScreenCaptureKit audio format");
                }
                ivars.audio_forwarded.fetch_add(1, Ordering::Relaxed);
                self.send(CaptureEvent::Audio(chunk));
            }
            Err(rejected) => {
                ivars.audio_rejected.fetch_add(1, Ordering::Relaxed);
                if !ivars.audio_drop_described.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        reason = %rejected.reason,
                        layout = %rejected.layout,
                        "ScreenCaptureKit audio sample dropped"
                    );
                }
            }
        }
    }

    /// The running counters, once every `REPORT_INTERVAL`.
    fn report_progress(&self) {
        let now = Instant::now();
        let mut last = self
            .ivars()
            .last_report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if now.duration_since(*last) < REPORT_INTERVAL {
            return;
        }
        *last = now;
        drop(last);
        self.report("ScreenCaptureKit stream running");
    }

    /// Every counter on one line, `message` being what the line says.
    fn report(&self, message: &str) {
        let ivars = self.ivars();
        tracing::info!(
            elapsed_s = ivars.started_at.elapsed().as_secs(),
            video_samples = ivars.video_samples.load(Ordering::Relaxed),
            video_complete = ivars.video_complete.load(Ordering::Relaxed),
            video_forwarded = ivars.video_forwarded.load(Ordering::Relaxed),
            audio_samples = ivars.audio_samples.load(Ordering::Relaxed),
            audio_forwarded = ivars.audio_forwarded.load(Ordering::Relaxed),
            audio_rejected = ivars.audio_rejected.load(Ordering::Relaxed),
            "{message}"
        );
    }

    /// Whether a frame at `now` keeps the rate at or under the requested one,
    /// counting it when it does.
    fn due(&self, now: Instant) -> bool {
        let mut last = self
            .ivars()
            .last_frame
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let due = last.is_none_or(|last| now.duration_since(last) >= self.ivars().pace_floor);
        if due {
            *last = Some(now);
        }
        due
    }
}

pub(super) fn start(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Unavailable>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("vorcall-screen".to_string())
        .spawn(move || run(request, events, &ready_tx, &stop_rx))
        .map_err(|err| Unavailable::Failed(format!("cannot start the capture thread: {err}")))?;

    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(())) => Ok(Capturer::new(
            "screencapturekit",
            Box::new(Handle {
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
        )),
        Ok(Err(err)) => {
            // The thread returns right after reporting, so this is immediate.
            let _ = thread.join();
            Err(err)
        }
        Err(_) => {
            // Dropping the sender is the stop signal, so whatever the thread
            // still brings up is torn down again on its own.
            drop(stop_tx);
            Err(Unavailable::Failed(
                "the screen capture did not come up".to_string(),
            ))
        }
    }
}

/// Everything the thread keeps alive while capturing, dropped in field order:
/// the stream first, then the object it called back into, then the queues the
/// callbacks ran on.
struct Live {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _video_queue: DispatchRetained<DispatchQueue>,
    _audio_queue: Option<DispatchRetained<DispatchQueue>>,
}

fn run(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
    stop: &mpsc::Receiver<()>,
) {
    // The thread's one pool: ScreenCaptureKit's getters autorelease what they
    // return, and on a thread without a pool that is a leak.
    autoreleasepool(|_| {
        let live = match bring_up(request, events) {
            Ok(live) => live,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let _ = ready.send(Ok(()));

        // Parks until the Capturer is dropped; a closed channel counts too,
        // so a caller that gave up on the handshake still gets the stream
        // torn down.
        let _ = stop.recv();
        shut_down(&live.stream);
        live.output.report("ScreenCaptureKit stream stopped");
        drop(live);
    });
}

fn bring_up(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Live, Unavailable> {
    let Some(source) = request.source.as_ref() else {
        return Err(Unavailable::Failed("no source".to_string()));
    };
    let content = content::fetch()?;
    let target = content::find(&content, source)?;
    let (width, height) = fit(target.size(), request.max_size);

    // SAFETY: init calls on freshly allocated filters with live arguments.
    let filter = unsafe {
        match &target {
            Target::Display { display, .. } => SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display,
                &NSArray::new(),
            ),
            Target::Window { window, .. } => {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
            }
        }
    };

    let fps = request.fps.hz();
    // SAFETY: setters on a configuration this thread owns.
    let configuration = unsafe {
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        configuration.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: fps as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
        configuration.setShowsCursor(request.cursor);
        configuration.setQueueDepth(QUEUE_DEPTH);
        if matches!(target, Target::Window { .. }) {
            // A display always fills the configured size; a window is only
            // scaled into it when asked.
            configuration.setScalesToFit(true);
        }
        if request.audio {
            configuration.setCapturesAudio(true);
            configuration.setExcludesCurrentProcessAudio(true);
            configuration.setSampleRate(AUDIO_SAMPLE_RATE);
            configuration.setChannelCount(AUDIO_CHANNELS);
        }
        configuration
    };

    let now = Instant::now();
    let output = Output::new(Ivars {
        events,
        started: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        pace_floor: request.fps.interval().mul_f64(PACE_FLOOR),
        last_frame: Mutex::new(None),
        started_at: now,
        last_report: Mutex::new(now),
        video_samples: AtomicU64::new(0),
        video_complete: AtomicU64::new(0),
        video_forwarded: AtomicU64::new(0),
        audio_samples: AtomicU64::new(0),
        audio_forwarded: AtomicU64::new(0),
        audio_rejected: AtomicU64::new(0),
        audio_described: AtomicBool::new(false),
        audio_drop_described: AtomicBool::new(false),
    });
    // A queue per output: on one shared queue every audio sample would wait
    // behind the synchronous copy of the frame ahead of it.
    let video_queue = DispatchQueue::new("vorcall-screen-samples", DispatchQueueAttr::SERIAL);

    // SAFETY: the filter, configuration and delegate are live, and `Live`
    // keeps the delegate alive for as long as the stream.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    attach(
        &stream,
        &output,
        &video_queue,
        SCStreamOutputType::Screen,
        "video",
    )?;
    let audio_queue = if request.audio {
        let queue = DispatchQueue::new("vorcall-screen-audio", DispatchQueueAttr::SERIAL);
        attach(&stream, &output, &queue, SCStreamOutputType::Audio, "audio")?;
        Some(queue)
    } else {
        None
    };

    start_capture(&stream)?;
    output.arm(CaptureEvent::Started {
        width,
        height,
        audio: request.audio.then_some(AudioMode::Excluded),
    });
    tracing::debug!(
        width,
        height,
        fps,
        audio = request.audio,
        "ScreenCaptureKit stream started"
    );

    Ok(Live {
        stream,
        output,
        _video_queue: video_queue,
        _audio_queue: audio_queue,
    })
}

fn attach(
    stream: &SCStream,
    output: &Output,
    queue: &DispatchQueue,
    kind: SCStreamOutputType,
    what: &str,
) -> Result<(), Unavailable> {
    // SAFETY: the output object and the queue are live and, through `Live`,
    // outlive the stream.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(output),
            kind,
            Some(queue),
        )
    }
    .map_err(|err| {
        Unavailable::Failed(format!(
            "cannot attach the {what} output: {}",
            err.localizedDescription()
        ))
    })
}

fn start_capture(stream: &SCStream) -> Result<(), Unavailable> {
    let (tx, rx) = mpsc::channel::<Result<(), Unavailable>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(start_outcome(error));
    });
    // SAFETY: the block has the completion handler's signature.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    match rx.recv_timeout(START_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_) => Err(Unavailable::Failed(
            "ScreenCaptureKit did not start the stream in time".to_string(),
        )),
    }
}

/// A declined prompt is the one start failure the user can fix.
fn start_outcome(error: *mut NSError) -> Result<(), Unavailable> {
    // SAFETY: null means success; a live error stays valid for the duration
    // of the handler that received it.
    let Some(error) = (unsafe { Retained::retain(error) }) else {
        return Ok(());
    };
    // SAFETY: reading a framework extern static.
    let domain = unsafe { SCStreamErrorDomain };
    if *error.domain() == *domain && error.code() == SCStreamErrorCode::UserDeclined.0 {
        return Err(Unavailable::PermissionDenied(permission::GRANT.to_string()));
    }
    Err(Unavailable::Failed(
        error.localizedDescription().to_string(),
    ))
}

/// What the user is told when ScreenCaptureKit stops the stream on its own.
/// Only the framework's own domain has codes worth reading; anything else is
/// passed on as it came, code and all.
fn stop_reason(domain: &NSString, code: isize, description: String) -> String {
    // SAFETY: reading a framework extern static.
    let own = unsafe { SCStreamErrorDomain };
    if *domain != *own {
        return format!("{description} (code {code})");
    }
    match SCStreamErrorCode(code) {
        SCStreamErrorCode::UserStopped => USER_STOPPED.to_string(),
        SCStreamErrorCode::SystemStoppedStream => SYSTEM_STOPPED.to_string(),
        SCStreamErrorCode::UserDeclined => permission::GRANT.to_string(),
        SCStreamErrorCode::FailedToStartAudioCapture
        | SCStreamErrorCode::FailedToStopAudioCapture => {
            format!("ScreenCaptureKit audio failed: {description}")
        }
        _ => format!("{description} (code {code})"),
    }
}

fn shut_down(stream: &SCStream) {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send((!error.is_null()).then(|| content::describe(error)));
    });
    // SAFETY: the block has the completion handler's signature.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };

    match rx.recv_timeout(STOP_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(description)) => {
            tracing::debug!(error = %description, "ScreenCaptureKit reported an error on stop");
        }
        Err(_) => tracing::debug!("ScreenCaptureKit did not acknowledge the stop in time"),
    }
}

struct Handle {
    /// Dropping it is the stop signal.
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            // Bounded by the thread's own stop wait.
            let _ = thread.join();
        }
    }
}

/// ScreenCaptureKit also delivers samples while nothing changed (idle, blank,
/// suspended); only a complete one carries new pixels.
fn is_complete(sample: &CMSampleBuffer) -> bool {
    // SAFETY: reads the attachments the framework set; `false` never creates
    // any.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return false;
    };
    // SAFETY: CFArray and NSArray are toll-free bridged, and a sample's
    // attachment array holds one string-keyed dictionary per sample.
    let attachments: &Attachments = unsafe { &*(&*attachments as *const _ as *const Attachments) };
    let Some(first) = attachments.firstObject() else {
        return false;
    };
    // SAFETY: reading a framework extern static.
    let key = unsafe { SCStreamFrameInfoStatus };
    let Some(status) = first.objectForKey(key) else {
        return false;
    };
    status
        .downcast_ref::<NSNumber>()
        .is_some_and(|status| SCFrameStatus(status.integerValue()) == SCFrameStatus::Complete)
}

/// The frame's pixels, tightly packed. The configuration asks for 32BGRA, so
/// anything else is dropped rather than misread.
fn copy_bgra(image: &CVPixelBuffer, captured: Instant) -> Option<VideoFrame> {
    if CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA
        || CVPixelBufferIsPlanar(image)
    {
        return None;
    }
    // SAFETY: the buffer lives for the callback, and the lock is released
    // below before it goes away.
    if unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) }
        != kCVReturnSuccess
    {
        return None;
    }

    let width = CVPixelBufferGetWidth(image);
    let height = CVPixelBufferGetHeight(image);
    let stride = CVPixelBufferGetBytesPerRow(image);
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>().cast_const();
    let row_bytes = width * 4;
    let frame = if base.is_null() || stride < row_bytes {
        None
    } else {
        let mut bgra = Vec::with_capacity(row_bytes * height);
        for row in 0..height {
            // SAFETY: the base address is locked for reading with `stride`
            // bytes mapped per row over `height` rows, and each copy stays
            // within the first `row_bytes` of its row.
            let source = unsafe { std::slice::from_raw_parts(base.add(row * stride), row_bytes) };
            bgra.extend_from_slice(source);
        }
        Some(VideoFrame {
            width: width as u32,
            height: height as u32,
            stride: row_bytes,
            bgra,
            captured,
        })
    };

    // SAFETY: matches the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// How ScreenCaptureKit laid an audio sample out, as far as it got read; what
/// the log lines say of a sample.
#[derive(Clone, Copy, Default)]
struct AudioLayout {
    asbd: Option<AudioStreamBasicDescription>,
    /// Buffers in the sample's list, once it has been unpacked.
    buffers: Option<u32>,
}

impl fmt::Display for AudioLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(asbd) = &self.asbd else {
            return f.write_str("unknown");
        };
        write!(
            f,
            "format_id={} format_flags={:#x} bits_per_channel={} channels_per_frame={} \
             sample_rate={} bytes_per_frame={} planar={}",
            fourcc(asbd.mFormatID),
            asbd.mFormatFlags,
            asbd.mBitsPerChannel,
            asbd.mChannelsPerFrame,
            asbd.mSampleRate,
            asbd.mBytesPerFrame,
            planar(asbd),
        )?;
        match self.buffers {
            Some(buffers) => write!(f, " buffers={buffers}"),
            None => Ok(()),
        }
    }
}

/// Why an audio sample was dropped, with as much of its layout as was read.
struct Rejected {
    layout: AudioLayout,
    reason: String,
}

impl Rejected {
    fn new(layout: AudioLayout, reason: impl Into<String>) -> Self {
        Self {
            layout,
            reason: reason.into(),
        }
    }
}

/// Float32 PCM interleaved, whichever way ScreenCaptureKit laid it out, with
/// the layout it came in. The configuration asks for 48 kHz stereo and gets
/// Float32 for it; any other format is dropped rather than misread.
fn read_audio(sample: &CMSampleBuffer) -> Result<(AudioChunk, AudioLayout), Rejected> {
    let mut layout = AudioLayout::default();
    // SAFETY: reads the sample's format description.
    let Some(description) = (unsafe { sample.format_description() }) else {
        return Err(Rejected::new(layout, "no format description"));
    };
    // SAFETY: the description belongs to the live sample; anything but audio
    // yields null, checked next.
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description) };
    if asbd.is_null() {
        return Err(Rejected::new(layout, "not an audio format"));
    }
    // SAFETY: non-null, points into the description, and is copied out once.
    let asbd: AudioStreamBasicDescription = unsafe { asbd.read() };
    layout.asbd = Some(asbd);
    if asbd.mFormatID != kAudioFormatLinearPCM
        || (asbd.mFormatFlags & kAudioFormatFlagIsFloat) == 0
        || asbd.mBitsPerChannel != 32
    {
        return Err(Rejected::new(layout, "not Float32 linear PCM"));
    }
    let channels = asbd.mChannelsPerFrame as usize;
    if channels == 0 {
        return Err(Rejected::new(layout, "no channels"));
    }

    let mut storage = [0u64; AUDIO_LIST_WORDS];
    let capacity = size_of::<[u64; AUDIO_LIST_WORDS]>();
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: `list` is writable for `capacity` bytes and `block` for one
    // pointer, and both outlive the call.
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            ptr::null_mut(),
            list,
            capacity,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        )
    };
    if status != 0 {
        return Err(Rejected::new(
            layout,
            format!("CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer returned {status}"),
        ));
    }
    // SAFETY: a zero status hands over a block buffer carrying one retain
    // that is the caller's to release (the Create rule). The data pointers
    // written into the list point into it, so it is held until this function
    // returns, after the samples have been copied out, and released exactly
    // once by the drop.
    let _block = NonNull::new(block).map(|block| unsafe { CFRetained::from_raw(block) });
    // SAFETY: the call filled the list; its buffer count sits at the start
    // and the buffers follow it contiguously, all inside `storage`.
    let count = unsafe { (*list).mNumberBuffers };
    layout.buffers = Some(count);
    let count = count as usize;
    let buffers_at = offset_of!(AudioBufferList, mBuffers);
    if count == 0 {
        return Err(Rejected::new(layout, "empty buffer list"));
    }
    if buffers_at + count * size_of::<AudioBuffer>() > capacity {
        return Err(Rejected::new(layout, "too many buffers"));
    }
    // SAFETY: bounds checked just above, and `mBuffers` is 8-byte aligned
    // inside a `u64` array.
    let buffers = unsafe {
        std::slice::from_raw_parts(
            storage
                .as_ptr()
                .cast::<u8>()
                .add(buffers_at)
                .cast::<AudioBuffer>(),
            count,
        )
    };

    let interleaved = if planar(&asbd) {
        if buffers.len() < channels {
            return Err(Rejected::new(layout, "fewer buffers than channels"));
        }
        let planes: Vec<&[f32]> = buffers[..channels].iter().map(samples).collect();
        let frames = planes.iter().map(|plane| plane.len()).min().unwrap_or(0);
        let mut interleaved = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            for plane in &planes {
                interleaved.push(plane[frame]);
            }
        }
        interleaved
    } else {
        samples(&buffers[0]).to_vec()
    };

    Ok((
        AudioChunk {
            sample_rate: asbd.mSampleRate as u32,
            channels: channels as u16,
            interleaved,
        },
        layout,
    ))
}

/// Whether the samples come one buffer per channel.
fn planar(asbd: &AudioStreamBasicDescription) -> bool {
    (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
}

/// A four-character code as the text it spells when it spells one, as hex
/// otherwise.
fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(u8::is_ascii_graphic) {
        bytes.map(char::from).into_iter().collect()
    } else {
        format!("{code:#010x}")
    }
}

/// The Float32 samples one buffer points at; empty when there are none or
/// they sit at an address `f32` cannot be read from.
fn samples(buffer: &AudioBuffer) -> &[f32] {
    let data = buffer.mData.cast::<f32>().cast_const();
    if data.is_null() || !data.is_aligned() {
        return &[];
    }
    let len = buffer.mDataByteSize as usize / size_of::<f32>();
    // SAFETY: the buffer describes `mDataByteSize` bytes of Float32 samples
    // (the caller checked the format) that live as long as the block buffer
    // the list came from, which the caller holds past this borrow.
    unsafe { std::slice::from_raw_parts(data, len) }
}

/// `source` (in pixels) inside `max_size` keeping the aspect ratio, never
/// upscaling; both dimensions even.
fn fit(source: (u32, u32), max_size: Option<(u32, u32)>) -> (u32, u32) {
    let (source_width, source_height) = (source.0.max(1), source.1.max(1));
    let (width, height) = match max_size {
        Some((box_width, box_height)) if source_width > box_width || source_height > box_height => {
            let by_width = (box_width, scale(source_height, box_width, source_width));
            if by_width.1 <= box_height {
                by_width
            } else {
                (scale(source_width, box_height, source_height), box_height)
            }
        }
        _ => (source_width, source_height),
    };
    (content::even(width), content::even(height))
}

/// `value * numerator / denominator` without overflowing at 4K.
fn scale(value: u32, numerator: u32, denominator: u32) -> u32 {
    (u64::from(value) * u64::from(numerator) / u64::from(denominator.max(1))) as u32
}
