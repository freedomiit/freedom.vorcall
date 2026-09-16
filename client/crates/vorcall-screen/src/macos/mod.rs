//! macOS backend: ScreenCaptureKit.
//!
//! Displays and windows come from `SCShareableContent`, the frames from an
//! `SCStream` configured for BGRA at the requested rate, and the machine's
//! playout from the same stream with this process left out, which is what
//! keeps Vorcall's own voice out of a share. All of it needs the Screen
//! Recording grant, which only takes effect after a relaunch and is lost on
//! every update because the bundle is ad-hoc signed.
//!
//! The camera next door in [`camera`] is AVFoundation instead, and needs its
//! own grant — but the two run side by side.

pub(crate) mod camera;
mod content;
mod permission;
mod stream;

use futures::channel::mpsc::UnboundedSender;

use crate::{Capabilities, CaptureEvent, CaptureRequest, Capturer, Source, Unavailable};

pub(crate) fn enumerate() -> Result<Vec<Source>, Unavailable> {
    permission::check()?;
    content::enumerate()
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        backend: "screencapturekit",
        portal_picker: false,
        windows: true,
        audio: true,
    }
}

pub(crate) fn start(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    permission::check()?;
    stream::start(request, events)
}
