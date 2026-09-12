//! Images on this machine: the bytes cached on disk, the decode that turns them
//! into pixels iced can draw, and the pruning that keeps the cache from growing
//! for good.
//!
//! Decoding never runs on the UI thread: every entry point here is called from a
//! blocking task.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::SystemTime;

use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder as _, ImageFormat};
use vorcall_core::images::ImagePurpose;

/// The longest side anything cached is drawn at. A phone photograph is several
/// times that, and every pixel above it is a texture nobody sees.
pub const MAX_SIDE: u32 = 1600;

/// How much of the cache survives a start.
pub const CACHE_LIMIT: u64 = 500 << 20;

/// Which image one cache entry is. Attachment and image ids are both
/// server-assigned and independent of each other, so the kind is part of the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImageKey {
    Attachment(i64),
    Image(i64),
}

impl ImageKey {
    /// The file this entry is cached as.
    pub fn file_name(self) -> String {
        match self {
            Self::Attachment(id) => format!("attachment-{id}"),
            Self::Image(id) => format!("image-{id}"),
        }
    }
}

/// Where the downloaded bytes live between runs; `None` when the platform
/// exposes no cache directory at all, which only turns the cache off.
pub fn cache_dir() -> Option<PathBuf> {
    vorcall_core::config::cache_dir().map(|dir| dir.join("images"))
}

/// The file one entry is cached as.
pub fn cached_path(key: ImageKey) -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(key.file_name()))
}

/// Writes the bytes of one image to the cache. Best effort: a cache that cannot
/// be written only means the next run downloads again.
pub fn store(key: ImageKey, bytes: &[u8]) {
    let (Some(dir), Some(path)) = (cache_dir(), cached_path(key)) else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::debug!(error = %e, "cannot create the image cache directory");
        return;
    }
    if let Err(e) = std::fs::write(&path, bytes) {
        tracing::debug!(?key, error = %e, "cannot cache an image");
    }
}

/// How large an image of one purpose is uploaded, which is also the box the
/// profile pages scale a picked file into before it leaves the machine.
pub fn upload_box(purpose: ImagePurpose) -> (u32, u32) {
    purpose.max_size()
}

/// Decodes one image into the RGBA8 pixels `iced::widget::image::Handle` takes,
/// downscaled so its longest side is at most `max_side`.
///
/// Profile images are already inside their own box ([`upload_box`]) when they
/// are uploaded, so this ceiling is a guard rather than the policy.
pub fn decode(bytes: &[u8], max_side: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;

    let (width, height) = (decoded.width(), decoded.height());
    let longest = width.max(height);
    let decoded = if max_side > 0 && longest > max_side {
        let scale = f64::from(max_side) / f64::from(longest);
        let width = ((f64::from(width) * scale).round() as u32).max(1);
        let height = ((f64::from(height) * scale).round() as u32).max(1);
        decoded.resize(width, height, FilterType::Triangle)
    } else {
        decoded
    };

    let rgba = decoded.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Re-encodes RGBA8 pixels as a PNG. Lossless, which is what a picture that has
/// already been through one lossy encoder deserves.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new(Cursor::new(&mut png))
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|e| e.to_string())?;
    Ok(png)
}

/// Scales a picked file into the box its purpose allows and re-encodes it, so
/// nothing larger than what will ever be drawn leaves this machine. Answers the
/// content type the upload must declare along with the bytes.
///
/// A GIF that already fits goes as it came: decoding one keeps its first frame
/// only, and an animated avatar that arrived still is not what was picked.
pub fn resize_for_upload(
    bytes: &[u8],
    purpose: ImagePurpose,
) -> Result<(&'static str, Vec<u8>), String> {
    let (box_width, box_height) = upload_box(purpose);
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return Err("the image has no pixels".to_string());
    }

    let fits = width <= box_width && height <= box_height;
    if fits && matches!(image::guess_format(bytes), Ok(ImageFormat::Gif)) {
        return Ok(("image/gif", bytes.to_vec()));
    }

    // `resize` fits the box by the smaller of the two ratios, which is below one
    // whenever a side is over it: the aspect ratio is kept and nothing grows.
    let scaled = if fits {
        decoded
    } else {
        decoded.resize(box_width, box_height, FilterType::Triangle)
    };

    let rgba = scaled.to_rgba8();
    let png = encode_png(rgba.width(), rgba.height(), rgba.as_raw())?;
    Ok(("image/png", png))
}

/// Deletes the oldest cached files until the cache fits `limit_bytes`.
pub fn prune(limit_bytes: u64) {
    let Some(dir) = cache_dir() else {
        return;
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A cache that was never written is the common case on a first run.
        Err(e) => {
            tracing::debug!(error = %e, "no image cache to prune");
            return;
        }
    };

    let mut cached: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        cached.push((entry.path(), modified, metadata.len()));
    }

    let victims = victims(&mut cached, limit_bytes);
    if victims.is_empty() {
        return;
    }

    let mut removed = 0usize;
    for path in &victims {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) => tracing::debug!(error = %e, "cannot remove a cached image"),
        }
    }
    tracing::info!(
        removed,
        kept = cached.len() - removed,
        "pruned the image cache"
    );
}

/// Which files a prune deletes: the oldest first, until what is left fits.
/// `cached` is `(path, last modified, size)` and is sorted in place.
fn victims(cached: &mut [(PathBuf, SystemTime, u64)], limit_bytes: u64) -> Vec<PathBuf> {
    let mut total: u64 = cached.iter().map(|(_, _, size)| *size).sum();
    if total <= limit_bytes {
        return Vec::new();
    }

    cached.sort_by_key(|(_, modified, _)| *modified);
    let mut victims = Vec::new();
    for (path, _, size) in cached.iter() {
        if total <= limit_bytes {
            break;
        }
        total = total.saturating_sub(*size);
        victims.push(path.clone());
    }
    victims
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn entry(name: &str, age_secs: u64, size: u64) -> (PathBuf, SystemTime, u64) {
        (
            PathBuf::from(name),
            SystemTime::UNIX_EPOCH + Duration::from_secs(age_secs),
            size,
        )
    }

    /// A PNG of the asked-for size, with a gradient so a resize has something to
    /// interpolate.
    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = image::RgbaImage::new(width, height);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, 0x40, 0xFF]);
        }
        encode_png(width, height, pixels.as_raw()).expect("the fixture encodes")
    }

    /// Speed 30 is the fastest quantisation the encoder offers; a fixture needs
    /// no palette quality. The encoder is dropped before the bytes are read: its
    /// own `Drop` is what closes the file.
    fn gif_fixture(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![0x80u8; (width as usize) * (height as usize) * 4];
        let mut gif = Vec::new();
        {
            let mut encoder =
                image::codecs::gif::GifEncoder::new_with_speed(Cursor::new(&mut gif), 30);
            encoder
                .encode(&pixels, width, height, ExtendedColorType::Rgba8)
                .expect("the fixture encodes");
        }
        gif
    }

    /// 2:1 inside 1600×600 is height-bound: 600 tall, so 1200 wide.
    #[test]
    fn a_banner_is_scaled_into_its_own_box() {
        let (content_type, scaled) =
            resize_for_upload(&png_fixture(2000, 1000), ImagePurpose::Banner)
                .expect("the banner is scaled");

        assert_eq!(content_type, "image/png");
        let image = image::load_from_memory(&scaled).expect("the scaled banner decodes");
        assert_eq!((image.width(), image.height()), (1200, 600));
    }

    #[test]
    fn an_image_inside_the_box_keeps_its_size() {
        let (content_type, encoded) =
            resize_for_upload(&png_fixture(100, 100), ImagePurpose::Avatar)
                .expect("the avatar is accepted");

        assert_eq!(content_type, "image/png");
        let image = image::load_from_memory(&encoded).expect("the avatar decodes");
        assert_eq!((image.width(), image.height()), (100, 100));
    }

    /// An animation survives only as the file it arrived in, and only while it
    /// fits.
    #[test]
    fn a_gif_that_fits_is_uploaded_as_it_came() {
        let gif = gif_fixture(64, 64);
        let (content_type, uploaded) =
            resize_for_upload(&gif, ImagePurpose::Avatar).expect("the gif is accepted");
        assert_eq!(content_type, "image/gif");
        assert_eq!(uploaded, gif);

        let (content_type, _) = resize_for_upload(&gif_fixture(600, 600), ImagePurpose::Avatar)
            .expect("the oversized gif is scaled");
        assert_eq!(content_type, "image/png");
    }

    #[test]
    fn a_decode_caps_the_longest_side() {
        let (width, height, pixels) =
            decode(&png_fixture(2000, 1000), MAX_SIDE).expect("the image decodes");
        // 1600/2000 is 0.8, so 1000 rows become 800.
        assert_eq!((width, height), (MAX_SIDE, 800));
        assert_eq!(pixels.len(), MAX_SIDE as usize * 800 * 4);

        let (width, height, _) = decode(&png_fixture(64, 32), MAX_SIDE).expect("the image decodes");
        assert_eq!((width, height), (64, 32));
    }

    #[test]
    fn a_cache_under_the_limit_loses_nothing() {
        let mut cached = vec![entry("a", 3, 10), entry("b", 1, 10)];
        assert!(victims(&mut cached, 100).is_empty());
    }

    /// Oldest first, and only as many as it takes to fit.
    #[test]
    fn the_oldest_files_go_until_the_rest_fits() {
        let mut cached = vec![
            entry("newest", 30, 40),
            entry("oldest", 10, 40),
            entry("middle", 20, 40),
        ];

        assert_eq!(
            victims(&mut cached, 50),
            vec![PathBuf::from("oldest"), PathBuf::from("middle")]
        );
    }

    #[test]
    fn a_limit_of_nothing_empties_the_cache() {
        let mut cached = vec![entry("a", 2, 1), entry("b", 1, 1)];
        assert_eq!(
            victims(&mut cached, 0),
            vec![PathBuf::from("b"), PathBuf::from("a")]
        );
    }

    /// Two ids of different kinds are two entries, never one.
    #[test]
    fn a_key_names_one_file_per_kind() {
        assert_eq!(ImageKey::Attachment(7).file_name(), "attachment-7");
        assert_eq!(ImageKey::Image(7).file_name(), "image-7");
        assert_ne!(
            ImageKey::Attachment(7).file_name(),
            ImageKey::Image(7).file_name()
        );
    }

    #[test]
    fn a_profile_image_is_uploaded_inside_its_own_box() {
        assert_eq!(upload_box(ImagePurpose::Avatar), (512, 512));
        assert_eq!(upload_box(ImagePurpose::Banner), (1600, 600));
        assert_eq!(upload_box(ImagePurpose::ServerIcon), (512, 512));
        assert_eq!(upload_box(ImagePurpose::RoleIcon), (128, 128));
    }
}
