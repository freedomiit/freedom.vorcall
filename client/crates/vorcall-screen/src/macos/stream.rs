//! One `SCStream` on a thread of its own, from the source lookup to the stop.
//!
//! The thread exists because nothing ScreenCaptureKit hands out is `Send` in
//! the bindings: the stream, its filter, its configuration and the output
//! object are created, started, stopped and dropped on this one thread, and
//! only the events leave it. Sample callbacks arrive on the stream's own
//! serial dispatch queue and touch nothing but the output object's ivars,
//! which are all thread-safe.

use std::mem::offset_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, CMTimeFlags,
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
            if !self.forwarding() {
                return;
            }
            if kind == SCStreamOutputType::Screen {
                self.video(sample);
            } else if kind == SCStreamOutputType::Audio {
                self.audio(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            if self.forwarding() {
                self.send(CaptureEvent::Ended(
                    error.localizedDescription().to_string(),
                ));
            }
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
        let now = Instant::now();
        if !self.due(now) {
            return;
        }
        // SAFETY: reads the sample's image buffer, which a screen sample has.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        if let Some(frame) = copy_bgra(&image, now) {
            self.send(CaptureEvent::Video(frame));
        }
    }

    fn audio(&self, sample: &CMSampleBuffer) {
        if let Some(chunk) = read_audio(sample) {
            self.send(CaptureEvent::Audio(chunk));
        }
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
/// the stream first, then the object it called back into, then their queue.
struct Live {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}

fn run(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
    stop: &mpsc::Receiver<()>,
) {
    let live = match bring_up(request, events) {
        Ok(live) => live,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    // Parks until the Capturer is dropped; a closed channel counts too, so a
    // caller that gave up on the handshake still gets the stream torn down.
    let _ = stop.recv();
    shut_down(&live.stream);
    drop(live);
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

    let output = Output::new(Ivars {
        events,
        started: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        pace_floor: request.fps.interval().mul_f64(PACE_FLOOR),
        last_frame: Mutex::new(None),
    });
    let queue = DispatchQueue::new("vorcall-screen-samples", DispatchQueueAttr::SERIAL);

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
        &queue,
        SCStreamOutputType::Screen,
        "video",
    )?;
    if request.audio {
        attach(&stream, &output, &queue, SCStreamOutputType::Audio, "audio")?;
    }

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
        _output: output,
        _queue: queue,
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

/// Float32 PCM interleaved, whichever way ScreenCaptureKit laid it out. The
/// configuration asks for 48 kHz stereo and gets Float32 for it; any other
/// format is dropped rather than misread.
fn read_audio(sample: &CMSampleBuffer) -> Option<AudioChunk> {
    // SAFETY: reads the sample's format description.
    let description = unsafe { sample.format_description() }?;
    // SAFETY: the description belongs to the live sample; anything but audio
    // yields null, checked next.
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description) };
    if asbd.is_null() {
        return None;
    }
    // SAFETY: non-null, points into the description, and is copied out once.
    let asbd: AudioStreamBasicDescription = unsafe { asbd.read() };
    if asbd.mFormatID != kAudioFormatLinearPCM
        || (asbd.mFormatFlags & kAudioFormatFlagIsFloat) == 0
        || asbd.mBitsPerChannel != 32
    {
        return None;
    }
    let channels = asbd.mChannelsPerFrame as usize;
    if channels == 0 {
        return None;
    }
    let planar = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;

    let mut storage = [0u64; AUDIO_LIST_WORDS];
    let capacity = size_of::<[u64; AUDIO_LIST_WORDS]>();
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    // SAFETY: `list` is writable for `capacity` bytes and outlives the call.
    // No block buffer is asked for (the out-pointer is nullable), so the data
    // pointers written into the list stay valid exactly as long as `sample`:
    // the rest of this callback, during which they are copied out.
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            ptr::null_mut(),
            list,
            capacity,
            None,
            None,
            0,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the call filled the list; its buffer count sits at the start
    // and the buffers follow it contiguously, all inside `storage`.
    let count = unsafe { (*list).mNumberBuffers } as usize;
    let buffers_at = offset_of!(AudioBufferList, mBuffers);
    if count == 0 || buffers_at + count * size_of::<AudioBuffer>() > capacity {
        return None;
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

    let interleaved = if planar {
        if buffers.len() < channels {
            return None;
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

    Some(AudioChunk {
        sample_rate: asbd.mSampleRate as u32,
        channels: channels as u16,
        interleaved,
    })
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
    // (the caller checked the format) that live as long as the sample the
    // list came from, which outlives this borrow.
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
