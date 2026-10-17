//! The machine's playout, or one application's, as a second PipeWire stream.
//!
//! The portal's remote carries the cast and nothing else, so this connects to
//! the session's own PipeWire socket instead, with one of two taps. The default
//! sink's monitor is everything the machine is playing, Vorcall's own voice
//! included. One application's streams are linked into a capture node of our
//! own that nothing else feeds, so Vorcall's own voice is never in it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use libspa::param::ParamType;
use libspa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use libspa::param::format::{MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::pod::{Object, Pod};
use libspa::utils::{Direction, SpaTypes};
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::properties::{PropertiesBox, properties};
use pipewire::registry::{Listener, RegistryRc};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};

use super::Ending;
use super::apps::{Linker, follow};
use crate::{AudioApp, AudioChunk, AudioMode, CaptureEvent};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

/// Numbers the app taps this process opens, so each capture node has a name of
/// its own for its linker to find it by.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// A running tap. Whichever kind, the stream goes before its listener, as on
/// the video side.
pub(super) struct Tap {
    kind: Kind,
}

enum Kind {
    Monitor {
        _stream: StreamRc,
        _listener: StreamListener<State>,
    },
    /// In drop order: the registry stops feeding the linker, the linker takes
    /// its links down, and only then do the registry and the stream go.
    App {
        _follow: Listener,
        _linker: Rc<RefCell<Linker>>,
        _registry: RegistryRc,
        _stream: StreamRc,
        _listener: StreamListener<State>,
    },
}

impl Tap {
    /// Whether what this tap hears includes Vorcall's own playout.
    pub(super) fn mode(&self) -> AudioMode {
        match self.kind {
            Kind::Monitor { .. } => AudioMode::IncludesOwnPlayout,
            Kind::App { .. } => AudioMode::Excluded,
        }
    }
}

struct State {
    ending: Rc<Ending>,
    format: Option<AudioInfoRaw>,
}

/// Taps `app`'s streams when one is named and the default sink's monitor
/// otherwise, or says why it could not and lets the share go on without sound.
pub(super) fn connect(
    context: &ContextRc,
    app: Option<AudioApp>,
    ending: Rc<Ending>,
) -> Option<Tap> {
    let tapped = match app {
        Some(app) => tap_app(context, app, ending),
        None => tap(context, ending),
    };
    match tapped {
        Ok(tap) => Some(tap),
        Err(reason) => {
            tracing::warn!(%reason, "sharing the screen without its audio");
            None
        }
    }
}

fn tap(context: &ContextRc, ending: Rc<Ending>) -> Result<Tap, String> {
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("cannot reach PipeWire: {error}"))?;
    let (stream, listener) = open(
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
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        false,
        ending,
    )?;

    Ok(Tap {
        kind: Kind::Monitor {
            _stream: stream,
            _listener: listener,
        },
    })
}

fn tap_app(context: &ContextRc, app: AudioApp, ending: Rc<Ending>) -> Result<Tap, String> {
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("cannot reach PipeWire: {error}"))?;
    let own_name = format!(
        "vorcall-share-app-audio-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let (stream, listener) = open(
        core.clone(),
        "vorcall-screen-app-audio",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::NODE_NAME => own_name.as_str(),
            // Linked by hand and by nothing else: left to the session manager,
            // a capture stream would be handed the microphone.
            *pipewire::keys::NODE_AUTOCONNECT => "false",
        },
        StreamFlags::MAP_BUFFERS,
        true,
        ending,
    )?;

    let registry = core
        .get_registry_rc()
        .map_err(|error| format!("cannot read the PipeWire registry: {error}"))?;
    let linker = Linker::new(core, app, own_name);
    let following = follow(&registry, &linker);

    Ok(Tap {
        kind: Kind::App {
            _follow: following,
            _linker: linker,
            _registry: registry,
            _stream: stream,
            _listener: listener,
        },
    })
}

/// Builds one capture stream in the layout the rest of the pipeline speaks and
/// connects it, its chunks going out through `ending`.
fn open(
    core: CoreRc,
    name: &str,
    properties: PropertiesBox,
    flags: StreamFlags,
    positioned: bool,
    ending: Rc<Ending>,
) -> Result<(StreamRc, StreamListener<State>), String> {
    let stream = StreamRc::new(core, name, properties)
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

    let format =
        format_param(positioned).ok_or_else(|| "cannot describe the audio format".to_string())?;
    let format =
        Pod::from_bytes(&format).ok_or_else(|| "cannot describe the audio format".to_string())?;
    stream
        .connect(Direction::Input, None, flags, &mut [format])
        .map_err(|error| format!("cannot connect the audio stream: {error}"))?;

    Ok((stream, listener))
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
///
/// `positioned` names the two channels front left and front right, which is what
/// gives the app tap's node the FL and FR input ports its linker routes to. The
/// monitor leaves them unnamed.
fn format_param(positioned: bool) -> Option<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS);
    if positioned {
        let mut position = [0; MAX_CHANNELS];
        position[0] = libspa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = libspa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(position);
    }

    super::serialise(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    })
}
