//! Files and images read off the system clipboard.
//!
//! iced's own clipboard is text: `iced::clipboard::read` answers a `String` and
//! nothing else, and on Wayland it goes through smithay-clipboard, whose MIME
//! type enum is a closed set of four text flavours. Pasting a file copied in a
//! file manager, or a screenshot, therefore needs the platform clipboard
//! directly — one backend per platform, the same shape as `vorcall-hotkey`.
//!
//! A [`Clipboard`] is built once and read from as often as the user pastes.
//! Wayland is the reason it is a handle rather than a free function: there,
//! only the client holding keyboard focus may read the selection, so the
//! backend has to borrow the application's own `wl_display` — see
//! [`Clipboard::new_wayland`]. Every other platform needs nothing and is built
//! with [`Clipboard::new`].

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) mod uri;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

use std::ffi::c_void;
use std::fmt;
use std::path::PathBuf;

/// What the clipboard is currently offering, in the order the composer cares
/// about.
#[derive(Clone, PartialEq, Eq)]
pub enum Pasted {
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
    /// An image already encoded as PNG by whoever put it there.
    Png(Vec<u8>),
    /// A raw bitmap, which the caller encodes itself.
    Rgba {
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    Text(String),
    Nothing,
}

/// Summarised, never dumped: the image arms carry megabytes and the text arm
/// carries whatever the user last copied, neither of which belongs in a log.
impl fmt::Debug for Pasted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pasted::Files(paths) => write!(formatter, "Files({} paths)", paths.len()),
            Pasted::Png(bytes) => write!(formatter, "Png({} bytes)", bytes.len()),
            Pasted::Rgba { width, height, .. } => write!(formatter, "Rgba({width}x{height})"),
            Pasted::Text(text) => write!(formatter, "Text({} chars)", text.chars().count()),
            Pasted::Nothing => formatter.write_str("Nothing"),
        }
    }
}

/// Which mechanism a [`Clipboard`] reads through, so the interface can say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    WindowsOle,
    MacPasteboard,
    X11Selection,
    WaylandSeat,
}

/// Why the clipboard could not be reached.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// The platform or session has no clipboard this crate can reach — there is
    /// nothing to retry.
    #[error("{0}")]
    Unsupported(String),
    /// The user has to grant something before this can work.
    #[error("{0}")]
    PermissionDenied(String),
    #[error("{0}")]
    Failed(String),
}

/// A way in to the system clipboard, built once and read from repeatedly.
///
/// `Send`, so the application can build it on the interface thread and read
/// from a blocking one.
pub struct Clipboard {
    backend: Backend,
    inner: Inner,
}

enum Inner {
    // Boxed because the two Linux backends carry a connection each and would
    // otherwise size this enum for the Windows and macOS builds too. A
    // `Clipboard` is built once and lives as long as the window, so the
    // allocation costs nothing.
    #[cfg(target_os = "linux")]
    Wayland(Box<wayland::Wayland>),
    #[cfg(target_os = "linux")]
    X11(Box<x11::X11>),
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    Mac,
    /// Nothing to hold: both constructors answer `Unsupported` on a platform
    /// with no backend, so this variant is never built.
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    Unsupported,
}

impl Clipboard {
    /// The clipboard of an X11, Windows or macOS session, none of which needs a
    /// handle on the window.
    ///
    /// On Linux this is always the X11 `CLIPBOARD` selection: a Wayland session
    /// has no clipboard a handle-less reader can reach, so it goes through
    /// [`Clipboard::new_wayland`] instead.
    pub fn new() -> Result<Clipboard, Unavailable> {
        #[cfg(windows)]
        {
            Ok(Clipboard {
                backend: Backend::WindowsOle,
                inner: Inner::Windows,
            })
        }

        #[cfg(target_os = "macos")]
        {
            Ok(Clipboard {
                backend: Backend::MacPasteboard,
                inner: Inner::Mac,
            })
        }

        #[cfg(target_os = "linux")]
        {
            Ok(Clipboard {
                backend: Backend::X11Selection,
                inner: Inner::X11(Box::new(x11::X11::new()?)),
            })
        }

        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            Err(Unavailable::Unsupported("unsupported platform".to_string()))
        }
    }

    /// The clipboard of a Wayland session, read on the application's own
    /// connection.
    ///
    /// Only the client holding keyboard focus may read a Wayland selection, and
    /// the window the user pressed the paste shortcut in is that client — so
    /// this borrows its display rather than opening a connection of its own.
    ///
    /// # Safety
    ///
    /// `display` must be a valid `*mut wl_display`, and it must stay valid for
    /// as long as the returned `Clipboard` lives. The display is borrowed, never
    /// owned: dropping the `Clipboard` does not disconnect it.
    pub unsafe fn new_wayland(display: *mut c_void) -> Result<Clipboard, Unavailable> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the caller's promise about `display` is passed straight
            // through to the one place that dereferences it.
            let wayland = unsafe { wayland::Wayland::new(display) }?;
            Ok(Clipboard {
                backend: Backend::WaylandSeat,
                inner: Inner::Wayland(Box::new(wayland)),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = display;
            Err(Unavailable::Unsupported(
                "Wayland is Linux only".to_string(),
            ))
        }
    }

    /// Reads the clipboard once.
    ///
    /// Blocking: every backend talks to a display server, a compositor or
    /// another application and waits for the answer, so the caller runs this on
    /// a blocking thread and never on the interface thread.
    ///
    /// An empty clipboard is [`Pasted::Nothing`], not an error. A path that no
    /// longer exists is still returned — whether a file is still there is the
    /// caller's question, and nothing here touches the filesystem.
    pub fn read(&self) -> Result<Pasted, Unavailable> {
        match &self.inner {
            #[cfg(target_os = "linux")]
            Inner::Wayland(wayland) => wayland.read(),
            #[cfg(target_os = "linux")]
            Inner::X11(x11) => x11.read(),
            #[cfg(windows)]
            Inner::Windows => windows::read(),
            #[cfg(target_os = "macos")]
            Inner::Mac => macos::read(),
            #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
            Inner::Unsupported => Err(Unavailable::Unsupported("unsupported platform".to_string())),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }
}

/// One kind of content the composer knows what to do with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavour {
    Files,
    Png,
    Rgba,
    Text,
}

/// The flavours in the order the composer prefers them: the files themselves
/// over a picture of them, an already-encoded image over a raw one, and text
/// last because almost everything also offers a textual rendering of itself.
pub(crate) const PRIORITY: [Flavour; 4] =
    [Flavour::Files, Flavour::Png, Flavour::Rgba, Flavour::Text];

/// Takes the first flavour the clipboard both offers and hands over.
///
/// `read` answers `Ok(None)` for a flavour that is simply not on offer and an
/// `Err` for one that is offered but unreadable; the latter is logged and
/// skipped rather than fatal, so a broken image offer costs only itself and the
/// text behind it still comes through.
pub(crate) fn pick(mut read: impl FnMut(Flavour) -> Result<Option<Pasted>, String>) -> Pasted {
    for flavour in PRIORITY {
        match read(flavour) {
            Ok(Some(pasted)) => return pasted,
            Ok(None) => (),
            Err(reason) => {
                tracing::debug!(?flavour, %reason, "skipping an unreadable clipboard flavour");
            }
        }
    }
    Pasted::Nothing
}

#[cfg(test)]
mod tests {
    use super::Clipboard;

    fn assert_send<T: Send>() {}

    /// The application builds the handle where it has the window and reads from
    /// a blocking thread, so this is a contract, not an accident.
    #[test]
    fn the_handle_can_move_to_a_blocking_thread() {
        assert_send::<Clipboard>();
    }
}
