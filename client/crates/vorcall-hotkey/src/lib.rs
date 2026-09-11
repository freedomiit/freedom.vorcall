//! One bound key or mouse button, observed system-wide.
//!
//! Push to talk has to work while the Vorcall window is unfocused, so the bound
//! input is *observed*, never grabbed: every backend leaves the key on its way
//! to whatever application actually has focus. Concretely that means low-level
//! hooks on Windows (which always call the next hook), a listen-only event tap
//! on macOS, XInput2 raw events on X11, and the GlobalShortcuts portal on
//! Wayland — never `RegisterHotKey` or `XGrabKey`.
//!
//! [`Listener::start`] hands press/release edges to an unbounded channel and
//! keeps running until the returned [`Listener`] is dropped. Nothing here knows
//! about iced, audio, or the rest of the client.

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

/// A keyboard key that can be bound to push to talk.
///
/// Modifiers name the logical key, not a side: `Control` matches either control
/// key. There are no chords — one key, held down, is the whole binding.
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
    /// A function key, `1..=24`.
    F(u8),
    /// An ASCII letter (stored lowercase) or digit.
    Char(char),
}

/// A mouse button that can be bound to push to talk.
///
/// The primary and secondary buttons are deliberately absent: binding them
/// would make ordinary clicking transmit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Back,
    Forward,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binding {
    Key(Key),
    Mouse(MouseButton),
}

impl Binding {
    /// Parses the stored form of a binding.
    ///
    /// Accepted: `"Control"`, `"Alt"`, `"Shift"`, `"Super"`, `"Space"`, `"Tab"`,
    /// `"CapsLock"`, `"Insert"`, `"Delete"`, `"Home"`, `"End"`, `"PageUp"`,
    /// `"PageDown"`, `"F1"`..`"F24"`, a single ASCII letter (either case) or
    /// digit, `"MouseBack"`, `"MouseForward"`, `"MouseMiddle"`. Anything else,
    /// including `"Escape"`, `"MouseLeft"`, `"F25"`, `""` and non-ASCII text,
    /// is `None`.
    pub fn parse(name: &str) -> Option<Binding> {
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
            "MouseBack" => return Some(Binding::Mouse(MouseButton::Back)),
            "MouseForward" => return Some(Binding::Mouse(MouseButton::Forward)),
            "MouseMiddle" => return Some(Binding::Mouse(MouseButton::Middle)),
            _ => return parse_function_or_char(name).map(Binding::Key),
        };
        Some(Binding::Key(key))
    }

    /// The stored form, which round-trips through [`Binding::parse`]. Letters
    /// come back lowercase.
    pub fn name(&self) -> String {
        let key = match self {
            Binding::Mouse(MouseButton::Back) => return "MouseBack".to_string(),
            Binding::Mouse(MouseButton::Forward) => return "MouseForward".to_string(),
            Binding::Mouse(MouseButton::Middle) => return "MouseMiddle".to_string(),
            Binding::Key(key) => key,
        };
        match key {
            Key::Control => "Control".to_string(),
            Key::Alt => "Alt".to_string(),
            Key::Shift => "Shift".to_string(),
            Key::Super => "Super".to_string(),
            Key::Space => "Space".to_string(),
            Key::Tab => "Tab".to_string(),
            Key::CapsLock => "CapsLock".to_string(),
            Key::Insert => "Insert".to_string(),
            Key::Delete => "Delete".to_string(),
            Key::Home => "Home".to_string(),
            Key::End => "End".to_string(),
            Key::PageUp => "PageUp".to_string(),
            Key::PageDown => "PageDown".to_string(),
            Key::F(number) => format!("F{number}"),
            Key::Char(character) => character.to_ascii_lowercase().to_string(),
        }
    }

    /// How the binding is spelled in the interface.
    pub fn label(&self) -> String {
        let key = match self {
            Binding::Mouse(MouseButton::Back) => return "Mouse back".to_string(),
            Binding::Mouse(MouseButton::Forward) => return "Mouse forward".to_string(),
            Binding::Mouse(MouseButton::Middle) => return "Mouse middle".to_string(),
            Binding::Key(key) => key,
        };
        match key {
            Key::Control => "Ctrl".to_string(),
            Key::Alt => "Alt".to_string(),
            Key::Shift => "Shift".to_string(),
            Key::Super => "Super".to_string(),
            Key::Space => "Space".to_string(),
            Key::Tab => "Tab".to_string(),
            Key::CapsLock => "Caps Lock".to_string(),
            Key::Insert => "Insert".to_string(),
            Key::Delete => "Delete".to_string(),
            Key::Home => "Home".to_string(),
            Key::End => "End".to_string(),
            Key::PageUp => "Page Up".to_string(),
            Key::PageDown => "Page Down".to_string(),
            Key::F(number) => format!("F{number}"),
            Key::Char(character) => character.to_ascii_uppercase().to_string(),
        }
    }
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

/// A backend's shutdown handle. Dropping the [`Listener`] calls `stop`, which
/// must return within 500 ms.
pub(crate) trait Stop: Send {
    fn stop(&mut self);
}

/// A running system-wide observation of one binding.
///
/// Stops when dropped, which also drops the backend's clone of the sender, so
/// the consumer's stream ends.
pub struct Listener {
    backend: Backend,
    trigger_description: Option<String>,
    stop: Box<dyn Stop>,
}

impl Listener {
    /// Observes `binding` system-wide without grabbing or consuming it.
    ///
    /// Edges are de-duplicated: key auto-repeat never produces a second
    /// `Pressed`, and a `Released` without a preceding `Pressed` is dropped.
    /// Returns within about a second — except on Wayland, where the compositor
    /// may put a confirmation dialog in front of the user first.
    pub fn start(binding: Binding, edges: UnboundedSender<Edge>) -> Result<Listener, Unavailable> {
        tracing::debug!(binding = %binding.name(), "starting the global hotkey listener");

        #[cfg(windows)]
        {
            windows::start(binding, edges)
        }

        #[cfg(target_os = "macos")]
        {
            macos::start(binding, edges)
        }

        #[cfg(target_os = "linux")]
        {
            // A Wayland session usually also exports DISPLAY for XWayland, and
            // XWayland never sees the raw events of native clients, so the
            // portal has to win the tie.
            if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
                wayland::start(binding, edges)
            } else if std::env::var_os("DISPLAY").is_some() {
                x11::start(binding, edges)
            } else {
                Err(Unavailable::Unsupported("no display".to_string()))
            }
        }

        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = (binding, edges);
            Err(Unavailable::Unsupported("unsupported platform".to_string()))
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Wayland only: the compositor's own description of what it bound, which
    /// may differ from what was asked for.
    pub fn trigger_description(&self) -> Option<String> {
        self.trigger_description.clone()
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn new(
        backend: Backend,
        trigger_description: Option<String>,
        stop: Box<dyn Stop>,
    ) -> Listener {
        Listener {
            backend,
            trigger_description,
            stop,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &[
        "Control",
        "Alt",
        "Shift",
        "Super",
        "Space",
        "Tab",
        "CapsLock",
        "Insert",
        "Delete",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "MouseBack",
        "MouseForward",
        "MouseMiddle",
    ];

    #[test]
    fn every_documented_name_round_trips() {
        let mut names: Vec<String> = NAMES.iter().map(|name| name.to_string()).collect();
        names.extend((1..=24).map(|number| format!("F{number}")));
        names.extend(
            "abcdefghijklmnopqrstuvwxyz0123456789"
                .chars()
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

    #[test]
    fn uppercase_letters_normalise_to_lowercase() {
        let binding = Binding::parse("A").expect("an uppercase letter parses");
        assert_eq!(binding, Binding::Key(Key::Char('a')));
        assert_eq!(binding.name(), "a");
        assert_eq!(Binding::parse("a"), Some(binding));
    }

    #[test]
    fn function_keys_parse_only_within_the_supported_range() {
        for number in 1..=24u8 {
            assert_eq!(
                Binding::parse(&format!("F{number}")),
                Some(Binding::Key(Key::F(number))),
                "F{number} should parse"
            );
        }
        assert_eq!(Binding::parse("F0"), None);
        assert_eq!(Binding::parse("F25"), None);
    }

    #[test]
    fn unknown_names_are_rejected() {
        for name in ["Escape", "é", "MouseLeft", "", "ab", "control", "F1x", "-"] {
            assert_eq!(Binding::parse(name), None, "{name} should be rejected");
        }
        // A bare "F" is the letter, not a truncated function key.
        assert_eq!(Binding::parse("F"), Some(Binding::Key(Key::Char('f'))));
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
            ("MouseBack", "Mouse back"),
            ("MouseForward", "Mouse forward"),
            ("MouseMiddle", "Mouse middle"),
        ];
        for (name, label) in cases {
            let binding = Binding::parse(name).expect("the name parses");
            assert_eq!(binding.label(), label, "{name} is mislabelled");
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
}
