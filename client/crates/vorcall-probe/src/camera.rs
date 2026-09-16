//! The camera halves of the probe: a synthetic camera and its watchers.
//!
//! A camera is a second video stream over the same voice session as a screen
//! share, so these sit beside the share's halves rather than replacing them:
//! one probe can run `--share-seconds` and `--camera-seconds` at once, which is
//! the oracle for two video streams on one session. Where a probe watches one
//! share, it watches up to four cameras, each with its own decode thread.

use std::collections::{HashMap, HashSet};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vorcall_screen::codec::{EncoderSettings, Usage, VideoEncoder};
use vorcall_screen::pattern::test_pattern;
use vorcall_screen::preset::FrameRate;
use vorcall_voice::{FrameSender, MediaEngine};

use crate::Roster;
use crate::share::{Decode, DecodeOutcome};

/// The most cameras the server lets one viewer watch.
pub const MAX_WATCHED: usize = 4;

pub struct CameraPlan {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub bitrate_kbps: u32,
    pub threads: u16,
    pub seconds: u64,
}

#[derive(Default)]
pub struct CameraOutcome {
    pub frames: u64,
    pub keyframes: u64,
    /// Access units the socket refused outright. The session-wide counter
    /// cannot say which stream gave up on one while a share runs beside the
    /// camera, so the camera counts its own.
    pub send_failures: u64,
}

/// The camera's worker thread: the encoder and
/// [`FrameSender::send_camera_video`] both block, exactly like the share's.
pub fn start_camera(sender: FrameSender, plan: CameraPlan) -> JoinHandle<CameraOutcome> {
    std::thread::Builder::new()
        .name("probe-camera".to_owned())
        .spawn(move || send_camera_video(sender, plan))
        .expect("spawning the camera thread")
}

fn send_camera_video(sender: FrameSender, plan: CameraPlan) -> CameraOutcome {
    let mut outcome = CameraOutcome::default();
    let mut encoder = match VideoEncoder::new(EncoderSettings {
        width: plan.width,
        height: plan.height,
        fps: plan.fps.hz(),
        bitrate_kbps: plan.bitrate_kbps,
        threads: plan.threads,
        usage: Usage::Camera,
    }) {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "no H.264 encoder; sending no camera video");
            return outcome;
        }
    };

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
        // The camera's own flag: a request for the share next door must not
        // cost this stream a keyframe, nor the other way round.
        let force = sender.take_camera_keyframe_request();
        match encoder.encode(&bgra, stride, force, &mut unit) {
            Ok(frame) if frame.skipped => {}
            Ok(frame) => {
                outcome.frames += 1;
                if frame.keyframe {
                    outcome.keyframes += 1;
                }
                if let Err(error) = sender.send_camera_video(index, frame.keyframe, &unit) {
                    outcome.send_failures += 1;
                    tracing::debug!(%error, "dropping a camera unit the socket refused");
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

    outcome
}

/// What one watched camera produced, as the probe reports it.
pub struct CameraResult {
    pub user: String,
    pub ssrc: u32,
    pub decoded: DecodeOutcome,
}

struct Watched {
    user: String,
    ssrc: u32,
    decode: Decode,
}

/// Drives the camera watches: follows who is on camera, asks for the named
/// ones, and keeps a decode thread per camera the server confirms.
pub struct CameraWatchPlan {
    pub channel_id: i64,
    /// The users `--watch-camera` named, in the order they were given.
    pub users: Vec<String>,
    /// Every one of them has to be confirmed by then, or the run has failed.
    pub confirm_by: Instant,
    requested: HashSet<i64>,
    watching: HashMap<i64, Watched>,
}

impl CameraWatchPlan {
    pub fn new(users: Vec<String>, channel_id: i64, confirm_by: Instant) -> Self {
        Self {
            channel_id,
            users,
            confirm_by,
            requested: HashSet::new(),
            watching: HashMap::new(),
        }
    }

    /// Whether every named camera is being decoded.
    pub fn confirmed(&self) -> bool {
        self.watching.len() == self.users.len()
    }

    /// The user ids to ask for now: named, known, on camera, not asked yet.
    pub fn pending_requests(&mut self, roster: &Roster) -> Vec<i64> {
        let mut pending = Vec::new();
        for user in &self.users {
            let Some(user_id) = roster.user_id_of(user) else {
                continue;
            };
            if roster.on_camera.contains(&user_id) && self.requested.insert(user_id) {
                pending.push(user_id);
            }
        }
        pending
    }

    /// The server named the cameras this client watches now: start decoding
    /// whichever of them is new.
    ///
    /// A camera that leaves the set keeps its decode thread. The set shrinks
    /// when the other side turns its camera off, which on a probe run is how
    /// the sharing half ends — throwing the pictures away then would turn a
    /// good run into a failed one.
    pub fn confirm(&mut self, engine: &MediaEngine, user_ids: &[i64], roster: &Roster) {
        for &user_id in user_ids {
            if self.watching.contains_key(&user_id) {
                continue;
            }
            let Some((&ssrc, (_, user))) = roster.names.iter().find(|(_, (id, _))| *id == user_id)
            else {
                tracing::warn!(user_id, "watching a camera with no known ssrc");
                continue;
            };
            let user = user.clone();
            let units = engine.watch_camera(ssrc);
            tracing::info!(user_id, ssrc, %user, "the server confirmed a camera watch");
            self.watching.insert(
                user_id,
                Watched {
                    user,
                    ssrc,
                    decode: Decode::start(units, "probe-camera-decode"),
                },
            );
        }
    }

    /// Stops every decode thread and reports what each camera produced, in the
    /// order `--watch-camera` named them.
    pub fn finish(self) -> Vec<CameraResult> {
        let CameraWatchPlan {
            users, watching, ..
        } = self;
        let mut results: Vec<CameraResult> = watching
            .into_values()
            .map(|watched| CameraResult {
                user: watched.user,
                ssrc: watched.ssrc,
                decoded: watched.decode.finish(),
            })
            .collect();
        results.sort_by_key(|result| {
            users
                .iter()
                .position(|user| *user == result.user)
                .unwrap_or(usize::MAX)
        });
        results
    }
}
