//! Windows backend: `WH_KEYBOARD_LL` and `WH_MOUSE_LL` hooks.
//!
//! Both procedures always call the next hook, so the input is observed and
//! never swallowed. They also stay trivial: Windows silently removes a
//! low-level hook whose procedure takes too long, so the only work they do is
//! a table lookup and a send on an unbounded channel.
//!
//! Hook procedures carry no user data, so every binding, the sender and the
//! edge filters live in one process-wide slot — which is why only one listener
//! can run at a time. One pair of hooks serves them all; the modifiers a chord
//! needs are tracked from the very same key events.

use std::ptr;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::{
    ActionId, Backend, Edge, Key, Listener, Modifier, ModifierKeys, MouseButton, Router, Shortcut,
    Stop, Trigger, Unavailable, keymap,
};

/// How long `start` waits for each step of the thread's start-up.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(1);

/// The thread's start-up, in the order it happens. The thread id comes first,
/// before the hooks exist, so a caller that gives up half way through still has
/// something to post `WM_QUIT` to.
enum Started {
    Thread(u32),
    Hooked,
    Failed(Unavailable),
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

struct HookState {
    modifiers: ModifierKeys<u16>,
    router: Router<Target>,
}

impl HookState {
    /// A key event: it may be a trigger, a modifier, or both — a binding on a
    /// bare `Control` is exactly that.
    fn key(&mut self, vk: u16, edge: Edge) {
        self.modifiers.note(&vk, edge);
        let mods = self.modifiers.mods();
        self.router.set_mods(mods);
        self.router.trigger(
            edge,
            |target| matches!(target, Target::Keys(keys) if keys.contains(&vk)),
        );
    }

    fn mouse(&mut self, button: MouseButton, edge: Edge) {
        self.router.trigger(
            edge,
            |target| matches!(target, Target::Mouse(bound) if *bound == button),
        );
    }
}

enum Target {
    /// Every virtual key that counts as the bound key, including both sides of
    /// a modifier.
    Keys(Vec<u16>),
    Mouse(MouseButton),
}

pub(crate) fn start(
    bindings: Vec<Shortcut>,
    edges: UnboundedSender<(ActionId, Edge)>,
) -> Result<Listener, Unavailable> {
    let mut router = Router::new(edges);
    for shortcut in &bindings {
        let target = match shortcut.binding.trigger {
            Trigger::Key(key) => Target::Keys(keymap::windows_vks(key)),
            Trigger::Mouse(button) => Target::Mouse(button),
        };
        router.push(shortcut.action, shortcut.binding.mods(), target);
    }
    let state = HookState {
        modifiers: ModifierKeys::new(modifier_keys()),
        router,
    };

    // Claimed before the thread starts so two concurrent calls cannot both
    // think the slot is free.
    {
        let mut slot = lock_state();
        if slot.is_some() {
            return Err(Unavailable::Failed(
                "a listener is already running".to_string(),
            ));
        }
        *slot = Some(state);
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Started>();
    let thread = std::thread::Builder::new()
        .name("vorcall-hotkey".to_string())
        .spawn(move || run(&ready_tx))
        .map_err(|err| {
            // Nothing ever claimed the slot on the thread's behalf.
            clear_state();
            Unavailable::Failed(format!("cannot start the listener thread: {err}"))
        })?;

    let Ok(Started::Thread(thread_id)) = ready_rx.recv_timeout(INSTALL_TIMEOUT) else {
        clear_state();
        return Err(Unavailable::Failed(
            "the listener thread did not start".to_string(),
        ));
    };

    // From here on the thread owns the state slot and clears it as it exits, so
    // every failure path stops the thread rather than clearing the slot itself.
    let mut handle = Handle {
        thread_id,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(INSTALL_TIMEOUT) {
        // Every key and button of the grammar has a virtual key, so this backend
        // never leaves a binding out.
        Ok(Started::Hooked) => Ok(Listener::new(
            Backend::WindowsHook,
            Vec::new(),
            Vec::new(),
            Box::new(handle),
        )),
        Ok(Started::Failed(err)) => {
            handle.stop();
            Err(err)
        }
        Ok(Started::Thread(_)) | Err(_) => {
            handle.stop();
            Err(Unavailable::Failed(
                "the input hooks did not install".to_string(),
            ))
        }
    }
}

/// Every virtual key that raises a modifier: both sides plus the side-agnostic
/// code injected input carries.
fn modifier_keys() -> Vec<(u16, Modifier)> {
    [
        (Key::Control, Modifier::Ctrl),
        (Key::Shift, Modifier::Shift),
        (Key::Alt, Modifier::Alt),
    ]
    .into_iter()
    .flat_map(|(key, modifier)| {
        keymap::windows_vks(key)
            .into_iter()
            .map(move |vk| (vk, modifier))
    })
    .collect()
}

fn run(ready: &mpsc::Sender<Started>) {
    ensure_message_queue();
    // SAFETY: called on the thread whose id this is.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready.send(Started::Thread(thread_id)).is_ok() {
        match install() {
            Ok(hooks) => {
                if ready.send(Started::Hooked).is_ok() {
                    pump();
                }
                for hook in hooks {
                    // SAFETY: both handles came from SetWindowsHookExW on this
                    // thread and are unhooked exactly once.
                    unsafe { UnhookWindowsHookEx(hook) };
                }
            }
            Err(err) => {
                let _ = ready.send(Started::Failed(err));
            }
        }
    }
    clear_state();
}

/// A thread has no message queue until it asks for one, and both the low-level
/// hooks and the `WM_QUIT` that stops them need it to exist. Peeking at a range
/// this thread never posts to is the documented way to create it.
fn ensure_message_queue() {
    let mut message = empty_message();
    // SAFETY: `message` is a live MSG; PM_NOREMOVE only inspects the queue.
    unsafe { PeekMessageW(&mut message, ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE) };
}

fn install() -> Result<[HHOOK; 2], Unavailable> {
    // SAFETY: a null module name asks for the handle of the current process,
    // which is what a hook procedure inside this binary needs.
    let module = unsafe { GetModuleHandleW(ptr::null()) };

    // SAFETY: both procedures live for the life of the process and match the
    // HOOKPROC signature.
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
    if keyboard.is_null() {
        return Err(Unavailable::Failed(
            "cannot install the keyboard hook".to_string(),
        ));
    }
    // SAFETY: as above.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) };
    if mouse.is_null() {
        // SAFETY: `keyboard` is a live handle from the call above.
        unsafe { UnhookWindowsHookEx(keyboard) };
        return Err(Unavailable::Failed(
            "cannot install the mouse hook".to_string(),
        ));
    }
    Ok([mouse, keyboard])
}

/// Low-level hooks are only delivered to a thread that pumps messages; this
/// one has no window, so nothing is dispatched, and `WM_QUIT` ends it.
fn pump() {
    let mut message = empty_message();
    // SAFETY: `message` is a live, correctly typed MSG for the duration of the
    // loop; a null window handle asks for every message of this thread.
    while unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } > 0 {}
}

fn empty_message() -> MSG {
    MSG {
        hwnd: ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: POINT { x: 0, y: 0 },
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let edge = match wparam as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(Edge::Pressed),
            WM_KEYUP | WM_SYSKEYUP => Some(Edge::Released),
            _ => None,
        };
        if let Some(edge) = edge {
            // SAFETY: for a non-negative code, Windows guarantees lparam points
            // at a KBDLLHOOKSTRUCT that outlives this call.
            let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let key = info.vkCode as u16;
            if let Some(state) = lock_state().as_mut() {
                state.key(key, edge);
            }
        }
    }
    // SAFETY: the documented way to pass the event on; a null hook handle is
    // ignored by the current API.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for a non-negative code, Windows guarantees lparam points at
        // an MSLLHOOKSTRUCT that outlives this call.
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if let Some((button, edge)) = mouse_event(wparam as u32, info.mouseData)
            && let Some(state) = lock_state().as_mut()
        {
            state.mouse(button, edge);
        }
    }
    // SAFETY: as in `keyboard_proc`.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

fn mouse_event(message: u32, mouse_data: u32) -> Option<(MouseButton, Edge)> {
    match message {
        WM_MBUTTONDOWN => Some((MouseButton::Middle, Edge::Pressed)),
        WM_MBUTTONUP => Some((MouseButton::Middle, Edge::Released)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // Which X button it was lives in the high word of mouseData.
            let button = match ((mouse_data >> 16) & 0xFFFF) as u16 {
                XBUTTON1 => MouseButton::Back,
                XBUTTON2 => MouseButton::Forward,
                _ => return None,
            };
            let edge = if message == WM_XBUTTONDOWN {
                Edge::Pressed
            } else {
                Edge::Released
            };
            Some((button, edge))
        }
        _ => None,
    }
}

/// The state slot is only ever held across a table lookup and a channel send,
/// so a poisoned lock still holds a usable value.
fn lock_state() -> std::sync::MutexGuard<'static, Option<HookState>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clear_state() {
    *lock_state() = None;
}

struct Handle {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        // SAFETY: posting WM_QUIT to a thread id is safe whether or not the
        // thread is still alive; a dead thread simply returns an error.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0;
        let Some(thread) = self.thread.take() else {
            return;
        };
        if !posted {
            // Nothing was woken, so there is no telling when the thread would
            // end; joining could block for good.
            tracing::debug!("the hotkey thread was already gone when it was stopped");
            return;
        }
        // The pump wakes on the posted message, so this returns as soon as the
        // two hooks are removed.
        let _ = thread.join();
    }
}
