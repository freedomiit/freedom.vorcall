//! The two themes that ship with the client.
//!
//! Vorcall Dark is the brand: every surface comes off `brand::palette`'s
//! constants. Vorcall Light is hand-tuned from the same accent — it is not a
//! mechanical inversion.

use iced::Color;

use crate::brand::palette;

use super::tokens::ThemeTokens;

pub const DARK_SLUG: &str = "vorcall-dark";
pub const LIGHT_SLUG: &str = "vorcall-light";

/// An alpha byte as iced wants it.
const fn alpha(byte: u8) -> f32 {
    byte as f32 / 255.0
}

pub static VORCALL_DARK: ThemeTokens = ThemeTokens {
    bg_rail: Color::from_rgb8(0x0F, 0x0C, 0x0D),
    bg_sidebar: Color::from_rgb8(0x1A, 0x16, 0x17),
    bg_chat: Color::from_rgb8(0x21, 0x1C, 0x1D),
    bg_elevated: Color::from_rgb8(0x2A, 0x24, 0x26),
    bg_input: Color::from_rgb8(0x17, 0x13, 0x14),
    bg_hover: Color::from_rgba8(0xF1, 0xEA, 0xE9, alpha(0x0F)),
    bg_active: Color::from_rgba8(0xF1, 0xEA, 0xE9, alpha(0x1A)),
    bg_selected: Color::from_rgba8(0xC8, 0x10, 0x2E, alpha(0x2E)),
    border_subtle: Color::from_rgba8(0xF1, 0xEA, 0xE9, alpha(0x14)),
    border_strong: Color::from_rgba8(0xF1, 0xEA, 0xE9, alpha(0x2E)),
    divider: Color::from_rgba8(0xF1, 0xEA, 0xE9, alpha(0x0A)),
    text_primary: palette::INK,
    text_secondary: palette::MUTED,
    text_muted: Color::from_rgb8(0x6F, 0x65, 0x66),
    text_on_accent: palette::INK,
    accent: palette::DEEP,
    accent_hover: Color::from_rgb8(0xD9, 0x21, 0x3F),
    accent_pressed: palette::CREASE,
    accent_tint: Color::from_rgba8(0xC8, 0x10, 0x2E, alpha(0x38)),
    control_bg: palette::KEYS,
    control_bg_hover: palette::STEEL,
    control_border: palette::DECK,
    track: palette::STEEL,
    thumb: palette::INK,
    success: palette::SUCCESS,
    warning: palette::WARNING,
    danger: palette::DANGER,
    online: palette::SUCCESS,
    mention: palette::DEEP,
};

pub static VORCALL_LIGHT: ThemeTokens = ThemeTokens {
    bg_rail: Color::from_rgb8(0xE6, 0xDE, 0xDC),
    bg_sidebar: palette::INK,
    bg_chat: Color::from_rgb8(0xFB, 0xF8, 0xF7),
    bg_elevated: Color::from_rgb8(0xFF, 0xFF, 0xFF),
    bg_input: palette::INK,
    bg_hover: Color::from_rgba8(0x14, 0x10, 0x11, alpha(0x0F)),
    bg_active: Color::from_rgba8(0x14, 0x10, 0x11, alpha(0x1A)),
    bg_selected: Color::from_rgba8(0xC8, 0x10, 0x2E, alpha(0x24)),
    border_subtle: Color::from_rgba8(0x14, 0x10, 0x11, alpha(0x14)),
    border_strong: Color::from_rgba8(0x14, 0x10, 0x11, alpha(0x2E)),
    divider: Color::from_rgba8(0x14, 0x10, 0x11, alpha(0x0A)),
    text_primary: palette::GROUND,
    text_secondary: Color::from_rgb8(0x6A, 0x60, 0x62),
    text_muted: palette::MUTED,
    text_on_accent: palette::INK,
    accent: palette::DEEP,
    accent_hover: Color::from_rgb8(0xD9, 0x21, 0x3F),
    accent_pressed: palette::CREASE,
    accent_tint: Color::from_rgba8(0xC8, 0x10, 0x2E, alpha(0x2A)),
    control_bg: Color::from_rgb8(0xE6, 0xDE, 0xDC),
    control_bg_hover: Color::from_rgb8(0xDA, 0xD0, 0xCE),
    control_border: palette::MUTED,
    track: Color::from_rgb8(0xDA, 0xD0, 0xCE),
    thumb: palette::STEEL,
    success: palette::SUCCESS,
    warning: palette::WARNING,
    danger: palette::DANGER,
    online: Color::from_rgb8(0x3F, 0xA8, 0x5B),
    mention: palette::DEEP,
};

/// The presets, with the name the appearance tab lists them under.
pub const PRESETS: [(&str, &str); 2] = [(DARK_SLUG, "Vorcall Dark"), (LIGHT_SLUG, "Vorcall Light")];

/// One preset by the name `Config::theme` stores.
pub fn by_name(name: &str) -> Option<&'static ThemeTokens> {
    match name {
        DARK_SLUG => Some(&VORCALL_DARK),
        LIGHT_SLUG => Some(&VORCALL_LIGHT),
        _ => None,
    }
}

/// What the appearance tab calls a preset.
pub fn label(slug: &str) -> Option<&'static str> {
    PRESETS
        .iter()
        .find(|(name, _)| *name == slug)
        .map(|(_, label)| *label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_is_found_by_the_name_the_configuration_stores() {
        assert_eq!(by_name(DARK_SLUG), Some(&VORCALL_DARK));
        assert_eq!(by_name(LIGHT_SLUG), Some(&VORCALL_LIGHT));
        assert_eq!(by_name("custom:mine"), None);
        assert_eq!(by_name(""), None);
    }

    /// The two presets are different themes, not one with a flipped background.
    #[test]
    fn the_two_presets_share_the_accent_but_not_the_surfaces() {
        assert_eq!(VORCALL_DARK.accent, VORCALL_LIGHT.accent);
        assert_ne!(VORCALL_DARK.bg_chat, VORCALL_LIGHT.bg_chat);
        assert_ne!(VORCALL_DARK.text_primary, VORCALL_LIGHT.text_primary);
        assert_ne!(VORCALL_DARK.online, VORCALL_LIGHT.online);
    }
}
