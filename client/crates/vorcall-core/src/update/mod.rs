//! Signed self-update: fetch the release manifest, check it against the keys
//! baked into this build, download the release for this platform, verify it,
//! and put it in the place of the running binary.
//!
//! UI-free like the rest of the crate: the app and the probe drive it and
//! decide what to show; this module only reports progress and outcomes.

pub mod client;
pub mod hash;
pub mod keys;
pub mod manifest;
pub mod pending;
pub mod swap;
pub mod version;

use std::fs;
use std::path::{Path, PathBuf};

pub use keys::PublicKey;
pub use manifest::{Asset, Manifest};
pub use version::Version;

use crate::endpoints::Endpoints;
use crate::http::ApiFailure;

/// How a build names itself in the manifest, e.g. `linux-x86_64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Everything a check needs that does not change between checks.
#[derive(Clone)]
pub struct Checker {
    pub endpoints: Endpoints,
    pub keys: Vec<PublicKey>,
    pub current: Version,
    pub platform: String,
}

#[derive(Clone, Debug)]
pub enum Progress {
    Checking,
    /// `required` comes from the manifest, so it is known before the first byte
    /// arrives and the UI can pick its screen right away.
    Downloading {
        version: Version,
        received: u64,
        total: u64,
        required: bool,
    },
}

#[derive(Debug)]
pub enum Outcome {
    UpToDate { manifest: Manifest },
    NoBuildForPlatform { manifest: Manifest },
    Downloaded(Ready),
}

/// A verified release waiting on disk. `manual` means it landed in the user's
/// data directory because the install directory is not writable, so it is
/// never applied automatically.
#[derive(Clone, Debug)]
pub struct Ready {
    pub manifest: Manifest,
    pub asset: Asset,
    pub file: PathBuf,
    pub manual: bool,
    pub required: bool,
}

/// No `Display` here ever carries a token, a header value or key material.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("updates are off: {0}")]
    Disabled(String),
    #[error(transparent)]
    Api(#[from] ApiFailure),
    #[error("the manifest is not signed by a trusted key")]
    Signature,
    #[error("the baked-in update keys are unusable: {0}")]
    Keys(String),
    #[error("the manifest is unusable: {0}")]
    Manifest(String),
    #[error("{0}")]
    Version(String),
    #[error("the download is {actual} bytes, the manifest says {expected}")]
    Size { expected: u64, actual: u64 },
    #[error("the download hashes to {actual}, the manifest says {expected}")]
    Hash { expected: String, actual: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Swap(String),
}

/// One full check: manifest, signature, then — if there is something newer for
/// this platform — the download, verified and left ready for [`swap`].
pub async fn check_and_download(
    checker: &Checker,
    access_token: &str,
    mut progress: impl FnMut(Progress) + Send,
) -> Result<Outcome, UpdateError> {
    progress(Progress::Checking);

    let (bytes, signature) = client::fetch_manifest(&checker.endpoints, access_token).await?;
    let manifest = manifest::verify_and_parse(&bytes, &signature, &checker.keys)?;

    if manifest.version <= checker.current {
        return Ok(Outcome::UpToDate { manifest });
    }
    let required = manifest.min_version > checker.current;

    let Some(asset) = manifest.platforms.get(&checker.platform).cloned() else {
        return Ok(Outcome::NoBuildForPlatform { manifest });
    };

    let dir = swap::install_dir()?;
    let version = manifest.version;

    let installed = swap::pending_path(&dir, &version);
    if reuse_verified(&installed, &asset) {
        return Ok(Outcome::Downloaded(Ready {
            manifest,
            asset,
            file: installed,
            manual: false,
            required,
        }));
    }

    let (reserved, part, manual) = swap::writable_target(&dir, &version)?;
    // The handle only claimed the name; the download opens the path itself.
    drop(reserved);
    let pending = swap::pending_path(part.parent().unwrap_or(&dir), &version);

    // The fallback directory keeps downloads of its own, and the install
    // directory was already ruled out above.
    if manual && reuse_verified(&pending, &asset) {
        let _ = fs::remove_file(&part);
        return Ok(Outcome::Downloaded(Ready {
            manifest,
            asset,
            file: pending,
            manual: true,
            required,
        }));
    }

    let downloaded = client::download_asset(
        &checker.endpoints,
        access_token,
        &version,
        &asset,
        &part,
        |received, total| {
            progress(Progress::Downloading {
                version,
                received,
                total,
                required,
            });
        },
    )
    .await
    .and_then(|()| seal(&bytes, &signature, &part, &pending));

    if let Err(e) = downloaded {
        let _ = fs::remove_file(&part);
        pending::discard(&pending);
        return Err(e);
    }

    Ok(Outcome::Downloaded(Ready {
        manifest,
        asset,
        file: pending,
        manual,
        required,
    }))
}

/// Why this build will not update itself, in the order the answer matters.
pub fn disabled_reason(
    endpoints: &Endpoints,
    keys: &[PublicKey],
    debug_build: bool,
) -> Option<String> {
    if endpoints.is_dev_key() {
        return Some("dev server key".to_owned());
    }
    if debug_build {
        return Some("debug build".to_owned());
    }
    if std::env::var_os("VORCALL_NO_UPDATE").is_some() {
        return Some("VORCALL_NO_UPDATE is set".to_owned());
    }
    if keys.is_empty() {
        return Some("no update keys baked in".to_owned());
    }
    None
}

/// Stores the manifest the download was verified against, then gives the file
/// its real name: a file under the pending name has always been verified.
fn seal(bytes: &[u8], signature: &str, part: &Path, pending: &Path) -> Result<(), UpdateError> {
    fs::write(swap::manifest_path(pending), bytes)?;
    fs::write(
        swap::signature_path(pending),
        format!("{}\n", signature.trim()),
    )?;
    fs::rename(part, pending)?;
    Ok(())
}

/// Whether a download an earlier run finished can be handed over as it is: it
/// was verified before it got its name. Anything else under that name never
/// was, so it goes here instead of surviving into the next check.
fn reuse_verified(pending: &Path, asset: &Asset) -> bool {
    if is_verified_copy(pending, asset) {
        return true;
    }
    pending::discard(pending);
    false
}

/// Whether a pending file from an earlier run, manifest included, is still
/// exactly what this manifest describes. `symlink_metadata` because the file
/// must be the download itself, not a pointer at something else on disk.
fn is_verified_copy(pending: &Path, asset: &Asset) -> bool {
    let Ok(metadata) = fs::symlink_metadata(pending) else {
        return false;
    };

    metadata.is_file()
        && metadata.len() == asset.size
        && swap::manifest_path(pending).exists()
        && swap::signature_path(pending).exists()
        && hash::sha256_file_hex(pending)
            .is_ok_and(|digest| digest.eq_ignore_ascii_case(&asset.sha256))
}

#[cfg(test)]
pub(crate) mod testing {
    //! What the unit tests of this module tree share: a throwaway signing key
    //! and a temporary directory of their own.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    use super::PublicKey;

    pub(crate) struct Signer {
        pair: Ed25519KeyPair,
    }

    impl Signer {
        pub(crate) fn generate() -> Self {
            let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .expect("should generate a key pair");
            let pair =
                Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("should parse the key pair");
            Self { pair }
        }

        pub(crate) fn public(&self) -> PublicKey {
            let mut key = [0u8; 32];
            key.copy_from_slice(self.pair.public_key().as_ref());
            PublicKey(key)
        }

        pub(crate) fn sign_hex(&self, bytes: &[u8]) -> String {
            hex::encode(self.pair.sign(bytes))
        }
    }

    pub(crate) fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "vorcall-core-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("should create the test directory");
        dir
    }

    pub(crate) fn remove_dir(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints(key: &str) -> Endpoints {
        Endpoints::parse("https://vorcall.example", key).expect("should parse")
    }

    fn a_key() -> PublicKey {
        PublicKey([7u8; 32])
    }

    /// The FIPS 180-2 test vector for "abc", which is what `plant` writes.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn abc_asset() -> Asset {
        Asset {
            path: "vorcall".to_owned(),
            sha256: ABC_SHA256.to_owned(),
            size: 3,
        }
    }

    /// Writes what a finished download leaves behind: the payload and, unless
    /// `signed` says otherwise, both side files.
    fn plant(dir: &Path, payload: &[u8], signed: bool) -> PathBuf {
        let version: Version = "9.9.9".parse().expect("should parse");
        let pending = swap::pending_path(dir, &version);

        fs::write(&pending, payload).expect("should write the payload");
        fs::write(swap::manifest_path(&pending), b"{}").expect("should write the manifest");
        if signed {
            fs::write(swap::signature_path(&pending), b"00\n").expect("should write the signature");
        }
        pending
    }

    #[test]
    fn a_verified_download_is_reused_where_it_lies() {
        let dir = testing::temp_dir("reuse-ok");
        let pending = plant(&dir, b"abc", true);

        assert!(reuse_verified(&pending, &abc_asset()));
        assert!(pending.exists(), "the download should be left alone");
        assert!(swap::manifest_path(&pending).exists());
        assert!(swap::signature_path(&pending).exists());

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_download_without_a_signature_is_discarded() {
        let dir = testing::temp_dir("reuse-unsigned");
        let pending = plant(&dir, b"abc", false);

        assert!(!reuse_verified(&pending, &abc_asset()));
        assert!(!pending.exists());
        assert!(!swap::manifest_path(&pending).exists());

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_download_that_does_not_hash_is_discarded() {
        let dir = testing::temp_dir("reuse-tampered");
        let pending = plant(&dir, b"abd", true);

        assert!(!reuse_verified(&pending, &abc_asset()));
        assert!(!pending.exists());
        assert!(!swap::signature_path(&pending).exists());

        testing::remove_dir(&dir);
    }

    #[test]
    fn the_platform_id_is_os_dash_arch() {
        let platform = platform();

        assert!(platform.contains('-'), "{platform} should be os-arch");
        assert!(platform.starts_with(std::env::consts::OS));
        assert!(platform.ends_with(std::env::consts::ARCH));
    }

    /// It runs inside an iced `Task`, so the future has to cross threads. The
    /// assertion is the type check; the future is never polled.
    #[test]
    fn the_check_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}

        let checker = Checker {
            endpoints: endpoints("a-real-key"),
            keys: vec![a_key()],
            current: Version::current(),
            platform: platform(),
        };
        let future = check_and_download(&checker, "token", |_| {});

        assert_send(&future);
    }

    #[test]
    fn a_dev_key_disables_updates() {
        assert_eq!(
            disabled_reason(&endpoints("dev"), &[a_key()], false).as_deref(),
            Some("dev server key")
        );
    }

    #[test]
    fn a_debug_build_disables_updates() {
        assert_eq!(
            disabled_reason(&endpoints("a-real-key"), &[a_key()], true).as_deref(),
            Some("debug build")
        );
    }

    #[test]
    fn no_baked_keys_disables_updates() {
        assert_eq!(
            disabled_reason(&endpoints("a-real-key"), &[], false).as_deref(),
            Some("no update keys baked in")
        );
    }

    #[test]
    fn a_release_build_with_keys_is_enabled() {
        assert_eq!(
            disabled_reason(&endpoints("a-real-key"), &[a_key()], false),
            None
        );
    }
}
