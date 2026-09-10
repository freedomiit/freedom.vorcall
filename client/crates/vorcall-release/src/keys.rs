//! Key generation, loading and signing. Nothing here ever prints, logs or
//! returns key material in an error.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use vorcall_core::update::{PublicKey, keys};

use crate::args::KeySource;

/// What a bad private key says, whatever went wrong with it: hex, length or
/// PKCS#8 structure. The value itself never reaches the message.
const BAD_KEY: &str = "the signing key is not a valid hex PKCS#8 document";

/// Writes the private key and returns the public key, both as lowercase hex.
/// The private half is the PKCS#8 v2 document ring emits, not the raw seed:
/// `Ed25519KeyPair::from_pkcs8` is the only way back to a usable key pair.
pub fn generate(out: &Path) -> Result<String> {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow!("could not generate a key pair"))?;
    let pair = Ed25519KeyPair::from_pkcs8(document.as_ref())
        .map_err(|_| anyhow!("could not parse the generated key pair"))?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = match options.open(out) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("refusing to overwrite {}", out.display())
        }
        Err(e) => return Err(e).with_context(|| format!("could not create {}", out.display())),
    };
    writeln!(file, "{}", hex::encode(document.as_ref()))
        .with_context(|| format!("could not write {}", out.display()))?;

    Ok(hex::encode(pair.public_key().as_ref()))
}

pub fn load_pair(source: &KeySource) -> Result<Ed25519KeyPair> {
    let raw = match source {
        KeySource::Env(variable) => std::env::var(variable)
            .with_context(|| format!("{variable} is not set in the environment"))?,
        KeySource::File(path) => fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?,
        KeySource::Hex(hex) => hex.clone(),
    };

    let document = hex::decode(raw.trim()).map_err(|_| anyhow!(BAD_KEY))?;
    if document.is_empty() {
        bail!(BAD_KEY);
    }
    Ed25519KeyPair::from_pkcs8(&document).map_err(|_| anyhow!(BAD_KEY))
}

/// The signature covers the file bytes exactly as they sit on disk; parsing
/// and re-serialising would sign something the client never sees.
pub fn sign_file(pair: &Ed25519KeyPair, manifest: &Path, out: &Path) -> Result<()> {
    let bytes =
        fs::read(manifest).with_context(|| format!("could not read {}", manifest.display()))?;
    let signature = hex::encode(pair.sign(&bytes));

    fs::write(out, format!("{signature}\n"))
        .with_context(|| format!("could not write {}", out.display()))
}

pub fn load_public(source: &KeySource) -> Result<Vec<PublicKey>> {
    let text = match source {
        KeySource::File(path) => fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?,
        KeySource::Hex(hex) => hex.clone(),
        KeySource::Env(variable) => std::env::var(variable)
            .with_context(|| format!("{variable} is not set in the environment"))?,
    };

    let parsed = keys::parse(&text).map_err(|e| anyhow!("{e}"))?;
    if parsed.is_empty() {
        bail!("no public keys were given");
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use vorcall_core::update::manifest;

    const MANIFEST: &[u8] = br#"{"version":"0.3.0","notes":"","published_at":"2026-09-10T18:00:00Z","min_version":"0.2.0","platforms":{}}"#;

    fn signature_of(dir: &Path, manifest_path: &Path, key: &Path) -> String {
        let pair = load_pair(&KeySource::File(key.to_path_buf())).expect("should load the key");
        let out = dir.join("manifest.sig");
        sign_file(&pair, manifest_path, &out).expect("should sign");
        fs::read_to_string(&out).expect("should read the signature")
    }

    #[test]
    fn gen_key_sign_and_verify_round_trip() {
        let dir = testing::temp_dir("round-trip");
        let key = dir.join("release.key");
        let manifest_path = dir.join("manifest.json");
        fs::write(&manifest_path, MANIFEST).expect("should write the manifest");

        let public_hex = generate(&key).expect("should generate");
        assert_eq!(public_hex.len(), 64);

        let signature = signature_of(&dir, &manifest_path, &key);
        assert!(signature.ends_with('\n'));

        // A key file as the workflow ships it: a comment line, then the key.
        let key_file = format!("# vorcall release key\n{public_hex}\n");
        let trusted = keys::parse(&key_file).expect("should parse the public keys");
        let parsed = manifest::verify_and_parse(MANIFEST, &signature, &trusted)
            .expect("should verify the manifest");
        assert_eq!(parsed.version.to_string(), "0.3.0");

        let mut tampered = MANIFEST.to_vec();
        tampered[2] ^= 0x01;
        assert!(manifest::verify_and_parse(&tampered, &signature, &trusted).is_err());

        let other = dir.join("other.key");
        let other_public = generate(&other).expect("should generate");
        let other_trusted = keys::parse(&other_public).expect("should parse");
        assert!(manifest::verify_and_parse(MANIFEST, &signature, &other_trusted).is_err());

        testing::remove_dir(&dir);
    }

    #[test]
    fn gen_key_refuses_to_overwrite() {
        let dir = testing::temp_dir("gen-key-overwrite");
        let key = dir.join("release.key");
        generate(&key).expect("should generate");

        let before = fs::read_to_string(&key).expect("should read");
        let error = generate(&key).expect_err("should refuse");

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read_to_string(&key).expect("should read"), before);

        testing::remove_dir(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = testing::temp_dir("gen-key-mode");
        let key = dir.join("release.key");
        generate(&key).expect("should generate");

        let mode = fs::metadata(&key)
            .expect("should stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);

        testing::remove_dir(&dir);
    }

    #[test]
    fn a_bad_private_key_never_echoes_the_value() {
        for raw in ["", "not-hex", "abcd"] {
            let error = load_pair(&KeySource::Hex(raw.to_owned()))
                .expect_err("should reject")
                .to_string();

            assert_eq!(error, BAD_KEY);
            assert!(!error.contains(raw) || raw.is_empty(), "{error}");
        }
    }
}
