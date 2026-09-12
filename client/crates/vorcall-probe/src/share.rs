//! The screen-share halves of the probe: a synthetic sharer and a watcher.
//!
//! The sharer renders the test pattern, encodes it and puts it on the wire; the
//! watcher decodes whatever the relay hands back. Both run on worker threads,
//! because the encoder, the decoder and [`FrameSender::send_video`] all block.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use vorcall_core::Event;
use vorcall_screen::codec::{EncoderSettings, VideoDecoder, VideoEncoder};
use vorcall_screen::pattern::test_pattern;
use vorcall_screen::preset::{FrameRate, Preset, Resolution};
use vorcall_voice::codec::StereoEncoder;
use vorcall_voice::tone::Tone;
use vorcall_voice::{AccessUnit, FRAME_MS, FRAME_SAMPLES, FrameSender, STEREO_FRAME_SAMPLES};

/// The share tone: 660 Hz on the left, the same tone halved on the right. The
/// asymmetry is what [`is_share_tone`] recognizes on the other side.
const SHARE_TONE_HZ: f32 = 660.0;
const SHARE_TONE_AMPLITUDE: f32 = 0.3;
const RIGHT_FACTOR: f32 = 0.5;

/// How much louder the left channel has to be before a frame counts as the
/// share's own tone rather than a voice, which reaches both channels equally.
const ASYMMETRY: f32 = 1.25;

/// Opus at 96 kbit/s over 20 ms stereo frames stays far below this.
const MAX_SHARE_PACKET: usize = 1024;

/// How long the decode thread waits before looking for another access unit.
const DECODE_POLL: Duration = Duration::from_millis(2);

/// `WxH`, both even and non-zero — H.264 subsamples chroma by two.
pub fn parse_size(raw: &str) -> Result<(u32, u32), String> {
    let (width, height) = raw
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("--share-size wants WxH, got {raw}"))?;
    let width: u32 = width
        .parse()
        .map_err(|_| format!("--share-size wants WxH, got {raw}"))?;
    let height: u32 = height
        .parse()
        .map_err(|_| format!("--share-size wants WxH, got {raw}"))?;
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(format!(
            "--share-size wants even, non-zero dimensions, got {raw}"
        ));
    }
    Ok((width, height))
}

/// The bitrate the settings screen would pick for this picture and frame rate.
pub fn default_bitrate_kbps(size: (u32, u32), fps: FrameRate) -> u32 {
    Preset {
        resolution: Resolution::Source,
        fps,
        bitrate_kbps: None,
    }
    .bitrate_kbps(size)
}

/// Root mean square of each channel of one interleaved stereo frame.
fn channel_rms(frame: &[f32; STEREO_FRAME_SAMPLES]) -> (f32, f32) {
    let mut left = 0.0f64;
    let mut right = 0.0f64;
    for pair in frame.as_chunks::<2>().0 {
        left += f64::from(pair[0]) * f64::from(pair[0]);
        right += f64::from(pair[1]) * f64::from(pair[1]);
    }
    let samples = (STEREO_FRAME_SAMPLES / 2) as f64;
    (
        (left / samples).sqrt() as f32,
        (right / samples).sqrt() as f32,
    )
}

/// Whether this frame carries the share's own tone: loud enough to be more than
/// concealment, and lopsided, which no voice ever is.
pub fn is_share_tone(frame: &[f32; STEREO_FRAME_SAMPLES]) -> bool {
    let (left, right) = channel_rms(frame);
    left > crate::TONE_RMS && left >= right * ASYMMETRY
}

/// The mono mix of an interleaved stereo frame, so the existing level metrics
/// measure exactly what they measured before the playout went stereo.
pub fn downmix(frame: &[f32; STEREO_FRAME_SAMPLES], out: &mut [f32; FRAME_SAMPLES]) {
    for (mono, pair) in out.iter_mut().zip(frame.as_chunks::<2>().0) {
        *mono = (pair[0] + pair[1]) / 2.0;
    }
}

pub struct SharePlan {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub bitrate_kbps: u32,
    pub threads: u16,
    pub seconds: u64,
    pub audio: bool,
}

#[derive(Default)]
pub struct ShareOutcome {
    pub frames_encoded: u64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub threads: u16,
    /// Datagrams this session's socket gave up on, retries included.
    pub send_failures: u64,
    pub elapsed: Duration,
}

impl ShareOutcome {
    pub fn encode_fps(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            self.frames_encoded as f64 / seconds
        } else {
            0.0
        }
    }

    pub fn kbps(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            self.bytes as f64 * 8.0 / 1000.0 / seconds
        } else {
            0.0
        }
    }
}

/// The sharer's worker threads: one encodes and sends the picture, the other,
/// under `--share-audio`, keeps the stereo tone going on its own 20 ms clock.
pub struct ShareHandles {
    video: JoinHandle<ShareOutcome>,
    audio: Option<JoinHandle<()>>,
}

impl ShareHandles {
    pub fn join(self) -> ShareOutcome {
        let outcome = self.video.join().unwrap_or_default();
        if let Some(audio) = self.audio {
            let _ = audio.join();
        }
        outcome
    }
}

pub fn start_share(sender: FrameSender, plan: SharePlan) -> ShareHandles {
    let audio = plan.audio.then(|| {
        let sender = sender.clone();
        let seconds = plan.seconds;
        std::thread::Builder::new()
            .name("probe-share-audio".to_owned())
            .spawn(move || send_share_tone(sender, seconds))
            .expect("spawning the share audio thread")
    });
    let video = std::thread::Builder::new()
        .name("probe-share".to_owned())
        .spawn(move || send_share_video(sender, plan))
        .expect("spawning the share thread");

    ShareHandles { video, audio }
}

fn send_share_video(sender: FrameSender, plan: SharePlan) -> ShareOutcome {
    let mut outcome = ShareOutcome::default();
    let mut encoder = match VideoEncoder::new(EncoderSettings {
        width: plan.width,
        height: plan.height,
        fps: plan.fps.hz(),
        bitrate_kbps: plan.bitrate_kbps,
        threads: plan.threads,
    }) {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "no H.264 encoder; sharing nothing");
            return outcome;
        }
    };
    outcome.threads = encoder.threads();

    let stride = plan.width as usize * 4;
    let interval = plan.fps.interval();
    let started = Instant::now();
    let stop = started + Duration::from_secs(plan.seconds);
    let mut next = started;
    let mut bgra = Vec::new();
    let mut unit = Vec::new();
    let mut index = 0u32;

    while Instant::now() < stop {
        test_pattern(plan.width, plan.height, index, &mut bgra);
        let force = sender.take_keyframe_request();
        if force {
            outcome.keyframe_requests += 1;
        }
        match encoder.encode(&bgra, stride, force, &mut unit) {
            Ok(frame) if frame.skipped => outcome.skipped += 1,
            Ok(frame) => {
                outcome.frames_encoded += 1;
                outcome.bytes += unit.len() as u64;
                if frame.keyframe {
                    outcome.keyframes += 1;
                }
                if let Err(error) = sender.send_video(index, frame.keyframe, &unit) {
                    tracing::debug!(%error, "dropping an access unit the socket refused");
                }
            }
            Err(error) => tracing::debug!(%error, "dropping a frame the encoder refused"),
        }

        index = index.wrapping_add(1);
        next += interval;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }

    outcome.send_failures = sender.send_failures();
    outcome.elapsed = started.elapsed();
    outcome
}

fn send_share_tone(sender: FrameSender, seconds: u64) {
    let mut encoder = match StereoEncoder::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "no stereo Opus encoder; sending no share audio");
            return;
        }
    };
    let mut tone = Tone::new(SHARE_TONE_HZ, SHARE_TONE_AMPLITUDE);
    let mut mono = [0.0f32; FRAME_SAMPLES];
    let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
    let mut packet = [0u8; MAX_SHARE_PACKET];
    let interval = Duration::from_millis(FRAME_MS);
    let mut next = Instant::now();

    for index in 0..seconds * 1000 / FRAME_MS {
        tone.fill(&mut mono);
        for (pair, sample) in pcm.as_chunks_mut::<2>().0.iter_mut().zip(mono.iter()) {
            pair[0] = *sample;
            pair[1] = *sample * RIGHT_FACTOR;
        }
        match encoder.encode(&pcm, &mut packet) {
            // A share's talk spurt starts with its first packet and runs on.
            Ok(written) => {
                if let Err(error) = sender.send_share_audio(&packet[..written], index == 0) {
                    tracing::debug!(%error, "dropping share audio the socket refused");
                }
            }
            Err(error) => tracing::debug!(%error, "dropping share audio the encoder refused"),
        }

        next += interval;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
}

#[derive(Default)]
pub struct DecodeOutcome {
    pub pictures: u64,
    pub keyframes: u64,
    pub decode_errors: u64,
    pub first_picture_ms: Option<f64>,
    pub width: u32,
    pub height: u32,
    elapsed: Duration,
}

impl DecodeOutcome {
    pub fn decode_fps(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            self.pictures as f64 / seconds
        } else {
            0.0
        }
    }
}

struct Decode {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<DecodeOutcome>,
}

/// Drives the watcher: finds the sharer, asks for their stream, and keeps the
/// decode thread once the server confirms the watch.
pub struct WatchPlan {
    pub user: String,
    pub channel_id: i64,
    /// The watch has to be confirmed by then, or the run has failed.
    pub confirm_by: Instant,
    user_id: Option<i64>,
    sharing: HashSet<i64>,
    requested: bool,
    confirmed: bool,
    decode: Option<Decode>,
}

impl WatchPlan {
    pub fn new(user: String, channel_id: i64, confirm_by: Instant) -> Self {
        Self {
            user,
            channel_id,
            confirm_by,
            user_id: None,
            sharing: HashSet::new(),
            requested: false,
            confirmed: false,
            decode: None,
        }
    }

    pub fn user_id(&self) -> Option<i64> {
        self.user_id
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// Follows who is sharing, from the roster and from the share events.
    pub fn note(&mut self, event: &Event) {
        match event {
            Event::VoiceState { members, .. } => {
                for member in members {
                    if member.sharing {
                        self.sharing.insert(member.user_id);
                    } else {
                        self.sharing.remove(&member.user_id);
                    }
                }
            }
            Event::VoiceMemberJoined { member, .. } if member.sharing => {
                self.sharing.insert(member.user_id);
            }
            Event::ShareStarted { user_id, .. } => {
                self.sharing.insert(*user_id);
            }
            Event::ShareStopped { user_id, .. } | Event::VoiceMemberLeft { user_id, .. } => {
                self.sharing.remove(user_id);
            }
            _ => {}
        }
    }

    /// The user id to watch, once the target is both known and sharing and no
    /// watch has been asked for yet.
    pub fn pending_request(&mut self, names: &HashMap<u32, (i64, String)>) -> Option<i64> {
        if self.requested {
            return None;
        }
        let user_id = names
            .values()
            .find(|(_, username)| *username == self.user)
            .map(|(user_id, _)| *user_id)?;
        if !self.sharing.contains(&user_id) {
            return None;
        }
        self.requested = true;
        self.user_id = Some(user_id);
        Some(user_id)
    }

    /// The server confirmed the watch: start decoding whatever arrives.
    pub fn confirm(&mut self, user_id: i64, units: Option<UnboundedReceiver<AccessUnit>>) {
        self.confirmed = true;
        self.user_id = Some(user_id);
        let Some(units) = units else {
            tracing::warn!("the access units were taken already; decoding nothing");
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("probe-decode".to_owned())
            .spawn(move || decode_units(units, flag))
            .expect("spawning the decode thread");
        self.decode = Some(Decode { stop, handle });
    }

    pub fn finish(self) -> DecodeOutcome {
        match self.decode {
            Some(decode) => {
                decode.stop.store(true, Ordering::Relaxed);
                decode.handle.join().unwrap_or_default()
            }
            None => DecodeOutcome::default(),
        }
    }
}

fn decode_units(mut units: UnboundedReceiver<AccessUnit>, stop: Arc<AtomicBool>) -> DecodeOutcome {
    let started = Instant::now();
    let mut outcome = DecodeOutcome::default();
    let mut decoder = match VideoDecoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::error!(%error, "no H.264 decoder; decoding nothing");
            return outcome;
        }
    };

    while !stop.load(Ordering::Relaxed) {
        match units.try_recv() {
            Ok(unit) => {
                if unit.keyframe {
                    outcome.keyframes += 1;
                }
                match decoder.decode(&unit.data) {
                    Ok(Some(picture)) => {
                        outcome.pictures += 1;
                        outcome
                            .first_picture_ms
                            .get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
                        outcome.width = picture.width;
                        outcome.height = picture.height;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        outcome.decode_errors += 1;
                        tracing::debug!(%error, "dropping an undecodable access unit");
                    }
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(DECODE_POLL),
            Err(TryRecvError::Disconnected) => break,
        }
    }

    outcome.elapsed = started.elapsed();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_size_parses_and_rejects_odd_values() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert_eq!(parse_size("640X480"), Ok((640, 480)));

        for bad in ["1281x720", "1280x721", "0x720", "1280x0", "1280", "axb", ""] {
            assert!(parse_size(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn an_asymmetric_frame_counts_as_share_tone_and_a_centred_one_does_not() {
        let mut share = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut voice = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut silence = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut mono = [0.0f32; FRAME_SAMPLES];
        Tone::new(SHARE_TONE_HZ, SHARE_TONE_AMPLITUDE).fill(&mut mono);

        for (index, sample) in mono.iter().enumerate() {
            share[2 * index] = *sample;
            share[2 * index + 1] = *sample * RIGHT_FACTOR;
            voice[2 * index] = *sample;
            voice[2 * index + 1] = *sample;
            silence[2 * index] = *sample * 0.001;
            silence[2 * index + 1] = *sample * 0.0005;
        }

        assert!(is_share_tone(&share));
        assert!(
            !is_share_tone(&voice),
            "a centred voice is not a share tone"
        );
        assert!(!is_share_tone(&silence), "near-silence is not a share tone");

        // The mono downmix of the share frame still measures as audio.
        let mut downmixed = [0.0f32; FRAME_SAMPLES];
        downmix(&share, &mut downmixed);
        assert!(vorcall_voice::tone::rms(&downmixed) > crate::TONE_RMS);
    }
}
