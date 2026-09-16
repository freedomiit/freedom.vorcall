//! The UDP side of a voice room: one socket to the relay, a send path safe to
//! call from the audio thread, keepalive pings for reachability and round-trip
//! time, and the receive task that feeds [`Playout`].
//!
//! The same socket, key and ssrc carry a screen share: its video as fragmented
//! access units ([`crate::video`]) and its audio as a stereo stream of its own.
//! A client watches at most one sharer at a time, and every datagram type draws
//! its sequence number from the one counter, so no nonce is ever reused.
//!
//! A camera is a second video stream over that same session, framed like the
//! share's but on its own packet type, so one member can share a screen and be
//! on camera at once. Cameras are many where the share is one: each watched
//! ssrc gets its own reassembly, its own keyframe pacing and its own channel.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use socket2::{Domain, Socket, Type};
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::SAMPLE_RATE;
use crate::crypto::MediaCipher;
use crate::jitter::Incoming;
use crate::packet::{Header, MAX_DATAGRAM, MIN_DATAGRAM, PONG_SEQ_BIT, PacketError, PacketType};
use crate::playout::{PeerStats, Playout};
use crate::video::{
    AccessUnit, Depacketizer, FragmentHeader, MAX_VIDEO_DATA, VIDEO_HEADER_LEN, VideoStats,
    fragments,
};

pub const PING_INTERVAL: Duration = Duration::from_secs(5);
pub const LINK_TIMEOUT: Duration = Duration::from_secs(15);

/// Pongs for pings older than this many rounds are no longer matched.
const PENDING_PINGS: usize = 8;
/// One MTU's worth of slack, so an oversized datagram is seen and rejected
/// rather than silently truncated into something that looks valid.
const RECV_BUFFER: usize = 2048;
/// However many holes a viewer sees, it asks the sharer this often at most.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
/// Fragments handed to the kernel before the sender pauses: a keyframe is a
/// burst of hundreds of datagrams and the send buffer is not infinite. Halving
/// the instantaneous burst only spreads it out; [`retry_send`] is what absorbs
/// a queue that is full anyway.
const VIDEO_BURST: usize = 8;
/// Long enough for the kernel to drain a burst, short enough to be invisible
/// inside a frame interval.
const BURST_PAUSE: Duration = Duration::from_millis(1);
/// Extra attempts a video fragment gets when the socket refuses it for a
/// reason that passes on its own: 40 × [`RETRY_PAUSE`] bounds one fragment to
/// about 40 ms, well under the second a watcher would otherwise spend frozen
/// waiting for the next keyframe.
const MAX_SEND_RETRIES: u32 = 40;
/// The same millisecond as [`BURST_PAUSE`], for the same reason: it is roughly
/// what an interface queue needs to drain.
const RETRY_PAUSE: Duration = Duration::from_millis(1);
/// 4 MiB each way: a keyframe arrives as one burst, and the few hundred KiB a
/// socket gets by default would lose most of it.
const SOCKET_BUFFER_BYTES: usize = 4 << 20;
/// `Shared::watched` holding no ssrc at all.
const NOT_WATCHING: i64 = -1;
/// One 20 ms frame on the 48 kHz media clock.
const TS_PER_FRAME: u32 = crate::FRAME_SAMPLES as u32;

pub struct MediaConfig {
    pub host: String,
    pub port: u16,
    pub key: [u8; 32],
    pub ssrc: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    #[default]
    Connecting,
    Connected,
    NoMedia,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    /// Every datagram the socket handed us, including the rejected ones.
    pub packets_received: u64,
    pub bytes_received: u64,
    /// Datagrams that failed length/header/AEAD checks.
    pub rejected: u64,
    /// Video and share audio from a sharer this client is not watching.
    pub ignored: u64,
    /// Camera video from an ssrc whose camera this client does not watch.
    pub camera_ignored: u64,
    /// Datagrams the socket refused for good, usually a full send buffer;
    /// a video fragment is only counted here once its retries ran out.
    pub send_failures: u64,
    pub rtt_last_ms: Option<f64>,
    pub rtt_min_ms: Option<f64>,
    pub rtt_avg_ms: Option<f64>,
    pub rtt_max_ms: Option<f64>,
    pub rtt_samples: u32,
    pub link: Link,
    pub peers: Vec<(u32, PeerStats)>,
    pub video: VideoStats,
    /// One entry per watched camera, by ssrc ascending; the share's numbers
    /// stay in [`video`](Self::video).
    pub cameras: Vec<(u32, VideoStats)>,
    /// Keyframe requests other clients sent this one, as the sharer.
    pub keyframe_requests_received: u64,
    /// The same, for this client's camera.
    pub camera_keyframe_requests_received: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("cannot resolve the relay: {0}")]
    Resolve(String),
    #[error("cannot bind the media socket: {0}")]
    Bind(std::io::Error),
    #[error("cannot send a media datagram: {0}")]
    Send(std::io::Error),
    #[error("cannot frame a media datagram: {0}")]
    Packet(#[from] PacketError),
    #[error("the media engine is closed")]
    Closed,
}

#[derive(Default)]
struct Rtt {
    last_ms: Option<f64>,
    min_ms: Option<f64>,
    max_ms: Option<f64>,
    sum_ms: f64,
    samples: u32,
    last_pong_at: Option<Instant>,
}

struct Shared {
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    packets_received: AtomicU64,
    bytes_received: AtomicU64,
    rejected: AtomicU64,
    ignored: AtomicU64,
    camera_ignored: AtomicU64,
    send_failures: AtomicU64,
    keyframe_requests_received: AtomicU64,
    camera_keyframe_requests_received: AtomicU64,
    /// Raised by an incoming keyframe request, lowered by the sharer reading
    /// it: requests arriving between reads coalesce into one keyframe.
    keyframe_request: AtomicBool,
    /// The camera's own flag: the two streams are encoded separately, so a
    /// request for one must never cost the other a keyframe.
    camera_keyframe_request: AtomicBool,
    /// The sharer whose video and share audio are accepted, or [`NOT_WATCHING`].
    watched: AtomicI64,
    /// Every datagram type draws from one counter so a nonce is never reused.
    seq: AtomicU64,
    /// The share audio's own media clock, one frame per frame sent. Taken from
    /// the wall clock instead, it would turn the sender's scheduling into holes
    /// on the watcher's side: a share's audio goes out beside the video rather
    /// than on a tick of its own.
    share_audio_ts: AtomicU32,
    started: Instant,
    pending_pings: Mutex<VecDeque<u64>>,
    rtt: Mutex<Rtt>,
}

impl Shared {
    /// The 48 kHz media clock, wrapping like the wire field it fills.
    fn ts(&self) -> u32 {
        let elapsed = self.started.elapsed();
        let samples = elapsed.as_secs().wrapping_mul(u64::from(SAMPLE_RATE))
            + u64::from(elapsed.subsec_nanos()) * u64::from(SAMPLE_RATE) / 1_000_000_000;
        samples as u32
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// A poisoned lock still holds usable state — a jitter buffer or a counter —
/// and losing the call over it would be worse than carrying on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct FrameSender {
    socket: Arc<UdpSocket>,
    cipher: Arc<MediaCipher>,
    shared: Arc<Shared>,
    ssrc: u32,
}

impl FrameSender {
    /// Seals one encoded frame and hands it to the kernel without ever
    /// blocking: a full socket buffer drops the frame, which is what a
    /// real-time stream wants anyway.
    pub fn send_audio(&self, opus: &[u8], marker: bool) -> Result<(), EngineError> {
        let header = Header {
            kind: PacketType::Audio,
            marker,
            ssrc: self.ssrc,
            seq: self.shared.seq.fetch_add(1, Ordering::Relaxed),
            ts: self.shared.ts(),
        };
        send(&self.socket, &self.shared, &self.cipher.seal(&header, opus))
    }

    /// One 20 ms stereo frame of the screen share's own audio, stamped one
    /// frame past the last one sent whenever it actually goes out.
    pub fn send_share_audio(&self, opus: &[u8], marker: bool) -> Result<(), EngineError> {
        let header = Header {
            kind: PacketType::ShareAudio,
            marker,
            ssrc: self.ssrc,
            seq: self.shared.seq.fetch_add(1, Ordering::Relaxed),
            ts: self
                .shared
                .share_audio_ts
                .fetch_add(TS_PER_FRAME, Ordering::Relaxed),
        };
        send(&self.socket, &self.shared, &self.cipher.seal(&header, opus))
    }

    /// Starts the share audio's clock over, as a share starts or its audio
    /// resumes. The marker on the first frame after it is what tells a watcher
    /// the stream begins again.
    pub fn reset_share_audio_clock(&self) {
        self.shared
            .share_audio_ts
            .store(self.shared.ts(), Ordering::Relaxed);
    }

    /// Moves the share audio's clock past `frames` frames that were never
    /// captured, so the silence reaches the watcher as the gap it was rather
    /// than as audio cut short.
    pub fn skip_share_audio(&self, frames: u32) {
        self.shared
            .share_audio_ts
            .fetch_add(frames.wrapping_mul(TS_PER_FRAME), Ordering::Relaxed);
    }

    /// Cuts one encoded access unit into fragments and sends them under one
    /// timestamp, returning how many it attempted.
    ///
    /// This blocks: every [`VIDEO_BURST`] fragments it sleeps for a
    /// millisecond, and a refused fragment is retried for up to
    /// [`MAX_SEND_RETRIES`] more, so it belongs on a worker thread and never on
    /// the UI or audio thread. A fragment the socket refuses for good is
    /// counted in [`Stats::send_failures`] and ends the unit; the fragments
    /// already out are harmless and the watcher asks for a keyframe. The count
    /// returned is how many were attempted, and the call only fails when not
    /// one of them went out.
    pub fn send_video(
        &self,
        frame_id: u32,
        keyframe: bool,
        data: &[u8],
    ) -> Result<usize, EngineError> {
        self.send_video_stream(PacketType::Video, frame_id, keyframe, data)
    }

    /// The camera's access units, on the camera's own packet type. Pacing,
    /// retries and [`Stats::send_failures`] work exactly as for
    /// [`send_video`](Self::send_video), and it blocks for the same reason.
    pub fn send_camera_video(
        &self,
        frame_id: u32,
        keyframe: bool,
        data: &[u8],
    ) -> Result<usize, EngineError> {
        self.send_video_stream(PacketType::CameraVideo, frame_id, keyframe, data)
    }

    fn send_video_stream(
        &self,
        kind: PacketType,
        frame_id: u32,
        keyframe: bool,
        data: &[u8],
    ) -> Result<usize, EngineError> {
        // One clock reading for the whole unit: the fragments are one frame.
        let ts = self.shared.ts();
        let mut plaintext = Vec::with_capacity(VIDEO_HEADER_LEN + MAX_VIDEO_DATA);
        let mut count = 0usize;
        let mut sent = 0usize;
        let mut last_error = None;

        for (index, (fragment, chunk)) in fragments(frame_id, keyframe, data)?.enumerate() {
            if index > 0 && index % VIDEO_BURST == 0 {
                std::thread::sleep(BURST_PAUSE);
            }
            plaintext.clear();
            plaintext.extend_from_slice(&fragment.encode());
            plaintext.extend_from_slice(chunk);

            let header = Header {
                kind,
                marker: false,
                ssrc: self.ssrc,
                seq: self.shared.seq.fetch_add(1, Ordering::Relaxed),
                ts,
            };
            count += 1;
            match send_with_retry(
                &self.socket,
                &self.shared,
                &self.cipher.seal(&header, &plaintext),
            ) {
                Ok(()) => sent += 1,
                // A unit missing a fragment cannot be decoded anyway, so the
                // rest would only spend bandwidth, and a retry's worth of
                // milliseconds each, on a socket that just proved unwritable.
                Err(EngineError::Send(error)) => {
                    last_error = Some(error);
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        match last_error {
            Some(error) if sent == 0 => Err(EngineError::Send(error)),
            _ => Ok(count),
        }
    }

    /// Asks `target_ssrc` for a keyframe of its screen share, as a viewer.
    pub fn request_keyframe(&self, target_ssrc: u32) -> Result<(), EngineError> {
        send_keyframe_request(
            &self.socket,
            &self.cipher,
            &self.shared,
            PacketType::KeyframeRequest,
            self.ssrc,
            target_ssrc,
        )
    }

    /// The same for `target_ssrc`'s camera, which is asked separately.
    pub fn request_camera_keyframe(&self, target_ssrc: u32) -> Result<(), EngineError> {
        send_keyframe_request(
            &self.socket,
            &self.cipher,
            &self.shared,
            PacketType::CameraKeyframeRequest,
            self.ssrc,
            target_ssrc,
        )
    }

    /// Whether a viewer asked for a keyframe since the last call, as the
    /// sharer. Requests arriving between two calls coalesce into one.
    pub fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe_request.swap(false, Ordering::Relaxed)
    }

    /// The same for this client's camera; the two flags are independent.
    pub fn take_camera_keyframe_request(&self) -> bool {
        self.shared
            .camera_keyframe_request
            .swap(false, Ordering::Relaxed)
    }

    /// [`Stats::send_failures`] without the rest of the report, for a sender
    /// that has no engine to ask: every datagram this session gave up on,
    /// retries included.
    pub fn send_failures(&self) -> u64 {
        self.shared.send_failures.load(Ordering::Relaxed)
    }
}

fn send_keyframe_request(
    socket: &UdpSocket,
    cipher: &MediaCipher,
    shared: &Shared,
    kind: PacketType,
    ssrc: u32,
    target_ssrc: u32,
) -> Result<(), EngineError> {
    let header = Header {
        kind,
        marker: false,
        ssrc,
        seq: shared.seq.fetch_add(1, Ordering::Relaxed),
        ts: shared.ts(),
    };
    send(
        socket,
        shared,
        &cipher.seal(&header, &target_ssrc.to_be_bytes()),
    )
}

/// The sharer this client is watching, if any.
fn watched(shared: &Shared) -> Option<u32> {
    match shared.watched.load(Ordering::Relaxed) {
        NOT_WATCHING => None,
        ssrc => u32::try_from(ssrc).ok(),
    }
}

fn send(socket: &UdpSocket, shared: &Shared, datagram: &[u8]) -> Result<(), EngineError> {
    if datagram.len() < MIN_DATAGRAM {
        return Err(undersized());
    }
    match socket.try_send(datagram) {
        Ok(sent) => {
            count_sent(shared, sent);
            Ok(())
        }
        Err(error) => {
            shared.send_failures.fetch_add(1, Ordering::Relaxed);
            Err(EngineError::Send(error))
        }
    }
}

/// [`send`] for a caller that can afford to wait: a datagram the socket refuses
/// for a transient reason is offered again after [`RETRY_PAUSE`] instead of
/// being lost. One dropped fragment costs a whole access unit, and a dropped
/// keyframe fragment freezes every watcher until the next keyframe — which a
/// still-full queue would lose the same way.
///
/// It sleeps, so it belongs on a worker thread; the audio path keeps [`send`].
fn send_with_retry(
    socket: &UdpSocket,
    shared: &Shared,
    datagram: &[u8],
) -> Result<(), EngineError> {
    if datagram.len() < MIN_DATAGRAM {
        return Err(undersized());
    }
    match retry_send(|| socket.try_send(datagram), std::thread::sleep) {
        Ok(sent) => {
            count_sent(shared, sent);
            Ok(())
        }
        Err(error) => {
            shared.send_failures.fetch_add(1, Ordering::Relaxed);
            Err(EngineError::Send(error))
        }
    }
}

/// Calls `attempt` until it succeeds, fails for a reason waiting cannot fix, or
/// has used all [`MAX_SEND_RETRIES`] retries, pausing in between.
///
/// The socket and the clock are both parameters so the loop can be tested
/// without either.
fn retry_send(
    mut attempt: impl FnMut() -> std::io::Result<usize>,
    mut pause: impl FnMut(Duration),
) -> std::io::Result<usize> {
    let mut retries_left = MAX_SEND_RETRIES;
    loop {
        match attempt() {
            Ok(sent) => return Ok(sent),
            Err(error) if is_transient(&error) && retries_left > 0 => {
                retries_left -= 1;
                pause(RETRY_PAUSE);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether the datagram was refused by a queue that drains rather than by
/// anything about the datagram itself. macOS answers a full interface queue
/// with `ENOBUFS` where Linux buffers, and `std` gives it no `ErrorKind`, so it
/// is matched on the raw code.
fn is_transient(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    const ENOBUFS: i32 = libc::ENOBUFS;
    // `WSAENOBUFS`, Winsock's own code for the same condition.
    #[cfg(windows)]
    const ENOBUFS: i32 = 10055;

    error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(ENOBUFS)
}

fn count_sent(shared: &Shared, bytes: usize) {
    shared.packets_sent.fetch_add(1, Ordering::Relaxed);
    shared.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Sealing always produces a header and a tag, so anything shorter means the
/// caller built the datagram wrong; it never reaches the wire.
fn undersized() -> EngineError {
    EngineError::Send(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "sealing produced no datagram",
    ))
}

/// What the [`MediaEngine`] knows about the video stream it is watching.
struct VideoState {
    depacketizer: Depacketizer,
    /// Requests sent for this stream; reset along with the depacketizer.
    keyframe_requests: u64,
    last_request_at: Option<Instant>,
}

impl VideoState {
    fn new() -> Self {
        Self {
            depacketizer: Depacketizer::new(),
            keyframe_requests: 0,
            last_request_at: None,
        }
    }
}

/// One watched camera. Everything the share keeps in [`VideoState`], plus the
/// channel that stream's decoder reads, held per ssrc so two cameras never gate
/// each other's frames or share a keyframe request.
struct CameraRx {
    depacketizer: Depacketizer,
    units: UnboundedSender<AccessUnit>,
    keyframe_requests: u64,
    last_request_at: Option<Instant>,
}

type Cameras = HashMap<u32, CameraRx>;

/// The video the receive loop reassembles: the one screen share this client
/// watches, and the cameras, of which there may be several.
struct Streams {
    video: Arc<Mutex<VideoState>>,
    access_units: UnboundedSender<AccessUnit>,
    cameras: Arc<Mutex<Cameras>>,
}

pub struct MediaEngine {
    sender: FrameSender,
    playout: Arc<Mutex<Playout>>,
    shared: Arc<Shared>,
    video: Arc<Mutex<VideoState>>,
    cameras: Arc<Mutex<Cameras>>,
    /// Handed out once, to whoever decodes the watched share's video.
    access_units: Mutex<Option<UnboundedReceiver<AccessUnit>>>,
    receive_task: JoinHandle<()>,
    ping_task: JoinHandle<()>,
}

impl MediaEngine {
    pub async fn connect(config: MediaConfig) -> Result<Self, EngineError> {
        let relay = lookup_host((config.host.as_str(), config.port))
            .await
            .map_err(|error| EngineError::Resolve(error.to_string()))?
            .find(|address| address.is_ipv4())
            .ok_or_else(|| EngineError::Resolve(format!("no IPv4 address for {}", config.host)))?;

        let (socket, recv_buffer, send_buffer) = bind_socket(relay)?;
        let socket = Arc::new(UdpSocket::from_std(socket).map_err(EngineError::Bind)?);

        let cipher = Arc::new(MediaCipher::new(&config.key));
        let shared = Arc::new(Shared {
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            camera_ignored: AtomicU64::new(0),
            send_failures: AtomicU64::new(0),
            keyframe_requests_received: AtomicU64::new(0),
            camera_keyframe_requests_received: AtomicU64::new(0),
            keyframe_request: AtomicBool::new(false),
            camera_keyframe_request: AtomicBool::new(false),
            watched: AtomicI64::new(NOT_WATCHING),
            seq: AtomicU64::new(0),
            share_audio_ts: AtomicU32::new(0),
            started: Instant::now(),
            pending_pings: Mutex::new(VecDeque::new()),
            rtt: Mutex::new(Rtt::default()),
        });
        let playout = Arc::new(Mutex::new(Playout::new()));
        let video = Arc::new(Mutex::new(VideoState::new()));
        let cameras = Arc::new(Mutex::new(Cameras::new()));
        let (access_units, incoming_units) = unbounded_channel();

        tracing::debug!(
            %relay,
            ssrc = config.ssrc,
            recv_buffer,
            send_buffer,
            "media engine connected"
        );

        let receive_task = tokio::spawn(receive_loop(
            Arc::clone(&socket),
            Arc::clone(&cipher),
            Arc::clone(&shared),
            Arc::clone(&playout),
            Streams {
                video: Arc::clone(&video),
                access_units,
                cameras: Arc::clone(&cameras),
            },
            config.ssrc,
        ));
        let ping_task = tokio::spawn(ping_loop(
            Arc::clone(&socket),
            Arc::clone(&cipher),
            Arc::clone(&shared),
            config.ssrc,
        ));

        Ok(Self {
            sender: FrameSender {
                socket,
                cipher,
                shared: Arc::clone(&shared),
                ssrc: config.ssrc,
            },
            playout,
            shared,
            video,
            cameras,
            access_units: Mutex::new(Some(incoming_units)),
            receive_task,
            ping_task,
        })
    }

    /// Chooses the sharer whose video and share audio are accepted; `None`
    /// drops both from everyone, counted in [`Stats::ignored`].
    ///
    /// Switching starts over: a new depacketizer, no share stream left in the
    /// playout, and one keyframe request so the stream begins at a frame the
    /// decoder can actually start from.
    pub fn watch(&self, ssrc: Option<u32>) {
        self.shared
            .watched
            .store(ssrc.map_or(NOT_WATCHING, i64::from), Ordering::Relaxed);
        *lock(&self.video) = VideoState::new();
        lock(&self.playout).remove_share();

        let Some(target) = ssrc else {
            return;
        };
        {
            let mut video = lock(&self.video);
            video.keyframe_requests += 1;
            video.last_request_at = Some(Instant::now());
        }
        if let Err(error) = self.sender.request_keyframe(target) {
            tracing::debug!(%error, "keyframe request not sent");
        }
    }

    /// The watched sharer's reassembled access units. Created at connect and
    /// handed out once; `None` afterwards.
    pub fn take_access_units(&self) -> Option<UnboundedReceiver<AccessUnit>> {
        lock(&self.access_units).take()
    }

    /// Starts watching one camera and hands back that stream's access units.
    ///
    /// Idempotent per ssrc: watching one again replaces the channel, starts its
    /// reassembly over and asks for a keyframe, exactly as the first call did.
    /// Neither the other cameras nor the screen share are touched.
    pub fn watch_camera(&self, ssrc: u32) -> UnboundedReceiver<AccessUnit> {
        let (units, incoming) = unbounded_channel();
        lock(&self.cameras).insert(
            ssrc,
            CameraRx {
                depacketizer: Depacketizer::new(),
                units,
                // The request below, counted whether or not the socket took it,
                // like the share's.
                keyframe_requests: 1,
                last_request_at: Some(Instant::now()),
            },
        );
        if let Err(error) = self.sender.request_camera_keyframe(ssrc) {
            tracing::debug!(%error, "camera keyframe request not sent");
        }
        incoming
    }

    /// Stops watching one camera; an ssrc nobody watches is a no-op.
    pub fn unwatch_camera(&self, ssrc: u32) {
        lock(&self.cameras).remove(&ssrc);
    }

    pub fn unwatch_all_cameras(&self) {
        lock(&self.cameras).clear();
    }

    /// Each watched camera's own reassembly counters, by ssrc ascending.
    pub fn camera_stats(&self) -> Vec<(u32, VideoStats)> {
        let mut stats: Vec<(u32, VideoStats)> = lock(&self.cameras)
            .iter()
            .map(|(ssrc, camera)| {
                let mut stats = camera.depacketizer.stats();
                stats.keyframe_requests = camera.keyframe_requests;
                (*ssrc, stats)
            })
            .collect();
        stats.sort_by_key(|(ssrc, _)| *ssrc);
        stats
    }

    /// The watched stream's reassembly counters, which start over whenever
    /// [`watch`](Self::watch) points somewhere else.
    pub fn video_stats(&self) -> VideoStats {
        let video = lock(&self.video);
        let mut stats = video.depacketizer.stats();
        stats.keyframe_requests = video.keyframe_requests;
        stats
    }

    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    pub fn playout(&self) -> Arc<Mutex<Playout>> {
        Arc::clone(&self.playout)
    }

    pub fn stats(&self) -> Stats {
        let video = self.video_stats();
        let cameras = self.camera_stats();
        let rtt = lock(&self.shared.rtt);
        let link = match rtt.last_pong_at {
            None => Link::Connecting,
            Some(at) if at.elapsed() > LINK_TIMEOUT => Link::NoMedia,
            Some(_) => Link::Connected,
        };
        Stats {
            packets_sent: self.shared.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.shared.bytes_sent.load(Ordering::Relaxed),
            packets_received: self.shared.packets_received.load(Ordering::Relaxed),
            bytes_received: self.shared.bytes_received.load(Ordering::Relaxed),
            rejected: self.shared.rejected.load(Ordering::Relaxed),
            ignored: self.shared.ignored.load(Ordering::Relaxed),
            camera_ignored: self.shared.camera_ignored.load(Ordering::Relaxed),
            send_failures: self.shared.send_failures.load(Ordering::Relaxed),
            rtt_last_ms: rtt.last_ms,
            rtt_min_ms: rtt.min_ms,
            rtt_avg_ms: (rtt.samples > 0).then(|| rtt.sum_ms / f64::from(rtt.samples)),
            rtt_max_ms: rtt.max_ms,
            rtt_samples: rtt.samples,
            link,
            peers: lock(&self.playout).stats(),
            video,
            cameras,
            keyframe_requests_received: self
                .shared
                .keyframe_requests_received
                .load(Ordering::Relaxed),
            camera_keyframe_requests_received: self
                .shared
                .camera_keyframe_requests_received
                .load(Ordering::Relaxed),
        }
    }

    pub async fn close(self) {
        self.receive_task.abort();
        self.ping_task.abort();
        let _ = self.receive_task.await;
        let _ = self.ping_task.await;
        tracing::debug!("media engine closed");
    }
}

/// The media socket: IPv4, connected to the relay, with kernel buffers wide
/// enough for a keyframe's burst. Returns the sizes the kernel granted, which
/// it is free to clamp.
fn bind_socket(relay: SocketAddr) -> Result<(std::net::UdpSocket, usize, usize), EngineError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).map_err(EngineError::Bind)?;
    // Best effort: a kernel that refuses the size is no reason to lose the call.
    if let Err(error) = socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES) {
        tracing::debug!(%error, "media socket receive buffer left at its default");
    }
    if let Err(error) = socket.set_send_buffer_size(SOCKET_BUFFER_BYTES) {
        tracing::debug!(%error, "media socket send buffer left at its default");
    }
    socket
        .bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into())
        .map_err(EngineError::Bind)?;
    // Connecting the socket makes the kernel drop datagrams from anyone but
    // the relay, so the receive path never sees off-path traffic.
    socket.connect(&relay.into()).map_err(EngineError::Bind)?;
    socket.set_nonblocking(true).map_err(EngineError::Bind)?;

    let recv_buffer = socket.recv_buffer_size().unwrap_or(0);
    let send_buffer = socket.send_buffer_size().unwrap_or(0);
    Ok((socket.into(), recv_buffer, send_buffer))
}

async fn receive_loop(
    socket: Arc<UdpSocket>,
    cipher: Arc<MediaCipher>,
    shared: Arc<Shared>,
    playout: Arc<Mutex<Playout>>,
    streams: Streams,
    ssrc: u32,
) {
    let Streams {
        video,
        access_units,
        cameras,
    } = streams;
    let mut buffer = [0u8; RECV_BUFFER];
    loop {
        let read = match socket.recv(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                // A connected UDP socket surfaces ICMP errors here; the relay
                // may simply not be listening yet.
                tracing::debug!(%error, "media receive failed");
                continue;
            }
        };
        shared.packets_received.fetch_add(1, Ordering::Relaxed);
        shared
            .bytes_received
            .fetch_add(read as u64, Ordering::Relaxed);

        if !(MIN_DATAGRAM..=MAX_DATAGRAM).contains(&read) {
            shared.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let (header, payload) = match cipher.open(&buffer[..read]) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::debug!(%error, "rejected a media datagram");
                shared.rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        match header.kind {
            PacketType::Audio => {
                if header.ssrc == ssrc {
                    continue;
                }
                lock(&playout).push(
                    header.ssrc,
                    Incoming {
                        seq: header.seq,
                        ts: header.ts,
                        marker: header.marker,
                        payload,
                    },
                );
            }
            PacketType::Video => {
                if watched(&shared) != Some(header.ssrc) {
                    shared.ignored.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let (fragment, data) = match FragmentHeader::decode(&payload) {
                    Ok(parts) => parts,
                    Err(error) => {
                        tracing::debug!(%error, "rejected a video fragment");
                        shared.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };

                let now = Instant::now();
                let (unit, ask) = {
                    let mut video = lock(&video);
                    let unit = video.depacketizer.push(now, header.ts, fragment, data);
                    let ask = video.depacketizer.needs_keyframe()
                        && video.last_request_at.is_none_or(|at| {
                            now.saturating_duration_since(at) >= KEYFRAME_REQUEST_INTERVAL
                        });
                    if ask {
                        video.keyframe_requests += 1;
                        video.last_request_at = Some(now);
                    }
                    (unit, ask)
                };
                if let Some(unit) = unit {
                    // Nobody is decoding any more: the unit just goes.
                    let _ = access_units.send(unit);
                }
                if ask
                    && let Err(error) = send_keyframe_request(
                        &socket,
                        &cipher,
                        &shared,
                        PacketType::KeyframeRequest,
                        ssrc,
                        header.ssrc,
                    )
                {
                    tracing::debug!(%error, "keyframe request not sent");
                }
            }
            PacketType::CameraVideo => {
                // A camera nobody watches costs one lookup and nothing else:
                // there are several of them, so this is not worth a log line.
                if !lock(&cameras).contains_key(&header.ssrc) {
                    shared.camera_ignored.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let (fragment, data) = match FragmentHeader::decode(&payload) {
                    Ok(parts) => parts,
                    Err(error) => {
                        tracing::debug!(%error, "rejected a camera fragment");
                        shared.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };

                let now = Instant::now();
                let ask = push_camera(&cameras, header.ssrc, now, header.ts, fragment, data);
                if ask
                    && let Err(error) = send_keyframe_request(
                        &socket,
                        &cipher,
                        &shared,
                        PacketType::CameraKeyframeRequest,
                        ssrc,
                        header.ssrc,
                    )
                {
                    tracing::debug!(%error, "camera keyframe request not sent");
                }
            }
            PacketType::ShareAudio => {
                if watched(&shared) != Some(header.ssrc) {
                    shared.ignored.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                lock(&playout).push_share(
                    header.ssrc,
                    Incoming {
                        seq: header.seq,
                        ts: header.ts,
                        marker: header.marker,
                        payload,
                    },
                );
            }
            PacketType::KeyframeRequest => {
                // Any request at all means "send a keyframe"; the target ssrc
                // is the relay's business, not the sharer's.
                if payload.len() < 4 {
                    tracing::debug!("keyframe request without a target ssrc");
                    continue;
                }
                shared.keyframe_request.store(true, Ordering::Relaxed);
                shared
                    .keyframe_requests_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            PacketType::CameraKeyframeRequest => {
                if payload.len() < 4 {
                    tracing::debug!("camera keyframe request without a target ssrc");
                    continue;
                }
                shared
                    .camera_keyframe_request
                    .store(true, Ordering::Relaxed);
                shared
                    .camera_keyframe_requests_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            PacketType::Pong => record_pong(&shared, &header, &payload),
            PacketType::Ping => {}
        }
    }
}

/// Feeds one fragment to the camera it belongs to and delivers whatever unit it
/// completes, returning whether that camera's owner should be asked for a
/// keyframe.
///
/// A stream whose decoder has dropped its receiver is forgotten here, so a
/// watcher that simply went away costs the loop one delivery and nothing more.
fn push_camera(
    cameras: &Mutex<Cameras>,
    ssrc: u32,
    now: Instant,
    ts: u32,
    fragment: FragmentHeader,
    data: &[u8],
) -> bool {
    let mut cameras = lock(cameras);
    let Some(camera) = cameras.get_mut(&ssrc) else {
        return false;
    };

    let unit = camera.depacketizer.push(now, ts, fragment, data);
    let ask = camera.depacketizer.needs_keyframe()
        && camera
            .last_request_at
            .is_none_or(|at| now.saturating_duration_since(at) >= KEYFRAME_REQUEST_INTERVAL);
    if ask {
        camera.keyframe_requests += 1;
        camera.last_request_at = Some(now);
    }
    let closed = match unit {
        Some(unit) => camera.units.send(unit).is_err(),
        None => false,
    };
    if closed {
        cameras.remove(&ssrc);
        return false;
    }
    ask
}

fn record_pong(shared: &Shared, header: &Header, payload: &[u8]) {
    let ping_seq = header.seq & !PONG_SEQ_BIT;
    let matched = {
        let mut pending = lock(&shared.pending_pings);
        match pending.iter().position(|seq| *seq == ping_seq) {
            Some(index) => {
                pending.remove(index);
                true
            }
            None => false,
        }
    };
    // A pong that authenticates but answers no ping of ours is a stray, not a
    // rejected datagram: `rejected` counts what failed the length, header or
    // AEAD checks, and this passed all three.
    if !matched {
        tracing::debug!(seq = ping_seq, "pong for an unknown ping");
        return;
    }
    let Ok(echoed) = <[u8; 8]>::try_from(payload) else {
        tracing::debug!("pong without an 8-byte clock echo");
        return;
    };

    let sent_ms = u64::from_be_bytes(echoed);
    let now_ms = shared.started.elapsed().as_secs_f64() * 1_000.0;
    let rtt_ms = (now_ms - sent_ms as f64).max(0.0);

    let mut rtt = lock(&shared.rtt);
    rtt.last_ms = Some(rtt_ms);
    rtt.min_ms = Some(rtt.min_ms.map_or(rtt_ms, |min| min.min(rtt_ms)));
    rtt.max_ms = Some(rtt.max_ms.map_or(rtt_ms, |max| max.max(rtt_ms)));
    rtt.sum_ms += rtt_ms;
    rtt.samples = rtt.samples.saturating_add(1);
    rtt.last_pong_at = Some(Instant::now());
}

async fn ping_loop(
    socket: Arc<UdpSocket>,
    cipher: Arc<MediaCipher>,
    shared: Arc<Shared>,
    ssrc: u32,
) {
    loop {
        let header = Header {
            kind: PacketType::Ping,
            marker: false,
            ssrc,
            seq: shared.seq.fetch_add(1, Ordering::Relaxed),
            ts: shared.ts(),
        };
        {
            let mut pending = lock(&shared.pending_pings);
            pending.push_back(header.seq);
            while pending.len() > PENDING_PINGS {
                pending.pop_front();
            }
        }
        let datagram = cipher.seal(&header, &shared.elapsed_ms().to_be_bytes());
        if let Err(error) = send(&socket, &shared, &datagram) {
            tracing::debug!(%error, "media ping not sent");
        }
        tokio::time::sleep(PING_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Encoder, STEREO_FRAME_SAMPLES, StereoEncoder};
    use crate::packet::{HEADER_LEN, TAG_LEN, VERSION};
    use crate::tone::Tone;
    use crate::{FRAME_SAMPLES, video};

    const KEY: [u8; 32] = [3u8; 32];

    /// A loopback stand-in for the relay, which has already learned the
    /// client's address from a datagram just as the real one does.
    struct Relay {
        socket: UdpSocket,
        cipher: MediaCipher,
    }

    impl Relay {
        async fn start(ssrc: u32) -> (Relay, MediaEngine) {
            let socket = UdpSocket::bind("127.0.0.1:0").await.expect("relay socket");
            let port = socket.local_addr().expect("relay address").port();
            let engine = MediaEngine::connect(MediaConfig {
                host: "127.0.0.1".to_string(),
                port,
                key: KEY,
                ssrc,
            })
            .await
            .expect("connects");

            // The relay can only answer an address it has already seen. The
            // engine's first keepalive can lose the race against its socket
            // becoming writable, and the next one is 5 s out, so the address is
            // knocked loose with throwaway audio instead of waited on.
            let mut buffer = [0u8; RECV_BUFFER];
            let mut client = None;
            for _ in 0..200 {
                let _ = engine.sender().send_audio(b"hello", false);
                if let Ok(read) =
                    tokio::time::timeout(Duration::from_millis(5), socket.recv_from(&mut buffer))
                        .await
                {
                    client = Some(read.expect("the relay reads").1);
                    break;
                }
            }
            let client = client.expect("the client never reached the relay");
            socket.connect(client).await.expect("learns the client");
            (
                Relay {
                    socket,
                    cipher: MediaCipher::new(&KEY),
                },
                engine,
            )
        }

        /// The next datagram of `kind`, or `None` once `within` has passed.
        async fn recv(&self, kind: PacketType, within: Duration) -> Option<(Header, Vec<u8>)> {
            let deadline = Instant::now() + within;
            let mut buffer = [0u8; RECV_BUFFER];
            loop {
                let left = deadline.checked_duration_since(Instant::now())?;
                let read = tokio::time::timeout(left, self.socket.recv(&mut buffer))
                    .await
                    .ok()?
                    .expect("the relay reads");
                let (header, payload) = self.cipher.open(&buffer[..read]).expect("opens");
                if header.kind == kind {
                    return Some((header, payload));
                }
            }
        }

        async fn send(&self, header: &Header, payload: &[u8]) {
            self.socket
                .send(&self.cipher.seal(header, payload))
                .await
                .expect("the relay sends");
        }

        /// One video fragment as the sharer `ssrc` would have sent it.
        async fn send_fragment(
            &self,
            ssrc: u32,
            seq: u64,
            ts: u32,
            fragment: FragmentHeader,
            data: &[u8],
        ) {
            self.send_stream_fragment(PacketType::Video, ssrc, seq, ts, fragment, data)
                .await;
        }

        /// The same on the camera's own packet type.
        async fn send_camera_fragment(
            &self,
            ssrc: u32,
            seq: u64,
            ts: u32,
            fragment: FragmentHeader,
            data: &[u8],
        ) {
            self.send_stream_fragment(PacketType::CameraVideo, ssrc, seq, ts, fragment, data)
                .await;
        }

        async fn send_stream_fragment(
            &self,
            kind: PacketType,
            ssrc: u32,
            seq: u64,
            ts: u32,
            fragment: FragmentHeader,
            data: &[u8],
        ) {
            let mut plaintext = fragment.encode().to_vec();
            plaintext.extend_from_slice(data);
            self.send(
                &Header {
                    kind,
                    marker: false,
                    ssrc,
                    seq,
                    ts,
                },
                &plaintext,
            )
            .await;
        }
    }

    fn fragment(frame_id: u32, index: u16, count: u16, keyframe: bool) -> FragmentHeader {
        FragmentHeader {
            frame_id,
            index,
            count,
            keyframe,
        }
    }

    async fn wait_for(mut ready: impl FnMut() -> bool) {
        for _ in 0..2_000 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the engine never reached the expected state");
    }

    #[tokio::test]
    async fn pings_are_answered_and_audio_reaches_the_playout() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.expect("relay socket");
        let relay_addr = relay.local_addr().expect("relay address");
        let cipher = MediaCipher::new(&KEY);

        let engine = MediaEngine::connect(MediaConfig {
            host: "127.0.0.1".to_string(),
            port: relay_addr.port(),
            key: KEY,
            ssrc: 11,
        })
        .await
        .expect("connects");

        assert_eq!(engine.stats().link, Link::Connecting);

        let mut buffer = [0u8; RECV_BUFFER];
        let (read, client_addr) = relay.recv_from(&mut buffer).await.expect("receives a ping");
        let (ping, echo) = cipher.open(&buffer[..read]).expect("opens the ping");
        assert_eq!(ping.kind, PacketType::Ping);
        assert_eq!(ping.ssrc, 11);
        assert_eq!(echo.len(), 8);

        let pong = Header {
            kind: PacketType::Pong,
            marker: false,
            ssrc: 0,
            seq: ping.seq | PONG_SEQ_BIT,
            ts: ping.ts,
        };
        relay
            .send_to(&cipher.seal(&pong, &echo), client_addr)
            .await
            .expect("sends the pong");

        wait_for(|| engine.stats().link == Link::Connected).await;
        let stats = engine.stats();
        assert_eq!(stats.rtt_samples, 1);
        assert!(stats.rtt_last_ms.is_some());
        assert_eq!(stats.rtt_min_ms, stats.rtt_max_ms);
        assert_eq!(stats.rejected, 0);

        let mut encoder = Encoder::new().expect("encoder");
        let mut tone = Tone::new(440.0, 0.5);
        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let mut frame = [0u8; 512];
        tone.fill(&mut pcm);
        let written = encoder.encode(&pcm, &mut frame).expect("encodes");

        let audio = Header {
            kind: PacketType::Audio,
            marker: true,
            ssrc: 77,
            seq: 0,
            ts: 0,
        };
        relay
            .send_to(&cipher.seal(&audio, &frame[..written]), client_addr)
            .await
            .expect("sends audio");

        let playout = engine.playout();
        wait_for(|| !lock(&playout).stats().is_empty()).await;
        let peers = lock(&playout).stats();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, 77);
        assert_eq!(peers[0].1.received, 1);

        assert!(engine.stats().packets_sent >= 1);
        engine.close().await;
    }

    #[tokio::test]
    async fn junk_and_foreign_keys_are_rejected() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.expect("relay socket");
        let relay_addr = relay.local_addr().expect("relay address");

        let engine = MediaEngine::connect(MediaConfig {
            host: "127.0.0.1".to_string(),
            port: relay_addr.port(),
            key: KEY,
            ssrc: 11,
        })
        .await
        .expect("connects");

        let mut buffer = [0u8; RECV_BUFFER];
        let (_, client_addr) = relay.recv_from(&mut buffer).await.expect("receives a ping");

        // Too short, then long enough but sealed under a key we do not share.
        relay
            .send_to(b"junk", client_addr)
            .await
            .expect("sends junk");
        let foreign = MediaCipher::new(&[9u8; 32]);
        let header = Header {
            kind: PacketType::Audio,
            marker: false,
            ssrc: 77,
            seq: 0,
            ts: 0,
        };
        relay
            .send_to(&foreign.seal(&header, &[0u8; 40]), client_addr)
            .await
            .expect("sends a foreign datagram");

        wait_for(|| engine.stats().rejected >= 2).await;
        assert_eq!(engine.stats().link, Link::Connecting);
        assert!(lock(&engine.playout()).stats().is_empty());
        engine.close().await;
    }

    #[test]
    fn a_sealed_ping_fits_the_minimum_datagram() {
        let cipher = MediaCipher::new(&KEY);
        let header = Header {
            kind: PacketType::Ping,
            marker: false,
            ssrc: 1,
            seq: 0,
            ts: 0,
        };
        let datagram = cipher.seal(&header, &0u64.to_be_bytes());
        assert_eq!(datagram.len(), MIN_DATAGRAM + 8);
        assert!(datagram.len() <= MAX_DATAGRAM);
        assert_eq!(datagram[0], VERSION);
        assert_eq!(datagram.len(), HEADER_LEN + 8 + TAG_LEN);
    }

    #[tokio::test]
    async fn a_video_unit_goes_out_as_fragments_under_one_timestamp() {
        let (relay, engine) = Relay::start(11).await;
        let unit: Vec<u8> = (0..5_000u32).map(|index| index as u8).collect();

        // 5 000 bytes over 1 156-byte fragments: five of them, no burst pause.
        let count = engine.sender().send_video(9, true, &unit).expect("sends");
        assert_eq!(count, 5);

        let mut rejoined = Vec::new();
        let mut previous_seq = None;
        let mut previous_ts = None;
        for index in 0..5u16 {
            let (header, payload) = relay
                .recv(PacketType::Video, Duration::from_secs(1))
                .await
                .expect("a fragment arrives");
            assert_eq!(header.ssrc, 11);
            assert!(!header.marker);
            if let Some(seq) = previous_seq {
                assert!(header.seq > seq, "{} follows {seq}", header.seq);
            }
            if let Some(ts) = previous_ts {
                assert_eq!(header.ts, ts, "the unit was split across timestamps");
            }
            previous_seq = Some(header.seq);
            previous_ts = Some(header.ts);

            let (fragment, data) = FragmentHeader::decode(&payload).expect("a fragment header");
            assert_eq!(fragment.frame_id, 9);
            assert_eq!(fragment.index, index);
            assert_eq!(fragment.count, 5);
            assert!(fragment.keyframe);
            assert!(payload.len() <= MAX_DATAGRAM - HEADER_LEN - TAG_LEN);
            rejoined.extend_from_slice(data);
        }
        assert_eq!(rejoined, unit);

        assert!(
            matches!(
                engine.sender().send_video(10, false, &[]),
                Err(EngineError::Packet(PacketError::TooShort))
            ),
            "an empty unit is not a frame"
        );
        engine.close().await;
    }

    #[tokio::test]
    async fn a_refused_fragment_ends_the_unit() {
        let (_relay, engine) = Relay::start(11).await;
        let refused_before = engine.stats().send_failures;

        // A socket with no peer refuses every datagram it is handed, so the
        // first fragment is the only one this unit ever attempts.
        let mut sender = engine.sender();
        sender.socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("a socket"));

        let unit: Vec<u8> = (0..5_000u32).map(|index| index as u8).collect();
        let error = sender
            .send_video(9, true, &unit)
            .expect_err("the unit reported success");
        assert!(matches!(error, EngineError::Send(_)), "{error}");
        assert_eq!(
            engine.stats().send_failures - refused_before,
            1,
            "the unit carried on past a fragment the socket refused"
        );
        engine.close().await;
    }

    #[tokio::test]
    async fn watching_a_sharer_asks_it_for_a_keyframe() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));

        let (header, payload) = relay
            .recv(PacketType::KeyframeRequest, Duration::from_millis(100))
            .await
            .expect("a keyframe request arrives");
        assert_eq!(header.ssrc, 11);
        assert_eq!(payload, 77u32.to_be_bytes());
        assert_eq!(engine.video_stats().keyframe_requests, 1);

        // Pointing somewhere else starts the stream's counters over.
        engine.watch(None);
        assert_eq!(engine.video_stats(), VideoStats::default());
        engine.close().await;
    }

    #[tokio::test]
    async fn video_from_a_sharer_nobody_watches_is_ignored() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));
        let mut units = engine.take_access_units().expect("the receiver");
        assert!(engine.take_access_units().is_none(), "handed out twice");

        relay
            .send_fragment(
                99,
                1,
                960,
                fragment(1, 0, 1, true),
                b"not the watched sharer",
            )
            .await;
        relay
            .send(
                &Header {
                    kind: PacketType::ShareAudio,
                    marker: true,
                    ssrc: 99,
                    seq: 2,
                    ts: 960,
                },
                b"not the watched sharer either",
            )
            .await;

        wait_for(|| engine.stats().ignored >= 2).await;
        assert_eq!(engine.stats().rejected, 0);
        assert_eq!(engine.video_stats().fragments, 0);
        assert!(units.try_recv().is_err(), "an ignored unit was delivered");
        assert!(lock(&engine.playout()).share_stats().is_none());
        engine.close().await;
    }

    #[tokio::test]
    async fn a_watched_unit_is_reassembled_and_handed_over() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));
        let mut units = engine.take_access_units().expect("the receiver");

        let unit: Vec<u8> = (0..2_000u32).map(|index| index as u8).collect();
        let cut: Vec<(FragmentHeader, &[u8])> = video::fragments(4, true, &unit)
            .expect("fragments")
            .collect();
        assert_eq!(cut.len(), 2);
        // Out of order, to prove the reassembly does not rely on arrival order.
        for (index, (header, data)) in cut.iter().enumerate().rev() {
            relay
                .send_fragment(77, 10 + index as u64, 48_000, *header, data)
                .await;
        }

        let delivered = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("a unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.frame_id, 4);
        assert!(delivered.keyframe);
        assert_eq!(delivered.ts, 48_000);
        assert_eq!(delivered.data, unit);

        let stats = engine.video_stats();
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.keyframes, 1);
        assert_eq!(stats.fragments, 2);
        assert_eq!(stats.dropped, 0);
        // The request `watch` sent, and no other: nothing was missing.
        assert_eq!(stats.keyframe_requests, 1);
        engine.close().await;
    }

    #[tokio::test]
    async fn a_lost_fragment_costs_exactly_one_keyframe_request() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));
        let mut units = engine.take_access_units().expect("the receiver");
        relay
            .recv(PacketType::KeyframeRequest, Duration::from_secs(1))
            .await
            .expect("the request `watch` sends");
        // Requests are paced from that one, so the stream starts once its
        // interval has passed; otherwise the hole below would be held back.
        tokio::time::sleep(KEYFRAME_REQUEST_INTERVAL).await;

        // Frame 1 whole, frame 2 missing its middle, frame 3 whole: completing
        // frame 3 is what gives up on frame 2.
        relay
            .send_fragment(77, 20, 960, fragment(1, 0, 1, true), b"one")
            .await;
        relay
            .send_fragment(77, 21, 1_920, fragment(2, 0, 3, false), b"two-a")
            .await;
        relay
            .send_fragment(77, 22, 1_920, fragment(2, 2, 3, false), b"two-c")
            .await;
        relay
            .send_fragment(77, 23, 2_880, fragment(3, 0, 1, false), b"three")
            .await;

        let first = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("the keyframe arrives")
            .expect("the channel is open");
        assert_eq!(first.data, b"one");

        relay
            .recv(PacketType::KeyframeRequest, Duration::from_millis(600))
            .await
            .expect("the hole is reported");
        assert!(
            relay
                .recv(PacketType::KeyframeRequest, Duration::from_millis(200))
                .await
                .is_none(),
            "the hole was reported twice"
        );

        // The keyframe that answers it is delivered, and asks for nothing more.
        relay
            .send_fragment(77, 24, 3_840, fragment(4, 0, 1, true), b"four")
            .await;
        let recovered = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("the next keyframe arrives")
            .expect("the channel is open");
        assert_eq!(recovered.frame_id, 4);
        assert_eq!(recovered.data, b"four");
        assert!(
            relay
                .recv(PacketType::KeyframeRequest, Duration::from_millis(200))
                .await
                .is_none(),
            "a delivered keyframe still asked for one"
        );

        let stats = engine.video_stats();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.keyframe_requests, 2);
        // Frame 2 abandoned and frame 3 gated behind the missing keyframe.
        assert_eq!(stats.dropped, 2);
        engine.close().await;
    }

    #[tokio::test]
    async fn a_full_size_fragment_is_carried_and_a_larger_datagram_is_not() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));
        let mut units = engine.take_access_units().expect("the receiver");

        // The largest fragment there is: exactly MAX_DATAGRAM on the wire.
        let unit = vec![7u8; MAX_VIDEO_DATA];
        let cut: Vec<(FragmentHeader, &[u8])> = video::fragments(1, true, &unit)
            .expect("fragments")
            .collect();
        assert_eq!(cut.len(), 1);
        assert_eq!(
            HEADER_LEN + VIDEO_HEADER_LEN + cut[0].1.len() + TAG_LEN,
            MAX_DATAGRAM
        );
        relay.send_fragment(77, 40, 960, cut[0].0, cut[0].1).await;

        let delivered = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("a unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, unit);

        // One byte more and the receive path drops it before the cipher.
        let oversized = vec![7u8; MAX_VIDEO_DATA + 1];
        relay
            .send_fragment(77, 41, 1_920, fragment(2, 0, 1, true), &oversized)
            .await;
        wait_for(|| engine.stats().rejected >= 1).await;
        assert_eq!(engine.video_stats().frames, 1);
        engine.close().await;
    }

    #[tokio::test]
    async fn keyframe_requests_reach_the_sharer_and_coalesce() {
        let (relay, engine) = Relay::start(11).await;
        let sender = engine.sender();
        assert!(!sender.take_keyframe_request());

        for seq in 0..2u64 {
            relay
                .send(
                    &Header {
                        kind: PacketType::KeyframeRequest,
                        marker: false,
                        ssrc: 77,
                        seq,
                        ts: 0,
                    },
                    &11u32.to_be_bytes(),
                )
                .await;
        }
        wait_for(|| engine.stats().keyframe_requests_received >= 2).await;

        assert!(sender.take_keyframe_request(), "the request was lost");
        assert!(!sender.take_keyframe_request(), "one request, one keyframe");
        assert_eq!(engine.stats().rejected, 0);
        engine.close().await;
    }

    #[tokio::test]
    async fn share_audio_from_the_watched_sharer_reaches_the_playout() {
        let (relay, engine) = Relay::start(11).await;
        engine.watch(Some(77));

        let mut encoder = StereoEncoder::new().expect("encoder");
        let mut tone = Tone::new(440.0, 0.5);
        let mut left = [0.0f32; FRAME_SAMPLES];
        let mut pcm = [0.0f32; STEREO_FRAME_SAMPLES];
        let mut frame = [0u8; 1156];
        tone.fill(&mut left);
        for (pair, sample) in pcm.as_chunks_mut::<2>().0.iter_mut().zip(left.iter()) {
            pair[0] = *sample;
            pair[1] = *sample;
        }
        let written = encoder.encode(&pcm, &mut frame).expect("encodes");

        relay
            .send(
                &Header {
                    kind: PacketType::ShareAudio,
                    marker: true,
                    ssrc: 77,
                    seq: 30,
                    ts: 960,
                },
                &frame[..written],
            )
            .await;

        let playout = engine.playout();
        wait_for(|| lock(&playout).share_stats().is_some()).await;
        let (ssrc, stats) = lock(&playout).share_stats().expect("the share reports");
        assert_eq!(ssrc, 77);
        assert_eq!(stats.received, 1);
        // A share stream is not a speaker.
        assert!(engine.stats().peers.is_empty());

        // Watching someone else drops it.
        engine.watch(None);
        assert!(lock(&playout).share_stats().is_none());
        engine.close().await;
    }

    #[tokio::test]
    async fn share_audio_timestamps_count_frames_and_not_wall_clock() {
        let (relay, engine) = Relay::start(11).await;
        let sender = engine.sender();
        sender.reset_share_audio_clock();

        let mut stamps = Vec::new();
        for index in 0..3 {
            // Three times what the frames are worth: the share sends its audio
            // beside the video, so the wall clock says nothing about how much
            // audio went out.
            tokio::time::sleep(Duration::from_millis(60)).await;
            sender
                .send_share_audio(b"share", index == 0)
                .expect("sends a share frame");
            let (header, _) = relay
                .recv(PacketType::ShareAudio, Duration::from_secs(2))
                .await
                .expect("the relay receives the frame");
            stamps.push(header.ts);
        }

        let frame = FRAME_SAMPLES as u32;
        assert_eq!(stamps[1].wrapping_sub(stamps[0]), frame);
        assert_eq!(stamps[2].wrapping_sub(stamps[1]), frame);

        // A real silence is the one thing that moves the clock further.
        sender.skip_share_audio(10);
        sender
            .send_share_audio(b"share", true)
            .expect("sends a share frame");
        let (header, _) = relay
            .recv(PacketType::ShareAudio, Duration::from_secs(2))
            .await
            .expect("the relay receives the frame");
        assert_eq!(header.ts.wrapping_sub(stamps[2]), 11 * frame);

        engine.close().await;
    }

    #[tokio::test]
    async fn a_camera_unit_goes_out_on_its_own_type_beside_the_share() {
        let (relay, engine) = Relay::start(11).await;
        let sender = engine.sender();
        let unit: Vec<u8> = (0..2_000u32).map(|index| index as u8).collect();

        assert_eq!(
            sender.send_video(1, true, &unit).expect("the share sends"),
            2
        );
        assert_eq!(
            sender
                .send_camera_video(1, true, &unit)
                .expect("the camera sends"),
            2
        );

        let mut share_seqs = Vec::new();
        for index in 0..2u16 {
            let (header, payload) = relay
                .recv(PacketType::Video, Duration::from_secs(1))
                .await
                .expect("a share fragment arrives");
            let (fragment, _) = FragmentHeader::decode(&payload).expect("a fragment header");
            assert_eq!(fragment.index, index);
            share_seqs.push(header.seq);
        }

        let mut camera_seqs = Vec::new();
        let mut rejoined = Vec::new();
        for index in 0..2u16 {
            let (header, payload) = relay
                .recv(PacketType::CameraVideo, Duration::from_secs(1))
                .await
                .expect("a camera fragment arrives");
            assert_eq!(header.ssrc, 11);
            assert!(!header.marker);
            let (fragment, data) = FragmentHeader::decode(&payload).expect("a fragment header");
            assert_eq!(fragment.frame_id, 1);
            assert_eq!(fragment.index, index);
            assert_eq!(fragment.count, 2);
            assert!(fragment.keyframe);
            camera_seqs.push(header.seq);
            rejoined.extend_from_slice(data);
        }
        assert_eq!(rejoined, unit);

        // One counter for the session: the camera's numbers follow the share's
        // rather than starting a second sequence and reusing its nonces.
        assert!(share_seqs[1] > share_seqs[0], "{share_seqs:?}");
        assert!(
            camera_seqs[0] > share_seqs[1],
            "{camera_seqs:?} does not follow {share_seqs:?}"
        );
        assert!(camera_seqs[1] > camera_seqs[0], "{camera_seqs:?}");

        assert!(
            matches!(
                sender.send_camera_video(2, false, &[]),
                Err(EngineError::Packet(PacketError::TooShort))
            ),
            "an empty unit is not a frame"
        );
        engine.close().await;
    }

    #[tokio::test]
    async fn watching_a_camera_asks_its_owner_for_a_keyframe() {
        let (relay, engine) = Relay::start(11).await;
        let _units = engine.watch_camera(77);

        let (header, payload) = relay
            .recv(
                PacketType::CameraKeyframeRequest,
                Duration::from_millis(100),
            )
            .await
            .expect("a camera keyframe request arrives");
        assert_eq!(header.ssrc, 11);
        assert_eq!(payload, 77u32.to_be_bytes());
        assert_eq!(
            engine.camera_stats(),
            vec![(
                77,
                VideoStats {
                    keyframe_requests: 1,
                    ..VideoStats::default()
                }
            )]
        );
        // The share's stream is a different thing entirely.
        assert_eq!(engine.video_stats(), VideoStats::default());

        engine.unwatch_camera(77);
        assert!(engine.camera_stats().is_empty());
        // An ssrc nobody watches is a no-op, both ways.
        engine.unwatch_camera(77);
        engine.unwatch_all_cameras();
        assert!(engine.camera_stats().is_empty());
        engine.close().await;
    }

    #[tokio::test]
    async fn two_cameras_are_reassembled_independently() {
        let (relay, engine) = Relay::start(11).await;
        let mut first = engine.watch_camera(77);
        let mut second = engine.watch_camera(88);

        let one: Vec<u8> = (0..2_000u32).map(|index| index as u8).collect();
        let two: Vec<u8> = (0..2_000u32).map(|index| (index as u8) ^ 0xFF).collect();
        let cut_one: Vec<(FragmentHeader, &[u8])> = video::fragments(1, true, &one)
            .expect("fragments")
            .collect();
        let cut_two: Vec<(FragmentHeader, &[u8])> = video::fragments(1, true, &two)
            .expect("fragments")
            .collect();
        assert_eq!(cut_one.len(), 2);
        assert_eq!(cut_two.len(), 2);

        // Interleaved, and each frame only completes on its own halves.
        relay
            .send_camera_fragment(77, 10, 960, cut_one[0].0, cut_one[0].1)
            .await;
        relay
            .send_camera_fragment(88, 11, 960, cut_two[0].0, cut_two[0].1)
            .await;
        relay
            .send_camera_fragment(88, 12, 960, cut_two[1].0, cut_two[1].1)
            .await;
        relay
            .send_camera_fragment(77, 13, 960, cut_one[1].0, cut_one[1].1)
            .await;

        let delivered = tokio::time::timeout(Duration::from_secs(2), second.recv())
            .await
            .expect("88's unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, two);
        let delivered = tokio::time::timeout(Duration::from_secs(2), first.recv())
            .await
            .expect("77's unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, one);

        let stats = engine.stats();
        assert_eq!(stats.cameras.len(), 2);
        assert_eq!(stats.cameras[0].0, 77);
        assert_eq!(stats.cameras[0].1.frames, 1);
        assert_eq!(stats.cameras[0].1.fragments, 2);
        assert_eq!(stats.cameras[0].1.dropped, 0);
        assert_eq!(stats.cameras[1].0, 88);
        assert_eq!(stats.cameras[1].1.frames, 1);
        assert_eq!(stats.cameras[1].1.fragments, 2);
        assert_eq!(stats.cameras[1].1.dropped, 0);
        // None of it went anywhere near the screen share.
        assert_eq!(stats.video, VideoStats::default());
        assert_eq!(stats.camera_ignored, 0);
        engine.close().await;
    }

    #[tokio::test]
    async fn camera_video_from_an_unwatched_ssrc_is_dropped() {
        let (relay, engine) = Relay::start(11).await;
        let mut units = engine.watch_camera(77);

        relay
            .send_camera_fragment(99, 1, 960, fragment(1, 0, 1, true), b"nobody watches this")
            .await;
        wait_for(|| engine.stats().camera_ignored >= 1).await;

        relay
            .send_camera_fragment(77, 2, 1_920, fragment(1, 0, 1, true), b"watched")
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("the watched camera's unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, b"watched");

        let stats = engine.stats();
        assert_eq!(stats.camera_ignored, 1);
        assert_eq!(stats.rejected, 0);
        // The share's counter is for the share's own strays.
        assert_eq!(stats.ignored, 0);
        assert_eq!(stats.cameras.len(), 1);
        assert_eq!(stats.cameras[0].1.fragments, 1);
        engine.close().await;
    }

    #[tokio::test]
    async fn watching_a_camera_again_resets_only_that_stream() {
        let (relay, engine) = Relay::start(11).await;
        let mut first = engine.watch_camera(77);
        let mut kept = engine.watch_camera(88);

        relay
            .send_camera_fragment(77, 1, 960, fragment(1, 0, 1, true), b"before")
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), first.recv())
            .await
            .expect("77's unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, b"before");

        relay
            .send_camera_fragment(88, 2, 960, fragment(1, 0, 1, true), b"other")
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), kept.recv())
            .await
            .expect("88's unit arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, b"other");

        let mut again = engine.watch_camera(77);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), first.recv())
                .await
                .expect("the replaced channel was left open")
                .is_none(),
            "the replaced channel still delivered"
        );

        let stats = engine.camera_stats();
        assert_eq!(
            stats[0],
            (
                77,
                VideoStats {
                    keyframe_requests: 1,
                    ..VideoStats::default()
                }
            ),
            "77's history survived the second watch"
        );
        assert_eq!(stats[1].0, 88);
        assert_eq!(stats[1].1.frames, 1, "88's history was reset too");
        assert_eq!(stats[1].1.fragments, 1);

        // Frame 1 again: the fresh depacketizer has no last decision to judge
        // it a straggler against.
        relay
            .send_camera_fragment(77, 3, 1_920, fragment(1, 0, 1, true), b"after")
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), again.recv())
            .await
            .expect("the restarted stream delivers")
            .expect("the channel is open");
        assert_eq!(delivered.data, b"after");
        engine.close().await;
    }

    #[tokio::test]
    async fn a_keyframe_request_only_ever_raises_its_own_streams_flag() {
        let (relay, engine) = Relay::start(11).await;
        let sender = engine.sender();
        assert!(!sender.take_keyframe_request());
        assert!(!sender.take_camera_keyframe_request());

        relay
            .send(
                &Header {
                    kind: PacketType::CameraKeyframeRequest,
                    marker: false,
                    ssrc: 77,
                    seq: 1,
                    ts: 0,
                },
                &11u32.to_be_bytes(),
            )
            .await;
        wait_for(|| engine.stats().camera_keyframe_requests_received >= 1).await;
        assert!(
            !sender.take_keyframe_request(),
            "a camera request cost the share a keyframe"
        );
        assert!(
            sender.take_camera_keyframe_request(),
            "the camera request was lost"
        );
        assert!(
            !sender.take_camera_keyframe_request(),
            "one request, one keyframe"
        );

        relay
            .send(
                &Header {
                    kind: PacketType::KeyframeRequest,
                    marker: false,
                    ssrc: 77,
                    seq: 2,
                    ts: 0,
                },
                &11u32.to_be_bytes(),
            )
            .await;
        wait_for(|| engine.stats().keyframe_requests_received >= 1).await;
        assert!(
            !sender.take_camera_keyframe_request(),
            "a share request cost the camera a keyframe"
        );
        assert!(sender.take_keyframe_request(), "the share request was lost");

        let stats = engine.stats();
        assert_eq!(stats.keyframe_requests_received, 1);
        assert_eq!(stats.camera_keyframe_requests_received, 1);
        assert_eq!(stats.rejected, 0);
        engine.close().await;
    }

    #[tokio::test]
    async fn camera_media_never_reaches_the_playout() {
        let (relay, engine) = Relay::start(11).await;
        let mut units = engine.watch_camera(77);

        relay
            .send_camera_fragment(
                77,
                1,
                960,
                fragment(1, 0, 1, true),
                b"a picture, not a voice",
            )
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), units.recv())
            .await
            .expect("a unit arrives")
            .expect("the channel is open");
        assert!(delivered.keyframe);

        // A camera is neither a speaker nor a share.
        assert!(engine.stats().peers.is_empty());
        let playout = engine.playout();
        assert!(lock(&playout).stats().is_empty());
        assert!(lock(&playout).share_stats().is_none());
        engine.close().await;
    }

    #[tokio::test]
    async fn a_camera_whose_decoder_went_away_is_forgotten() {
        let (relay, engine) = Relay::start(11).await;
        let units = engine.watch_camera(77);
        let mut kept = engine.watch_camera(88);
        drop(units);

        relay
            .send_camera_fragment(
                77,
                1,
                960,
                fragment(1, 0, 1, true),
                b"nobody left to decode",
            )
            .await;
        wait_for(|| engine.camera_stats().len() == 1).await;
        assert_eq!(engine.camera_stats()[0].0, 88);

        // The loop carried on: the camera still being watched arrives.
        relay
            .send_camera_fragment(88, 2, 960, fragment(1, 0, 1, true), b"still watched")
            .await;
        let delivered = tokio::time::timeout(Duration::from_secs(2), kept.recv())
            .await
            .expect("88 still arrives")
            .expect("the channel is open");
        assert_eq!(delivered.data, b"still watched");
        engine.close().await;
    }

    /// The code a full interface queue answers with, which is what the retry
    /// exists for.
    #[cfg(unix)]
    const ENOBUFS: i32 = libc::ENOBUFS;
    #[cfg(windows)]
    const ENOBUFS: i32 = 10055;

    #[test]
    fn a_transient_refusal_is_retried_until_the_datagram_goes_out() {
        let mut pauses = Vec::new();
        let mut attempts = 0;
        let sent = retry_send(
            || {
                attempts += 1;
                if attempts <= 2 {
                    Err(std::io::Error::from_raw_os_error(ENOBUFS))
                } else {
                    Ok(10)
                }
            },
            |pause| pauses.push(pause),
        );

        assert_eq!(sent.expect("the third attempt goes out"), 10);
        assert_eq!(attempts, 3);
        assert_eq!(pauses, vec![RETRY_PAUSE; 2]);
    }

    #[test]
    fn a_queue_that_never_drains_gives_up_after_the_last_retry() {
        let mut pauses = Vec::new();
        let mut attempts = 0;
        let sent = retry_send(
            || {
                attempts += 1;
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            },
            |pause| pauses.push(pause),
        );

        let error = sent.expect_err("a socket that never takes it");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        // One attempt, then one more for every retry.
        assert_eq!(attempts, MAX_SEND_RETRIES + 1);
        assert_eq!(pauses.len() as u32, MAX_SEND_RETRIES);
        // 40 ms of waiting at most, whatever the socket says.
        assert_eq!(
            pauses.iter().sum::<Duration>(),
            Duration::from_millis(u64::from(MAX_SEND_RETRIES))
        );
    }

    #[test]
    fn an_error_waiting_cannot_fix_comes_straight_back() {
        let mut pauses = Vec::new();
        let mut attempts = 0;
        let sent = retry_send(
            || {
                attempts += 1;
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            },
            |pause| pauses.push(pause),
        );

        let error = sent.expect_err("a refusal no pause changes");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, 1);
        assert!(pauses.is_empty(), "a permanent error was waited on");
    }

    #[test]
    fn the_media_socket_asks_for_wide_kernel_buffers() {
        let relay: SocketAddr = "127.0.0.1:9".parse().expect("an address");
        let plain = Socket::new(Domain::IPV4, Type::DGRAM, None).expect("a socket");
        let default_recv = plain.recv_buffer_size().expect("a receive buffer");
        let default_send = plain.send_buffer_size().expect("a send buffer");

        let (_socket, recv_buffer, send_buffer) = bind_socket(relay).expect("binds");
        assert!(
            recv_buffer >= default_recv,
            "{recv_buffer} < {default_recv}"
        );
        assert!(
            send_buffer >= default_send,
            "{send_buffer} < {default_send}"
        );
    }
}
