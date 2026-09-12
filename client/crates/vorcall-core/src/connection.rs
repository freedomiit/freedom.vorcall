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
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
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
    Attachment, ChatMessage, ClientFrame, CreateRoom, DeleteMessage, EditMessage, ErrorCode, Hello,
    JoinRoom, JoinVoice, LeaveRoom, LeaveVoice, MarkRead, Member, MessagePage, OpenDm, Ping, React,
    Reaction, Room, RoomEntry, SendMessage, ServerFrame, StartShare, StopShare, UnwatchShare,
    VoiceMember, WatchShare, client_frame, server_frame,
};

use crate::attachments;
use crate::auth;
use crate::endpoints::Endpoints;
use crate::history;
use crate::http::{self, ApiFailure};
use crate::session::{self, Session};
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
const BANNED_CLOSE_CODE: u16 = 4003;
const HISTORY_LIMIT: u32 = 100;
/// `PROTOCOL.md` stops gap-filling here; beyond it a gap may remain.
const GAP_FILL_MAX_PAGES: usize = 5;
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const BACKOFF_JITTER: f64 = 0.20;
/// Attachment transfers this loop runs at once; the rest wait their turn.
const TRANSFER_SLOTS: usize = 2;

/// The room every account belongs to from registration on; it cannot be left.
pub const GENERAL_ROOM: &str = "general";

#[derive(Debug, Clone)]
pub enum Command {
    Send {
        room_id: String,
        text: String,
        /// The message this one answers; `None` when it answers nothing.
        reply_to_id: Option<i64>,
        attachment_ids: Vec<i64>,
    },
    /// The newest page of a room, asked for when the UI first opens it.
    LoadHistory {
        room_id: String,
    },
    LoadOlder {
        room_id: String,
        before: i64,
    },
    JoinRoom {
        room_id: String,
    },
    LeaveRoom {
        room_id: String,
    },
    CreateRoom {
        name: String,
    },
    OpenDm {
        user_id: i64,
    },
    MarkRead {
        room_id: String,
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
        room_id: String,
        file_name: String,
        content_type: &'static str,
        bytes: Blob,
    },
    FetchAttachment {
        request_id: u64,
        id: i64,
    },
    JoinVoice {
        room_id: String,
    },
    LeaveVoice {
        room_id: String,
    },
    /// Sent once the local capture is running; idempotent server-side.
    StartShare {
        room_id: String,
        audio: bool,
    },
    StopShare {
        room_id: String,
    },
    /// Replaces any previous watch.
    WatchShare {
        room_id: String,
        user_id: i64,
    },
    UnwatchShare {
        room_id: String,
    },
}

/// The per-session media key from `VoiceReady`. Debug never prints it.
#[derive(Clone)]
pub struct MediaKey(pub [u8; 32]);

impl fmt::Debug for MediaKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaKey(<redacted>)")
    }
}

/// One attachment's bytes, shared rather than copied: a retry re-reads the very
/// same buffer, and so does the request body. Debug prints only the size —
/// every [`Command`] and [`Event`] is printed whole in a log line.
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
    /// An admin closed this connection; signing in again is allowed.
    Kicked(String),
    /// The account itself is disabled, so nothing this client holds still works.
    Banned(String),
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
            Self::Kicked(detail) if !detail.is_empty() => {
                write!(f, "disconnected by the admin: {detail}")
            }
            Self::Kicked(_) => f.write_str("disconnected by the admin"),
            Self::Banned(detail) if !detail.is_empty() => {
                write!(f, "this account is banned: {detail}")
            }
            Self::Banned(_) => f.write_str("this account is banned"),
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
    /// The newest page of one room plus whatever gap-fill added, ascending.
    History {
        room_id: String,
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    HistoryFailed {
        room_id: String,
        error: String,
    },
    OlderPage {
        room_id: String,
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    OlderFailed {
        room_id: String,
        error: String,
    },
    Users(Vec<Member>),
    UsersFailed(String),
    RoomState {
        room_id: String,
        members: Vec<Member>,
    },
    MemberJoined {
        room_id: String,
        member: Member,
    },
    MemberLeft {
        room_id: String,
        user_id: i64,
    },
    Message(ChatMessage),
    /// Every room the account can see, each with the reader's own counters.
    RoomList(Vec<RoomEntry>),
    RoomUpdated(Room),
    MessageEdited(ChatMessage),
    MessageDeleted {
        room_id: String,
        id: i64,
    },
    ReactionsChanged {
        room_id: String,
        message_id: i64,
        reactions: Vec<Reaction>,
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
    ServerError {
        code: i32,
        detail: String,
        fatal: bool,
    },
    SendDropped,
    /// A rotated token pair, already persisted by this loop.
    SessionUpdated(Session),
    VoiceReady {
        room_id: String,
        host: String,
        port: u16,
        key: MediaKey,
        ssrc: u32,
    },
    VoiceState {
        room_id: String,
        members: Vec<VoiceMember>,
    },
    VoiceMemberJoined {
        room_id: String,
        member: VoiceMember,
    },
    VoiceMemberLeft {
        room_id: String,
        user_id: i64,
    },
    Speaking {
        room_id: String,
        user_id: i64,
        speaking: bool,
    },
    ShareStarted {
        room_id: String,
        user_id: i64,
        audio: bool,
    },
    ShareStopped {
        room_id: String,
        user_id: i64,
    },
    /// Which share this client is watching now; `None` means none.
    WatchState {
        room_id: String,
        user_id: Option<i64>,
    },
    /// How many peers are watching the local share.
    ShareWatchers {
        room_id: String,
        count: u32,
    },
}

/// `WatchState.user_id` is 0 when the server means "not watching anyone".
fn watch_target(user_id: i64) -> Option<i64> {
    (user_id != 0).then_some(user_id)
}

/// A log-safe rendering of a received frame: `VoiceReady` carries the media key
/// and a message carries what somebody typed, neither of which may ever reach a
/// log line.
fn describe(frame: &ServerFrame) -> String {
    match &frame.payload {
        Some(server_frame::Payload::VoiceReady(ready)) => {
            let room_id = &ready.room_id;
            let host = &ready.host;
            let port = ready.port;
            let ssrc = ready.ssrc;
            format!(
                "VoiceReady {{ room_id: {room_id:?}, host: {host:?}, port: {port}, ssrc: {ssrc}, key: <redacted> }}"
            )
        }
        Some(server_frame::Payload::Message(message)) => {
            format!("Message {{ {} }}", describe_message(message))
        }
        Some(server_frame::Payload::MessageEdited(edited)) => match &edited.message {
            Some(message) => format!("MessageEdited {{ {} }}", describe_message(message)),
            None => "MessageEdited { message: None }".to_owned(),
        },
        _ => format!("{frame:?}"),
    }
}

/// Everything about a message that is worth a log line and nothing that is
/// worth keeping private: no text, no author name, no reply excerpt.
fn describe_message(message: &ChatMessage) -> String {
    let id = message.id;
    let room_id = &message.room_id;
    let author_id = message.author_id;
    let text_len = message.text.len();
    let attachments = message.attachments.len();
    let reply_to = message.reply_to.is_some();
    format!(
        "id: {id}, room_id: {room_id:?}, author_id: {author_id}, text_len: {text_len}, attachments: {attachments}, reply_to: {reply_to}"
    )
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
    // Survives every attempt: per room, what a reconnect gap-fills against.
    let mut newest_delivered: HashMap<String, i64> = HashMap::new();

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

    match refresh_session(endpoints, session, events).await {
        Refreshed::Ok => Ok(()),
        Refreshed::AuthRequired(detail) => Err(AfterAttempt::Stop(Some(
            DisconnectReason::AuthRequired(detail),
        ))),
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
        Err(ApiFailure::AuthChallenge(detail) | ApiFailure::Status(401, detail)) => {
            tracing::warn!("the refresh token was refused; the user must sign in again");
            Refreshed::AuthRequired(detail)
        }
        Err(failure) => {
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
        Command::LoadHistory { room_id } => {
            tracing::debug!(%room_id, "cannot load history while disconnected");
            events
                .send(Event::HistoryFailed {
                    room_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::LoadOlder { room_id, before } => {
            tracing::debug!(%room_id, before, "cannot load older messages while disconnected");
            events
                .send(Event::OlderFailed {
                    room_id,
                    error: "not connected".to_owned(),
                })
                .await
                .is_ok()
        }
        Command::UploadAttachment {
            request_id,
            room_id,
            ..
        } => {
            tracing::debug!(request_id, %room_id, "cannot upload an attachment while disconnected");
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
        // No event: the UI re-sends JoinVoice after every Connected.
        Command::JoinVoice { room_id } | Command::LeaveVoice { room_id } => {
            tracing::debug!(%room_id, "cannot change voice membership while disconnected");
            true
        }
        // No event: the UI re-asserts the share state after the next VoiceReady.
        Command::StartShare { room_id, .. }
        | Command::StopShare { room_id }
        | Command::WatchShare { room_id, .. }
        | Command::UnwatchShare { room_id } => {
            tracing::debug!(%room_id, "cannot change screen share while disconnected");
            true
        }
        // No event either: the UI disables these while disconnected.
        Command::JoinRoom { .. }
        | Command::LeaveRoom { .. }
        | Command::CreateRoom { .. }
        | Command::OpenDm { .. }
        | Command::MarkRead { .. }
        | Command::Edit { .. }
        | Command::Delete { .. }
        | Command::React { .. } => {
            tracing::debug!("dropping a room command queued while disconnected");
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
    ($label:lifetime, $sink:expr, $what:literal, $payload:expr) => {
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
    newest_delivered: &mut HashMap<String, i64>,
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

                match server_frame.payload {
                    Some(server_frame::Payload::Welcome(welcome)) => return Ok(welcome),
                    Some(server_frame::Payload::Error(error)) => {
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
                    // Voice frames cannot precede Welcome, but ignoring one is
                    // cheaper than tearing down an otherwise healthy attempt.
                    Some(server_frame::Payload::VoiceReady(ready)) => {
                        tracing::debug!(room = %ready.room_id, "ignoring a voice frame before Welcome");
                    }
                    Some(server_frame::Payload::VoiceState(state)) => {
                        tracing::debug!(room = %state.room_id, "ignoring a voice frame before Welcome");
                    }
                    Some(server_frame::Payload::VoiceMemberJoined(joined)) => {
                        tracing::debug!(room = %joined.room_id, "ignoring a voice frame before Welcome");
                    }
                    Some(server_frame::Payload::VoiceMemberLeft(left)) => {
                        tracing::debug!(room = %left.room_id, "ignoring a voice frame before Welcome");
                    }
                    Some(server_frame::Payload::Speaking(speaking)) => {
                        tracing::debug!(room = %speaking.room_id, "ignoring a voice frame before Welcome");
                    }
                    // Share frames cannot precede Welcome either.
                    Some(server_frame::Payload::ShareStarted(started)) => {
                        tracing::debug!(room = %started.room_id, "ignoring a share frame before Welcome");
                    }
                    Some(server_frame::Payload::ShareStopped(stopped)) => {
                        tracing::debug!(room = %stopped.room_id, "ignoring a share frame before Welcome");
                    }
                    Some(server_frame::Payload::WatchState(state)) => {
                        tracing::debug!(room = %state.room_id, "ignoring a share frame before Welcome");
                    }
                    Some(server_frame::Payload::ShareWatchers(watchers)) => {
                        tracing::debug!(room = %watchers.room_id, "ignoring a share frame before Welcome");
                    }
                    // Room and message frames cannot precede Welcome either.
                    Some(server_frame::Payload::RoomList(list)) => {
                        tracing::debug!(
                            rooms = list.rooms.len(),
                            "ignoring a room frame before Welcome"
                        );
                    }
                    Some(server_frame::Payload::RoomUpdated(updated)) => {
                        let room = updated.room.map(|room| room.room_id).unwrap_or_default();
                        tracing::debug!(%room, "ignoring a room frame before Welcome");
                    }
                    Some(server_frame::Payload::MessageEdited(edited)) => {
                        let id = edited.message.map(|message| message.id).unwrap_or_default();
                        tracing::debug!(id, "ignoring a message frame before Welcome");
                    }
                    Some(server_frame::Payload::MessageDeleted(deleted)) => {
                        tracing::debug!(room = %deleted.room_id, id = deleted.id, "ignoring a message frame before Welcome");
                    }
                    Some(server_frame::Payload::ReactionsChanged(changed)) => {
                        tracing::debug!(
                            room = %changed.room_id,
                            id = changed.message_id,
                            "ignoring a message frame before Welcome"
                        );
                    }
                    // A payload this build does not know: a newer server may add
                    // frames without a version bump.
                    None => {
                        tracing::warn!(
                            frame = %describe(&server_frame),
                            "ignoring unknown server frame"
                        );
                    }
                    other => {
                        return Err(AfterAttempt::Reconnect {
                            reason: DisconnectReason::ProtocolError(format!(
                                "expected Welcome, got {other:?}"
                            )),
                            after: Retry::Backoff,
                        });
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
    room_id: String,
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
        Refreshed::Failed(_) => FetchRecovery::Report(detail),
        Refreshed::UiGone => FetchRecovery::Stop(AfterAttempt::Stop(None)),
    }
}

/// The rooms waiting for their newest page. One fetch runs at a time, and a
/// room already queued or in flight is never asked for twice: the UI may open
/// the same room again long before the first answer arrives.
#[derive(Default)]
struct HistoryQueue {
    in_flight: Option<String>,
    waiting: VecDeque<String>,
}

impl HistoryQueue {
    /// `false` when that room is already queued or in flight.
    fn enqueue(&mut self, room_id: String) -> bool {
        if self.in_flight.as_deref() == Some(room_id.as_str())
            || self.waiting.iter().any(|queued| *queued == room_id)
        {
            return false;
        }
        self.waiting.push_back(room_id);
        true
    }

    /// The next room to fetch, or `None` while one is already in flight.
    fn start(&mut self) -> Option<String> {
        if self.in_flight.is_some() {
            return None;
        }
        let room_id = self.waiting.pop_front()?;
        self.in_flight = Some(room_id.clone());
        Some(room_id)
    }

    fn in_flight(&self) -> Option<&str> {
        self.in_flight.as_deref()
    }

    fn finish(&mut self) {
        self.in_flight = None;
    }

    /// Every room still expecting a page, so a disconnect can answer them all.
    fn abandon(&mut self) -> Vec<String> {
        self.in_flight
            .take()
            .into_iter()
            .chain(self.waiting.drain(..))
            .collect()
    }
}

/// One attachment transfer the UI asked for.
#[derive(Clone)]
enum Transfer {
    Upload {
        request_id: u64,
        room_id: String,
        file_name: String,
        content_type: &'static str,
        bytes: Blob,
    },
    Fetch {
        request_id: u64,
        id: i64,
    },
}

impl Transfer {
    fn request_id(&self) -> u64 {
        match self {
            Self::Upload { request_id, .. } | Self::Fetch { request_id, .. } => *request_id,
        }
    }
}

/// The attachment transfers, in the order the UI asked for them.
/// [`TRANSFER_SLOTS`] of them run at once, so one 8 MiB upload cannot hold up
/// every thumbnail behind it.
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

    /// What never started, so a disconnect can answer it.
    fn abandon(&mut self) -> Vec<Transfer> {
        self.waiting.drain(..).collect()
    }
}

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

/// Runs one transfer to the event that answers it. The bytes never reach a log
/// line; their size does.
async fn run_transfer(
    endpoints: &Endpoints,
    access_token: String,
    request: Transfer,
) -> Result<Event, ApiFailure> {
    match request {
        Transfer::Upload {
            request_id,
            room_id,
            file_name,
            content_type,
            bytes,
        } => {
            let attachment = attachments::upload(
                endpoints,
                &access_token,
                &room_id,
                &file_name,
                content_type,
                bytes,
            )
            .await?;
            tracing::info!(
                request_id,
                room = %room_id,
                id = attachment.id,
                size = attachment.size,
                "attachment uploaded"
            );
            Ok(Event::AttachmentUploaded {
                request_id,
                attachment,
            })
        }
        Transfer::Fetch { request_id, id } => {
            let bytes = attachments::download(endpoints, &access_token, id).await?;
            tracing::debug!(request_id, id, size = bytes.len(), "attachment fetched");
            Ok(Event::AttachmentFetched {
                request_id,
                id,
                bytes: Blob::from(bytes),
            })
        }
    }
}

/// The event telling the UI one transfer is not coming.
fn transfer_failed(request: &Transfer, error: String) -> Event {
    match request {
        Transfer::Upload { request_id, .. } => Event::UploadFailed {
            request_id: *request_id,
            error,
        },
        Transfer::Fetch { request_id, id } => Event::FetchFailed {
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
        move |token| -> Boxed<'a, Event> { Box::pin(run_transfer(endpoints, token, request)) }
    };

    match settle(slot, result, reissue, endpoints, session, events).await {
        Settled::Done(event) => Transferred::Emit(Box::new(event)),
        Settled::Retried => Transferred::Retried,
        Settled::Report(error) => {
            tracing::warn!(
                request_id = request.request_id(),
                %error,
                "an attachment transfer failed"
            );
            Transferred::Emit(Box::new(transfer_failed(&request, error)))
        }
        Settled::Stop(outcome) => Transferred::Stop(outcome),
    }
}

/// Starts the next queued room's newest page when nothing is in flight.
fn start_history<'a>(
    queue: &mut HistoryQueue,
    slot: &mut Option<Fetch<'a, (Vec<ChatMessage>, bool)>>,
    endpoints: &'a Endpoints,
    access_token: &str,
    newest_delivered: &HashMap<String, i64>,
) {
    if slot.is_some() {
        return;
    }
    let Some(room_id) = queue.start() else {
        return;
    };

    let newest = newest_delivered.get(&room_id).copied();
    *slot = Some(Fetch::new(initial_history(
        endpoints,
        access_token.to_owned(),
        room_id,
        newest,
    )));
}

/// Fills whichever transfer slots are free, in the order the UI asked.
fn start_transfers<'a>(
    queue: &mut TransferQueue,
    slots: [&mut Option<TransferFetch<'a>>; TRANSFER_SLOTS],
    endpoints: &'a Endpoints,
    access_token: &str,
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
            )),
            request,
        });
    }
}

/// The newest page of one room, plus the pages a reconnect needs to close the
/// gap between what the UI already has there and what the server kept.
async fn initial_history(
    endpoints: &Endpoints,
    access_token: String,
    room_id: String,
    newest_delivered: Option<i64>,
) -> Result<(Vec<ChatMessage>, bool), ApiFailure> {
    let page = history::fetch_page(endpoints, &access_token, &room_id, HISTORY_LIMIT, None).await?;
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
            &room_id,
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
        room = %room_id,
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
    room_id: String,
    before: i64,
) -> Result<MessagePage, ApiFailure> {
    history::fetch_page(
        endpoints,
        &access_token,
        &room_id,
        HISTORY_LIMIT,
        Some(before),
    )
    .await
}

async fn user_list(endpoints: &Endpoints, access_token: String) -> Result<Vec<Member>, ApiFailure> {
    history::fetch_users(endpoints, &access_token).await
}

/// Remembers the newest id the UI has been handed in a room, so the next page
/// there knows where its gap starts.
fn note_newest(newest_delivered: &mut HashMap<String, i64>, room_id: &str, id: i64) {
    if let Some(newest) = newest_delivered.get_mut(room_id) {
        *newest = (*newest).max(id);
    } else {
        newest_delivered.insert(room_id.to_owned(), id);
    }
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
    newest_delivered: &mut HashMap<String, i64>,
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
    // History is per room and on demand: nothing is fetched until the UI opens
    // a room and asks for it.
    let mut history = HistoryQueue::default();
    let mut history_fetch: Option<Fetch<'_, (Vec<ChatMessage>, bool)>> = None;
    let mut users_fetch = Some(Fetch::new(user_list(
        endpoints,
        session.access_token.clone(),
    )));
    let mut older_fetch: Option<OlderFetch<'_>> = None;
    let mut transfers = TransferQueue::default();
    // Two named slots rather than an array: each needs its own `select!` arm.
    let mut first_transfer: Option<TransferFetch<'_>> = None;
    let mut second_transfer: Option<TransferFetch<'_>> = None;

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
                                    room = %message.room_id,
                                    author = %message.author,
                                    "message"
                                );
                                note_newest(newest_delivered, &message.room_id, message.id);
                                emit_or_break!('live, events, Event::Message(message));
                            }
                            Some(server_frame::Payload::RoomState(state)) => {
                                emit_or_break!('live, events, Event::RoomState {
                                    room_id: state.room_id,
                                    members: state.members,
                                });
                            }
                            Some(server_frame::Payload::MemberJoined(joined)) => {
                                match joined.member {
                                    Some(member) => emit_or_break!('live, events, Event::MemberJoined {
                                        room_id: joined.room_id,
                                        member,
                                    }),
                                    None => tracing::warn!(
                                        room = %joined.room_id,
                                        "ignoring a MemberJoined without a member"
                                    ),
                                }
                            }
                            Some(server_frame::Payload::MemberLeft(left)) => {
                                emit_or_break!('live, events, Event::MemberLeft {
                                    room_id: left.room_id,
                                    user_id: left.user_id,
                                });
                            }
                            Some(server_frame::Payload::RoomList(list)) => {
                                tracing::debug!(rooms = list.rooms.len(), "room list");
                                emit_or_break!('live, events, Event::RoomList(list.rooms));
                            }
                            Some(server_frame::Payload::RoomUpdated(updated)) => {
                                match updated.room {
                                    Some(room) => emit_or_break!('live, events, Event::RoomUpdated(room)),
                                    None => tracing::warn!("ignoring a RoomUpdated without a room"),
                                }
                            }
                            Some(server_frame::Payload::MessageEdited(edited)) => {
                                match edited.message {
                                    Some(message) => {
                                        note_newest(newest_delivered, &message.room_id, message.id);
                                        emit_or_break!('live, events, Event::MessageEdited(message));
                                    }
                                    None => tracing::warn!("ignoring a MessageEdited without a message"),
                                }
                            }
                            Some(server_frame::Payload::MessageDeleted(deleted)) => {
                                emit_or_break!('live, events, Event::MessageDeleted {
                                    room_id: deleted.room_id,
                                    id: deleted.id,
                                });
                            }
                            Some(server_frame::Payload::ReactionsChanged(changed)) => {
                                emit_or_break!('live, events, Event::ReactionsChanged {
                                    room_id: changed.room_id,
                                    message_id: changed.message_id,
                                    reactions: changed.reactions,
                                });
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
                                // closing; this one has to stop the loop for good.
                                if is_session_replaced(&error) {
                                    break AfterAttempt::Stop(Some(DisconnectReason::SessionReplaced));
                                }
                            }
                            Some(server_frame::Payload::VoiceReady(ready)) => {
                                let Ok(key) = <[u8; 32]>::try_from(ready.key.as_slice()) else {
                                    tracing::warn!(
                                        room = %ready.room_id,
                                        len = ready.key.len(),
                                        "ignoring a VoiceReady whose key is not 32 bytes"
                                    );
                                    continue;
                                };
                                let Ok(port) = u16::try_from(ready.port) else {
                                    tracing::warn!(
                                        room = %ready.room_id,
                                        port = ready.port,
                                        "ignoring a VoiceReady with an out-of-range port"
                                    );
                                    continue;
                                };
                                emit_or_break!('live, events, Event::VoiceReady {
                                    room_id: ready.room_id,
                                    host: ready.host,
                                    port,
                                    key: MediaKey(key),
                                    ssrc: ready.ssrc,
                                });
                            }
                            Some(server_frame::Payload::VoiceState(state)) => {
                                emit_or_break!('live, events, Event::VoiceState {
                                    room_id: state.room_id,
                                    members: state.members,
                                });
                            }
                            Some(server_frame::Payload::VoiceMemberJoined(joined)) => {
                                match joined.member {
                                    Some(member) => emit_or_break!('live, events, Event::VoiceMemberJoined {
                                        room_id: joined.room_id,
                                        member,
                                    }),
                                    None => tracing::warn!(
                                        room = %joined.room_id,
                                        "ignoring a VoiceMemberJoined without a member"
                                    ),
                                }
                            }
                            Some(server_frame::Payload::VoiceMemberLeft(left)) => {
                                emit_or_break!('live, events, Event::VoiceMemberLeft {
                                    room_id: left.room_id,
                                    user_id: left.user_id,
                                });
                            }
                            Some(server_frame::Payload::Speaking(speaking)) => {
                                emit_or_break!('live, events, Event::Speaking {
                                    room_id: speaking.room_id,
                                    user_id: speaking.user_id,
                                    speaking: speaking.speaking,
                                });
                            }
                            Some(server_frame::Payload::ShareStarted(started)) => {
                                emit_or_break!('live, events, Event::ShareStarted {
                                    room_id: started.room_id,
                                    user_id: started.user_id,
                                    audio: started.audio,
                                });
                            }
                            Some(server_frame::Payload::ShareStopped(stopped)) => {
                                emit_or_break!('live, events, Event::ShareStopped {
                                    room_id: stopped.room_id,
                                    user_id: stopped.user_id,
                                });
                            }
                            Some(server_frame::Payload::WatchState(state)) => {
                                emit_or_break!('live, events, Event::WatchState {
                                    room_id: state.room_id,
                                    user_id: watch_target(state.user_id),
                                });
                            }
                            Some(server_frame::Payload::ShareWatchers(watchers)) => {
                                emit_or_break!('live, events, Event::ShareWatchers {
                                    room_id: watchers.room_id,
                                    count: watchers.count,
                                });
                            }
                            // A Pong only had to reach the watchdog above.
                            Some(server_frame::Payload::Pong(_)) => {}
                            // A payload this build does not know: a newer server
                            // may add frames without a version bump.
                            None => tracing::warn!(frame = %describe(&server_frame), "ignoring unknown server frame"),
                            other => break AfterAttempt::Reconnect {
                                reason: DisconnectReason::ProtocolError(format!("unexpected frame {other:?}")),
                                after: Retry::BackoffAfterSession,
                            },
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
                let Some(room_id) = history.in_flight().map(str::to_owned) else {
                    continue;
                };
                let newest = newest_delivered.get(&room_id).copied();
                let reissue = {
                    let room_id = room_id.clone();
                    move |token| -> Boxed<'_, _> {
                        Box::pin(initial_history(endpoints, token, room_id, newest))
                    }
                };
                match settle(&mut history_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done((messages, has_more)) => {
                        history.finish();
                        if let Some(last) = messages.last() {
                            note_newest(newest_delivered, &room_id, last.id);
                        }
                        emit_or_break!('live, events, Event::History { room_id, messages, has_more });
                    }
                    Settled::Retried => {}
                    Settled::Report(error) => {
                        history.finish();
                        tracing::warn!(room = %room_id, %error, "history fetch failed");
                        emit_or_break!('live, events, Event::HistoryFailed { room_id, error });
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

            result = pending(users_fetch.as_mut()), if users_fetch.is_some() => {
                let reissue = |token| -> Boxed<'_, _> {
                    Box::pin(user_list(endpoints, token))
                };
                match settle(&mut users_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done(members) => {
                        emit_or_break!('live, events, Event::Users(members));
                    }
                    Settled::Retried => {}
                    Settled::Report(detail) => {
                        tracing::warn!(%detail, "user list fetch failed");
                        emit_or_break!('live, events, Event::UsersFailed(detail));
                    }
                    Settled::Stop(outcome) => break outcome,
                }
            }

            result = pending(older_fetch.as_mut().map(|older| &mut older.fetch)), if older_fetch.is_some() => {
                // Copied out before `settle` takes the slot, so a refresh can
                // re-issue the very same request.
                let Some((room_id, before)) = older_fetch
                    .as_ref()
                    .map(|older| (older.room_id.clone(), older.before))
                else {
                    continue;
                };
                let reissue = {
                    let room_id = room_id.clone();
                    move |token| -> Boxed<'_, _> {
                        Box::pin(older_page(endpoints, token, room_id, before))
                    }
                };
                match settle(&mut older_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done(page) => {
                        if let Some(last) = page.messages.last() {
                            note_newest(newest_delivered, &room_id, last.id);
                        }
                        emit_or_break!('live, events, Event::OlderPage {
                            room_id,
                            messages: page.messages,
                            has_more: page.has_more,
                        });
                    }
                    Settled::Retried => {}
                    Settled::Report(error) => {
                        tracing::warn!(room = %room_id, %error, "older page fetch failed");
                        emit_or_break!('live, events, Event::OlderFailed { room_id, error });
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
                );
            }

            command = commands.next() => {
                let Some(command) = command else {
                    tracing::info!("UI dropped the command channel; closing");
                    break 'live AfterAttempt::Stop(None);
                };

                match command {
                    Command::Send { room_id, text, reply_to_id, attachment_ids } => {
                        let payload = client_frame::Payload::Send(SendMessage {
                            text,
                            room_id,
                            reply_to_id: reply_to_id.unwrap_or_default(),
                            attachment_ids,
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
                    Command::JoinRoom { room_id } => send_or_break!(
                        'live, sink, "JoinRoom",
                        client_frame::Payload::JoinRoom(JoinRoom { room_id })
                    ),
                    Command::LeaveRoom { room_id } => send_or_break!(
                        'live, sink, "LeaveRoom",
                        client_frame::Payload::LeaveRoom(LeaveRoom { room_id })
                    ),
                    Command::CreateRoom { name } => send_or_break!(
                        'live, sink, "CreateRoom",
                        client_frame::Payload::CreateRoom(CreateRoom { name })
                    ),
                    Command::OpenDm { user_id } => send_or_break!(
                        'live, sink, "OpenDm",
                        client_frame::Payload::OpenDm(OpenDm { user_id })
                    ),
                    Command::MarkRead { room_id, message_id } => send_or_break!(
                        'live, sink, "MarkRead",
                        client_frame::Payload::MarkRead(MarkRead { room_id, message_id })
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
                    Command::JoinVoice { room_id } => send_or_break!(
                        'live, sink, "JoinVoice",
                        client_frame::Payload::JoinVoice(JoinVoice { room_id })
                    ),
                    Command::LeaveVoice { room_id } => send_or_break!(
                        'live, sink, "LeaveVoice",
                        client_frame::Payload::LeaveVoice(LeaveVoice { room_id })
                    ),
                    Command::StartShare { room_id, audio } => send_or_break!(
                        'live, sink, "StartShare",
                        client_frame::Payload::StartShare(StartShare { room_id, audio })
                    ),
                    Command::StopShare { room_id } => send_or_break!(
                        'live, sink, "StopShare",
                        client_frame::Payload::StopShare(StopShare { room_id })
                    ),
                    Command::WatchShare { room_id, user_id } => send_or_break!(
                        'live, sink, "WatchShare",
                        client_frame::Payload::WatchShare(WatchShare { room_id, user_id })
                    ),
                    Command::UnwatchShare { room_id } => send_or_break!(
                        'live, sink, "UnwatchShare",
                        client_frame::Payload::UnwatchShare(UnwatchShare { room_id })
                    ),
                    Command::LoadHistory { room_id } => {
                        if history.enqueue(room_id.clone()) {
                            start_history(
                                &mut history,
                                &mut history_fetch,
                                endpoints,
                                &session.access_token,
                                newest_delivered,
                            );
                        } else {
                            tracing::debug!(%room_id, "that room's history is already loading");
                        }
                    }
                    Command::LoadOlder { room_id, before } => {
                        if older_fetch.is_some() {
                            tracing::debug!(%room_id, before, "an older page is already loading");
                        } else {
                            let request = older_page(
                                endpoints,
                                session.access_token.clone(),
                                room_id.clone(),
                                before,
                            );
                            older_fetch = Some(OlderFetch {
                                fetch: Fetch::new(request),
                                room_id,
                                before,
                            });
                        }
                    }
                    Command::UploadAttachment { request_id, room_id, file_name, content_type, bytes } => {
                        tracing::info!(request_id, %room_id, size = bytes.len(), "attachment upload queued");
                        transfers.push(Transfer::Upload {
                            request_id,
                            room_id,
                            file_name,
                            content_type,
                            bytes,
                        });
                        start_transfers(
                            &mut transfers,
                            [&mut first_transfer, &mut second_transfer],
                            endpoints,
                            &session.access_token,
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
                        );
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

    // The UI keeps a "loading" flag per room and per transfer until each of
    // these answers, and the socket is about to go: answer them here so nothing
    // spins for ever. Nothing is replayed into the next session; the UI asks
    // again once it is connected.
    for room_id in history.abandon() {
        let _ = events
            .send(Event::HistoryFailed {
                room_id,
                error: "disconnected".to_owned(),
            })
            .await;
    }
    if let Some(older) = older_fetch {
        let _ = events
            .send(Event::OlderFailed {
                room_id: older.room_id,
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

    close_gracefully(sink).await;
    outcome
}

fn is_session_replaced(error: &vorcall_proto::v1::Error) -> bool {
    error.fatal && ErrorCode::try_from(error.code) == Ok(ErrorCode::SessionReplaced)
}

/// A replaced session is the one error retrying cannot fix: the account is live
/// somewhere else, so the loop stops instead of fighting the other client.
fn fatal_outcome(error: &vorcall_proto::v1::Error) -> AfterAttempt {
    if is_session_replaced(error) {
        return AfterAttempt::Stop(Some(DisconnectReason::SessionReplaced));
    }
    AfterAttempt::Reconnect {
        reason: DisconnectReason::ProtocolError(error.detail.clone()),
        after: Retry::Backoff,
    }
}

fn closed_reason(frame: Option<&CloseFrame>) -> DisconnectReason {
    match frame {
        Some(frame) => {
            let reason = frame.reason.as_str().to_owned();
            match u16::from(frame.code) {
                KICKED_CLOSE_CODE => DisconnectReason::Kicked(reason),
                BANNED_CLOSE_CODE => DisconnectReason::Banned(reason),
                code => DisconnectReason::ServerClosed {
                    code: Some(code),
                    reason,
                },
            }
        }
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
        DisconnectReason::Kicked(_) | DisconnectReason::Banned(_)
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

    fn transfer(request_id: u64) -> Transfer {
        Transfer::Fetch {
            request_id,
            id: 100,
        }
    }

    fn request_ids(requests: &[Transfer]) -> Vec<u64> {
        requests.iter().map(Transfer::request_id).collect()
    }

    /// A `Command` or an `Event` carrying one reaches a log line whole.
    #[test]
    fn a_blob_prints_its_size_rather_than_its_bytes() {
        let printed = format!("{:?}", Blob::from(vec![1, 2, 3]));

        assert!(printed.contains("3 bytes"), "{printed}");
        assert!(!printed.contains('1'), "{printed}");
        assert!(!printed.contains('2'), "{printed}");
    }

    #[test]
    fn an_older_id_never_lowers_a_room_s_newest_one() {
        let mut newest = HashMap::new();

        note_newest(&mut newest, GENERAL_ROOM, 7);
        note_newest(&mut newest, GENERAL_ROOM, 12);
        note_newest(&mut newest, GENERAL_ROOM, 4);

        assert_eq!(newest.get(GENERAL_ROOM).copied(), Some(12));
    }

    #[test]
    fn each_room_keeps_its_own_newest_id() {
        let mut newest = HashMap::new();

        note_newest(&mut newest, GENERAL_ROOM, 9);
        note_newest(&mut newest, "dm-1-2", 3);

        assert_eq!(newest.get(GENERAL_ROOM).copied(), Some(9));
        assert_eq!(newest.get("dm-1-2").copied(), Some(3));
        assert_eq!(newest.get("music").copied(), None);
    }

    #[test]
    fn the_history_queue_serves_one_room_at_a_time_in_order() {
        let mut queue = HistoryQueue::default();
        assert!(queue.enqueue(GENERAL_ROOM.to_owned()));
        assert!(queue.enqueue("music".to_owned()));

        assert_eq!(queue.start().as_deref(), Some(GENERAL_ROOM));
        assert_eq!(queue.in_flight(), Some(GENERAL_ROOM));
        assert_eq!(queue.start(), None);

        queue.finish();
        assert_eq!(queue.start().as_deref(), Some("music"));

        queue.finish();
        assert_eq!(queue.start(), None);
        assert_eq!(queue.in_flight(), None);
    }

    #[test]
    fn the_history_queue_ignores_a_room_already_queued_or_in_flight() {
        let mut queue = HistoryQueue::default();
        assert!(queue.enqueue(GENERAL_ROOM.to_owned()));
        assert!(!queue.enqueue(GENERAL_ROOM.to_owned()));

        assert_eq!(queue.start().as_deref(), Some(GENERAL_ROOM));
        assert!(!queue.enqueue(GENERAL_ROOM.to_owned()));
        assert!(queue.enqueue("music".to_owned()));

        queue.finish();
        assert_eq!(queue.start().as_deref(), Some("music"));
        queue.finish();
        assert_eq!(queue.start(), None);
    }

    #[test]
    fn a_disconnect_abandons_the_room_in_flight_and_the_ones_waiting() {
        let mut queue = HistoryQueue::default();
        queue.enqueue(GENERAL_ROOM.to_owned());
        queue.enqueue("music".to_owned());
        queue.start();

        assert_eq!(
            queue.abandon(),
            vec![GENERAL_ROOM.to_owned(), "music".to_owned()]
        );
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

    #[test]
    fn a_failed_transfer_names_what_the_ui_asked_for() {
        let upload = Transfer::Upload {
            request_id: 4,
            room_id: GENERAL_ROOM.to_owned(),
            file_name: "shot.png".to_owned(),
            content_type: "image/png",
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
    }

    #[test]
    fn a_watch_state_of_zero_means_nobody() {
        assert_eq!(watch_target(0), None);
        assert_eq!(watch_target(7), Some(7));
    }

    /// A debug-level log file would otherwise persist every message anyone
    /// sends, which is the one thing this client never writes to disk.
    #[test]
    fn describing_a_message_frame_never_prints_what_was_typed() {
        let message = ChatMessage {
            id: 7,
            author: "alice".to_owned(),
            text: "secret-needle".to_owned(),
            room_id: GENERAL_ROOM.to_owned(),
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
            DisconnectReason::Kicked(detail) => assert_eq!(detail, "kicked by admin"),
            other => panic!("4001 must read as a kick, got {other}"),
        }

        match closed_reason(Some(&close_frame(4003, "account banned"))) {
            DisconnectReason::Banned(detail) => assert_eq!(detail, "account banned"),
            other => panic!("4003 must read as a ban, got {other}"),
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
            AfterAttempt::Stop(Some(DisconnectReason::Kicked(_)))
        ));

        let banned = after_close(Some(&close_frame(4003, "account banned")), Retry::Backoff);
        assert!(matches!(
            banned,
            AfterAttempt::Stop(Some(DisconnectReason::Banned(_)))
        ));

        let closed = after_close(Some(&close_frame(1008, "protocol error")), Retry::Backoff);
        assert!(matches!(closed, AfterAttempt::Reconnect { .. }));

        let dropped = after_close(None, Retry::BackoffAfterSession);
        assert!(matches!(dropped, AfterAttempt::Reconnect { .. }));
    }
}
