//! The client connection loop from `PROTOCOL.md`:
//!
//! `Disconnected -> Connecting -> AwaitingWelcome -> Connected -> (close, error
//! or 75 s of silence) -> Backoff -> Connecting`.
//!
//! [`run`] owns the socket and the session: it keeps the access token fresh,
//! consumes [`Command`]s from the UI and reports every transition as an
//! [`Event`]. It never blocks its caller and never panics on a network result.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
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
    ChatMessage, ClientFrame, ErrorCode, Hello, JoinVoice, LeaveVoice, Member, MessagePage, Ping,
    SendMessage, ServerFrame, VoiceMember, client_frame, server_frame,
};

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
const HISTORY_LIMIT: u32 = 100;
/// `PROTOCOL.md` stops gap-filling here; beyond it a gap may remain.
const GAP_FILL_MAX_PAGES: usize = 5;
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const BACKOFF_JITTER: f64 = 0.20;

/// The only room today; every connection is a member of it from `Hello` on.
pub const GENERAL_ROOM: &str = "general";

#[derive(Debug, Clone)]
pub enum Command {
    Send { room_id: String, text: String },
    LoadOlder { room_id: String, before: i64 },
    JoinVoice { room_id: String },
    LeaveVoice { room_id: String },
}

/// The per-session media key from `VoiceReady`. Debug never prints it.
#[derive(Clone)]
pub struct MediaKey(pub [u8; 32]);

impl fmt::Debug for MediaKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaKey(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// The door key was refused: this build is stale.
    Unauthorized,
    /// The tokens are gone for good; the UI has to ask for a sign-in.
    AuthRequired(String),
    SessionReplaced,
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
    /// The newest page of `general` plus whatever gap-fill added, ascending.
    History {
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    HistoryFailed(String),
    OlderPage {
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    OlderFailed(String),
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
}

/// A log-safe rendering of a received frame: `VoiceReady` carries the media key,
/// which must never reach a log line.
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
        _ => format!("{frame:?}"),
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
    // Survives every attempt: it is what a reconnect gap-fills against.
    let mut newest_delivered: Option<i64> = None;

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
        Command::LoadOlder { room_id, before } => {
            tracing::debug!(%room_id, before, "cannot load older messages while disconnected");
            events
                .send(Event::OlderFailed("not connected".to_owned()))
                .await
                .is_ok()
        }
        // No event: the UI re-sends JoinVoice after every Connected.
        Command::JoinVoice { room_id } | Command::LeaveVoice { room_id } => {
            tracing::debug!(%room_id, "cannot change voice membership while disconnected");
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

async fn attempt(
    endpoints: &Endpoints,
    session: &mut Session,
    newest_delivered: &mut Option<i64>,
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
                    // A payload this build does not know: a newer server may add
                    // frames without a version bump.
                    None => {
                        tracing::warn!(frame = ?server_frame, "ignoring unknown server frame");
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
                return Err(AfterAttempt::Reconnect {
                    reason: closed_reason(frame.as_ref()),
                    after: Retry::Backoff,
                });
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

/// The newest page of `general`, plus the pages a reconnect needs to close the
/// gap between what the UI already has and what the server kept.
async fn initial_history(
    endpoints: &Endpoints,
    access_token: String,
    newest_delivered: Option<i64>,
) -> Result<(Vec<ChatMessage>, bool), ApiFailure> {
    let page =
        history::fetch_page(endpoints, &access_token, GENERAL_ROOM, HISTORY_LIMIT, None).await?;
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
            GENERAL_ROOM,
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

    tracing::info!(count = messages.len(), pages, has_more, "history fetched");
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

/// Remembers the newest id the UI has been handed, so the next attempt knows
/// where its gap starts.
fn note_newest(newest_delivered: &mut Option<i64>, id: i64) {
    *newest_delivered = Some(newest_delivered.map_or(id, |newest| newest.max(id)));
}

async fn live_loop<Si, St>(
    endpoints: &Endpoints,
    session: &mut Session,
    newest_delivered: &mut Option<i64>,
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
    let mut history_fetch = Some(Fetch::new(initial_history(
        endpoints,
        session.access_token.clone(),
        *newest_delivered,
    )));
    let mut users_fetch = Some(Fetch::new(user_list(
        endpoints,
        session.access_token.clone(),
    )));
    let mut older_fetch: Option<OlderFetch<'_>> = None;

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
                                note_newest(newest_delivered, message.id);
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
                            // A Pong only had to reach the watchdog above.
                            Some(server_frame::Payload::Pong(_)) => {}
                            // A payload this build does not know: a newer server
                            // may add frames without a version bump.
                            None => tracing::warn!(frame = ?server_frame, "ignoring unknown server frame"),
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
                    WsMessage::Close(frame) => break AfterAttempt::Reconnect {
                        reason: closed_reason(frame.as_ref()),
                        after: Retry::BackoffAfterSession,
                    },
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
                }
            }

            result = pending(history_fetch.as_mut()), if history_fetch.is_some() => {
                let newest = *newest_delivered;
                let reissue = |token| -> Boxed<'_, _> {
                    Box::pin(initial_history(endpoints, token, newest))
                };
                match settle(&mut history_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done((messages, has_more)) => {
                        if let Some(last) = messages.last() {
                            note_newest(newest_delivered, last.id);
                        }
                        emit_or_break!('live, events, Event::History { messages, has_more });
                    }
                    Settled::Retried => {}
                    Settled::Report(detail) => {
                        tracing::warn!(%detail, "history fetch failed");
                        emit_or_break!('live, events, Event::HistoryFailed(detail));
                    }
                    Settled::Stop(outcome) => break outcome,
                }
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
                let reissue = |token| -> Boxed<'_, _> {
                    Box::pin(older_page(endpoints, token, room_id, before))
                };
                match settle(&mut older_fetch, result, reissue, endpoints, session, events).await {
                    Settled::Done(page) => {
                        if let Some(last) = page.messages.last() {
                            note_newest(newest_delivered, last.id);
                        }
                        emit_or_break!('live, events, Event::OlderPage {
                            messages: page.messages,
                            has_more: page.has_more,
                        });
                    }
                    Settled::Retried => {}
                    Settled::Report(detail) => {
                        tracing::warn!(%detail, "older page fetch failed");
                        emit_or_break!('live, events, Event::OlderFailed(detail));
                    }
                    Settled::Stop(outcome) => break outcome,
                }
            }

            command = commands.next() => {
                match command {
                    Some(Command::Send { room_id, text }) => {
                        let frame = ClientFrame {
                            payload: Some(client_frame::Payload::Send(SendMessage { text, room_id })),
                        };
                        if let Err(e) = sink.send(WsMessage::binary(frame.encode_to_vec())).await {
                            tracing::warn!(error = %e, "cannot send the message");
                            emit_or_break!('live, events, Event::SendDropped);
                            break AfterAttempt::Reconnect {
                                reason: DisconnectReason::Io(e.to_string()),
                                after: Retry::BackoffAfterSession,
                            };
                        }
                    }
                    Some(Command::JoinVoice { room_id }) => {
                        let frame = ClientFrame {
                            payload: Some(client_frame::Payload::JoinVoice(JoinVoice { room_id })),
                        };
                        if let Err(e) = sink.send(WsMessage::binary(frame.encode_to_vec())).await {
                            tracing::warn!(error = %e, "cannot send JoinVoice");
                            break AfterAttempt::Reconnect {
                                reason: DisconnectReason::Io(e.to_string()),
                                after: Retry::BackoffAfterSession,
                            };
                        }
                    }
                    Some(Command::LeaveVoice { room_id }) => {
                        let frame = ClientFrame {
                            payload: Some(client_frame::Payload::LeaveVoice(LeaveVoice { room_id })),
                        };
                        if let Err(e) = sink.send(WsMessage::binary(frame.encode_to_vec())).await {
                            tracing::warn!(error = %e, "cannot send LeaveVoice");
                            break AfterAttempt::Reconnect {
                                reason: DisconnectReason::Io(e.to_string()),
                                after: Retry::BackoffAfterSession,
                            };
                        }
                    }
                    Some(Command::LoadOlder { room_id, before }) => {
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
                    None => {
                        tracing::info!("UI dropped the command channel; closing");
                        break AfterAttempt::Stop(None);
                    }
                }
            }

            _ = ping.tick() => {
                let frame = ClientFrame {
                    payload: Some(client_frame::Payload::Ping(Ping {
                        sent_at_unix_ms: unix_millis(),
                    })),
                };
                tracing::debug!("ping");
                if let Err(e) = sink.send(WsMessage::binary(frame.encode_to_vec())).await {
                    break AfterAttempt::Reconnect {
                        reason: DisconnectReason::Io(e.to_string()),
                        after: Retry::BackoffAfterSession,
                    };
                }
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

    // The UI keeps a "loading older" flag until this fetch answers, and the
    // socket is about to go: answer it here so the button comes back.
    if older_fetch.is_some() {
        let _ = events
            .send(Event::OlderFailed("disconnected".to_owned()))
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
        Some(frame) => DisconnectReason::ServerClosed {
            code: Some(frame.code.into()),
            reason: frame.reason.as_str().to_owned(),
        },
        None => DisconnectReason::ServerClosed {
            code: None,
            reason: String::new(),
        },
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
