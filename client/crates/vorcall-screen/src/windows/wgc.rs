//! Windows.Graphics.Capture: the only way to capture a single window, and the
//! way a monitor is captured when Desktop Duplication refuses it.
//!
//! The frame pool is free-threaded, so `FrameArrived` fires on a thread pool
//! thread that has no business touching Direct3D — the immediate context is not
//! thread-safe. The handler therefore only rings a bell, and the capture thread
//! that owns every object here drains the pool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HSTRING, IInspectable, Interface};

use super::d3d::{Gpu, Staging};
use super::send;
use crate::{CaptureEvent, CaptureRequest, Unavailable, VideoFrame};

/// Two is enough to keep the compositor from stalling while one frame is being
/// copied, and small enough that a slow consumer never sees a stale backlog.
const POOL_BUFFERS: i32 = 2;
/// How long the loop waits on the frame bell before checking the stop flag.
const WAKE: Duration = Duration::from_millis(100);
const SESSION: &str = "Windows.Graphics.Capture.GraphicsCaptureSession";

pub(super) enum Item {
    Monitor(HMONITOR),
    Window(HWND),
}

pub(super) struct Capture {
    gpu: Gpu,
    device: IDirect3DDevice,
    /// Held for as long as the capture runs: closing it ends the session.
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    staging: Staging,
    size: (i32, i32),
    bell: std::sync::Arc<Bell>,
    closed_message: &'static str,
}

impl Capture {
    pub(super) fn open(target: Item, cursor: bool) -> Result<Capture, Unavailable> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(Unavailable::Unsupported(
                "this Windows build cannot capture a screen".to_string(),
            ));
        }

        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|error| {
                Unavailable::Unsupported(format!("no screen capture interop: {error}"))
            })?;
        let closed_message = match target {
            Item::Monitor(_) => "the monitor went away",
            Item::Window(_) => "the window closed",
        };
        // SAFETY: both handles came from this session's own enumeration and are
        // checked by the runtime, which answers with an error for a stale one.
        let item = unsafe {
            match target {
                Item::Monitor(handle) => interop.CreateForMonitor::<GraphicsCaptureItem>(handle),
                Item::Window(handle) => interop.CreateForWindow::<GraphicsCaptureItem>(handle),
            }
        }
        .map_err(|error| Unavailable::Failed(format!("cannot capture that source: {error}")))?;

        let bounds = item
            .Size()
            .map_err(|error| Unavailable::Failed(format!("the source has no size: {error}")))?;

        let gpu = Gpu::create(None)?;
        let dxgi: IDXGIDevice = gpu
            .device()
            .cast()
            .map_err(|error| Unavailable::Failed(format!("no DXGI device: {error}")))?;
        // SAFETY: `dxgi` is a live device created on this thread.
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|device| device.cast())
            .map_err(|error| Unavailable::Failed(format!("no capture device: {error}")))?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            bounds,
        )
        .map_err(|error| Unavailable::Failed(format!("no capture frame pool: {error}")))?;

        let bell = std::sync::Arc::new(Bell::default());
        let arrived = bell.clone();
        pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
                arrived.ring(|state| state.frames = true);
                Ok(())
            }),
        )
        .map_err(|error| Unavailable::Failed(format!("cannot watch for frames: {error}")))?;

        let gone = bell.clone();
        item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                gone.ring(|state| state.closed = true);
                Ok(())
            }),
        )
        .map_err(|error| Unavailable::Failed(format!("cannot watch the source: {error}")))?;

        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|error| Unavailable::Failed(format!("no capture session: {error}")))?;
        // Both arrived after Windows.Graphics.Capture itself did, so an older
        // build simply captures with its own defaults: cursor on, border drawn.
        if has_property("IsCursorCaptureEnabled") {
            let _ = session.SetIsCursorCaptureEnabled(cursor);
        }
        if has_property("IsBorderRequired") {
            let _ = session.SetIsBorderRequired(false);
        }
        session
            .StartCapture()
            .map_err(|error| Unavailable::Failed(format!("the capture did not start: {error}")))?;

        Ok(Capture {
            gpu,
            device,
            _item: item,
            pool,
            session,
            staging: Staging::new(),
            size: (bounds.Width, bounds.Height),
            bell,
            closed_message,
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.size.0.max(0) as u32, self.size.1.max(0) as u32)
    }

    pub(super) fn run(
        &mut self,
        request: &CaptureRequest,
        events: &UnboundedSender<CaptureEvent>,
        stop: &AtomicBool,
    ) {
        let interval = request.fps.interval();
        let mut due = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            let rung = self.bell.wait(WAKE);
            if rung.closed {
                send(events, CaptureEvent::Ended(self.closed_message.to_string()));
                return;
            }
            if !rung.frames {
                continue;
            }

            let Some(frame) = self.newest() else {
                continue;
            };
            let content = frame.ContentSize().unwrap_or(SizeInt32 {
                Width: self.size.0,
                Height: self.size.1,
            });

            let now = Instant::now();
            let pixels = if now >= due {
                match self.copy(&frame) {
                    Ok(pixels) => Some(pixels),
                    Err(reason) => {
                        let _ = frame.Close();
                        send(events, CaptureEvent::Ended(reason));
                        return;
                    }
                }
            } else {
                None
            };
            // Released before the pool is resized: a live frame holds one of
            // its buffers.
            let _ = frame.Close();

            if let Some(pixels) = pixels {
                due = now + interval;
                let video = VideoFrame {
                    width: pixels.width,
                    height: pixels.height,
                    stride: pixels.width as usize * 4,
                    bgra: pixels.bgra,
                    captured: now,
                };
                if !send(events, CaptureEvent::Video(video)) {
                    return;
                }
            }

            if (content.Width, content.Height) != self.size {
                self.resize(content);
            }
        }
    }

    /// The pool hands out frames oldest first; only the newest is worth
    /// encoding, and the rest have to be closed to give their buffers back.
    fn newest(&self) -> Option<Direct3D11CaptureFrame> {
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        for _ in 0..POOL_BUFFERS {
            let Ok(frame) = self.pool.TryGetNextFrame() else {
                break;
            };
            if let Some(stale) = newest.replace(frame) {
                let _ = stale.Close();
            }
        }
        newest
    }

    fn copy(&mut self, frame: &Direct3D11CaptureFrame) -> Result<super::d3d::Pixels, String> {
        let surface = frame
            .Surface()
            .map_err(|error| format!("the capture returned no surface: {error}"))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|error| format!("the capture surface was unreadable: {error}"))?;
        // SAFETY: `access` is the surface's own interop interface, alive for as
        // long as the frame is.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
            .map_err(|error| format!("the capture surface had no texture: {error}"))?;

        self.staging
            .read(&self.gpu, &texture)
            .map_err(|error| format!("cannot read the captured frame: {error}"))
    }

    fn resize(&mut self, content: SizeInt32) {
        match self.pool.Recreate(
            &self.device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_BUFFERS,
            content,
        ) {
            Ok(()) => self.size = (content.Width, content.Height),
            Err(error) => tracing::debug!(%error, "the capture could not follow a resize"),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// What the two WinRT callbacks are allowed to touch: no Direct3D, no WinRT,
/// nothing that cares which thread it is on.
#[derive(Default)]
struct Bell {
    state: Mutex<Rung>,
    rung: Condvar,
}

#[derive(Clone, Copy, Default)]
struct Rung {
    frames: bool,
    closed: bool,
}

impl Bell {
    fn ring(&self, mark: impl FnOnce(&mut Rung)) {
        mark(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner));
        self.rung.notify_all();
    }

    /// Waits up to `timeout` for something to have happened, and takes the
    /// frame mark with it — the close mark stays, because it is final.
    fn wait(&self, timeout: Duration) -> Rung {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = self
            .rung
            .wait_timeout_while(state, timeout, |state| !state.frames && !state.closed)
            .unwrap_or_else(PoisonError::into_inner);
        let rung = *state;
        state.frames = false;
        rung
    }
}

fn has_property(name: &str) -> bool {
    ApiInformation::IsPropertyPresent(&HSTRING::from(SESSION), &HSTRING::from(name))
        .unwrap_or(false)
}
