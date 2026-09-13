//! Importing a clip on this machine: the decode that turns a picked audio file
//! into the interleaved stereo 48 kHz a soundpad clip is made of, the waveform
//! the trim view draws over it, and the cut that turns a selection into the
//! bytes a slot holds.
//!
//! Nothing here reaches the network: a picked file becomes a clip entirely on
//! this machine, and the upload is the app layer's business.
//!
//! Decoding never runs on the UI thread: every entry point here is called from a
//! blocking task.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use vorcall_voice::{SAMPLE_RATE, SoundClip};

/// The longest source this will decode. A clip is re-encoded to 20 ms Opus
/// frames and `SoundClip` caps those at ten minutes, so a longer source could
/// never be stored whole — and decoding one costs a byte of memory per sample.
pub const MAX_SOURCE_MS: u32 = 10 * 60 * 1000;

/// How many buckets the trim view draws. Fixed so the geometry is pure.
pub const WAVEFORM_BUCKETS: usize = 512;

/// Everything a clip is made of is interleaved stereo.
const CHANNELS: usize = 2;

/// Long enough to kill the click at a cut, short enough not to swallow the
/// sound the user framed. The same 5 ms the interface motifs fade over.
const FADE_MS: usize = 5;

/// A decoded source, ready to be trimmed.
pub struct Source {
    /// Interleaved stereo, 48 kHz.
    pub pcm: Arc<Vec<f32>>,
    pub duration_ms: u32,
    /// `WAVEFORM_BUCKETS` pairs of (min, max), for drawing.
    pub peaks: Arc<Vec<(f32, f32)>>,
}

/// Decodes a picked file: demux, decode, downmix or upmix to stereo, resample to
/// 48 kHz, and measure the waveform.
///
/// The length is checked twice: once against whatever the container claims,
/// before a sample is decoded, and again as the samples accumulate, because
/// plenty of containers claim nothing at all.
pub fn decode(path: &Path) -> Result<Source, String> {
    let file = File::open(path).map_err(|error| format!("cannot open that file: {error}"))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }

    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("that file is not audio Vorcall can read: {error}"))?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| "that file has no audio in it".to_string())?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
        return Err("that file has no audio in it".to_string());
    };
    if let (Some(base), Some(duration)) = (track.time_base, track.duration)
        && let Some(time) = base.calc_duration(duration)
    {
        within_limit(time.as_millis())?;
    }

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|error| format!("Vorcall cannot decode that kind of audio: {error}"))?;

    // Per decoded buffer, at the source's own rate and channel count; `pcm` is
    // what has already been converted to interleaved stereo 48 kHz.
    let mut decoded: Vec<f32> = Vec::new();
    let mut stereo: Vec<f32> = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();

    let mut rate = 0u32;
    let mut resample: Option<Resample> = None;

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // The track list would have to be re-examined and every decoder
            // rebuilt; what has been decoded so far is the clip.
            Err(SymphoniaError::ResetRequired) => break,
            Err(error) => return Err(format!("that file could not be read: {error}")),
        };
        if packet.track_id != track_id {
            continue;
        }

        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // One damaged packet costs its own samples, not the whole import.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(format!("that file could not be decoded: {error}")),
        };
        if buffer.is_empty() {
            continue;
        }

        let buffer_rate = buffer.spec().rate();
        let channels = buffer.spec().channels().count();
        buffer.copy_to_vec_interleaved(&mut decoded);

        if buffer_rate == 0 {
            return Err("that file does not say what rate its audio is".to_string());
        }
        to_stereo(&decoded, channels, &mut stereo);
        if stereo.is_empty() {
            continue;
        }

        // A rate that changes mid-track is vanishingly rare, but a resampler
        // built for the old one would stretch everything after it.
        if buffer_rate != rate {
            rate = buffer_rate;
            resample = if rate == SAMPLE_RATE {
                None
            } else {
                Some(Resample::new(rate).map_err(|error| {
                    format!("that file's {rate} Hz audio cannot be resampled: {error}")
                })?)
            };
        }

        match resample.as_mut() {
            Some(resample) => resample.push(&stereo, &mut pcm),
            None => pcm.extend_from_slice(&stereo),
        }
        within_limit(frames_to_ms(pcm.len() / CHANNELS))?;
    }

    let frames = pcm.len() / CHANNELS;
    if frames == 0 {
        return Err("that file has no audio in it".to_string());
    }
    pcm.truncate(frames * CHANNELS);

    let duration_ms = frames_to_ms(frames) as u32;
    let peaks = peaks(&pcm);
    Ok(Source {
        pcm: Arc::new(pcm),
        duration_ms,
        peaks: Arc::new(peaks),
    })
}

/// Cuts `[start_ms, end_ms)` out of a decoded source and encodes it, with a
/// short fade at each edge so a cut mid-waveform does not click.
///
/// Both edges are clamped rather than refused: the range was worked out from
/// what the trim view was holding, and a rounding disagreement with the decoded
/// length must not cost the import. An empty selection is refused, because there
/// is no clip to make of it.
pub fn encode_clip(source: &Source, start_ms: u32, end_ms: u32) -> Result<Vec<u8>, String> {
    let frames = source.pcm.len() / CHANNELS;
    let start_ms = start_ms.min(source.duration_ms);
    let end_ms = end_ms.min(source.duration_ms);
    if start_ms >= end_ms {
        return Err("that selection is empty".to_string());
    }

    let first = ms_to_frames(start_ms).min(frames);
    let last = ms_to_frames(end_ms).min(frames);
    if first >= last {
        return Err("that selection is empty".to_string());
    }

    let mut cut = source.pcm[first * CHANNELS..last * CHANNELS].to_vec();
    fade_edges(&mut cut);
    let clip = SoundClip::encode(&cut).map_err(|error| match error {
        vorcall_voice::SoundError::TooManyFrames { .. } => {
            format!("a clip is at most {} minutes", MAX_SOURCE_MS / 60_000)
        }
        error => format!("that selection could not be encoded: {error}"),
    })?;
    Ok(clip.to_bytes())
}

/// Refuses anything over [`MAX_SOURCE_MS`], whether the container said so up
/// front or the decoded samples ran past it.
fn within_limit(ms: i128) -> Result<(), String> {
    if ms > i128::from(MAX_SOURCE_MS) {
        return Err(format!(
            "that file is longer than {} minutes",
            MAX_SOURCE_MS / 60_000
        ));
    }
    Ok(())
}

fn frames_to_ms(frames: usize) -> i128 {
    frames as i128 * 1000 / i128::from(SAMPLE_RATE)
}

fn ms_to_frames(ms: u32) -> usize {
    ms as usize * SAMPLE_RATE as usize / 1000
}

/// Lays one decoded buffer out as interleaved stereo: mono duplicates into both,
/// stereo passes through, and anything wider is the mean of its channels in
/// both — all a stereo clip can honestly carry, and what the share's audio
/// already does with a surround capture.
fn to_stereo(decoded: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        0 => {}
        1 => {
            for sample in decoded {
                out.push(*sample);
                out.push(*sample);
            }
        }
        CHANNELS => out.extend_from_slice(&decoded[..decoded.len() / CHANNELS * CHANNELS]),
        _ => {
            let scale = 1.0 / channels as f32;
            for frame in decoded.chunks_exact(channels) {
                let mean = frame.iter().sum::<f32>() * scale;
                out.push(mean);
                out.push(mean);
            }
        }
    }
}

/// A linear fade at each edge. A selection shorter than twice the fade gets
/// proportionally shorter ones, so no sample is ever faded from both ends.
fn fade_edges(pcm: &mut [f32]) {
    let frames = pcm.len() / CHANNELS;
    let fade = (FADE_MS * SAMPLE_RATE as usize / 1000).min(frames / 2);
    if fade == 0 {
        return;
    }
    for index in 0..fade {
        let gain = index as f32 / fade as f32;
        let tail = frames - 1 - index;
        for channel in 0..CHANNELS {
            pcm[index * CHANNELS + channel] *= gain;
            pcm[tail * CHANNELS + channel] *= gain;
        }
    }
}

/// The min and max of the mono downmix in each of [`WAVEFORM_BUCKETS`] equal
/// spans. A bucket with no frames in it — a source shorter than the bucket
/// count — is flat rather than missing, so the drawing stays pure.
fn peaks(pcm: &[f32]) -> Vec<(f32, f32)> {
    let frames = pcm.len() / CHANNELS;
    let mut peaks = Vec::with_capacity(WAVEFORM_BUCKETS);
    for bucket in 0..WAVEFORM_BUCKETS {
        let start = frames * bucket / WAVEFORM_BUCKETS;
        let end = frames * (bucket + 1) / WAVEFORM_BUCKETS;
        let mut low = 0.0f32;
        let mut high = 0.0f32;
        for frame in start..end {
            let mono = (pcm[frame * CHANNELS] + pcm[frame * CHANNELS + 1]) / 2.0;
            low = low.min(mono);
            high = high.max(mono);
        }
        peaks.push((low, high));
    }
    peaks
}

/// A sinc resampler from the source's rate to 48 kHz, interleaved stereo, fed
/// 20 ms of input at a time and producing a variable number of frames.
///
/// The audio thread and the share pipeline each hold one of these of their own;
/// they are deliberately not shared, because each lives on a thread that must
/// not wait on the others.
struct Resample {
    inner: Async<f32>,
    /// Input frames still waiting for a full chunk.
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Resample {
    fn new(rate: u32) -> Result<Self, rubato::ResamplerConstructionError> {
        let chunk = (rate as usize / 50).max(1);
        let inner = Async::<f32>::new_sinc(
            f64::from(SAMPLE_RATE) / f64::from(rate),
            1.0,
            &SincInterpolationParameters::default(),
            chunk,
            CHANNELS,
            FixedAsync::Input,
        )?;
        let output = vec![0.0; inner.output_frames_max() * CHANNELS];
        Ok(Self {
            inner,
            pending: Vec::with_capacity(chunk * CHANNELS * 2),
            output,
        })
    }

    fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        self.pending.extend_from_slice(samples);
        loop {
            let frames_in = self.inner.input_frames_next();
            let needed = frames_in * CHANNELS;
            if self.pending.len() < needed {
                return;
            }

            let frames_out = self.output.len() / CHANNELS;
            let source = match InterleavedSlice::new(&self.pending[..needed], CHANNELS, frames_in) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(%error, "cannot wrap the imported chunk");
                    self.pending.clear();
                    return;
                }
            };
            let mut target = match InterleavedSlice::new_mut(&mut self.output, CHANNELS, frames_out)
            {
                Ok(target) => target,
                Err(error) => {
                    tracing::warn!(%error, "cannot wrap the resampler output");
                    self.pending.clear();
                    return;
                }
            };

            let produced = match self.inner.process_into_buffer(&source, &mut target, None) {
                Ok((_, produced)) => produced,
                Err(error) => {
                    tracing::warn!(%error, "dropping a chunk the resampler refused");
                    0
                }
            };

            out.extend_from_slice(&self.output[..produced * CHANNELS]);
            self.pending.drain(..needed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source whose every frame is at the same amplitude, alternating sign, so
    /// every bucket of the waveform must measure exactly the same pair and no
    /// part of a cut is quiet unless the fade made it so.
    fn square(frames: usize) -> Vec<f32> {
        let mut pcm = Vec::with_capacity(frames * CHANNELS);
        for frame in 0..frames {
            let value = if frame % 2 == 0 { 0.5 } else { -0.5 };
            pcm.push(value);
            pcm.push(value);
        }
        pcm
    }

    fn source(ms: u32) -> Source {
        let frames = ms_to_frames(ms);
        let pcm = square(frames);
        let peaks = peaks(&pcm);
        Source {
            pcm: Arc::new(pcm),
            duration_ms: frames_to_ms(frames) as u32,
            peaks: Arc::new(peaks),
        }
    }

    /// The span asked for, to within the 20 ms frame `SoundClip` pads up to.
    #[test]
    fn a_cut_lasts_as_long_as_it_was_asked_to() {
        let source = source(2000);
        let bytes = encode_clip(&source, 500, 1300).expect("the cut encodes");

        let clip = SoundClip::parse(&bytes).expect("the clip parses");
        let span = 1300 - 500;
        assert!(
            clip.duration_ms().abs_diff(span) <= 20,
            "{} ms for an {span} ms selection",
            clip.duration_ms()
        );
    }

    /// The trim view's end can sit a rounding step past the decoded length; the
    /// clip is the rest of the source, not a refusal.
    #[test]
    fn a_range_past_the_end_is_clamped() {
        let source = source(500);
        let bytes = encode_clip(&source, 100, 10_000).expect("the cut encodes");

        let clip = SoundClip::parse(&bytes).expect("the clip parses");
        assert!(
            clip.duration_ms().abs_diff(400) <= 20,
            "{} ms for the last 400 ms",
            clip.duration_ms()
        );
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let source = source(500);
        let empty = Err("that selection is empty".to_string());
        assert_eq!(encode_clip(&source, 200, 200), empty);
        assert_eq!(encode_clip(&source, 300, 200), empty);
        // Both edges past the end clamp onto each other.
        assert_eq!(encode_clip(&source, 900, 1000), empty);
    }

    /// 250 ms is twelve and a half frames, so `SoundClip::encode` pads the last
    /// one: the tail of the decode is the pad, and the head is the fade plus
    /// whatever the encoder's own delay holds back. Both must be quiet, or a cut
    /// mid-waveform clicks.
    #[test]
    fn the_edges_of_a_cut_are_quiet() {
        let source = source(2000);
        let bytes = encode_clip(&source, 500, 750).expect("the cut encodes");
        let decoded = SoundClip::parse(&bytes)
            .expect("the clip parses")
            .decode_to_pcm()
            .expect("the clip decodes");

        for (name, sample) in [
            ("first", decoded[0]),
            ("second", decoded[1]),
            ("last", decoded[decoded.len() - 1]),
            ("last but one", decoded[decoded.len() - 2]),
        ] {
            assert!(sample.abs() < 0.05, "the {name} sample is {sample}");
        }
    }

    /// The fade itself, before any codec sees it: silent at both ends and
    /// untouched in the middle.
    #[test]
    fn the_fade_reaches_zero_at_both_ends() {
        let frames = ms_to_frames(100);
        let mut pcm = vec![1.0f32; frames * CHANNELS];
        fade_edges(&mut pcm);

        assert_eq!(pcm[0], 0.0);
        assert_eq!(pcm[1], 0.0);
        assert_eq!(pcm[pcm.len() - 1], 0.0);
        assert_eq!(pcm[pcm.len() - 2], 0.0);
        assert_eq!(pcm[frames], 1.0);
    }

    /// Under 10 ms there is no room for two 5 ms fades, and a sample faded from
    /// both ends would be silence.
    #[test]
    fn a_short_selection_fades_over_half_of_itself() {
        let frames = ms_to_frames(4);
        let mut pcm = vec![1.0f32; frames * CHANNELS];
        fade_edges(&mut pcm);

        assert_eq!(pcm[0], 0.0);
        assert_eq!(pcm[pcm.len() - 1], 0.0);
        // The middle pair is the loudest the fades leave it, never silent.
        let middle = pcm[frames / 2 * CHANNELS];
        assert!(middle > 0.9, "the middle of a short cut is {middle}");
    }

    #[test]
    fn a_source_over_the_limit_is_refused() {
        assert_eq!(within_limit(0), Ok(()));
        assert_eq!(within_limit(i128::from(MAX_SOURCE_MS)), Ok(()));
        assert_eq!(
            within_limit(i128::from(MAX_SOURCE_MS) + 1),
            Err("that file is longer than 10 minutes".to_string())
        );
    }

    #[test]
    fn the_waveform_is_always_the_same_width() {
        assert_eq!(
            peaks(&square(WAVEFORM_BUCKETS * 10)).len(),
            WAVEFORM_BUCKETS
        );
        assert_eq!(peaks(&square(3)).len(), WAVEFORM_BUCKETS);
        assert_eq!(peaks(&[]).len(), WAVEFORM_BUCKETS);
    }

    #[test]
    fn a_constant_source_measures_the_same_in_every_bucket() {
        let peaks = peaks(&square(WAVEFORM_BUCKETS * 10));
        assert!(
            peaks.iter().all(|pair| *pair == (-0.5, 0.5)),
            "the waveform of a constant source is not flat"
        );
    }

    /// Mono goes to both channels, stereo passes through, surround is the mean.
    #[test]
    fn every_channel_layout_becomes_stereo() {
        let mut out = Vec::new();

        to_stereo(&[0.25, -0.5], 1, &mut out);
        assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);

        to_stereo(&[0.1, 0.2, 0.3, 0.4], CHANNELS, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);

        to_stereo(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, &mut out);
        let sixth = 1.0 / 6.0;
        assert_eq!(out, vec![sixth, sixth]);

        to_stereo(&[0.5, 0.5], 0, &mut out);
        assert!(out.is_empty());
    }
}
