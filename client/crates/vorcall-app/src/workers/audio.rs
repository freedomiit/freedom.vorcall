//! The optional chime that goes with a desktop notification, and the mixer an
//! interface motif falls back to when there is no voice session to play it
//! through.
//!
//! rodio is compiled without decoders, so there is no file to play: the chime is
//! two short synthesized notes, and a motif arrives as samples somebody else
//! synthesized.

use std::num::NonZero;
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::source::{SineWave, Source as _};

use vorcall_voice::SAMPLE_RATE;

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

    /// One interface motif, when there is no voice session to play it through.
    /// Fire-and-forget like the chime: the mixer owns it until it runs out.
    pub fn play(&self, samples: Vec<f32>, gain: f32) {
        const MONO: rodio::ChannelCount = NonZero::new(1).unwrap();
        const RATE: rodio::SampleRate = NonZero::new(SAMPLE_RATE).unwrap();

        self.mixer
            .add(SamplesBuffer::new(MONO, RATE, samples).amplify(gain));
    }
}
