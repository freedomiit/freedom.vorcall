//! The cleartext datagram header.
//!
//! Every media datagram is `header || ciphertext || tag`. The header travels in
//! the clear because the relay routes on it, and is authenticated as additional
//! data so it cannot be rewritten without the room key.
//!
//! Layout, big-endian:
//!
//! ```text
//! [0]      version = 1
//! [1]      type (1 audio, 2 ping, 3 pong, 4 video, 5 share audio,
//!                6 keyframe request)
//! [2]      flags (bit 0 = marker)
//! [3..7]   ssrc  u32
//! [7..15]  seq   u64
//! [15..19] ts    u32  (48 kHz sample clock, wrapping)
//! ```

pub const HEADER_LEN: usize = 19;
pub const TAG_LEN: usize = 16;
/// One datagram fits a 1500-byte path MTU with room to spare for the IPv4 and
/// UDP headers and for a tunnel's own encapsulation.
pub const MAX_DATAGRAM: usize = 1200;
pub const MIN_DATAGRAM: usize = HEADER_LEN + TAG_LEN;
pub const VERSION: u8 = 1;
pub const MARKER_FLAG: u8 = 0b0000_0001;
/// Set by the relay on pongs so their nonce never equals the ping's under the same key.
pub const PONG_SEQ_BIT: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Audio = 1,
    Ping = 2,
    Pong = 3,
    /// One fragment of a screen-share access unit; see [`crate::video`].
    Video = 4,
    /// One 20 ms stereo Opus frame of the screen share's own audio.
    ShareAudio = 5,
    /// A viewer asking the sharer for a keyframe; payload is the target ssrc.
    KeyframeRequest = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: PacketType,
    pub marker: bool,
    pub ssrc: u32,
    pub seq: u64,
    pub ts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("datagram shorter than the minimum")]
    TooShort,
    #[error("datagram longer than the maximum")]
    TooLong,
    #[error("unsupported protocol version {0}")]
    BadVersion(u8),
    #[error("unknown packet type {0}")]
    BadType(u8),
    #[error("reserved flag bits set: {0:#010b}")]
    BadFlags(u8),
    #[error("video fragment {index} of {count} is out of range")]
    BadFragment { index: u16, count: u16 },
    #[error("authentication tag mismatch")]
    BadTag,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = VERSION;
        out[1] = self.kind as u8;
        out[2] = if self.marker { MARKER_FLAG } else { 0 };
        out[3..7].copy_from_slice(&self.ssrc.to_be_bytes());
        out[7..15].copy_from_slice(&self.seq.to_be_bytes());
        out[15..19].copy_from_slice(&self.ts.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Header, PacketError> {
        if bytes.len() < HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        if bytes[0] != VERSION {
            return Err(PacketError::BadVersion(bytes[0]));
        }
        let kind = match bytes[1] {
            1 => PacketType::Audio,
            2 => PacketType::Ping,
            3 => PacketType::Pong,
            4 => PacketType::Video,
            5 => PacketType::ShareAudio,
            6 => PacketType::KeyframeRequest,
            other => return Err(PacketError::BadType(other)),
        };
        let flags = bytes[2];
        if flags & !MARKER_FLAG != 0 {
            return Err(PacketError::BadFlags(flags));
        }
        Ok(Header {
            kind,
            marker: flags & MARKER_FLAG != 0,
            ssrc: u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]),
            seq: u64::from_be_bytes([
                bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
            ]),
            ts: u32::from_be_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]),
        })
    }

    /// The AEAD nonce: `ssrc || seq`, which is exactly header bytes 3..15.
    pub fn nonce(&self) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.ssrc.to_be_bytes());
        nonce[4..].copy_from_slice(&self.seq.to_be_bytes());
        nonce
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kind: PacketType, marker: bool) -> Header {
        Header {
            kind,
            marker,
            ssrc: 0xDEAD_BEEF,
            seq: 0x0102_0304_0506_0708,
            ts: 0x1122_3344,
        }
    }

    #[test]
    fn round_trips_every_type_and_marker() {
        for kind in [
            PacketType::Audio,
            PacketType::Ping,
            PacketType::Pong,
            PacketType::Video,
            PacketType::ShareAudio,
            PacketType::KeyframeRequest,
        ] {
            for marker in [false, true] {
                let header = sample(kind, marker);
                let decoded = Header::decode(&header.encode()).expect("decodes");
                assert_eq!(decoded, header);
            }
        }
    }

    #[test]
    fn encodes_the_documented_layout() {
        let bytes = sample(PacketType::Ping, true).encode();
        assert_eq!(bytes[0], VERSION);
        assert_eq!(bytes[1], 2);
        assert_eq!(bytes[2], MARKER_FLAG);
        assert_eq!(&bytes[3..7], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&bytes[7..15], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&bytes[15..19], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn rejects_short_input() {
        let bytes = sample(PacketType::Audio, false).encode();
        assert!(matches!(
            Header::decode(&bytes[..HEADER_LEN - 1]),
            Err(PacketError::TooShort)
        ));
        assert!(matches!(Header::decode(&[]), Err(PacketError::TooShort)));
    }

    #[test]
    fn rejects_bad_version_type_and_flags() {
        let mut bytes = sample(PacketType::Audio, false).encode();
        bytes[0] = 2;
        assert!(matches!(
            Header::decode(&bytes),
            Err(PacketError::BadVersion(2))
        ));

        let mut bytes = sample(PacketType::Audio, false).encode();
        bytes[1] = 7;
        assert!(matches!(
            Header::decode(&bytes),
            Err(PacketError::BadType(7))
        ));
        bytes[1] = 0;
        assert!(matches!(
            Header::decode(&bytes),
            Err(PacketError::BadType(0))
        ));

        let mut bytes = sample(PacketType::Audio, false).encode();
        bytes[2] = 0b0000_0010;
        assert!(matches!(
            Header::decode(&bytes),
            Err(PacketError::BadFlags(0b0000_0010))
        ));
    }

    #[test]
    fn nonce_is_the_header_slice() {
        let header = sample(PacketType::Audio, true);
        assert_eq!(header.nonce(), header.encode()[3..15]);
    }

    #[test]
    fn pong_bit_round_trips() {
        let mut header = sample(PacketType::Pong, false);
        header.seq = 42 | PONG_SEQ_BIT;
        let decoded = Header::decode(&header.encode()).expect("decodes");
        assert_eq!(decoded.seq, 42 | PONG_SEQ_BIT);
        assert_eq!(decoded.seq & !PONG_SEQ_BIT, 42);
        assert_ne!(header.nonce(), sample(PacketType::Pong, false).nonce());
    }

    #[test]
    fn minimum_datagram_is_header_plus_tag() {
        assert_eq!(MIN_DATAGRAM, 35);
    }

    #[test]
    fn every_wire_type_decodes_from_its_number() {
        for (number, kind) in [
            (1u8, PacketType::Audio),
            (2, PacketType::Ping),
            (3, PacketType::Pong),
            (4, PacketType::Video),
            (5, PacketType::ShareAudio),
            (6, PacketType::KeyframeRequest),
        ] {
            let mut bytes = sample(PacketType::Audio, false).encode();
            bytes[1] = number;
            assert_eq!(Header::decode(&bytes).expect("decodes").kind, kind);
            assert_eq!(kind as u8, number);
        }
    }
}
