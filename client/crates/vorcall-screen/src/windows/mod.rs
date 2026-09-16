//! Windows capture: DXGI Desktop Duplication for a monitor, Windows.Graphics
//! .Capture for a window and for the monitors duplication refuses, WASAPI
//! process loopback for the machine's own playout.
//!
//! Every Direct3D and WinRT object lives on the one capture thread started
//! here; what crosses a thread boundary is plain data and a flag. `max_size` is
//! ignored, because neither path scales — frames come out at the source's own
//! size and the encoder is what fits them.

mod audio;
pub(crate) mod camera;
mod cursor;
mod d3d;
mod duplication;
mod enumerate;
mod wgc;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

use crate::{
    AudioMode, Capabilities, CaptureEvent, CaptureRequest, Capturer, Source, SourceId, Stop,
    Unavailable,
};

/// What [`Capabilities`] calls this backend, whichever of the two paths a
/// particular capture ends up taking.
const BACKEND: &str = "wgc";
/// How long [`start`] waits for the capture thread's first outcome.
const START_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn enumerate() -> Result<Vec<Source>, Unavailable> {
    Ok(enumerate::sources())
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        portal_picker: false,
        windows: true,
        audio: true,
    }
}

pub(crate) fn start(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    let target = target(request.source.as_ref())?;

    let stop = Arc::new(AtomicBool::new(false));
    let mut handle = Handle {
        stop: stop.clone(),
        threads: Vec::new(),
    };

    // The audio thread opens its stream here, because `Started` has to carry
    // the mode it managed — but it is the capture thread that lets it speak,
    // once `Started` is out.
    let (audio, release) = if request.audio {
        match audio::spawn(events.clone(), stop.clone()) {
            Some((thread, mode, release)) => {
                handle.threads.push(thread);
                (Some(mode), Some(release))
            }
            None => (None, None),
        }
    } else {
        (None, None)
    };

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Unavailable>>();
    let spawned = std::thread::Builder::new()
        .name("vorcall-capture".to_string())
        .spawn({
            let stop = stop.clone();
            move || run(target, request, events, audio, release, &stop, &ready_tx)
        });
    match spawned {
        Ok(thread) => handle.threads.push(thread),
        Err(error) => {
            handle.stop();
            return Err(Unavailable::Failed(format!(
                "cannot start the capture thread: {error}"
            )));
        }
    }

    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(Capturer::new(BACKEND, Box::new(handle))),
        Ok(Err(error)) => {
            handle.stop();
            Err(error)
        }
        Err(_) => {
            handle.stop();
            Err(Unavailable::Failed("the capture did not start".to_string()))
        }
    }
}

/// What the app asked to share. Plain data, because a window or monitor handle
/// is only meaningful on the thread that will use it.
#[derive(Clone, Copy)]
enum Target {
    /// A position in [`enumerate::monitors`], resolved again at start.
    Monitor(usize),
    Window(u64),
}

fn target(source: Option<&SourceId>) -> Result<Target, Unavailable> {
    // There is no system picker on Windows, so a capture with no source named
    // is a capture with nothing to show.
    let Some(SourceId(id)) = source else {
        return Err(Unavailable::Failed("no source".to_string()));
    };

    if let Some(index) = id.strip_prefix("monitor:") {
        return index
            .parse()
            .map(Target::Monitor)
            .map_err(|_| Unavailable::Failed(format!("not a display: {id}")));
    }
    if let Some(handle) = id.strip_prefix("window:") {
        return handle
            .parse()
            .map(Target::Window)
            .map_err(|_| Unavailable::Failed(format!("not a window: {id}")));
    }
    Err(Unavailable::Failed(format!("unknown source: {id}")))
}

fn run(
    target: Target,
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
    audio: Option<AudioMode>,
    release: Option<audio::Release>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
) {
    // SAFETY: the first WinRT call on this thread, which exists only for the
    // length of this function.
    if let Err(error) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        let _ = ready.send(Err(Unavailable::Failed(format!(
            "WinRT refused this thread: {error}"
        ))));
        return;
    }

    capture(target, request, events, audio, release, stop, ready);

    // SAFETY: paired with the `RoInitialize` above, and `capture` has released
    // every Direct3D and WinRT object it made before returning.
    unsafe { RoUninitialize() };
}

fn capture(
    target: Target,
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
    audio: Option<AudioMode>,
    release: Option<audio::Release>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), Unavailable>>,
) {
    // Every path out of here that skips the release drops it instead, which is
    // what stops an audio thread the capture turned out not to need.
    let mut session = match open(target, &request) {
        Ok(session) => session,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let (width, height) = session.size();
    if !send(
        &events,
        CaptureEvent::Started {
            width,
            height,
            audio,
        },
    ) {
        return;
    }
    // Only now, so the app's first event is always `Started`.
    if let Some(release) = release {
        release.go();
    }
    if ready.send(Ok(())).is_err() {
        return;
    }

    session.run(&request, &events, stop);
}

enum Session {
    Duplicated(duplication::Duplication),
    Captured(wgc::Capture),
}

fn open(target: Target, request: &CaptureRequest) -> Result<Session, Unavailable> {
    match target {
        Target::Window(id) => {
            let window = wgc::Item::Window(enumerate::handle_from_id(id));
            wgc::Capture::open(window, request.cursor).map(Session::Captured)
        }
        Target::Monitor(index) => {
            let monitors = enumerate::monitors();
            let monitor = monitors
                .get(index)
                .ok_or_else(|| Unavailable::Failed("that display is gone".to_string()))?;

            match duplication::Duplication::open(monitor.handle) {
                Ok(duplicated) => {
                    tracing::debug!(index, "duplicating the monitor");
                    Ok(Session::Duplicated(duplicated))
                }
                Err(duplication::Refused::Failed(error)) => Err(error),
                Err(duplication::Refused::UseWgc(reason)) => {
                    tracing::debug!(index, %reason, "capturing the monitor through WGC instead");
                    wgc::Capture::open(wgc::Item::Monitor(monitor.handle), request.cursor)
                        .map(Session::Captured)
                }
            }
        }
    }
}

impl Session {
    fn size(&self) -> (u32, u32) {
        match self {
            Session::Duplicated(session) => session.size(),
            Session::Captured(session) => session.size(),
        }
    }

    fn run(
        &mut self,
        request: &CaptureRequest,
        events: &UnboundedSender<CaptureEvent>,
        stop: &AtomicBool,
    ) {
        match self {
            Session::Duplicated(session) => session.run(request, events, stop),
            Session::Captured(session) => session.run(request, events, stop),
        }
    }
}

/// Answers `false` once the app has dropped its end of the channel, which is
/// how a capture nobody is listening to stops without a word.
fn send(events: &UnboundedSender<CaptureEvent>, event: CaptureEvent) -> bool {
    events.unbounded_send(event).is_ok()
}

/// Stops every thread the capture started. Each of their loops looks at the
/// flag at least ten times a second, so the join is well inside the 500 ms the
/// trait allows.
struct Handle {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
