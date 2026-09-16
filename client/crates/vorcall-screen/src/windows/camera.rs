//! Windows camera: Media Foundation's source reader.
//!
//! The reader is created with `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`, so
//! whatever the device sends — MJPG, NV12, YUY2 — is converted for us and comes
//! out as RGB32, which is BGRA laid out the way the rest of this crate reads
//! it. That is the whole reason this backend has no colour conversion of its
//! own; the one thing it does have to undo is a bottom-up frame, which RGB32
//! announces with a negative stride.
//!
//! Every Media Foundation object lives on the one capture thread started here,
//! which is also the thread that initialises and shuts the platform down.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFSample, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateDeviceSource,
    MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, HSTRING, PWSTR};

use crate::camera::{CameraCapabilities, CameraRequest, CameraSource};
use crate::{CaptureEvent, Capturer, Stop, Unavailable, VideoFrame};

const BACKEND: &str = "mediafoundation";

/// How long [`start`] waits for the capture thread's first outcome. A webcam
/// takes a moment to power up, which is longer than a display ever does.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// `Drop` must not stall whoever dropped the capture. A device answering
/// normally hands over a frame every frame interval, so the join is well inside
/// this; a device that has stopped answering is left to wind itself down.
const STOP_BUDGET: Duration = Duration::from_millis(500);
const STOP_POLL: Duration = Duration::from_millis(5);

/// The stream index every call below reads, as a plain `u32`.
fn video_stream() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

pub(crate) fn cameras() -> Vec<CameraSource> {
    let Ok(_platform) = Platform::start() else {
        return Vec::new();
    };
    devices()
        .into_iter()
        .map(|device| CameraSource {
            id: device.id,
            name: device.name,
        })
        .collect()
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
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Unavailable>>();
    let thread = std::thread::Builder::new()
        .name("vorcall-camera".to_string())
        .spawn({
            let stop = stop.clone();
            move || run(request, events, &stop, &ready_tx)
        })
        .map_err(|error| Unavailable::Failed(format!("cannot start the camera thread: {error}")))?;

    let mut handle = Handle {
        stop,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(Capturer::new(BACKEND, Box::new(handle))),
        Ok(Err(error)) => {
            handle.stop();
            Err(error)
        }
        Err(_) => {
            handle.stop();
            Err(Unavailable::Failed(
                "the camera did not start in time".to_string(),
            ))
        }
    }
}

/// COM and Media Foundation, started for the length of one thread's work and
/// shut down in the reverse order.
struct Platform {
    /// Whether this thread's COM apartment is ours to uninitialise. It is not
    /// when somebody else made the thread an apartment first.
    owns_com: bool,
}

impl Platform {
    fn start() -> Result<Platform, Unavailable> {
        // SAFETY: the first COM call on this thread.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let owns_com = com.is_ok();

        // SAFETY: called once per thread, undone by the drop below.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if owns_com {
                // SAFETY: paired with the CoInitializeEx that succeeded above.
                unsafe { CoUninitialize() };
            }
            return Err(Unavailable::Failed(format!(
                "Media Foundation refused this thread: {error}"
            )));
        }
        Ok(Platform { owns_com })
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        // SAFETY: paired with the two calls in `start`, on the same thread, and
        // every Media Foundation object made in between is already released.
        unsafe {
            let _ = MFShutdown();
            if self.owns_com {
                CoUninitialize();
            }
        }
    }
}

/// A camera as Media Foundation describes it. `id` is the device's symbolic
/// link, which is what reopens exactly this device later.
struct Device {
    id: String,
    name: String,
}

fn devices() -> Vec<Device> {
    // SAFETY: every call below is made on an initialised platform, and the
    // enumeration's array and objects are released before returning.
    unsafe {
        let Some(attributes) = source_type_attributes(1) else {
            return Vec::new();
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFEnumDeviceSources(&attributes, &mut activates, &mut count).is_err()
            || activates.is_null()
        {
            return Vec::new();
        }

        let mut devices = Vec::new();
        for slot in std::slice::from_raw_parts(activates, count as usize) {
            if let Some(activate) = slot.as_ref()
                && let Some(device) = describe(activate)
            {
                devices.push(device);
            }
        }

        // Each activation object carries one reference that is ours; the array
        // itself was allocated with CoTaskMemAlloc.
        for slot in std::slice::from_raw_parts_mut(activates, count as usize) {
            drop(slot.take());
        }
        CoTaskMemFree(Some(activates as *const c_void));
        devices
    }
}

/// An attributes store already saying "video capture device", with room for
/// `extra` entries beyond that one.
///
/// # Safety
///
/// Media Foundation must be started on this thread.
unsafe fn source_type_attributes(extra: u32) -> Option<IMFAttributes> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, extra).ok()?;
        let attributes = attributes?;
        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .ok()?;
        Some(attributes)
    }
}

/// # Safety
///
/// `activate` must be a live activation object from `MFEnumDeviceSources`.
unsafe fn describe(activate: &IMFActivate) -> Option<Device> {
    unsafe {
        let id = allocated_string(
            activate,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )?;
        let name = allocated_string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
            .unwrap_or_else(|| "Camera".to_string());
        Some(Device { id, name })
    }
}

/// One string attribute, copied out of the buffer Media Foundation allocated
/// for it and freed again here.
///
/// # Safety
///
/// `attributes` must be live.
unsafe fn allocated_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut value = PWSTR::null();
        let mut length = 0u32;
        attributes
            .GetAllocatedString(key, &mut value, &mut length)
            .ok()?;
        if value.is_null() {
            return None;
        }
        let text = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as *const c_void));
        text
    }
}

fn run(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
) {
    let platform = match Platform::start() {
        Ok(platform) => platform,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    capture(&request, &events, stop, ready);

    // Explicit, so it is plain that every reader and source above is released
    // before Media Foundation is shut down.
    drop(platform);
}

fn capture(
    request: &CameraRequest,
    events: &UnboundedSender<CaptureEvent>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
) {
    // SAFETY: the platform is started for the length of this call, and every
    // object below lives and dies on this thread.
    let session = match unsafe { open(request) } {
        Ok(session) => session,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    if events
        .unbounded_send(CaptureEvent::Started {
            width: session.width as u32,
            height: session.height as u32,
            audio: None,
        })
        .is_err()
    {
        return;
    }
    if ready.send(Ok(())).is_err() {
        return;
    }
    tracing::debug!(
        width = session.width,
        height = session.height,
        stride = session.stride,
        fps = request.fps.hz(),
        "the camera stream started"
    );

    // SAFETY: as above; the reader is only ever read from this thread.
    let ended = unsafe { session.pump(request, events, stop) };
    if let Some(reason) = ended {
        let _ = events.unbounded_send(CaptureEvent::Ended(reason));
    }

    // SAFETY: shutting a source down twice is harmless, and nothing reads it
    // after this.
    unsafe {
        let _ = session.source.Shutdown();
    }
}

/// One open camera, and what its negotiated format says about the frames it
/// will hand over.
struct Session {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    width: usize,
    height: usize,
    /// Bytes between the starts of two rows, negative when the device's rows
    /// arrive bottom-up.
    stride: i32,
}

/// # Safety
///
/// Media Foundation must be started on this thread.
unsafe fn open(request: &CameraRequest) -> Result<Session, Unavailable> {
    unsafe {
        let wanted = match request.source.as_ref() {
            Some(source) => source.id.clone(),
            None => {
                devices()
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        Unavailable::Unsupported("there is no camera on this machine".to_string())
                    })?
                    .id
            }
        };

        let failed = |what: &str, error: windows::core::Error| {
            Unavailable::Failed(format!("{what}: {error}"))
        };

        let attributes = source_type_attributes(2)
            .ok_or_else(|| Unavailable::Failed("cannot describe the camera".to_string()))?;
        attributes
            .SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                &HSTRING::from(wanted.as_str()),
            )
            .map_err(|error| failed("cannot describe the camera", error))?;

        let source = MFCreateDeviceSource(&attributes)
            .map_err(|error| failed("cannot open the camera", error))?;

        let mut reader_attributes = None;
        MFCreateAttributes(&mut reader_attributes, 1)
            .map_err(|error| failed("cannot open the camera", error))?;
        let reader_attributes = reader_attributes
            .ok_or_else(|| Unavailable::Failed("cannot open the camera".to_string()))?;
        // Without this the reader hands back the device's own format, which is
        // as often MJPG as anything this crate could read.
        reader_attributes
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|error| failed("cannot open the camera", error))?;

        let reader = MFCreateSourceReaderFromMediaSource(&source, &reader_attributes)
            .map_err(|error| failed("cannot read the camera", error))?;
        let _ = reader.SetStreamSelection(video_stream(), true);

        request_rgb32(&reader, request)?;
        let (width, height, stride) = negotiated(&reader)?;
        Ok(Session {
            reader,
            source,
            width,
            height,
            stride,
        })
    }
}

/// Asks the reader for RGB32 at the requested size and rate, and settles for
/// RGB32 at whatever size and rate the device prefers when it refuses. The size
/// was only ever a hint; the pixel layout is not.
///
/// # Safety
///
/// `reader` must be live.
unsafe fn request_rgb32(
    reader: &IMFSourceReader,
    request: &CameraRequest,
) -> Result<(), Unavailable> {
    unsafe {
        let mut refused = None;
        for detailed in [true, false] {
            let Ok(media_type) = MFCreateMediaType() else {
                continue;
            };
            if media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .is_err()
                || media_type
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                    .is_err()
            {
                continue;
            }
            if detailed {
                let _ =
                    media_type.SetUINT64(&MF_MT_FRAME_SIZE, packed(request.size.0, request.size.1));
                let _ = media_type.SetUINT64(&MF_MT_FRAME_RATE, packed(request.fps.hz(), 1));
            }

            match reader.SetCurrentMediaType(video_stream(), None, &media_type) {
                Ok(()) => return Ok(()),
                Err(error) => refused = Some(error),
            }
        }

        Err(Unavailable::Failed(match refused {
            Some(error) => format!("the camera refused an RGB32 stream: {error}"),
            None => "the camera refused an RGB32 stream".to_string(),
        }))
    }
}

/// The size and stride of the format the reader settled on.
///
/// # Safety
///
/// `reader` must be live.
unsafe fn negotiated(reader: &IMFSourceReader) -> Result<(usize, usize, i32), Unavailable> {
    unsafe {
        let media_type = reader
            .GetCurrentMediaType(video_stream())
            .map_err(|error| Unavailable::Failed(format!("the camera named no format: {error}")))?;
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|error| {
            Unavailable::Failed(format!("the camera named no frame size: {error}"))
        })?;
        let (width, height) = ((size >> 32) as u32 as usize, size as u32 as usize);
        if width == 0 || height == 0 {
            return Err(Unavailable::Failed(
                "the camera named an empty frame size".to_string(),
            ));
        }

        // An absent default stride is a top-down frame packed tight, which is
        // what RGB32 out of the reader's own converter looks like.
        let stride = media_type
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map_or((width * 4) as i32, |stride| stride as i32);
        Ok((width, height, stride))
    }
}

/// Two `u32`s in the one `u64` Media Foundation stores a size or a ratio as:
/// the first in the high half, the second in the low one.
fn packed(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

impl Session {
    /// Reads frames until the capture is stopped, the device ends its stream or
    /// the consumer goes away. Answers with what to tell the consumer, if there
    /// is anything left to tell.
    ///
    /// # Safety
    ///
    /// The platform must be started on this thread.
    unsafe fn pump(
        &self,
        request: &CameraRequest,
        events: &UnboundedSender<CaptureEvent>,
        stop: &AtomicBool,
    ) -> Option<String> {
        let interval = request.fps.interval();
        let mut previous: Option<Instant> = None;

        while !stop.load(Ordering::SeqCst) {
            let mut flags = 0u32;
            let mut sample = None;
            // SAFETY: the reader is live and read only from this thread; this
            // blocks until the device hands over the next frame.
            let read = unsafe {
                self.reader.ReadSample(
                    video_stream(),
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )
            };
            if let Err(error) = read {
                return Some(format!("the camera stopped: {error}"));
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Some("the camera reported an error".to_string());
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Some("the camera ended its stream".to_string());
            }
            // The size and stride this session was built around no longer
            // describe the frames, and a frame read to the wrong measurements
            // is worse than no frame: end, and let the app start again.
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                return Some("the camera changed its format".to_string());
            }
            // A gap in the stream comes back as no sample at all, which is a
            // wait rather than an ending.
            let Some(sample) = sample else {
                continue;
            };

            let now = Instant::now();
            if previous.is_some_and(|previous| now.duration_since(previous) < interval) {
                continue;
            }
            // SAFETY: the sample is live for this iteration.
            let Some(frame) = (unsafe { self.copy(&sample, now) }) else {
                continue;
            };
            previous = Some(now);
            if events.unbounded_send(CaptureEvent::Video(frame)).is_err() {
                // Nobody is listening any more, so there is nobody to tell.
                return None;
            }
        }
        None
    }

    /// One sample's pixels, top-down and tightly packed.
    ///
    /// # Safety
    ///
    /// `sample` must be live.
    unsafe fn copy(&self, sample: &IMFSample, captured: Instant) -> Option<VideoFrame> {
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length)).ok()?;

            let row = self.width * 4;
            let step = self.stride.unsigned_abs() as usize;
            let bottom_up = self.stride < 0;
            let frame = if data.is_null() || step < row || (length as usize) < step * self.height {
                None
            } else {
                let mut bgra = Vec::with_capacity(row * self.height);
                for line in 0..self.height {
                    let source_line = if bottom_up {
                        self.height - 1 - line
                    } else {
                        line
                    };
                    // SAFETY: the buffer is locked over `length` bytes, which
                    // was just checked to cover `height` rows of `step`, and
                    // each copy stays inside the first `row` of its row.
                    let source = std::slice::from_raw_parts(data.add(source_line * step), row);
                    bgra.extend_from_slice(source);
                }
                Some(VideoFrame {
                    width: self.width as u32,
                    height: self.height as u32,
                    stride: row,
                    bgra,
                    captured,
                })
            };

            let _ = buffer.Unlock();
            frame
        }
    }
}

/// Stops the capture thread. The loop looks at the flag once per frame, so the
/// join is normally immediate; a device that has stopped answering is left to
/// wind itself down rather than holding up whoever dropped the capture.
struct Handle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(thread) = self.thread.take() else {
            return;
        };

        let deadline = Instant::now() + STOP_BUDGET;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(STOP_POLL);
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            tracing::debug!("the camera thread is still waiting on a frame");
        }
    }
}
