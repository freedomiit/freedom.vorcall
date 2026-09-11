//! X11 backend: XInput2 raw events.
//!
//! Raw events are delivered to every client that asks for them and are not
//! routed by focus, so nothing here takes a grab and nothing is consumed — the
//! key still reaches whichever window has focus.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use futures::channel::mpsc::UnboundedSender;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{ConnectionExt as XinputExt, EventMask, XIEventMask};
use x11rb::protocol::xproto::{ConnectionExt as XprotoExt, Keycode};
use x11rb::rust_connection::RustConnection;

use crate::{Backend, Binding, Edge, EdgeFilter, Listener, Stop, Unavailable, keymap};

/// `XIAllMasterDevices`: the logical keyboard and pointer, which is what a
/// binding is about — not one physical device out of several.
const ALL_MASTER_DEVICES: u16 = 1;

/// How long the thread blocks in `poll` before it rechecks the stop flag, and
/// therefore the worst case for `Drop`.
const POLL_TIMEOUT_MS: i32 = 500;

enum Target {
    /// Every keycode whose keysym table mentions one of the bound keysyms.
    Keycodes(Vec<Keycode>),
    Button(u32),
}

pub(crate) fn start(
    binding: Binding,
    edges: UnboundedSender<Edge>,
) -> Result<Listener, Unavailable> {
    let (conn, screen) = x11rb::connect(None)
        .map_err(|err| Unavailable::Failed(format!("cannot reach the X server: {err}")))?;

    let no_xinput = || Unavailable::Unsupported("the X server has no XInput 2".to_string());
    let version = conn
        .xinput_xi_query_version(2, 0)
        .map_err(|_| no_xinput())?
        .reply()
        .map_err(|_| no_xinput())?;
    if version.major_version < 2 {
        return Err(no_xinput());
    }

    let target = match binding {
        Binding::Key(key) => Target::Keycodes(keycodes_for(&conn, &keymap::x11_keysyms(key))?),
        Binding::Mouse(button) => Target::Button(keymap::x11_button(button)),
    };

    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or_else(|| Unavailable::Failed("the X server reported no screen".to_string()))?
        .root;
    let mask = XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE;
    conn.xinput_xi_select_events(
        root,
        &[EventMask {
            deviceid: ALL_MASTER_DEVICES,
            mask: vec![mask],
        }],
    )
    .map_err(|err| Unavailable::Failed(format!("cannot select raw input events: {err}")))?
    .check()
    .map_err(|err| Unavailable::Failed(format!("cannot select raw input events: {err}")))?;
    conn.flush()
        .map_err(|err| Unavailable::Failed(format!("cannot talk to the X server: {err}")))?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("vorcall-hotkey".to_string())
        .spawn(move || run(conn, target, edges, &flag))
        .map_err(|err| Unavailable::Failed(format!("cannot start the listener thread: {err}")))?;

    Ok(Listener::new(
        Backend::X11Raw,
        None,
        Box::new(Handle {
            stop,
            thread: Some(thread),
        }),
    ))
}

/// Resolves keysyms to the keycodes the current layout puts them on, in any
/// shift level, so that a binding survives a layout that only reaches the key
/// with a modifier.
fn keycodes_for(conn: &RustConnection, keysyms: &[u32]) -> Result<Vec<Keycode>, Unavailable> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
    let mapping = conn
        .get_keyboard_mapping(first, count)
        .map_err(|err| Unavailable::Failed(format!("cannot read the keyboard mapping: {err}")))?
        .reply()
        .map_err(|err| Unavailable::Failed(format!("cannot read the keyboard mapping: {err}")))?;

    let per_keycode = usize::from(mapping.keysyms_per_keycode);
    if per_keycode == 0 {
        return Err(Unavailable::Failed(
            "the X server reported an empty keyboard mapping".to_string(),
        ));
    }

    let keycodes: Vec<Keycode> = mapping
        .keysyms
        .chunks(per_keycode)
        .enumerate()
        .filter(|(_, symbols)| symbols.iter().any(|symbol| keysyms.contains(symbol)))
        .filter_map(|(offset, _)| u8::try_from(offset).ok()?.checked_add(first))
        .collect();

    if keycodes.is_empty() {
        return Err(Unavailable::Unsupported(
            "the current keyboard layout has no such key".to_string(),
        ));
    }
    Ok(keycodes)
}

fn run(conn: RustConnection, target: Target, edges: UnboundedSender<Edge>, stop: &AtomicBool) {
    let fd = conn.stream().as_raw_fd();
    let mut filter = EdgeFilter::default();

    while !stop.load(Ordering::Relaxed) {
        let mut watched = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: `watched` is a live, correctly sized array of pollfd, and the
        // fd belongs to the connection this thread owns.
        unsafe { libc::poll(watched.as_mut_ptr(), 1, POLL_TIMEOUT_MS) };

        loop {
            match conn.poll_for_event() {
                Ok(Some(event)) => {
                    let Some(edge) = translate(&event, &target) else {
                        continue;
                    };
                    if filter.admit(edge) && edges.unbounded_send(edge).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                // The X server went away; the app will notice through the
                // closed channel.
                Err(_) => return,
            }
        }
    }
}

fn translate(event: &Event, target: &Target) -> Option<Edge> {
    match (event, target) {
        (Event::XinputRawKeyPress(raw), Target::Keycodes(keycodes)) => {
            matches_keycode(raw.detail, keycodes).then_some(Edge::Pressed)
        }
        (Event::XinputRawKeyRelease(raw), Target::Keycodes(keycodes)) => {
            matches_keycode(raw.detail, keycodes).then_some(Edge::Released)
        }
        (Event::XinputRawButtonPress(raw), Target::Button(button)) => {
            (raw.detail == *button).then_some(Edge::Pressed)
        }
        (Event::XinputRawButtonRelease(raw), Target::Button(button)) => {
            (raw.detail == *button).then_some(Edge::Released)
        }
        _ => None,
    }
}

fn matches_keycode(detail: u32, keycodes: &[Keycode]) -> bool {
    keycodes.iter().any(|keycode| u32::from(*keycode) == detail)
}

struct Handle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Bounded by the poll timeout above.
            let _ = thread.join();
        }
    }
}
