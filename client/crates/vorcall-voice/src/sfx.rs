//! The client's own interface sounds, synthesized rather than shipped as files.
//!
//! Each motif is a short run of sine notes played one after another, every note
//! faded in and out so nothing clicks. Two notes rising mean something arrived,
//! two falling mean it left, and the deafen pair is the same interval in both
//! directions so the two are told apart by order alone.

use std::f32::consts::TAU;

use crate::SAMPLE_RATE;

/// Quiet enough to sit under a conversation; matches the notification chime.
pub const PEAK: f32 = 0.15;

/// Long enough to kill the click at a note's start, short enough not to soften
/// the attack.
const FADE_IN_MS: usize = 5;

/// One of the client's own interface sounds. Synthesized rather than shipped as
/// files: the notification chime already works this way, and nothing here needs
/// an audio decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sfx {
    Join,
    Leave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
}

impl Sfx {
    /// Whether this motif is still played while the listener is deafened.
    /// Only the deafen pair is: it is the confirmation of the press itself.
    pub fn bypass_deafen(self) -> bool {
        matches!(self, Sfx::Deafen | Sfx::Undeafen)
    }

    /// The notes, as `(hz, ms)` in order.
    fn notes(self) -> &'static [(f32, usize)] {
        match self {
            Sfx::Join => &[(660.0, 60), (880.0, 90)],
            Sfx::Leave => &[(880.0, 60), (660.0, 90)],
            Sfx::Mute => &[(440.0, 50)],
            Sfx::Unmute => &[(590.0, 50)],
            Sfx::Deafen => &[(520.0, 70), (390.0, 70)],
            Sfx::Undeafen => &[(390.0, 70), (520.0, 70)],
        }
    }

    /// Mono 48 kHz samples, peaking at [`PEAK`] before the user's volume.
    pub fn samples(self) -> Vec<f32> {
        let notes = self.notes();
        let total: usize = notes.iter().map(|(_, ms)| samples_for(*ms)).sum();
        let mut pcm = Vec::with_capacity(total);
        for (hz, ms) in notes {
            push_note(&mut pcm, *hz, *ms);
        }

        let peak = pcm.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        if peak > 0.0 {
            let scale = PEAK / peak;
            for sample in pcm.iter_mut() {
                *sample *= scale;
            }
        }
        pcm
    }
}

fn samples_for(ms: usize) -> usize {
    ms * SAMPLE_RATE as usize / 1_000
}

/// One sine note: a linear fade in, then a linear fade out over its last third,
/// so the concatenation of two notes has no step in it.
fn push_note(pcm: &mut Vec<f32>, hz: f32, ms: usize) {
    let len = samples_for(ms);
    let fade_in = samples_for(FADE_IN_MS).min(len);
    let fade_out = (len / 3).max(1);
    for index in 0..len {
        let mut gain = 1.0f32;
        if index < fade_in {
            gain = index as f32 / fade_in as f32;
        }
        let from_end = len - 1 - index;
        if from_end < fade_out {
            gain = gain.min(from_end as f32 / (fade_out - 1).max(1) as f32);
        }
        let phase = TAU * hz * index as f32 / SAMPLE_RATE as f32;
        pcm.push(phase.sin() * gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Sfx; 6] = [
        Sfx::Join,
        Sfx::Leave,
        Sfx::Mute,
        Sfx::Unmute,
        Sfx::Deafen,
        Sfx::Undeafen,
    ];

    #[test]
    fn every_motif_has_the_length_its_notes_add_up_to() {
        // 48 samples per millisecond at 48 kHz.
        assert_eq!(Sfx::Join.samples().len(), 48 * (60 + 90));
        assert_eq!(Sfx::Leave.samples().len(), 48 * (60 + 90));
        assert_eq!(Sfx::Mute.samples().len(), 48 * 50);
        assert_eq!(Sfx::Unmute.samples().len(), 48 * 50);
        assert_eq!(Sfx::Deafen.samples().len(), 48 * (70 + 70));
        assert_eq!(Sfx::Undeafen.samples().len(), 48 * (70 + 70));
    }

    #[test]
    fn every_motif_peaks_at_the_shared_ceiling() {
        for sfx in ALL {
            let pcm = sfx.samples();
            let peak = pcm.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert!(peak <= PEAK + 1e-6, "{sfx:?} peaked at {peak}");
            assert!(peak > PEAK - 1e-3, "{sfx:?} only reached {peak}");
        }
    }

    #[test]
    fn every_motif_fades_in_and_out() {
        for sfx in ALL {
            let pcm = sfx.samples();
            assert!(pcm[0].abs() < 1e-6, "{sfx:?} starts at {}", pcm[0]);
            let last = pcm[pcm.len() - 1];
            assert!(last.abs() < 1e-6, "{sfx:?} ends at {last}");
        }
    }

    #[test]
    fn only_the_deafen_pair_bypasses_deafen() {
        for sfx in ALL {
            let expected = matches!(sfx, Sfx::Deafen | Sfx::Undeafen);
            assert_eq!(sfx.bypass_deafen(), expected, "{sfx:?}");
        }
    }
}
