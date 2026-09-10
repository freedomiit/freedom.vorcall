//! The client connection loop from `PROTOCOL.md`:
//!
//! `Disconnected -> Connecting -> AwaitingWelcome -> Connected -> (close, error
//! or 75 s of silence) -> Backoff -> Connecting`.
//!
//! [`run`] owns the socket and never blocks its caller: it consumes [`Command`]s
//! from the UI and reports every transition as an [`Event`].

use std::fmt;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{Sink, SinkExt, Stream, StreamExt};
use prost::Message as _;
use tokio::time::{Instant, interval_at, sleep, sleep_until, timeout_at};
use tokio_tungstenite::tungstenite::{
    self, Utf8Bytes,
    client::IntoClientRequest,
    http::{HeaderValue, StatusCode},
    protocol::{CloseFrame, frame::coding::CloseCode},
};
use vorcall_proto::v1::{
    ChatMessage, ClientFrame, ErrorCode, Hello, Ping, SendMessage, ServerFrame, client_frame,
    server_frame,
};

use crate::endpoints::Endpoints;
use crate::history;

type WsMessage = tungstenite::Message;

const PROTOCOL_VERSION: u32 = 1;
/// The server answers `Hello` well inside its own 5 s deadline.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const SILENCE_TIMEOUT: Duration = Duration::from_secs(75);
/// A 401 means a stale build, not a blip: retry at the backoff cap.
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(30);
const HISTORY_LIMIT: u32 = 100;
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const BACKOFF_JITTER: f64 = 0.20;

#[derive(Debug, Clone)]
pub enum Command {
    Send(String),
}

#[derive(Debug, Clone)]
pub enum DisconnectReason {
    Unauthorized,
    InvalidNickname,
    ServerClosed { code: Option<u16>, reason: String },
    Io(String),
    Silence,
    ProtocolError(String),
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::InvalidNickname => f.write_str("invalid nickname"),
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
    },
    Disconnected {
        reason: DisconnectReason,
        /// The delay this loop is actually about to sleep; `None` when it stops.
        retry_in: Option<Duration>,
    },
    History(Vec<ChatMessage>),
    HistoryFailed(String),
    Message(ChatMessage),
    ServerError {
        code: i32,
        detail: String,
        fatal: bool,
    },
    SendDropped,
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

/// Drives connect/reconnect until the UI drops `commands` or the server tells
/// us the nickname is unusable. Never panics on a network result.
pub async fn run(
    endpoints: Endpoints,
    nickname: String,
    mut commands: mpsc::Receiver<Command>,
    mut events: mpsc::Sender<Event>,
) {
    let mut backoff = Backoff::default();

    loop {
        if events.send(Event::Connecting).await.is_err() {
            tracing::info!("UI is gone; stopping the connection loop");
            return;
        }
        tracing::info!(url = %endpoints.ws_url, nickname = %nickname, "connecting");

        if !drop_pending_commands(&mut commands, &mut events).await {
            tracing::info!("UI is gone; stopping the connection loop");
            return;
        }

        match attempt(&endpoints, &nickname, &mut commands, &mut events).await {
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

/// Reports one `Send` the loop cannot honour. `false` means the UI is gone.
async fn drop_send(text: &str, events: &mut mpsc::Sender<Event>) -> bool {
    tracing::info!(
        chars = text.chars().count(),
        "dropping a message queued while disconnected"
    );
    events.send(Event::SendDropped).await.is_ok()
}

/// Empties `commands` of sends that piled up while the loop was not live.
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
            Ok(Command::Send(text)) => {
                if !drop_send(&text, events).await {
                    return false;
                }
            }
            // The UI dropped the channel.
            Err(mpsc::TryRecvError::Closed) => return false,
            Err(mpsc::TryRecvError::Empty) => return true,
        }
    }
}

/// Sleeps out one backoff delay, answering sends as they arrive instead of
/// letting them sit in the channel until the next session. The deadline is
/// fixed, so a send does not restart the wait. `false` means the UI is gone.
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
                Some(Command::Send(text)) => {
                    if !drop_send(&text, events).await {
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
    nickname: &str,
    commands: &mut mpsc::Receiver<Command>,
    events: &mut mpsc::Sender<Event>,
) -> AfterAttempt {
    let mut request = match endpoints.ws_url.as_str().into_client_request() {
        Ok(request) => request,
        Err(e) => {
            return AfterAttempt::Reconnect {
                reason: DisconnectReason::Io(format!("cannot build the upgrade request: {e}")),
                after: Retry::Backoff,
            };
        }
    };
    match HeaderValue::from_str(&endpoints.key) {
        Ok(value) => {
            request.headers_mut().insert("x-vorcall-key", value);
        }
        Err(_) => {
            return AfterAttempt::Reconnect {
                reason: DisconnectReason::Unauthorized,
                after: Retry::Fixed(UNAUTHORIZED_RETRY),
            };
        }
    }

    let socket = match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _response)) => socket,
        Err(tungstenite::Error::Http(response))
            if response.status() == StatusCode::UNAUTHORIZED =>
        {
            tracing::warn!("the server rejected the pre-shared key (401)");
            return AfterAttempt::Reconnect {
                reason: DisconnectReason::Unauthorized,
                after: Retry::Fixed(UNAUTHORIZED_RETRY),
            };
        }
        Err(e) => {
            tracing::warn!(error = %e, "handshake failed");
            return AfterAttempt::Reconnect {
                reason: DisconnectReason::Io(e.to_string()),
                after: Retry::Backoff,
            };
        }
    };
    tracing::info!("websocket upgraded; sending Hello");

    let (mut sink, mut stream) = socket.split();

    let hello = ClientFrame {
        payload: Some(client_frame::Payload::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            nickname: nickname.to_owned(),
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
        "welcome received"
    );
    emit!(
        events,
        Event::Connected {
            latest_message_id: welcome.latest_message_id,
        }
    );

    live_loop(endpoints, &mut sink, &mut stream, commands, events).await
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
                tracing::debug!(frame = ?server_frame, "received");

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
                        return Err(error_outcome(&error));
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

async fn live_loop<Si, St>(
    endpoints: &Endpoints,
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
    let mut history = Box::pin(history::fetch_latest(endpoints, HISTORY_LIMIT));
    let mut history_pending = true;

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
                        tracing::debug!(frame = ?server_frame, "received");

                        match server_frame.payload {
                            Some(server_frame::Payload::Message(message)) => {
                                tracing::debug!(
                                    id = message.id,
                                    author = %message.author,
                                    "message"
                                );
                                emit_or_break!('live, events, Event::Message(message));
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
                                // A fatal error is followed by the server closing,
                                // except for a nickname we must never retry with.
                                if is_invalid_nickname(&error) {
                                    break error_outcome(&error);
                                }
                            }
                            // A Pong only had to reach the watchdog above.
                            Some(server_frame::Payload::Pong(_)) => {}
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

            result = &mut history, if history_pending => {
                history_pending = false;
                match result {
                    Ok(messages) => {
                        tracing::info!(count = messages.len(), "history loaded");
                        emit_or_break!('live, events, Event::History(messages));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "history fetch failed");
                        emit_or_break!('live, events, Event::HistoryFailed(e.to_string()));
                    }
                }
            }

            command = commands.next() => {
                match command {
                    Some(Command::Send(text)) => {
                        let frame = ClientFrame {
                            payload: Some(client_frame::Payload::Send(SendMessage { text })),
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

    close_gracefully(sink).await;
    outcome
}

fn is_invalid_nickname(error: &vorcall_proto::v1::Error) -> bool {
    error.fatal && ErrorCode::try_from(error.code) == Ok(ErrorCode::InvalidNickname)
}

/// A rejected nickname is the one error retrying cannot fix: the UI has to ask
/// for a new one, so the loop stops instead of hammering the server.
fn error_outcome(error: &vorcall_proto::v1::Error) -> AfterAttempt {
    if is_invalid_nickname(error) {
        return AfterAttempt::Stop(Some(DisconnectReason::InvalidNickname));
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
