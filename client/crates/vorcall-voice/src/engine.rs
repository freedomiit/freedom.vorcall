//! The UDP side of a voice room: one socket to the relay, a send path safe to
//! call from the audio thread, keepalive pings for reachability and round-trip
//! time, and the receive task that feeds [`Playout`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::net::{UdpSocket, lookup_host};
use tokio::task::JoinHandle;

use crate::SAMPLE_RATE;
use crate::crypto::MediaCipher;
use crate::jitter::Incoming;
use crate::packet::{Header, MAX_DATAGRAM, MIN_DATAGRAM, PONG_SEQ_BIT, PacketType};
use crate::playout::{PeerStats, Playout};

pub const PING_INTERVAL: Duration = Duration::from_secs(5);
pub const LINK_TIMEOUT: Duration = Duration::from_secs(15);

/// Pongs for pings older than this many rounds are no longer matched.
const PENDING_PINGS: usize = 8;
/// One MTU's worth of slack, so an oversized datagram is seen and rejected
/// rather than silently truncated into something that looks valid.
const RECV_BUFFER: usize = 2048;

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
    pub rtt_last_ms: Option<f64>,
    pub rtt_min_ms: Option<f64>,
    pub rtt_avg_ms: Option<f64>,
    pub rtt_max_ms: Option<f64>,
    pub rtt_samples: u32,
    pub link: Link,
    pub peers: Vec<(u32, PeerStats)>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("cannot resolve the relay: {0}")]
    Resolve(String),
    #[error("cannot bind the media socket: {0}")]
    Bind(std::io::Error),
    #[error("cannot send a media datagram: {0}")]
    Send(std::io::Error),
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
    /// Audio and pings draw from one counter so a nonce is never reused.
    seq: AtomicU64,
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
}

fn send(socket: &UdpSocket, shared: &Shared, datagram: &[u8]) -> Result<(), EngineError> {
    if datagram.len() < MIN_DATAGRAM {
        return Err(EngineError::Send(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "sealing produced no datagram",
        )));
    }
    match socket.try_send(datagram) {
        Ok(sent) => {
            shared.packets_sent.fetch_add(1, Ordering::Relaxed);
            shared.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
            Ok(())
        }
        Err(error) => Err(EngineError::Send(error)),
    }
}

pub struct MediaEngine {
    sender: FrameSender,
    playout: Arc<Mutex<Playout>>,
    shared: Arc<Shared>,
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

        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(EngineError::Bind)?;
        // Connecting the socket makes the kernel drop datagrams from anyone but
        // the relay, so the receive path never sees off-path traffic.
        socket.connect(relay).await.map_err(EngineError::Bind)?;
        let socket = Arc::new(socket);

        let cipher = Arc::new(MediaCipher::new(&config.key));
        let shared = Arc::new(Shared {
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            started: Instant::now(),
            pending_pings: Mutex::new(VecDeque::new()),
            rtt: Mutex::new(Rtt::default()),
        });
        let playout = Arc::new(Mutex::new(Playout::new()));

        tracing::debug!(%relay, ssrc = config.ssrc, "media engine connected");

        let receive_task = tokio::spawn(receive_loop(
            Arc::clone(&socket),
            Arc::clone(&cipher),
            Arc::clone(&shared),
            Arc::clone(&playout),
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
            receive_task,
            ping_task,
        })
    }

    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }

    pub fn playout(&self) -> Arc<Mutex<Playout>> {
        Arc::clone(&self.playout)
    }

    pub fn stats(&self) -> Stats {
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
            rtt_last_ms: rtt.last_ms,
            rtt_min_ms: rtt.min_ms,
            rtt_avg_ms: (rtt.samples > 0).then(|| rtt.sum_ms / f64::from(rtt.samples)),
            rtt_max_ms: rtt.max_ms,
            rtt_samples: rtt.samples,
            link,
            peers: lock(&self.playout).stats(),
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

async fn receive_loop(
    socket: Arc<UdpSocket>,
    cipher: Arc<MediaCipher>,
    shared: Arc<Shared>,
    playout: Arc<Mutex<Playout>>,
    ssrc: u32,
) {
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
            PacketType::Pong => record_pong(&shared, &header, &payload),
            PacketType::Ping => {}
        }
    }
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
    use crate::FRAME_SAMPLES;
    use crate::codec::Encoder;
    use crate::packet::{HEADER_LEN, TAG_LEN, VERSION};
    use crate::tone::Tone;

    const KEY: [u8; 32] = [3u8; 32];

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
}
