//! macOS backend: a listen-only `CGEventTap`.
//!
//! `CGEventTapOptions::ListenOnly` is what keeps this an observation: such a
//! tap cannot alter or swallow an event, and the value the callback returns is
//! ignored. Reading events at all needs the Input Monitoring permission, which
//! is checked before any tap is created.

use std::cell::{Cell, RefCell};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use futures::channel::mpsc::UnboundedSender;

use crate::{Backend, Binding, Edge, EdgeFilter, Key, Listener, Stop, Unavailable, keymap};

/// How long `start` waits for the tap to come up on its own thread.
const START_TIMEOUT: Duration = Duration::from_secs(1);

/// How long the thread stays inside the run loop before it rechecks the stop
/// flag, and therefore the worst case for `Drop`.
const RUN_LOOP_SLICE: Duration = Duration::from_millis(250);

// CoreGraphics entry points the `core-graphics` crate does not expose.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

enum Target {
    /// Ordinary keys, matched on `kVK` codes.
    Keycodes(Vec<u16>),
    /// Modifiers never produce key events — only a `FlagsChanged` carrying this
    /// bit.
    Modifier(u64),
    Button(i64),
}

pub(crate) fn start(
    binding: Binding,
    edges: UnboundedSender<Edge>,
) -> Result<Listener, Unavailable> {
    // SAFETY: both calls take no arguments and only consult (or ask for) this
    // process's Input Monitoring grant.
    if !unsafe { CGPreflightListenEventAccess() } {
        // Shows the system prompt the first time it is called; the grant only
        // takes effect on the next launch, so this is a failure either way.
        unsafe { CGRequestListenEventAccess() };
        return Err(Unavailable::PermissionDenied(
            "Input Monitoring is not granted for Vorcall".to_string(),
        ));
    }

    let target = match binding {
        // Caps Lock only ever arrives as a FlagsChanged, and the modifier table
        // has no bit for it, so a listener on it could never fire. Refusing
        // here is what lets the app fall back to its in-window handling.
        Binding::Key(Key::CapsLock) => {
            return Err(Unavailable::Unsupported(
                "Caps Lock cannot be observed system-wide on macOS".to_string(),
            ));
        }
        Binding::Key(key) => match keymap::mac_modifier_flag(key) {
            Some(flag) => Target::Modifier(flag),
            None => {
                let keycodes = keymap::mac_keycodes(key);
                if keycodes.is_empty() {
                    return Err(Unavailable::Unsupported(
                        "macOS has no code for that key".to_string(),
                    ));
                }
                Target::Keycodes(keycodes)
            }
        },
        Binding::Mouse(button) => Target::Button(keymap::mac_button(button)),
    };

    let (ready_tx, ready_rx) = mpsc::channel::<Result<CFRunLoop, Unavailable>>();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("vorcall-hotkey".to_string())
        .spawn(move || run(target, edges, &ready_tx, &flag))
        .map_err(|err| Unavailable::Failed(format!("cannot start the listener thread: {err}")))?;

    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(run_loop)) => Ok(Listener::new(
            Backend::MacEventTap,
            None,
            Box::new(Handle {
                run_loop,
                stop,
                thread: Some(thread),
            }),
        )),
        Ok(Err(err)) => Err(err),
        Err(_) => {
            // The thread is late rather than gone; nothing here holds its run
            // loop, so the flag is the only way to tell it to wind down.
            stop.store(true, Ordering::Release);
            Err(Unavailable::Failed(
                "the event tap did not come up".to_string(),
            ))
        }
    }
}

fn run(
    target: Target,
    edges: UnboundedSender<Edge>,
    ready: &mpsc::Sender<Result<CFRunLoop, Unavailable>>,
    stop: &AtomicBool,
) {
    // Filled in once the tap exists, so the callback can switch it back on
    // after the system disables it.
    let port: Rc<Cell<CFMachPortRef>> = Rc::new(Cell::new(ptr::null_mut()));
    let callback_port = Rc::clone(&port);
    let filter = RefCell::new(EdgeFilter::default());

    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ],
        move |_proxy, event_type, event| {
            match event_type {
                // The system drops a tap that was too slow, or that the user
                // suspended; both are recoverable by turning it back on.
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    let tap = callback_port.get();
                    if !tap.is_null() {
                        // SAFETY: `tap` is the live mach port of the tap this
                        // callback belongs to.
                        unsafe { CGEventTapEnable(tap, true) };
                    }
                }
                _ => {
                    if let Some(edge) = classify(&target, event_type, event)
                        && filter.borrow_mut().admit(edge)
                    {
                        let _ = edges.unbounded_send(edge);
                    }
                }
            }
            // A listen-only tap cannot change the event stream anyway.
            None
        },
    );

    let Ok(tap) = tap else {
        let _ = ready.send(Err(Unavailable::Failed(
            "cannot create the event tap".to_string(),
        )));
        return;
    };
    port.set(tap.mach_port.as_concrete_TypeRef());

    let Ok(source) = tap.mach_port.create_runloop_source(0) else {
        let _ = ready.send(Err(Unavailable::Failed(
            "cannot attach the event tap to a run loop".to_string(),
        )));
        return;
    };
    let run_loop = CFRunLoop::get_current();
    // SAFETY: reading a CoreFoundation extern static; the mode string is a
    // process-lifetime constant.
    let mode = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(&source, mode);
    tap.enable();

    if ready.send(Ok(run_loop)).is_err() {
        return;
    }

    // Not `run_current`: a stop that lands between the send above and the first
    // run of the loop would be a no-op and park this thread forever. Running in
    // slices means the flag is always rechecked, whatever the timing.
    // SAFETY: reading a CoreFoundation extern static.
    let mode = unsafe { kCFRunLoopDefaultMode };
    while !stop.load(Ordering::Acquire) {
        // The tap's source sits in the common modes, which include this one.
        CFRunLoop::run_in_mode(mode, RUN_LOOP_SLICE, false);
    }
}

fn classify(target: &Target, event_type: CGEventType, event: &CGEvent) -> Option<Edge> {
    match (target, event_type) {
        (Target::Keycodes(keycodes), CGEventType::KeyDown) => {
            if event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 {
                return None;
            }
            matches_keycode(event, keycodes).then_some(Edge::Pressed)
        }
        (Target::Keycodes(keycodes), CGEventType::KeyUp) => {
            matches_keycode(event, keycodes).then_some(Edge::Released)
        }
        (Target::Modifier(flag), CGEventType::FlagsChanged) => {
            // Any modifier change reports every modifier's current state, so
            // the bit alone says whether the bound one is down.
            let held = event.get_flags().bits() & flag != 0;
            Some(if held { Edge::Pressed } else { Edge::Released })
        }
        (Target::Button(number), CGEventType::OtherMouseDown) => {
            matches_button(event, *number).then_some(Edge::Pressed)
        }
        (Target::Button(number), CGEventType::OtherMouseUp) => {
            matches_button(event, *number).then_some(Edge::Released)
        }
        _ => None,
    }
}

fn matches_keycode(event: &CGEvent, keycodes: &[u16]) -> bool {
    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
    keycodes.iter().any(|code| i64::from(*code) == keycode)
}

fn matches_button(event: &CGEvent, number: i64) -> bool {
    event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) == number
}

struct Handle {
    /// `CFRunLoopStop` is documented as safe to call from another thread, which
    /// is why `core-foundation` marks `CFRunLoop` `Send`.
    run_loop: CFRunLoop,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Only an optimisation: it cuts the current slice short instead of
        // waiting it out, and is harmless if the loop is not running yet.
        self.run_loop.stop();
        if let Some(thread) = self.thread.take() {
            // Bounded by one run loop slice.
            let _ = thread.join();
        }
    }
}
