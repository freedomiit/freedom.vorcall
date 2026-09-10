//! Everything the Vorcall client does that is not drawing: configuration,
//! endpoint resolution, history fetching and the WebSocket connection loop.
//!
//! Deliberately free of any UI dependency.

pub mod config;
pub mod connection;
pub mod endpoints;
pub mod history;

pub use config::Config;
pub use connection::{Command, DisconnectReason, Event};
pub use endpoints::Endpoints;
pub use vorcall_proto::v1::{ChatMessage, ErrorCode, MessagePage};
