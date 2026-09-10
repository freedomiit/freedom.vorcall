//! A sine source and a level meter, for diagnostics and tests.

use std::f32::consts::TAU;

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

/// A continuous sine: the phase carries across `fill` calls, so consecutive
/// frames join without a click.
pub struct Tone {
    phase: f32,
    step: f32,
    amplitude: f32,
}

impl Tone {
    pub fn new(hz: f32, amplitude: f32) -> Self {
        Self {
            phase: 0.0,
            step: TAU * hz / SAMPLE_RATE as f32,
            amplitude,
        }
    }

    pub fn fill(&mut self, out: &mut [f32; FRAME_SAMPLES]) {
        for sample in out.iter_mut() {
            *sample = self.phase.sin() * self.amplitude;
            self.phase += self.step;
            if self.phase >= TAU {
                self.phase -= TAU;
            }
        }
    }
}

pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f32 = pcm.iter().map(|sample| sample * sample).sum();
    (sum / pcm.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_has_the_textbook_rms() {
        let mut tone = Tone::new(440.0, 0.3);
        let mut frame = [0.0f32; FRAME_SAMPLES];
        tone.fill(&mut frame);
        // amplitude / sqrt(2)
        assert!((rms(&frame) - 0.2121).abs() < 0.005, "{}", rms(&frame));
    }

    #[test]
    fn frames_join_without_a_discontinuity() {
        let mut tone = Tone::new(1_000.0, 1.0);
        let mut first = [0.0f32; FRAME_SAMPLES];
        let mut second = [0.0f32; FRAME_SAMPLES];
        tone.fill(&mut first);
        tone.fill(&mut second);
        let step = TAU * 1_000.0 / SAMPLE_RATE as f32;
        assert!((second[0] - first[FRAME_SAMPLES - 1]).abs() < step * 1.1);
    }

    #[test]
    fn rms_of_silence_and_of_nothing_is_zero() {
        assert_eq!(rms(&[0.0; 8]), 0.0);
        assert_eq!(rms(&[]), 0.0);
    }
}
