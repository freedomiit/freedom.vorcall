//! The application state, split by area, plus the pure rules that decide what it
//! does.
//!
//! Nothing here draws and nothing here talks to the network: a handler in
//! `app::update` is the only thing that changes any of it.

pub mod chat;
pub mod crop;
pub mod rules;
pub mod server;
pub mod settings;
pub mod sound;
pub mod sticker;
pub mod ui;
pub mod update;
pub mod voice;
