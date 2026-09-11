//! Screen capture and the software video codec behind screen share.
//!
//! No iced, cpal (outside the Windows loopback backend) or rodio here, so the
//! crate builds and tests without a GUI or a default audio device.
//!
//! [`Capturer::start`] hands frames and, where the platform can, the machine's
//! own playout to an unbounded channel, and keeps going until the [`Capturer`]
//! is dropped or the backend gives up. Everything above it — what to encode,
//! how often, at what size — comes out of [`preset`], and [`codec`] turns the
//! frames into H.264 access units. Nothing here knows about the wire.

pub mod codec;
pub mod pattern;
pub mod preset;
pub mod scale;

// Each backend module exposes exactly three entry points and nothing else:
//
//   pub(crate) fn enumerate() -> Result<Vec<Source>, Unavailable>
//   pub(crate) fn capabilities() -> Capabilities
//   pub(crate) fn start(request: CaptureRequest, events: UnboundedSender<CaptureEvent>)
//       -> Result<Capturer, Unavailable>
//
// The dispatch below is the only place that names them.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::fmt;
use std::time::Instant;

use futures::channel::mpsc::UnboundedSender;

/// Why a capture could not be started, or a source list could not be taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// This platform or session cannot capture at all — offer something else
    /// rather than a retry.
    #[error("{0}")]
    Unsupported(String),
    /// The user has to grant something first (macOS screen recording, the
    /// Wayland portal dialog).
    #[error("{0}")]
    PermissionDenied(String),
    #[error("{0}")]
    Failed(String),
}

/// A backend's own handle on a display or window. Opaque: the app carries it
/// back into [`CaptureRequest`] and never reads it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Display,
    Window,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub id: SourceId,
    pub kind: SourceKind,
    /// The monitor's or window's name, as the platform spells it.
    pub title: String,
    pub width: u32,
    pub height: u32,
}

/// What a backend does with the machine's own audio while capturing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioMode {
    Excluded,
    /// The capture carries this machine's playout, Vorcall's own voice included.
    IncludesOwnPlayout,
}

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub backend: &'static str,
    /// The picker belongs to the system, so [`enumerate`] has nothing to offer
    /// and the user chooses inside [`Capturer::start`].
    pub portal_picker: bool,
    /// Single windows can be captured, not just whole displays.
    pub windows: bool,
    pub audio: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureRequest {
    /// `None` lets the backend choose, which is what a portal picker needs.
    pub source: Option<SourceId>,
    pub fps: preset::FrameRate,
    pub cursor: bool,
    pub audio: bool,
    /// The frame the backend should aim for, when it can scale for us.
    pub max_size: Option<(u32, u32)>,
}

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes between the starts of two rows; at least `width * 4`.
    pub stride: usize,
    pub bgra: Vec<u8>,
    pub captured: Instant,
}

impl fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("bytes", &self.bgra.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct AudioChunk {
    pub sample_rate: u32,
    pub channels: u16,
    pub interleaved: Vec<f32>,
}

impl fmt::Debug for AudioChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioChunk")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("samples", &self.interleaved.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum CaptureEvent {
    /// The first event of every capture: what the backend actually gave us,
    /// which is not always what was asked for.
    Started {
        width: u32,
        height: u32,
        audio: Option<AudioMode>,
    },
    Video(VideoFrame),
    Audio(AudioChunk),
    /// The backend stopped on its own — the user revoked the share, the window
    /// closed, the stream broke. No further events follow.
    Ended(String),
}

/// A running capture. Stops when dropped, which also drops the backend's clone
/// of the sender, so the consumer's stream ends.
pub struct Capturer {
    backend: &'static str,
    stop: Box<dyn Stop>,
}

impl Capturer {
    /// Starts capturing, sending every frame to `events` until this handle is
    /// dropped or the backend sends [`CaptureEvent::Ended`].
    ///
    /// Blocking, and on Linux and macOS possibly for as long as the user takes
    /// to answer a dialog: call it from a worker thread, never from the UI
    /// thread.
    pub fn start(
        request: CaptureRequest,
        events: UnboundedSender<CaptureEvent>,
    ) -> Result<Capturer, Unavailable> {
        tracing::debug!(
            fps = request.fps.hz(),
            audio = request.audio,
            "starting a screen capture"
        );

        #[cfg(windows)]
        {
            windows::start(request, events)
        }

        #[cfg(target_os = "macos")]
        {
            macos::start(request, events)
        }

        #[cfg(target_os = "linux")]
        {
            linux::start(request, events)
        }

        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = (request, events);
            Err(Unavailable::Unsupported("unsupported platform".to_string()))
        }
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// The platform backends build their handle with this.
    pub(crate) fn new(backend: &'static str, stop: Box<dyn Stop>) -> Capturer {
        Capturer { backend, stop }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// A backend's shutdown handle. Dropping the [`Capturer`] calls `stop`, which
/// must return within 500 ms.
pub(crate) trait Stop: Send {
    fn stop(&mut self);
}

/// Every display and window that can be shared.
///
/// Blocking: on macOS the first call is what raises the screen-recording
/// prompt. On Linux the answer is always empty, because the portal's own picker
/// chooses the source and never tells us what else there was.
pub fn enumerate() -> Result<Vec<Source>, Unavailable> {
    #[cfg(windows)]
    {
        windows::enumerate()
    }

    #[cfg(target_os = "macos")]
    {
        macos::enumerate()
    }

    #[cfg(target_os = "linux")]
    {
        linux::enumerate()
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(Unavailable::Unsupported("unsupported platform".to_string()))
    }
}

/// What this platform's backend can do. Never fails, so the interface can ask
/// before it has permission to capture anything.
pub fn capabilities() -> Capabilities {
    #[cfg(windows)]
    {
        windows::capabilities()
    }

    #[cfg(target_os = "macos")]
    {
        macos::capabilities()
    }

    #[cfg(target_os = "linux")]
    {
        linux::capabilities()
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Capabilities {
            backend: "none",
            portal_picker: false,
            windows: false,
            audio: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Flag(Arc<AtomicBool>);

    impl Stop for Flag {
        fn stop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_a_capturer_stops_its_backend() {
        let stopped = Arc::new(AtomicBool::new(false));
        let capturer = Capturer::new("test", Box::new(Flag(stopped.clone())));

        assert_eq!(capturer.backend(), "test");
        assert!(!stopped.load(Ordering::SeqCst));

        drop(capturer);
        assert!(stopped.load(Ordering::SeqCst), "Drop must call stop");
    }

    /// The app starts a capture on a worker thread and keeps the handle where
    /// the interface can drop it, so the handle has to cross threads.
    #[test]
    fn a_capturer_moves_between_threads() {
        fn require_send<T: Send>() {}
        require_send::<Capturer>();
    }

    #[test]
    fn a_frame_never_prints_its_pixels() {
        let frame = VideoFrame {
            width: 2,
            height: 2,
            stride: 8,
            bgra: vec![0xAB; 16],
            captured: Instant::now(),
        };
        let printed = format!("{:?}", CaptureEvent::Video(frame));

        assert!(printed.contains("bytes: 16"), "{printed}");
        assert!(!printed.contains("171"), "{printed}");
    }
}
