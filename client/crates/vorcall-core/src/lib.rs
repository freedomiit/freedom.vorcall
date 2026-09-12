//! Everything the Vorcall client does that is not drawing: configuration,
//! endpoint resolution, the account endpoints, the signed-in session and its
//! tokens, history and image fetching, the moderation endpoints, the permission
//! engine, the WebSocket connection loop and the signed self-update.
//!
//! Deliberately free of any UI dependency.

pub mod admin;
pub mod attachments;
pub mod auth;
pub mod config;
pub mod connection;
pub mod diagnostics;
pub mod endpoints;
pub mod history;
pub mod http;
pub mod images;
pub mod mentions;
pub mod permissions;
pub mod report;
pub mod session;
pub mod update;

pub use config::{Config, Density, Entrance};
pub use connection::{Command, DisconnectReason, Event, MediaKey};
pub use endpoints::Endpoints;
pub use http::ApiFailure;
pub use images::ImagePurpose;
pub use session::Session;
pub use vorcall_proto::v1::{
    Attachment, Ban, Category, Channel, ChannelKind, ChannelPosition, ChatMessage, ErrorCode,
    Image, Invite, InviteCreated, MessagePage, Override, Permission, Profile, Reaction, ReadState,
    ReplyRef, Role, Server, ServerSnapshot, TokenResponse, VoiceMember,
};
