//! What this machine has to offer: the monitors `EnumDisplayMonitors` walks,
//! in that order, and the top-level windows of other processes that actually
//! have something on screen.

use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetClientRect, GetWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

use crate::{Source, SourceId, SourceKind};

/// A display, as Windows describes it.
pub(super) struct Monitor {
    pub(super) handle: HMONITOR,
    /// The adapter's own name for the display, `\\.\DISPLAY1` and the like.
    pub(super) name: String,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// A window that can be shared.
pub(super) struct Window {
    pub(super) handle: HWND,
    pub(super) title: String,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// Every display first, then every shareable window. A display's position in
/// this list is the index its [`SourceId`] carries, so the order has to be the
/// one [`monitors`] reproduces at start.
pub(super) fn sources() -> Vec<Source> {
    let mut sources: Vec<Source> = monitors()
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| Source {
            id: SourceId(format!("monitor:{index}")),
            kind: SourceKind::Display,
            title: monitor.name,
            width: monitor.width,
            height: monitor.height,
        })
        .collect();

    sources.extend(windows().into_iter().map(|window| Source {
        id: SourceId(format!("window:{}", handle_id(window.handle))),
        kind: SourceKind::Window,
        title: window.title,
        width: window.width,
        height: window.height,
    }));

    sources
}

pub(super) fn handle_id(window: HWND) -> u64 {
    window.0 as usize as u64
}

pub(super) fn handle_from_id(id: u64) -> HWND {
    HWND(id as usize as *mut c_void)
}

pub(super) fn monitors() -> Vec<Monitor> {
    let mut found: Vec<Monitor> = Vec::new();
    // SAFETY: `collect_monitor` is only ever called back during this call, and
    // the pointer it is handed is this live local.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut found as isize),
        );
    }
    found
}

pub(super) fn windows() -> Vec<Window> {
    let mut found: Vec<Window> = Vec::new();
    // SAFETY: as above — the callback runs inside this call and writes only
    // through the pointer to this live local.
    let _ = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut found as isize)) };
    found
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _context: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<Monitor>` that `monitors` passed, alive
    // for the whole enumeration and untouched by anything else meanwhile.
    let found = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `cbSize` says the buffer is the wider `MONITORINFOEXW`, which is
    // what it is.
    let read =
        unsafe { GetMonitorInfoW(handle, std::ptr::from_mut(&mut info).cast::<MONITORINFO>()) };
    if read.as_bool() {
        let bounds = info.monitorInfo.rcMonitor;
        found.push(Monitor {
            handle,
            name: trim_utf16(&info.szDevice),
            width: bounds.right.saturating_sub(bounds.left).max(0) as u32,
            height: bounds.bottom.saturating_sub(bounds.top).max(0) as u32,
        });
    }

    TRUE
}

unsafe extern "system" fn collect_window(handle: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<Window>` that `windows` passed, alive for
    // the whole enumeration.
    let found = unsafe { &mut *(data.0 as *mut Vec<Window>) };
    // SAFETY: `handle` is the window the enumeration is currently offering, so
    // every query below is made while it is still alive.
    if let Some(window) = unsafe { describe(handle) } {
        found.push(window);
    }
    TRUE
}

/// The filter that separates a window a friend would recognise from the
/// hundreds of invisible, owned, cloaked and off-screen ones Windows keeps.
///
/// # Safety
///
/// `handle` must be a live window handle.
unsafe fn describe(handle: HWND) -> Option<Window> {
    // SAFETY: the caller guarantees a live handle for all of these.
    unsafe {
        if !IsWindowVisible(handle).as_bool() {
            return None;
        }
        // A tool window, a dialog or a popup belongs to whatever owns it; the
        // owner is what the user means by "that application".
        if GetWindow(handle, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return None;
        }

        // Store apps leave their suspended windows visible but cloaked, which
        // is the only way to tell they are not on screen.
        let mut cloaked = 0u32;
        let asked = DwmGetWindowAttribute(
            handle,
            DWMWA_CLOAKED,
            std::ptr::from_mut(&mut cloaked).cast::<std::ffi::c_void>(),
            size_of::<u32>() as u32,
        );
        if asked.is_ok() && cloaked != 0 {
            return None;
        }

        // Sharing our own window back into the call is a hall of mirrors.
        let mut process = 0u32;
        GetWindowThreadProcessId(handle, Some(&mut process));
        if process == GetCurrentProcessId() {
            return None;
        }

        let length = GetWindowTextLengthW(handle);
        if length <= 0 {
            return None;
        }
        let mut title = vec![0u16; length as usize + 1];
        let written = GetWindowTextW(handle, &mut title);
        if written <= 0 {
            return None;
        }
        let title = String::from_utf16_lossy(&title[..written as usize]);

        let mut client = RECT::default();
        GetClientRect(handle, &mut client).ok()?;
        let width = client.right.saturating_sub(client.left);
        let height = client.bottom.saturating_sub(client.top);
        if width <= 0 || height <= 0 {
            return None;
        }

        Some(Window {
            handle,
            title,
            width: width as u32,
            height: height as u32,
        })
    }
}

fn trim_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|unit| *unit == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}
