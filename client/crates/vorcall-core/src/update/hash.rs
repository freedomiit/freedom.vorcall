//! SHA-256 in the one shape the manifest speaks: lowercase hex.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use ring::digest::{Context, SHA256, digest};

const CHUNK: usize = 64 * 1024;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest(&SHA256, bytes))
}

/// Streams the file so a 25 MB release never sits in memory twice.
pub fn sha256_file_hex(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0u8; CHUNK];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }

    Ok(hex::encode(context.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testing;

    /// The FIPS 180-2 test vector for "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn matches_the_published_vector() {
        assert_eq!(sha256_hex(b"abc"), ABC);
    }

    #[test]
    fn the_file_form_matches_the_byte_form() {
        let dir = testing::temp_dir("hash");
        let path = dir.join("payload.bin");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).expect("should write");

        assert_eq!(
            sha256_file_hex(&path).expect("should hash"),
            sha256_hex(&bytes)
        );

        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").expect("should write");
        assert_eq!(
            sha256_file_hex(&empty).expect("should hash"),
            sha256_hex(b"")
        );

        testing::remove_dir(&dir);
    }

    #[test]
    fn hashes_a_file_of_abc() {
        let dir = testing::temp_dir("hash-abc");
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").expect("should write");

        assert_eq!(sha256_file_hex(&path).expect("should hash"), ABC);

        testing::remove_dir(&dir);
    }
}
