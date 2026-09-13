//! The client connection loop from `PROTOCOL.md`:
//!
//! `Disconnected -> Connecting -> AwaitingWelcome -> Connected -> (close, error
//! or 75 s of silence) -> Backoff -> Connecting`.
//!
//! [`run`] owns the socket and the session: it keeps the access token fresh,
//! consumes [`Command`]s from the UI and reports every transition as an
//! [`Event`]. It never blocks its caller and never panics on a network result.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::stream::FuturesUnordered;
use futures::{Sink, SinkExt, Stream, StreamExt};
use prost::Message as _;
use tokio::time::{Instant, interval_at, sleep, sleep_until, timeout_at};
use tokio_tungstenite::tungstenite::{
    self, Utf8Bytes,
    client::IntoClientRequest,
    handshake::client::Request,
    http::{HeaderValue, StatusCode},
    protocol::{CloseFrame, frame::coding::CloseCode},
};
use vorcall_proto::v1::{
    Attachment, Ban, BanMember, Category, Channel, ChannelKind, ChannelPosition, ChatMessage,
    ClientFrame, CreateCategory, CreateChannel, CreateRole, DeleteCategory, DeleteChannel,
    DeleteMessage, DeleteRole, DeleteSound, EditMessage, ErrorCode, Hello, Image, Invite,
    InviteCreated, JoinVoice, KickMember, LeaveVoice, MarkRead, MessagePage, OpenDm, Override,
    Ping, PlaySound, Profile, React, Reaction, ReorderCategories, ReorderChannels, ReorderRoles,
    Role, SendMessage, Server, ServerFrame, ServerSnapshot, SetMemberRoles, SetNickname,
    SetOverride, Sound, StartShare, StopShare, StopSound, StreamRequest, StreamedFile,
    TransferOwnership, UnbanMember, UnwatchShare, UpdateCategory, UpdateChannel, UpdateProfile,
    UpdateRole, UpdateServer, UpdateSound, VoiceMember, VoiceModerate, VoiceSelfState, WatchShare,
    client_frame, server_frame,
};

use crate::admin;
use crate::attachments;
use crate::auth;
use crate::endpoints::Endpoints;
use crate::history;
use crate::http::{self, ApiFailure};
use crate::images::{self, ImagePurpose};
use crate::session::{self, Session};
use crate::streams::{self, StreamError};
use crate::update;

type WsMessage = tungstenite::Message;

const PROTOCOL_VERSION: u32 = 1;
/// The server answers `Hello` well inside its own 5 s deadline.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const SILENCE_TIMEOUT: Duration = Duration::from_secs(75);
/// A bare 401 means a stale build, not a blip: retry at the backoff cap.
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(30);
/// `PROTOCOL.md`'s admin close codes: neither is worth reconnecting after.
const KICKED_CLOSE_CODE: u16 = 4001;
/// The owner locked the account out of signing in; not the in-app ban.
const DISABLED_CLOSE_CODE: u16 = 4003;
const HISTORY_LIMIT: u32 = 100;
/// `PROTOCOL.md` stops gap-filling here; beyond it a gap may remain.
const GAP_FILL_MAX_PAGES: usize = 5;
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const BACKOFF_JITTER: f64 = 0.20;
/// Attachment and image transfers this loop runs at once; the rest wait.
const TRANSFER_SLOTS: usize = 2;
/// The most of an attachment a preview holds in memory. Only a picture is ever
/// drawn inline and `PROTOCOL.md` § Limits caps a picture at
/// [`images::MAX_BYTES`]; the file itself may be gigabytes now, and goes
/// through `attachments::download_to_path` onto the disk instead.
const PREVIEW_MAX_BYTES: u64 = images::MAX_BYTES;

#[derive(Debug, Clone)]
pub enum Command {
    Send {
        channel_id: i64,
        text: String,
        /// The message this one answers; `None` when it answers nothing.
        reply_to_id: Option<i64>,
        attachment_ids: Vec<i64>,
        /// Files this sender has offered for the channel and not yet linked to
        /// a message. Nothing was uploaded: the bytes stay on this disk and are
        /// read back out of this client.
        streamed_file_ids: Vec<i64>,
    },
    /// The newest page of a channel, asked for when the UI first opens it.
    LoadHistory {
        channel_id: i64,
    },
    LoadOlder {
        channel_id: i64,
        before: i64,
    },
    OpenDm {
        user_id: i64,
    },
    MarkRead {
        channel_id: i64,
        message_id: i64,
    },
    Edit {
        id: i64,
        text: String,
    },
    Delete {
        id: i64,
    },
    React {
        message_id: i64,
        emoji: String,
        remove: bool,
    },
    /// `request_id` is the UI's own handle on this transfer: whichever event
    /// answers it carries the same number back.
    UploadAttachment {
        request_id: u64,
        channel_id: i64,
        file_name: String,
        content_type: String,
        bytes: Blob,
    },
    /// A file on disk rather than bytes in hand: it streams straight off the
    /// disk, so sending a gigabyte never costs a gigabyte of memory.
    UploadAttachmentFile {
        request_id: u64,
        channel_id: i64,
        path: PathBuf,
        content_type: String,
    },
    FetchAttachment {
        request_id: u64,
        id: i64,
    },
    UploadImage {
        request_id: u64,
        purpose: ImagePurpose,
        content_type: String,
        bytes: Blob,
    },
    FetchImage {
        request_id: u64,
        id: i64,
    },
    /// Offers a local file for the others to read out of this client. Nothing
    /// is uploaded: the file stays at `path` and is served on demand.
    OfferStream {
        request_id: u64,
        channel_id: i64,
        path: PathBuf,
        content_type: String,
        size: u64,
    },
    /// Reads one streamed file onto the disk at `save_to`.
    FetchStream {
        request_id: u64,
        id: i64,
        save_to: PathBuf,
    },
    /// The answer to an [`Event::StreamRequested`]: `path` is the local file the
    /// app resolved the stream id to against its own registry. A file it found
    /// gone it declines itself, and that never reaches this loop.
    ServeStream {
        stream_id: i64,
        transfer_id: i64,
        offset: i64,
        length: i64,
        path: PathBuf,
    },
    /// Stops the transfer `request_id` names, in flight or still queued.
    CancelTransfer {
        request_id: u64,
    },
    JoinVoice {
        channel_id: i64,
        /// The joiner's own switches, so the channel never sees a flash of
        /// unmuted while the first `VoiceSelfState` is in flight.
        self_muted: bool,
        self_deafened: bool,
    },
    LeaveVoice {
        channel_id: i64,
    },
    /// The caller's own mute and deafen, for the other clients to draw. Display
    /// only: the relay is never told, so a self-mute stays this user's to undo.
    VoiceSelfState {
        channel_id: i64,
        muted: bool,
        deafened: bool,
    },
    /// Sent once the local capture is running; idempotent server-side.
    StartShare {
        channel_id: i64,
        audio: bool,
    },
    StopShare {
        channel_id: i64,
    },
    /// Replaces any previous watch.
    WatchShare {
        channel_id: i64,
        user_id: i64,
    },
    UnwatchShare {
        channel_id: i64,
    },
    /// Plays a soundpad clip into a voice session. Needs SOUNDPAD in that
    /// channel and a live voice session there; one clip plays per channel, so
    /// this replaces whatever was playing.
    PlaySound {
        channel_id: i64,
        sound_id: i64,
    },
    /// Stops the clip playing in that channel. The server allows it to whoever
    /// started it, or to a holder of MANAGE_SOUNDS.
    StopSound {
        channel_id: i64,
    },
    UpdateSound {
        sound_id: i64,
        name: String,
    },
    DeleteSound {
        sound_id: i64,
    },
    /// One management frame. Nothing is awaited: the server answers with an
    /// `Error` or with the delta the change produced.
    Admin(AdminCommand),
    /// One REST request, answered by [`Event::RestResult`].
    Rest(RestRequest),
}

impl Command {
    /// The variant's name, for log lines — never the payload (message text,
    /// ban reasons, file names).
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Send { .. } => "Send",
            Self::LoadHistory { .. } => "LoadHistory",
            Self::LoadOlder { .. } => "LoadOlder",
            Self::OpenDm { .. } => "OpenDm",
            Self::MarkRead { .. } => "MarkRead",
            Self::Edit { .. } => "Edit",
            Self::Delete { .. } => "Delete",
            Self::React { .. } => "React",
            Self::UploadAttachment { .. } => "UploadAttachment",
            Self::UploadAttachmentFile { .. } => "UploadAttachmentFile",
            Self::FetchAttachment { .. } => "FetchAttachment",
            Self::UploadImage { .. } => "UploadImage",
            Self::FetchImage { .. } => "FetchImage",
            Self::OfferStream { .. } => "OfferStream",
            Self::FetchStream { .. } => "FetchStream",
            Self::ServeStream { .. } => "ServeStream",
            Self::CancelTransfer { .. } => "CancelTransfer",
            Self::JoinVoice { .. } => "JoinVoice",
            Self::LeaveVoice { .. } => "LeaveVoice",
            Self::VoiceSelfState { .. } => "VoiceSelfState",
            Self::StartShare { .. } => "StartShare",
            Self::StopShare { .. } => "StopShare",
            Self::WatchShare { .. } => "WatchShare",
            Self::UnwatchShare { .. } => "UnwatchShare",
            Self::PlaySound { .. } => "PlaySound",
            Self::StopSound { .. } => "StopSound",
            Self::UpdateSound { .. } => "UpdateSound",
            Self::DeleteSound { .. } => "DeleteSound",
            Self::Admin(inner) => inner.kind_name(),
            Self::Rest(_) => "Rest",
        }
    }
}

/// The management frames of `PROTOCOL.md`, one variant each, their fields
/// mirroring the schema.
#[derive(Debug, Clone)]
pub enum AdminCommand {
    CreateChannel {
        kind: ChannelKind,
        name: String,
        topic: String,
        category_id: i64,
    },
    UpdateChannel {
        id: i64,
        name: String,
        topic: String,
    },
    DeleteChannel {
        id: i64,
    },
    CreateCategory {
        name: String,
    },
    UpdateCategory {
        id: i64,
        name: String,
    },
    DeleteCategory {
        id: i64,
    },
    ReorderChannels {
        positions: Vec<ChannelPosition>,
    },
    ReorderCategories {
        ids: Vec<i64>,
    },
    /// `allow == deny == 0` deletes the override.
    SetOverride {
        channel_id: i64,
        override_: Override,
    },
    CreateRole {
        name: String,
        color: u32,
        icon_emoji: String,
        icon_image_id: i64,
        permissions: u64,
        hoist: bool,
    },
    UpdateRole {
        role: Role,
    },
    DeleteRole {
        id: i64,
    },
    ReorderRoles {
        ids: Vec<i64>,
    },
    SetMemberRoles {
        user_id: i64,
        role_ids: Vec<i64>,
    },
    /// `user_id` 0 is the caller's own nickname; an empty one clears it.
    SetNickname {
        user_id: i64,
        nickname: String,
    },
    KickMember {
        user_id: i64,
    },
    BanMember {
        user_id: i64,
        reason: String,
    },
    UnbanMember {
        user_id: i64,
    },
    UpdateServer {
        name: String,
        description: String,
        icon_image_id: i64,
    },
    TransferOwnership {
        user_id: i64,
    },
    /// Each flag travels only when the UI actually set it; a `move_to` of
    /// `Some(0)` disconnects the target instead of moving it.
    VoiceModerate {
        user_id: i64,
        channel_id: i64,
        muted: Option<bool>,
        deafened: Option<bool>,
        move_to: Option<i64>,
    },
    UpdateProfile {
        description: String,
        accent_color: u32,
        avatar_image_id: i64,
        banner_image_id: i64,
    },
}

impl AdminCommand {
    /// The frame's name, for log lines and for [`Event::AdminDropped`].
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::CreateChannel { .. } => "CreateChannel",
            Self::UpdateChannel { .. } => "UpdateChannel",
            Self::DeleteChannel { .. } => "DeleteChannel",
            Self::CreateCategory { .. } => "CreateCategory",
            Self::UpdateCategory { .. } => "UpdateCategory",
            Self::DeleteCategory { .. } => "DeleteCategory",
            Self::ReorderChannels { .. } => "ReorderChannels",
            Self::ReorderCategories { .. } => "ReorderCategories",
            Self::SetOverride { .. } => "SetOverride",
            Self::CreateRole { .. } => "CreateRole",
            Self::UpdateRole { .. } => "UpdateRole",
            Self::DeleteRole { .. } => "DeleteRole",
            Self::ReorderRoles { .. } => "ReorderRoles",
            Self::SetMemberRoles { .. } => "SetMemberRoles",
            Self::SetNickname { .. } => "SetNickname",
            Self::KickMember { .. } => "KickMember",
            Self::BanMember { .. } => "BanMember",
            Self::UnbanMember { .. } => "UnbanMember",
            Self::UpdateServer { .. } => "UpdateServer",
            Self::TransferOwnership { .. } => "TransferOwnership",
            Self::VoiceModerate { .. } => "VoiceModerate",
            Self::UpdateProfile { .. } => "UpdateProfile",
        }
    }
}

/// One REST request the UI is waiting on; `request_id` is its handle on it.
#[derive(Debug, Clone)]
pub struct RestRequest {
    pub request_id: u64,
    pub kind: RestKind,
}

#[derive(Debug, Clone)]
pub enum RestKind {
    ListInvites,
    CreateInvite { days: u32 },
    RevokeInvite { id: i64 },
    ListBans,
    ListMembers,
}

/// What one finished [`RestRequest`] hands back.
#[derive(Clone)]
pub enum RestOutcome {
    Invites(Vec<Invite>),
    InviteCreated(InviteCreated),
    InviteRevoked { id: i64 },
    Bans(Vec<Ban>),
    Members(Vec<Profile>),
}

impl fmt::Debug for RestOutcome {
    /// A fresh invite code is a credential, and every [`Event`] is printed
    /// whole in a log line: this one variant hides it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invites(invites) => f.debug_tuple("Invites").field(invites).finish(),
            Self::InviteCreated(created) => write!(
                f,
                "InviteCreated {{ id: {}, code: <redacted>, expires_at_unix_ms: {} }}",
                created.id, created.expires_at_unix_ms
            ),
            Self::InviteRevoked { id } => f.debug_struct("InviteRevoked").field("id", id).finish(),
            Self::Bans(bans) => f.debug_tuple("Bans").field(bans).finish(),
            Self::Members(members) => f.debug_tuple("Members").field(members).finish(),
        }
    }
}

/// The per-session media key from `VoiceReady`. Debug never prints it.
#[derive(Clone)]
pub struct MediaKey(pub [u8; 32]);

impl fmt::Debug for MediaKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaKey(<redacted>)")
    }
}

/// One attachment's or image's bytes, shared rather than copied: a retry
/// re-reads the very same buffer, and so does the request body. Debug prints
/// only the size — every [`Command`] and [`Event`] is printed whole in a log
/// line.
#[derive(Clone)]
pub struct Blob(pub Arc<Vec<u8>>);

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

impl Deref for Blob {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl AsRef<[u8]> for Blob {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl From<Vec<u8>> for Blob {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }
}

impl From<Arc<Vec<u8>>> for Blob {
    fn from(bytes: Arc<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// The door key was refused: this build is stale.
    Unauthorized,
    /// The tokens are gone for good; the UI has to ask for a sign-in.
    AuthRequired(String),
    SessionReplaced,
    /// A moderator or the admin CLI kicked the account; it may sign in again
    /// at once.
    Kicked,
    /// The account is banned; nothing this client does helps.
    Banned,
    /// The owner disabled the account; it cannot sign in until that is undone.
    Disabled,
    ServerClosed {
        code: Option<u16>,
        reason: String,
    },
    Io(String),
    Silence,
    ProtocolError(String),
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::AuthRequired(detail) => write!(f, "sign in again: {detail}"),
            Self::SessionReplaced => f.write_str("this account connected from another device"),
            Self::Kicked => f.write_str("kicked from the server"),
            Self::Banned => f.write_str("banned from the server"),
            Self::Disabled => f.write_str("this account is disabled"),
            Self::ServerClosed { code, reason } => match code {
                Some(code) if !reason.is_empty() => write!(f, "server closed ({code}): {reason}"),
                Some(code) => write!(f, "server closed ({code})"),
                None => f.write_str("server closed"),
            },
            Self::Io(detail) => write!(f, "connection error: {detail}"),
            Self::Silence => f.write_str("no traffic for 75s"),
            Self::ProtocolError(detail) => write!(f, "protocol error: {detail}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Handed to the UI once, so it can talk back to this loop.
    Ready(mpsc::Sender<Command>),
    Connecting,
    Connected {
        latest_message_id: i64,
        member_id: i64,
        username: String,
    },
    Disconnected {
        reason: DisconnectReason,
        /// The delay this loop is actually about to sleep; `None` when it stops.
        retry_in: Option<Duration>,
    },
    /// A rotated token pair, already persisted by this loop.
    SessionUpdated(Session),
    ServerError {
        code: i32,
        detail: String,
        fatal: bool,
    },
    /// The whole server as this account may see it; replaces what the UI knows.
    Snapshot(ServerSnapshot),
    ServerUpdated(Server),
    RoleUpserted(Role),
    RoleDeleted {
        id: i64,
    },
    RoleOrder {
        ids: Vec<i64>,
    },
    CategoryUpserted(Category),
    CategoryDeleted {
        id: i64,
    },
    ChannelUpserted(Channel),
    ChannelDeleted {
        id: i64,
    },
    ChannelOrder {
        positions: Vec<ChannelPosition>,
    },
    MemberUpdated(Profile),
    MemberRemoved {
        user_id: i64,
    },
    /// A moderator moved this account; `channel_id` 0 means disconnected.
    VoiceMoved {
        channel_id: i64,
    },
    Message(ChatMessage),
    MessageEdited(ChatMessage),
    MessageDeleted {
        channel_id: i64,
        id: i64,
    },
    ReactionsChanged {
        channel_id: i64,
        message_id: i64,
        reactions: Vec<Reaction>,
    },
    /// The newest page of one channel plus whatever gap-fill added, ascending.
    History {
        channel_id: i64,
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    HistoryFailed {
        channel_id: i64,
        error: String,
    },
    OlderPage {
        channel_id: i64,
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    OlderFailed {
        channel_id: i64,
        error: String,
    },
    AttachmentUploaded {
        request_id: u64,
        attachment: Attachment,
    },
    UploadFailed {
        request_id: u64,
        error: String,
    },
    AttachmentFetched {
        request_id: u64,
        id: i64,
        bytes: Blob,
    },
    FetchFailed {
        request_id: u64,
        id: i64,
        error: String,
    },
    ImageUploaded {
        request_id: u64,
        image: Image,
    },
    ImageUploadFailed {
        request_id: u64,
        error: String,
    },
    ImageFetched {
        request_id: u64,
        id: i64,
        bytes: Blob,
    },
    ImageFetchFailed {
        request_id: u64,
        id: i64,
        error: String,
    },
    /// The server wrote the offer down; the file itself never moved.
    StreamOffered {
        request_id: u64,
        file: StreamedFile,
    },
    StreamOfferFailed {
        request_id: u64,
        error: String,
    },
    StreamFetched {
        request_id: u64,
        id: i64,
        path: PathBuf,
    },
    StreamFetchFailed {
        request_id: u64,
        id: i64,
        error: String,
    },
    /// The server wants a range of a file this client offered. The app resolves
    /// the id against its own registry and answers with
    /// [`Command::ServeStream`], or declines the transfer itself.
    StreamRequested {
        stream_id: i64,
        transfer_id: i64,
        offset: i64,
        length: i64,
    },
    /// How far one transfer has got. Coarse by design: a report every megabyte
    /// or every percent, whichever is larger, and a last one at the end.
    TransferProgress {
        request_id: u64,
        sent: u64,
        total: u64,
    },
    RestResult {
        request_id: u64,
        outcome: Result<RestOutcome, String>,
    },
    VoiceReady {
        channel_id: i64,
        host: String,
        port: u16,
        key: MediaKey,
        ssrc: u32,
    },
    VoiceState {
        channel_id: i64,
        members: Vec<VoiceMember>,
    },
    VoiceMemberJoined {
        channel_id: i64,
        member: VoiceMember,
    },
    VoiceMemberLeft {
        channel_id: i64,
        user_id: i64,
    },
    Speaking {
        channel_id: i64,
        user_id: i64,
        speaking: bool,
    },
    ShareStarted {
        channel_id: i64,
        user_id: i64,
        audio: bool,
    },
    ShareStopped {
        channel_id: i64,
        user_id: i64,
    },
    /// Which share this client is watching now; `None` means none.
    WatchState {
        channel_id: i64,
        user_id: Option<i64>,
    },
    /// How many peers are watching the local share.
    ShareWatchers {
        channel_id: i64,
        count: u32,
    },
    SoundUpserted {
        sound: Sound,
    },
    SoundDeleted {
        sound_id: i64,
    },
    SoundPlayed {
        channel_id: i64,
        user_id: i64,
        sound_id: i64,
    },
    SoundStopped {
        channel_id: i64,
    },
    SendDropped,
    /// A management frame the loop could not send, named by its kind.
    AdminDropped {
        kind: &'static str,
    },
}

/// `WatchState.user_id` is 0 when the server means "not watching anyone".
fn watch_target(user_id: i64) -> Option<i64> {
    (user_id != 0).then_some(user_id)
}

/// One of the four picture types [`images::upload`] takes, which names them as
/// `&'static str` while the UI resolves a content type as a `String`. Anything
/// else travels as [`attachments::DEFAULT_TYPE`], which the image endpoint
/// refuses in its own words rather than this loop inventing them.
fn image_type(content_type: &str) -> &'static str {
    ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find(|known| known.eq_ignore_ascii_case(content_type))
        .unwrap_or(attachments::DEFAULT_TYPE)
}

/// The name a file travels under. It is metadata: a path with nothing usable in
/// it is not worth failing an offer over.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

/// A streamed-file failure as the transfer pipeline's own error. An
/// [`ApiFailure`] travels on untouched, so a bearer challenge still buys the one
/// refresh-and-retry every fetch gets; the answers only a stream can give — the
/// sender is offline, declined, or never answered — become an
/// [`ApiFailure::Io`], the variant that prints the words it was handed.
fn stream_failure(error: StreamError) -> ApiFailure {
    match error {
        StreamError::Api(failure) => failure,
        other => ApiFailure::Io(other.to_string()),
    }
}

/// The progress callback one transfer reports through. [`attachments`] and
/// [`streams`] already hold a report back to a megabyte or a percent, so every
/// call here is worth a frame; one the UI is too far behind to take is dropped
/// rather than stalling the transfer, and the finishing event is what says the
/// transfer is over.
fn progress_reporter(
    mut events: mpsc::Sender<Event>,
    request_id: u64,
) -> impl FnMut(u64, u64) + Send + 'static {
    move |sent, total| {
        let _ = events.try_send(Event::TransferProgress {
            request_id,
            sent,
            total,
        });
    }
}

/// A log-safe rendering of a received frame: `VoiceReady` carries the media key
/// and a message carries what somebody typed, neither of which may ever reach a
/// log line.
fn describe(frame: &ServerFrame) -> String {
    match &frame.payload {
        Some(server_frame::Payload::VoiceReady(ready)) => {
            let channel_id = ready.channel_id;
            let host = &ready.host;
            let port = ready.port;
            let ssrc = ready.ssrc;
            format!(
                "VoiceReady {{ channel_id: {channel_id}, host: {host:?}, port: {port}, ssrc: {ssrc}, key: <redacted> }}"
            )
        }
        Some(server_frame::Payload::Message(message)) => {
            format!("Message {{ {} }}", describe_message(message))
        }
        Some(server_frame::Payload::MessageEdited(edited)) => match &edited.message {
            Some(message) => format!("MessageEdited {{ {} }}", describe_message(message)),
            None => "MessageEdited { message: None }".to_owned(),
        },
        // A clip's name is what somebody typed, so it is reduced to a length
        // the way a message's text is.
        Some(server_frame::Payload::SoundUpserted(upserted)) => match &upserted.sound {
            Some(sound) => {
                let id = sound.id;
                let uploader_id = sound.uploader_id;
                let name_len = sound.name.len();
                let duration_ms = sound.duration_ms;
                let size = sound.size;
                format!(
                    "SoundUpserted {{ id: {id}, uploader_id: {uploader_id}, name_len: {name_len}, duration_ms: {duration_ms}, size: {size} }}"
                )
            }
            None => "SoundUpserted { sound: None }".to_owned(),
        },
        Some(server_frame::Payload::SoundDeleted(deleted)) => {
            let sound_id = deleted.sound_id;
            format!("SoundDeleted {{ sound_id: {sound_id} }}")
        }
        Some(server_frame::Payload::SoundPlayed(played)) => {
            let channel_id = played.channel_id;
            let user_id = played.user_id;
            let sound_id = played.sound_id;
            format!(
                "SoundPlayed {{ channel_id: {channel_id}, user_id: {user_id}, sound_id: {sound_id} }}"
            )
        }
        Some(server_frame::Payload::SoundStopped(stopped)) => {
            let channel_id = stopped.channel_id;
            format!("SoundStopped {{ channel_id: {channel_id} }}")
        }
        // Nothing here needs redacting today; naming every field keeps a later
        // addition to the frame out of the `{frame:?}` catch-all by accident.
        Some(server_frame::Payload::StreamRequest(request)) => {
            let stream_id = request.stream_id;
            let transfer_id = request.transfer_id;
            let offset = request.offset;
            let length = request.length;
            format!(
                "StreamRequest {{ stream_id: {stream_id}, transfer_id: {transfer_id}, offset: {offset}, length: {length} }}"
            )
        }
        _ => format!("{frame:?}"),
    }
}

/// Everything about a message that is worth a log line and nothing that is
/// worth keeping private: no text, no author name, no reply excerpt.
fn describe_message(message: &ChatMessage) -> String {
    let id = message.id;
    let channel_id = message.channel_id;
    let author_id = message.author_id;
    let text_len = message.text.len();
    let attachments = message.attachments.len();
    let reply_to = message.reply_to.is_some();
    format!(
        "id: {id}, channel_id: {channel_id}, author_id: {author_id}, text_len: {text_len}, attachments: {attachments}, reply_to: {reply_to}"
    )
}

/// What one frame that arrived before `Welcome` means.
#[derive(Debug)]
enum FirstFrame {
    Welcome(vorcall_proto::v1::Welcome),
    Error(vorcall_proto::v1::Error),
    /// A payload that cannot precede `Welcome`, named for the log line.
    Ignore(&'static str),
    /// No payload at all: a newer server may add frames without a version bump.
    Unknown,
}

/// Sorts one payload received before `Welcome`. Every payload of the schema has
/// an arm, so a frame out of order costs an ignored log line rather than the
/// whole attempt, and a new frame can never become a "known but unhandled" one.
fn classify_first_frame(payload: Option<server_frame::Payload>) -> FirstFrame {
    match payload {
        Some(server_frame::Payload::Welcome(welcome)) => FirstFrame::Welcome(welcome),
        Some(server_frame::Payload::Error(error)) => FirstFrame::Error(error),
        Some(server_frame::Payload::Message(_)) => FirstFrame::Ignore("Message"),
        Some(server_frame::Payload::Pong(_)) => FirstFrame::Ignore("Pong"),
        Some(server_frame::Payload::VoiceReady(_)) => FirstFrame::Ignore("VoiceReady"),
        Some(server_frame::Payload::VoiceState(_)) => FirstFrame::Ignore("VoiceState"),
        Some(server_frame::Payload::VoiceMemberJoined(_)) => {
            FirstFrame::Ignore("VoiceMemberJoined")
        }
        Some(server_frame::Payload::VoiceMemberLeft(_)) => FirstFrame::Ignore("VoiceMemberLeft"),
        Some(server_frame::Payload::Speaking(_)) => FirstFrame::Ignore("Speaking"),
        Some(server_frame::Payload::MessageEdited(_)) => FirstFrame::Ignore("MessageEdited"),
        Some(server_frame::Payload::MessageDeleted(_)) => FirstFrame::Ignore("MessageDeleted"),
        Some(server_frame::Payload::ReactionsChanged(_)) => FirstFrame::Ignore("ReactionsChanged"),
        Some(server_frame::Payload::ShareStarted(_)) => FirstFrame::Ignore("ShareStarted"),
        Some(server_frame::Payload::ShareStopped(_)) => FirstFrame::Ignore("ShareStopped"),
        Some(server_frame::Payload::WatchState(_)) => FirstFrame::Ignore("WatchState"),
        Some(server_frame::Payload::ShareWatchers(_)) => FirstFrame::Ignore("ShareWatchers"),
        Some(server_frame::Payload::ServerSnapshot(_)) => FirstFrame::Ignore("ServerSnapshot"),
        Some(server_frame::Payload::ServerUpdated(_)) => FirstFrame::Ignore("ServerUpdated"),
        Some(server_frame::Payload::RoleUpserted(_)) => FirstFrame::Ignore("RoleUpserted"),
        Some(server_frame::Payload::RoleDeleted(_)) => FirstFrame::Ignore("RoleDeleted"),
        Some(server_frame::Payload::RoleOrder(_)) => FirstFrame::Ignore("RoleOrder"),
        Some(server_frame::Payload::CategoryUpserted(_)) => FirstFrame::Ignore("CategoryUpserted"),
        Some(server_frame::Payload::CategoryDeleted(_)) => FirstFrame::Ignore("CategoryDeleted"),
        Some(server_frame::Payload::ChannelUpserted(_)) => FirstFrame::Ignore("ChannelUpserted"),
        Some(server_frame::Payload::ChannelDeleted(_)) => FirstFrame::Ignore("ChannelDeleted"),
        Some(server_frame::Payload::ChannelOrder(_)) => FirstFrame::Ignore("ChannelOrder"),
        Some(server_frame::Payload::MemberUpdated(_)) => FirstFrame::Ignore("MemberUpdated"),
        Some(server_frame::Payload::MemberRemoved(_)) => FirstFrame::Ignore("MemberRemoved"),
        Some(server_frame::Payload::VoiceMoved(_)) => FirstFrame::Ignore("VoiceMoved"),
        Some(server_frame::Payload::StreamRequest(_)) => FirstFrame::Ignore("StreamRequest"),
        Some(server_frame::Payload::SoundUpserted(_)) => FirstFrame::Ignore("SoundUpserted"),
        Some(server_frame::Payload::SoundDeleted(_)) => FirstFrame::Ignore("SoundDeleted"),
        Some(server_frame::Payload::SoundPlayed(_)) => FirstFrame::Ignore("SoundPlayed"),
        Some(server_frame::Payload::SoundStopped(_)) => FirstFrame::Ignore("SoundStopped"),
        None => FirstFrame::Unknown,
    }
}

/// The client frame one management command encodes into.
fn admin_payload(command: AdminCommand) -> client_frame::Payload {
    match command {
        AdminCommand::CreateChannel {
            kind,
            name,
            topic,
            category_id,
        } => client_frame::Payload::CreateChannel(CreateChannel {
            kind: kind.into(),
            name,
            topic,
            category_id,
        }),
        AdminCommand::UpdateChannel { id, name, topic } => {
            client_frame::Payload::UpdateChannel(UpdateChannel { id, name, topic })
        }
        AdminCommand::DeleteChannel { id } => {
            client_frame::Payload::DeleteChannel(DeleteChannel { id })
        }
        AdminCommand::CreateCategory { name } => {
            client_frame::Payload::CreateCategory(CreateCategory { name })
        }
        AdminCommand::UpdateCategory { id, name } => {
            client_frame::Payload::UpdateCategory(UpdateCategory { id, name })
        }
        AdminCommand::DeleteCategory { id } => {
            client_frame::Payload::DeleteCategory(DeleteCategory { id })
        }
        AdminCommand::ReorderChannels { positions } => {
            client_frame::Payload::ReorderChannels(ReorderChannels { positions })
        }
        AdminCommand::ReorderCategories { ids } => {
            client_frame::Payload::ReorderCategories(ReorderCategories { ids })
        }
        AdminCommand::SetOverride {
            channel_id,
            override_,
        } => client_frame::Payload::SetOverride(SetOverride {
            channel_id,
            r#override: Some(override_),
        }),
        AdminCommand::CreateRole {
            name,
            color,
            icon_emoji,
            icon_image_id,
            permissions,
            hoist,
        } => client_frame::Payload::CreateRole(CreateRole {
            name,
            color,
            icon_emoji,
            icon_image_id,
            permissions,
            hoist,
        }),
        AdminCommand::UpdateRole { role } => {
            client_frame::Payload::UpdateRole(UpdateRole { role: Some(role) })
        }
        AdminCommand::DeleteRole { id } => client_frame::Payload::DeleteRole(DeleteRole { id }),
        AdminCommand::ReorderRoles { ids } => {
            client_frame::Payload::ReorderRoles(ReorderRoles { ids })
        }
        AdminCommand::SetMemberRoles { user_id, role_ids } => {
            client_frame::Payload::SetMemberRoles(SetMemberRoles { user_id, role_ids })
        }
        AdminCommand::SetNickname { user_id, nickname } => {
            client_frame::Payload::SetNickname(SetNickname { user_id, nickname })
        }
        AdminCommand::KickMember { user_id } => {
            client_frame::Payload::KickMember(KickMember { user_id })
        }
        AdminCommand::BanMember { user_id, reason } => {
            client_frame::Payload::BanMember(BanMember { user_id, reason })
        }
        AdminCommand::UnbanMember { user_id } => {
            client_frame::Payload::UnbanMember(UnbanMember { user_id })
        }
        AdminCommand::UpdateServer {
            name,
            description,
            icon_image_id,
        } => client_frame::Payload::UpdateServer(UpdateServer {
            name,
            description,
            icon_image_id,
        }),
        AdminCommand::TransferOwnership { user_id } => {
            client_frame::Payload::TransferOwnership(TransferOwnership { user_id })
        }
        // Each `set_*` flag is what tells the server the companion value was
        // meant; an unset one leaves that part of the session alone.
        AdminCommand::VoiceModerate {
            user_id,
            channel_id,
            muted,
            deafened,
            move_to,
        } => client_frame::Payload::VoiceModerate(VoiceModerate {
            user_id,
            channel_id,
            set_muted: muted.is_some(),
            muted: muted.unwrap_or_default(),
            set_deafened: deafened.is_some(),
            deafened: deafened.unwrap_or_default(),
            r#move: move_to.is_some(),
            move_to: move_to.unwrap_or_default(),
        }),
        AdminCommand::UpdateProfile {
            description,
            accent_color,
            avatar_image_id,
            banner_image_id,
        } => client_frame::Payload::UpdateProfile(UpdateProfile {
            description,
            accent_color,
            avatar_image_id,
            banner_image_id,
        }),
    }
}

/// The fatal errors retrying cannot fix: the account is live elsewhere, or a
/// moderator closed the door on it. Every other fatal error is followed by the
/// server closing the socket, which the backoff already handles.
fn terminal_reason(error: &vorcall_proto::v1::Error) -> Option<DisconnectReason> {
    if !error.fatal {
        return None;
    }

    match ErrorCode::try_from(error.code) {
        Ok(ErrorCode::SessionReplaced) => Some(DisconnectReason::SessionReplaced),
        Ok(ErrorCode::Kicked) => Some(DisconnectReason::Kicked),
        Ok(ErrorCode::Banned) => Some(DisconnectReason::Banned),
        _ => None,
    }
}

/// What an `Error` received instead of `Welcome` leads to: the attempt is over
/// either way, the question is only whether the loop may try again.
fn fatal_outcome(error: &vorcall_proto::v1::Error) -> AfterAttempt {
    match terminal_reason(error) {
        Some(reason) => AfterAttempt::Stop(Some(reason)),
        None => AfterAttempt::Reconnect {
            reason: DisconnectReason::ProtocolError(error.detail.clone()),
            after: Retry::Backoff,
        },
    }
}

/// What [`run`] does once one connection attempt is over.
enum AfterAttempt {
    Reconnect {
        reason: DisconnectReason,
        after: Retry,
    },
    /// Stop the loop for good. `None` means the UI is gone, so nothing is emitted.
    Stop(Option<DisconnectReason>),
}

enum Retry {
    /// Advance the exponential backoff.
    Backoff,
    /// The attempt reached `Welcome`, so the backoff restarts at 1 s.
    BackoffAfterSession,
    /// A fixed delay that leaves the backoff untouched.
    Fixed(Duration),
}

/// The outcome of the one refresh path every caller goes through.
enum Refreshed {
    Ok,
    /// The refresh token itself was refused: signing in again is the only cure.
    AuthRequired(String),
    /// The account is banned; no token will ever be issued again.
    Banned,
    /// An admin disabled the account; no token will be issued until they undo it.
    Disabled,
    Failed(ApiFailure),
    /// The UI is gone.
    UiGone,
}

/// Drives connect/reconnect until the UI drops `commands`, the account signs in
/// somewhere else, or the tokens stop being renewable.
pub async fn run(
    endpoints: Endpoints,
    mut session: Session,
    mut commands: mpsc::Receiver<Command>,
    mut events: mpsc::Sender<Event>,
) {
    let mut backoff = Backoff::default();
    // Survives every attempt: per channel, what a reconnect gap-fills against.
    let mut newest_delivered: HashMap<i64, i64> = HashMap::new();

    loop {
        if events.send(Event::Connecting).await.is_err() {
            tracing::info!("UI is gone; stopping the connection loop");
            return;
        }
        tracing::info!(
            url = %endpoints.ws_url,
            user_id = session.user_id,
            "connecting"
        );

        if !drop_pending_commands(&mut commands, &mut events).await {
            tracing::info!("UI is gone; stopping the connection loop");
            return;
        }

        let outcome = match ensure_fresh(&endpoints, &mut session, &mut events).await {
            Ok(()) => {
                attempt(
                    &endpoints,
                    &mut session,
                    &mut newest_delivered,
                    &mut commands,
                    &mut events,
                )
                .await
            }
            Err(outcome) => outcome,
        };

        match outcome {
            AfterAttempt::Stop(None) => {
                tracing::info!("connection loop stopped");
                return;
            }
            AfterAttempt::Stop(Some(reason)) => {
                tracing::info!(%reason, "disconnected for good");
                let _ = events
                    .send(Event::Disconnected {
                        reason,
                        retry_in: None,
                    })
                    .await;
                return;
            }
            AfterAttempt::Reconnect { reason, after } => {
                let retry_in = match after {
                    Retry::Backoff => backoff.advance(),
                    Retry::BackoffAfterSession => {
                        backoff.reset();
                        backoff.advance()
                    }
                    Retry::Fixed(delay) => delay,
                };
                tracing::info!(
                    %reason,
                    retry_in_ms = retry_in.as_millis(),
                    "disconnected; will retry"
                );
                if events
                    .send(Event::Disconnected {
                        reason,
                        retry_in: Some(retry_in),
                    })
                    .await
                    .is_err()
                {
                    tracing::info!("UI is gone; stopping the connection loop");
                    return;
                }
                if !wait_out_backoff(retry_in, &mut commands, &mut events).await {
                    return;
                }
                if !drop_pending_commands(&mut commands, &mut events).await {
                    tracing::info!("UI is gone; stopping the connection loop");
                    return;
                }
            }
        }
    }
}

/// Renews the access token when it is about to expire, so an attempt does not
/// spend its first round trip on a 401 it could have foreseen. `Err` carries the
/// outcome [`run`] must act on instead of connecting.
async fn ensure_fresh(
    endpoints: &Endpoints,
    session: &mut Session,
    events: &mut mpsc::Sender<Event>,
) -> Result<(), AfterAttempt> {
    if !session.needs_refresh(session::now_unix()) {
        return Ok(());
    }

    refresh_outcome(refresh_session(endpoints, session, events).await)
}

/// What a refresh leaves the connection attempt doing. Split out of
/// `ensure_fresh` so the cases that end the loop can be pinned without a server.
fn refresh_outcome(refreshed: Refreshed) -> Result<(), AfterAttempt> {
    match refreshed {
        Refreshed::Ok => Ok(()),
        Refreshed::AuthRequired(detail) => Err(AfterAttempt::Stop(Some(
            DisconnectReason::AuthRequired(detail),
        ))),
        Refreshed::Banned => Err(AfterAttempt::Stop(Some(DisconnectReason::Banned))),
        Refreshed::Disabled => Err(AfterAttempt::Stop(Some(DisconnectReason::Disabled))),
        Refreshed::Failed(failure) => Err(refresh_failure_outcome(&failure)),
        Refreshed::UiGone => Err(AfterAttempt::Stop(None)),
    }
}

/// The one place tokens rotate: it persists them and tells the UI, so no caller
/// can leave the loop, `session.toml` and the app out of step.
async fn refresh_session(
    endpoints: &Endpoints,
    session: &mut Session,
    events: &mut mpsc::Sender<Event>,
) -> Refreshed {
    match auth::refresh(endpoints, &session.refresh_token).await {
        Ok(fresh) => {
            *session = fresh;
            if let Err(e) = session::save(session) {
                tracing::warn!(error = %e, "cannot persist the refreshed session");
            }
            tracing::info!(user_id = session.user_id, "access token refreshed");
            if events
                .send(Event::SessionUpdated(session.clone()))
                .await
                .is_err()
            {
                return Refreshed::UiGone;
            }
            Refreshed::Ok
        }
        Err(failure) => refresh_refusal(failure),
    }
}

/// Reads a refused refresh. Split out of `refresh_session` so every refusal the
/// loop must not retry can be pinned without a server.
fn refresh_refusal(failure: ApiFailure) -> Refreshed {
    match failure {
        // `PROTOCOL.md` § Moderation: a banned account is a 403 with detail
        // "banned", and an account an admin disabled a 403 with detail
        // "account disabled" — the detail, not the status alone, is what
        // identifies each case, since a proxy/WAF 403 must fall through to the
        // generic failure arm and back off instead of ending the loop.
        ApiFailure::Status(403, detail) if detail == "banned" => {
            tracing::warn!("the account is banned; stopping the connection loop");
            Refreshed::Banned
        }
        ApiFailure::Status(403, detail) if detail == "account disabled" => {
            tracing::warn!("the account is disabled; stopping the connection loop");
            Refreshed::Disabled
        }
        ApiFailure::AuthChallenge(detail) | ApiFailure::Status(401, detail) => {
            tracing::warn!("the refresh token was refused; the user must sign in again");
            Refreshed::AuthRequired(detail)
        }
        failure => {
            tracing::warn!(error = %failure, "cannot refresh the access token");
            Refreshed::Failed(failure)
        }
    }
}

/// A refresh that failed for a reason other than a refused token: never a stop,
/// always a delay, so the loop cannot spin.
fn refresh_failure_outcome(failure: &ApiFailure) -> AfterAttempt {
    match failure {
        ApiFailure::StaleKey => AfterAttempt::Reconnect {
            reason: DisconnectReason::Unauthorized,
            after: Retry::Fixed(UNAUTHORIZED_RETRY),
        },
        ApiFailure::Throttled(seconds) => AfterAttempt::Reconnect {
            reason: DisconnectReason::Io(failure.to_string()),
            after: Retry::Fixed(Duration::from_secs((*seconds).max(1))),
        },
        other => AfterAttempt::Reconnect {
            reason: DisconnectReason::Io(other.to_string()),
            after: Retry::Backoff,
        },
    }
}

/// Reports one command the loop cannot honour. `false` means the UI is gone.
async fn drop_command(command: Command, events: &mut mpsc::Sender<Event>) -> bool {
    match command {
        Command::Send { text, .. } => {
            tracing::info!(
                chars = text.chars().count(),
                "dropping a message queued while disconnected"
            );
            events.send(Event::SendDropped).await.is_ok()
        }
        Command::LoadHistory { channel_id } => {
            tracing::debug!(channel_id, "cannot load history while disconnected");
            events
                .send(Event::HistoryFailed {
                    channel_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::LoadOlder { channel_id, before } => {
            tracing::debug!(
                channel_id,
                before,
                "cannot load older messages while disconnected"
            );
            events
                .send(Event::OlderFailed {
                    channel_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::UploadAttachment {
            request_id,
            channel_id,
            ..
        }
        | Command::UploadAttachmentFile {
            request_id,
            channel_id,
            ..
        } => {
            tracing::debug!(
                request_id,
                channel_id,
                "cannot upload an attachment while disconnected"
            );
            events
                .send(Event::UploadFailed {
                    request_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::FetchAttachment { request_id, id } => {
            tracing::debug!(
                request_id,
                id,
                "cannot fetch an attachment while disconnected"
            );
            events
                .send(Event::FetchFailed {
                    request_id,
                    id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::UploadImage { request_id, .. } => {
            tracing::debug!(request_id, "cannot upload an image while disconnected");
            events
                .send(Event::ImageUploadFailed {
                    request_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::FetchImage { request_id, id } => {
            tracing::debug!(request_id, id, "cannot fetch an image while disconnected");
            events
                .send(Event::ImageFetchFailed {
                    request_id,
                    id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::OfferStream {
            request_id,
            channel_id,
            ..
        } => {
            tracing::debug!(
                request_id,
                channel_id,
                "cannot offer a file while disconnected"
            );
            events
                .send(Event::StreamOfferFailed {
                    request_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::FetchStream { request_id, id, .. } => {
            tracing::debug!(
                request_id,
                id,
                "cannot read a streamed file while disconnected"
            );
            events
                .send(Event::StreamFetchFailed {
                    request_id,
                    id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        // No event: the transfer the server was asking for is gone with the
        // socket that asked, and the reader waits the server out.
        Command::ServeStream {
            stream_id,
            transfer_id,
            ..
        } => {
            tracing::debug!(
                stream_id,
                transfer_id,
                "cannot serve a streamed range while disconnected"
            );
            true
        }
        // No event either: whatever this was cancelling has already been
        // answered with a failure of its own.
        Command::CancelTransfer { request_id } => {
            tracing::debug!(request_id, "nothing to cancel while disconnected");
            true
        }
        Command::Rest(request) => {
            tracing::debug!(
                request_id = request.request_id,
                kind = ?request.kind,
                "cannot run a REST request while disconnected"
            );
            events
                .send(Event::RestResult {
                    request_id: request.request_id,
                    outcome: Err("not connected".to_owned()),
                })
                .await
                .is_ok()
        }
        Command::Admin(admin) => {
            let kind = admin.kind_name();
            tracing::debug!(kind, "cannot manage the server while disconnected");
            events.send(Event::AdminDropped { kind }).await.is_ok()
        }
        // No event: the UI re-sends JoinVoice after every Connected, and it carries
        // the self flags, so a dropped VoiceSelfState is re-asserted by that join.
        Command::JoinVoice { channel_id, .. }
        | Command::LeaveVoice { channel_id }
        | Command::VoiceSelfState { channel_id, .. } => {
            tracing::debug!(channel_id, "cannot change voice state while disconnected");
            true
        }
        // No event: the UI re-asserts the share state after the next VoiceReady.
        Command::StartShare { channel_id, .. }
        | Command::StopShare { channel_id }
        | Command::WatchShare { channel_id, .. }
        | Command::UnwatchShare { channel_id } => {
            tracing::debug!(channel_id, "cannot change screen share while disconnected");
            true
        }
        // No event: the soundpad is disabled while disconnected, and a clip
        // that was playing is over as soon as the voice session is.
        Command::PlaySound { channel_id, .. } | Command::StopSound { channel_id } => {
            tracing::debug!(channel_id, "cannot play a sound while disconnected");
            true
        }
        Command::UpdateSound { sound_id, .. } | Command::DeleteSound { sound_id } => {
            tracing::debug!(sound_id, "cannot manage a sound while disconnected");
            true
        }
        // No event either: the UI disables these while disconnected.
        Command::OpenDm { .. }
        | Command::MarkRead { .. }
        | Command::Edit { .. }
        | Command::Delete { .. }
        | Command::React { .. } => {
            tracing::debug!("dropping a chat command queued while disconnected");
            true
        }
    }
}

/// Empties `commands` of what piled up while the loop was not live.
///
/// `PROTOCOL.md` has no client-side outbox: a message typed during a reconnect
/// is dropped, not replayed into the next session behind the user's back.
/// `false` means the UI is gone and the loop must stop.
async fn drop_pending_commands(
    commands: &mut mpsc::Receiver<Command>,
    events: &mut mpsc::Sender<Event>,
) -> bool {
    loop {
        match commands.try_recv() {
            Ok(command) => {
                if !drop_command(command, events).await {
                    return false;
                }
            }
            // The UI dropped the channel.
            Err(mpsc::TryRecvError::Closed) => return false,
            Err(mpsc::TryRecvError::Empty) => return true,
        }
    }
}

/// Sleeps out one backoff delay, answering commands as they arrive instead of
/// letting them sit in the channel until the next session. The deadline is
/// fixed, so a command does not restart the wait. `false` means the UI is gone.
async fn wait_out_backoff(
    retry_in: Duration,
    commands: &mut mpsc::Receiver<Command>,
    events: &mut mpsc::Sender<Event>,
) -> bool {
    let wait = sleep_until(Instant::now() + retry_in);
    tokio::pin!(wait);

    loop {
        tokio::select! {
            () = &mut wait => return true,
            command = commands.next() => match command {
                Some(command) => {
                    if !drop_command(command, events).await {
                        tracing::info!("UI is gone; stopping the connection loop");
                        return false;
                    }
                }
                None => {
                    tracing::info!("UI dropped the command channel; stopping the connection loop");
                    return false;
                }
            },
        }
    }
}

/// Emits an event, giving up on the whole attempt if the UI has gone away.
macro_rules! emit {
    ($events:expr, $event:expr) => {
        if $events.send($event).await.is_err() {
            tracing::info!("UI is gone; abandoning the connection");
            return AfterAttempt::Stop(None);
        }
    };
}

/// [`emit!`] for `live_loop`'s labelled loop: breaking out of it instead of
/// returning keeps the `close_gracefully` that follows it, so a UI that goes
/// away mid-emit still costs the server a close frame rather than a dead socket.
macro_rules! emit_or_break {
    ($label:lifetime, $events:expr, $event:expr) => {
        if $events.send($event).await.is_err() {
            tracing::info!("UI is gone; abandoning the connection");
            break $label AfterAttempt::Stop(None);
        }
    };
}

/// Sends one client frame, ending the attempt when the socket refuses it.
macro_rules! send_or_break {
    ($label:lifetime, $sink:expr, $what:expr, $payload:expr) => {
        if let Err(e) = send_frame($sink, $payload).await {
            tracing::warn!(error = %e, frame = $what, "cannot send a frame");
            break $label AfterAttempt::Reconnect {
                reason: DisconnectReason::Io(e),
                after: Retry::BackoffAfterSession,
            };
        }
    };
}

async fn attempt(
    endpoints: &Endpoints,
    session: &mut Session,
    newest_delivered: &mut HashMap<i64, i64>,
    commands: &mut mpsc::Receiver<Command>,
    events: &mut mpsc::Sender<Event>,
) -> AfterAttempt {
    // At most one refresh per attempt, so a server that keeps refusing the
    // bearer cannot turn the handshake into a refresh loop.
    let mut refreshed_here = false;

    let socket = loop {
        let request = match upgrade_request(endpoints, &session.access_token) {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };

        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _response)) => break socket,
            // `PROTOCOL.md` § Moderation: the upgrade answers 403 for a banned
            // account and for nothing else.
            Err(tungstenite::Error::Http(response))
                if response.status() == StatusCode::FORBIDDEN =>
            {
                tracing::warn!("the upgrade was refused with a 403; the account is banned");
                return AfterAttempt::Stop(Some(DisconnectReason::Banned));
            }
            Err(tungstenite::Error::Http(response))
                if response.status() == StatusCode::UNAUTHORIZED =>
            {
                if http::is_key_challenge(response.headers()) {
                    tracing::warn!("the server rejected the pre-shared key (401)");
                    return AfterAttempt::Reconnect {
                        reason: DisconnectReason::Unauthorized,
                        after: Retry::Fixed(UNAUTHORIZED_RETRY),
                    };
                }
                if !http::is_bearer_challenge(response.headers()) {
                    // Both gates of `PROTOCOL.md` name their scheme, so a
                    // challenge-less 401 here is a proxy or a server we do not
                    // know: back off rather than guess which token to renew.
                    tracing::warn!("the upgrade was refused with a 401 carrying no challenge");
                    return AfterAttempt::Reconnect {
                        reason: DisconnectReason::Io(
                            "unexpected 401 without a challenge".to_owned(),
                        ),
                        after: Retry::Backoff,
                    };
                }
                if refreshed_here {
                    tracing::warn!("the refreshed access token was refused too");
                    return AfterAttempt::Stop(Some(DisconnectReason::AuthRequired(
                        "access token rejected".to_owned(),
                    )));
                }
                refreshed_here = true;
                tracing::info!("the upgrade was refused with a Bearer challenge; refreshing");
                match refresh_session(endpoints, session, events).await {
                    Refreshed::Ok => continue,
                    Refreshed::AuthRequired(detail) => {
                        return AfterAttempt::Stop(Some(DisconnectReason::AuthRequired(detail)));
                    }
                    Refreshed::Banned => {
                        return AfterAttempt::Stop(Some(DisconnectReason::Banned));
                    }
                    Refreshed::Disabled => {
                        return AfterAttempt::Stop(Some(DisconnectReason::Disabled));
                    }
                    Refreshed::Failed(failure) => return refresh_failure_outcome(&failure),
                    Refreshed::UiGone => return AfterAttempt::Stop(None),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "handshake failed");
                return AfterAttempt::Reconnect {
                    reason: DisconnectReason::Io(e.to_string()),
                    after: Retry::Backoff,
                };
            }
        }
    };
    tracing::info!("websocket upgraded; sending Hello");

    let (mut sink, mut stream) = socket.split();

    // `Hello.nickname` is deprecated and ignored: the bearer token of the
    // upgrade is what identifies this connection.
    let hello = ClientFrame {
        payload: Some(client_frame::Payload::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: update::Version::current().to_string(),
            client_platform: update::platform(),
            ..Default::default()
        })),
    };
    if let Err(e) = sink.send(WsMessage::binary(hello.encode_to_vec())).await {
        return AfterAttempt::Reconnect {
            reason: DisconnectReason::Io(format!("cannot send Hello: {e}")),
            after: Retry::Backoff,
        };
    }

    let welcome = match await_welcome(&mut stream, events).await {
        Ok(welcome) => welcome,
        Err(outcome) => {
            close_gracefully(&mut sink).await;
            return outcome;
        }
    };
    tracing::info!(
        latest_message_id = welcome.latest_message_id,
        member_id = welcome.member_id,
        username = %welcome.username,
        "welcome received"
    );
    emit!(
        events,
        Event::Connected {
            latest_message_id: welcome.latest_message_id,
            member_id: welcome.member_id,
            username: welcome.username,
        }
    );

    live_loop(
        endpoints,
        session,
        newest_delivered,
        &mut sink,
        &mut stream,
        commands,
        events,
    )
    .await
}

fn upgrade_request(endpoints: &Endpoints, access_token: &str) -> Result<Request, AfterAttempt> {
    let mut request = endpoints
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| AfterAttempt::Reconnect {
            reason: DisconnectReason::Io(format!("cannot build the upgrade request: {e}")),
            after: Retry::Backoff,
        })?;

    let key = HeaderValue::from_str(&endpoints.key).map_err(|_| AfterAttempt::Reconnect {
        reason: DisconnectReason::Unauthorized,
        after: Retry::Fixed(UNAUTHORIZED_RETRY),
    })?;
    request.headers_mut().insert("x-vorcall-key", key);

    let bearer = HeaderValue::from_str(&http::bearer(access_token)).map_err(|_| {
        AfterAttempt::Stop(Some(DisconnectReason::AuthRequired(
            "the access token is not a valid header value".to_owned(),
        )))
    })?;
    request.headers_mut().insert("authorization", bearer);

    Ok(request)
}

/// Reads frames until `Welcome`. Keep-alive control frames may legitimately
/// arrive first, so they are skipped rather than treated as a bad first frame.
async fn await_welcome<St>(
    stream: &mut St,
    events: &mut mpsc::Sender<Event>,
) -> Result<vorcall_proto::v1::Welcome, AfterAttempt>
where
    St: Stream<Item = Result<WsMessage, tungstenite::Error>> + Unpin,
{
    let deadline = Instant::now() + WELCOME_TIMEOUT;

    loop {
        let frame = match timeout_at(deadline, stream.next()).await {
            Err(_elapsed) => {
                return Err(AfterAttempt::Reconnect {
                    reason: DisconnectReason::ProtocolError(
                        "the server did not answer Hello within 5s".to_owned(),
                    ),
                    after: Retry::Backoff,
                });
            }
            Ok(None) => {
                return Err(AfterAttempt::Reconnect {
                    reason: DisconnectReason::Io("the connection ended before Welcome".to_owned()),
                    after: Retry::Backoff,
                });
            }
            Ok(Some(Err(e))) => {
                return Err(AfterAttempt::Reconnect {
                    reason: DisconnectReason::Io(e.to_string()),
                    after: Retry::Backoff,
                });
            }
            Ok(Some(Ok(frame))) => frame,
        };

        match frame {
            WsMessage::Binary(bytes) => {
                let server_frame = match ServerFrame::decode(bytes) {
                    Ok(frame) => frame,
                    Err(e) => {
                        return Err(AfterAttempt::Reconnect {
                            reason: DisconnectReason::ProtocolError(format!(
                                "undecodable first frame: {e}"
                            )),
                            after: Retry::Backoff,
                        });
                    }
                };
                tracing::debug!(frame = %describe(&server_frame), "received");

                match classify_first_frame(server_frame.payload) {
                    FirstFrame::Welcome(welcome) => return Ok(welcome),
                    FirstFrame::Error(error) => {
                        if events
                            .send(Event::ServerError {
                                code: error.code,
                                detail: error.detail.clone(),
                                fatal: error.fatal,
                            })
                            .await
                            .is_err()
                        {
                            return Err(AfterAttempt::Stop(None));
                        }
                        return Err(fatal_outcome(&error));
                    }
                    // Nothing but Welcome may precede Welcome, but ignoring a
                    // stray frame is cheaper than tearing down an otherwise
                    // healthy attempt.
                    FirstFrame::Ignore(payload) => {
                        tracing::debug!(payload, "ignoring a frame that arrived before Welcome");
                    }
                    // A payload this build does not know: a newer server may add
                    // frames without a version bump.
                    FirstFrame::Unknown => {
                        tracing::warn!("ignoring a server frame with no payload this build knows");
                    }
                }
            }
            WsMessage::Text(_) => {
                return Err(AfterAttempt::Reconnect {
                    reason: DisconnectReason::ProtocolError(
                        "text frames are not allowed".to_owned(),
                    ),
                    after: Retry::Backoff,
                });
            }
            WsMessage::Close(frame) => {
                return Err(after_close(frame.as_ref(), Retry::Backoff));
            }
            // tungstenite answers Ping itself; both are just noise here.
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
        }
    }
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiFailure>> + Send + 'a>>;

/// One in-flight REST fetch, plus the single refresh-and-retry it is allowed.
struct Fetch<'a, T> {
    future: Boxed<'a, T>,
    retried: bool,
}

impl<'a, T> Fetch<'a, T> {
    fn new(future: impl Future<Output = Result<T, ApiFailure>> + Send + 'a) -> Self {
        Self {
            future: Box::pin(future),
            retried: false,
        }
    }

    fn retry(&mut self, future: Boxed<'a, T>) {
        self.future = future;
        self.retried = true;
    }
}

/// A "load older" fetch, kept whole so a refresh can re-issue the same request.
struct OlderFetch<'a> {
    fetch: Fetch<'a, MessagePage>,
    channel_id: i64,
    before: i64,
}

/// Lets [`settle`] drive a slot that holds a bare [`Fetch`] as well as one that
/// carries the request beside it.
trait FetchSlot<'a, T> {
    fn fetch(&mut self) -> &mut Fetch<'a, T>;
}

impl<'a, T> FetchSlot<'a, T> for Fetch<'a, T> {
    fn fetch(&mut self) -> &mut Fetch<'a, T> {
        self
    }
}

impl<'a> FetchSlot<'a, MessagePage> for OlderFetch<'a> {
    fn fetch(&mut self) -> &mut Fetch<'a, MessagePage> {
        &mut self.fetch
    }
}

/// Awaits a fetch, if there is one. The `None` arm never completes, so an empty
/// slot simply keeps its `select!` branch quiet.
async fn pending<T>(fetch: Option<&mut Fetch<'_, T>>) -> Result<T, ApiFailure> {
    match fetch {
        Some(fetch) => fetch.future.as_mut().await,
        None => std::future::pending().await,
    }
}

/// What one finished fetch leaves for its `select!` arm to do.
enum Settled<T> {
    Done(T),
    /// A refresh re-issued the same fetch; its arm has nothing to say yet.
    Retried,
    Report(String),
    Stop(AfterAttempt),
}

/// Resolves a finished fetch: hands its value back, or spends the one refresh
/// it is allowed and re-issues it with the fresh token. The slot is emptied
/// unless the fetch was re-issued, so the arms only decide what to emit.
async fn settle<'a, T, S>(
    slot: &mut Option<S>,
    result: Result<T, ApiFailure>,
    reissue: impl FnOnce(String) -> Boxed<'a, T>,
    endpoints: &Endpoints,
    session: &mut Session,
    events: &mut mpsc::Sender<Event>,
) -> Settled<T>
where
    S: FetchSlot<'a, T>,
{
    let failure = match result {
        Ok(value) => {
            *slot = None;
            return Settled::Done(value);
        }
        Err(failure) => failure,
    };

    let retried = slot.as_mut().is_some_and(|slot| slot.fetch().retried);
    match recover_fetch(failure, retried, endpoints, session, events).await {
        FetchRecovery::Retry => {
            if let Some(slot) = slot.as_mut() {
                slot.fetch().retry(reissue(session.access_token.clone()));
            }
            Settled::Retried
        }
        FetchRecovery::Report(detail) => {
            *slot = None;
            Settled::Report(detail)
        }
        FetchRecovery::Stop(outcome) => Settled::Stop(outcome),
    }
}

/// What a failed REST fetch inside the live loop leads to.
enum FetchRecovery {
    /// The tokens rotated: re-issue the same fetch once.
    Retry,
    /// Report this detail to the UI and drop the fetch.
    Report(String),
    Stop(AfterAttempt),
}

/// A Bearer 401 on a REST call buys one refresh and one retry; anything else,
/// including a second failure, is reported to the UI as it stands.
async fn recover_fetch(
    failure: ApiFailure,
    retried: bool,
    endpoints: &Endpoints,
    session: &mut Session,
    events: &mut mpsc::Sender<Event>,
) -> FetchRecovery {
    let detail = failure.to_string();
    if retried || !matches!(failure, ApiFailure::AuthChallenge(_)) {
        return FetchRecovery::Report(detail);
    }

    match refresh_session(endpoints, session, events).await {
        Refreshed::Ok => FetchRecovery::Retry,
        Refreshed::AuthRequired(reason) => FetchRecovery::Stop(AfterAttempt::Stop(Some(
            DisconnectReason::AuthRequired(reason),
        ))),
        Refreshed::Banned => {
            FetchRecovery::Stop(AfterAttempt::Stop(Some(DisconnectReason::Banned)))
        }
        Refreshed::Disabled => {
            FetchRecovery::Stop(AfterAttempt::Stop(Some(DisconnectReason::Disabled)))
        }
        Refreshed::Failed(_) => FetchRecovery::Report(detail),
        Refreshed::UiGone => FetchRecovery::Stop(AfterAttempt::Stop(None)),
    }
}

/// The channels waiting for their newest page. One fetch runs at a time, and a
/// channel already queued or in flight is never asked for twice: the UI may
/// open the same channel again long before the first answer arrives.
#[derive(Default)]
struct HistoryQueue {
    in_flight: Option<i64>,
    waiting: VecDeque<i64>,
}

impl HistoryQueue {
    /// `false` when that channel is already queued or in flight.
    fn enqueue(&mut self, channel_id: i64) -> bool {
        if self.in_flight == Some(channel_id) || self.waiting.contains(&channel_id) {
            return false;
        }
        self.waiting.push_back(channel_id);
        true
    }

    /// The next channel to fetch, or `None` while one is already in flight.
    fn start(&mut self) -> Option<i64> {
        if self.in_flight.is_some() {
            return None;
        }
        let channel_id = self.waiting.pop_front()?;
        self.in_flight = Some(channel_id);
        Some(channel_id)
    }

    fn in_flight(&self) -> Option<i64> {
        self.in_flight
    }

    fn finish(&mut self) {
        self.in_flight = None;
    }

    /// Every channel still expecting a page, so a disconnect can answer them all.
    fn abandon(&mut self) -> Vec<i64> {
        self.in_flight
            .take()
            .into_iter()
            .chain(self.waiting.drain(..))
            .collect()
    }
}

/// One attachment, image or streamed-file transfer the UI asked for.
#[derive(Clone)]
enum Transfer {
    Upload {
        request_id: u64,
        channel_id: i64,
        file_name: String,
        content_type: String,
        bytes: Blob,
    },
    UploadFile {
        request_id: u64,
        channel_id: i64,
        path: PathBuf,
        content_type: String,
    },
    Fetch {
        request_id: u64,
        id: i64,
    },
    UploadImage {
        request_id: u64,
        purpose: ImagePurpose,
        content_type: String,
        bytes: Blob,
    },
    FetchImage {
        request_id: u64,
        id: i64,
    },
    Offer {
        request_id: u64,
        channel_id: i64,
        path: PathBuf,
        content_type: String,
        size: u64,
    },
    FetchStream {
        request_id: u64,
        id: i64,
        save_to: PathBuf,
    },
}

impl Transfer {
    fn request_id(&self) -> u64 {
        match self {
            Self::Upload { request_id, .. }
            | Self::UploadFile { request_id, .. }
            | Self::Fetch { request_id, .. }
            | Self::UploadImage { request_id, .. }
            | Self::FetchImage { request_id, .. }
            | Self::Offer { request_id, .. }
            | Self::FetchStream { request_id, .. } => *request_id,
        }
    }
}

/// The transfers, in the order the UI asked for them. [`TRANSFER_SLOTS`] of
/// them run at once, so one 8 MiB upload cannot hold up every thumbnail behind
/// it; attachments and images share the queue.
#[derive(Default)]
struct TransferQueue {
    waiting: VecDeque<Transfer>,
    running: usize,
}

impl TransferQueue {
    fn push(&mut self, request: Transfer) {
        self.waiting.push_back(request);
    }

    /// The next request to start, or `None` while every slot is busy.
    fn start(&mut self) -> Option<Transfer> {
        if self.running >= TRANSFER_SLOTS {
            return None;
        }
        let request = self.waiting.pop_front()?;
        self.running += 1;
        Some(request)
    }

    fn finish(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    /// Takes one that has not started out of the queue, so a cancel does not
    /// have to wait for its turn to come round first.
    fn cancel(&mut self, request_id: u64) -> Option<Transfer> {
        let index = self
            .waiting
            .iter()
            .position(|request| request.request_id() == request_id)?;
        self.waiting.remove(index)
    }

    /// What never started, so a disconnect can answer it.
    fn abandon(&mut self) -> Vec<Transfer> {
        self.waiting.drain(..).collect()
    }
}

/// One range this client is serving, resolving to the request it answered so
/// its arm can name that in a log line.
type Serve<'a> = Pin<Box<dyn Future<Output = (i64, i64, Result<(), StreamError>)> + Send + 'a>>;

/// One in-flight transfer, kept whole so a refresh can re-issue the same
/// request.
struct TransferFetch<'a> {
    fetch: Fetch<'a, Event>,
    request: Transfer,
}

impl<'a> FetchSlot<'a, Event> for TransferFetch<'a> {
    fn fetch(&mut self) -> &mut Fetch<'a, Event> {
        &mut self.fetch
    }
}

/// What one finished transfer leaves for its `select!` arm to do.
enum Transferred {
    /// What the UI is owed; the slot is free again. Boxed because an [`Event`]
    /// dwarfs the other two variants.
    Emit(Box<Event>),
    /// A refresh re-issued the request; the slot still holds it.
    Retried,
    Stop(AfterAttempt),
}

/// Runs one transfer to the event that answers it, reporting its progress into
/// `events` as it goes. Neither the bytes nor the path reach a log line; the
/// size does.
async fn run_transfer(
    endpoints: &Endpoints,
    access_token: String,
    request: Transfer,
    events: mpsc::Sender<Event>,
) -> Result<Event, ApiFailure> {
    match request {
        Transfer::Upload {
            request_id,
            channel_id,
            file_name,
            content_type,
            bytes,
        } => {
            let attachment = attachments::upload_bytes(
                endpoints,
                &access_token,
                channel_id,
                &file_name,
                &content_type,
                bytes,
            )
            .await?;
            tracing::info!(
                request_id,
                channel_id,
                id = attachment.id,
                size = attachment.size,
                "attachment uploaded"
            );
            Ok(Event::AttachmentUploaded {
                request_id,
                attachment,
            })
        }
        Transfer::UploadFile {
            request_id,
            channel_id,
            path,
            content_type,
        } => {
            let attachment = attachments::upload_from_path(
                endpoints,
                &access_token,
                channel_id,
                &path,
                &content_type,
                progress_reporter(events, request_id),
            )
            .await?;
            tracing::info!(
                request_id,
                channel_id,
                id = attachment.id,
                size = attachment.size,
                "attachment streamed up from disk"
            );
            Ok(Event::AttachmentUploaded {
                request_id,
                attachment,
            })
        }
        Transfer::Fetch { request_id, id } => {
            let bytes =
                attachments::download(endpoints, &access_token, id, PREVIEW_MAX_BYTES).await?;
            tracing::debug!(request_id, id, size = bytes.len(), "attachment fetched");
            Ok(Event::AttachmentFetched {
                request_id,
                id,
                bytes: Blob::from(bytes),
            })
        }
        Transfer::UploadImage {
            request_id,
            purpose,
            content_type,
            bytes,
        } => {
            let image = images::upload(
                endpoints,
                &access_token,
                purpose,
                image_type(&content_type),
                bytes,
            )
            .await?;
            tracing::info!(
                request_id,
                ?purpose,
                id = image.id,
                size = image.size,
                "image uploaded"
            );
            Ok(Event::ImageUploaded { request_id, image })
        }
        Transfer::FetchImage { request_id, id } => {
            let bytes = images::download(endpoints, &access_token, id).await?;
            tracing::debug!(request_id, id, size = bytes.len(), "image fetched");
            Ok(Event::ImageFetched {
                request_id,
                id,
                bytes: Blob::from(bytes),
            })
        }
        Transfer::Offer {
            request_id,
            channel_id,
            path,
            content_type,
            size,
        } => {
            let file = streams::offer(
                endpoints,
                &access_token,
                channel_id,
                &file_name_of(&path),
                &content_type,
                i64::try_from(size).unwrap_or(i64::MAX),
            )
            .await?;
            tracing::info!(
                request_id,
                channel_id,
                id = file.id,
                size = file.size,
                "streamed file offered"
            );
            Ok(Event::StreamOffered { request_id, file })
        }
        Transfer::FetchStream {
            request_id,
            id,
            save_to,
        } => {
            streams::fetch_to_path(
                endpoints,
                &access_token,
                id,
                &save_to,
                progress_reporter(events, request_id),
            )
            .await
            .map_err(stream_failure)?;
            tracing::info!(request_id, id, "streamed file read onto disk");
            Ok(Event::StreamFetched {
                request_id,
                id,
                path: save_to,
            })
        }
    }
}

/// The event telling the UI one transfer is not coming.
fn transfer_failed(request: &Transfer, error: String) -> Event {
    match request {
        Transfer::Upload { request_id, .. } | Transfer::UploadFile { request_id, .. } => {
            Event::UploadFailed {
                request_id: *request_id,
                error,
            }
        }
        Transfer::Fetch { request_id, id } => Event::FetchFailed {
            request_id: *request_id,
            id: *id,
            error,
        },
        Transfer::UploadImage { request_id, .. } => Event::ImageUploadFailed {
            request_id: *request_id,
            error,
        },
        Transfer::FetchImage { request_id, id } => Event::ImageFetchFailed {
            request_id: *request_id,
            id: *id,
            error,
        },
        Transfer::Offer { request_id, .. } => Event::StreamOfferFailed {
            request_id: *request_id,
            error,
        },
        Transfer::FetchStream { request_id, id, .. } => Event::StreamFetchFailed {
            request_id: *request_id,
            id: *id,
            error,
        },
    }
}

/// Resolves one finished transfer, spending its one refresh-and-retry like any
/// other REST fetch.
async fn settle_transfer<'a>(
    slot: &mut Option<TransferFetch<'a>>,
    request: Transfer,
    result: Result<Event, ApiFailure>,
    endpoints: &'a Endpoints,
    session: &mut Session,
    events: &mut mpsc::Sender<Event>,
) -> Transferred {
    let reissue = {
        let request = request.clone();
        let events = events.clone();
        move |token| -> Boxed<'a, Event> {
            Box::pin(run_transfer(endpoints, token, request, events))
        }
    };

    match settle(slot, result, reissue, endpoints, session, events).await {
        Settled::Done(event) => Transferred::Emit(Box::new(event)),
        Settled::Retried => Transferred::Retried,
        Settled::Report(error) => {
            tracing::warn!(
                request_id = request.request_id(),
                %error,
                "a transfer failed"
            );
            Transferred::Emit(Box::new(transfer_failed(&request, error)))
        }
        Settled::Stop(outcome) => Transferred::Stop(outcome),
    }
}

/// The REST requests the UI asked for, in order. One runs at a time: these are
/// the admin panes' list fetches, never on the critical path.
#[derive(Default)]
struct RestQueue {
    waiting: VecDeque<RestRequest>,
    running: bool,
}

impl RestQueue {
    fn push(&mut self, request: RestRequest) {
        self.waiting.push_back(request);
    }

    /// The next request to start, or `None` while one is still running.
    fn start(&mut self) -> Option<RestRequest> {
        if self.running {
            return None;
        }
        let request = self.waiting.pop_front()?;
        self.running = true;
        Some(request)
    }

    fn finish(&mut self) {
        self.running = false;
    }

    /// What never started, so a disconnect can answer it.
    fn abandon(&mut self) -> Vec<RestRequest> {
        self.waiting.drain(..).collect()
    }
}

/// One in-flight REST request, kept whole so a refresh can re-issue it.
struct RestJob<'a> {
    fetch: Fetch<'a, RestOutcome>,
    request: RestRequest,
}

impl<'a> FetchSlot<'a, RestOutcome> for RestJob<'a> {
    fn fetch(&mut self) -> &mut Fetch<'a, RestOutcome> {
        &mut self.fetch
    }
}

/// Runs one REST request to the outcome that answers it.
async fn run_rest(
    endpoints: &Endpoints,
    access_token: String,
    kind: RestKind,
) -> Result<RestOutcome, ApiFailure> {
    match kind {
        RestKind::ListInvites => {
            let invites = admin::list_invites(endpoints, &access_token).await?;
            tracing::debug!(count = invites.len(), "invite list fetched");
            Ok(RestOutcome::Invites(invites))
        }
        RestKind::CreateInvite { days } => {
            let created = admin::create_invite(endpoints, &access_token, days).await?;
            // The code is a credential; only the id of the row is log-safe.
            tracing::info!(id = created.id, days, "invite created");
            Ok(RestOutcome::InviteCreated(created))
        }
        RestKind::RevokeInvite { id } => {
            admin::revoke_invite(endpoints, &access_token, id).await?;
            tracing::info!(id, "invite revoked");
            Ok(RestOutcome::InviteRevoked { id })
        }
        RestKind::ListBans => {
            let bans = admin::list_bans(endpoints, &access_token).await?;
            tracing::debug!(count = bans.len(), "ban list fetched");
            Ok(RestOutcome::Bans(bans))
        }
        RestKind::ListMembers => {
            let members = admin::list_members(endpoints, &access_token).await?;
            tracing::debug!(count = members.len(), "member list fetched");
            Ok(RestOutcome::Members(members))
        }
    }
}

/// Starts the next queued channel's newest page when nothing is in flight.
fn start_history<'a>(
    queue: &mut HistoryQueue,
    slot: &mut Option<Fetch<'a, (Vec<ChatMessage>, bool)>>,
    endpoints: &'a Endpoints,
    access_token: &str,
    newest_delivered: &HashMap<i64, i64>,
) {
    if slot.is_some() {
        return;
    }
    let Some(channel_id) = queue.start() else {
        return;
    };

    let newest = newest_delivered.get(&channel_id).copied();
    *slot = Some(Fetch::new(initial_history(
        endpoints,
        access_token.to_owned(),
        channel_id,
        newest,
    )));
}

/// Fills whichever transfer slots are free, in the order the UI asked.
fn start_transfers<'a>(
    queue: &mut TransferQueue,
    slots: [&mut Option<TransferFetch<'a>>; TRANSFER_SLOTS],
    endpoints: &'a Endpoints,
    access_token: &str,
    events: &mpsc::Sender<Event>,
) {
    for slot in slots {
        if slot.is_some() {
            continue;
        }
        let Some(request) = queue.start() else {
            return;
        };
        *slot = Some(TransferFetch {
            fetch: Fetch::new(run_transfer(
                endpoints,
                access_token.to_owned(),
                request.clone(),
                events.clone(),
            )),
            request,
        });
    }
}

/// Stops the transfer `request_id` names and hands it back, so its arm can
/// answer the UI. Dropping the future in its slot is what actually cancels one
/// in flight: the request ends there, and the reader or writer thread behind it
/// ends with the channel it was feeding. One still queued has only to leave the
/// queue.
fn cancel_transfer(
    request_id: u64,
    slots: [&mut Option<TransferFetch<'_>>; TRANSFER_SLOTS],
    queue: &mut TransferQueue,
) -> Option<Transfer> {
    for slot in slots {
        if let Some(transfer) = slot.take_if(|transfer| transfer.request.request_id() == request_id)
        {
            queue.finish();
            return Some(transfer.request);
        }
    }

    queue.cancel(request_id)
}

/// Starts the next queued REST request when the one slot is free.
fn start_rest<'a>(
    queue: &mut RestQueue,
    slot: &mut Option<RestJob<'a>>,
    endpoints: &'a Endpoints,
    access_token: &str,
) {
    if slot.is_some() {
        return;
    }
    let Some(request) = queue.start() else {
        return;
    };

    *slot = Some(RestJob {
        fetch: Fetch::new(run_rest(
            endpoints,
            access_token.to_owned(),
            request.kind.clone(),
        )),
        request,
    });
}

/// The newest page of one channel, plus the pages a reconnect needs to close
/// the gap between what the UI already has there and what the server kept.
async fn initial_history(
    endpoints: &Endpoints,
    access_token: String,
    channel_id: i64,
    newest_delivered: Option<i64>,
) -> Result<(Vec<ChatMessage>, bool), ApiFailure> {
    let page =
        history::fetch_page(endpoints, &access_token, channel_id, HISTORY_LIMIT, None).await?;
    let mut messages = page.messages;
    let mut has_more = page.has_more;
    let mut pages = 1;

    while let Some(newest) = newest_delivered {
        let Some(oldest) = messages.first().map(|message| message.id) else {
            break;
        };
        // The pages overlap what the UI has once the oldest fetched id is the
        // very next one after it.
        if !has_more || pages >= GAP_FILL_MAX_PAGES || oldest.saturating_sub(1) <= newest {
            break;
        }

        let older = history::fetch_page(
            endpoints,
            &access_token,
            channel_id,
            HISTORY_LIMIT,
            Some(oldest),
        )
        .await?;
        has_more = older.has_more;
        pages += 1;

        if older.messages.is_empty() {
            break;
        }
        let mut merged = older.messages;
        merged.append(&mut messages);
        messages = merged;
    }

    // Concurrent senders can put the same message in two pages.
    messages.sort_unstable_by_key(|message| message.id);
    messages.dedup_by_key(|message| message.id);

    tracing::info!(
        channel_id,
        count = messages.len(),
        pages,
        has_more,
        "history fetched"
    );
    Ok((messages, has_more))
}

/// The fetches the loop owns take the token by value: they outlive the borrow
/// of the session they were spawned from.
async fn older_page(
    endpoints: &Endpoints,
    access_token: String,
    channel_id: i64,
    before: i64,
) -> Result<MessagePage, ApiFailure> {
    history::fetch_page(
        endpoints,
        &access_token,
        channel_id,
        HISTORY_LIMIT,
        Some(before),
    )
    .await
}

/// Remembers the newest id the UI has been handed in a channel, so the next
/// page there knows where its gap starts.
fn note_newest(newest_delivered: &mut HashMap<i64, i64>, channel_id: i64, id: i64) {
    let newest = newest_delivered.entry(channel_id).or_insert(id);
    *newest = (*newest).max(id);
}

/// Encodes and sends one client frame. `Err` carries the transport error.
async fn send_frame<Si>(sink: &mut Si, payload: client_frame::Payload) -> Result<(), String>
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: fmt::Display,
{
    let frame = ClientFrame {
        payload: Some(payload),
    };
    sink.send(WsMessage::binary(frame.encode_to_vec()))
        .await
        .map_err(|e| e.to_string())
}

async fn live_loop<Si, St>(
    endpoints: &Endpoints,
    session: &mut Session,
    newest_delivered: &mut HashMap<i64, i64>,
    sink: &mut Si,
    stream: &mut St,
    commands: &mut mpsc::Receiver<Command>,
    events: &mut mpsc::Sender<Event>,
) -> AfterAttempt
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: fmt::Display,
    St: Stream<Item = Result<WsMessage, tungstenite::Error>> + Unpin,
{
    // History is per channel and on demand: nothing is fetched until the UI
    // opens a channel and asks for it. The snapshot carried everything else.
    let mut history = HistoryQueue::default();
    let mut history_fetch: Option<Fetch<'_, (Vec<ChatMessage>, bool)>> = None;
    let mut older_fetch: Option<OlderFetch<'_>> = None;
    let mut transfers = TransferQueue::default();
    // Two named slots rather than an array: each needs its own `select!` arm.
    let mut first_transfer: Option<TransferFetch<'_>> = None;
    let mut second_transfer: Option<TransferFetch<'_>> = None;
    let mut rest = RestQueue::default();
    let mut rest_job: Option<RestJob<'_>> = None;
    // The ranges of its own files this client is serving. Deliberately not the
    // transfer queue: a served range is this loop answering the server for a
    // reader who is already blocked on it, so it must never sit behind the
    // user's own uploads in one of the two `TRANSFER_SLOTS`. How many run at
    // once is the server's to bound — it caps the transfers one account may
    // have open at all.
    let mut serves: FuturesUnordered<Serve<'_>> = FuturesUnordered::new();

    // Skip the immediate first tick: we have just finished a handshake.
    let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);

    let silence = sleep(SILENCE_TIMEOUT);
    tokio::pin!(silence);

    let outcome = 'live: loop {
        tokio::select! {
            incoming = stream.next() => {
                let frame = match incoming {
                    None => break AfterAttempt::Reconnect {
                        reason: DisconnectReason::Io("the connection ended".to_owned()),
                        after: Retry::BackoffAfterSession,
                    },
                    Some(Err(e)) => break AfterAttempt::Reconnect {
                        reason: DisconnectReason::Io(e.to_string()),
                        after: Retry::BackoffAfterSession,
                    },
                    Some(Ok(frame)) => frame,
                };

                // Every received frame feeds the watchdog, Pong included.
                silence.as_mut().reset(Instant::now() + SILENCE_TIMEOUT);

                match frame {
                    WsMessage::Binary(bytes) => {
                        let server_frame = match ServerFrame::decode(bytes) {
                            Ok(frame) => frame,
                            Err(e) => break AfterAttempt::Reconnect {
                                reason: DisconnectReason::ProtocolError(format!("undecodable frame: {e}")),
                                after: Retry::BackoffAfterSession,
                            },
                        };
                        tracing::debug!(frame = %describe(&server_frame), "received");

                        match server_frame.payload {
                            Some(server_frame::Payload::Message(message)) => {
                                tracing::debug!(
                                    id = message.id,
                                    channel_id = message.channel_id,
                                    author = %message.author,
                                    "message"
                                );
                                note_newest(newest_delivered, message.channel_id, message.id);
                                emit_or_break!('live, events, Event::Message(message));
                            }
                            Some(server_frame::Payload::MessageEdited(edited)) => {
                                match edited.message {
                                    Some(message) => {
                                        note_newest(newest_delivered, message.channel_id, message.id);
                                        emit_or_break!('live, events, Event::MessageEdited(message));
                                    }
                                    None => tracing::warn!("ignoring a MessageEdited without a message"),
                                }
                            }
                            Some(server_frame::Payload::MessageDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::MessageDeleted {
                                    channel_id: deleted.channel_id,
                                    id: deleted.id,
                                });
                            }
                            Some(server_frame::Payload::ReactionsChanged(changed)) => {
                                emit_or_break!('live, events, Event::ReactionsChanged {
                                    channel_id: changed.channel_id,
                                    message_id: changed.message_id,
                                    reactions: changed.reactions,
                                });
                            }
                            Some(server_frame::Payload::ServerSnapshot(snapshot)) => {
                                tracing::info!(
                                    roles = snapshot.roles.len(),
                                    categories = snapshot.categories.len(),
                                    channels = snapshot.channels.len(),
                                    members = snapshot.members.len(),
                                    sounds = snapshot.sounds.len(),
                                    "server snapshot"
                                );
                                emit_or_break!('live, events, Event::Snapshot(snapshot));
                            }
                            Some(server_frame::Payload::ServerUpdated(updated)) => {
                                match updated.server {
                                    Some(server) => emit_or_break!('live, events, Event::ServerUpdated(server)),
                                    None => tracing::warn!("ignoring a ServerUpdated without a server"),
                                }
                            }
                            Some(server_frame::Payload::RoleUpserted(upserted)) => {
                                match upserted.role {
                                    Some(role) => emit_or_break!('live, events, Event::RoleUpserted(role)),
                                    None => tracing::warn!("ignoring a RoleUpserted without a role"),
                                }
                            }
                            Some(server_frame::Payload::RoleDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::RoleDeleted { id: deleted.id });
                            }
                            Some(server_frame::Payload::RoleOrder(order)) => {
                                emit_or_break!('live, events, Event::RoleOrder { ids: order.ids });
                            }
                            Some(server_frame::Payload::CategoryUpserted(upserted)) => {
                                match upserted.category {
                                    Some(category) => emit_or_break!('live, events, Event::CategoryUpserted(category)),
                                    None => tracing::warn!("ignoring a CategoryUpserted without a category"),
                                }
                            }
                            Some(server_frame::Payload::CategoryDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::CategoryDeleted { id: deleted.id });
                            }
                            Some(server_frame::Payload::ChannelUpserted(upserted)) => {
                                match upserted.channel {
                                    Some(channel) => emit_or_break!('live, events, Event::ChannelUpserted(channel)),
                                    None => tracing::warn!("ignoring a ChannelUpserted without a channel"),
                                }
                            }
                            Some(server_frame::Payload::ChannelDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::ChannelDeleted { id: deleted.id });
                            }
                            Some(server_frame::Payload::ChannelOrder(order)) => {
                                emit_or_break!('live, events, Event::ChannelOrder { positions: order.positions });
                            }
                            Some(server_frame::Payload::MemberUpdated(updated)) => {
                                match updated.member {
                                    Some(member) => emit_or_break!('live, events, Event::MemberUpdated(member)),
                                    None => tracing::warn!("ignoring a MemberUpdated without a member"),
                                }
                            }
                            Some(server_frame::Payload::MemberRemoved(removed)) => {
                                emit_or_break!('live, events, Event::MemberRemoved { user_id: removed.user_id });
                            }
                            Some(server_frame::Payload::VoiceMoved(moved)) => {
                                tracing::info!(channel_id = moved.channel_id, "moved by a moderator");
                                emit_or_break!('live, events, Event::VoiceMoved { channel_id: moved.channel_id });
                            }
                            Some(server_frame::Payload::Error(error)) => {
                                tracing::warn!(
                                    code = error.code,
                                    fatal = error.fatal,
                                    detail = %error.detail,
                                    "server error"
                                );
                                emit_or_break!('live, events, Event::ServerError {
                                    code: error.code,
                                    detail: error.detail.clone(),
                                    fatal: error.fatal,
                                });
                                // Other fatal errors are followed by the server
                                // closing; these have to stop the loop for good.
                                if let Some(reason) = terminal_reason(&error) {
                                    break AfterAttempt::Stop(Some(reason));
                                }
                            }
                            Some(server_frame::Payload::VoiceReady(ready)) => {
                                let Ok(key) = <[u8; 32]>::try_from(ready.key.as_slice()) else {
                                    tracing::warn!(
                                        channel_id = ready.channel_id,
                                        len = ready.key.len(),
                                        "ignoring a VoiceReady whose key is not 32 bytes"
                                    );
                                    continue;
                                };
                                let Ok(port) = u16::try_from(ready.port) else {
                                    tracing::warn!(
                                        channel_id = ready.channel_id,
                                        port = ready.port,
                                        "ignoring a VoiceReady with an out-of-range port"
                                    );
                                    continue;
                                };
                                emit_or_break!('live, events, Event::VoiceReady {
                                    channel_id: ready.channel_id,
                                    host: ready.host,
                                    port,
                                    key: MediaKey(key),
                                    ssrc: ready.ssrc,
                                });
                            }
                            Some(server_frame::Payload::VoiceState(state)) => {
                                emit_or_break!('live, events, Event::VoiceState {
                                    channel_id: state.channel_id,
                                    members: state.members,
                                });
                            }
                            Some(server_frame::Payload::VoiceMemberJoined(joined)) => {
                                match joined.member {
                                    Some(member) => emit_or_break!('live, events, Event::VoiceMemberJoined {
                                        channel_id: joined.channel_id,
                                        member,
                                    }),
                                    None => tracing::warn!(
                                        channel_id = joined.channel_id,
                                        "ignoring a VoiceMemberJoined without a member"
                                    ),
                                }
                            }
                            Some(server_frame::Payload::VoiceMemberLeft(left)) => {
                                emit_or_break!('live, events, Event::VoiceMemberLeft {
                                    channel_id: left.channel_id,
                                    user_id: left.user_id,
                                });
                            }
                            Some(server_frame::Payload::Speaking(speaking)) => {
                                emit_or_break!('live, events, Event::Speaking {
                                    channel_id: speaking.channel_id,
                                    user_id: speaking.user_id,
                                    speaking: speaking.speaking,
                                });
                            }
                            Some(server_frame::Payload::ShareStarted(started)) => {
                                emit_or_break!('live, events, Event::ShareStarted {
                                    channel_id: started.channel_id,
                                    user_id: started.user_id,
                                    audio: started.audio,
                                });
                            }
                            Some(server_frame::Payload::ShareStopped(stopped)) => {
                                emit_or_break!('live, events, Event::ShareStopped {
                                    channel_id: stopped.channel_id,
                                    user_id: stopped.user_id,
                                });
                            }
                            Some(server_frame::Payload::WatchState(state)) => {
                                emit_or_break!('live, events, Event::WatchState {
                                    channel_id: state.channel_id,
                                    user_id: watch_target(state.user_id),
                                });
                            }
                            Some(server_frame::Payload::ShareWatchers(watchers)) => {
                                emit_or_break!('live, events, Event::ShareWatchers {
                                    channel_id: watchers.channel_id,
                                    count: watchers.count,
                                });
                            }
                            Some(server_frame::Payload::SoundUpserted(upserted)) => {
                                match upserted.sound {
                                    Some(sound) => emit_or_break!('live, events, Event::SoundUpserted { sound }),
                                    None => tracing::warn!("ignoring a SoundUpserted without a sound"),
                                }
                            }
                            Some(server_frame::Payload::SoundDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::SoundDeleted {
                                    sound_id: deleted.sound_id,
                                });
                            }
                            Some(server_frame::Payload::SoundPlayed(played)) => {
                                emit_or_break!('live, events, Event::SoundPlayed {
                                    channel_id: played.channel_id,
                                    user_id: played.user_id,
                                    sound_id: played.sound_id,
                                });
                            }
                            Some(server_frame::Payload::SoundStopped(stopped)) => {
                                emit_or_break!('live, events, Event::SoundStopped {
                                    channel_id: stopped.channel_id,
                                });
                            }
                            // The app owns the registry: it resolves the id to a
                            // local file and answers with `ServeStream`, or
                            // declines the transfer itself.
                            Some(server_frame::Payload::StreamRequest(request)) => {
                                emit_or_break!('live, events, Event::StreamRequested {
                                    stream_id: request.stream_id,
                                    transfer_id: request.transfer_id,
                                    offset: request.offset,
                                    length: request.length,
                                });
                            }
                            // A Pong only had to reach the watchdog above.
                            Some(server_frame::Payload::Pong(_)) => {}
                            // The handshake is over: a second Welcome means the
                            // peer is not the server this build speaks to.
                            Some(server_frame::Payload::Welcome(_)) => break AfterAttempt::Reconnect {
                                reason: DisconnectReason::ProtocolError("a second Welcome".to_owned()),
                                after: Retry::BackoffAfterSession,
                            },
                            // A payload this build does not know: a newer server
                            // may add frames without a version bump.
                            None => tracing::warn!("ignoring a server frame with no payload this build knows"),
                        }
                    }
                    WsMessage::Text(_) => break AfterAttempt::Reconnect {
                        reason: DisconnectReason::ProtocolError("text frames are not allowed".to_owned()),
                        after: Retry::BackoffAfterSession,
                    },
                    WsMessage::Close(frame) => break after_close(frame.as_ref(), Retry::BackoffAfterSession),
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
                }
            }

            result = pending(history_fetch.as_mut()), if history_fetch.is_some() => {
                // Copied out before `settle` takes the slot, so a refresh can
                // re-issue the very same request.
                let Some(channel_id) = history.in_flight() else {
                    continue;
                };
                let newest = newest_delivered.get(&channel_id).copied();
                let reissue = move |token| -> Boxed<'_, _> {
                    Box::pin(initial_history(endpoints, token, channel_id, newest))
                };
                match settle(&mut history_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done((messages, has_more)) => {
                        history.finish();
                        if let Some(last) = messages.last() {
                            note_newest(newest_delivered, channel_id, last.id);
                        }
                        emit_or_break!('live, events, Event::History { channel_id, messages, has_more });
                    }
                    Settled::Retried => {}
                    Settled::Report(error) => {
                        history.finish();
                        tracing::warn!(channel_id, %error, "history fetch failed");
                        emit_or_break!('live, events, Event::HistoryFailed { channel_id, error });
                    }
                    Settled::Stop(outcome) => break outcome,
                }
                start_history(
                    &mut history,
                    &mut history_fetch,
                    endpoints,
                    &session.access_token,
                    newest_delivered,
                );
            }

            result = pending(older_fetch.as_mut().map(|older| &mut older.fetch)), if older_fetch.is_some() => {
                // Copied out before `settle` takes the slot, so a refresh can
                // re-issue the very same request.
                let Some((channel_id, before)) = older_fetch
                    .as_ref()
                    .map(|older| (older.channel_id, older.before))
                else {
                    continue;
                };
                let reissue = move |token| -> Boxed<'_, _> {
                    Box::pin(older_page(endpoints, token, channel_id, before))
                };
                match settle(&mut older_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done(page) => {
                        if let Some(last) = page.messages.last() {
                            note_newest(newest_delivered, channel_id, last.id);
                        }
                        emit_or_break!('live, events, Event::OlderPage {
                            channel_id,
                            messages: page.messages,
                            has_more: page.has_more,
                        });
                    }
                    Settled::Retried => {}
                    Settled::Report(error) => {
                        tracing::warn!(channel_id, %error, "older page fetch failed");
                        emit_or_break!('live, events, Event::OlderFailed { channel_id, error });
                    }
                    Settled::Stop(outcome) => break outcome,
                }
            }

            result = pending(first_transfer.as_mut().map(|transfer| &mut transfer.fetch)), if first_transfer.is_some() => {
                let Some(request) = first_transfer.as_ref().map(|transfer| transfer.request.clone()) else {
                    continue;
                };
                match settle_transfer(&mut first_transfer, request, result, endpoints, session, events).await {
                    Transferred::Emit(event) => {
                        transfers.finish();
                        emit_or_break!('live, events, *event);
                    }
                    Transferred::Retried => {}
                    Transferred::Stop(outcome) => break outcome,
                }
                start_transfers(
                    &mut transfers,
                    [&mut first_transfer, &mut second_transfer],
                    endpoints,
                    &session.access_token,
                    events,
                );
            }

            result = pending(second_transfer.as_mut().map(|transfer| &mut transfer.fetch)), if second_transfer.is_some() => {
                let Some(request) = second_transfer.as_ref().map(|transfer| transfer.request.clone()) else {
                    continue;
                };
                match settle_transfer(&mut second_transfer, request, result, endpoints, session, events).await {
                    Transferred::Emit(event) => {
                        transfers.finish();
                        emit_or_break!('live, events, *event);
                    }
                    Transferred::Retried => {}
                    Transferred::Stop(outcome) => break outcome,
                }
                start_transfers(
                    &mut transfers,
                    [&mut first_transfer, &mut second_transfer],
                    endpoints,
                    &session.access_token,
                    events,
                );
            }

            result = pending(rest_job.as_mut().map(|job| &mut job.fetch)), if rest_job.is_some() => {
                // Copied out before `settle` takes the slot, so a refresh can
                // re-issue the very same request.
                let Some(request) = rest_job.as_ref().map(|job| job.request.clone()) else {
                    continue;
                };
                let request_id = request.request_id;
                let reissue = {
                    let kind = request.kind.clone();
                    move |token| -> Boxed<'_, _> { Box::pin(run_rest(endpoints, token, kind)) }
                };
                match settle(&mut rest_job, result, reissue, endpoints, session, events).await {
                    Settled::Done(outcome) => {
                        rest.finish();
                        emit_or_break!('live, events, Event::RestResult {
                            request_id,
                            outcome: Ok(outcome),
                        });
                    }
                    Settled::Retried => {}
                    Settled::Report(error) => {
                        rest.finish();
                        tracing::warn!(request_id, kind = ?request.kind, %error, "a REST request failed");
                        emit_or_break!('live, events, Event::RestResult {
                            request_id,
                            outcome: Err(error),
                        });
                    }
                    Settled::Stop(outcome) => break outcome,
                }
                start_rest(&mut rest, &mut rest_job, endpoints, &session.access_token);
            }

            Some((stream_id, transfer_id, result)) = serves.next(), if !serves.is_empty() => {
                match result {
                    Ok(()) => tracing::debug!(stream_id, transfer_id, "streamed range served"),
                    // Nothing to tell the UI: it never asked for this range, and
                    // the reader's own client is what reports the transfer.
                    Err(error) => tracing::warn!(
                        stream_id,
                        transfer_id,
                        %error,
                        "cannot serve a streamed range"
                    ),
                }
            }

            command = commands.next() => {
                let Some(command) = command else {
                    tracing::info!("UI dropped the command channel; closing");
                    break 'live AfterAttempt::Stop(None);
                };
                tracing::debug!(kind = command.kind_name(), "command");

                match command {
                    Command::Send { channel_id, text, reply_to_id, attachment_ids, streamed_file_ids } => {
                        let payload = client_frame::Payload::Send(SendMessage {
                            text,
                            channel_id,
                            reply_to_id: reply_to_id.unwrap_or_default(),
                            attachment_ids,
                            streamed_file_ids,
                        });
                        if let Err(e) = send_frame(sink, payload).await {
                            tracing::warn!(error = %e, "cannot send the message");
                            emit_or_break!('live, events, Event::SendDropped);
                            break AfterAttempt::Reconnect {
                                reason: DisconnectReason::Io(e),
                                after: Retry::BackoffAfterSession,
                            };
                        }
                    }
                    Command::OpenDm { user_id } => send_or_break!(
                        'live, sink, "OpenDm",
                        client_frame::Payload::OpenDm(OpenDm { user_id })
                    ),
                    Command::MarkRead { channel_id, message_id } => send_or_break!(
                        'live, sink, "MarkRead",
                        client_frame::Payload::MarkRead(MarkRead { channel_id, message_id })
                    ),
                    Command::Edit { id, text } => send_or_break!(
                        'live, sink, "EditMessage",
                        client_frame::Payload::EditMessage(EditMessage { id, text })
                    ),
                    Command::Delete { id } => send_or_break!(
                        'live, sink, "DeleteMessage",
                        client_frame::Payload::DeleteMessage(DeleteMessage { id })
                    ),
                    Command::React { message_id, emoji, remove } => send_or_break!(
                        'live, sink, "React",
                        client_frame::Payload::React(React { message_id, emoji, remove })
                    ),
                    Command::JoinVoice { channel_id, self_muted, self_deafened } => send_or_break!(
                        'live, sink, "JoinVoice",
                        client_frame::Payload::JoinVoice(JoinVoice {
                            channel_id,
                            self_muted,
                            self_deafened,
                        })
                    ),
                    Command::LeaveVoice { channel_id } => send_or_break!(
                        'live, sink, "LeaveVoice",
                        client_frame::Payload::LeaveVoice(LeaveVoice { channel_id })
                    ),
                    Command::VoiceSelfState { channel_id, muted, deafened } => send_or_break!(
                        'live, sink, "VoiceSelfState",
                        client_frame::Payload::VoiceSelfState(VoiceSelfState {
                            channel_id,
                            muted,
                            deafened,
                        })
                    ),
                    Command::StartShare { channel_id, audio } => send_or_break!(
                        'live, sink, "StartShare",
                        client_frame::Payload::StartShare(StartShare { channel_id, audio })
                    ),
                    Command::StopShare { channel_id } => send_or_break!(
                        'live, sink, "StopShare",
                        client_frame::Payload::StopShare(StopShare { channel_id })
                    ),
                    Command::WatchShare { channel_id, user_id } => send_or_break!(
                        'live, sink, "WatchShare",
                        client_frame::Payload::WatchShare(WatchShare { channel_id, user_id })
                    ),
                    Command::UnwatchShare { channel_id } => send_or_break!(
                        'live, sink, "UnwatchShare",
                        client_frame::Payload::UnwatchShare(UnwatchShare { channel_id })
                    ),
                    Command::PlaySound { channel_id, sound_id } => send_or_break!(
                        'live, sink, "PlaySound",
                        client_frame::Payload::PlaySound(PlaySound { channel_id, sound_id })
                    ),
                    Command::StopSound { channel_id } => send_or_break!(
                        'live, sink, "StopSound",
                        client_frame::Payload::StopSound(StopSound { channel_id })
                    ),
                    Command::UpdateSound { sound_id, name } => send_or_break!(
                        'live, sink, "UpdateSound",
                        client_frame::Payload::UpdateSound(UpdateSound { sound_id, name })
                    ),
                    Command::DeleteSound { sound_id } => send_or_break!(
                        'live, sink, "DeleteSound",
                        client_frame::Payload::DeleteSound(DeleteSound { sound_id })
                    ),
                    // Fire and forget: the server answers with an `Error` or
                    // with the delta the change produced.
                    Command::Admin(admin) => {
                        let kind = admin.kind_name();
                        send_or_break!('live, sink, kind, admin_payload(admin));
                    }
                    Command::LoadHistory { channel_id } => {
                        if history.enqueue(channel_id) {
                            start_history(
                                &mut history,
                                &mut history_fetch,
                                endpoints,
                                &session.access_token,
                                newest_delivered,
                            );
                        } else {
                            tracing::debug!(channel_id, "that channel's history is already loading");
                        }
                    }
                    Command::LoadOlder { channel_id, before } => {
                        if older_fetch.is_some() {
                            tracing::debug!(channel_id, before, "an older page is already loading");
                        } else {
                            let request = older_page(
                                endpoints,
                                session.access_token.clone(),
                                channel_id,
                                before,
                            );
                            older_fetch = Some(OlderFetch {
                                fetch: Fetch::new(request),
                                channel_id,
                                before,
                            });
                        }
                    }
                    Command::UploadAttachment { request_id, channel_id, file_name, content_type, bytes } => {
                        tracing::info!(request_id, channel_id, size = bytes.len(), "attachment upload queued");
                        transfers.push(Transfer::Upload {
                            request_id,
                            channel_id,
                            file_name,
                            content_type,
                            bytes,
                        });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::UploadAttachmentFile { request_id, channel_id, path, content_type } => {
                        tracing::info!(request_id, channel_id, "attachment upload from disk queued");
                        transfers.push(Transfer::UploadFile {
                            request_id,
                            channel_id,
                            path,
                            content_type,
                        });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::FetchAttachment { request_id, id } => {
                        tracing::debug!(request_id, id, "attachment fetch queued");
                        transfers.push(Transfer::Fetch { request_id, id });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::UploadImage { request_id, purpose, content_type, bytes } => {
                        tracing::info!(request_id, ?purpose, size = bytes.len(), "image upload queued");
                        transfers.push(Transfer::UploadImage {
                            request_id,
                            purpose,
                            content_type,
                            bytes,
                        });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::FetchImage { request_id, id } => {
                        tracing::debug!(request_id, id, "image fetch queued");
                        transfers.push(Transfer::FetchImage { request_id, id });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::OfferStream { request_id, channel_id, path, content_type, size } => {
                        tracing::info!(request_id, channel_id, size, "streamed file offer queued");
                        transfers.push(Transfer::Offer {
                            request_id,
                            channel_id,
                            path,
                            content_type,
                            size,
                        });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    Command::FetchStream { request_id, id, save_to } => {
                        tracing::info!(request_id, id, "streamed file read queued");
                        transfers.push(Transfer::FetchStream { request_id, id, save_to });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
                            events,
                        );
                    }
                    // Not a queued transfer: the server is asking on behalf of a
                    // reader who is already waiting, so this never queues behind
                    // the user's own uploads. The app has already resolved the
                    // id against its registry; a file it found gone it declined
                    // itself.
                    Command::ServeStream { stream_id, transfer_id, offset, length, path } => {
                        tracing::debug!(stream_id, transfer_id, offset, length, "serving a streamed range");
                        let request = StreamRequest { stream_id, transfer_id, offset, length };
                        let access_token = session.access_token.clone();
                        serves.push(Box::pin(async move {
                            let result = streams::serve_chunk(
                                endpoints,
                                &access_token,
                                &request,
                                &path,
                                // A served range has no `request_id`: the UI
                                // never asked for it and has nothing to show.
                                |_sent, _wanted| {},
                            )
                            .await;
                            (stream_id, transfer_id, result)
                        }));
                    }
                    Command::CancelTransfer { request_id } => {
                        match cancel_transfer(
                            request_id,
                            [&mut first_transfer, &mut second_transfer],
                            &mut transfers,
                        ) {
                            Some(request) => {
                                tracing::info!(request_id, "transfer cancelled");
                                emit_or_break!('live, events, transfer_failed(&request, "cancelled".to_owned()));
                                start_transfers(
                                    &mut transfers,
                                    [&mut first_transfer, &mut second_transfer],
                                    endpoints,
                                    &session.access_token,
                                    events,
                                );
                            }
                            None => tracing::debug!(request_id, "nothing to cancel: that transfer is already over"),
                        }
                    }
                    Command::Rest(request) => {
                        tracing::debug!(request_id = request.request_id, kind = ?request.kind, "REST request queued");
                        rest.push(request);
                        start_rest(&mut rest, &mut rest_job, endpoints, &session.access_token);
                    }
                }
            }

            _ = ping.tick() => {
                tracing::debug!("ping");
                send_or_break!(
                    'live, sink, "Ping",
                    client_frame::Payload::Ping(Ping { sent_at_unix_ms: unix_millis() })
                );
            }

            () = &mut silence => {
                tracing::warn!("no frame for 75s; dropping the connection");
                break AfterAttempt::Reconnect {
                    reason: DisconnectReason::Silence,
                    after: Retry::BackoffAfterSession,
                };
            }
        }
    };

    // The UI keeps a "loading" flag per channel and per request until each of
    // these answers, and the socket is about to go: answer them here so nothing
    // spins for ever. Nothing is replayed into the next session; the UI asks
    // again once it is connected.
    for channel_id in history.abandon() {
        let _ = events
            .send(Event::HistoryFailed {
                channel_id,
                error: "disconnected".to_owned(),
            })
            .await;
    }
    if let Some(older) = older_fetch {
        let _ = events
            .send(Event::OlderFailed {
                channel_id: older.channel_id,
                error: "disconnected".to_owned(),
            })
            .await;
    }
    let abandoned = first_transfer
        .into_iter()
        .chain(second_transfer)
        .map(|transfer| transfer.request)
        .chain(transfers.abandon());
    for request in abandoned {
        let _ = events
            .send(transfer_failed(&request, "disconnected".to_owned()))
            .await;
    }
    if !serves.is_empty() {
        // Nothing to answer: a range this client never finished is one the
        // server times its reader out of, and the reader asks again.
        tracing::debug!(
            count = serves.len(),
            "abandoning the ranges this client was serving"
        );
    }
    let abandoned = rest_job
        .into_iter()
        .map(|job| job.request)
        .chain(rest.abandon());
    for request in abandoned {
        let _ = events
            .send(Event::RestResult {
                request_id: request.request_id,
                outcome: Err("disconnected".to_owned()),
            })
            .await;
    }

    close_gracefully(sink).await;
    outcome
}

fn closed_reason(frame: Option<&CloseFrame>) -> DisconnectReason {
    match frame {
        Some(frame) => match u16::from(frame.code) {
            KICKED_CLOSE_CODE => DisconnectReason::Kicked,
            DISABLED_CLOSE_CODE => DisconnectReason::Disabled,
            code => DisconnectReason::ServerClosed {
                code: Some(code),
                reason: frame.reason.as_str().to_owned(),
            },
        },
        None => DisconnectReason::ServerClosed {
            code: None,
            reason: String::new(),
        },
    }
}

/// What a close frame leaves the loop doing. The two admin closes end it for
/// good: reconnecting would only be refused again, and the UI has something to
/// say about both.
fn after_close(frame: Option<&CloseFrame>, after: Retry) -> AfterAttempt {
    let reason = closed_reason(frame);
    if matches!(
        reason,
        DisconnectReason::Kicked | DisconnectReason::Banned | DisconnectReason::Disabled
    ) {
        AfterAttempt::Stop(Some(reason))
    } else {
        AfterAttempt::Reconnect { reason, after }
    }
}

async fn close_gracefully<Si>(sink: &mut Si)
where
    Si: Sink<WsMessage> + Unpin,
{
    let close = WsMessage::Close(Some(CloseFrame {
        code: CloseCode::Normal,
        reason: Utf8Bytes::from_static(""),
    }));
    let _ = sink.send(close).await;
    let _ = sink.close().await;
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// 1, 2, 4, 8, 16, 30 s (cap) with +/-20 % jitter.
#[derive(Default)]
struct Backoff {
    step: usize,
}

impl Backoff {
    fn reset(&mut self) {
        self.step = 0;
    }

    fn advance(&mut self) -> Duration {
        let last = BACKOFF_SECS.len() - 1;
        let base = BACKOFF_SECS[self.step.min(last)];
        self.step = (self.step + 1).min(last);

        let factor = rand::random_range((1.0 - BACKOFF_JITTER)..=(1.0 + BACKOFF_JITTER));
        Duration::from_secs_f64(base as f64 * factor)
    }
}

#[cfg(test)]
mod tests {
    use vorcall_proto::v1::{MessageEdited, ReplyRef};

    use super::*;

    const GENERAL: i64 = 1;
    const MUSIC: i64 = 2;

    fn transfer(request_id: u64) -> Transfer {
        Transfer::Fetch {
            request_id,
            id: 100,
        }
    }

    fn request_ids(requests: &[Transfer]) -> Vec<u64> {
        requests.iter().map(Transfer::request_id).collect()
    }

    fn rest_request(request_id: u64) -> RestRequest {
        RestRequest {
            request_id,
            kind: RestKind::ListInvites,
        }
    }

    fn error(code: ErrorCode, fatal: bool) -> vorcall_proto::v1::Error {
        vorcall_proto::v1::Error {
            code: code.into(),
            detail: String::new(),
            fatal,
        }
    }

    /// A `Command` or an `Event` carrying one reaches a log line whole.
    #[test]
    fn a_blob_prints_its_size_rather_than_its_bytes() {
        let printed = format!("{:?}", Blob::from(vec![1, 2, 3]));

        assert!(printed.contains("3 bytes"), "{printed}");
        assert!(!printed.contains('1'), "{printed}");
        assert!(!printed.contains('2'), "{printed}");
    }

    /// `Event::VoiceReady` is printed whole by the UI's own event trace.
    #[test]
    fn a_media_key_never_prints_its_bytes() {
        let printed = format!("{:?}", MediaKey([7; 32]));

        assert_eq!(printed, "MediaKey(<redacted>)");
        assert!(!printed.contains('7'), "{printed}");
    }

    #[test]
    fn an_invite_created_outcome_redacts_the_code() {
        let outcome = RestOutcome::InviteCreated(InviteCreated {
            id: 3,
            code: "SWORDFISH".to_owned(),
            expires_at_unix_ms: 42,
        });

        let printed = format!("{outcome:?}");
        assert!(printed.contains("code: <redacted>"), "{printed}");
        assert!(printed.contains("id: 3"), "{printed}");
        assert!(!printed.contains("SWORDFISH"), "{printed}");

        // The UI logs whole events, so the redaction has to survive nesting.
        let event = Event::RestResult {
            request_id: 1,
            outcome: Ok(outcome),
        };
        assert!(!format!("{event:?}").contains("SWORDFISH"), "{event:?}");
    }

    #[test]
    fn an_older_id_never_lowers_a_channel_s_newest_one() {
        let mut newest = HashMap::new();

        note_newest(&mut newest, GENERAL, 7);
        note_newest(&mut newest, GENERAL, 12);
        note_newest(&mut newest, GENERAL, 4);

        assert_eq!(newest.get(&GENERAL).copied(), Some(12));
    }

    #[test]
    fn each_channel_keeps_its_own_newest_id() {
        let mut newest = HashMap::new();

        note_newest(&mut newest, GENERAL, 9);
        note_newest(&mut newest, MUSIC, 3);

        assert_eq!(newest.get(&GENERAL).copied(), Some(9));
        assert_eq!(newest.get(&MUSIC).copied(), Some(3));
        assert_eq!(newest.get(&7).copied(), None);
    }

    #[test]
    fn the_history_queue_serves_one_channel_at_a_time_in_order() {
        let mut queue = HistoryQueue::default();
        assert!(queue.enqueue(GENERAL));
        assert!(queue.enqueue(MUSIC));

        assert_eq!(queue.start(), Some(GENERAL));
        assert_eq!(queue.in_flight(), Some(GENERAL));
        assert_eq!(queue.start(), None);

        queue.finish();
        assert_eq!(queue.start(), Some(MUSIC));

        queue.finish();
        assert_eq!(queue.start(), None);
        assert_eq!(queue.in_flight(), None);
    }

    #[test]
    fn the_history_queue_ignores_a_channel_already_queued_or_in_flight() {
        let mut queue = HistoryQueue::default();
        assert!(queue.enqueue(GENERAL));
        assert!(!queue.enqueue(GENERAL));

        assert_eq!(queue.start(), Some(GENERAL));
        assert!(!queue.enqueue(GENERAL));
        assert!(queue.enqueue(MUSIC));

        queue.finish();
        assert_eq!(queue.start(), Some(MUSIC));
        queue.finish();
        assert_eq!(queue.start(), None);
    }

    #[test]
    fn a_disconnect_abandons_the_channel_in_flight_and_the_ones_waiting() {
        let mut queue = HistoryQueue::default();
        queue.enqueue(GENERAL);
        queue.enqueue(MUSIC);
        queue.start();

        assert_eq!(queue.abandon(), vec![GENERAL, MUSIC]);
        assert_eq!(queue.start(), None);
    }

    #[test]
    fn the_transfer_queue_runs_two_at_a_time_in_order() {
        let mut queue = TransferQueue::default();
        for request_id in 1..=3 {
            queue.push(transfer(request_id));
        }

        assert_eq!(queue.start().map(|request| request.request_id()), Some(1));
        assert_eq!(queue.start().map(|request| request.request_id()), Some(2));
        assert!(queue.start().is_none());
    }

    #[test]
    fn a_finished_transfer_admits_the_next_one() {
        let mut queue = TransferQueue::default();
        for request_id in 1..=3 {
            queue.push(transfer(request_id));
        }
        queue.start();
        queue.start();

        queue.finish();
        assert_eq!(queue.start().map(|request| request.request_id()), Some(3));
        assert!(queue.start().is_none());

        queue.finish();
        queue.finish();
        assert!(queue.start().is_none());
    }

    #[test]
    fn a_disconnect_abandons_the_transfers_that_never_started() {
        let mut queue = TransferQueue::default();
        for request_id in 1..=3 {
            queue.push(transfer(request_id));
        }
        queue.start();
        queue.start();

        assert_eq!(request_ids(&queue.abandon()), vec![3]);
        assert!(queue.abandon().is_empty());
    }

    /// One REST request at a time: the settings panes never race each other.
    #[test]
    fn a_rest_queue_runs_one_request_at_a_time() {
        let mut queue = RestQueue::default();
        for request_id in 1..=3 {
            queue.push(rest_request(request_id));
        }

        assert_eq!(
            queue.start().map(|request| request.request_id),
            Some(1),
            "the first request starts at once"
        );
        assert!(queue.start().is_none(), "the second waits its turn");

        queue.finish();
        assert_eq!(queue.start().map(|request| request.request_id), Some(2));

        let abandoned: Vec<u64> = queue
            .abandon()
            .iter()
            .map(|request| request.request_id)
            .collect();
        assert_eq!(abandoned, vec![3]);
        assert!(queue.abandon().is_empty());
    }

    #[test]
    fn a_failed_transfer_names_what_the_ui_asked_for() {
        let upload = Transfer::Upload {
            request_id: 4,
            channel_id: GENERAL,
            file_name: "shot.png".to_owned(),
            content_type: "image/png".to_owned(),
            bytes: Blob::from(vec![0; 8]),
        };

        match transfer_failed(&upload, "boom".to_owned()) {
            Event::UploadFailed { request_id, error } => {
                assert_eq!(request_id, 4);
                assert_eq!(error, "boom");
            }
            other => panic!("expected an UploadFailed, got {other:?}"),
        }

        match transfer_failed(&transfer(5), "boom".to_owned()) {
            Event::FetchFailed {
                request_id,
                id,
                error,
            } => {
                assert_eq!(request_id, 5);
                assert_eq!(id, 100);
                assert_eq!(error, "boom");
            }
            other => panic!("expected a FetchFailed, got {other:?}"),
        }

        let fetch_image = Transfer::FetchImage {
            request_id: 6,
            id: 11,
        };
        match transfer_failed(&fetch_image, "boom".to_owned()) {
            Event::ImageFetchFailed {
                request_id,
                id,
                error,
            } => {
                assert_eq!(request_id, 6);
                assert_eq!(id, 11);
                assert_eq!(error, "boom");
            }
            other => panic!("expected an ImageFetchFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_watch_state_of_zero_means_nobody() {
        assert_eq!(watch_target(0), None);
        assert_eq!(watch_target(7), Some(7));
    }

    #[test]
    fn a_session_replaced_error_is_terminal() {
        let reason = terminal_reason(&error(ErrorCode::SessionReplaced, true))
            .expect("a replaced session stops the loop");

        assert!(
            matches!(reason, DisconnectReason::SessionReplaced),
            "{reason}"
        );
        assert!(matches!(
            fatal_outcome(&error(ErrorCode::SessionReplaced, true)),
            AfterAttempt::Stop(Some(DisconnectReason::SessionReplaced))
        ));
    }

    #[test]
    fn a_fatal_kicked_error_is_terminal() {
        let reason =
            terminal_reason(&error(ErrorCode::Kicked, true)).expect("a kick stops the loop");

        assert!(matches!(reason, DisconnectReason::Kicked), "{reason}");
        assert_eq!(reason.to_string(), "kicked from the server");
        // Only the fatal flag closes the door; a non-fatal code is the server
        // being chatty, and the loop reconnects.
        assert!(terminal_reason(&error(ErrorCode::Kicked, false)).is_none());
    }

    #[test]
    fn a_fatal_banned_error_is_terminal() {
        let reason =
            terminal_reason(&error(ErrorCode::Banned, true)).expect("a ban stops the loop");

        assert!(matches!(reason, DisconnectReason::Banned), "{reason}");
        assert_eq!(reason.to_string(), "banned from the server");
        assert!(terminal_reason(&error(ErrorCode::Banned, false)).is_none());
    }

    /// Every other fatal error is followed by the server closing the socket, so
    /// the loop backs off instead of giving up.
    #[test]
    fn another_fatal_error_only_costs_the_attempt() {
        assert!(terminal_reason(&error(ErrorCode::Protocol, true)).is_none());
        assert!(matches!(
            fatal_outcome(&error(ErrorCode::Protocol, true)),
            AfterAttempt::Reconnect { .. }
        ));
    }

    #[tokio::test]
    async fn an_admin_command_dropped_names_its_kind() {
        let (mut events, mut received) = mpsc::channel(1);

        assert!(
            drop_command(
                Command::Admin(AdminCommand::BanMember {
                    user_id: 9,
                    reason: "spam".to_owned(),
                }),
                &mut events,
            )
            .await
        );

        match received.try_recv() {
            Ok(Event::AdminDropped { kind }) => assert_eq!(kind, "BanMember"),
            other => panic!("expected an AdminDropped, got {other:?}"),
        }
        assert_eq!(
            AdminCommand::CreateChannel {
                kind: ChannelKind::Text,
                name: "music".to_owned(),
                topic: String::new(),
                category_id: 0,
            }
            .kind_name(),
            "CreateChannel"
        );
    }

    /// Nothing the server can send before `Welcome` may reach the
    /// "known but unhandled" path: every payload of the schema is named here.
    #[test]
    fn every_server_payload_has_a_welcome_arm() {
        let ignored = [
            server_frame::Payload::Message(Default::default()),
            server_frame::Payload::Pong(Default::default()),
            server_frame::Payload::VoiceReady(Default::default()),
            server_frame::Payload::VoiceState(Default::default()),
            server_frame::Payload::VoiceMemberJoined(Default::default()),
            server_frame::Payload::VoiceMemberLeft(Default::default()),
            server_frame::Payload::Speaking(Default::default()),
            server_frame::Payload::MessageEdited(Default::default()),
            server_frame::Payload::MessageDeleted(Default::default()),
            server_frame::Payload::ReactionsChanged(Default::default()),
            server_frame::Payload::ShareStarted(Default::default()),
            server_frame::Payload::ShareStopped(Default::default()),
            server_frame::Payload::WatchState(Default::default()),
            server_frame::Payload::ShareWatchers(Default::default()),
            server_frame::Payload::ServerSnapshot(Default::default()),
            server_frame::Payload::ServerUpdated(Default::default()),
            server_frame::Payload::RoleUpserted(Default::default()),
            server_frame::Payload::RoleDeleted(Default::default()),
            server_frame::Payload::RoleOrder(Default::default()),
            server_frame::Payload::CategoryUpserted(Default::default()),
            server_frame::Payload::CategoryDeleted(Default::default()),
            server_frame::Payload::ChannelUpserted(Default::default()),
            server_frame::Payload::ChannelDeleted(Default::default()),
            server_frame::Payload::ChannelOrder(Default::default()),
            server_frame::Payload::MemberUpdated(Default::default()),
            server_frame::Payload::MemberRemoved(Default::default()),
            server_frame::Payload::VoiceMoved(Default::default()),
            server_frame::Payload::StreamRequest(Default::default()),
            server_frame::Payload::SoundUpserted(Default::default()),
            server_frame::Payload::SoundDeleted(Default::default()),
            server_frame::Payload::SoundPlayed(Default::default()),
            server_frame::Payload::SoundStopped(Default::default()),
        ];
        // `ServerFrame` carries 34 payloads: these are all but Welcome and Error.
        assert_eq!(ignored.len(), 32);

        for payload in ignored {
            let printed = format!("{payload:?}");
            let frame = ServerFrame {
                payload: Some(payload),
            };
            match classify_first_frame(frame.payload) {
                // The log line names the payload it ignored.
                FirstFrame::Ignore(name) => {
                    assert!(printed.starts_with(name), "{name} for {printed}");
                }
                other => panic!("expected {printed} to be ignored, got {other:?}"),
            }
        }

        assert!(matches!(
            classify_first_frame(Some(server_frame::Payload::Welcome(Default::default()))),
            FirstFrame::Welcome(_)
        ));
        assert!(matches!(
            classify_first_frame(Some(server_frame::Payload::Error(Default::default()))),
            FirstFrame::Error(_)
        ));
        assert!(matches!(classify_first_frame(None), FirstFrame::Unknown));
    }

    /// A debug-level log file would otherwise persist every message anyone
    /// sends, which is the one thing this client never writes to disk.
    #[test]
    fn describing_a_message_frame_never_prints_what_was_typed() {
        let message = ChatMessage {
            id: 7,
            author: "alice".to_owned(),
            text: "secret-needle".to_owned(),
            channel_id: GENERAL,
            author_id: 3,
            reply_to: Some(ReplyRef {
                id: 6,
                author: "bob".to_owned(),
                excerpt: "secret-excerpt".to_owned(),
                deleted: false,
            }),
            attachments: vec![Attachment::default()],
            ..ChatMessage::default()
        };

        for frame in [
            ServerFrame {
                payload: Some(server_frame::Payload::Message(message.clone())),
            },
            ServerFrame {
                payload: Some(server_frame::Payload::MessageEdited(MessageEdited {
                    message: Some(message.clone()),
                })),
            },
        ] {
            let described = describe(&frame);

            assert!(!described.contains("secret-needle"), "{described}");
            assert!(!described.contains("secret-excerpt"), "{described}");
            assert!(!described.contains("alice"), "{described}");
            assert!(described.contains("text_len: 13"), "{described}");
            assert!(described.contains("channel_id: 1"), "{described}");
            assert!(described.contains("attachments: 1"), "{described}");
            assert!(described.contains("reply_to: true"), "{described}");
            assert!(described.contains("author_id: 3"), "{described}");
        }
    }

    fn close_frame(code: u16, reason: &str) -> CloseFrame {
        CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        }
    }

    #[test]
    fn the_admin_close_codes_are_read_apart_from_every_other_close() {
        match closed_reason(Some(&close_frame(4001, "kicked by admin"))) {
            DisconnectReason::Kicked => {}
            other => panic!("4001 must read as a kick, got {other}"),
        }

        match closed_reason(Some(&close_frame(4003, "account disabled"))) {
            DisconnectReason::Disabled => {}
            other => panic!("4003 must read as a disabled account, got {other}"),
        }

        match terminal_reason(&error(ErrorCode::Banned, true)) {
            Some(DisconnectReason::Banned) => {}
            other => panic!("a fatal ban frame must read as a ban, got {other:?}"),
        }

        match closed_reason(Some(&close_frame(1008, "protocol error"))) {
            DisconnectReason::ServerClosed { code, reason } => {
                assert_eq!(code, Some(1008));
                assert_eq!(reason, "protocol error");
            }
            other => panic!("1008 must stay a plain close, got {other}"),
        }
    }

    #[test]
    fn only_the_admin_close_codes_stop_the_loop_for_good() {
        let kicked = after_close(Some(&close_frame(4001, "kicked by admin")), Retry::Backoff);
        assert!(matches!(
            kicked,
            AfterAttempt::Stop(Some(DisconnectReason::Kicked))
        ));

        let disabled = after_close(Some(&close_frame(4003, "account disabled")), Retry::Backoff);
        assert!(matches!(
            disabled,
            AfterAttempt::Stop(Some(DisconnectReason::Disabled))
        ));

        let closed = after_close(Some(&close_frame(1008, "protocol error")), Retry::Backoff);
        assert!(matches!(closed, AfterAttempt::Reconnect { .. }));

        let dropped = after_close(None, Retry::BackoffAfterSession);
        assert!(matches!(dropped, AfterAttempt::Reconnect { .. }));
    }

    #[test]
    fn a_refused_refresh_stops_the_loop_for_a_disabled_or_banned_account() {
        let disabled = refresh_refusal(ApiFailure::Status(403, "account disabled".to_owned()));
        match refresh_outcome(disabled) {
            Err(AfterAttempt::Stop(Some(DisconnectReason::Disabled))) => {}
            _ => panic!("a disabled refresh must stop the loop with the disabled reason"),
        }

        let banned = refresh_refusal(ApiFailure::Status(403, "banned".to_owned()));
        match refresh_outcome(banned) {
            Err(AfterAttempt::Stop(Some(DisconnectReason::Banned))) => {}
            _ => panic!("a banned refresh must stop the loop with the banned reason"),
        }
    }

    #[test]
    fn a_refresh_403_from_anywhere_else_only_backs_off() {
        let other = refresh_refusal(ApiFailure::Status(403, "Forbidden by proxy".to_owned()));
        assert!(
            matches!(other, Refreshed::Failed(_)),
            "an unknown 403 detail must not be read as an account state"
        );

        match refresh_outcome(other) {
            Err(AfterAttempt::Reconnect {
                after: Retry::Backoff,
                ..
            }) => {}
            _ => panic!("an unknown 403 detail must back off rather than stop the loop"),
        }

        let challenged = refresh_refusal(ApiFailure::Status(401, "token expired".to_owned()));
        match refresh_outcome(challenged) {
            Err(AfterAttempt::Stop(Some(DisconnectReason::AuthRequired(_)))) => {}
            _ => panic!("a 401 must ask for a new sign-in"),
        }
    }
}
