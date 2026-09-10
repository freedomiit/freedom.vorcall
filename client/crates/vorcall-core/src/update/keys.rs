//! The Ed25519 public keys this build trusts to sign a release manifest, baked
//! in from `client/update-keys.pub` at compile time.

use super::UpdateError;

/// The key list as shipped. Rotation is a rebuild, never a server-side change.
pub const BAKED: &str = include_str!("../../../../update-keys.pub");

/// One trusted signing key: 32 raw Ed25519 public key bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey(pub [u8; 32]);

/// One 64-hex key per line; blank lines and `#` comments are ignored. An empty
/// result is not an error here — it disables the updater, which the caller
/// reports through [`super::disabled_reason`].
pub fn parse(text: &str) -> Result<Vec<PublicKey>, UpdateError> {
    let mut keys = Vec::new();

    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let number = index + 1;
        let bytes = hex::decode(line)
            .map_err(|_| UpdateError::Keys(format!("line {number} is not hexadecimal")))?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| {
            UpdateError::Keys(format!("line {number} is not 64 hexadecimal characters"))
        })?;
        keys.push(PublicKey(key));
    }

    Ok(keys)
}

pub fn baked() -> Result<Vec<PublicKey>, UpdateError> {
    parse(BAKED)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_B: &str = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = format!("# a comment\n\n   \n{KEY_A}\n  # indented comment\n");
        let keys = parse(&text).expect("should parse");

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0[31], 1);
        assert_eq!(keys[0].0[..31], [0u8; 31]);
    }

    #[test]
    fn accepts_uppercase_hex() {
        let upper = parse(KEY_B).expect("should parse");
        let lower = parse(&KEY_B.to_lowercase()).expect("should parse");

        assert_eq!(upper, lower);
        assert_eq!(upper[0].0[0], 0xab);
    }

    #[test]
    fn rejects_a_short_key() {
        let short = &KEY_A[1..];
        assert_eq!(short.len(), 63);
        assert!(matches!(parse(short), Err(UpdateError::Keys(_))));
    }

    #[test]
    fn rejects_a_non_hex_key() {
        let bad = "z".repeat(64);
        assert!(matches!(parse(&bad), Err(UpdateError::Keys(_))));
    }

    #[test]
    fn the_baked_list_parses() {
        assert!(baked().is_ok());
    }
}
