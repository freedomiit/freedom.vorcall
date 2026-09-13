//! The container a soundpad clip travels in: `VORCSND1`.
//!
//! A clip is nothing but the 20 ms stereo Opus frames the share stream already
//! speaks, laid end to end behind an 18-byte header, so playing one needs no
//! decoder beyond the one this crate already carries and no format negotiation
//! at all.
//!
//! ```text
//! offset 0   magic       8 bytes   "VORCSND1"
//! offset 8   sample_rate u32 LE    48000
//! offset 12  channels    u8        2
//! offset 13  reserved    u8        0
//! offset 14  frames      u32 LE    count of 20 ms Opus packets
//! offset 18  frames x [ length u16 LE ][ opus packet ]
//! ```
//!
//! Nothing in a clip ever travels over UDP: it is a file, played locally.

use crate::SAMPLE_RATE;
use crate::codec::{STEREO_FRAME_SAMPLES, StereoDecoder, StereoEncoder};

/// The media type the container travels under.
pub const MEDIA_TYPE: &str = "application/vnd.vorcall.sound";
pub const MAGIC: &[u8; 8] = b"VORCSND1";
/// 30 000 frames of 20 ms: ten minutes.
pub const MAX_FRAMES: u32 = 30_000;
/// Opus's own maximum packet size.
pub const MAX_PACKET: usize = 1275;
pub const HEADER_LEN: usize = 18;

const CHANNELS: u8 = 2;
const FRAME_MS: u32 = crate::FRAME_MS as u32;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SoundError {
    #[error("a sound is at least {HEADER_LEN} bytes")]
    TooShort,
    #[error("not a vorcall sound")]
    BadMagic,
    #[error("sample rate {found} is not {SAMPLE_RATE}")]
    SampleRate { found: u32 },
    #[error("{found} channels, expected {CHANNELS}")]
    Channels { found: u8 },
    #[error("reserved byte is {found}, expected 0")]
    Reserved { found: u8 },
    #[error("a sound carries at least one frame")]
    NoFrames,
    #[error("{found} frames is over the {MAX_FRAMES} limit")]
    TooManyFrames { found: u32 },
    #[error("frame {index} is empty")]
    EmptyFrame { index: u32 },
    #[error("frame {index} is {len} bytes, over the {MAX_PACKET} limit")]
    FrameTooLarge { index: u32, len: usize },
    #[error("frame {index} runs past the end")]
    Truncated { index: u32 },
    #[error("{extra} bytes after the last frame")]
    TrailingBytes { extra: usize },
    #[error("opus encode failed: {0}")]
    Encode(String),
    #[error("opus decode failed: {0}")]
    Decode(String),
}

/// One clip: 20 ms stereo Opus frames at 48 kHz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundClip {
    frames: Vec<Vec<u8>>,
}

impl SoundClip {
    /// Encodes interleaved stereo 48 kHz samples. A trailing partial frame is
    /// zero-padded to a whole one.
    pub fn encode(pcm: &[f32]) -> Result<Self, SoundError> {
        let count = pcm.len().div_ceil(STEREO_FRAME_SAMPLES);
        if count == 0 {
            return Err(SoundError::NoFrames);
        }
        if count > MAX_FRAMES as usize {
            return Err(SoundError::TooManyFrames {
                found: u32::try_from(count).unwrap_or(u32::MAX),
            });
        }

        let mut encoder = StereoEncoder::new().map_err(|e| SoundError::Encode(e.to_string()))?;
        let mut frames = Vec::with_capacity(count);
        let mut block = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; MAX_PACKET];
        for (index, chunk) in pcm.chunks(STEREO_FRAME_SAMPLES).enumerate() {
            block.fill(0.0);
            block[..chunk.len()].copy_from_slice(chunk);
            let written = encoder
                .encode(&block, &mut packet)
                .map_err(|e| SoundError::Encode(e.to_string()))?;
            let index = index as u32;
            if written == 0 {
                return Err(SoundError::EmptyFrame { index });
            }
            if written > MAX_PACKET {
                return Err(SoundError::FrameTooLarge {
                    index,
                    len: written,
                });
            }
            frames.push(packet[..written].to_vec());
        }
        Ok(Self { frames })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SoundError> {
        let mut frames = Vec::new();
        walk(bytes, |frame| frames.push(frame.to_vec()))?;
        Ok(Self { frames })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let body: usize = self.frames.iter().map(|frame| 2 + frame.len()).sum();
        let mut bytes = Vec::with_capacity(HEADER_LEN + body);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.push(CHANNELS);
        bytes.push(0);
        bytes.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        for frame in &self.frames {
            bytes.extend_from_slice(&(frame.len() as u16).to_le_bytes());
            bytes.extend_from_slice(frame);
        }
        bytes
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    pub fn duration_ms(&self) -> u32 {
        self.frames.len() as u32 * FRAME_MS
    }

    /// Interleaved stereo 48 kHz. A frame the decoder refuses becomes silence
    /// rather than failing the whole clip.
    pub fn decode_to_pcm(&self) -> Result<Vec<f32>, SoundError> {
        let mut decoder = StereoDecoder::new().map_err(|e| SoundError::Decode(e.to_string()))?;
        let mut pcm = Vec::with_capacity(self.frames.len() * STEREO_FRAME_SAMPLES);
        let mut block = [0.0f32; STEREO_FRAME_SAMPLES];
        for frame in &self.frames {
            if let Err(error) = decoder.decode(Some(frame), &mut block) {
                tracing::debug!(%error, "a clip frame did not decode, playing silence");
                block.fill(0.0);
            }
            pcm.extend_from_slice(&block);
        }
        Ok(pcm)
    }
}

/// Validates a container's shape without decoding a sample — what a server
/// needs, since it must never interpret the audio. It is also the same walk
/// [`SoundClip::parse`] makes, so the two can never disagree on what a valid
/// clip is.
pub fn validate(bytes: &[u8]) -> Result<u32, SoundError> {
    walk(bytes, |_| {})
}

/// The one reader both [`validate`] and [`SoundClip::parse`] go through;
/// `on_frame` sees each packet in order. Returns the clip's duration in ms.
fn walk(bytes: &[u8], mut on_frame: impl FnMut(&[u8])) -> Result<u32, SoundError> {
    if bytes.len() < HEADER_LEN {
        return Err(SoundError::TooShort);
    }
    if &bytes[..8] != MAGIC {
        return Err(SoundError::BadMagic);
    }
    let sample_rate = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if sample_rate != SAMPLE_RATE {
        return Err(SoundError::SampleRate { found: sample_rate });
    }
    if bytes[12] != CHANNELS {
        return Err(SoundError::Channels { found: bytes[12] });
    }
    if bytes[13] != 0 {
        return Err(SoundError::Reserved { found: bytes[13] });
    }
    let count = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    if count == 0 {
        return Err(SoundError::NoFrames);
    }
    if count > MAX_FRAMES {
        return Err(SoundError::TooManyFrames { found: count });
    }

    let mut at = HEADER_LEN;
    for index in 0..count {
        if at + 2 > bytes.len() {
            return Err(SoundError::Truncated { index });
        }
        let len = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
        at += 2;
        if len == 0 {
            return Err(SoundError::EmptyFrame { index });
        }
        if len > MAX_PACKET {
            return Err(SoundError::FrameTooLarge { index, len });
        }
        if at + len > bytes.len() {
            return Err(SoundError::Truncated { index });
        }
        on_frame(&bytes[at..at + len]);
        at += len;
    }
    if at != bytes.len() {
        return Err(SoundError::TrailingBytes {
            extra: bytes.len() - at,
        });
    }
    Ok(count * FRAME_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tone::rms;

    /// `frames` worth of a 440 Hz tone, left channel only.
    fn pcm(samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|index| {
                let pair = index / 2;
                if index % 2 == 0 {
                    (std::f32::consts::TAU * 440.0 * pair as f32 / SAMPLE_RATE as f32).sin() * 0.5
                } else {
                    0.0
                }
            })
            .collect()
    }

    fn good_bytes(frames: usize) -> Vec<u8> {
        SoundClip::encode(&pcm(frames * STEREO_FRAME_SAMPLES))
            .expect("encodes")
            .to_bytes()
    }

    /// Both readers must refuse for the same reason.
    fn refuses(bytes: &[u8], expected: SoundError) {
        assert_eq!(SoundClip::parse(bytes), Err(expected.clone()));
        assert_eq!(validate(bytes), Err(expected));
    }

    #[test]
    fn round_trips_a_clip() {
        let clip = SoundClip::encode(&pcm(5 * STEREO_FRAME_SAMPLES)).expect("encodes");
        assert_eq!(clip.frames(), 5);
        assert_eq!(clip.duration_ms(), 100);

        let bytes = clip.to_bytes();
        assert_eq!(validate(&bytes), Ok(100));
        let parsed = SoundClip::parse(&bytes).expect("parses");
        assert_eq!(parsed, clip);

        let decoded = parsed.decode_to_pcm().expect("decodes");
        assert_eq!(decoded.len(), 5 * STEREO_FRAME_SAMPLES);
        assert!(rms(&decoded) > 0.0, "the clip decoded to silence");
    }

    #[test]
    fn a_trailing_partial_frame_is_padded() {
        let clip = SoundClip::encode(&pcm(STEREO_FRAME_SAMPLES + 100)).expect("encodes");
        assert_eq!(clip.frames(), 2);
        assert_eq!(clip.duration_ms(), 40);
        assert_eq!(
            clip.decode_to_pcm().expect("decodes").len(),
            2 * STEREO_FRAME_SAMPLES
        );
    }

    #[test]
    fn an_empty_input_carries_no_frames() {
        assert_eq!(SoundClip::encode(&[]), Err(SoundError::NoFrames));
    }

    #[test]
    fn a_short_buffer_is_refused() {
        refuses(&[], SoundError::TooShort);
        refuses(&good_bytes(1)[..HEADER_LEN - 1], SoundError::TooShort);
    }

    #[test]
    fn a_bad_header_is_refused() {
        let mut bytes = good_bytes(1);
        bytes[0] = b'X';
        refuses(&bytes, SoundError::BadMagic);

        let mut bytes = good_bytes(1);
        bytes[8..12].copy_from_slice(&44_100u32.to_le_bytes());
        refuses(&bytes, SoundError::SampleRate { found: 44_100 });

        let mut bytes = good_bytes(1);
        bytes[12] = 1;
        refuses(&bytes, SoundError::Channels { found: 1 });

        let mut bytes = good_bytes(1);
        bytes[13] = 7;
        refuses(&bytes, SoundError::Reserved { found: 7 });
    }

    #[test]
    fn a_bad_frame_count_is_refused() {
        let mut bytes = good_bytes(1);
        bytes[14..18].copy_from_slice(&0u32.to_le_bytes());
        refuses(&bytes, SoundError::NoFrames);

        let mut bytes = good_bytes(1);
        let over = MAX_FRAMES + 1;
        bytes[14..18].copy_from_slice(&over.to_le_bytes());
        refuses(&bytes, SoundError::TooManyFrames { found: over });
    }

    #[test]
    fn a_bad_frame_length_is_refused() {
        let mut bytes = good_bytes(2);
        bytes[HEADER_LEN..HEADER_LEN + 2].copy_from_slice(&0u16.to_le_bytes());
        refuses(&bytes, SoundError::EmptyFrame { index: 0 });

        let mut bytes = good_bytes(2);
        let over = MAX_PACKET + 1;
        bytes[HEADER_LEN..HEADER_LEN + 2].copy_from_slice(&(over as u16).to_le_bytes());
        refuses(
            &bytes,
            SoundError::FrameTooLarge {
                index: 0,
                len: over,
            },
        );
    }

    #[test]
    fn a_frame_past_the_end_is_refused() {
        let bytes = good_bytes(2);
        // The last frame's bytes cut in half.
        let short = &bytes[..bytes.len() - 4];
        refuses(short, SoundError::Truncated { index: 1 });

        // A length header cut in half is the same refusal.
        let first = 2 + u16::from_le_bytes([bytes[HEADER_LEN], bytes[HEADER_LEN + 1]]) as usize;
        refuses(
            &bytes[..HEADER_LEN + first + 1],
            SoundError::Truncated { index: 1 },
        );
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = good_bytes(2);
        bytes.extend_from_slice(&[0, 0, 0]);
        refuses(&bytes, SoundError::TrailingBytes { extra: 3 });
    }
}
