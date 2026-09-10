# Vorcall wire protocol (v1)

Schema: `proto/vorcall.proto` (proto3). Both the .NET server and the Rust client generate code from it at build time. Never hand-edit generated code.

## Transport

Live traffic: one WebSocket at `/ws`. Every WebSocket message is a **binary** frame holding exactly one `ClientFrame` (client to server) or one `ServerFrame` (server to client). Text frames are a protocol error. Max inbound WebSocket message: 16 KiB; larger messages are a fatal protocol error.

REST: every request and response body is `application/x-protobuf`. Request bodies are capped at 16 KiB; a larger body gets `413`.

| Endpoint | Request | Success | Failure |
|---|---|---|---|
| `GET /api/messages?room=general&limit=100&before=<id>` | — | 200 `MessagePage` | 400 when `room` does not match the room id grammar |
| `GET /api/users` | — | 200 `UserList` | — |
| `POST /api/auth/register` | `RegisterRequest` | 201 `TokenResponse` | 400 / 403 / 409 / 429 `ApiError` |
| `POST /api/auth/login` | `LoginRequest` | 200 `TokenResponse` | 401 / 429 `ApiError` |
| `POST /api/auth/refresh` | `RefreshRequest` | 200 `TokenResponse` | 401 / 429 `ApiError` |
| `POST /api/auth/logout` | `LogoutRequest` | 204 | — |
| `POST /api/auth/password` | `ChangePasswordRequest` | 204 | 400 / 401 `ApiError` |
| `GET /health` | — | 200 `{"status":"ok"}` | 503 |

- `GET /api/messages`: `room` is optional and defaults to `general`; a value that does not match the room id grammar is `400`. `limit` is clamped to 1..100 (default 100). `before` is optional and exclusive: only messages with `id < before` are returned. Messages come back **ascending by id**; without `before` the page is the newest `limit` messages. `has_more` is true when older messages exist before `messages[0]`.
- `GET /api/users`: every registered user, ordered by username case-insensitively.
- `POST /api/auth/register`: `400` when the username, password or invite code fail the format rules; `403` when the invite is unknown, used or expired; `409` when the username is taken — and then the invite is **not** consumed; `429` when rate-limited.
- `POST /api/auth/login`: `401` is always `ApiError{"invalid username or password"}`, the same body whether the user exists or not. `429` carries `Retry-After: <seconds>` when the username is locked or the IP is rate-limited.
- `POST /api/auth/refresh`: `401` when the token is unknown, expired, revoked or already rotated (see Sessions); `429` when rate-limited.
- `POST /api/auth/logout`: always `204`, idempotent; it revokes the presented refresh token. No bearer needed.
- `POST /api/auth/password`: bearer required. `400` when the new password fails the policy; `401` when the current password is wrong or the bearer is invalid.
- `GET /health` needs neither the door key nor a bearer.

## Access

Two gates, both checked before any WebSocket upgrade:

1. **Door key** — the header `X-Vorcall-Key: <pre-shared key>` on every `/ws` and `/api/*` request. It is compared in constant time. A missing or wrong key gets an empty `401` with `WWW-Authenticate: X-Vorcall-Key`. It is a door key, not identity.
2. **Bearer access token** — `Authorization: Bearer <jwt>` on `/ws`, `/api/messages`, `/api/users` and `/api/auth/password`. Failure is `401` with `WWW-Authenticate: Bearer ...`.

`/api/auth/register`, `/api/auth/login`, `/api/auth/refresh` and `/api/auth/logout` need only the door key.

Clients tell the gates apart by the `WWW-Authenticate` scheme: `X-Vorcall-Key` means a stale build (wrong key); `Bearer` means refresh or sign in again; a 401 with no challenge is an application answer from the auth endpoints (`ApiError`, e.g. wrong password or refused refresh token).

## Identity and accounts

Accounts are invite-only.

- **Username** — trimmed, 1..32 Unicode scalars, no control characters, unique case-insensitively (compared after uppercasing with invariant culture). The display name is the username in its registered casing.
- **Password** — 8..128 Unicode scalars, no other rules.
- **Invite code** — 20 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, shown as four groups of five separated by `-`. Input is uppercased and stripped of `-` and whitespace. Single use; expires 7 days after creation by default; created only by the admin CLI on the server.

Registration signs the user in and returns tokens. `Hello.nickname` is deprecated and ignored.

## Sessions (tokens)

**Access token** — JWT HS256, 15 minutes (`expires_in = 900`), claims `sub` (user id), `name` (username), `iat`, `exp`, `jti`, issuer and audience `vorcall`. A WebSocket authenticated at upgrade time stays valid after its access token expires; only new requests need a fresh token.

**Refresh token** — opaque, 32 random bytes base64url; the server stores only its SHA-256. Each token belongs to a family created at login or registration. A refresh rotates the token (the presented one is marked rotated, a new one is issued in the same family) and slides the expiry to now + 30 days. Presenting a token that was already rotated or revoked revokes the whole family and answers `401` (reuse detection). Logout revokes the presented token's whole family (so a token the client had already rotated still ends the session). Changing the password revokes every refresh token of the user except the live token of the family the presented one belongs to. The admin CLI can revoke all tokens of a user.

**Rate limits** — `/api/auth/register|login|refresh` allow 10 requests per minute per client IP (sliding window; `429` with `Retry-After: 60`). Login locks a username after 5 consecutive failures for 60 s, doubling on each further lock up to 15 minutes, cleared by a successful login.

## Rooms and presence

Room ids match `^[a-z0-9-]{1,32}$`. The only room today is `general`, the text room. Every connection is a member of `general` from `Hello` on. A `Member` is `{user_id, username}`; ids are the database user ids and are stable across sessions.

One live connection per user. A `Hello` from a user who already has a live connection replaces it: the older connection receives fatal `ERROR_CODE_SESSION_REPLACED` then close 1008 "session replaced"; its room memberships transfer to the new connection silently (no `MemberLeft`/`MemberJoined` to others); the new connection receives `RoomState` for each room it is in.

Presence frames: `RoomState{room_id, members}` fully replaces what the client knows about that room; `MemberJoined{room_id, member}`; `MemberLeft{room_id, user_id}`.

- `JoinRoom{room_id}` — invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. Already a member: `RoomState` only (idempotent resync). Otherwise the caller is added, the other members receive `MemberJoined`, and the caller receives `RoomState`.
- `LeaveRoom{room_id}` — unknown room: `ERROR_CODE_UNKNOWN_ROOM`. Not a member: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Otherwise every current member **including the leaver** receives `MemberLeft`, then the leaver is removed. Leaving `general` is allowed; the client never does it today.
- `SendMessage{text, room_id}` — an empty `room_id` means `general`. Invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. Not a member: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Invalid text: non-fatal `ERROR_CODE_INVALID_MESSAGE`. Otherwise the message is persisted (with `room_id`, `author_id`, `author`) then broadcast as `ChatMessage` to every member of that room, **including the sender**. The sender renders only the echo.

When a connection ends, the remaining members of each room it was in receive `MemberLeft`.

Ordering guarantee: membership changes and room broadcasts are serialized by the server. A connection never sees a `ChatMessage`, `MemberJoined` or `MemberLeft` before its `Welcome` and initial `RoomState`.

## Server session state machine (per connection)

1. **AwaitingHello** — starts at upgrade, 5 s deadline. The bearer token of the upgrade request already identified the user. The first frame must be `Hello{protocol_version = 1}`; `nickname` is ignored. The server replies `Welcome{latest_message_id, member_id, username}` (`latest_message_id` is 0 when no message exists yet) immediately followed by `RoomState{"general", ...}` — and one `RoomState` per further room when this connection replaced an earlier one — then moves to Ready. Anything else (other frame, bad version, timeout) gets `Error{fatal = true}` with `ERROR_CODE_PROTOCOL`, then close code 1008.
2. **Ready** — handles `SendMessage`, `Ping`/`Pong`, `JoinRoom` and `LeaveRoom` as described under Rooms and presence. `Ping` gets `Pong` echoing `sent_at_unix_ms`. A second `Hello` is a fatal `ERROR_CODE_PROTOCOL`.
3. **Any state** — unparsable bytes or a text frame: fatal protocol error, close 1008. No frame received for 120 s: close 1001. A connection whose outbound queue exceeds 256 frames is closed with 1013. On server shutdown every socket is closed with 1001. If a connection's outbound queue is already full when a fatal error occurs, the `Error` frame may be dropped and only the close frame (1013 or 1008) is delivered: a slow consumer is closed as a slow consumer.

The server also sends WebSocket-level keep-alive pings every 30 s; clients answer with pong frames automatically.

## Ordering

`ChatMessage.id` is the only order. Live frames may arrive out of id order under concurrent senders, and a reconnect may deliver a message both live and in history. Clients keep messages in a map keyed by id and render in id order.

## Client connection loop

`Disconnected → Connecting → AwaitingWelcome → Connected → (close, error or 75 s of silence) → Backoff → Connecting`.

- Before every connection attempt the client refreshes the access token when fewer than 60 s remain; a REST call that answers 401 with a Bearer challenge is retried once after a refresh. On a `401` with `WWW-Authenticate: Bearer` it refreshes once and retries that request or upgrade. When a refresh itself answers `401` the loop stops for good and the UI returns to the sign-in screen. A bare `401` (door key) means a stale build: the client keeps retrying at the 30 s cap and shows "Unauthorized".
- After `Welcome` the client fetches the newest 100 messages of `general` and `GET /api/users`, then merges live frames.
- Gap-fill: the client remembers the newest message id it has delivered. If the newest page starts after that id + 1 and `has_more` is true, it pages backwards with `before` until the pages overlap that id, up to 5 pages in total (500 messages); beyond that a gap may remain. "Load older" uses `before = <oldest known id>`. The client keeps at most 2000 messages, dropping the oldest.
- `ERROR_CODE_SESSION_REPLACED` stops the loop for good; the UI returns to sign-in.
- The client sends `Ping` every 30 s while connected.
- Backoff: 1, 2, 4, 8, 16, 30 s (cap) with ±20 % jitter, reset after `Welcome`.

## Forward compatibility

A client that receives a `ServerFrame` whose payload it does not recognise — including an empty payload — logs it and ignores it. Only undecodable bytes are a protocol error. Servers may therefore add new `ServerFrame` payloads without a version bump. New `ClientFrame` payloads still require server support; an unknown client payload is a fatal `ERROR_CODE_PROTOCOL` as today.

## Limits (summary)

| Item | Limit |
|---|---|
| Username | 1..32 scalars after trim, no control chars |
| Password | 8..128 scalars |
| Invite code | 20 chars, single use, 7-day expiry |
| Room id | `^[a-z0-9-]{1,32}$` |
| Message text | 1..2000 scalars after trim |
| Inbound WebSocket message | 16 KiB |
| REST request body | 16 KiB |
| History page | 1..100, default 100 |
| Gap-fill | max 5 pages |
| Client message cap | 2000 messages |
| Access token | 15 min |
| Refresh token | 30 days sliding |
| Auth rate limit | 10 requests/min per IP |
| Login lockout | 5 failures → 60 s, doubling, cap 15 min |
| Hello deadline | 5 s |
| Server idle close | 120 s |
| Client silence watchdog | 75 s |
| Client ping interval | 30 s |
| Outbound queue per connection | 256 frames |
