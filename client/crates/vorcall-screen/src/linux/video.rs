//! The screen's own PipeWire stream: what to ask the compositor for, and how to
//! get every frame it answers with out of its buffer before it is recycled.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libspa::param::ParamType;
use libspa::param::format::{FormatProperties, MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::param::video::{VideoFormat, VideoInfoRaw};
use libspa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value, object, property};
use libspa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes};
use pipewire::core::CoreRc;
use pipewire::properties::properties;
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};

use super::portal::Cast;
use super::{Ending, Report};
use crate::preset::FrameRate;
use crate::{AudioMode, CaptureEvent, CaptureRequest, Unavailable, VideoFrame};

/// No compositor hands over a frame wider than this, and the range has to be
/// bounded for the format to be a range at all.
const LARGEST: Rectangle = Rectangle {
    width: 16_384,
    height: 16_384,
};

/// The running screen stream. Both halves have to stay alive: dropping the
/// listener stops the callbacks, dropping the stream stops the capture. The
/// stream goes first, so no callback can reach the listener's data after it.
pub(super) struct Screen {
    _stream: StreamRc,
    _listener: StreamListener<State>,
}

struct State {
    ending: Rc<Ending>,
    /// Answers `Capturer::start`, once, with whichever of the two arrives first:
    /// the stream running, or the stream failing.
    reports: Option<mpsc::Sender<Report>>,
    audio: Option<AudioMode>,
    format: Option<VideoInfoRaw>,
    started: bool,
    interval: Duration,
    previous: Option<Instant>,
    /// A buffer we cannot read is worth saying once, not once per frame.
    complained: bool,
}

/// Connects to the node the portal picked, asking for a format this process can
/// read on the CPU.
pub(super) fn connect(
    core: &CoreRc,
    cast: &Cast,
    request: &CaptureRequest,
    audio: Option<AudioMode>,
    ending: Rc<Ending>,
    reports: mpsc::Sender<Report>,
) -> Result<Screen, Unavailable> {
    let stream = StreamRc::new(
        core.clone(),
        "vorcall-screen",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|error| Unavailable::Failed(format!("cannot create the screen stream: {error}")))?;

    let listener = stream
        .add_local_listener_with_user_data(State {
            ending,
            reports: Some(reports),
            audio,
            format: None,
            started: false,
            interval: request.fps.interval(),
            previous: None,
            complained: false,
        })
        .state_changed(|_, state, _, new| state.changed(new))
        .param_changed(|stream, state, id, param| state.negotiated(stream, id, param))
        .process(|stream, state| state.frame(stream))
        .register()
        .map_err(|error| {
            Unavailable::Failed(format!("cannot listen to the screen stream: {error}"))
        })?;

    let format = format_param(request.fps, cast.size)
        .ok_or_else(|| Unavailable::Failed("cannot describe the screen format".to_string()))?;
    let format = Pod::from_bytes(&format)
        .ok_or_else(|| Unavailable::Failed("cannot describe the screen format".to_string()))?;
    stream
        .connect(
            Direction::Input,
            Some(cast.node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|error| {
            Unavailable::Failed(format!("cannot connect the screen stream: {error}"))
        })?;

    Ok(Screen {
        _stream: stream,
        _listener: listener,
    })
}

impl State {
    /// The stream is live, so whoever is waiting inside `Capturer::start` may go.
    fn running(&mut self) {
        if let Some(reports) = self.reports.take() {
            let _ = reports.send(Ok(()));
        }
    }

    /// The stream died. Before `start` has answered that is its error and the
    /// consumer never hears of the capture at all; after it, the consumer is the
    /// one who needs to be told.
    fn failed(&mut self, reason: String) {
        match self.reports.take() {
            Some(reports) => {
                let _ = reports.send(Err(Unavailable::Failed(reason)));
                self.ending.abandon();
            }
            None => self.ending.end(reason),
        }
    }

    fn changed(&mut self, state: StreamState) {
        match state {
            StreamState::Streaming => self.running(),
            StreamState::Error(error) => self.failed(format!("the screen stream failed: {error}")),
            // Nothing but the source going away drops a connected stream back to
            // unconnected, so this is the compositor ending the share.
            StreamState::Unconnected => self.failed("the screen share was stopped".to_string()),
            StreamState::Connecting | StreamState::Paused => {}
        }
    }

    fn negotiated(&mut self, stream: &Stream, id: u32, param: Option<&Pod>) {
        let Some(param) = param else {
            return;
        };
        if id != ParamType::Format.as_raw() {
            return;
        }
        let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
            return;
        };
        if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
            return;
        }
        let mut format = VideoInfoRaw::default();
        if format.parse(param).is_err() {
            return;
        }

        let size = format.size();
        tracing::debug!(
            format = ?format.format(),
            width = size.width,
            height = size.height,
            "negotiated a screen capture format"
        );
        self.format = Some(format);
        if !self.started {
            self.started = true;
            self.ending.send(CaptureEvent::Started {
                width: size.width,
                height: size.height,
                audio: self.audio,
            });
        }

        // Said only now, because this is the one moment pw_stream reads buffer
        // parameters: without it a compositor with a GPU path is free to hand
        // over DMA-BUF frames, which this process cannot map.
        if let Some(buffers) = buffers_param().as_deref().and_then(Pod::from_bytes)
            && let Err(error) = stream.update_params(&mut [buffers])
        {
            tracing::debug!(%error, "the screen stream kept its own buffer types");
        }
    }

    fn frame(&mut self, stream: &Stream) {
        // Dequeued first and unconditionally: a buffer only goes back to the
        // compositor when it is dropped, and a frame paced out below still has
        // to be given back.
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let Some(format) = self.format else {
            return;
        };

        let now = Instant::now();
        if self
            .previous
            .is_some_and(|previous| now.duration_since(previous) < self.interval)
        {
            return;
        }

        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let kind = data.type_();
        let (size, stride, offset) = (
            data.chunk().size(),
            data.chunk().stride(),
            data.chunk().offset() as usize,
        );
        if size == 0 {
            return;
        }

        let (width, height) = (format.size().width as usize, format.size().height as usize);
        let row = width * 4;
        // A compositor may pad its rows, and answers with a stride it has not
        // filled in as zero.
        let stride = usize::try_from(stride).unwrap_or(0).max(row);
        if width == 0 || height == 0 {
            return;
        }

        // Decided before the copy, so an answer we cannot use costs no work.
        let swap = match format.format() {
            VideoFormat::BGRx | VideoFormat::BGRA => false,
            VideoFormat::RGBx | VideoFormat::RGBA => true,
            other => {
                self.failed(format!("the compositor sends {other:?} frames"));
                return;
            }
        };

        let Some(mapped) = data.data() else {
            // MAP_BUFFERS and the buffer types answered above should have ruled
            // this out; a compositor that insists on it can never be read here.
            self.failed(format!("the compositor offers only {kind:?} frames"));
            return;
        };

        let mut bgra = Vec::with_capacity(stride * height);
        for line in 0..height {
            let begin = offset + line * stride;
            match mapped.get(begin..begin + stride) {
                Some(line) => bgra.extend_from_slice(line),
                None => {
                    if !self.complained {
                        self.complained = true;
                        tracing::warn!(
                            len = mapped.len(),
                            stride,
                            height,
                            "the compositor's frame is shorter than the format it negotiated"
                        );
                    }
                    return;
                }
            }
        }

        if swap {
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }

        self.previous = Some(now);
        self.running();
        self.ending.send(CaptureEvent::Video(VideoFrame {
            width: width as u32,
            height: height as u32,
            stride,
            bgra,
            captured: now,
        }));
    }
}

/// Every pixel layout this backend can turn into BGRA, at any size the source
/// happens to be. `size` is only the preferred value of the range: the portal
/// measures in the compositor's coordinate space, which on a scaled display is
/// not pixels.
///
/// Nothing here answers the request's `max_size`, because the portal cannot
/// scale a source down; the frame arrives at whatever size it is.
fn format_param(fps: FrameRate, size: Option<(u32, u32)>) -> Option<Vec<u8>> {
    let preferred = size.map_or(
        Rectangle {
            width: 1920,
            height: 1080,
        },
        |(width, height)| Rectangle {
            width: width.clamp(1, LARGEST.width),
            height: height.clamp(1, LARGEST.height),
        },
    );

    super::serialise(object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            preferred,
            Rectangle {
                width: 1,
                height: 1
            },
            LARGEST
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: fps.hz(),
                denom: 1
            },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1_000,
                denom: 1
            }
        ),
    ))
}

/// The memory this process can actually read: a plain mapping, or a memfd the
/// stream maps for us. Anything else — a DMA-BUF the GPU holds — would arrive
/// as a buffer with no address.
fn buffers_param() -> Option<Vec<u8>> {
    let readable = (1 << libspa::sys::SPA_DATA_MemPtr) | (1 << libspa::sys::SPA_DATA_MemFd);
    super::serialise(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: libspa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: readable,
                    flags: vec![readable],
                },
            ))),
        }],
    })
}
