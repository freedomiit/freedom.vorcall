//! Brand colours and the two application themes.

use std::sync::LazyLock;

use iced::theme::{Palette, Style};
use iced::{Color, Theme};

/// "Deep", the brand red.
pub const DEEP: Color = Color::from_rgb8(0xC8, 0x10, 0x2E);
/// Darker Deep: the creases between the fingers in the rare entrance.
pub const CREASE: Color = Color::from_rgb8(0x8F, 0x0B, 0x22);
pub const GROUND: Color = Color::from_rgb8(0x14, 0x10, 0x11);
pub const INK: Color = Color::from_rgb8(0xF1, 0xEA, 0xE9);
pub const MUTED: Color = Color::from_rgb8(0xA0, 0x93, 0x93);
pub const SUCCESS: Color = Color::from_rgb8(0x5C, 0xC7, 0x75);
pub const WARNING: Color = Color::from_rgb8(0xF2, 0xB8, 0x47);
pub const DANGER: Color = Color::from_rgb8(0xEB, 0x5E, 0x5E);

const PALETTE: Palette = Palette {
    background: GROUND,
    text: INK,
    primary: DEEP,
    success: SUCCESS,
    warning: WARNING,
    danger: DANGER,
};

static THEME: LazyLock<Theme> = LazyLock::new(|| Theme::custom("Vorcall", PALETTE));
/// The splash window keeps the same palette on a transparent ground, which is what the
/// compositor clears it with.
static SPLASH_THEME: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "Vorcall splash",
        Palette {
            background: Color::TRANSPARENT,
            ..PALETTE
        },
    )
});

pub fn theme() -> Theme {
    THEME.clone()
}

pub fn splash_theme() -> Theme {
    SPLASH_THEME.clone()
}

/// The window's clear colour and default text colour, taken straight from the palette so a
/// transparent background stays transparent.
pub fn style(theme: &Theme) -> Style {
    let palette = theme.palette();
    Style {
        background_color: palette.background,
        text_color: palette.text,
    }
}
