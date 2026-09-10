//! The offline half of the updater: what a previous run left in the install
//! directory, verified again from scratch before the next start trusts it.

use std::fs;
use std::path::{Path, PathBuf};

use super::{PublicKey, Version, hash, manifest, swap};

/// The first pending download that still verifies against the baked keys, is
/// newer than `current` and matches the manifest byte for byte. Anything that
/// does not is deleted, so a broken download never survives two starts.
pub fn take_verified(
    dir: &Path,
    keys: &[PublicKey],
    current: &Version,
    platform: &str,
) -> Option<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "cannot list the install directory");
            return None;
        }
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(rest) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(swap::PENDING_PREFIX))
        else {
            continue;
        };

        if rest.ends_with(".part") {
            let _ = fs::remove_file(&path);
            continue;
        }
        if rest.ends_with(".manifest.json") || rest.ends_with(".manifest.sig") {
            continue;
        }
        candidates.push(path);
    }

    for path in candidates {
        match check(&path, keys, current, platform) {
            Ok(()) => {
                tracing::debug!(file = %path.display(), "a verified update is waiting");
                return Some(path);
            }
            Err(reason) => {
                tracing::warn!(file = %path.display(), %reason, "discarding a pending update");
                discard(&path);
            }
        }
    }

    None
}

/// Deletes a pending download and the manifest it came with.
pub(crate) fn discard(pending: &Path) {
    let _ = fs::remove_file(pending);
    let _ = fs::remove_file(swap::manifest_path(pending));
    let _ = fs::remove_file(swap::signature_path(pending));
}

/// `Err` carries why the trio was rejected: never any file content, only names
/// and sizes.
fn check(
    pending: &Path,
    keys: &[PublicKey],
    current: &Version,
    platform: &str,
) -> Result<(), String> {
    let bytes = fs::read(swap::manifest_path(pending))
        .map_err(|e| format!("the manifest is unreadable: {e}"))?;
    let signature = fs::read_to_string(swap::signature_path(pending))
        .map_err(|e| format!("the signature is unreadable: {e}"))?;

    let manifest =
        manifest::verify_and_parse(&bytes, &signature, keys).map_err(|e| e.to_string())?;

    if manifest.version <= *current {
        return Err(format!(
            "version {} is not newer than {current}",
            manifest.version
        ));
    }

    let asset = manifest
        .platforms
        .get(platform)
        .ok_or_else(|| format!("the manifest carries no {platform} build"))?;

    // Symlink metadata: the file must be the download itself, not a pointer at
    // something else on disk.
    let size =
        fs::symlink_metadata(pending).map_err(|e| format!("the download is unreadable: {e}"))?;
    if !size.is_file() {
        return Err("the download is not a regular file".to_owned());
    }
    if size.len() != asset.size {
        return Err(format!(
            "the download is {} bytes, the manifest says {}",
            size.len(),
            asset.size
        ));
    }

    let digest = hash::sha256_file_hex(pending).map_err(|e| format!("cannot hash it: {e}"))?;
    if !digest.eq_ignore_ascii_case(&asset.sha256) {
        return Err("the download does not match the manifest digest".to_owned());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testing::{self, Signer};

    /// A platform id no host reports, so the test never depends on where it runs.
    const PLATFORM: &str = "testos-testarch";
    /// The FIPS 180-2 test vector for "abc", the payload every trio carries.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn version(raw: &str) -> Version {
        raw.parse().expect("should parse")
    }

    fn manifest_json(release: &str, platform: &str, sha: &str, size: u64) -> String {
        format!(
            "{{\n  \"version\": \"{release}\",\n  \"notes\": \"\",\n  \"published_at\": \"2026-09-10T18:00:00Z\",\n  \"min_version\": \"0.1.0\",\n  \"platforms\": {{\n    \"{platform}\": {{ \"path\": \"vorcall\", \"sha256\": \"{sha}\", \"size\": {size} }}\n  }}\n}}\n"
        )
    }

    /// Writes a pending trio: the payload, the manifest describing it and the
    /// signature over the exact manifest bytes.
    fn plant(dir: &Path, signer: &Signer, release: &str, payload: &[u8], json: &str) -> PathBuf {
        let pending = swap::pending_path(dir, &version(release));
        fs::write(&pending, payload).expect("should write the payload");
        fs::write(swap::manifest_path(&pending), json.as_bytes())
            .expect("should write the manifest");
        fs::write(
            swap::signature_path(&pending),
            format!("{}\n", signer.sign_hex(json.as_bytes())),
        )
        .expect("should write the signature");
        pending
    }

    fn trio_is_gone(pending: &Path) -> bool {
        !pending.exists()
            && !swap::manifest_path(pending).exists()
            && !swap::signature_path(pending).exists()
    }

    #[test]
    fn returns_a_verified_download_and_leaves_it_alone() {
        let dir = testing::temp_dir("pending-ok");
        let signer = Signer::generate();
        let json = manifest_json("9.9.9", PLATFORM, ABC, 3);
        let pending = plant(&dir, &signer, "9.9.9", b"abc", &json);

        let found = take_verified(&dir, &[signer.public()], &version("0.2.0"), PLATFORM);

        assert_eq!(found.as_deref(), Some(pending.as_path()));
        assert!(pending.exists());
        assert!(swap::manifest_path(&pending).exists());
        assert!(swap::signature_path(&pending).exists());

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_tampered_download_is_deleted() {
        let dir = testing::temp_dir("pending-tampered");
        let signer = Signer::generate();
        let json = manifest_json("9.9.9", PLATFORM, ABC, 3);
        let pending = plant(&dir, &signer, "9.9.9", b"abd", &json);

        let found = take_verified(&dir, &[signer.public()], &version("0.2.0"), PLATFORM);

        assert!(found.is_none());
        assert!(trio_is_gone(&pending));

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_forged_manifest_is_deleted() {
        let dir = testing::temp_dir("pending-forged");
        let signer = Signer::generate();
        let stranger = Signer::generate();
        let json = manifest_json("9.9.9", PLATFORM, ABC, 3);
        let pending = plant(&dir, &stranger, "9.9.9", b"abc", &json);

        let found = take_verified(&dir, &[signer.public()], &version("0.2.0"), PLATFORM);

        assert!(found.is_none());
        assert!(trio_is_gone(&pending));

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_download_that_is_not_newer_is_deleted() {
        let dir = testing::temp_dir("pending-old");
        let signer = Signer::generate();
        let json = manifest_json("0.2.0", PLATFORM, ABC, 3);
        let pending = plant(&dir, &signer, "0.2.0", b"abc", &json);

        let found = take_verified(&dir, &[signer.public()], &version("0.2.0"), PLATFORM);

        assert!(found.is_none());
        assert!(trio_is_gone(&pending));

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_download_for_another_platform_is_deleted() {
        let dir = testing::temp_dir("pending-platform");
        let signer = Signer::generate();
        let json = manifest_json("9.9.9", "otheros-otherarch", ABC, 3);
        let pending = plant(&dir, &signer, "9.9.9", b"abc", &json);

        let found = take_verified(&dir, &[signer.public()], &version("0.2.0"), PLATFORM);

        assert!(found.is_none());
        assert!(trio_is_gone(&pending));

        testing::remove_dir(&dir);
    }

    #[test]
    fn stray_part_files_are_swept_away() {
        let dir = testing::temp_dir("pending-part");
        let part = swap::part_path(&swap::pending_path(&dir, &version("9.9.9")));
        fs::write(&part, b"half a download").expect("should write");
        let unrelated = dir.join("notes.txt");
        fs::write(&unrelated, b"keep me").expect("should write");

        let found = take_verified(&dir, &[], &version("0.2.0"), PLATFORM);

        assert!(found.is_none());
        assert!(!part.exists());
        assert!(unrelated.exists(), "only our own files are swept");

        testing::remove_dir(&dir);
    }
}
