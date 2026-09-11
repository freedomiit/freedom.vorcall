//! The capture-side noise gate: which 20 ms frames are worth sending.
//!
//! One RMS per frame decides. The gate opens on the first frame that reaches
//! the threshold and closes only once the level has stayed [`HYSTERESIS_DB`]
//! below it for [`HOLD`] — the pauses inside a sentence are shorter than that,
//! so a spurt goes out whole instead of being chopped at every word gap.

use std::time::{Duration, Instant};

use crate::FRAME_SAMPLES;
use crate::tone::rms;

pub const MIN_THRESHOLD_DB: f32 = -60.0;
pub const MAX_THRESHOLD_DB: f32 = -20.0;
pub const DEFAULT_THRESHOLD_DB: f32 = -45.0;
/// How far below the open threshold the level may fall before the hold timer
/// starts.
pub const HYSTERESIS_DB: f32 = 6.0;
/// How long the level must stay below `threshold - HYSTERESIS_DB` before the
/// gate closes.
pub const HOLD: Duration = Duration::from_millis(300);
/// The level reported for digital silence, which has no logarithm.
pub const SILENCE_DB: f32 = -100.0;

/// 20·log10(rms), floored at [`SILENCE_DB`].
pub fn dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        return SILENCE_DB;
    }
    (20.0 * rms.log10()).max(SILENCE_DB)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Not sent.
    Closed,
    /// The first frame of a spurt: send it with the marker.
    Opened,
    /// Send.
    Open,
}

pub struct NoiseGate {
    threshold_db: f32,
    open: bool,
    /// When the level first fell under the closing threshold, cleared by any
    /// frame loud enough to keep the gate open.
    below_since: Option<Instant>,
}

impl NoiseGate {
    pub fn new(threshold_db: f32) -> Self {
        Self {
            threshold_db: threshold_db.clamp(MIN_THRESHOLD_DB, MAX_THRESHOLD_DB),
            open: false,
            below_since: None,
        }
    }

    /// Retunes the gate without disturbing a spurt already in progress.
    pub fn set_threshold(&mut self, threshold_db: f32) {
        self.threshold_db = threshold_db.clamp(MIN_THRESHOLD_DB, MAX_THRESHOLD_DB);
    }

    pub fn threshold_db(&self) -> f32 {
        self.threshold_db
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Advances the gate by one 20 ms frame and returns the decision for that
    /// frame.
    pub fn process(&mut self, frame: &[f32; FRAME_SAMPLES], now: Instant) -> GateDecision {
        let level = dbfs(rms(frame));

        if !self.open {
            if level >= self.threshold_db {
                self.open = true;
                self.below_since = None;
                return GateDecision::Opened;
            }
            return GateDecision::Closed;
        }

        if level >= self.threshold_db - HYSTERESIS_DB {
            self.below_since = None;
            return GateDecision::Open;
        }

        let since = *self.below_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= HOLD {
            self.open = false;
            self.below_since = None;
            return GateDecision::Closed;
        }
        // The tail of the hold is still speech as far as the listener is
        // concerned: send it.
        GateDecision::Open
    }

    /// The level the meter shows for `frame`, in dBFS.
    pub fn level(frame: &[f32; FRAME_SAMPLES]) -> f32 {
        dbfs(rms(frame))
    }
}

impl Default for NoiseGate {
    fn default() -> Self {
        Self::new(DEFAULT_THRESHOLD_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tone::Tone;

    /// A 440 Hz frame whose RMS is `db` dBFS. A sine of amplitude `a` has RMS
    /// `a / sqrt(2)`, and the phase restarts every frame so the level does not
    /// drift with the caller's frame count.
    fn frame_at(db: f32) -> [f32; FRAME_SAMPLES] {
        let amplitude = 10f32.powf(db / 20.0) * 2f32.sqrt();
        let mut tone = Tone::new(440.0, amplitude);
        let mut frame = [0.0f32; FRAME_SAMPLES];
        tone.fill(&mut frame);
        frame
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn dbfs_of_silence_is_the_floor() {
        assert!((dbfs(0.0) - SILENCE_DB).abs() < 0.001);
        assert!((dbfs(-1.0) - SILENCE_DB).abs() < 0.001);
        // 20·log10(1e-9) is -180 dB, which the floor swallows.
        assert!((dbfs(1e-9) - SILENCE_DB).abs() < 0.001);
        assert!((NoiseGate::level(&[0.0; FRAME_SAMPLES]) - SILENCE_DB).abs() < 0.001);
    }

    #[test]
    fn dbfs_of_full_scale_sine_is_about_minus_three() {
        let mut tone = Tone::new(440.0, 1.0);
        let mut frame = [0.0f32; FRAME_SAMPLES];
        tone.fill(&mut frame);
        // 20·log10(1 / sqrt(2)) = -3.0103 dB.
        let level = NoiseGate::level(&frame);
        assert!((level + 3.0103).abs() < 0.2, "{level}");
    }

    #[test]
    fn level_reports_the_frame_in_dbfs() {
        let mut tone = Tone::new(440.0, 0.1);
        let mut frame = [0.0f32; FRAME_SAMPLES];
        tone.fill(&mut frame);
        // 20·log10(0.1 / sqrt(2)) = -23.0103 dB.
        let level = NoiseGate::level(&frame);
        assert!((level + 23.0103).abs() < 0.1, "{level}");
        assert!((level - dbfs(rms(&frame))).abs() < 0.001);
    }

    #[test]
    fn the_threshold_clamps_to_the_usable_range() {
        assert!((NoiseGate::new(-200.0).threshold_db() - MIN_THRESHOLD_DB).abs() < 0.001);
        assert!((NoiseGate::new(0.0).threshold_db() - MAX_THRESHOLD_DB).abs() < 0.001);
        assert!((NoiseGate::new(-45.0).threshold_db() + 45.0).abs() < 0.001);

        let mut gate = NoiseGate::default();
        assert!((gate.threshold_db() - DEFAULT_THRESHOLD_DB).abs() < 0.001);
        gate.set_threshold(-200.0);
        assert!((gate.threshold_db() - MIN_THRESHOLD_DB).abs() < 0.001);
        gate.set_threshold(10.0);
        assert!((gate.threshold_db() - MAX_THRESHOLD_DB).abs() < 0.001);
    }

    #[test]
    fn setting_the_threshold_does_not_disturb_an_open_spurt() {
        let t0 = Instant::now();
        let mut gate = NoiseGate::new(-45.0);
        assert_eq!(gate.process(&frame_at(-40.0), t0), GateDecision::Opened);
        gate.set_threshold(-30.0);
        assert!(gate.is_open());
    }

    #[test]
    fn opens_at_the_threshold_and_stays_closed_below_it() {
        let t0 = Instant::now();

        let mut quiet = NoiseGate::new(-45.0);
        assert_eq!(quiet.process(&frame_at(-46.0), t0), GateDecision::Closed);
        assert!(!quiet.is_open());

        // A 20 ms window holds 8.8 cycles of 440 Hz, so this frame measures
        // 0.02 dB over its nominal level: the boundary is tested on the open
        // side of `>=`, deterministically.
        let mut loud = NoiseGate::new(-45.0);
        assert_eq!(loud.process(&frame_at(-45.0), t0), GateDecision::Opened);
        assert!(loud.is_open());
        // Only the first frame of the spurt carries the marker.
        assert_eq!(
            loud.process(&frame_at(-45.0), at(t0, 20)),
            GateDecision::Open
        );
    }

    #[test]
    fn hysteresis_holds_the_gate_open_below_the_threshold() {
        let t0 = Instant::now();
        let mut gate = NoiseGate::new(-45.0);
        assert_eq!(gate.process(&frame_at(-40.0), t0), GateDecision::Opened);

        // 5 dB down is still above threshold - HYSTERESIS_DB, so the hold timer
        // never starts however long the level stays there.
        let quiet = frame_at(-50.0);
        for step in 1..=100 {
            assert_eq!(gate.process(&quiet, at(t0, step * 20)), GateDecision::Open);
        }
        assert!(gate.is_open());
    }

    #[test]
    fn closes_after_the_hold_and_reopens_on_the_next_spurt() {
        let t0 = Instant::now();
        let mut gate = NoiseGate::new(-45.0);
        assert_eq!(gate.process(&frame_at(-40.0), t0), GateDecision::Opened);

        // 7 dB down is under threshold - HYSTERESIS_DB: the hold runs from here.
        let quiet = frame_at(-52.0);
        assert_eq!(gate.process(&quiet, at(t0, 20)), GateDecision::Open);
        // 280 ms into the hold.
        assert_eq!(gate.process(&quiet, at(t0, 300)), GateDecision::Open);
        // 300 ms into the hold.
        assert_eq!(gate.process(&quiet, at(t0, 320)), GateDecision::Closed);
        assert!(!gate.is_open());

        assert_eq!(
            gate.process(&frame_at(-40.0), at(t0, 340)),
            GateDecision::Opened
        );
    }

    #[test]
    fn a_loud_frame_restarts_the_hold() {
        let t0 = Instant::now();
        let mut gate = NoiseGate::new(-45.0);
        assert_eq!(gate.process(&frame_at(-40.0), t0), GateDecision::Opened);

        let quiet = frame_at(-52.0);
        assert_eq!(gate.process(&quiet, at(t0, 20)), GateDecision::Open);
        assert_eq!(gate.process(&quiet, at(t0, 120)), GateDecision::Open);
        assert_eq!(
            gate.process(&frame_at(-40.0), at(t0, 200)),
            GateDecision::Open
        );

        // Without the restart the level would have been down since +20 ms and
        // this frame, 380 ms later, would have closed the gate.
        assert_eq!(gate.process(&quiet, at(t0, 220)), GateDecision::Open);
        assert_eq!(gate.process(&quiet, at(t0, 400)), GateDecision::Open);
        assert_eq!(gate.process(&quiet, at(t0, 520)), GateDecision::Closed);
    }
}
