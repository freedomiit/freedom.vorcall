//! What `SCShareableContent` says can be shared, and the lookup of one entry.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use core_graphics::display::CGDisplay;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCDisplay, SCShareableContent, SCWindow};

use crate::{Source, SourceId, SourceKind, Unavailable};

/// How long the snapshot may take; it is a round trip through the window
/// server.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(10);

const DISPLAY_PREFIX: &str = "display:";
const WINDOW_PREFIX: &str = "window:";

/// A retained `SCShareableContent` on its way from the completion handler's
/// queue to the thread that asked for it.
///
/// `Retained<SCShareableContent>` is not `Send` because the bindings mark no
/// ScreenCaptureKit class thread-safe; the framework itself hands the object
/// to a queue of its choosing and never changes the snapshot afterwards, so
/// moving it once is sound.
struct ContentHandle(*mut SCShareableContent);

// SAFETY: see the type's documentation. The pointer carries one retain, taken
// in the completion handler and given back to `Retained` where it lands.
unsafe impl Send for ContentHandle {}

/// The display or window a capture request named, with its size in backing
/// pixels, which is what the stream is configured in.
pub(super) enum Target {
    Display {
        display: Retained<SCDisplay>,
        pixels: (u32, u32),
    },
    Window {
        window: Retained<SCWindow>,
        pixels: (u32, u32),
    },
}

impl Target {
    pub(super) fn size(&self) -> (u32, u32) {
        match self {
            Target::Display { pixels, .. } | Target::Window { pixels, .. } => *pixels,
        }
    }
}

/// Everything ScreenCaptureKit lists right now.
pub(super) fn fetch() -> Result<Retained<SCShareableContent>, Unavailable> {
    let (tx, rx) = mpsc::channel::<Result<ContentHandle, String>>();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: the framework passes either a live content object or a
            // live error, both valid until the handler returns; retaining takes
            // a reference of our own before that.
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(ContentHandle(Retained::into_raw(content))),
                None => Err(describe(error)),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the block has the completion handler's signature.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    match rx.recv_timeout(CONTENT_TIMEOUT) {
        // SAFETY: the pointer carries the retain taken in the handler.
        Ok(Ok(handle)) => unsafe { Retained::from_raw(handle.0) }.ok_or_else(|| {
            Unavailable::Failed("ScreenCaptureKit handed out no content".to_string())
        }),
        Ok(Err(description)) => Err(Unavailable::Failed(format!(
            "cannot list what can be shared: {description}"
        ))),
        Err(_) => Err(Unavailable::Failed(
            "ScreenCaptureKit did not list what can be shared in time".to_string(),
        )),
    }
}

/// Every display, then every on-screen titled window that is not Vorcall's,
/// each with its size in backing pixels.
pub(super) fn enumerate() -> Result<Vec<Source>, Unavailable> {
    let content = fetch()?;
    let mut sources = Vec::new();

    // SAFETY: plain property reads on a live snapshot.
    let displays = unsafe { content.displays() };
    for (index, display) in displays.iter().enumerate() {
        // SAFETY: as above.
        let id = unsafe { display.displayID() };
        let (width, height) = display_pixels(&display);
        sources.push(Source {
            id: SourceId(format!("{DISPLAY_PREFIX}{id}")),
            kind: SourceKind::Display,
            title: format!("Display {} ({width}×{height})", index + 1),
            width,
            height,
        });
    }

    let own_pid = i64::from(std::process::id());
    // SAFETY: plain property reads on a live snapshot.
    let windows = unsafe { content.windows() };
    for window in windows.iter() {
        // SAFETY: as above.
        let (on_screen, title, owner) = unsafe {
            (
                window.isOnScreen(),
                window.title(),
                window.owningApplication(),
            )
        };
        if !on_screen {
            continue;
        }
        let title = title.map(|title| title.to_string()).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // SAFETY: plain property read on a live object.
        let owner_pid = owner.map(|owner| i64::from(unsafe { owner.processID() }));
        if owner_pid == Some(own_pid) {
            continue;
        }
        // SAFETY: plain property read on a live object.
        let frame = unsafe { window.frame() };
        if length(frame.size.width) == 0 || length(frame.size.height) == 0 {
            continue;
        }
        let (width, height) = window_pixels(&window, &displays);
        // SAFETY: plain property read on a live object.
        let id = unsafe { window.windowID() };
        sources.push(Source {
            id: SourceId(format!("{WINDOW_PREFIX}{id}")),
            kind: SourceKind::Window,
            title,
            width,
            height,
        });
    }

    Ok(sources)
}

/// The entry `id` names inside `content`, which must be a fresh snapshot: an
/// id from an earlier one may point at a window that has since closed.
pub(super) fn find(content: &SCShareableContent, id: &SourceId) -> Result<Target, Unavailable> {
    // SAFETY: plain property reads on a live snapshot.
    let displays = unsafe { content.displays() };
    if let Some(number) = id.0.strip_prefix(DISPLAY_PREFIX) {
        let wanted: u32 = number.parse().map_err(|_| unknown(id))?;
        return displays
            .iter()
            .find(|display| unsafe { display.displayID() } == wanted)
            .map(|display| {
                let pixels = display_pixels(&display);
                Target::Display { display, pixels }
            })
            .ok_or_else(|| Unavailable::Failed("that display is gone".to_string()));
    }
    if let Some(number) = id.0.strip_prefix(WINDOW_PREFIX) {
        let wanted: u32 = number.parse().map_err(|_| unknown(id))?;
        // SAFETY: plain property reads on a live snapshot.
        let windows = unsafe { content.windows() };
        return windows
            .iter()
            .find(|window| unsafe { window.windowID() } == wanted)
            .map(|window| {
                let pixels = window_pixels(&window, &displays);
                Target::Window { window, pixels }
            })
            .ok_or_else(|| Unavailable::Failed("that window is gone".to_string()));
    }
    Err(unknown(id))
}

/// The error's own description, or a placeholder when the framework passed
/// none.
pub(super) fn describe(error: *mut NSError) -> String {
    // SAFETY: null yields `None`; a live error stays valid for the duration of
    // the handler that received it.
    match unsafe { Retained::retain(error) } {
        Some(error) => error.localizedDescription().to_string(),
        None => "no error given".to_string(),
    }
}

/// Rounded down to even, and at least 2, which is what the codec downstream
/// needs of every dimension.
pub(super) fn even(value: u32) -> u32 {
    (value & !1).max(2)
}

/// A display's size in backing pixels. The current mode is what knows the
/// backing size of a Retina display (`CGDisplayPixelsWide` reports points in
/// a HiDPI mode), so the plain query only stands in when no mode comes back,
/// and the points ScreenCaptureKit reports when even that is unknown.
fn display_pixels(display: &SCDisplay) -> (u32, u32) {
    // SAFETY: plain property reads on a live object.
    let (id, width, height) = unsafe { (display.displayID(), display.width(), display.height()) };
    let cg = CGDisplay::new(id);
    let (pixels_wide, pixels_high) = match cg.display_mode() {
        Some(mode) => (mode.pixel_width(), mode.pixel_height()),
        None => (cg.pixels_wide(), cg.pixels_high()),
    };
    (
        pixels(pixels_wide).unwrap_or_else(|| points(width)),
        pixels(pixels_high).unwrap_or_else(|| points(height)),
    )
}

/// A window's size in backing pixels: its frame in points times the scale of
/// the display holding its origin (the first display when none does), even.
fn window_pixels(window: &SCWindow, displays: &NSArray<SCDisplay>) -> (u32, u32) {
    // SAFETY: plain property read on a live object.
    let frame = unsafe { window.frame() };
    let (x, y) = (frame.origin.x, frame.origin.y);
    let scale = displays
        .iter()
        .find(|display| {
            // SAFETY: plain property read on a live object.
            let bounds = unsafe { display.frame() };
            x >= bounds.origin.x
                && x < bounds.origin.x + bounds.size.width
                && y >= bounds.origin.y
                && y < bounds.origin.y + bounds.size.height
        })
        .or_else(|| displays.firstObject())
        .map_or(1, |display| display_scale(&display));
    (
        even(length(frame.size.width * f64::from(scale))),
        even(length(frame.size.height * f64::from(scale))),
    )
}

/// Backing pixels per point, a whole number on macOS: 1, or 2 on Retina.
fn display_scale(display: &SCDisplay) -> u32 {
    // SAFETY: plain property read on a live object.
    let points_wide = points(unsafe { display.width() }).max(1);
    let (pixels_wide, _) = display_pixels(display);
    (pixels_wide / points_wide).max(1)
}

fn unknown(id: &SourceId) -> Unavailable {
    Unavailable::Failed(format!("unknown source id {:?}", id.0))
}

/// A pixel count as `u32`, `None` when the query answered zero or nonsense.
fn pixels(value: u64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
}

fn points(value: isize) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn length(value: f64) -> u32 {
    // Saturating on purpose: a negative or absurd extent becomes 0 or u32::MAX,
    // never a panic.
    value.round().max(0.0) as u32
}
