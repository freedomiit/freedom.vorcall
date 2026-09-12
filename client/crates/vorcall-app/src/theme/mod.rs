//! Themes: the token set, the two presets, the custom themes on disk and the
//! widget styles every view reads them through.

pub mod file;
pub mod presets;
pub mod styles;
pub mod tokens;

pub use presets::VORCALL_DARK;
pub use tokens::ThemeTokens;
