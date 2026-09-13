//! The files this client has offered as streamed files, in `streamed.json`
//! beside `config.toml`.
//!
//! A streamed file is served by the sender's own client, so a sender that
//! restarts still has to know which local file each stream id stands for, and
//! whether that file is still the one it offered. That is all this registry
//! holds: the path, the size and the modification time as of the offer.
//!
//! Every function here touches the disk synchronously, like `vorcall-app`'s
//! theme files: call them from a blocking thread.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The file the registry lives in.
pub const FILE_NAME: &str = "streamed.json";

/// Where the registry is stored; `None` when the platform exposes no
/// configuration directory, which only means no offer survives a restart.
pub fn path() -> Option<PathBuf> {
    let config = crate::config::path()?;
    Some(config.parent()?.join(FILE_NAME))
}

/// One offered file: where to read its bytes, and what it looked like when the
/// offer went out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The local file the bytes are read from.
    pub path: PathBuf,
    /// Its length in bytes at the time of the offer.
    pub size: u64,
    /// Its modification time at the time of the offer, in microseconds since
    /// the Unix epoch, negative before it. Only ever compared with itself, so
    /// the unit is a matter of resolution alone: microseconds are finer than
    /// any filesystem's own stamp and cannot overflow an `i64`.
    pub modified_us: i64,
}

/// What every stream id this client has offered is served from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    files: BTreeMap<i64, Entry>,
}

/// Whether a stream id may still be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The file is still the one that was offered; its bytes are at this path.
    Unchanged(PathBuf),
    /// No entry, no file, or a file that is no longer the one offered.
    Gone,
}

impl Registry {
    /// Records what `id` is served from. `modified` is the file's modification
    /// time as the offer measured it.
    pub fn insert(&mut self, id: i64, path: PathBuf, size: u64, modified: SystemTime) {
        self.files.insert(
            id,
            Entry {
                path,
                size,
                modified_us: micros(modified),
            },
        );
    }

    pub fn get(&self, id: i64) -> Option<&Entry> {
        self.files.get(&id)
    }

    pub fn remove(&mut self, id: i64) -> Option<Entry> {
        self.files.remove(&id)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Whether `id` may still be served, and from where.
    ///
    /// A stream id must never serve bytes out of a file that has changed since
    /// the offer: a reader asks for a byte range, and the same range of a
    /// different file is silently the wrong data — somebody else's, in the
    /// worst case. The recorded size and modification time are what say the
    /// file is still the one offered; hashing is not an option, since the file
    /// may be tens of gigabytes and an offer has to be instant.
    ///
    /// Anything short of a match — no entry, no file, a file that cannot be
    /// stat'd, one that is no longer a file, a different length, a stamp that
    /// moved — is [`Verdict::Gone`], and the caller declines the transfer.
    pub fn verify(&self, id: i64) -> Verdict {
        let Some(entry) = self.files.get(&id) else {
            return Verdict::Gone;
        };
        let Ok(metadata) = std::fs::metadata(&entry.path) else {
            return Verdict::Gone;
        };
        if !metadata.is_file() || metadata.len() != entry.size {
            return Verdict::Gone;
        }
        match metadata.modified() {
            Ok(modified) if micros(modified) == entry.modified_us => {
                Verdict::Unchanged(entry.path.clone())
            }
            _ => Verdict::Gone,
        }
    }
}

/// Reads the registry. A first run has no file, and a broken one is not worth
/// failing over: either way nothing is servable, which is exactly what an empty
/// registry says.
pub fn load() -> Registry {
    let Some(path) = path() else {
        tracing::debug!("no configuration directory: offered files will not survive a restart");
        return Registry::default();
    };
    load_from(&path)
}

/// Writes the registry, creating the configuration directory on the way.
pub fn save(registry: &Registry) -> anyhow::Result<()> {
    let path = path().context("there is no configuration directory")?;
    save_to(&path, registry)
}

fn load_from(path: &Path) -> Registry {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // Nobody has offered a file yet, which is the common case.
        Err(e) if e.kind() == ErrorKind::NotFound => return Registry::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read the offered files");
            return Registry::default();
        }
    };

    match serde_json::from_str(&raw) {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "the offered files are unreadable, starting empty");
            Registry::default()
        }
    }
}

fn save_to(path: &Path, registry: &Registry) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(registry).context("cannot encode the offered files")?;
    std::fs::write(path, body).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn micros(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_micros()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_micros()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;

    /// A directory of this test's own, removed however the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "vorcall-registry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Self(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).expect("a scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What the file on disk actually is, which is what an offer records.
    fn stat(path: &Path) -> (u64, SystemTime) {
        let metadata = std::fs::metadata(path).expect("the file");
        (
            metadata.len(),
            metadata.modified().expect("a modification time"),
        )
    }

    #[test]
    fn verify_accepts_the_file_that_was_offered() {
        let scratch = Scratch::new();
        let path = scratch.file("offered.bin", b"twelve bytes");
        let (size, modified) = stat(&path);

        let mut registry = Registry::default();
        registry.insert(7, path.clone(), size, modified);

        assert_eq!(registry.verify(7), Verdict::Unchanged(path));
    }

    #[test]
    fn verify_refuses_a_file_whose_size_changed() {
        let scratch = Scratch::new();
        let path = scratch.file("offered.bin", b"twelve bytes");
        let (size, modified) = stat(&path);

        // One byte longer than what is on disk: the size is the only field that
        // disagrees.
        let mut registry = Registry::default();
        registry.insert(7, path, size + 1, modified);

        assert_eq!(registry.verify(7), Verdict::Gone);
    }

    #[test]
    fn verify_refuses_a_file_whose_modification_time_moved() {
        let scratch = Scratch::new();
        let path = scratch.file("offered.bin", b"twelve bytes");
        let (size, modified) = stat(&path);

        // A second past what is on disk: the stamp is the only field that
        // disagrees.
        let mut registry = Registry::default();
        registry.insert(7, path, size, modified + Duration::from_secs(1));

        assert_eq!(registry.verify(7), Verdict::Gone);
    }

    #[test]
    fn verify_refuses_what_it_does_not_have() {
        let scratch = Scratch::new();
        let path = scratch.file("offered.bin", b"twelve bytes");
        let (size, modified) = stat(&path);

        let mut registry = Registry::default();
        registry.insert(7, path.clone(), size, modified);

        // An id that was never offered, and a directory rather than a file.
        assert_eq!(registry.verify(404), Verdict::Gone);
        registry.insert(8, scratch.0.clone(), size, modified);
        assert_eq!(registry.verify(8), Verdict::Gone);

        std::fs::remove_file(&path).expect("the file goes");
        assert_eq!(registry.verify(7), Verdict::Gone);
    }

    #[test]
    fn the_file_round_trips() {
        let scratch = Scratch::new();
        let offered = scratch.file("offered.bin", b"twelve bytes");
        let (size, modified) = stat(&offered);
        let store = scratch.0.join("nested").join(FILE_NAME);

        let mut registry = Registry::default();
        registry.insert(7, offered.clone(), size, modified);
        registry.insert(9, offered.clone(), size, modified);
        registry.remove(9);
        save_to(&store, &registry).expect("the registry is written");

        let read = load_from(&store);
        assert_eq!(read.len(), 1);
        assert_eq!(read.get(7), registry.get(7));
        assert_eq!(read.get(9), None);
        assert_eq!(read.verify(7), Verdict::Unchanged(offered));
    }

    #[test]
    fn a_broken_or_missing_file_reads_as_empty() {
        let scratch = Scratch::new();

        let broken = scratch.file("broken.json", b"{ this is not json");
        assert!(load_from(&broken).is_empty());

        // A well-formed document of the wrong shape is just as broken.
        let wrong = scratch.file("wrong.json", br#"{"files": 3}"#);
        assert!(load_from(&wrong).is_empty());

        assert!(load_from(&scratch.0.join("nothing.json")).is_empty());
    }
}
