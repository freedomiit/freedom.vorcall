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
//! - [`engine`] — the UDP socket, the send path, keepalive pings and statistics.
//!
//! [`tone`] is a test/diagnostic source; nothing here touches an audio device,
//! a window, or the network beyond the single UDP socket the engine owns.

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms at 48 kHz, mono.
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_MS: u64 = 20;

pub mod codec;
pub mod crypto;
pub mod engine;
pub mod jitter;
pub mod packet;
pub mod playout;
pub mod tone;

pub use engine::{FrameSender, Link, MediaConfig, MediaEngine, Stats};
pub use playout::{PeerStats, Playout};
