//! The machine's own playout, as a second PipeWire stream.
//!
//! The portal's remote carries the cast and nothing else, so this connects to
//! the session's own PipeWire socket instead and asks for the default sink's
//! monitor: everything the machine is playing, Vorcall's own voice included.

use std::rc::Rc;

use libspa::param::ParamType;
use libspa::param::audio::{AudioFormat, AudioInfoRaw};
use libspa::param::format::{MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::pod::{Object, Pod};
use libspa::utils::{Direction, SpaTypes};
use pipewire::context::ContextRc;
use pipewire::properties::properties;
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};

use super::Ending;
use crate::{AudioChunk, CaptureEvent};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

/// The running sink monitor. Both halves have to stay alive, and the stream goes
/// first, as on the video side.
pub(super) struct Monitor {
    _stream: StreamRc,
    _listener: StreamListener<State>,
}

struct State {
    ending: Rc<Ending>,
    format: Option<AudioInfoRaw>,
}

/// Taps the default sink's monitor, or says why it could not and lets the share
/// go on without sound.
pub(super) fn connect(context: &ContextRc, ending: Rc<Ending>) -> Option<Monitor> {
    match tap(context, ending) {
        Ok(monitor) => Some(monitor),
        Err(reason) => {
            tracing::warn!(%reason, "sharing the screen without its audio");
            None
        }
    }
}

fn tap(context: &ContextRc, ending: Rc<Ending>) -> Result<Monitor, String> {
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("cannot reach PipeWire: {error}"))?;
    let stream = StreamRc::new(
        core,
        "vorcall-screen-audio",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            // What turns a plain capture into a tap on the sink rather than on
            // the microphone.
            *pipewire::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .map_err(|error| format!("cannot create the audio stream: {error}"))?;

    let listener = stream
        .add_local_listener_with_user_data(State {
            ending,
            format: None,
        })
        .state_changed(|_, _, _, new| {
            if let StreamState::Error(error) = new {
                tracing::warn!(%error, "the screen share lost its audio");
            }
        })
        .param_changed(|_, state, id, param| state.negotiated(id, param))
        .process(|stream, state| state.chunk(stream))
        .register()
        .map_err(|error| format!("cannot listen to the audio stream: {error}"))?;

    let format = format_param().ok_or_else(|| "cannot describe the audio format".to_string())?;
    let format =
        Pod::from_bytes(&format).ok_or_else(|| "cannot describe the audio format".to_string())?;
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|error| format!("cannot connect the audio stream: {error}"))?;

    Ok(Monitor {
        _stream: stream,
        _listener: listener,
    })
}

impl State {
    fn negotiated(&mut self, id: u32, param: Option<&Pod>) {
        let Some(param) = param else {
            return;
        };
        if id != ParamType::Format.as_raw() {
            return;
        }
        let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
            return;
        };
        if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
            return;
        }
        let mut format = AudioInfoRaw::new();
        if format.parse(param).is_err() {
            return;
        }

        tracing::debug!(
            rate = format.rate(),
            channels = format.channels(),
            "negotiated a share audio format"
        );
        self.format = Some(format);
    }

    fn chunk(&mut self, stream: &Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let Some(format) = self.format else {
            return;
        };
        let (rate, channels) = (format.rate(), format.channels());
        if rate == 0 || channels == 0 {
            return;
        }

        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let (size, offset) = (data.chunk().size() as usize, data.chunk().offset() as usize);
        let Some(mapped) = data.data() else {
            return;
        };
        let Some(samples) = mapped.get(offset..offset + size) else {
            return;
        };

        let interleaved: Vec<f32> = samples
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(f32::from_le_bytes)
            .collect();
        if interleaved.is_empty() {
            return;
        }

        self.ending.send(CaptureEvent::Audio(AudioChunk {
            sample_rate: rate,
            channels: channels as u16,
            interleaved,
        }));
    }
}

/// The one layout the rest of the pipeline speaks. The graph resamples and remixes
/// to reach it, so a 44.1 kHz sink is still two channels of 48 kHz float here.
fn format_param() -> Option<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS);

    super::serialise(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    })
}
