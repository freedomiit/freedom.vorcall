//! Assembling a release manifest from the built files, and checking a manifest
//! that already exists against a signature.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use vorcall_core::update::{Asset, Manifest, PublicKey, hash, manifest};

use crate::args::ManifestSpec;

pub fn build(spec: &ManifestSpec) -> Result<Manifest> {
    let mut platforms = BTreeMap::new();
    for (platform, path) in &spec.assets {
        platforms.insert(platform.clone(), asset(path)?);
    }

    let notes = match &spec.notes_file {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?,
        None => String::new(),
    };

    let manifest = Manifest {
        version: spec.version,
        notes,
        published_at: spec
            .published_at
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        min_version: spec.min_version,
        platforms,
    };
    manifest.validate().map_err(|e| anyhow!("{e}"))?;

    Ok(manifest)
}

pub fn write(manifest: &Manifest, out: &Path) -> Result<()> {
    let json =
        serde_json::to_string_pretty(manifest).context("could not serialise the manifest")?;

    fs::write(out, format!("{json}\n"))
        .with_context(|| format!("could not write {}", out.display()))
}

/// The version the manifest names, once the signature checks out.
pub fn verify(manifest: &Path, signature: &Path, keys: &[PublicKey]) -> Result<String> {
    let bytes =
        fs::read(manifest).with_context(|| format!("could not read {}", manifest.display()))?;
    let sig = fs::read_to_string(signature)
        .with_context(|| format!("could not read {}", signature.display()))?;

    let parsed = manifest::verify_and_parse(&bytes, sig.trim(), keys)
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    Ok(parsed.version.to_string())
}

/// The manifest names a file inside its own release directory, so only the
/// basename of the built file survives here.
fn asset(path: &Path) -> Result<Asset> {
    let metadata =
        fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a file", path.display());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("{} has no usable file name", path.display()))?;

    Ok(Asset {
        path: name.to_owned(),
        sha256: hash::sha256_file_hex(path)
            .with_context(|| format!("could not hash {}", path.display()))?,
        size: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ManifestSpec;
    use crate::testing;
    use std::path::PathBuf;

    const PAYLOAD: &[u8] = b"vorcall release payload";

    fn spec(dir: &Path, version: &str, min_version: &str, asset: PathBuf) -> ManifestSpec {
        ManifestSpec {
            version: version.parse().expect("should parse"),
            min_version: min_version.parse().expect("should parse"),
            published_at: Some("2026-09-10T18:00:00Z".to_owned()),
            notes_file: None,
            assets: vec![("linux-x86_64".to_owned(), asset)],
            out: dir.join("manifest.json"),
        }
    }

    #[test]
    fn an_asset_carries_the_basename_size_and_hash() {
        let dir = testing::temp_dir("manifest-asset");
        let nested = dir.join("dist").join("linux");
        fs::create_dir_all(&nested).expect("should create");
        let file = nested.join("vorcall-linux-x86_64");
        fs::write(&file, PAYLOAD).expect("should write");

        let spec = spec(&dir, "0.3.0", "0.2.0", file.clone());
        let built = build(&spec).expect("should build");
        write(&built, &spec.out).expect("should write");

        let text = fs::read_to_string(&spec.out).expect("should read");
        assert!(text.ends_with("}\n"), "{text}");
        assert!(!text.ends_with("}\n\n"), "{text}");
        assert!(text.contains("\n  \"version\": \"0.3.0\","), "{text}");

        let parsed: Manifest =
            serde_json::from_str(&text).expect("should parse back into a manifest");
        parsed.validate().expect("should validate");

        let asset = parsed
            .platforms
            .get("linux-x86_64")
            .expect("should carry the platform");
        assert_eq!(asset.path, "vorcall-linux-x86_64");
        assert_eq!(asset.size, PAYLOAD.len() as u64);
        assert_eq!(asset.sha256, hash::sha256_hex(PAYLOAD));
        assert_eq!(parsed.notes, "");
        assert_eq!(parsed.published_at, "2026-09-10T18:00:00Z");

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_min_version_above_the_version_fails() {
        let dir = testing::temp_dir("manifest-min-version");
        let file = dir.join("vorcall-linux-x86_64");
        fs::write(&file, PAYLOAD).expect("should write");

        let error = build(&spec(&dir, "0.2.0", "0.3.0", file))
            .expect_err("should refuse")
            .to_string();
        assert!(error.contains("min_version"), "{error}");

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_missing_asset_file_fails() {
        let dir = testing::temp_dir("manifest-missing");
        let missing = dir.join("nowhere");

        assert!(build(&spec(&dir, "0.2.0", "0.2.0", missing)).is_err());

        testing::remove_dir(&dir);
    }

    #[test]
    fn notes_come_from_the_notes_file() {
        let dir = testing::temp_dir("manifest-notes");
        let file = dir.join("vorcall-linux-x86_64");
        fs::write(&file, PAYLOAD).expect("should write");
        let notes = dir.join("notes.md");
        fs::write(&notes, "fixed the voice relay\n").expect("should write");

        let mut spec = spec(&dir, "0.3.0", "0.2.0", file);
        spec.notes_file = Some(notes);
        let built = build(&spec).expect("should build");

        assert_eq!(built.notes, "fixed the voice relay\n");

        spec.notes_file = Some(dir.join("absent.md"));
        assert!(build(&spec).is_err());

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_missing_published_at_defaults_to_now() {
        let dir = testing::temp_dir("manifest-published-at");
        let file = dir.join("vorcall-linux-x86_64");
        fs::write(&file, PAYLOAD).expect("should write");

        let mut spec = spec(&dir, "0.3.0", "0.2.0", file);
        spec.published_at = None;
        let built = build(&spec).expect("should build");

        assert_eq!(built.published_at.len(), "2026-09-10T18:00:00Z".len());
        assert!(built.published_at.ends_with('Z'), "{}", built.published_at);

        testing::remove_dir(&dir);
    }
}
