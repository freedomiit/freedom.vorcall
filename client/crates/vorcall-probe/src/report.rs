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
    pub room: String,
    pub ssrc: u32,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rejected: u64,
    pub send_failures: u64,
    pub decoded_frames: u64,
    pub tone_frames: u64,
    pub rtt: Rtt,
    pub link: &'static str,
    pub peers: Vec<PeerReport>,
    pub speaking_events: Vec<SpeakingEvent>,
}

impl Report {
    pub fn tone_seconds(&self) -> f64 {
        seconds(self.tone_frames)
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

        format!(
            "{{\"user\":{},\"user_id\":{},\"room\":{},\"ssrc\":{},\
\"packets_sent\":{},\"packets_received\":{},\"bytes_sent\":{},\"bytes_received\":{},\"rejected\":{},\"send_failures\":{},\
\"decoded_seconds\":{:.2},\"tone_seconds\":{:.2},\"gaps\":{},\"late\":{},\
\"rtt_ms\":{{\"min\":{},\"avg\":{},\"max\":{},\"last\":{},\"samples\":{}}},\
\"link\":{},\"peers\":[{}],\"speaking_events\":[{}]}}",
            quote(&self.user),
            self.user_id,
            quote(&self.room),
            self.ssrc,
            self.packets_sent,
            self.packets_received,
            self.bytes_sent,
            self.bytes_received,
            self.rejected,
            self.send_failures,
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

fn quote(value: &str) -> String {
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
