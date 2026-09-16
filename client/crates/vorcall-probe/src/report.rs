//! The single JSON line the probe prints, formatted by hand: the probe carries
//! no serialization dependency.

use vorcall_voice::FRAME_MS;

pub struct PeerReport {
    pub user_id: i64,
    pub username: String,
    pub ssrc: u32,
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub decoded_frames: u64,
    pub decoder_resets: u32,
}

pub struct SpeakingEvent {
    pub user_id: i64,
    pub speaking: bool,
}

/// Only under `--share-seconds`: what the local encoder produced and sent.
pub struct ShareReport {
    pub frames_encoded: u64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    pub encode_fps: f64,
    pub kbps: f64,
    pub watchers_max: u32,
    pub bytes: u64,
    pub threads: u16,
    pub skipped: u64,
    /// Datagrams the socket gave up on, retries included; the sharer's half of
    /// the top-level `send_failures`, read when the share thread finished.
    pub send_failures: u64,
}

/// Only under `--watch`: what came back from the sharer.
pub struct WatchReport {
    pub user: String,
    pub user_id: i64,
    pub pictures: u64,
    pub keyframes: u64,
    pub dropped: u64,
    pub decode_errors: u64,
    pub first_picture_ms: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub decode_fps: f64,
    pub keyframe_requests_sent: u64,
    pub share_tone_frames: u64,
    /// What the share's audio cost the watcher's jitter buffer: frames played
    /// from a packet, frames the decoder concealed, frames the timestamps prove
    /// never arrived, and packets that came too late to play.
    pub audio_played: u64,
    pub audio_concealed: u64,
    pub audio_lost: u64,
    pub audio_late: u64,
}

/// Only under `--camera-seconds`: what the local camera encoder produced and
/// sent, mirroring [`ShareReport`] for the second video stream.
pub struct CameraReport {
    pub frames: u64,
    pub keyframes: u64,
    /// Access units this camera's own sends gave up on; the session-wide
    /// `send_failures` cannot tell the two streams apart.
    pub send_failures: u64,
}

/// One entry per `--watch-camera`: what came back from that camera.
pub struct CameraWatchReport {
    pub user: String,
    pub ssrc: u32,
    pub pictures: u64,
    pub keyframes: u64,
    pub dropped: u64,
}

#[derive(Default)]
pub struct Rtt {
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
    pub last: Option<f64>,
    pub samples: u32,
}

pub struct Report {
    pub user: String,
    pub user_id: i64,
    pub channel_id: i64,
    pub channel_name: String,
    pub ssrc: u32,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rejected: u64,
    pub send_failures: u64,
    /// Audio frames handed to the socket; `packets_sent` also counts keepalive
    /// pings, which is why this exists.
    pub frames_sent: u64,
    pub frames_gated: u64,
    pub decoded_frames: u64,
    pub tone_frames: u64,
    pub rtt: Rtt,
    pub link: &'static str,
    pub peers: Vec<PeerReport>,
    pub speaking_events: Vec<SpeakingEvent>,
    pub share: Option<ShareReport>,
    pub watch: Option<WatchReport>,
    pub camera: Option<CameraReport>,
    pub cameras: Vec<CameraWatchReport>,
}

impl Report {
    pub fn tone_seconds(&self) -> f64 {
        seconds(self.tone_frames)
    }

    pub fn share_tone_seconds(&self) -> f64 {
        self.watch
            .as_ref()
            .map_or(0.0, |watch| seconds(watch.share_tone_frames))
    }

    /// How much of the watched share's audio was concealment rather than sound
    /// that arrived, in per cent. `0` when nothing was watched.
    pub fn share_concealed_pct(&self) -> f64 {
        self.watch.as_ref().map_or(0.0, |watch| {
            let heard = watch.audio_played + watch.audio_concealed;
            if heard == 0 {
                return 0.0;
            }
            watch.audio_concealed as f64 * 100.0 / heard as f64
        })
    }

    pub fn pictures(&self) -> u64 {
        self.watch.as_ref().map_or(0, |watch| watch.pictures)
    }

    /// The fewest pictures any watched camera decoded, so `--expect-camera-video`
    /// judges every camera rather than their total. No camera watched is 0,
    /// which fails that check the same way an empty stream would.
    pub fn min_camera_pictures(&self) -> u64 {
        self.cameras
            .iter()
            .map(|camera| camera.pictures)
            .min()
            .unwrap_or(0)
    }

    pub fn render(&self) -> String {
        let gaps: u64 = self.peers.iter().map(|peer| peer.lost).sum();
        let late: u64 = self.peers.iter().map(|peer| peer.late).sum();

        let peers = self
            .peers
            .iter()
            .map(|peer| {
                format!(
                    "{{\"user_id\":{},\"username\":{},\"ssrc\":{},\"received\":{},\"lost\":{},\"late\":{},\"decoded_frames\":{},\"decoder_resets\":{}}}",
                    peer.user_id,
                    quote(&peer.username),
                    peer.ssrc,
                    peer.received,
                    peer.lost,
                    peer.late,
                    peer.decoded_frames,
                    peer.decoder_resets,
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let speaking = self
            .speaking_events
            .iter()
            .map(|event| {
                format!(
                    "{{\"user_id\":{},\"speaking\":{}}}",
                    event.user_id, event.speaking
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let share = self.share.as_ref().map_or(String::new(), |share| {
            format!(
                ",\"share\":{{\"frames_encoded\":{},\"keyframes\":{},\"keyframe_requests\":{},\
\"encode_fps\":{:.2},\"kbps\":{:.1},\"watchers_max\":{},\"bytes\":{},\"threads\":{},\"skipped\":{},\
\"send_failures\":{}}}",
                share.frames_encoded,
                share.keyframes,
                share.keyframe_requests,
                share.encode_fps,
                share.kbps,
                share.watchers_max,
                share.bytes,
                share.threads,
                share.skipped,
                share.send_failures,
            )
        });

        let watch = self.watch.as_ref().map_or(String::new(), |watch| {
            format!(
                ",\"watch\":{{\"user\":{},\"user_id\":{},\"pictures\":{},\"keyframes\":{},\
\"dropped\":{},\"decode_errors\":{},\"first_picture_ms\":{},\"width\":{},\"height\":{},\
\"decode_fps\":{:.2},\"keyframe_requests_sent\":{},\"share_tone_seconds\":{:.2},\
\"audio_played\":{},\"audio_concealed\":{},\"audio_lost\":{},\"audio_late\":{},\
\"audio_concealed_pct\":{:.2}}}",
                quote(&watch.user),
                watch.user_id,
                watch.pictures,
                watch.keyframes,
                watch.dropped,
                watch.decode_errors,
                millis(watch.first_picture_ms),
                watch.width,
                watch.height,
                watch.decode_fps,
                watch.keyframe_requests_sent,
                seconds(watch.share_tone_frames),
                watch.audio_played,
                watch.audio_concealed,
                watch.audio_lost,
                watch.audio_late,
                self.share_concealed_pct(),
            )
        });

        let camera = self.camera.as_ref().map_or(String::new(), |camera| {
            format!(
                ",\"camera\":{{\"frames\":{},\"keyframes\":{},\"send_failures\":{}}}",
                camera.frames, camera.keyframes, camera.send_failures,
            )
        });

        let cameras = if self.cameras.is_empty() {
            String::new()
        } else {
            let entries = self
                .cameras
                .iter()
                .map(|camera| {
                    format!(
                        "{{\"user\":{},\"ssrc\":{},\"pictures\":{},\"keyframes\":{},\"dropped\":{}}}",
                        quote(&camera.user),
                        camera.ssrc,
                        camera.pictures,
                        camera.keyframes,
                        camera.dropped,
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(",\"cameras\":[{entries}]")
        };

        format!(
            "{{\"user\":{},\"user_id\":{},\"channel_id\":{},\"channel_name\":{},\"ssrc\":{},\
\"packets_sent\":{},\"packets_received\":{},\"bytes_sent\":{},\"bytes_received\":{},\"rejected\":{},\"send_failures\":{},\"frames_sent\":{},\"frames_gated\":{},\
\"decoded_seconds\":{:.2},\"tone_seconds\":{:.2},\"gaps\":{},\"late\":{},\
\"rtt_ms\":{{\"min\":{},\"avg\":{},\"max\":{},\"last\":{},\"samples\":{}}},\
\"link\":{},\"peers\":[{}],\"speaking_events\":[{}]{}{}{}{}}}",
            quote(&self.user),
            self.user_id,
            self.channel_id,
            quote(&self.channel_name),
            self.ssrc,
            self.packets_sent,
            self.packets_received,
            self.bytes_sent,
            self.bytes_received,
            self.rejected,
            self.send_failures,
            self.frames_sent,
            self.frames_gated,
            seconds(self.decoded_frames),
            self.tone_seconds(),
            gaps,
            late,
            millis(self.rtt.min),
            millis(self.rtt.avg),
            millis(self.rtt.max),
            millis(self.rtt.last),
            self.rtt.samples,
            quote(self.link),
            peers,
            speaking,
            share,
            watch,
            camera,
            cameras,
        )
    }
}

fn seconds(frames: u64) -> f64 {
    frames as f64 * FRAME_MS as f64 / 1000.0
}

fn millis(value: Option<f64>) -> String {
    match value {
        Some(ms) => format!("{ms:.2}"),
        None => "null".to_owned(),
    }
}

pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
