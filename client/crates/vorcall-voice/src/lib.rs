//! Client-side media engine for voice rooms.
//!
//! The pieces stack in one direction, each layer only knowing the one below it:
//!
//! - [`packet`] — the 19-byte cleartext header that prefixes every datagram.
//! - [`crypto`] — ChaCha20-Poly1305 over that header (as AAD) and the payload.
//! - [`codec`] — Opus at 48 kHz mono, 20 ms frames, with packet loss concealment.
//! - [`jitter`] — one adaptive buffer per remote speaker, reordering and
//!   absorbing network jitter, reporting losses to the codec so it can conceal.
//! - [`playout`] — every remote speaker decoded and summed into one frame.
//! - [`video`] — screen-share and camera access units cut into datagrams and
//!   reassembled, one stream at a time.
//! - [`engine`] — the UDP socket, the send path, keepalive pings and statistics.
//! - [`cleanup`] — on the capture side instead: echo cancellation, noise
//!   suppression and automatic gain, applied before anything else sees a frame.
//!
//! [`gate`] sits on the capture side, deciding which frames are worth sending;
//! [`sound`] is the container a soundpad clip is stored in and [`sfx`] the
//! synthesized interface motifs, neither of which ever reaches the wire;
//! [`tone`] is a test/diagnostic source; nothing here touches an audio device,
//! a window, or the network beyond the single UDP socket the engine owns.

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz, mono.
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_MS: u64 = 20;

pub mod cleanup;
pub mod codec;
pub mod crypto;
pub mod engine;
pub mod gate;
pub mod jitter;
pub mod packet;
pub mod playout;
pub mod sfx;
pub mod sound;
pub mod tone;
pub mod video;

pub use cleanup::{CleanupSettings, EchoMetrics, InputCleanup, ShareCleanup};
pub use codec::{STEREO_FRAME_SAMPLES, StereoDecoder, StereoEncoder};
pub use engine::{FrameSender, Link, MediaConfig, MediaEngine, Stats};
pub use gate::{GateDecision, NoiseGate};
pub use playout::{PeerStats, Playout};
pub use sfx::Sfx;
pub use sound::{SoundClip, SoundError};
pub use video::{AccessUnit, Depacketizer, FragmentHeader, VideoStats};
