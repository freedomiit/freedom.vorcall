# Vorcall wire protocol (v1)

Schema: `proto/vorcall.proto` (proto3). Both the .NET server and the Rust client generate code from it at build time. Never hand-edit generated code.

## Transport

- Live traffic: one WebSocket at `/ws`. Every WebSocket message is a **binary** frame holding exactly one `ClientFrame` (client to server) or one `ServerFrame` (server to client). Text frames are a protocol error.
- History: `GET /api/messages?limit=100&before=<id>` returning a `MessagePage` with `Content-Type: application/x-protobuf`. `limit` is clamped to 1..100 (default 100). `before` is optional and exclusive: only messages with `id < before` are returned. Messages come back **ascending by id**; without `before` the page is the newest `limit` messages. `has_more` is true when older messages exist before `messages[0]`.
- Health: `GET /health` returns 200 `{"status":"ok"}` when the database answers, 503 otherwise. No key required.
- Max inbound WebSocket message: 16 KiB. Larger messages are a fatal protocol error.

## Access

Every request to `/ws` and `/api/*` must carry the header `X-Vorcall-Key: <pre-shared key>`. A missing or wrong key gets an empty `401` before any upgrade. The key is compared in constant time. It is a door key, not identity.

## Identity

The client sends its nickname in `Hello`. The server trusts it as-is. Nicknames are trimmed, 1..32 Unicode scalars, no control characters.

## Server session state machine (per connection)

1. **AwaitingHello** — starts at upgrade, 5 s deadline. The first frame must be `Hello{protocol_version = 1, nickname valid}`. The server replies `Welcome{latest_message_id}` (0 when the room is empty) and moves to Ready. Anything else (other frame, bad version, bad nickname, timeout) gets `Error{fatal = true}` with `ERROR_CODE_PROTOCOL` or `ERROR_CODE_INVALID_NICKNAME`, then close code 1008.
2. **Ready** —
   - `SendMessage`: text is trimmed and must be 1..2000 Unicode scalars, otherwise `Error{ERROR_CODE_INVALID_MESSAGE, fatal = false}` and nothing is broadcast. Valid text is persisted first, then broadcast as `ChatMessage` to every Ready connection, **including the sender**. The sender renders only the echo.
   - `Ping` gets `Pong` echoing `sent_at_unix_ms`.
   - A second `Hello` is a fatal `ERROR_CODE_PROTOCOL`.
3. **Any state** — unparsable bytes or a text frame: fatal protocol error, close 1008. No frame received for 120 s: close 1001. A connection whose outbound queue exceeds 256 frames is closed with 1013. On server shutdown every socket is closed with 1001. If a connection's outbound queue is already full when a fatal error occurs, the `Error` frame may be dropped and only the close frame (1013 or 1008) is delivered: a slow consumer is closed as a slow consumer.

The server also sends WebSocket-level keep-alive pings every 30 s; clients answer with pong frames automatically.

## Ordering

`ChatMessage.id` is the only order. Live frames may arrive out of id order under concurrent senders, and a reconnect may deliver a message both live and in history. Clients keep messages in a map keyed by id and render in id order.

## Client connection loop

`Disconnected → Connecting → AwaitingWelcome → Connected → (close, error or 75 s of silence) → Backoff → Connecting`.

- On `Connected` the client fetches `GET /api/messages?limit=100` and merges the page by id, then keeps merging live frames.
- The client sends `Ping` every 30 s while connected.
- Backoff: 1, 2, 4, 8, 16, 30 s (cap) with ±20 % jitter, reset after `Welcome`.
- A `401` on the handshake means the client was built with a stale key. The client keeps retrying at the 30 s cap and shows "Unauthorized".

## Limits (summary)

| Item | Limit |
|---|---|
| Nickname | 1..32 scalars after trim, no control chars |
| Message text | 1..2000 scalars after trim |
| Inbound WebSocket message | 16 KiB |
| History page | 1..100, default 100 |
| Hello deadline | 5 s |
| Server idle close | 120 s |
| Client silence watchdog | 75 s |
| Client ping interval | 30 s |
| Outbound queue per connection | 256 frames |
