//! Windows backend: the clipboard formats, read through the Win32 API.
//!
//! Everything is read while one `OpenClipboard`/`CloseClipboard` pair is open,
//! because the clipboard is a system-wide lock: for as long as it is open no
//! other application can touch it, so the pair has to survive every early
//! return and every error. The same goes for `GlobalLock`/`GlobalUnlock`, which
//! is a count and not a flag; both live in a guard whose `Drop` closes them.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::HGLOBAL;
use windows_sys::Win32::Graphics::Gdi::{BI_BITFIELDS, BI_RGB, BITMAPV5HEADER};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

use crate::{Flavour, Pasted, Unavailable};

/// Another application holds the clipboard open for the moment it takes to read
/// or write it, and `OpenClipboard` simply fails while it does. Retrying is the
/// documented answer to that, not a workaround for a race of our own.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY: Duration = Duration::from_millis(10);

/// The registered format most applications offer a screenshot as, and the one
/// worth having: `CF_DIBV5`'s alpha channel is only as trustworthy as whoever
/// filled it in.
const PNG_FORMAT: &str = "PNG";

pub(crate) fn read() -> Result<Pasted, Unavailable> {
    let _board = Board::open()?;

    Ok(crate::pick(|flavour| match flavour {
        Flavour::Files => Ok(files().map(Pasted::Files)),
        Flavour::Png => Ok(png().map(Pasted::Png)),
        Flavour::Rgba => rgba(),
        Flavour::Text => Ok(text().map(Pasted::Text)),
    }))
}

/// Holds the clipboard open for as long as it lives.
struct Board;

impl Board {
    fn open() -> Result<Board, Unavailable> {
        for attempt in 0..OPEN_ATTEMPTS {
            // A null owner window makes this thread the owner, which is all a
            // reader needs.
            if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                return Ok(Board);
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                thread::sleep(OPEN_RETRY);
            }
        }
        Err(Unavailable::Failed(
            "another application is holding the clipboard open".to_string(),
        ))
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// A locked global block, unlocked again by `Drop`.
struct Locked {
    handle: HGLOBAL,
    bytes: *const u8,
    len: usize,
}

impl Locked {
    /// The clipboard's block for `format`, locked, or `None` when the format is
    /// not on offer.
    fn get(format: u32) -> Option<Locked> {
        if unsafe { IsClipboardFormatAvailable(format) } == 0 {
            return None;
        }
        let handle: HGLOBAL = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            return None;
        }
        let bytes = unsafe { GlobalLock(handle) }.cast::<u8>();
        if bytes.is_null() {
            return None;
        }
        let len = unsafe { GlobalSize(handle) };
        Some(Locked { handle, bytes, len })
    }

    fn as_slice(&self) -> &[u8] {
        // `GlobalLock` handed out a block of exactly `GlobalSize` bytes and the
        // lock is held for as long as this value lives.
        unsafe { std::slice::from_raw_parts(self.bytes, self.len) }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe { GlobalUnlock(self.handle) };
    }
}

fn files() -> Option<Vec<PathBuf>> {
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP)) } == 0 {
        return None;
    }
    // `DragQueryFileW` takes the handle itself and locks the block behind it,
    // so this one is not a `Locked`.
    let hdrop: HDROP = unsafe { GetClipboardData(u32::from(CF_HDROP)) };
    if hdrop.is_null() {
        return None;
    }

    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // The reported length leaves out the terminator the call writes.
        let len = unsafe { DragQueryFileW(hdrop, index, ptr::null_mut(), 0) };
        if len == 0 {
            continue;
        }
        let mut name = vec![0u16; len as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, index, name.as_mut_ptr(), name.len() as u32) };
        if written == 0 {
            continue;
        }
        name.truncate(written as usize);
        paths.push(PathBuf::from(OsString::from_wide(&name)));
    }
    (!paths.is_empty()).then_some(paths)
}

fn png() -> Option<Vec<u8>> {
    let format = registered(PNG_FORMAT)?;
    Some(Locked::get(format)?.as_slice().to_vec())
}

fn text() -> Option<String> {
    let locked = Locked::get(u32::from(CF_UNICODETEXT))?;
    let units: Vec<u16> = locked
        .as_slice()
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn rgba() -> Result<Option<Pasted>, String> {
    let Some(locked) = Locked::get(u32::from(CF_DIBV5)) else {
        return Ok(None);
    };
    decode_dib(locked.as_slice()).map(Some)
}

fn registered(name: &str) -> Option<u32> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    match unsafe { RegisterClipboardFormatW(wide.as_ptr()) } {
        0 => None,
        format => Some(format),
    }
}

/// A packed `CF_DIBV5` block turned into top-down RGBA.
fn decode_dib(bytes: &[u8]) -> Result<Pasted, String> {
    let declared = match bytes.get(..4) {
        Some(size) => u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize,
        None => return Err("the bitmap header is truncated".to_string()),
    };
    if declared < size_of::<BITMAPV5HEADER>() || bytes.len() < declared {
        return Err(format!("unexpected bitmap header size {declared}"));
    }
    // A clipboard block carries no alignment guarantee.
    let header: BITMAPV5HEADER = unsafe { ptr::read_unaligned(bytes.as_ptr().cast()) };

    if header.bV5Width <= 0 || header.bV5Height == 0 {
        return Err("the bitmap has no pixels".to_string());
    }
    let width = header.bV5Width as u32;
    let height = header.bV5Height.unsigned_abs();
    // A positive height means the rows are stored bottom to top, the way a DIB
    // has been since OS/2; a negative one means they are already top-down.
    let bottom_up = header.bV5Height > 0;

    let bits = u32::from(header.bV5BitCount);
    if !matches!(bits, 24 | 32) || !matches!(header.bV5Compression, BI_RGB | BI_BITFIELDS) {
        return Err(format!(
            "unsupported bitmap: {bits} bits, compression {}",
            header.bV5Compression
        ));
    }

    let (red, green, blue, alpha) = if header.bV5Compression == BI_BITFIELDS {
        (
            header.bV5RedMask,
            header.bV5GreenMask,
            header.bV5BlueMask,
            header.bV5AlphaMask,
        )
    } else {
        // BI_RGB packs the channels as BGR, with a reserved fourth byte at 32
        // bits that is not an alpha channel however it happens to be filled.
        (0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0)
    };

    // At 24 and 32 bits a colour table is only an optimisation palette, but it
    // still sits between the header and the pixels when there is one.
    let start = declared + header.bV5ClrUsed as usize * 4;
    let stride = (width as usize * bits as usize).div_ceil(32) * 4;
    let pixels = bytes
        .get(start..)
        .filter(|pixels| pixels.len() >= stride * height as usize)
        .ok_or_else(|| "the bitmap rows are truncated".to_string())?;

    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let source = if bottom_up {
            height as usize - 1 - row
        } else {
            row
        };
        let line = &pixels[source * stride..source * stride + stride];
        for column in 0..width as usize {
            if bits == 24 {
                let at = column * 3;
                data.extend_from_slice(&[line[at + 2], line[at + 1], line[at], u8::MAX]);
            } else {
                let at = column * 4;
                let pixel =
                    u32::from_le_bytes([line[at], line[at + 1], line[at + 2], line[at + 3]]);
                data.extend_from_slice(&[
                    channel(pixel, red),
                    channel(pixel, green),
                    channel(pixel, blue),
                    if alpha == 0 {
                        u8::MAX
                    } else {
                        channel(pixel, alpha)
                    },
                ]);
            }
        }
    }

    Ok(Pasted::Rgba {
        width,
        height,
        data,
    })
}

/// One channel of a packed pixel, scaled from the width its mask gives it up to
/// a full byte.
fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let value = (pixel & mask) >> shift;
    let full = mask >> shift;
    (value * u32::from(u8::MAX) / full) as u8
}
