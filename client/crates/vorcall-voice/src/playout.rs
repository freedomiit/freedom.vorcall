//! Every remote speaker, decoded and summed into one frame.
//!
//! The engine's receive task feeds packets in by ssrc; the audio thread calls
//! [`Playout::next_frame`] (or [`Playout::next_stereo_frame`]) every 20 ms.
//! Each speaker owns its jitter buffer and its decoder, so one speaker's loss
//! or reset never disturbs another's.
//!
//! A watched screen share's audio is a stream of its own: stereo, one at a
//! time, with its own buffer, decoder and volume, mixed in next to the voices.
//! Unlike a speaker it is never reaped for going quiet — a shared window that
//! plays nothing sends nothing, and the stream is the watcher's choice rather
//! than something inferred from traffic.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};

use crate::FRAME_SAMPLES;
use crate::codec::{Decoder, STEREO_FRAME_SAMPLES, StereoDecoder};
use crate::jitter::{Frame, Incoming, JitterBuffer};

/// A speaker heard from this long ago is gone; its decoder is dropped so stale
/// ssrcs do not accumulate over a long call. The share stream is exempt: see
/// [`Playout::push_share`].
const SPEAKER_TIMEOUT: Duration = Duration::from_secs(30);
/// 200 %: enough to rescue a quiet talker without turning the mix to mush.
const MAX_GAIN: f32 = 2.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerStats {
    pub received: u64,
    pub lost: u64,
    pub concealed: u64,
    pub late: u64,
    pub duplicates: u64,
    pub decoded_frames: u64,
    pub decoder_resets: u32,
}

struct Speaker {
    jitter: JitterBuffer,
    decoder: Decoder,
    decoded_frames: u64,
}

/// The one screen share being watched, if any.
struct Share {
    ssrc: u32,
    jitter: JitterBuffer,
    decoder: StereoDecoder,
    decoded_frames: u64,
}

/// What the listener did to a speaker's volume.
#[derive(Clone, Copy)]
struct Tuning {
    gain: f32,
    muted: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
        }
    }
}

pub struct Playout {
    speakers: HashMap<u32, Speaker>,
    /// Kept apart from `speakers` so a setting made before the first packet, or
    /// while an idle speaker is reaped and heard from again, survives.
    tuning: HashMap<u32, Tuning>,
    share: Option<Share>,
    /// Outside `share` for the same reason `tuning` is outside `speakers`.
    share_gain: f32,
}

impl Playout {
    pub fn new() -> Self {
        Self {
            speakers: HashMap::new(),
            tuning: HashMap::new(),
            share: None,
            share_gain: 1.0,
        }
    }

    pub fn push(&mut self, ssrc: u32, packet: Incoming) {
        let now = Instant::now();
        let speaker = match self.speakers.entry(ssrc) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => match Decoder::new() {
                Ok(decoder) => entry.insert(Speaker {
                    jitter: JitterBuffer::new(),
                    decoder,
                    decoded_frames: 0,
                }),
                Err(error) => {
                    tracing::warn!(ssrc, %error, "no decoder for speaker, dropping packet");
                    return;
                }
            },
        };
        speaker.jitter.push(now, packet);
    }

    /// Sums one 20 ms frame from every speaker into `out` and returns how many
    /// of them contributed audio. A watched share's audio is folded in as its
    /// own downmix, so the mono path hears it too; it is not a speaker and is
    /// not counted as one.
    pub fn next_frame(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> usize {
        self.mix(Instant::now(), out, false)
    }

    /// The same mix, interleaved stereo: every voice reaches both channels
    /// equally and the share stream keeps its own left and right.
    pub fn next_stereo_frame(&mut self, out: &mut [f32; STEREO_FRAME_SAMPLES]) -> usize {
        self.mix(Instant::now(), out, true)
    }

    fn mix(&mut self, now: Instant, out: &mut [f32], stereo: bool) -> usize {
        out.fill(0.0);
        let mut mixed = 0;
        let mut audible = false;
        let mut decoded = [0.0f32; FRAME_SAMPLES];

        for (ssrc, speaker) in self.speakers.iter_mut() {
            let packet = match speaker.jitter.pull(now) {
                Frame::Packet(payload) => Some(payload),
                Frame::Lost => None,
                Frame::Idle => continue,
            };
            if let Err(error) = speaker.decoder.decode(packet.as_deref(), &mut decoded) {
                tracing::debug!(%error, "dropping an undecodable frame");
                continue;
            }
            // Decoded even when it is not heard: the concealment and the buffer
            // depth only stay right while the stream keeps draining.
            speaker.decoded_frames += 1;

            let tuning = self.tuning.get(ssrc).copied().unwrap_or_default();
            let factor = if tuning.muted { 0.0 } else { tuning.gain };
            if factor == 0.0 {
                continue;
            }
            mixed += 1;
            audible = true;
            if stereo {
                for (pair, sample) in out.as_chunks_mut::<2>().0.iter_mut().zip(decoded.iter()) {
                    pair[0] += *sample * factor;
                    pair[1] += *sample * factor;
                }
            } else {
                for (sum, sample) in out.iter_mut().zip(decoded.iter()) {
                    *sum += *sample * factor;
                }
            }
        }

        audible |= self.mix_share(out, stereo, now);

        if audible {
            for sample in out.iter_mut() {
                *sample = sample.clamp(-1.0, 1.0);
            }
        }

        self.speakers.retain(|_, speaker| {
            speaker
                .jitter
                .last_packet_at()
                .is_some_and(|last| now.saturating_duration_since(last) < SPEAKER_TIMEOUT)
        });
        mixed
    }

    /// Adds the share stream to `out`; `true` when anything was heard.
    fn mix_share(&mut self, out: &mut [f32], stereo: bool, now: Instant) -> bool {
        let Some(share) = self.share.as_mut() else {
            return false;
        };
        let packet = match share.jitter.pull(now) {
            Frame::Packet(payload) => Some(payload),
            Frame::Lost => None,
            Frame::Idle => return false,
        };
        let mut decoded = [0.0f32; STEREO_FRAME_SAMPLES];
        if let Err(error) = share.decoder.decode(packet.as_deref(), &mut decoded) {
            tracing::debug!(%error, "dropping an undecodable share frame");
            return false;
        }
        share.decoded_frames += 1;

        let gain = self.share_gain;
        if gain == 0.0 {
            return false;
        }
        if stereo {
            for (pair, source) in out
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(decoded.as_chunks::<2>().0.iter())
            {
                pair[0] += source[0] * gain;
                pair[1] += source[1] * gain;
            }
        } else {
            for (sum, source) in out.iter_mut().zip(decoded.as_chunks::<2>().0.iter()) {
                *sum += (source[0] + source[1]) * 0.5 * gain;
            }
        }
        true
    }

    /// Feeds the watched share's audio. A packet from a different ssrc starts a
    /// new stream: one share is heard at a time.
    ///
    /// The stream lives until [`remove_share`](Self::remove_share) or another
    /// sharer takes it over; silence never retires it. A share whose endpoint
    /// is idle — a loopback capture with nothing playing — sends nothing for
    /// minutes, and dropping it would cost the buffer and the decoder every
    /// time the sound came back.
    pub fn push_share(&mut self, ssrc: u32, packet: Incoming) {
        let now = Instant::now();
        if self.share.as_ref().is_none_or(|share| share.ssrc != ssrc) {
            match StereoDecoder::new() {
                Ok(decoder) => {
                    self.share = Some(Share {
                        ssrc,
                        jitter: JitterBuffer::new(),
                        decoder,
                        decoded_frames: 0,
                    });
                }
                Err(error) => {
                    tracing::warn!(ssrc, %error, "no decoder for the share, dropping packet");
                    return;
                }
            }
        }
        if let Some(share) = self.share.as_mut() {
            share.jitter.push(now, packet);
        }
    }

    /// The share's volume, clamped to 0.0..=2.0 like a speaker's. Default 1.0,
    /// and it outlives the stream it applies to.
    pub fn set_share_gain(&mut self, gain: f32) {
        self.share_gain = gain.clamp(0.0, MAX_GAIN);
    }

    pub fn remove_share(&mut self) {
        self.share = None;
    }

    pub fn share_stats(&self) -> Option<(u32, PeerStats)> {
        self.share.as_ref().map(|share| {
            (
                share.ssrc,
                peer_stats(&share.jitter, share.decoded_frames, share.decoder.resets()),
            )
        })
    }

    /// The ssrcs whose last packet arrived within `within` — the local speaking
    /// indicator, which needs no help from the server.
    pub fn speaking(&self, within: Duration) -> Vec<u32> {
        let now = Instant::now();
        let mut speaking: Vec<u32> = self
            .speakers
            .iter()
            .filter(|(_, speaker)| {
                speaker
                    .jitter
                    .last_packet_at()
                    .is_some_and(|last| now.saturating_duration_since(last) <= within)
            })
            .map(|(ssrc, _)| *ssrc)
            .collect();
        speaking.sort_unstable();
        speaking
    }

    /// Per-speaker volume, clamped to 0.0..=2.0 (0 to 200 %). Default 1.0.
    pub fn set_gain(&mut self, ssrc: u32, gain: f32) {
        self.tuning.entry(ssrc).or_default().gain = gain.clamp(0.0, MAX_GAIN);
    }

    pub fn set_muted(&mut self, ssrc: u32, muted: bool) {
        self.tuning.entry(ssrc).or_default().muted = muted;
    }

    /// The gain and mute currently applied to `ssrc`.
    pub fn tuning(&self, ssrc: u32) -> (f32, bool) {
        let tuning = self.tuning.get(&ssrc).copied().unwrap_or_default();
        (tuning.gain, tuning.muted)
    }

    pub fn clear_tuning(&mut self, ssrc: u32) {
        self.tuning.remove(&ssrc);
    }

    /// Drops the speaker along with its tuning. An ssrc belongs to one voice
    /// session, so the app re-applies a user's gain and mute every time their
    /// ssrc changes.
    pub fn remove(&mut self, ssrc: u32) {
        self.speakers.remove(&ssrc);
        self.tuning.remove(&ssrc);
    }

    /// The speakers only; the share stream is reported by
    /// [`share_stats`](Self::share_stats).
    pub fn stats(&self) -> Vec<(u32, PeerStats)> {
        let mut stats: Vec<(u32, PeerStats)> = self
            .speakers
            .iter()
            .map(|(ssrc, speaker)| {
                (
                    *ssrc,
                    peer_stats(
                        &speaker.jitter,
                        speaker.decoded_frames,
                        speaker.decoder.resets(),
                    ),
                )
            })
            .collect();
        stats.sort_unstable_by_key(|(ssrc, _)| *ssrc);
        stats
    }
}

fn peer_stats(jitter: &JitterBuffer, decoded_frames: u64, decoder_resets: u32) -> PeerStats {
    let stats = jitter.stats();
    PeerStats {
        received: stats.received,
        lost: stats.lost,
        concealed: stats.concealed,
        late: stats.late,
        duplicates: stats.duplicates,
        decoded_frames,
        decoder_resets,
    }
}

impl Default for Playout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Encoder, StereoEncoder};
    use crate::tone::{Tone, rms};

    fn opus_frames(amplitude: f32, count: usize) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new().expect("encoder");
        let mut tone = Tone::new(440.0, amplitude);
        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let mut packet = [0u8; 512];
        (0..count)
            .map(|_| {
                tone.fill(&mut pcm);
                let written = encoder.encode(&pcm, &mut packet).expect("encodes");
                packet[..written].to_vec()
            })
            .collect()
    }

    /// Mixes `pulls` frames and hands back the last one with its speaker count.
    fn frame_after(playout: &mut Playout, pulls: usize) -> ([f32; FRAME_SAMPLES], usize) {
        let mut out = [0.0f32; FRAME_SAMPLES];
        let mut mixed = 0;
        for _ in 0..pulls {
            mixed = playout.next_frame(&mut out);
        }
        (out, mixed)
    }

    fn feed(playout: &mut Playout, ssrc: u32, frames: &[Vec<u8>]) {
        for (index, payload) in frames.iter().enumerate() {
            playout.push(
                ssrc,
                Incoming {
                    seq: index as u64,
                    ts: (index as u32) * 960,
                    marker: index == 0,
                    payload: payload.clone(),
                },
            );
        }
    }

    #[test]
    fn mixes_two_speakers_and_clamps_the_sum() {
        let frames = opus_frames(0.9, 5);
        let mut playout = Playout::new();
        feed(&mut playout, 1, &frames);
        feed(&mut playout, 2, &frames);

        let mut out = [0.0f32; FRAME_SAMPLES];
        assert_eq!(playout.next_frame(&mut out), 2);
        assert!(
            out.iter().all(|sample| (-1.0..=1.0).contains(sample)),
            "the mix left the valid range"
        );
        assert!(rms(&out) > 0.0, "the mix is silent");

        let stats = playout.stats();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].0, 1);
        assert_eq!(stats[0].1.received, 5);
        assert_eq!(stats[0].1.decoded_frames, 1);
        assert_eq!(stats[0].1.decoder_resets, 0);
    }

    #[test]
    fn an_idle_speaker_contributes_nothing() {
        let mut playout = Playout::new();
        feed(&mut playout, 1, &opus_frames(0.5, 1));

        // One packet, no deadline reached: the buffer is still filling.
        let mut out = [0.0f32; FRAME_SAMPLES];
        assert_eq!(playout.next_frame(&mut out), 0);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn speaking_tracks_recent_packets() {
        let mut playout = Playout::new();
        let frames = opus_frames(0.5, 1);
        feed(&mut playout, 7, &frames);
        feed(&mut playout, 9, &frames);

        assert_eq!(playout.speaking(Duration::from_millis(200)), vec![7, 9]);

        playout.remove(9);
        assert_eq!(playout.speaking(Duration::from_millis(200)), vec![7]);
        assert_eq!(playout.stats().len(), 1);
    }

    #[test]
    fn gain_scales_a_speaker_linearly() {
        let frames = opus_frames(0.5, 6);
        let mut full = Playout::new();
        feed(&mut full, 1, &frames);
        let mut half = Playout::new();
        // Set before the first packet ever arrives.
        half.set_gain(1, 0.5);
        feed(&mut half, 1, &frames);

        let (loud_frame, loud_mixed) = frame_after(&mut full, 3);
        let (quiet_frame, quiet_mixed) = frame_after(&mut half, 3);
        assert_eq!(loud_mixed, 1);
        assert_eq!(quiet_mixed, 1);

        let loud = rms(&loud_frame);
        let quiet = rms(&quiet_frame);
        assert!(loud > 0.01, "the reference mix is silent: {loud}");
        assert!(
            (quiet - loud * 0.5).abs() < loud * 0.05,
            "half gain gave {quiet}, expected about {}",
            loud * 0.5
        );
    }

    #[test]
    fn gain_above_one_clamps_the_mix() {
        let frames = opus_frames(0.9, 6);
        let mut plain = Playout::new();
        feed(&mut plain, 1, &frames);
        let mut boosted = Playout::new();
        boosted.set_gain(1, 2.0);
        feed(&mut boosted, 1, &frames);

        let (plain_frame, _) = frame_after(&mut plain, 3);
        let (boosted_frame, mixed) = frame_after(&mut boosted, 3);
        assert_eq!(mixed, 1);
        assert!(
            boosted_frame
                .iter()
                .all(|sample| (-1.0..=1.0).contains(sample)),
            "the boosted mix left the valid range"
        );
        assert!(
            rms(&boosted_frame) > rms(&plain_frame),
            "{} is no louder than {}",
            rms(&boosted_frame),
            rms(&plain_frame)
        );
    }

    #[test]
    fn a_muted_speaker_is_silent_but_still_decoded() {
        let mut playout = Playout::new();
        playout.set_muted(1, true);
        feed(&mut playout, 1, &opus_frames(0.6, 6));

        let mut out = [0.0f32; FRAME_SAMPLES];
        assert_eq!(playout.next_frame(&mut out), 0);
        assert_eq!(rms(&out), 0.0);
        // The jitter buffer keeps draining and the decoder keeps its state.
        assert_eq!(playout.stats()[0].1.decoded_frames, 1);

        playout.set_muted(1, false);
        assert_eq!(playout.next_frame(&mut out), 1);
        assert!(rms(&out) > 0.0, "unmuting left the speaker silent");
    }

    #[test]
    fn a_muted_speaker_does_not_silence_the_others() {
        let frames = opus_frames(0.6, 6);
        let mut playout = Playout::new();
        playout.set_muted(1, true);
        feed(&mut playout, 1, &frames);
        feed(&mut playout, 2, &frames);

        let mut out = [0.0f32; FRAME_SAMPLES];
        assert_eq!(playout.next_frame(&mut out), 1);
        assert!(rms(&out) > 0.0, "the unmuted speaker was not heard");
    }

    #[test]
    fn tuning_clamps_and_outlives_the_speaker() {
        let mut playout = Playout::new();
        assert_eq!(playout.tuning(3), (1.0, false));
        playout.set_gain(3, 5.0);
        assert_eq!(playout.tuning(3), (2.0, false));
        playout.set_gain(3, -1.0);
        assert_eq!(playout.tuning(3), (0.0, false));
        playout.set_muted(3, true);
        assert_eq!(playout.tuning(3), (0.0, true));

        // Nothing was ever heard from ssrc 3, and a mix cycle does not forget.
        let mut out = [0.0f32; FRAME_SAMPLES];
        assert_eq!(playout.next_frame(&mut out), 0);
        assert!(playout.stats().is_empty());
        assert_eq!(playout.tuning(3), (0.0, true));

        playout.clear_tuning(3);
        assert_eq!(playout.tuning(3), (1.0, false));
    }

    #[test]
    fn removing_a_speaker_clears_its_tuning() {
        let mut playout = Playout::new();
        feed(&mut playout, 1, &opus_frames(0.5, 3));
        playout.set_gain(1, 0.25);
        playout.set_muted(1, true);

        playout.remove(1);
        assert!(playout.stats().is_empty());
        assert_eq!(playout.tuning(1), (1.0, false));
    }

    fn stereo_opus_frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut tone = Tone::new(440.0, 0.5);
        let mut left = [0.0f32; FRAME_SAMPLES];
        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut packet = [0u8; 1156];
        (0..count)
            .map(|_| {
                // Left only, so the mix has to keep the channels apart.
                tone.fill(&mut left);
                for (pair, sample) in pcm.as_chunks_mut::<2>().0.iter_mut().zip(left.iter()) {
                    pair[0] = *sample;
                    pair[1] = 0.0;
                }
                let written = encoder.encode(&pcm, &mut packet).expect("encodes");
                packet[..written].to_vec()
            })
            .collect()
    }

    fn feed_share(playout: &mut Playout, ssrc: u32, frames: &[Vec<u8>]) {
        for (index, payload) in frames.iter().enumerate() {
            playout.push_share(
                ssrc,
                Incoming {
                    seq: index as u64,
                    ts: (index as u32) * 960,
                    marker: index == 0,
                    payload: payload.clone(),
                },
            );
        }
    }

    fn stereo_frame_after(
        playout: &mut Playout,
        pulls: usize,
    ) -> ([f32; STEREO_FRAME_SAMPLES], usize) {
        let mut out = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut mixed = 0;
        for _ in 0..pulls {
            mixed = playout.next_stereo_frame(&mut out);
        }
        (out, mixed)
    }

    fn channel(frame: &[f32; STEREO_FRAME_SAMPLES], index: usize) -> Vec<f32> {
        frame.iter().skip(index).step_by(2).copied().collect()
    }

    #[test]
    fn a_voice_reaches_both_channels_equally() {
        let mut playout = Playout::new();
        feed(&mut playout, 1, &opus_frames(0.5, 6));

        let (frame, mixed) = stereo_frame_after(&mut playout, 3);
        assert_eq!(mixed, 1);
        let (left, right) = (channel(&frame, 0), channel(&frame, 1));
        assert!(rms(&left) > 0.01, "the voice is silent: {}", rms(&left));
        assert_eq!(left, right);
    }

    #[test]
    fn the_share_keeps_its_own_left_and_right() {
        let mut playout = Playout::new();
        feed_share(&mut playout, 42, &stereo_opus_frames(6));

        let (frame, mixed) = stereo_frame_after(&mut playout, 3);
        // The share is heard but is not a speaker.
        assert_eq!(mixed, 0);
        let (left, right) = (channel(&frame, 0), channel(&frame, 1));
        assert!(rms(&left) > 0.01, "the share is silent: {}", rms(&left));
        // Measured: left 0.355, right 7.4e-13 — the codec's mid/side coding
        // leaves an all-zero channel all but exactly zero.
        assert!(
            rms(&right) < rms(&left) * 0.01,
            "left {}, right {}",
            rms(&left),
            rms(&right)
        );
    }

    #[test]
    fn the_mono_mix_carries_the_share_downmix() {
        let frames = stereo_opus_frames(6);
        let mut stereo = Playout::new();
        feed_share(&mut stereo, 42, &frames);
        let (stereo_frame, _) = stereo_frame_after(&mut stereo, 3);

        let mut mono = Playout::new();
        feed_share(&mut mono, 42, &frames);
        let (mono_frame, mixed) = frame_after(&mut mono, 3);
        assert_eq!(mixed, 0);

        // Left only in, so the downmix is half of it.
        let left = rms(&channel(&stereo_frame, 0));
        let down = rms(&mono_frame);
        assert!(down > 0.005, "the mono mix lost the share: {down}");
        assert!(
            (down - left * 0.5).abs() < left * 0.05,
            "downmix {down}, expected about {}",
            left * 0.5
        );
    }

    #[test]
    fn the_share_is_not_a_speaker() {
        let mut playout = Playout::new();
        feed_share(&mut playout, 42, &stereo_opus_frames(3));
        feed(&mut playout, 7, &opus_frames(0.5, 3));

        assert_eq!(playout.speaking(Duration::from_millis(200)), vec![7]);
        assert_eq!(playout.stats().len(), 1);
        assert_eq!(playout.stats()[0].0, 7);

        let (ssrc, stats) = playout.share_stats().expect("the share reports");
        assert_eq!(ssrc, 42);
        assert_eq!(stats.received, 3);
        assert_eq!(stats.decoded_frames, 0);
        assert_eq!(stats.decoder_resets, 0);

        playout.remove_share();
        assert!(playout.share_stats().is_none());
        assert_eq!(playout.speaking(Duration::from_millis(200)), vec![7]);
    }

    #[test]
    fn a_second_sharer_replaces_the_stream() {
        let frames = stereo_opus_frames(4);
        let mut playout = Playout::new();
        feed_share(&mut playout, 42, &frames);
        feed_share(&mut playout, 43, &frames[..2]);

        let (ssrc, stats) = playout.share_stats().expect("the share reports");
        assert_eq!(ssrc, 43);
        assert_eq!(stats.received, 2);
    }

    #[test]
    fn the_share_gain_clamps_silences_and_outlives_the_stream() {
        let frames = stereo_opus_frames(6);
        let mut playout = Playout::new();
        playout.set_share_gain(5.0);
        playout.set_share_gain(0.0);
        feed_share(&mut playout, 42, &frames);

        let (silent, mixed) = stereo_frame_after(&mut playout, 3);
        assert_eq!(mixed, 0);
        assert_eq!(rms(&silent), 0.0);
        // Muted but still drained, like a muted speaker.
        assert_eq!(playout.share_stats().expect("reports").1.decoded_frames, 3);

        // A new stream under the same setting is just as silent.
        playout.remove_share();
        feed_share(&mut playout, 43, &frames);
        let (still_silent, _) = stereo_frame_after(&mut playout, 3);
        assert_eq!(rms(&still_silent), 0.0);

        playout.set_share_gain(3.0);
        let (loud, _) = stereo_frame_after(&mut playout, 1);
        assert!(rms(&loud) > 0.0, "the share stayed silent after unmuting");
        assert!(
            loud.iter().all(|sample| (-1.0..=1.0).contains(sample)),
            "the boosted share left the valid range"
        );
    }

    #[test]
    fn a_silent_share_stream_is_kept_until_removed() {
        let mut playout = Playout::new();
        feed_share(&mut playout, 42, &stereo_opus_frames(6));
        feed(&mut playout, 7, &opus_frames(0.5, 6));
        let (_, mixed) = stereo_frame_after(&mut playout, 3);
        assert_eq!(mixed, 1);
        assert!(playout.share_stats().is_some());

        // Long past the speaker timeout, with nothing arriving in between.
        let mut out = [0.0f32; STEREO_FRAME_SAMPLES];
        let later = Instant::now() + SPEAKER_TIMEOUT + Duration::from_secs(1);
        playout.mix(later, &mut out, true);

        let (ssrc, stats) = playout.share_stats().expect("the share is still there");
        assert_eq!(ssrc, 42);
        assert_eq!(stats.received, 6);
        // The voice beside it was reaped on the same pass.
        assert!(playout.stats().is_empty());

        playout.remove_share();
        assert!(playout.share_stats().is_none());
    }
}
