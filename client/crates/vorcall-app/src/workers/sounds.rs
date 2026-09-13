//! Soundpad clips on this machine: the bytes cached on disk, the parse that
//! turns them back into samples, and the pruning that keeps the cache from
//! growing for good.
//!
//! A clip's bytes never change — a re-recorded slot is a new clip with a new id
//! — so a cached file is good until it is pruned, and a download is only ever
//! needed once per clip per machine. Fetching one is the app layer's business;
//! nothing here touches the network.
//!
//! Decoding never runs on the UI thread: every entry point here is called from a
//! blocking task.

use std::path::PathBuf;
use std::time::SystemTime;

use vorcall_voice::SoundClip;

/// How much of the clip cache survives a start.
pub const CACHE_LIMIT: u64 = 200 << 20;

/// Where the downloaded bytes live between runs; `None` when the platform
/// exposes no cache directory at all, which only turns the cache off.
pub fn cache_dir() -> Option<PathBuf> {
    vorcall_core::config::cache_dir().map(|dir| dir.join("sounds"))
}

/// The file one clip is cached as.
pub fn cached_path(id: i64) -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(format!("{id}.vcsnd")))
}

/// Writes the bytes of one clip to the cache. Best effort: a cache that cannot
/// be written only means the next run downloads again.
pub fn store(id: i64, bytes: &[u8]) {
    let (Some(dir), Some(path)) = (cache_dir(), cached_path(id)) else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::debug!(error = %e, "cannot create the sound cache directory");
        return;
    }
    if let Err(e) = std::fs::write(&path, bytes) {
        tracing::debug!(id, error = %e, "cannot cache a sound");
    }
}

/// Drops one clip from the cache. Best effort, like everything else here: a file
/// that cannot be removed is only downloaded again next time.
pub fn forget(id: i64) {
    let Some(path) = cached_path(id) else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        tracing::debug!(id, error = %e, "cannot drop a cached sound");
    }
}

/// The bytes of one cached clip. A miss and an unreadable file are the same
/// answer: not cached.
pub fn load(id: i64) -> Option<Vec<u8>> {
    let path = cached_path(id)?;
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::debug!(id, error = %e, "a sound is not cached");
            None
        }
    }
}

/// Parses a cached or freshly downloaded clip into interleaved stereo 48 kHz.
///
/// A cached file that no longer parses is the same as a miss to the caller, so
/// the message is for a toast and a log rather than a decision.
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let clip =
        SoundClip::parse(bytes).map_err(|error| format!("that sound is damaged: {error}"))?;
    clip.decode_to_pcm()
        .map_err(|error| format!("that sound could not be decoded: {error}"))
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
            tracing::debug!(error = %e, "no sound cache to prune");
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
            Err(e) => tracing::debug!(error = %e, "cannot remove a cached sound"),
        }
    }
    tracing::info!(
        removed,
        kept = cached.len() - removed,
        "pruned the sound cache"
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

    use vorcall_voice::codec::STEREO_FRAME_SAMPLES;

    use super::*;

    fn entry(name: &str, age_secs: u64, size: u64) -> (PathBuf, SystemTime, u64) {
        (
            PathBuf::from(name),
            SystemTime::UNIX_EPOCH + Duration::from_secs(age_secs),
            size,
        )
    }

    /// A real clip of `frames` 20 ms frames, quiet but not silent.
    fn clip_bytes(frames: usize) -> Vec<u8> {
        let pcm: Vec<f32> = (0..frames * STEREO_FRAME_SAMPLES)
            .map(|index| if index % 4 < 2 { 0.25 } else { -0.25 })
            .collect();
        SoundClip::encode(&pcm)
            .expect("the fixture encodes")
            .to_bytes()
    }

    /// One file per id, under the sounds cache, never shared with the images one.
    #[test]
    fn a_clip_is_cached_under_its_own_id() {
        let Some(path) = cached_path(7) else {
            return;
        };
        let name = path.file_name().and_then(|name| name.to_str());
        assert_eq!(name, Some("7.vcsnd"));
        assert_eq!(path.parent(), cache_dir().as_deref());
        assert_ne!(cached_path(7), cached_path(8));
    }

    /// The ids are negative so nothing a running client cached can collide with
    /// them; the files are removed either way.
    #[test]
    fn a_stored_clip_comes_back() {
        let id = -424_242;
        let Some(path) = cached_path(id) else {
            return;
        };

        let bytes = clip_bytes(2);
        store(id, &bytes);
        let loaded = load(id);
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.as_deref(), Some(bytes.as_slice()));
    }

    /// What a damaged cached file gets: the fetch drops it and downloads again.
    #[test]
    fn a_forgotten_clip_is_a_miss_again() {
        let id = -424_244;
        if cached_path(id).is_none() {
            return;
        }

        store(id, &clip_bytes(1));
        assert!(load(id).is_some());
        forget(id);

        assert_eq!(load(id), None);
        // A clip that is not there is not an error either.
        forget(id);
    }

    #[test]
    fn a_clip_that_was_never_cached_is_a_miss() {
        assert_eq!(load(-424_243), None);
    }

    /// A cached file someone truncated must read as a message, never a panic.
    #[test]
    fn garbage_is_refused_with_a_message() {
        for bytes in [b"not a sound at all".as_slice(), &[], &clip_bytes(1)[..10]] {
            let error = decode(bytes).expect_err("garbage is refused");
            assert!(
                error.starts_with("that sound is damaged"),
                "unhelpful message: {error}"
            );
        }
    }

    #[test]
    fn a_real_clip_decodes_to_stereo_frames() {
        let decoded = decode(&clip_bytes(3)).expect("the clip decodes");
        assert_eq!(decoded.len(), 3 * STEREO_FRAME_SAMPLES);
    }

    #[test]
    fn a_cache_under_the_limit_loses_nothing() {
        let mut cached = vec![entry("a", 3, 10), entry("b", 1, 10)];
        assert!(victims(&mut cached, CACHE_LIMIT).is_empty());
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
}
