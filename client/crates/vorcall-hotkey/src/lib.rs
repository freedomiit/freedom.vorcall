//! Several bound keys or mouse buttons, observed system-wide.
//!
//! Push to talk has to work while the Vorcall window is unfocused, so the bound
//! input is *observed*, never grabbed: every backend leaves the key on its way
//! to whatever application actually has focus. Concretely that means low-level
//! hooks on Windows (which always call the next hook), a listen-only event tap
//! on macOS, XInput2 raw events on X11, and the GlobalShortcuts portal on
//! Wayland — never `RegisterHotKey` or `XGrabKey`.
//!
//! [`Listener::start`] takes one [`Shortcut`] per action, hands `(action, edge)`
//! pairs to an unbounded channel and keeps running until the returned
//! [`Listener`] is dropped. Nothing here knows about iced, audio, or the rest of
//! the client.

pub mod keymap;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

use futures::channel::mpsc::UnboundedSender;

/// Which action an edge belongs to.
///
/// The crate never interprets the number — the app's keybinding map owns what
/// each one means — except on Wayland, where action 0 is described to the
/// compositor as push to talk.
pub type ActionId = u32;

/// A keyboard key that can be bound.
///
/// Modifiers name the logical key, not a side: `Control` matches either control
/// key. A chord is a key plus the `ctrl`/`shift`/`alt` flags of its [`Binding`],
/// so a modifier appears here only when it is the trigger itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Control,
    Alt,
    Shift,
    Super,
    Space,
    Tab,
    CapsLock,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Enter,
    Escape,
    Backspace,
    Comma,
    Period,
    Slash,
    Semicolon,
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backquote,
    Quote,
    Backslash,
    /// A function key, `1..=24`.
    F(u8),
    /// An ASCII letter (stored lowercase) or digit.
    Char(char),
}

/// A mouse button that can be bound.
///
/// The primary and secondary buttons are deliberately absent: binding them
/// would make ordinary clicking transmit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Back,
    Forward,
    Middle,
}

/// The input a binding is held on, before its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    Key(Key),
    Mouse(MouseButton),
}

/// One trigger plus the modifiers that have to be held with it.
///
/// The stored form is `[Ctrl+][Shift+][Alt+]<trigger>`, always in that order. A
/// modifier prefix that repeats the trigger (`Ctrl+Control`) is not a binding,
/// and [`Binding::parse`] rejects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub trigger: Trigger,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Binding {
    /// A binding on `trigger` alone, which fires whatever the modifiers are
    /// doing.
    pub fn simple(trigger: Trigger) -> Binding {
        Binding {
            trigger,
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    /// Parses the stored form, `[Ctrl+][Shift+][Alt+]<trigger>`.
    ///
    /// `<trigger>` is `"Control"`, `"Alt"`, `"Shift"`, `"Super"`, `"Space"`,
    /// `"Tab"`, `"CapsLock"`, `"Insert"`, `"Delete"`, `"Home"`, `"End"`,
    /// `"PageUp"`, `"PageDown"`, `"ArrowUp"`, `"ArrowDown"`, `"ArrowLeft"`,
    /// `"ArrowRight"`, `"Enter"`, `"Escape"`, `"Backspace"`, `"Comma"`,
    /// `"Period"`, `"Slash"`, `"Semicolon"`, `"Minus"`, `"Equal"`,
    /// `"BracketLeft"`, `"BracketRight"`, `"Backquote"`, `"Quote"`,
    /// `"Backslash"`, `"F1"`..`"F24"`, a single ASCII letter (either case) or
    /// digit, `"MouseBack"`, `"MouseForward"`, `"MouseMiddle"`. The punctuation
    /// character a name like `"Comma"` stands for is an alias of it, so `","`
    /// and `"Comma"` are the same binding and a file written by an earlier build
    /// still reads. Anything else — prefixes out of order, a prefix that repeats
    /// the trigger, `"MouseLeft"`, `"F25"`, `""`, non-ASCII text — is `None`.
    pub fn parse(name: &str) -> Option<Binding> {
        let mut rest = name;
        let ctrl = strip(&mut rest, "Ctrl+");
        let shift = strip(&mut rest, "Shift+");
        let alt = strip(&mut rest, "Alt+");
        let binding = Binding {
            trigger: parse_trigger(rest)?,
            ctrl,
            shift,
            alt,
        };
        binding.coherent().then_some(binding)
    }

    /// The stored form, which round-trips through [`Binding::parse`]. Letters
    /// come back lowercase.
    pub fn name(&self) -> String {
        let mut name = self.prefix();
        name.push_str(&trigger_name(self.trigger));
        name
    }

    /// How the binding is spelled in the interface.
    pub fn label(&self) -> String {
        let mut label = self.prefix();
        label.push_str(&trigger_label(self.trigger));
        label
    }

    /// The modifier prefixes, in the one order the grammar accepts. The
    /// interface spells them the same way the stored form does.
    fn prefix(&self) -> String {
        let mut prefix = String::new();
        if self.ctrl {
            prefix.push_str("Ctrl+");
        }
        if self.shift {
            prefix.push_str("Shift+");
        }
        if self.alt {
            prefix.push_str("Alt+");
        }
        prefix
    }

    /// The modifiers this binding needs held.
    pub(crate) fn mods(&self) -> Mods {
        Mods {
            ctrl: self.ctrl,
            shift: self.shift,
            alt: self.alt,
        }
    }

    /// False when a modifier prefix repeats the trigger, which no keyboard can
    /// ever satisfy as a chord.
    fn coherent(&self) -> bool {
        !matches!(
            (self.trigger, self.ctrl, self.shift, self.alt),
            (Trigger::Key(Key::Control), true, _, _)
                | (Trigger::Key(Key::Shift), _, true, _)
                | (Trigger::Key(Key::Alt), _, _, true)
        )
    }
}

/// Consumes `prefix` from the front of `rest`, reporting whether it was there.
fn strip(rest: &mut &str, prefix: &str) -> bool {
    match rest.strip_prefix(prefix) {
        Some(tail) => {
            *rest = tail;
            true
        }
        None => false,
    }
}

fn parse_trigger(name: &str) -> Option<Trigger> {
    let key = match name {
        "Control" => Key::Control,
        "Alt" => Key::Alt,
        "Shift" => Key::Shift,
        "Super" => Key::Super,
        "Space" => Key::Space,
        "Tab" => Key::Tab,
        "CapsLock" => Key::CapsLock,
        "Insert" => Key::Insert,
        "Delete" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "Enter" => Key::Enter,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        // Each of these also parses as the character it is, which is both what
        // iced reports for the key and what an earlier build stored.
        "Comma" | "," => Key::Comma,
        "Period" | "." => Key::Period,
        "Slash" | "/" => Key::Slash,
        "Semicolon" | ";" => Key::Semicolon,
        "Minus" | "-" => Key::Minus,
        "Equal" | "=" => Key::Equal,
        "BracketLeft" | "[" => Key::BracketLeft,
        "BracketRight" | "]" => Key::BracketRight,
        "Backquote" | "`" => Key::Backquote,
        "Quote" | "'" => Key::Quote,
        "Backslash" | "\\" => Key::Backslash,
        "MouseBack" => return Some(Trigger::Mouse(MouseButton::Back)),
        "MouseForward" => return Some(Trigger::Mouse(MouseButton::Forward)),
        "MouseMiddle" => return Some(Trigger::Mouse(MouseButton::Middle)),
        _ => return parse_function_or_char(name).map(Trigger::Key),
    };
    Some(Trigger::Key(key))
}

fn trigger_name(trigger: Trigger) -> String {
    let key = match trigger {
        Trigger::Mouse(MouseButton::Back) => return "MouseBack".to_string(),
        Trigger::Mouse(MouseButton::Forward) => return "MouseForward".to_string(),
        Trigger::Mouse(MouseButton::Middle) => return "MouseMiddle".to_string(),
        Trigger::Key(key) => key,
    };
    let name = match key {
        Key::Control => "Control",
        Key::Alt => "Alt",
        Key::Shift => "Shift",
        Key::Super => "Super",
        Key::Space => "Space",
        Key::Tab => "Tab",
        Key::CapsLock => "CapsLock",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::ArrowUp => "ArrowUp",
        Key::ArrowDown => "ArrowDown",
        Key::ArrowLeft => "ArrowLeft",
        Key::ArrowRight => "ArrowRight",
        Key::Enter => "Enter",
        Key::Escape => "Escape",
        Key::Backspace => "Backspace",
        Key::Comma => "Comma",
        Key::Period => "Period",
        Key::Slash => "Slash",
        Key::Semicolon => "Semicolon",
        Key::Minus => "Minus",
        Key::Equal => "Equal",
        Key::BracketLeft => "BracketLeft",
        Key::BracketRight => "BracketRight",
        Key::Backquote => "Backquote",
        Key::Quote => "Quote",
        Key::Backslash => "Backslash",
        Key::F(number) => return format!("F{number}"),
        Key::Char(character) => return character.to_ascii_lowercase().to_string(),
    };
    name.to_string()
}

fn trigger_label(trigger: Trigger) -> String {
    let key = match trigger {
        Trigger::Mouse(MouseButton::Back) => return "Mouse back".to_string(),
        Trigger::Mouse(MouseButton::Forward) => return "Mouse forward".to_string(),
        Trigger::Mouse(MouseButton::Middle) => return "Mouse middle".to_string(),
        Trigger::Key(key) => key,
    };
    let label = match key {
        Key::Control => "Ctrl",
        Key::Alt => "Alt",
        Key::Shift => "Shift",
        Key::Super => "Super",
        Key::Space => "Space",
        Key::Tab => "Tab",
        Key::CapsLock => "Caps Lock",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "Page Up",
        Key::PageDown => "Page Down",
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::Enter => "Enter",
        Key::Escape => "Esc",
        Key::Backspace => "Backspace",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Slash => "/",
        Key::Semicolon => ";",
        Key::Minus => "-",
        Key::Equal => "=",
        Key::BracketLeft => "[",
        Key::BracketRight => "]",
        Key::Backquote => "`",
        Key::Quote => "'",
        Key::Backslash => "\\",
        Key::F(number) => return format!("F{number}"),
        Key::Char(character) => return character.to_ascii_uppercase().to_string(),
    };
    label.to_string()
}

fn parse_function_or_char(name: &str) -> Option<Key> {
    if let Some(digits) = name.strip_prefix('F')
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        let number: u8 = digits.parse().ok()?;
        return (1..=24).contains(&number).then_some(Key::F(number));
    }
    let mut characters = name.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    if character.is_ascii_alphabetic() {
        Some(Key::Char(character.to_ascii_lowercase()))
    } else if character.is_ascii_digit() {
        Some(Key::Char(character))
    } else {
        None
    }
}

/// One binding to observe, under the action it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub action: ActionId,
    pub binding: Binding,
    /// What the compositor shows the user on Wayland, where every shortcut is
    /// confirmed by hand. Empty asks for this crate's own wording.
    pub description: String,
}

/// The shortcuts a backend took, each as the action it belongs to, the modifiers
/// it needs held, and whatever the backend matches its own events against.
pub(crate) type Bound<T> = Vec<(ActionId, Mods, T)>;

/// The actions a backend could not observe at all, each with the reason it gave.
pub(crate) type Skipped = Vec<(ActionId, Unavailable)>;

/// Splits the wanted shortcuts into the ones a backend can observe and the ones
/// its own rule turns down, so one unobservable binding costs only itself
/// instead of the whole listener.
///
/// `target` is the backend's rule: whatever it matches its own events against,
/// or why this binding can never reach it.
pub(crate) fn partition<T>(
    bindings: &[Shortcut],
    mut target: impl FnMut(&Shortcut) -> Result<T, Unavailable>,
) -> (Bound<T>, Skipped) {
    let mut bound = Vec::new();
    let mut skipped = Vec::new();
    for shortcut in bindings {
        match target(shortcut) {
            Ok(target) => bound.push((shortcut.action, shortcut.binding.mods(), target)),
            Err(reason) => skipped.push((shortcut.action, reason)),
        }
    }
    (bound, skipped)
}

/// What a backend answers when its rule turned every binding down: the first
/// reason, which is the wording the interface shows.
pub(crate) fn nothing_bindable(skipped: &[(ActionId, Unavailable)]) -> Unavailable {
    skipped
        .first()
        .map(|(_, reason)| reason.clone())
        .unwrap_or_else(|| Unavailable::Failed("no bindings".to_string()))
}

/// The pairs of actions that share a binding, each as `(lower id, higher id)`.
///
/// Three actions on one binding are three pairs, so every clash the interface
/// has to warn about appears.
pub fn conflicts(bindings: &[(ActionId, Binding)]) -> Vec<(ActionId, ActionId)> {
    let mut pairs = Vec::new();
    for (offset, (action, binding)) in bindings.iter().enumerate() {
        for (other, other_binding) in &bindings[offset + 1..] {
            if binding == other_binding {
                pairs.push(if action <= other {
                    (*action, *other)
                } else {
                    (*other, *action)
                });
            }
        }
    }
    pairs
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Pressed,
    Released,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    WindowsHook,
    MacEventTap,
    X11Raw,
    WaylandPortal,
}

/// Why a system-wide listener could not be started.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// The platform, session or binding cannot be observed at all — the
    /// interface should offer the in-window fallback instead of retrying.
    #[error("{0}")]
    Unsupported(String),
    /// The user has to grant something before this can work.
    #[error("{0}")]
    PermissionDenied(String),
    #[error("{0}")]
    Failed(String),
}

/// Which of the three modifiers are held, or are required.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mods {
    pub(crate) ctrl: bool,
    pub(crate) shift: bool,
    pub(crate) alt: bool,
}

impl Mods {
    /// Whether everything `required` asks for is held. Extra modifiers never
    /// stand in the way, which is what keeps an unmodified binding firing the
    /// way it always has.
    pub(crate) fn contains(self, required: Mods) -> bool {
        (self.ctrl || !required.ctrl)
            && (self.shift || !required.shift)
            && (self.alt || !required.alt)
    }

    /// How many modifiers this asks for, which is how specific a binding on it
    /// is.
    pub(crate) fn count(self) -> u32 {
        u32::from(self.ctrl) + u32::from(self.shift) + u32::from(self.alt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modifier {
    Ctrl,
    Shift,
    Alt,
}

/// Which modifier keys are down, one entry per platform code.
///
/// Per code rather than per modifier, so that releasing the left shift while the
/// right one is still held does not report shift as up, and so that a missed
/// event can only strand one code instead of drifting a count.
#[derive(Debug)]
pub(crate) struct ModifierKeys<C> {
    keys: Vec<(C, Modifier, bool)>,
}

impl<C: PartialEq> ModifierKeys<C> {
    pub(crate) fn new(keys: impl IntoIterator<Item = (C, Modifier)>) -> ModifierKeys<C> {
        ModifierKeys {
            keys: keys
                .into_iter()
                .map(|(code, modifier)| (code, modifier, false))
                .collect(),
        }
    }

    pub(crate) fn note(&mut self, code: &C, edge: Edge) {
        for (candidate, _, held) in &mut self.keys {
            if candidate == code {
                *held = edge == Edge::Pressed;
            }
        }
    }

    pub(crate) fn mods(&self) -> Mods {
        let mut mods = Mods::default();
        for (_, modifier, held) in &self.keys {
            if !*held {
                continue;
            }
            match modifier {
                Modifier::Ctrl => mods.ctrl = true,
                Modifier::Shift => mods.shift = true,
                Modifier::Alt => mods.alt = true,
            }
        }
        mods
    }
}

/// Drops auto-repeat and unpaired releases so consumers see one `Pressed` per
/// physical press.
#[derive(Debug, Default)]
pub(crate) struct EdgeFilter {
    held: bool,
}

impl EdgeFilter {
    pub(crate) fn admit(&mut self, edge: Edge) -> bool {
        match edge {
            Edge::Pressed if self.held => false,
            Edge::Pressed => {
                self.held = true;
                true
            }
            Edge::Released if self.held => {
                self.held = false;
                true
            }
            Edge::Released => false,
        }
    }
}

/// The edge logic every backend shares: which actions a trigger belongs to,
/// which modifiers each of them needs, and the `(action, edge)` pairs that fall
/// out of the two.
///
/// `T` is whatever the backend matches its own events against — virtual keys,
/// keycodes, a portal shortcut id.
pub(crate) struct Router<T> {
    slots: Vec<Slot<T>>,
    held: Mods,
    edges: UnboundedSender<(ActionId, Edge)>,
}

struct Slot<T> {
    action: ActionId,
    target: T,
    required: Mods,
    /// The trigger is physically down, whatever the modifiers are doing.
    down: bool,
    filter: EdgeFilter,
}

impl<T> Slot<T> {
    fn emit(&mut self, edges: &UnboundedSender<(ActionId, Edge)>, edge: Edge) {
        if self.filter.admit(edge) {
            let _ = edges.unbounded_send((self.action, edge));
        }
    }
}

impl<T> Router<T> {
    pub(crate) fn new(edges: UnboundedSender<(ActionId, Edge)>) -> Router<T> {
        Router {
            slots: Vec::new(),
            held: Mods::default(),
            edges,
        }
    }

    pub(crate) fn push(&mut self, action: ActionId, required: Mods, target: T) {
        self.slots.push(Slot {
            action,
            target,
            required,
            down: false,
            filter: EdgeFilter::default(),
        });
    }

    /// The consumer hung up, so the backend's loop can stop.
    #[cfg(target_os = "linux")]
    pub(crate) fn closed(&self) -> bool {
        self.edges.is_closed()
    }

    /// A trigger went down or up; `matches` picks the slots it belongs to.
    ///
    /// A press only fires while the slot's modifiers are held. Pressing a
    /// modifier afterwards never starts the chord: the trigger is what the user
    /// presses last.
    ///
    /// Several slots on one trigger can be satisfied at once, because extra
    /// modifiers never stand in the way: `Alt+Shift+ArrowDown` satisfies a slot
    /// bound to `Alt+ArrowDown` as well. Only the most specific of them — the
    /// ones asking for the most modifiers — fire, so the chord the user pressed
    /// does not also fire the looser binding underneath it. Slots that ask for
    /// the same modifiers all fire, as they always have.
    pub(crate) fn trigger(&mut self, edge: Edge, matches: impl Fn(&T) -> bool) {
        let Router { slots, held, edges } = self;
        let specific = slots
            .iter()
            .filter(|slot| matches(&slot.target) && held.contains(slot.required))
            .map(|slot| slot.required.count())
            .max();
        for slot in slots.iter_mut() {
            if !matches(&slot.target) {
                continue;
            }
            match edge {
                Edge::Pressed => {
                    slot.down = true;
                    if held.contains(slot.required) && specific == Some(slot.required.count()) {
                        slot.emit(edges, Edge::Pressed);
                    }
                }
                Edge::Released => {
                    slot.down = false;
                    slot.emit(edges, Edge::Released);
                }
            }
        }
    }

    /// The held modifiers changed. A chord whose modifier went up is released
    /// even though its trigger is still down.
    pub(crate) fn set_mods(&mut self, mods: Mods) {
        if mods == self.held {
            return;
        }
        self.held = mods;
        let Router { slots, edges, .. } = self;
        for slot in slots.iter_mut() {
            if slot.down && !mods.contains(slot.required) {
                slot.emit(edges, Edge::Released);
            }
        }
    }
}

/// A backend's shutdown handle. Dropping the [`Listener`] calls `stop`, which
/// must return within 500 ms.
pub(crate) trait Stop: Send {
    fn stop(&mut self);
}

/// A running system-wide observation of a set of bindings.
///
/// Stops when dropped, which also drops the backend's clone of the sender, so
/// the consumer's stream ends.
pub struct Listener {
    backend: Backend,
    trigger_descriptions: Vec<(ActionId, String)>,
    unavailable: Skipped,
    stop: Box<dyn Stop>,
}

impl Listener {
    /// Observes every binding in `bindings` system-wide without grabbing or
    /// consuming it.
    ///
    /// An action is `Pressed` when its trigger goes down while all of its
    /// modifiers are held, and `Released` when its trigger goes up or one of its
    /// modifiers does. Edges are de-duplicated per action: key auto-repeat never
    /// produces a second `Pressed`, and a `Released` without a preceding
    /// `Pressed` is dropped. Two actions may share a binding, and both fire.
    ///
    /// A binding this platform cannot observe at all — a mouse button under
    /// Wayland, Caps Lock on macOS, a key the X11 layout has no code for — is
    /// skipped rather than fatal: the rest of the set is still observed, and
    /// [`Listener::unavailable`] names what was left out so the caller can hold
    /// those actions itself. Only a set where nothing could be bound is an
    /// `Err`.
    ///
    /// Returns within about a second — except on Wayland, where the compositor
    /// may put a confirmation dialog in front of the user first.
    pub fn start(
        bindings: Vec<Shortcut>,
        edges: UnboundedSender<(ActionId, Edge)>,
    ) -> Result<Listener, Unavailable> {
        if bindings.is_empty() {
            return Err(Unavailable::Failed("no bindings".to_string()));
        }
        tracing::debug!(
            bindings = %describe(&bindings),
            "starting the global hotkey listener"
        );

        #[cfg(windows)]
        {
            windows::start(bindings, edges)
        }

        #[cfg(target_os = "macos")]
        {
            macos::start(bindings, edges)
        }

        #[cfg(target_os = "linux")]
        {
            // A Wayland session usually also exports DISPLAY for XWayland, and
            // XWayland never sees the raw events of native clients, so the
            // portal has to win the tie.
            if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
                wayland::start(bindings, edges)
            } else if std::env::var_os("DISPLAY").is_some() {
                x11::start(bindings, edges)
            } else {
                Err(Unavailable::Unsupported("no display".to_string()))
            }
        }

        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = (bindings, edges);
            Err(Unavailable::Unsupported("unsupported platform".to_string()))
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Wayland only: the compositor's own description of what it bound for
    /// `action`, which may differ from what was asked for.
    pub fn trigger_description(&self, action: ActionId) -> Option<String> {
        self.trigger_descriptions
            .iter()
            .find(|(bound, _)| *bound == action)
            .map(|(_, description)| description.clone())
    }

    /// The actions nothing was bound for, each with the reason. Every other
    /// action of the set is observed system-wide; these are the caller's own to
    /// hold while its window has focus.
    pub fn unavailable(&self) -> &[(ActionId, Unavailable)] {
        &self.unavailable
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn new(
        backend: Backend,
        trigger_descriptions: Vec<(ActionId, String)>,
        unavailable: Skipped,
        stop: Box<dyn Stop>,
    ) -> Listener {
        Listener {
            backend,
            trigger_descriptions,
            unavailable,
            stop,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

fn describe(bindings: &[Shortcut]) -> String {
    bindings
        .iter()
        .map(|shortcut| format!("{}={}", shortcut.action, shortcut.binding.name()))
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc::UnboundedReceiver;

    fn key(key: Key) -> Binding {
        Binding::simple(Trigger::Key(key))
    }

    fn parse(name: &str) -> Binding {
        Binding::parse(name).unwrap_or_else(|| panic!("{name} parses"))
    }

    #[test]
    fn every_documented_name_round_trips() {
        let mut names: Vec<String> = keymap::every_key()
            .into_iter()
            .map(|key| trigger_name(Trigger::Key(key)))
            .collect();
        names.extend(
            ["MouseBack", "MouseForward", "MouseMiddle"]
                .into_iter()
                .map(String::from),
        );

        for name in names {
            let binding = Binding::parse(&name).expect("the documented name parses");
            assert_eq!(binding.name(), name, "{name} does not round-trip");
            assert_eq!(
                Binding::parse(&binding.name()),
                Some(binding),
                "{name} does not re-parse"
            );
        }
    }

    /// The names the client stored before modifiers existed.
    #[test]
    fn the_old_stored_names_still_parse() {
        assert_eq!(Binding::parse("Control"), Some(key(Key::Control)));
        assert_eq!(Binding::parse("F7"), Some(key(Key::F(7))));
        assert_eq!(
            Binding::parse("MouseBack"),
            Some(Binding::simple(Trigger::Mouse(MouseButton::Back)))
        );
    }

    #[test]
    fn modified_names_round_trip() {
        let cases = [
            ("Ctrl+Shift+m", Key::Char('m'), (true, true, false)),
            ("Ctrl+Comma", Key::Comma, (true, false, false)),
            ("Alt+ArrowUp", Key::ArrowUp, (false, false, true)),
            ("Shift+Alt+ArrowDown", Key::ArrowDown, (false, true, true)),
            ("Ctrl+Shift+Alt+F4", Key::F(4), (true, true, true)),
            ("Ctrl+Shift", Key::Shift, (true, false, false)),
        ];
        for (name, trigger, (ctrl, shift, alt)) in cases {
            let binding = parse(name);
            assert_eq!(
                binding,
                Binding {
                    trigger: Trigger::Key(trigger),
                    ctrl,
                    shift,
                    alt,
                },
                "{name} parses wrongly"
            );
            assert_eq!(binding.name(), name, "{name} does not round-trip");
        }
        let mouse = parse("Ctrl+MouseMiddle");
        assert_eq!(mouse.trigger, Trigger::Mouse(MouseButton::Middle));
        assert!(mouse.ctrl);
        assert_eq!(mouse.name(), "Ctrl+MouseMiddle");
    }

    #[test]
    fn a_prefix_that_repeats_its_trigger_is_not_a_binding() {
        for name in ["Ctrl+Control", "Shift+Shift", "Alt+Alt", "Ctrl+Shift+Shift"] {
            assert_eq!(Binding::parse(name), None, "{name} should be rejected");
        }
        // A modifier as the trigger of a different modifier is still a binding.
        assert!(Binding::parse("Ctrl+Alt").is_some());
        assert!(Binding::parse("Shift+Control").is_some());
    }

    #[test]
    fn uppercase_letters_normalise_to_lowercase() {
        let binding = Binding::parse("A").expect("an uppercase letter parses");
        assert_eq!(binding, key(Key::Char('a')));
        assert_eq!(binding.name(), "a");
        assert_eq!(Binding::parse("a"), Some(binding));
    }

    #[test]
    fn function_keys_parse_only_within_the_supported_range() {
        for number in 1..=24u8 {
            assert_eq!(
                Binding::parse(&format!("F{number}")),
                Some(key(Key::F(number))),
                "F{number} should parse"
            );
        }
        assert_eq!(Binding::parse("F0"), None);
        assert_eq!(Binding::parse("F25"), None);
    }

    /// The character a punctuation key types is an alias of its name, which is
    /// both what iced reports and what an earlier build stored.
    #[test]
    fn punctuation_characters_parse_as_their_names() {
        let cases = [
            (",", Key::Comma),
            (".", Key::Period),
            ("/", Key::Slash),
            (";", Key::Semicolon),
            ("-", Key::Minus),
            ("=", Key::Equal),
            ("[", Key::BracketLeft),
            ("]", Key::BracketRight),
            ("`", Key::Backquote),
            ("'", Key::Quote),
            ("\\", Key::Backslash),
        ];
        for (character, wanted) in cases {
            assert_eq!(
                Binding::parse(character),
                Some(key(wanted)),
                "{character} should parse as {wanted:?}"
            );
        }
        assert_eq!(Binding::parse("Ctrl+,"), Binding::parse("Ctrl+Comma"));
        // The stored form stays the name, so a re-save canonicalises it.
        assert_eq!(parse("Ctrl+,").name(), "Ctrl+Comma");
        assert_eq!(parse("Ctrl+,").label(), "Ctrl+,");
    }

    #[test]
    fn unknown_names_are_rejected() {
        for name in [
            "é",
            "MouseLeft",
            "",
            "ab",
            "control",
            "F1x",
            "Arrow",
            "ctrl+m",
            "Ctrl+",
            "Ctrl",
            // The prefixes only parse in the documented order.
            "Shift+Ctrl+m",
            "Alt+Ctrl+m",
            "Alt+Shift+Ctrl+m",
        ] {
            assert_eq!(Binding::parse(name), None, "{name} should be rejected");
        }
        // A bare "F" is the letter, not a truncated function key.
        assert_eq!(Binding::parse("F"), Some(key(Key::Char('f'))));
    }

    #[test]
    fn labels_read_the_way_the_interface_spells_them() {
        let cases = [
            ("Control", "Ctrl"),
            ("Alt", "Alt"),
            ("Shift", "Shift"),
            ("Super", "Super"),
            ("Space", "Space"),
            ("CapsLock", "Caps Lock"),
            ("PageUp", "Page Up"),
            ("PageDown", "Page Down"),
            ("F8", "F8"),
            ("a", "A"),
            ("5", "5"),
            ("Enter", "Enter"),
            ("Escape", "Esc"),
            ("MouseBack", "Mouse back"),
            ("MouseForward", "Mouse forward"),
            ("MouseMiddle", "Mouse middle"),
            ("Ctrl+Shift+m", "Ctrl+Shift+M"),
            ("Ctrl+Comma", "Ctrl+,"),
            ("Alt+ArrowUp", "Alt+↑"),
            ("Alt+ArrowDown", "Alt+↓"),
            ("Alt+ArrowLeft", "Alt+←"),
            ("Alt+ArrowRight", "Alt+→"),
            ("Ctrl+Shift+Alt+Backslash", "Ctrl+Shift+Alt+\\"),
        ];
        for (name, label) in cases {
            assert_eq!(parse(name).label(), label, "{name} is mislabelled");
        }
    }

    /// The macOS backend refuses `Key::CapsLock` outright. This pins the two
    /// table facts that make that refusal correct: macOS reports Caps Lock only
    /// through `FlagsChanged`, and there is no flag for it to test.
    #[test]
    fn caps_lock_has_a_macos_keycode_but_no_macos_modifier_flag() {
        assert_eq!(keymap::mac_modifier_flag(Key::CapsLock), None);
        assert!(!keymap::mac_keycodes(Key::CapsLock).is_empty());
    }

    #[test]
    fn no_shared_binding_is_no_conflict() {
        let bindings = [
            (0, key(Key::Control)),
            (1, parse("Ctrl+Shift+m")),
            (2, parse("Ctrl+Shift+d")),
        ];
        assert!(conflicts(&bindings).is_empty());
    }

    #[test]
    fn a_shared_binding_is_one_pair_in_ascending_order() {
        let bindings = [
            (7, parse("Ctrl+Shift+m")),
            (1, key(Key::Control)),
            (3, parse("Ctrl+Shift+m")),
        ];
        assert_eq!(conflicts(&bindings), vec![(3, 7)]);
    }

    #[test]
    fn three_actions_on_one_binding_are_three_pairs() {
        let bindings = [
            (1, parse("Alt+ArrowUp")),
            (2, parse("Alt+ArrowUp")),
            (3, parse("Alt+ArrowUp")),
        ];
        assert_eq!(conflicts(&bindings), vec![(1, 2), (1, 3), (2, 3)]);
    }

    /// A binding that differs only in its modifiers is a different binding.
    #[test]
    fn modifiers_are_part_of_the_comparison() {
        let bindings = [(1, parse("Ctrl+m")), (2, parse("m"))];
        assert!(conflicts(&bindings).is_empty());
    }

    fn drain(edges: &mut UnboundedReceiver<(ActionId, Edge)>) -> Vec<(ActionId, Edge)> {
        let mut seen = Vec::new();
        while let Ok(edge) = edges.try_recv() {
            seen.push(edge);
        }
        seen
    }

    /// Drives the shared edge logic the way a backend does: the target is a
    /// made-up code the matcher compares against.
    fn router(
        bindings: &[(ActionId, Binding, u16)],
    ) -> (Router<u16>, UnboundedReceiver<(ActionId, Edge)>) {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let mut router = Router::new(sender);
        for (action, binding, code) in bindings {
            router.push(*action, binding.mods(), *code);
        }
        (router, receiver)
    }

    fn press(router: &mut Router<u16>, code: u16) {
        router.trigger(Edge::Pressed, |target| *target == code);
    }

    fn release(router: &mut Router<u16>, code: u16) {
        router.trigger(Edge::Released, |target| *target == code);
    }

    fn only(ctrl: bool, shift: bool, alt: bool) -> Mods {
        Mods { ctrl, shift, alt }
    }

    #[test]
    fn auto_repeat_does_not_produce_a_second_press() {
        let mut filter = EdgeFilter::default();
        assert!(filter.admit(Edge::Pressed));
        assert!(!filter.admit(Edge::Pressed));
    }

    #[test]
    fn a_release_without_a_press_is_dropped() {
        let mut filter = EdgeFilter::default();
        assert!(!filter.admit(Edge::Released));
    }

    #[test]
    fn a_full_press_release_press_passes_every_edge() {
        let mut filter = EdgeFilter::default();
        let admitted = [Edge::Pressed, Edge::Released, Edge::Pressed]
            .into_iter()
            .filter(|edge| filter.admit(*edge))
            .count();
        assert_eq!(admitted, 3);
    }

    #[test]
    fn a_repeat_never_yields_a_second_press_per_action() {
        let (mut router, mut edges) = router(&[(4, key(Key::Control), 1)]);
        press(&mut router, 1);
        press(&mut router, 1);
        press(&mut router, 1);
        assert_eq!(drain(&mut edges), vec![(4, Edge::Pressed)]);
        release(&mut router, 1);
        assert_eq!(drain(&mut edges), vec![(4, Edge::Released)]);
    }

    #[test]
    fn an_unpaired_release_is_dropped_per_action() {
        let (mut router, mut edges) = router(&[(4, key(Key::Control), 1)]);
        release(&mut router, 1);
        assert!(drain(&mut edges).is_empty());
    }

    #[test]
    fn an_unmodified_binding_fires_whatever_else_is_held() {
        let (mut router, mut edges) = router(&[(0, key(Key::Control), 1)]);
        router.set_mods(only(true, true, true));
        press(&mut router, 1);
        assert_eq!(drain(&mut edges), vec![(0, Edge::Pressed)]);
        release(&mut router, 1);
        assert_eq!(drain(&mut edges), vec![(0, Edge::Released)]);
    }

    #[test]
    fn a_chord_only_fires_while_its_modifiers_are_held() {
        let (mut router, mut edges) = router(&[(2, parse("Ctrl+Shift+m"), 77)]);
        press(&mut router, 77);
        assert!(drain(&mut edges).is_empty(), "no modifier was held");
        release(&mut router, 77);

        router.set_mods(only(true, false, false));
        press(&mut router, 77);
        assert!(drain(&mut edges).is_empty(), "shift was missing");
        release(&mut router, 77);

        router.set_mods(only(true, true, false));
        press(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(2, Edge::Pressed)]);
    }

    /// Letting go of a required modifier releases the action once, and the
    /// trigger's own release then adds nothing.
    #[test]
    fn a_modifier_release_while_held_yields_released_once() {
        let (mut router, mut edges) = router(&[(5, parse("Ctrl+m"), 77)]);
        router.set_mods(only(true, false, false));
        press(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(5, Edge::Pressed)]);

        router.set_mods(Mods::default());
        assert_eq!(drain(&mut edges), vec![(5, Edge::Released)]);

        release(&mut router, 77);
        assert!(drain(&mut edges).is_empty());
    }

    /// Pressing the modifier after the trigger is not a chord: the trigger is
    /// what the user presses last.
    #[test]
    fn a_modifier_pressed_after_the_trigger_does_not_start_a_chord() {
        let (mut router, mut edges) = router(&[(5, parse("Ctrl+m"), 77)]);
        press(&mut router, 77);
        router.set_mods(only(true, false, false));
        assert!(drain(&mut edges).is_empty());
    }

    #[test]
    fn two_actions_on_the_same_binding_both_fire() {
        let (mut router, mut edges) =
            router(&[(1, key(Key::Char('m')), 77), (2, key(Key::Char('m')), 77)]);
        press(&mut router, 77);
        assert_eq!(
            drain(&mut edges),
            vec![(1, Edge::Pressed), (2, Edge::Pressed)]
        );
        release(&mut router, 77);
        assert_eq!(
            drain(&mut edges),
            vec![(1, Edge::Released), (2, Edge::Released)]
        );
    }

    /// The two default arrow chords: `Alt+ArrowDown` is the next channel and
    /// `Shift+Alt+ArrowDown` the next unread one, and pressing the chord must not
    /// move the channel as well.
    #[test]
    fn only_the_most_specific_chord_on_a_trigger_fires() {
        let (mut router, mut edges) = router(&[
            (1, parse("Alt+ArrowDown"), 108),
            (2, parse("Shift+Alt+ArrowDown"), 108),
        ]);
        router.set_mods(only(false, true, true));
        press(&mut router, 108);
        assert_eq!(drain(&mut edges), vec![(2, Edge::Pressed)]);
        release(&mut router, 108);
        assert_eq!(drain(&mut edges), vec![(2, Edge::Released)]);

        // Without shift the looser binding is the most specific one satisfied.
        router.set_mods(only(false, false, true));
        press(&mut router, 108);
        assert_eq!(drain(&mut edges), vec![(1, Edge::Pressed)]);
    }

    #[test]
    fn a_modified_press_does_not_also_fire_the_bare_binding() {
        let (mut router, mut edges) =
            router(&[(1, key(Key::Char('m')), 77), (2, parse("Ctrl+m"), 77)]);
        router.set_mods(only(true, false, false));
        press(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(2, Edge::Pressed)]);
        release(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(2, Edge::Released)]);

        router.set_mods(Mods::default());
        press(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(1, Edge::Pressed)]);
    }

    fn shortcut(action: ActionId, name: &str) -> Shortcut {
        Shortcut {
            action,
            binding: parse(name),
            description: String::new(),
        }
    }

    #[test]
    fn a_rule_that_turns_one_binding_down_keeps_the_others() {
        let reason =
            Unavailable::Unsupported("mouse buttons are window-only on Wayland".to_string());
        let bindings = [
            shortcut(0, "Control"),
            shortcut(1, "MouseBack"),
            shortcut(2, "F8"),
        ];
        let (bound, skipped) = partition(&bindings, |shortcut| match shortcut.binding.trigger {
            Trigger::Mouse(_) => Err(reason.clone()),
            Trigger::Key(_) => Ok(shortcut.action),
        });

        assert_eq!(
            bound
                .iter()
                .map(|(action, _, _)| *action)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(skipped, vec![(1, reason)]);
    }

    #[test]
    fn a_rule_that_turns_every_binding_down_answers_with_the_first_reason() {
        let reason =
            Unavailable::Unsupported("the current keyboard layout has no such key".to_string());
        let bindings = [shortcut(0, "Control"), shortcut(1, "F8")];
        let (bound, skipped) = partition(&bindings, |_| Err::<(), Unavailable>(reason.clone()));

        assert!(bound.is_empty());
        assert_eq!(skipped.len(), 2);
        assert_eq!(nothing_bindable(&skipped), reason);
    }

    /// An action whose modifiers are not held sits out a press its neighbour on
    /// the same trigger takes.
    #[test]
    fn only_the_actions_whose_modifiers_are_held_fire() {
        let (mut router, mut edges) =
            router(&[(1, key(Key::Char('m')), 77), (2, parse("Ctrl+m"), 77)]);
        press(&mut router, 77);
        assert_eq!(drain(&mut edges), vec![(1, Edge::Pressed)]);
    }

    #[test]
    fn an_empty_binding_list_is_refused() {
        let (sender, _edges) = futures::channel::mpsc::unbounded();
        assert_eq!(
            Listener::start(Vec::new(), sender).err(),
            Some(Unavailable::Failed("no bindings".to_string()))
        );
    }
}
