//! Every remote speaker, decoded and summed into one frame.
//!
//! The engine's receive task feeds packets in by ssrc; the audio thread calls
//! [`Playout::next_frame`] every 20 ms. Each speaker owns its jitter buffer and
//! its decoder, so one speaker's loss or reset never disturbs another's.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};

use crate::FRAME_SAMPLES;
use crate::codec::Decoder;
use crate::jitter::{Frame, Incoming, JitterBuffer};

/// A speaker heard from this long ago is gone; its decoder is dropped so stale
/// ssrcs do not accumulate over a long call.
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
}

impl Playout {
    pub fn new() -> Self {
        Self {
            speakers: HashMap::new(),
            tuning: HashMap::new(),
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
    /// of them contributed audio.
    pub fn next_frame(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> usize {
        let now = Instant::now();
        out.fill(0.0);
        let mut mixed = 0;
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
            for (sum, sample) in out.iter_mut().zip(decoded.iter()) {
                *sum += *sample * factor;
            }
        }

        if mixed > 0 {
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

    pub fn stats(&self) -> Vec<(u32, PeerStats)> {
        let mut stats: Vec<(u32, PeerStats)> = self
            .speakers
            .iter()
            .map(|(ssrc, speaker)| {
                let jitter = speaker.jitter.stats();
                (
                    *ssrc,
                    PeerStats {
                        received: jitter.received,
                        lost: jitter.lost,
                        concealed: jitter.concealed,
                        late: jitter.late,
                        duplicates: jitter.duplicates,
                        decoded_frames: speaker.decoded_frames,
                        decoder_resets: speaker.decoder.resets(),
                    },
                )
            })
            .collect();
        stats.sort_unstable_by_key(|(ssrc, _)| *ssrc);
        stats
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
    use crate::codec::Encoder;
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
}
