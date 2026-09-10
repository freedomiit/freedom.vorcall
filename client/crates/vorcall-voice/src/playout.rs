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

pub struct Playout {
    speakers: HashMap<u32, Speaker>,
}

impl Playout {
    pub fn new() -> Self {
        Self {
            speakers: HashMap::new(),
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

        for speaker in self.speakers.values_mut() {
            let packet = match speaker.jitter.pull(now) {
                Frame::Packet(payload) => Some(payload),
                Frame::Lost => None,
                Frame::Idle => continue,
            };
            if let Err(error) = speaker.decoder.decode(packet.as_deref(), &mut decoded) {
                tracing::debug!(%error, "dropping an undecodable frame");
                continue;
            }
            speaker.decoded_frames += 1;
            mixed += 1;
            for (sum, sample) in out.iter_mut().zip(decoded.iter()) {
                *sum += *sample;
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

    pub fn remove(&mut self, ssrc: u32) {
        self.speakers.remove(&ssrc);
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
}
