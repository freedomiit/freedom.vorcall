//! The capture-side cleanup chain: echo cancellation, noise suppression and
//! automatic gain.
//!
//! High-pass filter → AEC3 echo canceller → noise suppressor → AGC2, which is
//! WebRTC's order and the one the `aec3` crate's linear pipeline wires for us.
//! Every stage works on 10 ms blocks, so one 20 ms frame is two of them.
//!
//! The canceller's job is to subtract what the machine is playing from what the
//! microphone hears, so it has to be handed that far end — whatever the local
//! mixer sent to the speakers — through [`InputCleanup::push_far_end`]. It works
//! out the delay between the two itself; the estimate it settles on is readable
//! through [`InputCleanup::metrics`].
//!
//! AEC3 stays in the graph whether or not echo cancellation is wanted: with it
//! off the canceller is simply fed silence, so the chain has one shape instead
//! of two. Unlike [`crate::codec`], nothing here catches panics — a cleanup
//! failure surfaces as an error and the caller decides whether to drop the
//! chain and capture raw.

use std::collections::VecDeque;

use aec3::graph::GraphError;
use aec3::nodes::audio::AudioFormat;
use aec3::pipelines::linear;

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

/// 10 ms at 48 kHz, mono: the block the chain actually processes.
pub const BLOCK_SAMPLES: usize = 480;
/// 200 ms of far end. A caller that stops pushing capture frames must not make
/// the reference queue grow without bound, so anything older is dropped.
pub const FAR_END_MAX_SAMPLES: usize = SAMPLE_RATE as usize / 5;

const _: () = assert!(FRAME_SAMPLES == 2 * BLOCK_SAMPLES);

/// Fed to the canceller in place of the far end when echo cancellation is off,
/// keeping the always-present AEC3 running at the capture cadence.
const SILENCE_BLOCK: [f32; BLOCK_SAMPLES] = [0.0; BLOCK_SAMPLES];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupSettings {
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub auto_gain: bool,
}

impl Default for CleanupSettings {
    fn default() -> Self {
        Self {
            noise_suppression: true,
            echo_cancellation: true,
            auto_gain: false,
        }
    }
}

impl CleanupSettings {
    /// Whether any stage is on; the caller does not build a chain otherwise.
    pub fn any(self) -> bool {
        self.noise_suppression || self.echo_cancellation || self.auto_gain
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("cannot build the input cleanup chain: {0}")]
    Build(String),
    #[error("the input cleanup chain refused a frame: {0}")]
    Process(String),
}

/// What the canceller reports about the echo path it has found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EchoMetrics {
    pub delay_ms: i32,
    pub echo_return_loss_db: f64,
    pub echo_return_loss_enhancement_db: f64,
}

pub struct InputCleanup {
    pipeline: linear::LinearPipeline,
    settings: CleanupSettings,
    far_end: VecDeque<f32>,
    block: [f32; BLOCK_SAMPLES],
    out: [f32; BLOCK_SAMPLES],
    last_metrics: Option<EchoMetrics>,
    passed_through: u64,
}

impl InputCleanup {
    pub fn new(settings: CleanupSettings) -> Result<Self, CleanupError> {
        let format = AudioFormat::ten_ms(SAMPLE_RATE, 1);
        let pipeline = linear::builder(format, format)
            .enable_high_pass_filter(true)
            .enable_noise_suppression(settings.noise_suppression)
            .enable_gain_controller2(settings.auto_gain)
            .export_metrics(settings.echo_cancellation)
            .build()
            .map_err(|error| CleanupError::Build(error.to_string()))?;

        Ok(Self {
            pipeline,
            settings,
            far_end: VecDeque::with_capacity(FAR_END_MAX_SAMPLES),
            block: [0.0; BLOCK_SAMPLES],
            out: [0.0; BLOCK_SAMPLES],
            last_metrics: None,
            passed_through: 0,
        })
    }

    /// Appends far-end (played-back) samples for the echo canceller. A no-op
    /// unless echo cancellation is on. Keeps only the newest
    /// [`FAR_END_MAX_SAMPLES`], dropping the oldest.
    pub fn push_far_end(&mut self, samples: &[f32]) {
        if !self.settings.echo_cancellation {
            return;
        }

        // Trimmed before the push rather than after, so the queue never grows
        // past the capacity reserved at construction.
        let keep = samples.len().min(FAR_END_MAX_SAMPLES);
        let excess = (self.far_end.len() + keep).saturating_sub(FAR_END_MAX_SAMPLES);
        self.far_end.drain(..excess);
        self.far_end.extend(&samples[samples.len() - keep..]);
    }

    pub fn pending_far_end(&self) -> usize {
        self.far_end.len()
    }

    /// The oldest pending far-end sample; tests use it to prove which end of
    /// the reference the bound drops.
    #[cfg(test)]
    fn far_end_head(&self) -> Option<f32> {
        self.far_end.front().copied()
    }

    /// Cleans one 20 ms frame in place. On `Err` the frame's contents are
    /// unspecified and the caller drops the chain.
    pub fn process(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<(), CleanupError> {
        if self.settings.echo_cancellation {
            // Everything pending goes in before the capture: the pipeline
            // expects render and capture to arrive asynchronously and aligns
            // them itself.
            while self.far_end.len() >= BLOCK_SAMPLES {
                let render = self.far_end.drain(..BLOCK_SAMPLES);
                for (slot, sample) in self.block.iter_mut().zip(render) {
                    *slot = sample;
                }
                let fed = self.pipeline.handle_render_frame(&self.block);
                fed.map_err(process_error)?;
            }
        }

        let (halves, _) = frame.as_chunks_mut::<BLOCK_SAMPLES>();
        for half in halves {
            if !self.settings.echo_cancellation {
                let fed = self.pipeline.handle_render_frame(&SILENCE_BLOCK);
                fed.map_err(process_error)?;
            }

            let produced = self.pipeline.process_capture_frame(half, &mut self.out);
            if produced.map_err(process_error)? {
                half.copy_from_slice(&self.out);
            } else {
                // The pipeline had nothing ready and filled `out` with silence;
                // the captured half is better than that, so it goes out raw.
                self.passed_through += 1;
            }

            // The sink keeps only the latest packet, so one pull per block is
            // the whole drain.
            if let Some(packet) = self.pipeline.try_pull_metrics().map_err(process_error)? {
                let metrics = packet.payload();
                self.last_metrics = Some(EchoMetrics {
                    delay_ms: metrics.delay_ms,
                    echo_return_loss_db: metrics.echo_return_loss,
                    echo_return_loss_enhancement_db: metrics.echo_return_loss_enhancement,
                });
            }
        }

        Ok(())
    }

    /// The newest metrics the canceller emitted; `None` until it has, or when
    /// echo cancellation is off.
    pub fn metrics(&self) -> Option<EchoMetrics> {
        self.last_metrics
    }

    /// Blocks the pipeline produced no output for, left as captured.
    pub fn passed_through(&self) -> u64 {
        self.passed_through
    }
}

fn process_error(error: GraphError) -> CleanupError {
    CleanupError::Process(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;
    use std::sync::OnceLock;

    use super::*;
    use crate::FRAME_MS;
    use crate::gate::{SILENCE_DB, dbfs};
    use crate::tone::{Tone, rms};

    /// 20 ms frames, so a second is 50 of them.
    const FPS: usize = 1000 / FRAME_MS as usize;

    /// White noise from a fixed linear congruential generator: the same samples
    /// every run, so a threshold that holds once holds always. Uniform in
    /// [-1, 1), which puts its RMS at 1/sqrt(3) of the amplitude it is scaled by.
    struct Lcg(u32);

    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            ((self.0 >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// A speech-like source: a ~130 Hz glottal pulse train with a little
    /// vibrato, shaped by a syllable envelope and broken by a word gap. AGC2's
    /// VAD only lifts what it takes for speech, and a pure tone does not
    /// qualify, so the gain tests need this rather than a [`Tone`].
    struct Voice {
        t: f64,
        scale: f32,
    }

    impl Voice {
        /// Calibrated so the RMS over its non-silent samples is `target_dbfs`.
        fn new(target_dbfs: f32) -> Self {
            let mut probe = Self { t: 0.0, scale: 1.0 };
            let voiced: Vec<f32> = (0..SAMPLE_RATE as usize)
                .map(|_| probe.next())
                .filter(|sample| sample.abs() > 1e-4)
                .collect();
            Self {
                t: 0.0,
                scale: 10f32.powf(target_dbfs / 20.0) / rms(&voiced),
            }
        }

        fn next(&mut self) -> f32 {
            let t = self.t;
            self.t += 1.0 / f64::from(SAMPLE_RATE);

            if t % 2.0 > 1.6 {
                return 0.0;
            }

            let f0 = 130.0 * (1.0 + 0.03 * (TAU * 5.0 * t).sin());
            let mut harmonics = 0.0f64;
            for k in 1..=20 {
                harmonics += (TAU * f0 * f64::from(k) * t).sin() / f64::from(k);
            }
            let envelope = (((TAU * 3.0 * t).sin() + 1.0) / 2.0).powf(1.5);
            (harmonics * envelope) as f32 * self.scale
        }
    }

    fn frame_of(mut source: impl FnMut() -> f32) -> [f32; FRAME_SAMPLES] {
        let mut frame = [0.0f32; FRAME_SAMPLES];
        for sample in frame.iter_mut() {
            *sample = source();
        }
        frame
    }

    fn level(frame: &[f32; FRAME_SAMPLES]) -> f32 {
        dbfs(rms(frame))
    }

    fn echo_only() -> CleanupSettings {
        CleanupSettings {
            noise_suppression: false,
            echo_cancellation: true,
            auto_gain: false,
        }
    }

    fn auto_gain_only() -> CleanupSettings {
        CleanupSettings {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: true,
        }
    }

    /// One frame's worth of measurements. `near_db` is [`SILENCE_DB`] on the
    /// frames where nothing was talking into the microphone.
    struct Row {
        frame: usize,
        input_db: f32,
        output_db: f32,
        near_db: f32,
    }

    /// Mean of `pick` over the rows in `[lo, hi)`, skipping the ones it rejects.
    fn avg_db(rows: &[Row], lo: usize, hi: usize, pick: impl Fn(&Row) -> Option<f32>) -> f32 {
        let window = &rows[lo..hi];
        let picked: Vec<f32> = window.iter().filter_map(&pick).collect();
        assert!(
            !picked.is_empty(),
            "no rows matched over frames {}..{}",
            window.first().map_or(0, |row| row.frame),
            window.last().map_or(0, |row| row.frame),
        );
        picked.iter().sum::<f32>() / picked.len() as f32
    }

    struct EchoRun {
        rows: Vec<Row>,
        metrics: Option<EchoMetrics>,
        passed_through: u64,
    }

    /// Plays bursts of noise into a room and hands the chain both the far end it
    /// played and the microphone signal that came back `delay_ms` later.
    fn echo_run(
        delay_ms: usize,
        settings: CleanupSettings,
        seconds: usize,
        mut near: impl FnMut(usize) -> Option<[f32; FRAME_SAMPLES]>,
    ) -> EchoRun {
        let mut cleanup = InputCleanup::new(settings).expect("the chain builds");
        let mut noise = Lcg(7);
        let mut path: VecDeque<f32> = vec![0.0; delay_ms * SAMPLE_RATE as usize / 1000].into();
        let frames = seconds * FPS;
        let mut rows = Vec::with_capacity(frames);

        for index in 0..frames {
            // 300 ms of far end then 100 ms of quiet: the canceller has to hold
            // its estimate across the gaps rather than relearn it each burst.
            let mut far = [0.0f32; FRAME_SAMPLES];
            if index % 20 < 15 {
                for sample in far.iter_mut() {
                    *sample = noise.next() * 0.3;
                }
            }

            let near_frame = near(index);
            let mut mic = [0.0f32; FRAME_SAMPLES];
            for (slot, played) in mic.iter_mut().zip(far.iter()) {
                path.push_back(*played);
                *slot = path.pop_front().unwrap_or(0.0) * 0.5 + noise.next() * 0.001;
            }
            if let Some(near_frame) = &near_frame {
                for (slot, spoken) in mic.iter_mut().zip(near_frame.iter()) {
                    *slot += spoken;
                }
            }

            let input_db = level(&mic);
            cleanup.push_far_end(&far);
            cleanup
                .process(&mut mic)
                .expect("the chain accepts a frame");
            rows.push(Row {
                frame: index,
                input_db,
                output_db: level(&mic),
                near_db: near_frame.map_or(SILENCE_DB, |frame| level(&frame)),
            });
        }

        EchoRun {
            rows,
            metrics: cleanup.metrics(),
            passed_through: cleanup.passed_through(),
        }
    }

    /// Three tests read one 6 s run of the 100 ms echo path; sharing it keeps
    /// the suite's synthetic audio down.
    fn converged_echo_run() -> &'static EchoRun {
        static RUN: OnceLock<EchoRun> = OnceLock::new();
        RUN.get_or_init(|| echo_run(100, echo_only(), 6, |_| None))
    }

    struct DryRun {
        rows: Vec<Row>,
        max_pending: usize,
    }

    /// Runs generated capture through the chain with nothing playing back. Every
    /// dry scenario has echo cancellation off, so the far end pushed on each
    /// iteration must be discarded outright: `max_pending` is the proof.
    fn dry_run(
        settings: CleanupSettings,
        seconds: usize,
        mut source: impl FnMut(usize) -> ([f32; FRAME_SAMPLES], f32),
    ) -> DryRun {
        let mut cleanup = InputCleanup::new(settings).expect("the chain builds");
        let far = [0.25f32; FRAME_SAMPLES];
        let frames = seconds * FPS;
        let mut rows = Vec::with_capacity(frames);
        let mut max_pending = 0;

        for index in 0..frames {
            let (mut mic, near_db) = source(index);
            let input_db = level(&mic);
            cleanup.push_far_end(&far);
            max_pending = max_pending.max(cleanup.pending_far_end());
            cleanup
                .process(&mut mic)
                .expect("the chain accepts a frame");
            rows.push(Row {
                frame: index,
                input_db,
                output_db: level(&mic),
                near_db,
            });
        }

        DryRun { rows, max_pending }
    }

    #[test]
    fn defaults_clean_noise_and_echo_but_leave_the_gain_alone() {
        let defaults = CleanupSettings::default();
        assert!(defaults.noise_suppression);
        assert!(defaults.echo_cancellation);
        assert!(!defaults.auto_gain);
        assert!(defaults.any());

        let off = CleanupSettings {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        };
        assert!(!off.any());
    }

    #[test]
    fn the_crate_block_is_ten_ms_at_48_khz() {
        assert_eq!(
            AudioFormat::ten_ms(SAMPLE_RATE, 1).sample_count(),
            BLOCK_SAMPLES
        );
    }

    #[test]
    fn echo_of_the_far_end_is_cancelled() {
        let run = converged_echo_run();
        let cancelled = avg_db(&run.rows, 3 * FPS, 6 * FPS, |row| {
            Some(row.input_db - row.output_db)
        });
        assert!(cancelled >= 12.0, "{cancelled}");
    }

    #[test]
    fn a_long_echo_path_is_still_found() {
        let run = echo_run(250, echo_only(), 6, |_| None);
        let cancelled = avg_db(&run.rows, 3 * FPS, 6 * FPS, |row| {
            Some(row.input_db - row.output_db)
        });
        assert!(cancelled >= 12.0, "{cancelled}");
    }

    #[test]
    fn metrics_report_the_delay_once_converged() {
        let metrics = converged_echo_run()
            .metrics
            .expect("the canceller reported metrics");
        assert!((metrics.delay_ms - 100).abs() <= 30, "{}", metrics.delay_ms);
        assert!(
            metrics.echo_return_loss_enhancement_db >= 10.0,
            "{}",
            metrics.echo_return_loss_enhancement_db
        );
    }

    #[test]
    fn near_end_speech_survives_double_talk() {
        let mut tone = Tone::new(440.0, 0.2);
        let run = echo_run(100, echo_only(), 6, |index| {
            if index < 3 * FPS {
                return None;
            }
            let mut frame = [0.0f32; FRAME_SAMPLES];
            tone.fill(&mut frame);
            Some(frame)
        });

        // The suppressor releases over the first second of double talk, so this
        // window sits ~2 dB clear of the floor while the noise realisation moves
        // it by ~1 dB: retune the scenario rather than the seed if it ever trips.
        let (lo, hi) = (7 * FPS / 2, 6 * FPS);
        let out = avg_db(&run.rows, lo, hi, |row| Some(row.output_db));
        let near = avg_db(&run.rows, lo, hi, |row| Some(row.near_db));
        let mic = avg_db(&run.rows, lo, hi, |row| Some(row.input_db));
        assert!(out >= near - 4.0, "out {out}, near {near}");
        assert!(out <= mic + 1.0, "out {out}, mic {mic}");
    }

    #[test]
    fn stationary_noise_is_suppressed_and_speech_kept() {
        let settings = CleanupSettings {
            noise_suppression: true,
            echo_cancellation: false,
            auto_gain: false,
        };
        let mut noise = Lcg(0x1234);
        let mut voice = Voice::new(-18.5);
        let run = dry_run(settings, 6, |index| {
            let near = if index >= 3 * FPS {
                frame_of(|| voice.next())
            } else {
                [0.0f32; FRAME_SAMPLES]
            };
            let mut mic = [0.0f32; FRAME_SAMPLES];
            for (slot, spoken) in mic.iter_mut().zip(near.iter()) {
                *slot = noise.next() * 0.0173 + spoken;
            }
            (mic, level(&near))
        });

        assert_eq!(run.max_pending, 0, "{}", run.max_pending);

        let suppressed = avg_db(&run.rows, FPS, 3 * FPS, |row| {
            Some(row.input_db - row.output_db)
        });
        assert!(suppressed >= 6.0, "{suppressed}");

        let kept = avg_db(&run.rows, 7 * FPS / 2, 6 * FPS, |row| {
            (row.near_db > SILENCE_DB).then_some(row.output_db - row.input_db)
        })
        .abs();
        assert!(kept <= 3.0, "{kept}");
    }

    #[test]
    fn auto_gain_lifts_a_quiet_talker() {
        let mut voice = Voice::new(-45.0);
        let run = dry_run(auto_gain_only(), 6, |_| {
            let frame = frame_of(|| voice.next());
            let db = level(&frame);
            (frame, db)
        });

        let lift = avg_db(&run.rows, 3 * FPS, 6 * FPS, |row| {
            (row.near_db > SILENCE_DB).then_some(row.output_db - row.input_db)
        });
        assert!((8.0..=30.0).contains(&lift), "{lift}");
    }

    #[test]
    fn auto_gain_leaves_a_loud_talker_alone() {
        let mut voice = Voice::new(-25.0);
        let run = dry_run(auto_gain_only(), 6, |_| {
            let frame = frame_of(|| voice.next());
            let db = level(&frame);
            (frame, db)
        });

        let lift = avg_db(&run.rows, 3 * FPS, 6 * FPS, |row| {
            (row.near_db > SILENCE_DB).then_some(row.output_db - row.input_db)
        });
        assert!(lift <= 4.0, "{lift}");
    }

    #[test]
    fn with_every_stage_off_speech_passes_almost_untouched() {
        let settings = CleanupSettings {
            noise_suppression: false,
            echo_cancellation: false,
            auto_gain: false,
        };
        let mut voice = Voice::new(-18.5);
        let run = dry_run(settings, 4, |_| {
            let frame = frame_of(|| voice.next());
            let db = level(&frame);
            (frame, db)
        });

        let drift = avg_db(&run.rows, FPS, 4 * FPS, |row| {
            (row.near_db > SILENCE_DB).then_some(row.output_db - row.input_db)
        })
        .abs();
        assert!(drift <= 2.5, "{drift}");
    }

    #[test]
    fn the_far_end_keeps_only_the_newest_200_ms() {
        let mut cleanup = InputCleanup::new(CleanupSettings::default()).expect("the chain builds");

        // 300 ms of a ramp, pushed a frame at a time.
        let mut ramp = 0.0f32;
        for _ in 0..15 {
            let frame = frame_of(|| {
                ramp += 1.0;
                ramp
            });
            cleanup.push_far_end(&frame);
        }
        assert_eq!(cleanup.pending_far_end(), FAR_END_MAX_SAMPLES);
        // The oldest survivor is the sample right after the dropped prefix.
        let expected_head = (15 * FRAME_SAMPLES - FAR_END_MAX_SAMPLES + 1) as f32;
        assert_eq!(cleanup.far_end_head(), Some(expected_head));

        let mut fresh = InputCleanup::new(CleanupSettings::default()).expect("the chain builds");
        let oversized: Vec<f32> = (1..=(FAR_END_MAX_SAMPLES + 2_400)).map(|n| n as f32).collect();
        fresh.push_far_end(&oversized);
        assert_eq!(fresh.pending_far_end(), FAR_END_MAX_SAMPLES);
        assert_eq!(fresh.far_end_head(), Some(2_401.0));

        let mut silence = [0.0f32; FRAME_SAMPLES];
        cleanup
            .process(&mut silence)
            .expect("the chain accepts a frame");
        assert!(
            cleanup.pending_far_end() < BLOCK_SAMPLES,
            "{}",
            cleanup.pending_far_end()
        );
    }

    #[test]
    fn no_block_was_passed_through_in_lockstep() {
        let passed_through = converged_echo_run().passed_through;
        assert_eq!(passed_through, 0, "{passed_through}");
    }
}
