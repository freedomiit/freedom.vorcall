//! Webcam capture, beside the screen capture rather than instead of it: a
//! camera and a share can run at the same time, in this one process.
//!
//! The frames come out of the same [`CaptureEvent`] channel a screen capture
//! uses, in the same BGRA, and stop on the same [`Capturer`] drop — so the
//! scale → encode → send pipeline above does not need to know which of the two
//! it is carrying. A camera never sends [`CaptureEvent::Audio`]: the microphone
//! is somebody else's business.

use futures::channel::mpsc::UnboundedSender;

use crate::preset::FrameRate;
use crate::{CaptureEvent, Capturer, Unavailable};

/// One camera the platform can name. Opaque, like [`crate::SourceId`]: the app
/// carries it back into [`CameraRequest`] and never reads `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraSource {
    pub id: String,
    /// What the platform calls the device, for the settings screen.
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct CameraRequest {
    /// `None` is the system's default camera, which on Linux is the only thing
    /// on offer — the portal picks the device there.
    pub source: Option<CameraSource>,
    /// The frame this capture would like. A hint only: a webcam delivers one of
    /// the handful of sizes it was built for, and the caller scales what
    /// arrives. [`CaptureEvent::Started`] carries the size that was really
    /// negotiated.
    pub size: (u32, u32),
    pub fps: FrameRate,
}

#[derive(Clone, Debug)]
pub struct CameraCapabilities {
    pub backend: &'static str,
    /// This platform has a camera path at all. It says nothing about whether a
    /// camera is plugged in or whether the user will allow it — only a
    /// [`start_camera`] can answer that.
    pub available: bool,
    /// [`cameras`] can name the devices. When it cannot, the platform picks one
    /// and the interface has no list to offer.
    pub enumerates: bool,
}

/// Every camera this machine can name, empty when there are none or when the
/// platform does not enumerate them.
///
/// Never raises a permission dialog: it is safe to call from a settings screen.
pub fn cameras() -> Vec<CameraSource> {
    #[cfg(windows)]
    {
        crate::windows::camera::cameras()
    }

    #[cfg(target_os = "macos")]
    {
        crate::macos::camera::cameras()
    }

    #[cfg(target_os = "linux")]
    {
        crate::linux::camera::cameras()
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// What this platform's camera backend can do. Never fails, so the interface
/// can ask before anything has been granted.
pub fn camera_capabilities() -> CameraCapabilities {
    #[cfg(windows)]
    {
        crate::windows::camera::capabilities()
    }

    #[cfg(target_os = "macos")]
    {
        crate::macos::camera::capabilities()
    }

    #[cfg(target_os = "linux")]
    {
        crate::linux::camera::capabilities()
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        CameraCapabilities {
            backend: "none",
            available: false,
            enumerates: false,
        }
    }
}

/// Starts the camera, sending every frame to `events` until the returned handle
/// is dropped or the backend sends [`CaptureEvent::Ended`].
///
/// Blocking, and possibly for as long as the user takes to answer a permission
/// dialog: call it from a worker thread, never from the UI thread.
pub fn start_camera(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    tracing::debug!(
        fps = request.fps.hz(),
        size = ?request.size,
        named = request.source.is_some(),
        "starting a camera capture"
    );

    #[cfg(windows)]
    {
        crate::windows::camera::start(request, events)
    }

    #[cfg(target_os = "macos")]
    {
        crate::macos::camera::start(request, events)
    }

    #[cfg(target_os = "linux")]
    {
        crate::linux::camera::start(request, events)
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (request, events);
        Err(Unavailable::Unsupported("unsupported platform".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machines without a camera are the norm in CI, and a machine with one
    /// answers with it; neither may panic, and neither may hang on a dialog.
    #[test]
    fn listing_cameras_answers_without_a_device() {
        let found = cameras();
        for camera in &found {
            assert!(!camera.id.is_empty(), "a camera needs an id to be reopened");
        }

        let capabilities = camera_capabilities();
        assert!(!capabilities.backend.is_empty());
        if !capabilities.enumerates {
            assert!(
                found.is_empty(),
                "a backend that does not enumerate cannot have named a camera"
            );
        }
    }
}
