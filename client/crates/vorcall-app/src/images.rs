//! Image attachments on this machine: the bytes cached on disk, the decode that
//! turns them into pixels iced can draw, and the pruning that keeps the cache
//! from growing for good.
//!
//! Decoding never runs on the UI thread: every entry point here is called from
//! a blocking task.

use std::path::PathBuf;
use std::time::SystemTime;

use image::imageops::FilterType;

/// The longest side an attachment is drawn at. A phone photograph is several
/// times that, and every pixel above it is a texture nobody sees.
const MAX_SIDE: u32 = 1600;

/// Where the downloaded bytes live between runs; `None` when the platform
/// exposes no cache directory at all, which only turns the cache off.
pub fn cache_dir() -> Option<PathBuf> {
    vorcall_core::config::cache_dir().map(|dir| dir.join("attachments"))
}

/// The file one attachment is cached as. Ids are server-assigned, so the id
/// alone names the file.
pub fn cached_path(id: i64) -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(id.to_string()))
}

/// Writes the bytes of one attachment to the cache. Best effort: a cache that
/// cannot be written only means the next run downloads again.
pub fn store(id: i64, bytes: &[u8]) {
    let (Some(dir), Some(path)) = (cache_dir(), cached_path(id)) else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::debug!(error = %e, "cannot create the attachment cache directory");
        return;
    }
    if let Err(e) = std::fs::write(&path, bytes) {
        tracing::debug!(id, error = %e, "cannot cache an attachment");
    }
}

/// Decodes one image into the RGBA8 pixels [`iced::widget::image::Handle`]
/// takes, downscaled to [`MAX_SIDE`].
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;

    let (width, height) = (decoded.width(), decoded.height());
    let longest = width.max(height);
    let decoded = if longest > MAX_SIDE {
        let scale = f64::from(MAX_SIDE) / f64::from(longest);
        let width = ((f64::from(width) * scale).round() as u32).max(1);
        let height = ((f64::from(height) * scale).round() as u32).max(1);
        decoded.resize(width, height, FilterType::Triangle)
    } else {
        decoded
    };

    let rgba = decoded.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
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
            tracing::debug!(error = %e, "no attachment cache to prune");
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
            Err(e) => tracing::debug!(error = %e, "cannot remove a cached attachment"),
        }
    }
    tracing::info!(
        removed,
        kept = cached.len() - removed,
        "pruned the attachment cache"
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
}
