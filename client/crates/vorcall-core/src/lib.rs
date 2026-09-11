//! Everything the Vorcall client does that is not drawing: configuration,
//! endpoint resolution, the account endpoints, the signed-in session and its
//! tokens, history fetching, the WebSocket connection loop and the signed
//! self-update.
//!
//! Deliberately free of any UI dependency.

pub mod attachments;
pub mod auth;
pub mod config;
pub mod connection;
pub mod endpoints;
pub mod history;
pub mod http;
pub mod mentions;
pub mod session;
pub mod update;

pub use config::Config;
pub use connection::{Command, DisconnectReason, Event, MediaKey};
pub use endpoints::Endpoints;
pub use http::ApiFailure;
pub use session::Session;
pub use vorcall_proto::v1::{
    Attachment, ChatMessage, ErrorCode, Member, MessagePage, Reaction, ReplyRef, Room, RoomEntry,
    RoomKind, TokenResponse, VoiceMember,
};
