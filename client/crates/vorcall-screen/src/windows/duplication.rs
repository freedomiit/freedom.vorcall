//! DXGI Desktop Duplication: the cheapest way to read a whole monitor, and the
//! one Windows sometimes will not give — a secure desktop, a duplication
//! another application already holds, a driver that never offered it. Every
//! such refusal sends the monitor to [`super::wgc`] instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::{HRESULT, Interface};

use super::cursor::Cursor;
use super::d3d::{Gpu, Pixels, Staging};
use super::send;
use crate::{CaptureEvent, CaptureRequest, Unavailable, VideoFrame};

/// How long the capture keeps trying to take a lost duplication back before
/// giving up, and how long it waits between attempts. Whatever took the desktop
/// holds it for as long as it likes — a UAC prompt sits there until the user
/// looks at it — so the budget has to outlast a glance at a dialog.
const RETRY_BUDGET: Duration = Duration::from_secs(5);
const RETRY_PAUSE: Duration = Duration::from_millis(250);
/// How often that pause looks at the stop flag, so a capture being torn down
/// never waits out a whole [`RETRY_PAUSE`] first.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Why [`open`] did not produce a duplication.
pub(super) enum Refused {
    /// Not this monitor, not this session — Windows.Graphics.Capture may still
    /// manage it.
    UseWgc(String),
    /// Nothing will capture this.
    Failed(Unavailable),
}

pub(super) struct Duplication {
    gpu: Gpu,
    output: IDXGIOutput1,
    /// Empty between losing the duplication and taking it again.
    duplication: Option<IDXGIOutputDuplication>,
    size: (u32, u32),
    cursor: Cursor,
    staging: Staging,
    /// Whether a full picture has gone out yet. Until it has, no frame may be
    /// skipped for being unchanged — the consumer has nothing to leave alone.
    delivered: bool,
}

impl Duplication {
    pub(super) fn open(monitor: HMONITOR) -> Result<Duplication, Refused> {
        let (adapter, output) = find_output(monitor)?;
        let gpu = Gpu::create(Some(&adapter)).map_err(Refused::Failed)?;

        // SAFETY: `output` came from this thread's adapter enumeration and is
        // still alive.
        let desc = unsafe { output.GetDesc() }
            .map_err(|error| Refused::UseWgc(format!("the monitor has no mode: {error}")))?;
        let bounds = desc.DesktopCoordinates;
        let size = (
            bounds.right.saturating_sub(bounds.left).max(0) as u32,
            bounds.bottom.saturating_sub(bounds.top).max(0) as u32,
        );

        // SAFETY: the device belongs to the adapter that owns this output,
        // which is what `DuplicateOutput` requires of it.
        let duplication = unsafe { output.DuplicateOutput(gpu.device()) }.map_err(|error| {
            if refuses_duplication(error.code()) {
                Refused::UseWgc(format!("duplication was refused: {error}"))
            } else {
                Refused::Failed(Unavailable::Failed(format!(
                    "cannot duplicate the monitor: {error}"
                )))
            }
        })?;

        Ok(Duplication {
            gpu,
            output,
            duplication: Some(duplication),
            size,
            cursor: Cursor::default(),
            staging: Staging::new(),
            delivered: false,
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    pub(super) fn run(
        &mut self,
        request: &CaptureRequest,
        events: &UnboundedSender<CaptureEvent>,
        stop: &AtomicBool,
    ) {
        let interval = request.fps.interval();
        // The wait for a frame doubles as the loop's stop poll, so it is never
        // longer than one frame.
        let timeout_ms = interval.as_millis().max(1) as u32;
        let mut due = Instant::now();
        // Set the moment the duplication is lost, cleared once it is back.
        let mut give_up_at: Option<Instant> = None;

        while !stop.load(Ordering::Relaxed) {
            if self.duplication.is_none() {
                let deadline = *give_up_at.get_or_insert_with(|| Instant::now() + RETRY_BUDGET);
                if !self.retake(deadline, stop) {
                    send(
                        events,
                        CaptureEvent::Ended("desktop duplication lost".to_string()),
                    );
                    return;
                }
                if self.duplication.is_some() {
                    give_up_at = None;
                }
                continue;
            }

            let now = Instant::now();
            match self.acquire(timeout_ms, now >= due, request.cursor) {
                Ok(None) => {}
                Ok(Some(pixels)) => {
                    due = now + interval;
                    let frame = VideoFrame {
                        width: pixels.width,
                        height: pixels.height,
                        stride: pixels.width as usize * 4,
                        bgra: pixels.bgra,
                        captured: now,
                    };
                    if !send(events, CaptureEvent::Video(frame)) {
                        return;
                    }
                }
                Err(Lost::Duplication) => self.duplication = None,
                Err(Lost::Fatal(reason)) => {
                    send(events, CaptureEvent::Ended(reason));
                    return;
                }
            }
        }
    }

    /// One attempt at taking the duplication back, followed by a pause if it
    /// failed. `false` once `deadline` has passed with the duplication still
    /// gone, which is the capture giving up.
    fn retake(&mut self, deadline: Instant, stop: &AtomicBool) -> bool {
        // SAFETY: the output outlives the duplication it hands out, and the
        // device is the one it was opened with.
        match unsafe { self.output.DuplicateOutput(self.gpu.device()) } {
            Ok(duplication) => {
                self.duplication = Some(duplication);
                self.staging = Staging::new();
                self.delivered = false;
                return true;
            }
            Err(error) => tracing::debug!(%error, "taking the monitor duplication again failed"),
        }

        if Instant::now() >= deadline {
            return false;
        }
        pause(RETRY_PAUSE, stop);
        true
    }

    /// Waits up to `timeout_ms` for the desktop to change. The cursor is read
    /// from every frame that carries it; the pixels only when `wanted`, which
    /// is how the loop paces itself down to the requested rate.
    fn acquire(
        &mut self,
        timeout_ms: u32,
        wanted: bool,
        cursor: bool,
    ) -> Result<Option<Pixels>, Lost> {
        let Some(duplication) = self.duplication.clone() else {
            return Ok(None);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both out parameters outlive the call.
        match unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                return Err(Lost::Duplication);
            }
            Err(error) => {
                return Err(Lost::Fatal(format!("the monitor capture stopped: {error}")));
            }
        }

        let taken = self.take(&duplication, resource.as_ref(), &info, wanted, cursor);
        // SAFETY: paired with the `AcquireNextFrame` above, which succeeded.
        // Nothing may be acquired again until this returns.
        let _ = unsafe { duplication.ReleaseFrame() };
        taken
    }

    fn take(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        resource: Option<&IDXGIResource>,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        wanted: bool,
        cursor: bool,
    ) -> Result<Option<Pixels>, Lost> {
        if cursor {
            if info.LastMouseUpdateTime != 0 {
                self.cursor.moved(&info.PointerPosition);
            }
            if info.PointerShapeBufferSize > 0 {
                self.reshape(duplication, info.PointerShapeBufferSize);
            }
        }

        if !wanted {
            return Ok(None);
        }
        // A frame that accumulated nothing is the pointer moving over a desktop
        // that did not change, so the one the consumer already has still holds
        // — unless the pointer is being painted into it.
        if info.AccumulatedFrames == 0 && !cursor && self.delivered {
            return Ok(None);
        }
        let Some(resource) = resource else {
            return Ok(None);
        };

        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|error| Lost::Fatal(format!("the desktop image was unreadable: {error}")))?;
        let mut pixels = self
            .staging
            .read(&self.gpu, &texture)
            .map_err(|error| Lost::Fatal(format!("cannot read the desktop image: {error}")))?;
        if cursor {
            self.cursor
                .composite(&mut pixels.bgra, pixels.width, pixels.height);
        }
        self.delivered = true;
        Ok(Some(pixels))
    }

    fn reshape(&mut self, duplication: &IDXGIOutputDuplication, size: u32) {
        let mut bits = vec![0u8; size as usize];
        let mut required = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the buffer is exactly the `size` bytes the frame asked for,
        // and both out parameters outlive the call.
        let read = unsafe {
            duplication.GetFramePointerShape(
                size,
                bits.as_mut_ptr().cast(),
                &mut required,
                &mut info,
            )
        };
        match read {
            Ok(()) => self.cursor.reshaped(&info, bits),
            Err(error) => tracing::debug!(%error, "the pointer shape was unreadable"),
        }
    }
}

/// Sleeps for `total`, in slices short enough that a stop set meanwhile is
/// noticed well inside the 500 ms the capture handle is allowed.
fn pause(total: Duration, stop: &AtomicBool) {
    let until = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(STOP_POLL));
    }
}

enum Lost {
    /// The duplication has to be taken again — a mode change, a desktop switch,
    /// another application taking over.
    Duplication,
    Fatal(String),
}

/// The adapter that drives `monitor` and the output it drives it through. The
/// device has to be created on that adapter or duplication refuses it.
fn find_output(monitor: HMONITOR) -> Result<(IDXGIAdapter, IDXGIOutput1), Refused> {
    // SAFETY: the factory is created and used entirely on this thread.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|error| {
        Refused::Failed(Unavailable::Failed(format!("no DXGI factory: {error}")))
    })?;

    let mut adapters = 0u32;
    // SAFETY: enumeration stops at the first index DXGI refuses.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapters) } {
        adapters += 1;
        let mut outputs = 0u32;
        // SAFETY: as above.
        while let Ok(output) = unsafe { adapter.EnumOutputs(outputs) } {
            outputs += 1;
            // SAFETY: `output` is alive for this iteration.
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if desc.Monitor != monitor {
                continue;
            }
            let output = output.cast::<IDXGIOutput1>().map_err(|error| {
                Refused::UseWgc(format!("the monitor has no duplication interface: {error}"))
            })?;
            let adapter = adapter.cast::<IDXGIAdapter>().map_err(|error| {
                Refused::Failed(Unavailable::Failed(format!("no DXGI adapter: {error}")))
            })?;
            return Ok((adapter, output));
        }
    }

    Err(Refused::UseWgc(
        "no display adapter reports this monitor".to_string(),
    ))
}

/// The refusals that mean "not through duplication", as opposed to a broken
/// machine: a session with no duplication at all, a monitor already duplicated
/// by another application, a locked or otherwise protected desktop.
fn refuses_duplication(code: HRESULT) -> bool {
    [
        DXGI_ERROR_UNSUPPORTED,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
        DXGI_ERROR_SESSION_DISCONNECTED,
        E_ACCESSDENIED,
    ]
    .contains(&code)
}
