//! Opus at 48 kHz, one 20 ms frame per call: mono for voice, interleaved
//! stereo for a screen share's own audio.
//!
//! `opus-rs` is a young pure-Rust port of libopus. Its known rough edges are
//! panics on the SILK paths, which `RestrictedLowDelay` (CELT-only) never
//! reaches, but a panic on the audio thread would still take the whole process
//! down — so every codec call runs under a catch-and-recreate guard.

use std::panic::{AssertUnwindSafe, catch_unwind};

use opus_rs::{Application, OpusDecoder, OpusEncoder};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

pub const BITRATE_BPS: i32 = 48_000;
/// The share stream carries program material — music, game sound, a video
/// being watched — rather than one talker, so it gets a wider budget.
pub const SHARE_BITRATE_BPS: i32 = 96_000;
/// 20 ms at 48 kHz, interleaved stereo: `L R L R ...`.
pub const STEREO_FRAME_SAMPLES: usize = 2 * FRAME_SAMPLES;

const MONO: usize = 1;
const STEREO: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus init failed: {0}")]
    Create(String),
    #[error("opus encode failed: {0}")]
    Encode(String),
    #[error("opus decode failed: {0}")]
    Decode(String),
}

/// The guard both encoders share: a panic inside the port rebuilds the codec
/// rather than taking the audio thread's process down with it.
struct EncoderCore {
    inner: OpusEncoder,
    channels: usize,
    bitrate_bps: i32,
}

impl EncoderCore {
    fn new(channels: usize, bitrate_bps: i32) -> Result<Self, CodecError> {
        Ok(Self {
            inner: build_encoder(channels, bitrate_bps)?,
            channels,
            bitrate_bps,
        })
    }

    /// `pcm` is `FRAME_SAMPLES` frames, interleaved over `channels`.
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError> {
        let inner = &mut self.inner;
        let encoded = catch_unwind(AssertUnwindSafe(|| inner.encode(pcm, FRAME_SAMPLES, out)));
        match encoded {
            Ok(Ok(written)) => Ok(written),
            Ok(Err(reason)) => Err(CodecError::Encode(reason.to_string())),
            Err(_) => {
                self.inner = build_encoder(self.channels, self.bitrate_bps)?;
                Err(CodecError::Encode("codec panic".to_string()))
            }
        }
    }
}

struct DecoderCore {
    inner: OpusDecoder,
    channels: usize,
    /// TOC byte of the last packet that decoded, replayed alone to drive
    /// concealment: libopus treats a one-byte packet as a lost frame.
    last_toc: Option<u8>,
    resets: u32,
}

impl DecoderCore {
    fn new(channels: usize) -> Result<Self, CodecError> {
        Ok(Self {
            inner: build_decoder(channels)?,
            channels,
            last_toc: None,
            resets: 0,
        })
    }

    /// `out` is `FRAME_SAMPLES` frames, interleaved over `channels`.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<(), CodecError> {
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
                // The port counts frames, not samples.
                let samples = written * self.channels;
                if samples < out.len() {
                    out[samples..].fill(0.0);
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
                self.inner = build_decoder(self.channels)?;
                self.last_toc = None;
                Ok(())
            }
        }
    }
}

pub struct Encoder {
    core: EncoderCore,
}

impl Encoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            core: EncoderCore::new(MONO, BITRATE_BPS)?,
        })
    }

    pub fn encode(
        &mut self,
        pcm: &[f32; FRAME_SAMPLES],
        out: &mut [u8],
    ) -> Result<usize, CodecError> {
        self.core.encode(pcm, out)
    }
}

pub struct Decoder {
    core: DecoderCore,
}

impl Decoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            core: DecoderCore::new(MONO)?,
        })
    }

    /// `None` conceals a lost frame. Before any packet has been decoded there is
    /// nothing to conceal from, so `out` is filled with silence.
    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        out: &mut [f32; FRAME_SAMPLES],
    ) -> Result<(), CodecError> {
        self.core.decode(packet, out)
    }

    pub fn resets(&self) -> u32 {
        self.core.resets
    }
}

/// The share stream's encoder: same 20 ms CELT-only frames as the voice one,
/// two channels and a wider bitrate.
pub struct StereoEncoder {
    core: EncoderCore,
}

impl StereoEncoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            core: EncoderCore::new(STEREO, SHARE_BITRATE_BPS)?,
        })
    }

    pub fn encode(
        &mut self,
        pcm: &[f32; STEREO_FRAME_SAMPLES],
        out: &mut [u8],
    ) -> Result<usize, CodecError> {
        self.core.encode(pcm, out)
    }
}

pub struct StereoDecoder {
    core: DecoderCore,
}

impl StereoDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            core: DecoderCore::new(STEREO)?,
        })
    }

    /// `None` conceals a lost frame, exactly as [`Decoder::decode`] does.
    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        out: &mut [f32; STEREO_FRAME_SAMPLES],
    ) -> Result<(), CodecError> {
        self.core.decode(packet, out)
    }

    pub fn resets(&self) -> u32 {
        self.core.resets
    }
}

fn build_encoder(channels: usize, bitrate_bps: i32) -> Result<OpusEncoder, CodecError> {
    let mut encoder = OpusEncoder::new(
        SAMPLE_RATE as i32,
        channels,
        Application::RestrictedLowDelay,
    )
    .map_err(|reason| CodecError::Create(reason.to_string()))?;
    encoder.bitrate_bps = bitrate_bps;
    encoder.use_cbr = true;
    // Every frame is independent, so in-band FEC would only cost bitrate: loss
    // is concealed by the decoder instead.
    encoder.use_inband_fec = false;
    Ok(encoder)
}

fn build_decoder(channels: usize) -> Result<OpusDecoder, CodecError> {
    OpusDecoder::new(SAMPLE_RATE as i32, channels)
        .map_err(|reason| CodecError::Create(reason.to_string()))
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

    /// Amplitude of `freq` in `samples`, by correlation with that exact tone —
    /// 440 Hz is not a bin centre over 960 samples, so a plain DFT bin would
    /// smear the two channels' tones into each other.
    fn tone_amplitude(samples: &[f32], freq: f32) -> f32 {
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for (index, sample) in samples.iter().enumerate() {
            let phase =
                std::f64::consts::TAU * f64::from(freq) * index as f64 / f64::from(SAMPLE_RATE);
            re += f64::from(*sample) * phase.cos();
            im -= f64::from(*sample) * phase.sin();
        }
        (2.0 * re.hypot(im) / samples.len() as f64) as f32
    }

    fn channel(frame: &[f32; STEREO_FRAME_SAMPLES], index: usize) -> Vec<f32> {
        frame.iter().skip(index).step_by(2).copied().collect()
    }

    /// L 440 Hz, R 660 Hz, both at amplitude 0.5.
    fn stereo_tones() -> impl FnMut(&mut [f32; STEREO_FRAME_SAMPLES]) {
        let mut left = Tone::new(440.0, 0.5);
        let mut right = Tone::new(660.0, 0.5);
        move |frame: &mut [f32; STEREO_FRAME_SAMPLES]| {
            let mut left_pcm = [0.0f32; FRAME_SAMPLES];
            let mut right_pcm = [0.0f32; FRAME_SAMPLES];
            left.fill(&mut left_pcm);
            right.fill(&mut right_pcm);
            for (index, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                pair[0] = left_pcm[index];
                pair[1] = right_pcm[index];
            }
        }
    }

    #[test]
    fn round_trips_a_stereo_pair_without_mixing_the_channels() {
        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut decoder = StereoDecoder::new().expect("decoder");
        let mut fill = stereo_tones();

        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut decoded = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; 1156];
        let mut input = (0.0, 0.0);
        let mut output = (0.0, 0.0);

        // The first frames carry the codec's warm-up, as in the mono case.
        for _ in 0..6 {
            fill(&mut pcm);
            let written = encoder.encode(&pcm, &mut packet).expect("encodes");
            assert!(written > 1, "encoder produced {written} bytes");
            decoder
                .decode(Some(&packet[..written]), &mut decoded)
                .expect("decodes");
            input = (rms(&channel(&pcm, 0)), rms(&channel(&pcm, 1)));
            output = (rms(&channel(&decoded, 0)), rms(&channel(&decoded, 1)));
        }

        assert!(
            (output.0 - input.0).abs() <= 0.3 * input.0,
            "left input rms {}, output rms {}",
            input.0,
            output.0
        );
        assert!(
            (output.1 - input.1).abs() <= 0.3 * input.1,
            "right input rms {}, output rms {}",
            input.1,
            output.1
        );

        // Measured separation of the 440 Hz tone: 22.8 dB.
        let left_440 = tone_amplitude(&channel(&decoded, 0), 440.0);
        let right_440 = tone_amplitude(&channel(&decoded, 1), 440.0);
        let separation_db = 20.0 * (left_440 / right_440).log10();
        assert!(separation_db >= 10.0, "{separation_db} dB");
    }

    #[test]
    fn the_stereo_decoder_conceals_a_lost_frame() {
        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut decoder = StereoDecoder::new().expect("decoder");
        let mut fill = stereo_tones();

        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut decoded = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; 1156];

        for _ in 0..3 {
            fill(&mut pcm);
            let written = encoder.encode(&pcm, &mut packet).expect("encodes");
            decoder
                .decode(Some(&packet[..written]), &mut decoded)
                .expect("decodes");
        }

        decoder.decode(None, &mut decoded).expect("conceals");
        assert_eq!(decoder.resets(), 0);
        assert!(decoded.iter().all(|sample| sample.is_finite()));

        let mut fresh = StereoDecoder::new().expect("decoder");
        let mut silent = [1.0f32; STEREO_FRAME_SAMPLES];
        fresh.decode(None, &mut silent).expect("conceals");
        assert!(silent.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_mono_decoder_refuses_a_stereo_packet() {
        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut decoder = Decoder::new().expect("decoder");
        let mut fill = stereo_tones();

        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; 1156];
        fill(&mut pcm);
        let written = encoder.encode(&pcm, &mut packet).expect("encodes");

        let mut decoded = [0.0f32; FRAME_SAMPLES];
        // A refusal, not a panic: the channel mismatch is caught by the port.
        assert!(matches!(
            decoder.decode(Some(&packet[..written]), &mut decoded),
            Err(CodecError::Decode(_))
        ));
        assert_eq!(decoder.resets(), 0);
    }

    #[test]
    fn stereo_garbage_never_panics() {
        let mut decoder = StereoDecoder::new().expect("decoder");
        let mut decoded = [0.0f32; STEREO_FRAME_SAMPLES];

        for seed in 0u32..64 {
            let garbage: Vec<u8> = (0..40u32)
                .map(|index| (seed.wrapping_mul(2_654_435_761).wrapping_add(index * 31)) as u8)
                .collect();
            let _ = decoder.decode(Some(&garbage), &mut decoded);
            assert!(decoded.iter().all(|sample| sample.is_finite()));
        }

        // Measured: the port rejects every one of them without unwinding.
        assert_eq!(decoder.resets(), 0);

        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut fill = stereo_tones();
        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; 1156];
        fill(&mut pcm);
        let written = encoder.encode(&pcm, &mut packet).expect("encodes");
        decoder
            .decode(Some(&packet[..written]), &mut decoded)
            .expect("still decodes");
        assert!(rms(&decoded) > 0.0);
    }
}
