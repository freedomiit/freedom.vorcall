//! The release manifest: what it looks like, and how bytes claiming to be one
//! become a [`Manifest`] — signature first, JSON second.

use std::collections::BTreeMap;

use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use super::{PublicKey, UpdateError, Version};

/// Unknown fields are ignored on purpose: a newer manifest must still be
/// readable by an older client, which is the client that needs to update.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: Version,
    #[serde(default)]
    pub notes: String,
    pub published_at: String,
    pub min_version: Version,
    pub platforms: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asset {
    /// A bare filename under `releases/<version>/`, never a path.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

impl Manifest {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.min_version > self.version {
            return Err(UpdateError::Manifest(format!(
                "min_version {} is above version {}",
                self.min_version, self.version
            )));
        }

        for (platform, asset) in &self.platforms {
            if !is_bare_filename(&asset.path) {
                return Err(UpdateError::Manifest(format!(
                    "{platform}: {:?} is not a bare filename",
                    asset.path
                )));
            }
            if !is_sha256_hex(&asset.sha256) {
                return Err(UpdateError::Manifest(format!(
                    "{platform}: sha256 is not 64 lowercase hexadecimal characters"
                )));
            }
        }

        Ok(())
    }
}

/// The signature covers the exact bytes the server sent, so it is checked
/// before serde_json ever looks at them: an unsigned manifest must not reach
/// the parser, let alone the downloader.
pub fn verify_and_parse(
    bytes: &[u8],
    sig_hex: &str,
    keys: &[PublicKey],
) -> Result<Manifest, UpdateError> {
    let signature = hex::decode(sig_hex.trim()).map_err(|_| UpdateError::Signature)?;
    if signature.len() != 64 {
        return Err(UpdateError::Signature);
    }

    // An empty key list verifies nothing, which is how a build with no baked
    // keys refuses every manifest.
    let trusted = keys.iter().any(|key| {
        UnparsedPublicKey::new(&ED25519, &key.0)
            .verify(bytes, &signature)
            .is_ok()
    });
    if !trusted {
        return Err(UpdateError::Signature);
    }

    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|e| UpdateError::Manifest(e.to_string()))?;
    manifest.validate()?;

    Ok(manifest)
}

/// A manifest entry may only name a file inside its own release directory.
pub(crate) fn is_bare_filename(path: &str) -> bool {
    !path.is_empty()
        && path != "."
        && !path.contains('/')
        && !path.contains('\\')
        && !path.contains("..")
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testing::Signer;

    const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    const MANIFEST: &str = r#"{
  "version": "0.3.0",
  "notes": "second release",
  "published_at": "2026-09-10T18:00:00Z",
  "min_version": "0.2.0",
  "platforms": {
    "linux-x86_64": {
      "path": "vorcall-linux-x86_64",
      "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
      "size": 12345678
    }
  }
}
"#;

    fn signed(json: &str, signer: &Signer) -> Result<Manifest, UpdateError> {
        verify_and_parse(
            json.as_bytes(),
            &signer.sign_hex(json.as_bytes()),
            &[signer.public()],
        )
    }

    #[test]
    fn parses_a_signed_manifest() {
        let signer = Signer::generate();
        let manifest = signed(MANIFEST, &signer).expect("should verify and parse");

        assert_eq!(manifest.version.to_string(), "0.3.0");
        assert_eq!(manifest.min_version.to_string(), "0.2.0");
        assert_eq!(manifest.notes, "second release");
        assert_eq!(manifest.published_at, "2026-09-10T18:00:00Z");
        assert_eq!(manifest.platforms.len(), 1);

        let asset = manifest
            .platforms
            .get("linux-x86_64")
            .expect("the linux asset should be there");
        assert_eq!(asset.path, "vorcall-linux-x86_64");
        assert_eq!(asset.sha256, SHA);
        assert_eq!(asset.size, 12_345_678);
    }

    #[test]
    fn a_single_flipped_byte_breaks_the_signature() {
        let signer = Signer::generate();
        let signature = signer.sign_hex(MANIFEST.as_bytes());

        let mut tampered = MANIFEST.as_bytes().to_vec();
        tampered[10] ^= 0x01;

        let error = verify_and_parse(&tampered, &signature, &[signer.public()])
            .expect_err("a tampered manifest must not verify");
        assert!(matches!(error, UpdateError::Signature));
    }

    #[test]
    fn another_key_does_not_verify() {
        let signer = Signer::generate();
        let stranger = Signer::generate();
        let signature = signer.sign_hex(MANIFEST.as_bytes());

        let error = verify_and_parse(MANIFEST.as_bytes(), &signature, &[stranger.public()])
            .expect_err("a foreign key must not verify");
        assert!(matches!(error, UpdateError::Signature));
    }

    #[test]
    fn an_empty_key_list_verifies_nothing() {
        let signer = Signer::generate();
        let signature = signer.sign_hex(MANIFEST.as_bytes());

        let error = verify_and_parse(MANIFEST.as_bytes(), &signature, &[])
            .expect_err("no keys means no trust");
        assert!(matches!(error, UpdateError::Signature));
    }

    #[test]
    fn a_malformed_signature_is_rejected_before_parsing() {
        let signer = Signer::generate();

        for bad in [
            "".to_owned(),
            "not-hex".to_owned(),
            "ab".to_owned(),
            "a".repeat(126),
        ] {
            let error = verify_and_parse(MANIFEST.as_bytes(), &bad, &[signer.public()])
                .expect_err("a malformed signature must be refused");
            assert!(matches!(error, UpdateError::Signature), "for {bad:?}");
        }
    }

    #[test]
    fn a_min_version_above_the_version_is_refused() {
        let signer = Signer::generate();
        let json = MANIFEST.replace("\"min_version\": \"0.2.0\"", "\"min_version\": \"9.0.0\"");

        let error = signed(&json, &signer).expect_err("min_version must not exceed version");
        assert!(matches!(error, UpdateError::Manifest(_)));
    }

    #[test]
    fn a_path_that_is_not_a_bare_filename_is_refused() {
        let signer = Signer::generate();

        for path in [
            "sub/vorcall",
            "..\\vorcall",
            "../vorcall",
            "vorcall..exe",
            "",
            ".",
        ] {
            let json = MANIFEST.replace("vorcall-linux-x86_64", path);
            let error =
                signed(&json, &signer).expect_err("a path with a separator must be refused");
            assert!(matches!(error, UpdateError::Manifest(_)), "for {path:?}");
        }
    }

    #[test]
    fn a_sha256_that_is_not_64_lowercase_hex_is_refused() {
        let signer = Signer::generate();

        for sha in [
            SHA[1..].to_owned(),
            SHA.to_uppercase(),
            format!("{SHA}00"),
            "not a hash".to_owned(),
        ] {
            let json = MANIFEST.replace(SHA, &sha);
            let error = signed(&json, &signer).expect_err("a bad digest must be refused");
            assert!(matches!(error, UpdateError::Manifest(_)), "for {sha:?}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let signer = Signer::generate();
        let json = MANIFEST.replace(
            "\"notes\": \"second release\",",
            "\"notes\": \"second release\",\n  \"channel\": \"beta\",\n  \"future\": { \"x\": 1 },",
        );

        let manifest = signed(&json, &signer).expect("a newer manifest must still parse");
        assert_eq!(manifest.version.to_string(), "0.3.0");
    }

    #[test]
    fn notes_default_to_empty() {
        let signer = Signer::generate();
        let json = MANIFEST.replace("\"notes\": \"second release\",", "");

        let manifest = signed(&json, &signer).expect("notes are optional");
        assert!(manifest.notes.is_empty());
    }

    #[test]
    fn a_missing_required_field_is_an_error() {
        let signer = Signer::generate();
        let json = MANIFEST.replace("\"published_at\": \"2026-09-10T18:00:00Z\",", "");

        let error = signed(&json, &signer).expect_err("published_at is required");
        assert!(matches!(error, UpdateError::Manifest(_)));
    }
}
