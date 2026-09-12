//! X11 backend: XInput2 raw events.
//!
//! Raw events are delivered to every client that asks for them and are not
//! routed by focus, so nothing here takes a grab and nothing is consumed — the
//! key still reaches whichever window has focus.
//!
//! Raw events carry no modifier state either, so the modifiers a chord needs are
//! tracked from the press and release of the modifier keys themselves, whose
//! keycodes are resolved once at start.

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

use crate::{
    ActionId, Backend, Edge, Key, Listener, Modifier, ModifierKeys, Router, Shortcut, Stop,
    Trigger, Unavailable, keymap,
};

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
    bindings: Vec<Shortcut>,
    edges: UnboundedSender<(ActionId, Edge)>,
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

    // Read once, then resolve every binding and every modifier key against it.
    let layout = Layout::read(&conn)?;
    // A key this layout has no code for is skipped rather than fatal; the app
    // falls back to its in-window handling for that action alone.
    let (bound, unavailable) = crate::partition(&bindings, |shortcut| {
        Ok(match shortcut.binding.trigger {
            Trigger::Key(key) => Target::Keycodes(layout.bound_keycodes(key)?),
            Trigger::Mouse(button) => Target::Button(keymap::x11_button(button)),
        })
    });
    if bound.is_empty() {
        return Err(crate::nothing_bindable(&unavailable));
    }

    let mut router = Router::new(edges);
    for (action, mods, target) in bound {
        router.push(action, mods, target);
    }
    let modifiers = ModifierKeys::new(layout.modifier_keys());

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
        .spawn(move || run(conn, router, modifiers, &flag))
        .map_err(|err| Unavailable::Failed(format!("cannot start the listener thread: {err}")))?;

    Ok(Listener::new(
        Backend::X11Raw,
        Vec::new(),
        unavailable,
        Box::new(Handle {
            stop,
            thread: Some(thread),
        }),
    ))
}

/// The server's keycode-to-keysym table, as it stands when the listener starts.
struct Layout {
    first: Keycode,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

impl Layout {
    fn read(conn: &RustConnection) -> Result<Layout, Unavailable> {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
        let mapping = conn
            .get_keyboard_mapping(first, count)
            .map_err(|err| Unavailable::Failed(format!("cannot read the keyboard mapping: {err}")))?
            .reply()
            .map_err(|err| {
                Unavailable::Failed(format!("cannot read the keyboard mapping: {err}"))
            })?;

        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        if per_keycode == 0 {
            return Err(Unavailable::Failed(
                "the X server reported an empty keyboard mapping".to_string(),
            ));
        }
        Ok(Layout {
            first,
            per_keycode,
            keysyms: mapping.keysyms,
        })
    }

    /// The keycodes the current layout puts `keysyms` on, in any shift level, so
    /// that a binding survives a layout that only reaches the key with a
    /// modifier.
    fn keycodes(&self, keysyms: &[u32]) -> Vec<Keycode> {
        self.keysyms
            .chunks(self.per_keycode)
            .enumerate()
            .filter(|(_, symbols)| symbols.iter().any(|symbol| keysyms.contains(symbol)))
            .filter_map(|(offset, _)| u8::try_from(offset).ok()?.checked_add(self.first))
            .collect()
    }

    fn bound_keycodes(&self, key: Key) -> Result<Vec<Keycode>, Unavailable> {
        let keycodes = self.keycodes(&keymap::x11_keysyms(key));
        if keycodes.is_empty() {
            return Err(Unavailable::Unsupported(
                "the current keyboard layout has no such key".to_string(),
            ));
        }
        Ok(keycodes)
    }

    /// Both sides of each modifier, one entry per keycode. A modifier the layout
    /// has no key for is simply absent: only a binding that needs it is then
    /// unreachable, and failing the whole listener over it would be worse.
    fn modifier_keys(&self) -> Vec<(Keycode, Modifier)> {
        [
            (Key::Control, Modifier::Ctrl),
            (Key::Shift, Modifier::Shift),
            (Key::Alt, Modifier::Alt),
        ]
        .into_iter()
        .flat_map(|(key, modifier)| {
            self.keycodes(&keymap::x11_keysyms(key))
                .into_iter()
                .map(move |keycode| (keycode, modifier))
        })
        .collect()
    }
}

fn run(
    conn: RustConnection,
    mut router: Router<Target>,
    mut modifiers: ModifierKeys<Keycode>,
    stop: &AtomicBool,
) {
    let fd = conn.stream().as_raw_fd();

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
                    dispatch(&mut router, &mut modifiers, &event);
                    if router.closed() {
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

fn dispatch(router: &mut Router<Target>, modifiers: &mut ModifierKeys<Keycode>, event: &Event) {
    match event {
        Event::XinputRawKeyPress(raw) => key(router, modifiers, raw.detail, Edge::Pressed),
        Event::XinputRawKeyRelease(raw) => key(router, modifiers, raw.detail, Edge::Released),
        Event::XinputRawButtonPress(raw) => button(router, raw.detail, Edge::Pressed),
        Event::XinputRawButtonRelease(raw) => button(router, raw.detail, Edge::Released),
        _ => {}
    }
}

/// A key event: it may be a trigger, a modifier, or both — a binding on a bare
/// `Control` is exactly that.
fn key(
    router: &mut Router<Target>,
    modifiers: &mut ModifierKeys<Keycode>,
    detail: u32,
    edge: Edge,
) {
    if let Ok(keycode) = Keycode::try_from(detail) {
        modifiers.note(&keycode, edge);
        router.set_mods(modifiers.mods());
    }
    router.trigger(edge, |target| matches_keycode(target, detail));
}

fn button(router: &mut Router<Target>, detail: u32, edge: Edge) {
    router.trigger(
        edge,
        |target| matches!(target, Target::Button(bound) if *bound == detail),
    );
}

fn matches_keycode(target: &Target, detail: u32) -> bool {
    match target {
        Target::Keycodes(keycodes) => keycodes.iter().any(|code| u32::from(*code) == detail),
        Target::Button(_) => false,
    }
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
