//! The threads and blocking work the interface must never do itself: the audio
//! devices, the screen-share pipeline and decoder, the image cache, desktop
//! notifications and the chime.
//!
//! Every worker is reached the same way — a handle that never blocks goes in, an
//! event stream comes out — and every handle's `Drop` is what stops its thread.

pub mod audio;
pub mod images;
pub mod notify;
pub mod share;
pub mod voice;
