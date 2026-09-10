//! Everything the Vorcall client does that is not drawing: configuration,
//! endpoint resolution, the account endpoints, the signed-in session and its
//! tokens, history fetching and the WebSocket connection loop.
//!
//! Deliberately free of any UI dependency.

pub mod auth;
pub mod config;
pub mod connection;
pub mod endpoints;
pub mod history;
pub mod http;
pub mod session;

pub use config::Config;
pub use connection::{Command, DisconnectReason, Event, MediaKey};
pub use endpoints::Endpoints;
pub use http::ApiFailure;
pub use session::Session;
pub use vorcall_proto::v1::{
    ChatMessage, ErrorCode, Member, MessagePage, TokenResponse, VoiceMember,
};
