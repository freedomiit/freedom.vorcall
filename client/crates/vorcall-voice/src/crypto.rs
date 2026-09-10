//! AEAD for media datagrams.
//!
//! IETF ChaCha20-Poly1305 (12-byte nonce) under the room key delivered by
//! `VoiceReady`. The nonce is `ssrc || seq`, which is unique per sender for as
//! long as the key lives, and the cleartext header is authenticated as
//! additional data.

use std::fmt;

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};

use crate::packet::{HEADER_LEN, Header, MAX_DATAGRAM, MIN_DATAGRAM, PacketError, TAG_LEN};

pub struct MediaCipher {
    cipher: ChaCha20Poly1305,
}

impl MediaCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(&Key::from(*key)),
        }
    }

    /// Returns `header || ciphertext || tag`.
    pub fn seal(&self, header: &Header, plaintext: &[u8]) -> Vec<u8> {
        let aad = header.encode();
        let nonce = Nonce::from(header.nonce());
        let mut datagram = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
        datagram.extend_from_slice(&aad);
        // The AEAD only fails on a message longer than its 256 GiB limit, which
        // a 20 ms frame never is. Returning an empty vector instead of
        // panicking leaves the sender a length it can refuse to put on the wire.
        match self.cipher.encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        ) {
            Ok(sealed) => datagram.extend_from_slice(&sealed),
            Err(_) => datagram.clear(),
        }
        datagram
    }

    pub fn open(&self, datagram: &[u8]) -> Result<(Header, Vec<u8>), PacketError> {
        if datagram.len() < MIN_DATAGRAM {
            return Err(PacketError::TooShort);
        }
        if datagram.len() > MAX_DATAGRAM {
            return Err(PacketError::TooLong);
        }
        let header = Header::decode(datagram)?;
        let (aad, sealed) = datagram.split_at(HEADER_LEN);
        let plaintext = self
            .cipher
            .decrypt(&Nonce::from(header.nonce()), Payload { msg: sealed, aad })
            .map_err(|_| PacketError::BadTag)?;
        Ok((header, plaintext))
    }
}

impl fmt::Debug for MediaCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaCipher(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PacketType;

    const KEY: [u8; 32] = [7u8; 32];

    fn header() -> Header {
        Header {
            kind: PacketType::Audio,
            marker: true,
            ssrc: 0x0000_2A2A,
            seq: 9,
            ts: 48_000,
        }
    }

    #[test]
    fn seals_and_opens() {
        let cipher = MediaCipher::new(&KEY);
        let plaintext = b"twenty milliseconds of opus".to_vec();
        let datagram = cipher.seal(&header(), &plaintext);

        assert_eq!(datagram.len(), HEADER_LEN + plaintext.len() + TAG_LEN);
        assert_eq!(&datagram[..HEADER_LEN], &header().encode());
        assert_ne!(
            &datagram[HEADER_LEN..HEADER_LEN + plaintext.len()],
            &plaintext[..]
        );

        let (opened_header, opened) = cipher.open(&datagram).expect("opens");
        assert_eq!(opened_header, header());
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn a_flipped_body_byte_fails_the_tag() {
        let cipher = MediaCipher::new(&KEY);
        let datagram = cipher.seal(&header(), b"payload");

        // Bytes 0..2 carry version and type, which the header decoder rejects
        // before the AEAD runs; everything from the flags byte on is only
        // caught by the tag.
        for index in 2..datagram.len() {
            let mut damaged = datagram.clone();
            damaged[index] ^= 0x01;
            assert!(
                matches!(cipher.open(&damaged), Err(PacketError::BadTag)),
                "byte {index} was accepted"
            );
        }
        for index in 0..2 {
            let mut damaged = datagram.clone();
            damaged[index] ^= 0x01;
            assert!(cipher.open(&damaged).is_err(), "byte {index} was accepted");
        }
    }

    #[test]
    fn a_different_key_fails_the_tag() {
        let datagram = MediaCipher::new(&KEY).seal(&header(), b"payload");
        let other = MediaCipher::new(&[8u8; 32]);
        assert!(matches!(other.open(&datagram), Err(PacketError::BadTag)));
    }

    #[test]
    fn a_different_header_fails_the_tag() {
        let cipher = MediaCipher::new(&KEY);
        let mut datagram = cipher.seal(&header(), b"payload");
        let mut moved = header();
        moved.seq = 10;
        datagram[..HEADER_LEN].copy_from_slice(&moved.encode());
        assert!(matches!(cipher.open(&datagram), Err(PacketError::BadTag)));
    }

    #[test]
    fn enforces_datagram_lengths() {
        let cipher = MediaCipher::new(&KEY);
        let datagram = cipher.seal(&header(), b"payload");
        assert!(matches!(
            cipher.open(&datagram[..MIN_DATAGRAM - 1]),
            Err(PacketError::TooShort)
        ));

        let long = cipher.seal(&header(), &vec![0u8; MAX_DATAGRAM]);
        assert!(long.len() > MAX_DATAGRAM);
        assert!(matches!(cipher.open(&long), Err(PacketError::TooLong)));
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let rendered = format!("{:?}", MediaCipher::new(&KEY));
        assert_eq!(rendered, "MediaCipher(<redacted>)");
    }

    #[test]
    fn an_empty_payload_still_authenticates() {
        let cipher = MediaCipher::new(&KEY);
        let datagram = cipher.seal(&header(), b"");
        assert_eq!(datagram.len(), MIN_DATAGRAM);
        let (_, opened) = cipher.open(&datagram).expect("opens");
        assert!(opened.is_empty());
    }
}
