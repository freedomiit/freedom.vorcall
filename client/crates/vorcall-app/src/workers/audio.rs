//! The optional chime that goes with a desktop notification.
//!
//! rodio is compiled without decoders, so there is no file to play: the sound is
//! two short synthesized notes.

use std::time::Duration;

use rodio::source::{SineWave, Source as _};

/// Quiet enough to sit under whatever the desktop plays for the notification.
const VOLUME: f32 = 0.15;

/// The open output device. Dropping the [`rodio::MixerDeviceSink`] silences the
/// mixer, so the sink has to outlive every sound handed to it — which is why
/// `App` keeps this whole struct.
pub struct Audio {
    _sink: rodio::MixerDeviceSink,
    mixer: rodio::mixer::Mixer,
}

pub fn open() -> Result<Audio, rodio::DeviceSinkError> {
    let mut sink = rodio::DeviceSinkBuilder::open_default_sink()?;
    // Left on, rodio prints to stderr when the sink is dropped at exit.
    sink.log_on_drop(false);
    let mixer = sink.mixer().clone();

    Ok(Audio { _sink: sink, mixer })
}

impl Audio {
    /// Two rising notes, played fire-and-forget: the mixer owns them until they
    /// run out.
    pub fn chime(&self) {
        self.mixer.add(
            SineWave::new(880.0)
                .take_duration(Duration::from_millis(90))
                .amplify(VOLUME)
                .fade_in(Duration::from_millis(5))
                .fade_out(Duration::from_millis(40)),
        );
        self.mixer.add(
            SineWave::new(1175.0)
                .take_duration(Duration::from_millis(160))
                .amplify(VOLUME)
                .fade_in(Duration::from_millis(5))
                .fade_out(Duration::from_millis(80))
                .delay(Duration::from_millis(90)),
        );
    }
}
