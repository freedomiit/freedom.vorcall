//! Opus at 48 kHz mono, one 20 ms frame per call.
//!
//! `opus-rs` is a young pure-Rust port of libopus. Its known rough edges are
//! panics on the SILK paths, which `RestrictedLowDelay` (CELT-only) never
//! reaches, but a panic on the audio thread would still take the whole process
//! down — so every codec call runs under a catch-and-recreate guard.

use std::panic::{AssertUnwindSafe, catch_unwind};

use opus_rs::{Application, OpusDecoder, OpusEncoder};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

pub const BITRATE_BPS: i32 = 48_000;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus init failed: {0}")]
    Create(String),
    #[error("opus encode failed: {0}")]
    Encode(String),
    #[error("opus decode failed: {0}")]
    Decode(String),
}

pub struct Encoder {
    inner: OpusEncoder,
}

impl Encoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            inner: build_encoder()?,
        })
    }

    pub fn encode(
        &mut self,
        pcm: &[f32; FRAME_SAMPLES],
        out: &mut [u8],
    ) -> Result<usize, CodecError> {
        let inner = &mut self.inner;
        let encoded = catch_unwind(AssertUnwindSafe(|| inner.encode(pcm, FRAME_SAMPLES, out)));
        match encoded {
            Ok(Ok(written)) => Ok(written),
            Ok(Err(reason)) => Err(CodecError::Encode(reason.to_string())),
            Err(_) => {
                self.inner = build_encoder()?;
                Err(CodecError::Encode("codec panic".to_string()))
            }
        }
    }
}

pub struct Decoder {
    inner: OpusDecoder,
    /// TOC byte of the last packet that decoded, replayed alone to drive
    /// concealment: libopus treats a one-byte packet as a lost frame.
    last_toc: Option<u8>,
    resets: u32,
}

impl Decoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            inner: build_decoder()?,
            last_toc: None,
            resets: 0,
        })
    }

    /// `None` conceals a lost frame. Before any packet has been decoded there is
    /// nothing to conceal from, so `out` is filled with silence.
    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        out: &mut [f32; FRAME_SAMPLES],
    ) -> Result<(), CodecError> {
        let toc_only;
        let input: &[u8] = match packet {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => match self.last_toc {
                Some(toc) => {
                    toc_only = [toc];
                    &toc_only
                }
                None => {
                    out.fill(0.0);
                    return Ok(());
                }
            },
        };

        let inner = &mut self.inner;
        let decoded = catch_unwind(AssertUnwindSafe(|| inner.decode(input, FRAME_SAMPLES, out)));
        match decoded {
            Ok(Ok(written)) => {
                if written < FRAME_SAMPLES {
                    out[written..].fill(0.0);
                }
                if let Some(&toc) = input.first()
                    && input.len() > 1
                {
                    self.last_toc = Some(toc);
                }
                Ok(())
            }
            Ok(Err(reason)) => Err(CodecError::Decode(reason.to_string())),
            Err(_) => {
                self.resets = self.resets.saturating_add(1);
                out.fill(0.0);
                self.inner = build_decoder()?;
                self.last_toc = None;
                Ok(())
            }
        }
    }

    pub fn resets(&self) -> u32 {
        self.resets
    }
}

fn build_encoder() -> Result<OpusEncoder, CodecError> {
    let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::RestrictedLowDelay)
        .map_err(|reason| CodecError::Create(reason.to_string()))?;
    encoder.bitrate_bps = BITRATE_BPS;
    encoder.use_cbr = true;
    // Every frame is independent, so in-band FEC would only cost bitrate: loss
    // is concealed by the decoder instead.
    encoder.use_inband_fec = false;
    Ok(encoder)
}

fn build_decoder() -> Result<OpusDecoder, CodecError> {
    OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(|reason| CodecError::Create(reason.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tone::{Tone, rms};

    #[test]
    fn round_trips_a_tone() {
        let mut encoder = Encoder::new().expect("encoder");
        let mut decoder = Decoder::new().expect("decoder");
        let mut tone = Tone::new(440.0, 0.5);

        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let mut decoded = [0.0f32; FRAME_SAMPLES];
        let mut packet = [0u8; 512];
        let mut input_rms = 0.0;
        let mut output_rms = 0.0;

        // The first frames carry the encoder's and decoder's warm-up, so the
        // level is only compared once both have settled.
        for _ in 0..6 {
            tone.fill(&mut pcm);
            let written = encoder.encode(&pcm, &mut packet).expect("encodes");
            assert!(written > 1, "encoder produced {written} bytes");
            decoder
                .decode(Some(&packet[..written]), &mut decoded)
                .expect("decodes");
            input_rms = rms(&pcm);
            output_rms = rms(&decoded);
        }

        assert!(
            (output_rms - input_rms).abs() <= 0.3 * input_rms,
            "input rms {input_rms}, output rms {output_rms}"
        );
    }

    #[test]
    fn conceals_a_lost_frame() {
        let mut encoder = Encoder::new().expect("encoder");
        let mut decoder = Decoder::new().expect("decoder");
        let mut tone = Tone::new(440.0, 0.5);

        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let mut decoded = [0.0f32; FRAME_SAMPLES];
        let mut packet = [0u8; 512];

        for _ in 0..3 {
            tone.fill(&mut pcm);
            let written = encoder.encode(&pcm, &mut packet).expect("encodes");
            decoder
                .decode(Some(&packet[..written]), &mut decoded)
                .expect("decodes");
        }

        decoder.decode(None, &mut decoded).expect("conceals");
        assert_eq!(decoder.resets(), 0);
        assert!(decoded.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn conceals_before_any_packet() {
        let mut decoder = Decoder::new().expect("decoder");
        let mut decoded = [1.0f32; FRAME_SAMPLES];
        decoder.decode(None, &mut decoded).expect("conceals");
        assert!(decoded.iter().all(|sample| *sample == 0.0));
        assert_eq!(decoder.resets(), 0);
    }

    #[test]
    fn garbage_never_panics() {
        let mut decoder = Decoder::new().expect("decoder");
        let mut decoded = [0.0f32; FRAME_SAMPLES];

        for seed in 0u32..64 {
            let garbage: Vec<u8> = (0..40u32)
                .map(|index| (seed.wrapping_mul(2_654_435_761).wrapping_add(index * 31)) as u8)
                .collect();
            // Either outcome is fine; what matters is that the call returns and
            // the decoder stays usable afterwards.
            let _ = decoder.decode(Some(&garbage), &mut decoded);
            assert!(decoded.iter().all(|sample| sample.is_finite()));
        }

        assert!(decoder.decode(None, &mut decoded).is_ok());
    }
}
