//! macOS backend: the general `NSPasteboard`.
//!
//! Everything comes back as an autoreleased object the runtime already owns, so
//! `Retained` is only ever received here, never created by hand — there is
//! nothing for this backend to balance under the Create rule.
//!
//! TIFF is deliberately left alone: it is the other image type the pasteboard
//! offers and decoding it is not this crate's job, so a TIFF-only offer falls
//! through to whatever text sits beside it.

use std::path::PathBuf;

use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSFilenamesPboardType, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
    NSPasteboardTypeString,
};
use objc2_foundation::{NSArray, NSString};

use crate::{Flavour, Pasted, Unavailable, uri};

pub(crate) fn read() -> Result<Pasted, Unavailable> {
    let pasteboard = NSPasteboard::generalPasteboard();

    Ok(crate::pick(|flavour| match flavour {
        Flavour::Files => Ok(files(&pasteboard).map(Pasted::Files)),
        Flavour::Png => Ok(pasteboard
            // SAFETY: reading a pasteboard type constant the framework owns.
            .dataForType(unsafe { NSPasteboardTypePNG })
            .map(|data| Pasted::Png(data.to_vec()))),
        // The pasteboard hands out encoded images, never a raw bitmap.
        Flavour::Rgba => Ok(None),
        Flavour::Text => Ok(pasteboard
            // SAFETY: as above.
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|text| Pasted::Text(text.to_string()))),
    }))
}

fn files(pasteboard: &NSPasteboard) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();

    if let Some(items) = pasteboard.pasteboardItems() {
        for index in 0..items.count() {
            // SAFETY: reading a pasteboard type constant the framework owns.
            let Some(url) = items
                .objectAtIndex(index)
                .stringForType(unsafe { NSPasteboardTypeFileURL })
            else {
                continue;
            };
            let url = url.to_string();
            match uri::to_path(&url) {
                Some(path) => paths.push(path),
                None => tracing::debug!(uri = url, "skipping a pasteboard URL that is not a file"),
            }
        }
    }

    if paths.is_empty() {
        paths = filenames(pasteboard);
    }
    (!paths.is_empty()).then_some(paths)
}

/// The legacy property list of plain paths, which applications older than the
/// per-item file URL still copy files as.
#[allow(deprecated)]
fn filenames(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    // SAFETY: reading a pasteboard type constant the framework owns.
    let Some(list) = pasteboard.propertyListForType(unsafe { NSFilenamesPboardType }) else {
        return Vec::new();
    };
    // The element type of a collection cannot be checked at run time, so the
    // downcast lands on the erased array and each element is checked in turn.
    let Some(names) = list.downcast_ref::<NSArray<AnyObject>>() else {
        return Vec::new();
    };
    (0..names.count())
        .filter_map(|index| {
            let element = names.objectAtIndex(index);
            let name = element.downcast_ref::<NSString>()?;
            Some(PathBuf::from(name.to_string()))
        })
        .collect()
}
