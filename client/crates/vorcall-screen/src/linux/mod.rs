//! Linux backend: the desktop portal picks the source, PipeWire carries it.
//!
//! One thread, `vorcall-capture`, owns the whole capture. The portal is D-Bus,
//! which needs a tokio runtime; PipeWire is a C event loop that expects a thread
//! to itself. They share this one: instead of `pw_main_loop_run`, which would
//! never yield, the loop below iterates PipeWire by hand and hands the runtime a
//! millisecond in between — enough for the session's `Closed` signal to arrive,
//! and far too little for a frame to notice.
//!
//! Every `pipewire` and `ashpd` value here is `!Send`; all of them are born and
//! die on that thread. Only the stop signal crosses over, through a `pipewire`
//! channel whose receiver is an fd in the loop, since `quit` from another thread
//! is not allowed.

mod audio;
mod portal;
mod video;

use std::cell::Cell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Value};
use pipewire::channel;
use pipewire::context::ContextRc;
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use tokio::runtime::Runtime;

use crate::{
    AudioMode, Capabilities, CaptureEvent, CaptureRequest, Capturer, Source, Stop, Unavailable,
};

const BACKEND: &str = "pipewire";

/// How long the portal's picker may keep the user. It is a dialog with a human
/// in front of it, so this is generous on purpose; `Capturer::start` says as
/// much.
const PICKER_BUDGET: Duration = Duration::from_secs(120);
/// How long PipeWire then has to deliver a running stream, which involves
/// nobody.
const STREAM_BUDGET: Duration = Duration::from_secs(10);
/// `Drop` must not stall whoever dropped the capture.
const STOP_BUDGET: Duration = Duration::from_millis(500);
const STOP_POLL: Duration = Duration::from_millis(5);

/// How long one turn of the PipeWire loop may block. Frames wake it on their
/// own; this only bounds how late a revoked share is noticed.
const ITERATION: Duration = Duration::from_millis(250);
/// And how much of the thread the portal's D-Bus connection gets afterwards.
const PORTAL_SLICE: Duration = Duration::from_millis(1);

/// What the capture thread tells `start`: first that the portal answered, then
/// that the stream runs.
type Report = Result<(), Unavailable>;

/// The portal's handle on the source this process shared last. Reusing it is
/// what keeps a second share in the same run from asking the user again.
static RESTORE_TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn restore_token() -> MutexGuard<'static, Option<String>> {
    RESTORE_TOKEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn enumerate() -> Result<Vec<Source>, Unavailable> {
    Ok(vec![])
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        portal_picker: true,
        windows: true,
        audio: true,
    }
}

pub(crate) fn start(
    request: CaptureRequest,
    events: UnboundedSender<CaptureEvent>,
) -> Result<Capturer, Unavailable> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return Err(Unavailable::Unsupported(
            "there is no graphical session to capture".to_string(),
        ));
    }

    let (reports, report) = mpsc::channel::<Report>();
    let (stop, stopped) = channel::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("vorcall-capture".to_string())
        .spawn(move || capture(request, events, &reports, stopped))
        .map_err(|error| {
            Unavailable::Failed(format!("cannot start the capture thread: {error}"))
        })?;

    let mut handle = Handle {
        stop: Some(stop),
        thread: Some(thread),
    };
    let started = await_report(&report, PICKER_BUDGET, "the desktop portal did not answer")
        .and_then(|()| await_report(&report, STREAM_BUDGET, "PipeWire stream did not start"));
    match started {
        Ok(()) => Ok(Capturer::new(BACKEND, Box::new(handle))),
        Err(err) => {
            handle.stop();
            Err(err)
        }
    }
}

/// Waits for one of the capture thread's two reports, turning a silence into
/// `failure`.
fn await_report(
    report: &mpsc::Receiver<Report>,
    budget: Duration,
    failure: &str,
) -> Result<(), Unavailable> {
    match report.recv_timeout(budget) {
        Ok(report) => report,
        Err(_) => Err(Unavailable::Failed(failure.to_string())),
    }
}

fn capture(
    request: CaptureRequest,
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
    // signal stream or a proxy by spawning the D-Bus unsubscribe onto the
    // runtime, and a `tokio::spawn` outside a runtime's context panics. A panic
    // in a destructor while another is unwinding aborts the process, so this
    // guard is what keeps the end of a share from taking the app with it. It is
    // declared here so it outlives every zbus value and is dropped just before
    // the runtime itself.
    let _context = runtime.enter();

    let (cast, remote) = match runtime.block_on(portal::open(&request)) {
        Ok(opened) => opened,
        Err(err) => {
            let _ = reports.send(Err(err));
            return;
        }
    };
    if reports.send(Ok(())).is_err() {
        runtime.block_on(cast.close());
        return;
    }

    let capture = Capture {
        runtime: &runtime,
        request: &request,
        ending: Rc::new(Ending::new(events)),
        reports,
    };
    let mut revoked = runtime.block_on(cast.revoked());
    if let Err(err) = capture.run(&cast, remote, &mut revoked, stopped) {
        let _ = reports.send(Err(err));
    }

    // Ordered by hand: the events sender first, so the consumer's stream ends
    // without waiting on the portal, and then the two values whose `Drop` talks
    // to D-Bus — the signal stream unsubscribing its match rule, and the
    // session's proxy unsubscribing in turn as `close` consumes it. Those two
    // are why the runtime context above is still held here.
    drop(capture);
    drop(revoked);
    runtime.block_on(cast.close());
}

/// Everything the capture loop needs that outlives one turn of it.
struct Capture<'a> {
    runtime: &'a Runtime,
    request: &'a CaptureRequest,
    ending: Rc<Ending>,
    reports: &'a mpsc::Sender<Report>,
}

impl Capture<'_> {
    /// Builds the PipeWire streams and then owns the thread until the capture
    /// ends. An error here is still `start`'s to answer with: no frame has
    /// reached the consumer yet.
    fn run(
        &self,
        cast: &portal::Cast,
        remote: OwnedFd,
        revoked: &mut Option<portal::Revoked<'_>>,
        stopped: channel::Receiver<()>,
    ) -> Result<(), Unavailable> {
        pipewire::init();
        let broken =
            |error: pipewire::Error| Unavailable::Failed(format!("cannot reach PipeWire: {error}"));

        let mainloop = MainLoopRc::new(None).map_err(broken)?;
        let context = ContextRc::new(&mainloop, None).map_err(broken)?;

        // Share audio never comes down the portal's remote: that connection
        // carries the cast's node and nothing else. The sink monitor lives on
        // the session's own socket, which is a second connection.
        let audio = self
            .request
            .audio
            .then(|| audio::connect(&context, self.ending.clone()))
            .flatten();
        let cast_core = context.connect_fd_rc(remote, None).map_err(broken)?;
        let _screen = video::connect(
            &cast_core,
            cast,
            self.request,
            audio.is_some().then_some(AudioMode::IncludesOwnPlayout),
            self.ending.clone(),
            self.reports.clone(),
        )?;

        let stopping = Rc::new(Cell::new(false));
        let _stop = stopped.attach(mainloop.loop_(), {
            let stopping = stopping.clone();
            move |()| stopping.set(true)
        });

        while !self.ending.done() && !stopping.get() {
            mainloop.loop_().iterate(Timeout::Finite(ITERATION));
            if self.revoked(revoked) {
                self.ending
                    .end("the screen share was stopped from the desktop".to_string());
            }
        }
        Ok(())
    }

    /// Gives the portal's D-Bus connection its moment of this thread, and
    /// answers whether the compositor has taken the share away.
    fn revoked(&self, revoked: &mut Option<portal::Revoked<'_>>) -> bool {
        let polled = {
            let Some(closed) = revoked.as_mut() else {
                return false;
            };
            self.runtime
                .block_on(async { tokio::time::timeout(PORTAL_SLICE, closed.next()).await })
        };
        match polled {
            Ok(Some(())) => true,
            // The signal's own stream ended, so it will never say anything: stop
            // spending the thread on it and leave the ending to the stream.
            Ok(None) => {
                *revoked = None;
                false
            }
            Err(_) => false,
        }
    }
}

/// Where a capture ends, whoever notices first. The consumer hears one
/// [`CaptureEvent::Ended`] and nothing after it.
struct Ending {
    events: UnboundedSender<CaptureEvent>,
    done: Cell<bool>,
}

impl Ending {
    fn new(events: UnboundedSender<CaptureEvent>) -> Ending {
        Ending {
            events,
            done: Cell::new(false),
        }
    }

    fn done(&self) -> bool {
        self.done.get()
    }

    /// Passes one event on, unless the consumer has dropped its end — which ends
    /// the capture quietly, there being nobody left to tell.
    fn send(&self, event: CaptureEvent) {
        if self.done.get() {
            return;
        }
        if self.events.unbounded_send(event).is_err() {
            self.done.set(true);
        }
    }

    fn end(&self, reason: String) {
        if self.done.get() {
            return;
        }
        tracing::debug!(%reason, "the screen capture ended");
        let _ = self.events.unbounded_send(CaptureEvent::Ended(reason));
        self.done.set(true);
    }

    /// Ends without a word, because `start` is about to answer with the error
    /// instead and there is no capture for the consumer to hear about.
    fn abandon(&self) {
        self.done.set(true);
    }
}

struct Handle {
    stop: Option<channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let Some(thread) = self.thread.take() else {
            return;
        };

        // The thread still owes the portal a session close, a D-Bus round trip
        // that can outlast the budget. Past it, it winds itself down unwatched
        // rather than holding up whoever dropped the capture.
        let deadline = Instant::now() + STOP_BUDGET;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(STOP_POLL);
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            tracing::debug!("the capture thread is still closing the portal session");
        }
    }
}

/// Turns one POD object into the bytes `pw_stream` takes. Only a malformed
/// object can fail here, and then the stream simply never negotiates.
fn serialise(object: Object) -> Option<Vec<u8>> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .ok()
}
