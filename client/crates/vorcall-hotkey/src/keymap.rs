//! Per-platform codes for every [`Key`] and [`MouseButton`].
//!
//! Plain data, compiled on every OS so the tables stay testable from a Linux
//! CI box. A key that names a modifier maps to several codes — one per side of
//! the keyboard, plus the side-agnostic virtual key on Windows — because a
//! binding is a logical key, not a physical one.

use crate::{Binding, Key, MouseButton};

/// Windows virtual-key codes.
pub fn windows_vks(key: Key) -> Vec<u16> {
    match key {
        Key::Control => vec![0x11, 0xA2, 0xA3],
        Key::Alt => vec![0x12, 0xA4, 0xA5],
        Key::Shift => vec![0x10, 0xA0, 0xA1],
        Key::Super => vec![0x5B, 0x5C],
        Key::Space => vec![0x20],
        Key::Tab => vec![0x09],
        Key::CapsLock => vec![0x14],
        Key::Insert => vec![0x2D],
        Key::Delete => vec![0x2E],
        Key::Home => vec![0x24],
        Key::End => vec![0x23],
        Key::PageUp => vec![0x21],
        Key::PageDown => vec![0x22],
        Key::F(number) => vec![0x70 + u16::from(number) - 1],
        // VK codes for the alphanumeric block are the uppercase ASCII values.
        Key::Char(character) => vec![character.to_ascii_uppercase() as u16],
    }
}

/// macOS virtual key codes (`kVK_*`).
///
/// F21..F24 have no `kVK` constant and come back empty. The character keys are
/// positional, so the table assumes the ANSI US layout — the same assumption
/// every `kVK_ANSI_*` constant makes.
pub fn mac_keycodes(key: Key) -> Vec<u16> {
    match key {
        Key::Control => vec![0x3B, 0x3E],
        Key::Alt => vec![0x3A, 0x3D],
        Key::Shift => vec![0x38, 0x3C],
        Key::Super => vec![0x37, 0x36],
        Key::Space => vec![0x31],
        Key::Tab => vec![0x30],
        Key::CapsLock => vec![0x39],
        // Help occupies the Insert position on a full-size Apple keyboard.
        Key::Insert => vec![0x72],
        Key::Delete => vec![0x75],
        Key::Home => vec![0x73],
        Key::End => vec![0x77],
        Key::PageUp => vec![0x74],
        Key::PageDown => vec![0x79],
        Key::F(number) => mac_function_key(number)
            .map(|code| vec![code])
            .unwrap_or_default(),
        Key::Char(character) => mac_character_key(character)
            .map(|code| vec![code])
            .unwrap_or_default(),
    }
}

fn mac_function_key(number: u8) -> Option<u16> {
    const CODES: [u16; 20] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71,
        0x6A, 0x40, 0x4F, 0x50, 0x5A,
    ];
    CODES.get(usize::from(number).checked_sub(1)?).copied()
}

fn mac_character_key(character: char) -> Option<u16> {
    let code = match character.to_ascii_lowercase() {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => 0x09,
        'b' => 0x0B,
        'q' => 0x0C,
        'w' => 0x0D,
        'e' => 0x0E,
        'r' => 0x0F,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '9' => 0x19,
        '7' => 0x1A,
        '8' => 0x1C,
        '0' => 0x1D,
        'o' => 0x1F,
        'u' => 0x20,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        'k' => 0x28,
        'n' => 0x2D,
        'm' => 0x2E,
        _ => return None,
    };
    Some(code)
}

/// The `CGEventFlags` bit a modifier key raises, for keys that have one.
///
/// Modifiers arrive as `FlagsChanged` rather than key events, so this is how
/// their press and release are read.
pub fn mac_modifier_flag(key: Key) -> Option<u64> {
    match key {
        Key::Control => Some(0x0004_0000),
        Key::Alt => Some(0x0008_0000),
        Key::Shift => Some(0x0002_0000),
        Key::Super => Some(0x0010_0000),
        _ => None,
    }
}

/// X11 keysyms.
pub fn x11_keysyms(key: Key) -> Vec<u32> {
    match key {
        Key::Control => vec![0xffe3, 0xffe4],
        Key::Alt => vec![0xffe9, 0xffea],
        Key::Shift => vec![0xffe1, 0xffe2],
        Key::Super => vec![0xffeb, 0xffec],
        Key::Space => vec![0x20],
        Key::Tab => vec![0xff09],
        Key::CapsLock => vec![0xffe5],
        Key::Insert => vec![0xff63],
        Key::Delete => vec![0xffff],
        Key::Home => vec![0xff50],
        Key::End => vec![0xff57],
        Key::PageUp => vec![0xff55],
        Key::PageDown => vec![0xff56],
        Key::F(number) => vec![0xffbe + u32::from(number) - 1],
        // Latin-1 keysyms are the ASCII values of the unshifted character.
        Key::Char(character) => vec![character.to_ascii_lowercase() as u32],
    }
}

/// X11 button numbers, as they appear in `RawButtonPress`/`RawButtonRelease`.
pub fn x11_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Middle => 2,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// macOS button numbers, as they appear in `MouseEventButtonNumber`.
pub fn mac_button(button: MouseButton) -> i64 {
    match button {
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// The preferred trigger handed to the GlobalShortcuts portal, spelled the way
/// the XDG shortcuts specification wants it.
///
/// Mouse buttons have no spelling there, so they return `None`.
pub fn xdg_trigger(binding: &Binding) -> Option<String> {
    let key = match binding {
        Binding::Key(key) => key,
        Binding::Mouse(_) => return None,
    };
    let trigger = match key {
        Key::Control => "CTRL".to_string(),
        Key::Alt => "ALT".to_string(),
        Key::Shift => "SHIFT".to_string(),
        Key::Super => "LOGO".to_string(),
        Key::Space => "space".to_string(),
        Key::Tab => "Tab".to_string(),
        Key::CapsLock => "Caps_Lock".to_string(),
        Key::Insert => "Insert".to_string(),
        Key::Delete => "Delete".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::PageUp => "Page_Up".to_string(),
        Key::PageDown => "Page_Down".to_string(),
        Key::F(number) => format!("F{number}"),
        Key::Char(character) => character.to_string(),
    };
    Some(trigger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn every_key() -> Vec<Key> {
        let mut keys = vec![
            Key::Control,
            Key::Alt,
            Key::Shift,
            Key::Super,
            Key::Space,
            Key::Tab,
            Key::CapsLock,
            Key::Insert,
            Key::Delete,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
        ];
        keys.extend((1..=24).map(Key::F));
        keys.extend(
            "abcdefghijklmnopqrstuvwxyz0123456789"
                .chars()
                .map(Key::Char),
        );
        keys
    }

    /// Fails when two keys claim the same platform code: a binding on one would
    /// then also fire on the other.
    fn assert_codes_are_unique<T: Copy + Eq + std::hash::Hash + std::fmt::Debug>(
        platform: &str,
        codes: impl Fn(Key) -> Vec<T>,
    ) {
        let mut owner: HashMap<T, Key> = HashMap::new();
        for key in every_key() {
            for code in codes(key) {
                if let Some(previous) = owner.insert(code, key) {
                    panic!("{platform}: {code:?} is claimed by both {previous:?} and {key:?}");
                }
            }
        }
    }

    #[test]
    fn windows_codes_cover_every_key_and_never_collide() {
        for key in every_key() {
            assert!(
                !windows_vks(key).is_empty(),
                "{key:?} has no Windows virtual key"
            );
        }
        assert_codes_are_unique("windows", windows_vks);
    }

    #[test]
    fn mac_codes_cover_every_key_except_the_high_function_keys() {
        for key in every_key() {
            let expected = !matches!(key, Key::F(21..=24));
            assert_eq!(
                !mac_keycodes(key).is_empty(),
                expected,
                "{key:?} has the wrong macOS coverage"
            );
        }
        assert_codes_are_unique("macos", mac_keycodes);
    }

    #[test]
    fn x11_codes_cover_every_key_and_never_collide() {
        for key in every_key() {
            assert!(!x11_keysyms(key).is_empty(), "{key:?} has no X11 keysym");
        }
        assert_codes_are_unique("x11", x11_keysyms);
    }

    #[test]
    fn mac_modifier_flags_belong_to_the_modifiers_only() {
        for key in every_key() {
            let expected = matches!(key, Key::Control | Key::Alt | Key::Shift | Key::Super);
            assert_eq!(
                mac_modifier_flag(key).is_some(),
                expected,
                "{key:?} has the wrong macOS modifier flag"
            );
        }
    }

    #[test]
    fn known_codes_match_the_platform_constants() {
        // Spot checks against the platform headers rather than against the
        // tables themselves: VK_SPACE, kVK_ANSI_A, XK_Tab, XK_F1.
        assert_eq!(windows_vks(Key::Space), vec![0x20]);
        assert_eq!(windows_vks(Key::F(1)), vec![0x70]);
        assert_eq!(windows_vks(Key::F(24)), vec![0x87]);
        assert_eq!(windows_vks(Key::Char('a')), vec![0x41]);
        assert_eq!(mac_keycodes(Key::Char('a')), vec![0x00]);
        assert_eq!(mac_keycodes(Key::F(12)), vec![0x6F]);
        assert_eq!(x11_keysyms(Key::Tab), vec![0xff09]);
        assert_eq!(x11_keysyms(Key::F(1)), vec![0xffbe]);
        assert_eq!(x11_keysyms(Key::Char('z')), vec![0x7a]);
    }

    #[test]
    fn xdg_triggers_exist_for_keys_and_never_for_mouse_buttons() {
        for key in every_key() {
            assert!(
                xdg_trigger(&Binding::Key(key)).is_some(),
                "{key:?} has no XDG trigger"
            );
        }
        for button in [MouseButton::Back, MouseButton::Forward, MouseButton::Middle] {
            assert_eq!(xdg_trigger(&Binding::Mouse(button)), None);
        }
    }

    #[test]
    fn mouse_buttons_map_to_the_documented_numbers() {
        assert_eq!(x11_button(MouseButton::Middle), 2);
        assert_eq!(x11_button(MouseButton::Back), 8);
        assert_eq!(x11_button(MouseButton::Forward), 9);
        assert_eq!(mac_button(MouseButton::Middle), 2);
        assert_eq!(mac_button(MouseButton::Back), 3);
        assert_eq!(mac_button(MouseButton::Forward), 4);
    }
}
