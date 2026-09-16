//! macOS camera: an `AVCaptureSession` on a thread of its own.
//!
//! The shape is [`super::stream`]'s, and for the same reason: nothing
//! AVFoundation hands out is `Send` in the bindings, so the session, its input,
//! its output and the delegate are created, started, stopped and dropped on
//! this one thread, inside one autorelease pool, and only the events leave it.
//! Samples arrive on a serial dispatch queue of their own and touch nothing but
//! the delegate's ivars, which are all thread-safe.
//!
//! The output asks for `kCVPixelFormatType_32BGRA`, which is what lets this
//! module reuse the pixel-buffer copy the ScreenCaptureKit path already has.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceType,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureOutput, AVCaptureSession,
    AVCaptureSessionPreset, AVCaptureSessionPreset640x480, AVCaptureSessionPreset1280x720,
    AVCaptureSessionPresetHigh, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaType, AVMediaTypeVideo,
};
use objc2_core_foundation::CFString;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use super::stream::copy_bgra;
use crate::camera::{CameraCapabilities, CameraRequest, CameraSource};
use crate::{CaptureEvent, Capturer, Stop, Unavailable};

const BACKEND: &str = "avfoundation";

/// How long the camera's permission dialog may keep the user.
const ACCESS_TIMEOUT: Duration = Duration::from_secs(120);

/// A backstop over bringing the session up, so the caller never waits forever
/// on a thread that has stopped answering.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a camera has to hand over its first frame. Only a frame says what
/// size the device settled on, so a start is not finished until one arrives.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_POLL: Duration = Duration::from_millis(20);

/// How often the thread looks up from its park to see whether the camera is
/// still delivering.
const WATCHDOG_STEP: Duration = Duration::from_millis(250);

/// A camera that has handed over nothing for this long is gone — unplugged, or
/// taken over by something else. AVFoundation says so by simply going quiet.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// AVFoundation's own timing jitters around the frame interval, so a frame only
/// counts as an extra when it comes this early.
const PACE_FLOOR: f64 = 0.75;

const GRANT: &str = "Allow Vorcall to use the camera in System Settings › Privacy & Security › \
                     Camera, then relaunch Vorcall";

struct Ivars {
    events: UnboundedSender<CaptureEvent>,
    /// Set once a send fails: the consumer is gone and nothing else matters.
    closed: AtomicBool,
    /// Set once `Started` has gone out, which the first frame does — only the
    /// frame itself knows the size the device settled on.
    started: AtomicBool,
    /// The least time between two forwarded frames.
    pace_floor: Duration,
    last_frame: Mutex<Option<Instant>>,
    delivered: AtomicU64,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Output has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "VorcallCameraOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Output {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.video(sample);
        }
    }
);

impl Output {
    fn new(ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's `init` takes no arguments; the ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: CaptureEvent) {
        if self.ivars().events.unbounded_send(event).is_err() {
            self.ivars().closed.store(true, Ordering::Release);
        }
    }

    fn video(&self, sample: &CMSampleBuffer) {
        let ivars = self.ivars();
        if ivars.closed.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if !self.due(now) {
            return;
        }
        // SAFETY: reads the sample's image buffer, which a video sample has.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let Some(frame) = copy_bgra(&image, now) else {
            return;
        };

        // The device's real size is only knowable from a frame it delivered, so
        // `Started` rides on the first one — always before it, never twice.
        if !ivars.started.swap(true, Ordering::AcqRel) {
            self.send(CaptureEvent::Started {
                width: frame.width,
                height: frame.height,
                audio: None,
            });
            tracing::debug!(
                width = frame.width,
                height = frame.height,
                "the camera delivered its first frame"
            );
        }
        ivars.delivered.fetch_add(1, Ordering::Relaxed);
        self.send(CaptureEvent::Video(frame));
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

    /// Whether a frame has gone out, which is also whether `Started` has.
    fn delivering(&self) -> bool {
        self.ivars().delivered.load(Ordering::Relaxed) > 0
    }

    /// Whether the camera has gone quiet for longer than [`STALL_TIMEOUT`]. A
    /// session that has not delivered anything yet is still starting up, and
    /// gets the same grace from the moment it was built.
    fn stalled(&self, since: Instant) -> bool {
        let last = self
            .ivars()
            .last_frame
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        last.unwrap_or(since).elapsed() > STALL_TIMEOUT
    }
}

pub(crate) fn cameras() -> Vec<CameraSource> {
    autoreleasepool(|_| {
        // SAFETY: reads framework statics and calls class methods that need no
        // permission; a camera nobody may use is still listed.
        unsafe {
            discovered()
                .iter()
                .map(|device| CameraSource {
                    id: device.uniqueID().to_string(),
                    name: device.localizedName().to_string(),
                })
                .collect()
        }
    })
}

pub(crate) fn capabilities() -> CameraCapabilities {
    CameraCapabilities {
        backend: BACKEND,
        available: true,
        enumerates: true,
    }
}

pub(crate) fn start(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    authorise()?;

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Unavailable>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("vorcall-camera".to_string())
        .spawn(move || run(request, events, &ready_tx, &stop_rx))
        .map_err(|error| Unavailable::Failed(format!("cannot start the camera thread: {error}")))?;

    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(())) => Ok(Capturer::new(
            BACKEND,
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
                "the camera did not come up".to_string(),
            ))
        }
    }
}

/// The cameras this Mac has: its own, and anything plugged into it.
///
/// `AVCaptureDeviceTypeExternalUnknown` rather than its modern spelling, which
/// only exists from macOS 14 and would not resolve at load time on the 13 this
/// bundle still supports. Apple kept the two the same value, so a USB webcam
/// answers to this one on both.
///
/// # Safety
///
/// Reads AVFoundation's framework statics.
#[allow(deprecated)]
unsafe fn discovered() -> Retained<NSArray<AVCaptureDevice>> {
    unsafe {
        let kinds: Retained<NSArray<AVCaptureDeviceType>> = NSArray::from_slice(&[
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            objc2_av_foundation::AVCaptureDeviceTypeExternalUnknown,
        ]);
        let session =
            AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                &kinds,
                AVMediaTypeVideo,
                AVCaptureDevicePosition::Unspecified,
            );
        session.devices()
    }
}

/// AVFoundation's `AVMediaTypeVideo`, which the bindings expose as an
/// `Option` because the framework header does not mark it non-null. `None`
/// would mean AVFoundation itself did not load, which is reported like any
/// other reason the camera cannot be used rather than panicking on the
/// capture thread.
///
/// # Safety
///
/// Reads a framework static.
unsafe fn video_media_type() -> Result<&'static AVMediaType, Unavailable> {
    unsafe { AVMediaTypeVideo }
        .ok_or_else(|| Unavailable::Unsupported("AVFoundation has no video media type".to_string()))
}

/// Makes sure the camera may be used at all, asking the user once when macOS
/// has never asked on Vorcall's behalf.
fn authorise() -> Result<(), Unavailable> {
    // SAFETY: reads a framework static and a class method.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(video_media_type()?) };
    match status {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::NotDetermined => request_access(),
        _ => Err(Unavailable::PermissionDenied(GRANT.to_string())),
    }
}

fn request_access() -> Result<(), Unavailable> {
    let (tx, rx) = mpsc::channel::<bool>();
    let handler = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: the block has the completion handler's signature, and the call
    // itself returns at once — the answer arrives on some other queue.
    unsafe {
        let media_type = video_media_type()?;
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
    }

    match rx.recv_timeout(ACCESS_TIMEOUT) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Unavailable::PermissionDenied(GRANT.to_string())),
        Err(_) => Err(Unavailable::PermissionDenied(
            "the camera permission dialog was not answered".to_string(),
        )),
    }
}

/// Everything the thread keeps alive while capturing, dropped in field order:
/// the session first, then the object it called back into, then the queue the
/// callbacks ran on.
struct Live {
    session: Retained<AVCaptureSession>,
    output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}

fn run(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
    stop: &mpsc::Receiver<()>,
) {
    // The thread's one pool: AVFoundation's getters autorelease what they
    // return, and on a thread without a pool that is a leak.
    autoreleasepool(|_| {
        let started = Instant::now();
        let live = match bring_up(&request, events) {
            Ok(live) => live,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        // Answered only once a frame has been through, because `Started` rides
        // on the first one: a camera that hands over nothing is a start that
        // failed, not a capture with no picture.
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        while !live.output.delivering() && Instant::now() < deadline {
            std::thread::sleep(FIRST_FRAME_POLL);
        }
        if !live.output.delivering() {
            // SAFETY: the session is live and this thread owns it.
            unsafe { live.session.stopRunning() };
            let _ = ready.send(Err(Unavailable::Failed(
                "the camera handed over no frames".to_string(),
            )));
            return;
        }
        let _ = ready.send(Ok(()));

        // Parks until the Capturer is dropped — a closed channel counts too, so
        // a caller that gave up on the handshake still gets the session torn
        // down — looking up now and then to see whether the camera is still
        // there. An unplugged device simply stops delivering.
        loop {
            match stop.recv_timeout(WATCHDOG_STEP) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if live.output.stalled(started) {
                        live.output
                            .send(CaptureEvent::Ended("the camera stopped".to_string()));
                        break;
                    }
                }
                _ => break,
            }
        }

        // SAFETY: the session is live and stopping it is what releases the
        // device; the delegate outlives it through `Live`.
        unsafe { live.session.stopRunning() };
        tracing::debug!(
            frames = live.output.ivars().delivered.load(Ordering::Relaxed),
            "the camera stream stopped"
        );
        drop(live);
    });
}

fn bring_up(
    request: &CameraRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Live, Unavailable> {
    let device = device(request.source.as_ref())?;
    // SAFETY: `device` is live; the input takes control of it and reports why
    // when it cannot.
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
        .map_err(|error| Unavailable::Failed(error.localizedDescription().to_string()))?;

    let output = Output::new(Ivars {
        events,
        closed: AtomicBool::new(false),
        started: AtomicBool::new(false),
        pace_floor: request.fps.interval().mul_f64(PACE_FLOOR),
        last_frame: Mutex::new(None),
        delivered: AtomicU64::new(0),
    });
    let queue = DispatchQueue::new("vorcall-camera-samples", DispatchQueueAttr::SERIAL);

    // SAFETY: every call below is a setter or a getter on objects this thread
    // owns, and the delegate and its queue outlive the session through `Live`.
    let session = unsafe {
        let session = AVCaptureSession::new();
        session.beginConfiguration();

        let preset = preset(request.size);
        if session.canSetSessionPreset(preset) {
            session.setSessionPreset(preset);
        } else if session.canSetSessionPreset(AVCaptureSessionPresetHigh) {
            session.setSessionPreset(AVCaptureSessionPresetHigh);
        }

        if !session.canAddInput(&input) {
            session.commitConfiguration();
            return Err(Unavailable::Failed(
                "the camera cannot be added to a capture session".to_string(),
            ));
        }
        session.addInput(&input);

        let video = AVCaptureVideoDataOutput::new();
        // A late frame is worth less than the next one, and dropping it is what
        // keeps the sample queue from growing behind a slow encoder.
        video.setAlwaysDiscardsLateVideoFrames(true);
        video.setVideoSettings(Some(&bgra_settings()));
        if !session.canAddOutput(&video) {
            session.commitConfiguration();
            return Err(Unavailable::Failed(
                "the camera cannot deliver video samples".to_string(),
            ));
        }
        session.addOutput(&video);
        video.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*output)), Some(&queue));

        session.commitConfiguration();
        session.startRunning();
        session
    };

    // SAFETY: reading a property of a session this thread owns.
    if !unsafe { session.isRunning() } {
        return Err(Unavailable::Failed(
            "the camera session did not start".to_string(),
        ));
    }

    Ok(Live {
        session,
        output,
        _queue: queue,
    })
}

/// The camera the request names, or the system's default one.
fn device(wanted: Option<&CameraSource>) -> Result<Retained<AVCaptureDevice>, Unavailable> {
    // SAFETY: reads a framework static and calls class methods.
    unsafe {
        match wanted {
            Some(wanted) => AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&wanted.id))
                .ok_or_else(|| Unavailable::Failed(format!("the camera {} is gone", wanted.name))),
            None => {
                let media_type = video_media_type()?;
                AVCaptureDevice::defaultDeviceWithMediaType(media_type).ok_or_else(|| {
                    Unavailable::Unsupported("there is no camera on this Mac".to_string())
                })
            }
        }
    }
}

/// The session preset nearest the size that was asked for. AVFoundation picks
/// the device format from this; the exact frame is whatever the device has, and
/// the first frame is what reports it.
fn preset(size: (u32, u32)) -> &'static AVCaptureSessionPreset {
    // SAFETY: reading framework statics.
    unsafe {
        if size.0 <= 640 && size.1 <= 480 {
            AVCaptureSessionPreset640x480
        } else {
            AVCaptureSessionPreset1280x720
        }
    }
}

/// `{ kCVPixelBufferPixelFormatTypeKey: 32BGRA }`, which is the one thing this
/// backend insists on.
fn bgra_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    // SAFETY: reading a framework static.
    let key = unsafe { kCVPixelBufferPixelFormatTypeKey };
    // SAFETY: CFString and NSString are toll-free bridged, and this key is a
    // constant that outlives every dictionary made from it.
    let key: &NSString = unsafe { &*(key as *const CFString).cast::<NSString>() };

    let number = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    let value: &AnyObject = &number;
    NSDictionary::from_slices(&[key], &[value])
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
            // Bounded by the thread's own watchdog step.
            let _ = thread.join();
        }
    }
}
