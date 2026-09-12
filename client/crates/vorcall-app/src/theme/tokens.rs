//! The token set every widget style reads, and the one iced [`Theme`] built from
//! it.
//!
//! A theme is 29 colours and nothing else: no widget style asks the brand
//! palette for anything, so a custom theme really does repaint the whole window.
//! The mark and the creature are the exception and keep their brand constants.

use std::fmt;

use iced::theme::Palette;
use iced::{Color, Theme};
use serde::{Deserialize, Serialize};

/// Every token, with the group the appearance editor lists it under. The order
/// is the order the editor draws.
pub const TOKEN_NAMES: [(&str, &str); 29] = [
    ("Surfaces", "bg_rail"),
    ("Surfaces", "bg_sidebar"),
    ("Surfaces", "bg_chat"),
    ("Surfaces", "bg_elevated"),
    ("Surfaces", "bg_input"),
    ("Surfaces", "bg_hover"),
    ("Surfaces", "bg_active"),
    ("Surfaces", "bg_selected"),
    ("Lines", "border_subtle"),
    ("Lines", "border_strong"),
    ("Lines", "divider"),
    ("Text", "text_primary"),
    ("Text", "text_secondary"),
    ("Text", "text_muted"),
    ("Text", "text_on_accent"),
    ("Accent", "accent"),
    ("Accent", "accent_hover"),
    ("Accent", "accent_pressed"),
    ("Accent", "accent_tint"),
    ("Controls", "control_bg"),
    ("Controls", "control_bg_hover"),
    ("Controls", "control_border"),
    ("Controls", "track"),
    ("Controls", "thumb"),
    ("Status", "success"),
    ("Status", "warning"),
    ("Status", "danger"),
    ("Status", "online"),
    ("Status", "mention"),
];

/// One theme. `Copy` on purpose: every style closure takes a copy of it rather
/// than borrowing the application state it came from.
///
/// Every field is required in the JSON form — a theme file with a token missing
/// is a broken theme, not a half-applied one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThemeTokens {
    #[serde(with = "hex_color")]
    pub bg_rail: Color,
    #[serde(with = "hex_color")]
    pub bg_sidebar: Color,
    #[serde(with = "hex_color")]
    pub bg_chat: Color,
    #[serde(with = "hex_color")]
    pub bg_elevated: Color,
    #[serde(with = "hex_color")]
    pub bg_input: Color,
    #[serde(with = "hex_color")]
    pub bg_hover: Color,
    #[serde(with = "hex_color")]
    pub bg_active: Color,
    #[serde(with = "hex_color")]
    pub bg_selected: Color,
    #[serde(with = "hex_color")]
    pub border_subtle: Color,
    #[serde(with = "hex_color")]
    pub border_strong: Color,
    #[serde(with = "hex_color")]
    pub divider: Color,
    #[serde(with = "hex_color")]
    pub text_primary: Color,
    #[serde(with = "hex_color")]
    pub text_secondary: Color,
    #[serde(with = "hex_color")]
    pub text_muted: Color,
    #[serde(with = "hex_color")]
    pub text_on_accent: Color,
    #[serde(with = "hex_color")]
    pub accent: Color,
    #[serde(with = "hex_color")]
    pub accent_hover: Color,
    #[serde(with = "hex_color")]
    pub accent_pressed: Color,
    #[serde(with = "hex_color")]
    pub accent_tint: Color,
    #[serde(with = "hex_color")]
    pub control_bg: Color,
    #[serde(with = "hex_color")]
    pub control_bg_hover: Color,
    #[serde(with = "hex_color")]
    pub control_border: Color,
    #[serde(with = "hex_color")]
    pub track: Color,
    #[serde(with = "hex_color")]
    pub thumb: Color,
    #[serde(with = "hex_color")]
    pub success: Color,
    #[serde(with = "hex_color")]
    pub warning: Color,
    #[serde(with = "hex_color")]
    pub danger: Color,
    #[serde(with = "hex_color")]
    pub online: Color,
    #[serde(with = "hex_color")]
    pub mention: Color,
}

impl ThemeTokens {
    /// One token by name, for the appearance editor. `None` for a name this
    /// build does not know.
    pub fn field(&self, name: &str) -> Option<Color> {
        let color = match name {
            "bg_rail" => self.bg_rail,
            "bg_sidebar" => self.bg_sidebar,
            "bg_chat" => self.bg_chat,
            "bg_elevated" => self.bg_elevated,
            "bg_input" => self.bg_input,
            "bg_hover" => self.bg_hover,
            "bg_active" => self.bg_active,
            "bg_selected" => self.bg_selected,
            "border_subtle" => self.border_subtle,
            "border_strong" => self.border_strong,
            "divider" => self.divider,
            "text_primary" => self.text_primary,
            "text_secondary" => self.text_secondary,
            "text_muted" => self.text_muted,
            "text_on_accent" => self.text_on_accent,
            "accent" => self.accent,
            "accent_hover" => self.accent_hover,
            "accent_pressed" => self.accent_pressed,
            "accent_tint" => self.accent_tint,
            "control_bg" => self.control_bg,
            "control_bg_hover" => self.control_bg_hover,
            "control_border" => self.control_border,
            "track" => self.track,
            "thumb" => self.thumb,
            "success" => self.success,
            "warning" => self.warning,
            "danger" => self.danger,
            "online" => self.online,
            "mention" => self.mention,
            _ => return None,
        };
        Some(color)
    }

    /// Writes one token by name, reporting whether the name was known.
    pub fn set_field(&mut self, name: &str, color: Color) -> bool {
        let slot = match name {
            "bg_rail" => &mut self.bg_rail,
            "bg_sidebar" => &mut self.bg_sidebar,
            "bg_chat" => &mut self.bg_chat,
            "bg_elevated" => &mut self.bg_elevated,
            "bg_input" => &mut self.bg_input,
            "bg_hover" => &mut self.bg_hover,
            "bg_active" => &mut self.bg_active,
            "bg_selected" => &mut self.bg_selected,
            "border_subtle" => &mut self.border_subtle,
            "border_strong" => &mut self.border_strong,
            "divider" => &mut self.divider,
            "text_primary" => &mut self.text_primary,
            "text_secondary" => &mut self.text_secondary,
            "text_muted" => &mut self.text_muted,
            "text_on_accent" => &mut self.text_on_accent,
            "accent" => &mut self.accent,
            "accent_hover" => &mut self.accent_hover,
            "accent_pressed" => &mut self.accent_pressed,
            "accent_tint" => &mut self.accent_tint,
            "control_bg" => &mut self.control_bg,
            "control_bg_hover" => &mut self.control_bg_hover,
            "control_border" => &mut self.control_border,
            "track" => &mut self.track,
            "thumb" => &mut self.thumb,
            "success" => &mut self.success,
            "warning" => &mut self.warning,
            "danger" => &mut self.danger,
            "online" => &mut self.online,
            "mention" => &mut self.mention,
            _ => return false,
        };
        *slot = color;
        true
    }

    /// The iced theme the widgets that are not styled by hand fall back to:
    /// the six colours iced generates its own palette from.
    pub fn iced_theme(&self) -> Theme {
        Theme::custom(
            "Vorcall",
            Palette {
                background: self.bg_chat,
                text: self.text_primary,
                primary: self.accent,
                success: self.success,
                warning: self.warning,
                danger: self.danger,
            },
        )
    }
}

/// `#RRGGBB`, or `#RRGGBBAA` when the colour is not fully opaque: the one form a
/// theme file uses.
pub fn to_hex(color: Color) -> String {
    let [r, g, b, a] = color.into_rgba8();
    if a == u8::MAX {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    }
}

/// Reads `#RRGGBB` or `#RRGGBBAA`. Anything else is an error rather than a
/// silent black.
pub fn parse_hex(raw: &str) -> Result<Color, HexError> {
    let body = raw.trim().strip_prefix('#').ok_or(HexError)?;
    if !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HexError);
    }
    let byte = |at: usize| u8::from_str_radix(&body[at..at + 2], 16).map_err(|_| HexError);
    match body.len() {
        6 => Ok(Color::from_rgb8(byte(0)?, byte(2)?, byte(4)?)),
        8 => Ok(Color::from_rgba8(
            byte(0)?,
            byte(2)?,
            byte(4)?,
            f32::from(byte(6)?) / 255.0,
        )),
        _ => Err(HexError),
    }
}

/// What a colour that is not `#RRGGBB` or `#RRGGBBAA` answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexError;

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a colour is #RRGGBB or #RRGGBBAA")
    }
}

impl std::error::Error for HexError {}

/// serde for one token: a colour is a string in the file, never an object.
mod hex_color {
    use iced::Color;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(color: &Color, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::to_hex(*color))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Color, D::Error> {
        let raw = String::deserialize(deserializer)?;
        super::parse_hex(&raw).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::presets::VORCALL_DARK;

    #[test]
    fn the_dark_preset_parses_every_token() {
        let json = serde_json::to_string(&VORCALL_DARK).expect("the preset serialises");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let object = value.as_object().expect("an object of tokens");

        assert_eq!(object.len(), TOKEN_NAMES.len());
        for (_, name) in TOKEN_NAMES {
            let raw = object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{name} is in the file"));
            assert!(parse_hex(raw).is_ok(), "{name} is a colour");
            assert!(VORCALL_DARK.field(name).is_some(), "{name} is readable");
        }
    }

    #[test]
    fn a_theme_round_trips_through_json() {
        let json = serde_json::to_string_pretty(&VORCALL_DARK).expect("the preset serialises");
        let back: ThemeTokens = serde_json::from_str(&json).expect("the preset parses");

        assert_eq!(back, VORCALL_DARK);
        // A fully opaque colour is written without an alpha; one with an alpha
        // of its own keeps it.
        assert_eq!(to_hex(VORCALL_DARK.accent), "#C8102E");
        assert_eq!(to_hex(VORCALL_DARK.bg_hover), "#F1EAE90F");
    }

    #[test]
    fn a_bad_hex_is_an_error() {
        for raw in ["", "C8102E", "#C810", "#GGGGGG", "#C8102E2", "rgb(1,2,3)"] {
            assert!(parse_hex(raw).is_err(), "{raw:?} is not a colour");
        }

        // A token left out is a broken file, not a default.
        assert!(serde_json::from_str::<ThemeTokens>("{\"bg_rail\":\"#000000\"}").is_err());
    }

    #[test]
    fn an_unknown_token_name_is_refused() {
        let mut tokens = VORCALL_DARK;

        assert!(tokens.set_field("accent", Color::BLACK));
        assert_eq!(tokens.field("accent"), Some(Color::BLACK));
        assert!(!tokens.set_field("not_a_token", Color::WHITE));
        assert_eq!(tokens.field("not_a_token"), None);
    }
}
