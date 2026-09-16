//! Linux camera: the portal grants the access, PipeWire carries the frames.
//!
//! The shape is the screen capture's next door, and for the same reasons: one
//! thread, `vorcall-camera`, owning a tokio runtime for the portal's D-Bus and
//! giving the PipeWire loop the rest of its time. Every `pipewire` and `ashpd`
//! value here is `!Send`, and zbus unsubscribes from D-Bus by spawning onto the
//! runtime a proxy was built in — so the runtime's context guard is held for
//! the whole thread and the proxy is dropped by hand inside it.
//!
//! The camera portal has no picker. It grants access to the cameras as a whole
//! and answers with one PipeWire remote carrying every camera node, which is
//! why [`cameras`] has nothing to list and the node is chosen here, by walking
//! the registry: without a node id the session manager is free to connect the
//! stream to the wrong device.
//!
//! Only raw formats are asked for. A camera that offers nothing but MJPG would
//! need a JPEG decoder this crate does not have, and a stream that cannot
//! negotiate never runs — which is what the start budget below turns into a
//! message.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ashpd::desktop::ResponseError;
use ashpd::desktop::camera::Camera;
use futures::channel::mpsc::UnboundedSender;
use libspa::param::ParamType;
use libspa::param::format::{FormatProperties, MediaSubtype, MediaType};
use libspa::param::format_utils;
use libspa::param::video::{VideoFormat, VideoInfoRaw};
use libspa::pod::{Pod, object, property};
use libspa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pipewire::channel;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::types::ObjectType;

use super::{
    Ending, Handle, Kind, LIVE_CAMERA, Live, Report, admit, await_report, convert, serialise,
};
use crate::camera::{CameraCapabilities, CameraRequest, CameraSource};
use crate::preset::FrameRate;
use crate::{CaptureEvent, Capturer, Stop, Unavailable, VideoFrame};

const BACKEND: &str = "pipewire";

/// No camera hands over a frame larger than this, and the range has to be
/// bounded for the format to be a range at all.
const LARGEST: Rectangle = Rectangle {
    width: 8_192,
    height: 8_192,
};

/// How long the portal itself has to answer, before any dialog is raised.
const PORTAL_BUDGET: Duration = Duration::from_secs(5);
/// How long the portal's permission dialog may keep the user. A human may be
/// in front of it, so this is generous on purpose.
const ACCESS_BUDGET: Duration = Duration::from_secs(120);
/// How long PipeWire then has to deliver a running stream, which involves
/// nobody. Running out of it is also how an MJPG-only camera shows up: its
/// stream never negotiates a format this client offered.
const STREAM_BUDGET: Duration = Duration::from_secs(10);

/// How long the registry walk waits for the camera nodes to arrive.
const DISCOVERY_BUDGET: Duration = Duration::from_secs(3);
const DISCOVERY_STEP: Duration = Duration::from_millis(20);

/// How long one turn of the PipeWire loop may block. Frames wake it on their
/// own; this only bounds how late a stop is noticed.
const ITERATION: Duration = Duration::from_millis(250);

/// The portal has no per-device picker, so there is no list to offer: the
/// device is chosen for us and named only once a stream is open.
pub(crate) fn cameras() -> Vec<CameraSource> {
    Vec::new()
}

pub(crate) fn capabilities() -> CameraCapabilities {
    CameraCapabilities {
        backend: BACKEND,
        available: true,
        enumerates: false,
    }
}

pub(crate) fn start(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    admit(Kind::Camera, LIVE_CAMERA.load(Ordering::SeqCst))?;

    let (reports, report) = mpsc::channel::<Report>();
    let (stop, stopped) = channel::channel::<()>();
    let live = Live::claim(Kind::Camera);
    let thread = std::thread::Builder::new()
        .name("vorcall-camera".to_string())
        .spawn(move || {
            // Bound rather than merely captured, so the slot is given back when
            // this body returns, however it returns.
            let _live = live;
            capture(request, events, &reports, stopped)
        })
        .map_err(|error| Unavailable::Failed(format!("cannot start the camera thread: {error}")))?;

    let mut handle = Handle {
        stop: Some(stop),
        thread: Some(thread),
    };
    match await_started(&report) {
        Ok(()) => Ok(Capturer::new(BACKEND, Box::new(handle))),
        Err(err) => {
            handle.stop();
            Err(err)
        }
    }
}

/// Waits out the three steps of a start in turn, each against the budget of
/// what it is actually waiting on: the portal, then the user, then PipeWire.
fn await_started(report: &mpsc::Receiver<Report>) -> Result<(), Unavailable> {
    await_report(report, PORTAL_BUDGET, "the desktop portal did not answer")?;
    await_report(report, ACCESS_BUDGET, "camera access was not granted")?;
    await_report(
        report,
        STREAM_BUDGET,
        "the camera stream did not start; a camera that offers only MJPG cannot be read here",
    )
}

fn capture(
    request: CameraRequest,
    events: UnboundedSender<CaptureEvent>,
    reports: &mpsc::Sender<Report>,
    stopped: channel::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = reports.send(Err(Unavailable::Failed(format!(
                "cannot start the portal runtime: {error}"
            ))));
            return;
        }
    };
    // Held, named and never dropped early, for everything below: zbus drops a
    // proxy by spawning the D-Bus unsubscribe onto the runtime, and a
    // `tokio::spawn` outside a runtime's context panics. A panic in a
    // destructor while another is unwinding aborts the process, so this guard
    // is what keeps the end of a camera from taking the app with it.
    let _context = runtime.enter();

    // Sent from inside `open`, the moment the portal has answered: everything
    // after it is the user in front of the permission dialog, whose budget is
    // its own.
    let reached = || {
        let _ = reports.send(Ok(()));
    };
    let (portal, remote) = match runtime.block_on(open(reached)) {
        Ok(opened) => opened,
        Err(err) => {
            let _ = reports.send(Err(err));
            return;
        }
    };
    // The second report: access is granted and the remote is ours.
    if reports.send(Ok(())).is_err() {
        drop(portal);
        return;
    }

    let ending = Rc::new(Ending::new(Kind::Camera, events));
    if let Err(err) = run(&request, remote, &ending, reports, stopped) {
        let _ = reports.send(Err(err));
    }

    // Ordered by hand: the events sender first, so the consumer's stream ends
    // without waiting on the portal, and then the proxy whose `Drop` talks to
    // D-Bus — which is why the runtime context above is still held here.
    drop(ending);
    drop(portal);
}

/// Asks the portal for camera access and for the PipeWire remote the cameras
/// live on.
///
/// Calls `reached` once the portal has answered at all, which is the last thing
/// here that happens without a human: from there on this blocks on the user for
/// as long as the permission dialog is up.
async fn open(reached: impl FnOnce()) -> Result<(Camera, OwnedFd), Unavailable> {
    let camera = Camera::new()
        .await
        .map_err(|_| Unavailable::Unsupported("no desktop portal".to_string()))?;
    reached();

    let remote = grant(&camera).await?;
    Ok((camera, remote))
}

async fn grant(camera: &Camera) -> Result<OwnedFd, Unavailable> {
    if !camera.is_present().await.unwrap_or(false) {
        return Err(Unavailable::Unsupported(
            "there is no camera on this machine".to_string(),
        ));
    }

    camera
        .request_access(Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|error| match error {
            ashpd::Error::Response(ResponseError::Cancelled) => {
                Unavailable::PermissionDenied("camera access was refused".to_string())
            }
            error => Unavailable::Failed(format!("the portal refused camera access: {error}")),
        })?;

    camera
        .open_pipe_wire_remote(Default::default())
        .await
        .map_err(|error| {
            Unavailable::Failed(format!("the portal gave no PipeWire remote: {error}"))
        })
}

/// Builds the PipeWire stream and then owns the thread until the capture ends.
/// An error here is still `start`'s to answer with: no frame has reached the
/// consumer yet.
fn run(
    request: &CameraRequest,
    remote: OwnedFd,
    ending: &Rc<Ending>,
    reports: &mpsc::Sender<Report>,
    stopped: channel::Receiver<()>,
) -> Result<(), Unavailable> {
    pipewire::init();
    let broken =
        |error: pipewire::Error| Unavailable::Failed(format!("cannot reach PipeWire: {error}"));

    let mainloop = MainLoopRc::new(None).map_err(broken)?;
    let context = ContextRc::new(&mainloop, None).map_err(broken)?;
    let core = context.connect_fd_rc(remote, None).map_err(broken)?;

    let node = choose(&mainloop, &core, request.source.as_ref())?;
    tracing::debug!(node = node.id, name = %node.name, "opening a camera");
    let _camera = connect(&core, &node, request, ending.clone(), reports.clone())?;

    let stopping = Rc::new(Cell::new(false));
    let _stop = stopped.attach(mainloop.loop_(), {
        let stopping = stopping.clone();
        move |()| stopping.set(true)
    });

    while !ending.done() && !stopping.get() {
        mainloop.loop_().iterate(Timeout::Finite(ITERATION));
    }
    Ok(())
}

/// One camera node on the portal's remote.
struct Node {
    id: u32,
    name: String,
}

/// The node the request names, or the first one the remote offers.
fn choose(
    mainloop: &MainLoopRc,
    core: &CoreRc,
    wanted: Option<&CameraSource>,
) -> Result<Node, Unavailable> {
    let found = discover(mainloop, core)?;
    let Some(wanted) = wanted else {
        return found
            .into_iter()
            .next()
            .ok_or_else(|| Unavailable::Unsupported("the portal offered no camera".to_string()));
    };

    found
        .into_iter()
        .find(|node| node.id.to_string() == wanted.id)
        .ok_or_else(|| Unavailable::Failed(format!("the camera {} is gone", wanted.name)))
}

/// Every node on the remote that calls itself a camera, in the order PipeWire
/// announced them.
fn discover(mainloop: &MainLoopRc, core: &CoreRc) -> Result<Vec<Node>, Unavailable> {
    let registry = core.get_registry_rc().map_err(|error| {
        Unavailable::Failed(format!("cannot read the PipeWire registry: {error}"))
    })?;

    let found = Rc::new(RefCell::new(Vec::new()));
    let _listener = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                if props.get("media.role") != Some("Camera") {
                    return;
                }
                let name = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .or_else(|| props.get("node.name"))
                    .unwrap_or("Camera");
                found.borrow_mut().push(Node {
                    id: global.id,
                    name: name.to_string(),
                });
            }
        })
        .register();

    // The registry's opening burst ends when the server answers the sync below.
    // Nothing else on this connection ever asks for one, so any `done` on the
    // core is that answer.
    let done = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done({
            let done = done.clone();
            move |id, _| {
                if id == PW_ID_CORE {
                    done.set(true);
                }
            }
        })
        .register();
    core.sync(0)
        .map_err(|error| Unavailable::Failed(format!("cannot reach PipeWire: {error}")))?;

    let deadline = Instant::now() + DISCOVERY_BUDGET;
    while !done.get() && Instant::now() < deadline {
        mainloop.loop_().iterate(Timeout::Finite(DISCOVERY_STEP));
    }

    // Taken rather than unwrapped: the listener's closure still holds its own
    // handle on the list, and it only ever runs inside the loop above.
    let nodes = std::mem::take(&mut *found.borrow_mut());
    Ok(nodes)
}

/// The running camera stream. Both halves have to stay alive: dropping the
/// listener stops the callbacks, dropping the stream stops the capture. The
/// stream goes first, so no callback can reach the listener's data after it.
struct Running {
    _stream: StreamRc,
    _listener: StreamListener<State>,
}

fn connect(
    core: &CoreRc,
    node: &Node,
    request: &CameraRequest,
    ending: Rc<Ending>,
    reports: mpsc::Sender<Report>,
) -> Result<Running, Unavailable> {
    let stream = StreamRc::new(
        core.clone(),
        "vorcall-camera",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Camera",
        },
    )
    .map_err(|error| Unavailable::Failed(format!("cannot create the camera stream: {error}")))?;

    let listener = stream
        .add_local_listener_with_user_data(State {
            ending,
            reports: Some(reports),
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
            Unavailable::Failed(format!("cannot listen to the camera stream: {error}"))
        })?;

    let format = format_param(request.fps, request.size)
        .ok_or_else(|| Unavailable::Failed("cannot describe the camera format".to_string()))?;
    let format = Pod::from_bytes(&format)
        .ok_or_else(|| Unavailable::Failed("cannot describe the camera format".to_string()))?;
    stream
        .connect(
            Direction::Input,
            Some(node.id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|error| {
            Unavailable::Failed(format!("cannot connect the camera stream: {error}"))
        })?;

    Ok(Running {
        _stream: stream,
        _listener: listener,
    })
}

/// How a negotiated format is laid out in the buffer, and what it takes to turn
/// it into BGRA.
#[derive(Clone, Copy)]
enum Layout {
    Yuy2,
    Nv12,
    /// Four bytes a pixel; `swap` for the layouts whose red and blue are the
    /// other way round.
    Packed {
        swap: bool,
    },
}

impl Layout {
    fn of(format: VideoFormat) -> Option<Layout> {
        match format {
            VideoFormat::YUY2 => Some(Layout::Yuy2),
            VideoFormat::NV12 => Some(Layout::Nv12),
            VideoFormat::BGRx | VideoFormat::BGRA => Some(Layout::Packed { swap: false }),
            VideoFormat::RGBx | VideoFormat::RGBA => Some(Layout::Packed { swap: true }),
            _ => None,
        }
    }

    /// The bytes of the first plane one row of `width` pixels really needs,
    /// which is the floor under whatever stride the device reports.
    fn row_bytes(self, width: usize) -> usize {
        match self {
            Layout::Yuy2 => width.div_ceil(2) * 4,
            Layout::Nv12 => width,
            Layout::Packed { .. } => width * 4,
        }
    }
}

struct State {
    ending: Rc<Ending>,
    /// Answers `start`, once, with whichever of the two arrives first: the
    /// stream running, or the stream failing.
    reports: Option<mpsc::Sender<Report>>,
    format: Option<VideoInfoRaw>,
    started: bool,
    interval: Duration,
    previous: Option<Instant>,
    /// A frame we cannot read is worth saying once, not once per frame.
    complained: bool,
}

impl State {
    /// The stream is live, so whoever is waiting inside `start` may go.
    fn running(&mut self) {
        if let Some(reports) = self.reports.take() {
            let _ = reports.send(Ok(()));
        }
    }

    /// The stream died. Before `start` has answered that is its error and the
    /// consumer never hears of the capture at all; after it, the consumer is
    /// the one who needs to be told.
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
            StreamState::Error(error) => self.failed(format!("the camera stream failed: {error}")),
            // Nothing but the device going away drops a connected stream back
            // to unconnected, so this is the camera being unplugged.
            StreamState::Unconnected => self.failed("the camera was disconnected".to_string()),
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
            "negotiated a camera format"
        );
        self.format = Some(format);
        if !self.started {
            self.started = true;
            self.ending.send(CaptureEvent::Started {
                width: size.width,
                height: size.height,
                audio: None,
            });
        }

        // Said only now, because this is the one moment pw_stream reads buffer
        // parameters: without it a device behind a GPU path is free to hand
        // over DMA-BUF frames, which this process cannot map.
        if let Some(buffers) = super::video::buffers_param()
            .as_deref()
            .and_then(Pod::from_bytes)
            && let Err(error) = stream.update_params(&mut [buffers])
        {
            tracing::debug!(%error, "the camera stream kept its own buffer types");
        }
    }

    fn frame(&mut self, stream: &Stream) {
        // Dequeued first and unconditionally: a buffer only goes back to the
        // device when it is dropped, and a frame paced out below still has to
        // be given back.
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

        let (width, height) = (format.size().width as usize, format.size().height as usize);
        if width == 0 || height == 0 {
            return;
        }
        let Some(layout) = Layout::of(format.format()) else {
            self.failed(format!("the camera sends {:?} frames", format.format()));
            return;
        };

        let datas = buffer.datas_mut();
        let Some((first, rest)) = datas.split_first_mut() else {
            return;
        };
        let kind = first.type_();
        let (size, stride, offset) = {
            let chunk = first.chunk();
            (
                chunk.size(),
                // A device may pad its rows, and answers with a stride it has
                // not filled in as zero.
                usize::try_from(chunk.stride())
                    .unwrap_or(0)
                    .max(layout.row_bytes(width)),
                chunk.offset() as usize,
            )
        };
        if size == 0 {
            return;
        }
        // Read before the planes are borrowed, since both come off the same
        // buffer entry.
        let (second_stride, second_offset) = match rest.first() {
            Some(data) => (
                usize::try_from(data.chunk().stride()).unwrap_or(0),
                data.chunk().offset() as usize,
            ),
            None => (0, 0),
        };

        let Some(mapped) = first.data() else {
            // MAP_BUFFERS and the buffer types answered above should have ruled
            // this out; a device that insists on it can never be read here.
            self.failed(format!("the camera offers only {kind:?} frames"));
            return;
        };
        let Some(plane) = mapped.get(offset..) else {
            return;
        };
        let second = match rest.first_mut() {
            Some(data) => data.data().and_then(|bytes| bytes.get(second_offset..)),
            None => None,
        };

        let mut bgra = Vec::new();
        let read = match layout {
            Layout::Yuy2 => convert::yuy2(plane, width, height, stride, &mut bgra),
            Layout::Nv12 => {
                let (luma, chroma, chroma_stride) = match second {
                    Some(chroma) => (plane, chroma, second_stride.max(width.div_ceil(2) * 2)),
                    // One buffer carrying both planes, which is how a V4L2
                    // device's own frame arrives: the chroma plane follows the
                    // luma one at the same stride.
                    None => match plane.split_at_checked(stride * height) {
                        Some((luma, chroma)) => (luma, chroma, stride),
                        None => (plane, &[][..], stride),
                    },
                };
                convert::nv12(
                    luma,
                    stride,
                    chroma,
                    chroma_stride,
                    width,
                    height,
                    &mut bgra,
                )
            }
            Layout::Packed { swap } => {
                convert::packed(plane, width, height, stride, swap, &mut bgra)
            }
        };
        if !read {
            if !self.complained {
                self.complained = true;
                tracing::warn!(
                    len = mapped.len(),
                    stride,
                    width,
                    height,
                    "the camera's frame is shorter than the format it negotiated"
                );
            }
            return;
        }

        self.previous = Some(now);
        self.running();
        self.ending.send(CaptureEvent::Video(VideoFrame {
            width: width as u32,
            height: height as u32,
            stride: width * 4,
            bgra,
            captured: now,
        }));
    }
}

/// Every pixel layout this backend can turn into BGRA, at any size the device
/// happens to offer. Raw only, and YUY2 first: it is what a USB webcam sends
/// unless it is asked for something else.
///
/// `size` is only the preferred value of the range — a camera has a handful of
/// sizes it was built for and picks the nearest one it has.
fn format_param(fps: FrameRate, size: (u32, u32)) -> Option<Vec<u8>> {
    let preferred = Rectangle {
        width: size.0.clamp(1, LARGEST.width),
        height: size.1.clamp(1, LARGEST.height),
    };

    serialise(object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::YUY2,
            VideoFormat::YUY2,
            VideoFormat::NV12,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_has_no_camera_list_to_offer() {
        assert!(cameras().is_empty());
        let capabilities = capabilities();
        assert!(!capabilities.enumerates);
        assert_eq!(capabilities.backend, BACKEND);
    }

    #[test]
    fn every_offered_format_has_a_layout_and_nothing_else_does() {
        for format in [
            VideoFormat::YUY2,
            VideoFormat::NV12,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ] {
            assert!(Layout::of(format).is_some(), "{format:?} was offered");
        }
        // Encoded is what an MJPG stream negotiates, and this crate has no
        // decoder for it.
        for format in [VideoFormat::Encoded, VideoFormat::I420, VideoFormat::UYVY] {
            assert!(Layout::of(format).is_none(), "{format:?} cannot be read");
        }
    }

    #[test]
    fn a_row_never_measures_less_than_its_pixels() {
        assert_eq!(Layout::Yuy2.row_bytes(1280), 2_560);
        // An odd width still pays for the whole last macropixel.
        assert_eq!(Layout::Yuy2.row_bytes(3), 8);
        assert_eq!(Layout::Nv12.row_bytes(1280), 1_280);
        assert_eq!(Layout::Packed { swap: false }.row_bytes(1280), 5_120);
    }

    #[test]
    fn the_format_offered_describes_itself() {
        assert!(format_param(FrameRate::F30, (1280, 720)).is_some());
        // A zero size is clamped rather than refused: the request's size is
        // only ever a hint.
        assert!(format_param(FrameRate::F15, (0, 0)).is_some());
    }
}
