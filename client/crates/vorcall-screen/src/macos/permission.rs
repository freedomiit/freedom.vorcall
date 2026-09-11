//! The macOS floor and the Screen Recording grant, checked before anything
//! touches ScreenCaptureKit.

use core_graphics::access::ScreenCaptureAccess;
use objc2_foundation::NSProcessInfo;

use crate::Unavailable;

/// ScreenCaptureKit arrived in 12.3 but the window filters and audio capture
/// this backend relies on only settled in 13.
const MIN_MAJOR_VERSION: isize = 13;

pub(super) const GRANT: &str = "Grant Screen Recording to Vorcall in System Settings › Privacy & \
                                Security › Screen Recording, then relaunch Vorcall";

/// Fails on a macOS that is too old or without the Screen Recording grant.
///
/// A missing grant is requested, which opens the system prompt only the first
/// time (afterwards Vorcall already sits in the Screen Recording list, so a
/// second request from `start` after `enumerate` shows nothing). The grant
/// takes effect on the next launch, so the answer is a failure either way.
pub(super) fn check() -> Result<(), Unavailable> {
    let version = NSProcessInfo::processInfo().operatingSystemVersion();
    if version.majorVersion < MIN_MAJOR_VERSION {
        return Err(Unavailable::Unsupported(
            "screen share needs macOS 13".to_string(),
        ));
    }

    let access = ScreenCaptureAccess;
    if !access.preflight() {
        access.request();
        return Err(Unavailable::PermissionDenied(GRANT.to_string()));
    }
    Ok(())
}
