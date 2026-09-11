# Vorcall wire protocol (v1)

Schema: `proto/vorcall.proto` (proto3). Both the .NET server and the Rust client generate code from it at build time. Never hand-edit generated code.

## Transport

Live traffic: one WebSocket at `/ws`. Every WebSocket message is a **binary** frame holding exactly one `ClientFrame` (client to server) or one `ServerFrame` (server to client). Text frames are a protocol error. Max inbound WebSocket message: 16 KiB; larger messages are a fatal protocol error.

REST: every request and response body is `application/x-protobuf`. Request bodies are capped at 16 KiB; a larger body gets `413`. The one exception is the attachment upload body, which is raw image bytes and is capped at 8 MiB instead.

| Endpoint | Request | Success | Failure |
|---|---|---|---|
| `GET /api/messages?room=general&limit=100&before=<id>` | — | 200 `MessagePage` | 400 when `room` does not match the room id grammar; 403 `ApiError` when the bearer user is not a member of that room |
| `GET /api/users` | — | 200 `UserList` | — |
| `POST /api/attachments?room=<id>` | raw image bytes | 201 `Attachment` | 400 / 403 / 411 / 413 / 415 / 507 `ApiError` |
| `GET /api/attachments/{id}` | — | 200 the bytes | 403 / 404 `ApiError` |
| `POST /api/auth/register` | `RegisterRequest` | 201 `TokenResponse` | 400 / 403 / 409 / 429 `ApiError` |
| `POST /api/auth/login` | `LoginRequest` | 200 `TokenResponse` | 401 / 429 `ApiError` |
| `POST /api/auth/refresh` | `RefreshRequest` | 200 `TokenResponse` | 401 / 429 `ApiError` |
| `POST /api/auth/logout` | `LogoutRequest` | 204 | — |
| `POST /api/auth/password` | `ChangePasswordRequest` | 204 | 400 / 401 `ApiError` |
| `GET /health` | — | 200 `{"status":"ok"}` | 503 |

- `GET /api/messages`: `room` is optional and defaults to `general`; a value that does not match the room id grammar is `400`. `limit` is clamped to 1..100 (default 100). `before` is optional and exclusive: only messages with `id < before` are returned. Messages come back **ascending by id**; without `before` the page is the newest `limit` messages. `has_more` is true when older messages exist before `messages[0]`.
- `GET /api/users`: every registered user, ordered by username case-insensitively.
- `POST /api/attachments?room=<id>`: the body is the raw image, its `Content-Type` one of `image/png`, `image/jpeg`, `image/gif`, `image/webp`. `X-Vorcall-Filename` is optional. See Attachments for the failure rules. Rate limited to 20 uploads per minute per user.
- `GET /api/attachments/{id}`: the bytes with `Content-Length`, a strong `ETag` (`"<id>-<size>"`), `Cache-Control: private, max-age=31536000, immutable`, and Range supported. `404` when the id is unknown; `403` when the caller may not see it (see Attachments).
- `POST /api/auth/register`: `400` when the username, password or invite code fail the format rules; `403` when the invite is unknown, used or expired; `409` when the username is taken — and then the invite is **not** consumed; `429` when rate-limited.
- `POST /api/auth/login`: `401` is always `ApiError{"invalid username or password"}`, the same body whether the user exists or not. `429` carries `Retry-After: <seconds>` when the username is locked or the IP is rate-limited.
- `POST /api/auth/refresh`: `401` when the token is unknown, expired, revoked or already rotated (see Sessions); `429` when rate-limited.
- `POST /api/auth/logout`: always `204`, idempotent; it revokes the presented refresh token. No bearer needed.
- `POST /api/auth/password`: bearer required. `400` when the new password fails the policy; `401` when the current password is wrong or the bearer is invalid.
- `GET /health` needs neither the door key nor a bearer.

## Access

Two gates, both checked before any WebSocket upgrade:

1. **Door key** — the header `X-Vorcall-Key: <pre-shared key>` on every `/ws` and `/api/*` request. It is compared in constant time. A missing or wrong key gets an empty `401` with `WWW-Authenticate: X-Vorcall-Key`. It is a door key, not identity.
2. **Bearer access token** — `Authorization: Bearer <jwt>` on `/ws`, `/api/messages`, `/api/users`, `/api/attachments/*` and `/api/auth/password`. Failure is `401` with `WWW-Authenticate: Bearer ...`.

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

## Updates

Both endpoints need the door key and a valid bearer access token, same as `/api/messages`.

| Endpoint | Success | Failure |
|---|---|---|
| `GET /api/updates/manifest` | 200 `application/json`, header `X-Vorcall-Manifest-Signature`, `Cache-Control: no-store` | 404 when no manifest is published |
| `GET /api/updates/{version}/{file}` | 200 `application/octet-stream`, `Content-Length`, a strong `ETag` (`"<sha256>"`), Range supported | 404 unless `version` equals the published manifest's version and `file` is one of its `platforms.*.path` |

- The manifest body is `manifest.json` exactly as it sits on disk — never re-serialised — so the signature header covers precisely those bytes. It is an Ed25519 signature over the raw body, hex-encoded.
- `manifest.json` shape: `version`, `notes`, `published_at`, `min_version` (all strings; the three version fields are `MAJOR.MINOR.PATCH`), and `platforms`, a map of platform id (e.g. `linux-x86_64`) to `{path, sha256, size}` — `path` a bare filename served from `/api/updates/{version}/{path}`, `sha256` lowercase hex, `size` in bytes.
- The client verifies the manifest's signature against the keys baked in from `client/update-keys.pub` before parsing it at all, then verifies a downloaded asset's size and SHA-256 against the manifest before treating it as installable.
- Version comparison is a plain three-integer compare (`MAJOR.MINOR.PATCH`); anything else — a `v` prefix, a pre-release suffix, extra fields — is rejected rather than parsed loosely.

## Rooms and presence

Rooms are persistent rows (`rooms`, `room_members`); the connection registry mirrors them in memory at boot. There are two kinds:

- **Public** (`ROOM_KIND_PUBLIC`) — any account may see it in `RoomList` and join it. `general` is public and every account is a member of it from registration on; it cannot be left.
- **DM** (`ROOM_KIND_DM`) — room id `dm-<lower user id>-<higher user id>`, exactly two members, both permanent. Only its two members ever see it.

Room ids still match `^[a-z0-9-]{1,48}$`. A public room's id is the slug of its name. The name is trimmed, 1..32 Unicode scalars, no control characters. The slug is the name lowercased, with whitespace and `_` turned into `-`, everything outside `[a-z0-9-]` dropped, runs of `-` collapsed and leading/trailing `-` trimmed; it must be non-empty, at most 32 characters and must not start with `dm-`, otherwise `ERROR_CODE_INVALID_ROOM_NAME`. A slug already taken is `ERROR_CODE_ROOM_EXISTS`. The display name keeps the casing that was typed. A DM's `Room.name` is empty; clients show the other member's username.

Presence is still per live connection and unchanged in shape: `RoomState{room_id, members}` fully replaces what the client knows about who is **online** in that room, `MemberJoined{room_id, member}`, `MemberLeft{room_id, user_id}` — now for every room the account is a member of, not just `general`. `Room.member_ids` is the persistent membership, which is a different thing: a client derives "joined" as `member_ids` containing its own id. A `Member` is `{user_id, username}`; ids are the database user ids and are stable across sessions.

One live connection per user. A `Hello` from a user who already has a live connection replaces it: the older connection receives fatal `ERROR_CODE_SESSION_REPLACED` then close 1008 "session replaced"; its room memberships transfer to the new connection silently (no `MemberLeft`/`MemberJoined` to others).

**Hello sequence.** `Welcome`, then `RoomState`+`VoiceState` for `general`, then the same pair for every other room the account belongs to, in an order clients must not rely on, then `RoomList` — every public room plus the account's DMs, in an order clients must not rely on, each with `unread`, `mentions` and `last_message_id`; a public room the account has not joined reports 0 unread and 0 mentions. Then `MemberJoined` is broadcast to each of those rooms. On a session replacement no `MemberJoined` is broadcast: the sequence is otherwise identical, silent to everyone else.

- `CreateRoom{name}` — invalid name or unusable slug: non-fatal `ERROR_CODE_INVALID_ROOM_NAME`. Slug already taken: non-fatal `ERROR_CODE_ROOM_EXISTS`. Otherwise the room and the creator's membership are persisted, the creator receives `RoomState`+`VoiceState` for it, and every online connection receives `RoomUpdated`.
- `JoinRoom{room_id}` — invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. A DM the caller is not part of: non-fatal `ERROR_CODE_FORBIDDEN`. Already a member: `RoomState`+`VoiceState` only (idempotent resync). Otherwise a membership row is written with the read cursor at the room's newest message id, the room's online members receive `MemberJoined`, the joiner receives `RoomState`+`VoiceState`, and every online connection receives `RoomUpdated`.
- `LeaveRoom{room_id}` — invalid or unknown room: `ERROR_CODE_UNKNOWN_ROOM`. `general` or a DM: non-fatal `ERROR_CODE_FORBIDDEN` — neither can be left. Not a member: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Otherwise `VoiceMemberLeft` first if the leaver was in that room's voice channel, then every current member **including the leaver** receives `MemberLeft`, the membership row is removed, and every online connection receives `RoomUpdated`.
- `OpenDm{user_id}` — the caller's own id or an unknown user: non-fatal `ERROR_CODE_FORBIDDEN`. The DM already exists: `RoomState`+`VoiceState` resync to the caller only. Otherwise the room is created with both memberships (read cursor 0), each party that is online receives `RoomState`+`VoiceState`, and `RoomUpdated` goes to those two parties only — never to the whole server.
- `MarkRead{room_id, message_id}` — not a member: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Otherwise the cursor becomes `max(cursor, min(message_id, newest id in the room))`. There is no reply frame.
- `RoomUpdated{room}` replaces what the client knows about that room's shared facts (kind, name, member_ids, created_by). It never carries counters.

Counters in `RoomEntry` are per reader: `unread` counts the room's messages with `id > cursor` that are not tombstones and not written by the reader; `mentions` counts those among them whose `mention_ids` contain the reader.

When a connection ends, the remaining online members of each room it was in receive `MemberLeft`.

Ordering guarantee: membership changes and room broadcasts are serialized by the server. A connection never sees a `ChatMessage`, `MemberJoined`, `MemberLeft` or any other room frame before its `Welcome` and that room's initial `RoomState`.

## Messages

- `SendMessage{text, room_id, reply_to_id, attachment_ids}` — an empty `room_id` means `general`. Invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. Not a member: non-fatal `ERROR_CODE_NOT_A_MEMBER`. `text` is 1..2000 Unicode scalars after trimming, or empty only when `attachment_ids` is non-empty; anything else is non-fatal `ERROR_CODE_INVALID_MESSAGE`. A `reply_to_id` other than 0 must name a message of the same room — a tombstone is allowed — else `ERROR_CODE_UNKNOWN_MESSAGE`. At most 4 attachment ids, each of which must exist, have been uploaded by the sender, belong to this room and not be linked to a message yet, else `ERROR_CODE_INVALID_ATTACHMENT`. Mentions are the `<@id>` tokens of the text whose id names an existing user; they are stored as `mention_ids`, distinct, and an unknown id stays as plain text and is not stored. The message and the links to its attachments are persisted in one transaction, then broadcast as `ChatMessage` to every member of the room, **including the sender**. The sender renders only the echo.
- `ChatMessage` carries `edited_at_unix_ms` (0 when never edited), `deleted` (a tombstone: empty text, no attachments, no reactions — tombstones stay in history and in pages), `reply_to`, `mention_ids`, `reactions` (grouped by emoji, user ids ascending) and `attachments`. `reply_to` is a `ReplyRef` the server fills at read time from the target's **current** state: its id, author, the first 120 scalars of its text and whether it is a tombstone.
- `EditMessage{id, text}` — invalid text: non-fatal `ERROR_CODE_INVALID_MESSAGE`. Unknown message or a tombstone: non-fatal `ERROR_CODE_UNKNOWN_MESSAGE`. No longer a member of the message's room: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Not the author: non-fatal `ERROR_CODE_FORBIDDEN`. Otherwise the text and `edited_at_unix_ms` are set and `mention_ids` is recomputed, then `MessageEdited{message}` is broadcast to the room.
- `DeleteMessage{id}` — unknown message or a tombstone: `ERROR_CODE_UNKNOWN_MESSAGE`. No longer a member of the message's room: `ERROR_CODE_NOT_A_MEMBER` (even the author, once they have left the room). Not the author: `ERROR_CODE_FORBIDDEN`. Otherwise the message becomes a tombstone: text, mentions, reactions and attachments are cleared and the attachment files are removed. `MessageDeleted{room_id, id}` is broadcast to the room.
- `React{message_id, emoji, remove}` — `emoji` must be one of the eight the server accepts (👍 ❤️ 😂 😮 😢 🔥 🎉 👀), else non-fatal `ERROR_CODE_INVALID_REACTION`. Unknown message or a tombstone: `ERROR_CODE_UNKNOWN_MESSAGE`. Not a member of its room: `ERROR_CODE_NOT_A_MEMBER`. Otherwise the reader's reaction is added, or removed when `remove` is true; both are idempotent. `ReactionsChanged{room_id, message_id, reactions}` carrying the full grouped set is broadcast to the room.

Mentions on the wire are the token `<@user_id>`. Clients render it as the mentioned user's username and convert a typed `@username` into the token before sending; the server never parses usernames out of text.

Every error above is non-fatal. Every broadcast in this section is serialized under the same server lock as membership changes.

## Attachments

Both attachment endpoints need the door key and a bearer access token.

**Upload** — `POST /api/attachments?room=<id>`, the body being the raw image bytes:

- `room` missing or outside the room id grammar: `400`. The bearer user not a member of that room: `403`.
- `Content-Type` outside `image/png`, `image/jpeg`, `image/gif`, `image/webp`: `415`.
- No `Content-Length`: `411`. `Content-Length` above 8 MiB: `413`.
- Total stored attachment bytes plus the declared length above the quota `Vorcall:AttachmentsMaxBytes` (2 GiB by default): `507` `ApiError{"attachment storage is full"}`.
- Otherwise the body is streamed to `<Vorcall:AttachmentsDir>/<id>.<ext>`. The upload is aborted with `413` if the body exceeds the declared length while streaming, or `400` if its first bytes do not match the declared type's magic number, or if the body ends before reaching the declared length. Success is `201` `Attachment{id, file_name, content_type, size}`.
- `X-Vorcall-Filename` is optional: a bare file name, at most 128 characters after sanitising (path separators and control characters removed). The default is `image.<ext>`.
- Rate limit: 20 uploads per minute per user.

An upload is **unlinked** until a `SendMessage` names its id. Unlinked uploads older than 1 hour are swept every 10 minutes, file and row together.

**Download** — `GET /api/attachments/{id}`: unknown id `404`; unlinked and the caller is not the uploader `403`; linked and the caller is not a member of the message's room `403`. Otherwise the bytes with `Content-Length`, the strong `ETag` `"<id>-<size>"`, `Cache-Control: private, max-age=31536000, immutable`, and Range supported.

Deleting a message removes its attachments, rows and files alike.

## Voice

One voice channel per text room. Voice membership is separate from text membership: a user is in a room's voice channel only after `JoinVoice`, and signalling for it rides the same WebSocket. Media does not — see Media transport.

### Signalling

- `JoinVoice{room_id}` — requires membership of that **text** room. Invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. Not a member of the text room: non-fatal `ERROR_CODE_NOT_A_MEMBER`. Relay disabled: non-fatal `ERROR_CODE_VOICE_UNAVAILABLE`. Already in that voice channel: the existing media session ends first — every member of the text room, the caller included, receives `VoiceMemberLeft` — and a new one is created exactly like a fresh join, so a `VoiceReady` always carries a key and ssrc that were never used before. Otherwise the server creates a media session, sends the caller `VoiceReady` then `VoiceState`, and every other member of the **text room** receives `VoiceMemberJoined`.
- `LeaveVoice{room_id}` — invalid or unknown room: `ERROR_CODE_UNKNOWN_ROOM`. Not in that room's voice channel: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Otherwise every member of the text room **including the leaver** receives `VoiceMemberLeft`, and the media session is invalidated.

Audience: `VoiceState`, `VoiceMemberJoined`, `VoiceMemberLeft` and `Speaking` go to every member of the text room, whether or not they are in voice. `VoiceReady` goes only to the joiner — it carries that session's key and ssrc.

`VoiceMember` is `{user_id, username, ssrc, sharing, share_audio}`; `user_id` is the same stable id as in `Member`. The two share flags are described under Screen share.

`VoiceState` is also sent right after every `RoomState` — at `Hello`, after a session replacement and after `JoinRoom`, `CreateRoom` or `OpenDm` — possibly with zero members, so a client always learns the voice occupancy of a room it is in.

- `LeaveRoom` while in that room's voice channel: every member receives `VoiceMemberLeft` first, then the normal `MemberLeft`.
- When a connection ends, each room it had a voice session in receives `VoiceMemberLeft` before `MemberLeft`.
- Session replacement: the replaced connection's voice sessions end with `VoiceMemberLeft` to the room. Voice membership is **not** transferred — the media key and ssrc belong to the old session. The new connection must send `JoinVoice` again.

`Speaking{room_id, user_id, speaking}` is server-derived, never sent by clients. `true` on the first authenticated audio packet of a session that was not speaking; `false` once 250 ms pass without an audio packet, evaluated every 100 ms; `false` is also sent when a speaking session ends.

Ordering: voice membership changes and their broadcasts are serialized with text membership under the same server lock. A connection never sees a voice frame for a room before that room's `RoomState`.

### Screen share

Sharing rides the voice channel: a user may only share in a room it already has a voice session in, and the share ends with that session.

- `StartShare{room_id, audio}` — invalid or unknown room: non-fatal `ERROR_CODE_UNKNOWN_ROOM`. Not in that room's voice channel: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Sharing disabled on the server: non-fatal `ERROR_CODE_SHARE_UNAVAILABLE`. The room already at its sharer limit: non-fatal `ERROR_CODE_SHARE_LIMIT`. Otherwise the session is marked as sharing, `ShareStarted{room_id, user_id, audio}` is broadcast to every member of the **text room, the sharer included**, and the sharer receives `ShareWatchers{room_id, count}`. The frame is idempotent: repeating it — including with a different `audio` — re-sends both frames and does not count against the limit again.
- `StopShare{room_id}` — invalid or unknown room: `ERROR_CODE_UNKNOWN_ROOM`. Not in that room's voice channel: non-fatal `ERROR_CODE_NOT_IN_VOICE`. In voice but not sharing: non-fatal `ERROR_CODE_NOT_SHARING`. Otherwise `ShareStopped{room_id, user_id}` is broadcast to the same audience and every watcher of that share receives `WatchState{room_id, user_id = 0}`.
- `WatchShare{room_id, user_id}` — invalid or unknown room: `ERROR_CODE_UNKNOWN_ROOM`. The viewer not in that room's voice channel: `ERROR_CODE_NOT_IN_VOICE`. The named user not sharing in that room, or the viewer itself: `ERROR_CODE_NOT_SHARING`. Otherwise the watch replaces whatever the viewer was watching before — a viewer watches at most one share per voice session (the app holds one voice session, so one share at a time) — the viewer receives `WatchState{room_id, user_id}`, and both the new sharer and the one the viewer left receive a fresh `ShareWatchers`.
- `UnwatchShare{room_id}` — invalid or unknown room: `ERROR_CODE_UNKNOWN_ROOM`. Not in that room's voice channel: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Otherwise idempotent: the caller receives `WatchState{room_id, user_id = 0}` and the sharer it was watching, if any, a fresh `ShareWatchers`.

Audience: `ShareStarted` and `ShareStopped` go to every member of the text room, in voice or not. `WatchState` goes only to the viewer it describes; `ShareWatchers` only to the sharer, on every change to its watcher count.

`VoiceMember.sharing` and `VoiceMember.share_audio` carry the same facts in `VoiceState` and `VoiceMemberJoined`, so a client that joins a room late learns who is already sharing without waiting for a `ShareStarted`.

A share ends whenever the voice session behind it ends — `LeaveVoice`, `LeaveRoom`, the connection ending, or a session replacement. `ShareStopped` is then sent **before** `VoiceMemberLeft`, and the share's watchers receive `WatchState{user_id = 0}`. A viewer's watch ends the same way when the viewer's own voice session ends, silently: no frame is sent for it.

Share audio reaches that share's watchers only — it is never mixed into the room's voice audio, so a member who is in voice but not watching hears nothing of it.

The server keeps at most `Vorcall:MaxSharersPerRoom` (default 3) sharers per room, and `Vorcall:ShareEnabled` (default true) is the kill switch that makes every `StartShare` answer `ERROR_CODE_SHARE_UNAVAILABLE`.

Sharing and watching are client intent and survive a reconnect the way "in voice" does: after `Welcome` and the new `JoinVoice`/`VoiceReady`, a client that was sharing sends `StartShare` again, and a client that was watching sends `WatchShare` again — the latter only if the watched user is still listed as sharing in the new `VoiceState`.

### Media transport

Media takes a separate UDP path, IPv4, default port 5005 (`Vorcall:VoicePort`), advertised in `VoiceReady`. An empty `VoiceReady.host` means the host part of the WebSocket URL.

Every datagram is big-endian, with the header in clear and used as AEAD associated data, then the ciphertext, then a 16-byte tag:

```
offset 0   ver    u8   = 1
offset 1   type   u8   1 = audio, 2 = ping, 3 = pong, 4 = video, 5 = share audio, 6 = keyframe request
offset 2   flags  u8   bit 0 = marker (talk-spurt start for audio, first packet of a share-audio run); other bits 0
offset 3   ssrc   u32
offset 7   seq    u64
offset 15  ts     u32  sender's 48 kHz sample clock
offset 19  ciphertext, then tag (16 bytes)
```

`ts` is the sender's 48 kHz clock for every type; on a video packet it is the capture instant of the frame it belongs to.

Cipher: IETF ChaCha20-Poly1305 (RFC 8439). The key is the 32-byte session key from `VoiceReady`; the nonce is the 12 header bytes `ssrc || seq`. The header is authenticated, not encrypted.

- **Client to relay** — sealed with the client's own session key. `seq` starts at 0 and increases by one per packet of any type, share media included: one counter per session, never one per stream. Bit 63 of `seq` is never set by a client.
- **Relay to client, audio** — the relay opens the packet with the sender's key, re-seals the plaintext with the recipient's key under the unchanged header, so the recipient sees the original sender's `ssrc`, `seq` and `ts`, and forwards it to every other voice member of the room. No mixing, no transcoding.
- **Relay to client, pong** — the same header as the ping except `type = 3` and `seq = ping.seq | (1 << 63)`, sealed with that session's key. The bit keeps the pong nonce distinct from the ping nonce.
- **Relay to client, share media** — types 4 and 5 are accepted only from a session that is sharing (type 5 only when that share was started with `audio`), and are re-sealed per recipient under the unchanged header and forwarded **only to that sharer's watchers**, never to the rest of the room. They leave from a per-session sender worker rather than the receive path, which stays free for audio.
- **Relay to client, keyframe request** — type 6 is accepted only from a viewer that is watching the session named by `target_ssrc`, and is forwarded to that target re-sealed with the target's key, the payload unchanged.

Ping/pong payload: 8 bytes, an opaque client clock value echoed unchanged. Clients send a ping every 5 s from the moment they hold a `VoiceReady`, regardless of push-to-talk, to keep NAT and conntrack mappings alive and to measure the media path RTT. A client that receives no pong for 15 s reports the media link as down.

Audio payload: exactly one Opus packet of 20 ms at 48 kHz mono, CELT-only mode, 48 kbps CBR.

Video payload (type 4), one fragment of an encoded frame:

```
offset 0   frame_id  u32
offset 4   index     u16  fragment index within the frame
offset 6   count     u16  number of fragments the frame was split into
offset 8   flags     u8   bit 0 = keyframe (an IDR access unit carrying SPS/PPS); other bits 0
offset 9   data      up to 1156 bytes of the encoded frame
```

Share audio payload (type 5): exactly one Opus packet of 20 ms at 48 kHz **stereo**, CELT-only mode, 96 kbps CBR.

Keyframe request payload (type 6): 4 bytes, `target_ssrc u32` — the sharer the request is for. A viewer sends at most 2 per second; a sharer coalesces the requests it receives into at most one extra keyframe.

The relay processes each inbound datagram in this order: size (≤ 1200 bytes, ≥ 35 bytes) → header (`ver = 1`, `type ∈ {1, 2, 4, 5, 6}`) → session lookup by ssrc → per-session rate limit — share media (types 4 and 5) is charged to a byte budget (`Vorcall:ShareMaxKbps`, default 30000 kbit/s, burst the larger of 1.5 MiB and half a second of that rate) instead of the packet bucket, and types 1, 2 and 6 to the packet bucket (100 packets/s sustained, burst 200) → AEAD open → replay window (1024 sequence numbers; a seq already seen or older than the window is dropped) → the source address is learned from this packet, and re-learned whenever an authenticated packet arrives from a new address, which is how NAT rebinding is survived → dispatch. Every failure drops the datagram silently; the relay counts drops by reason — the share-media reasons being share-rate, not-sharing, not-watching and queue-full — and logs per-room counters every 30 s while the room has voice members.

`Speaking` is derived from type 1 alone: share media never marks a session as speaking. The relay's UDP socket buffers are 8 MiB in each direction, which the host sysctl in `deploy/provision-host.sh` has to allow.

The relay only ever sends to an address it learned this way: a member whose address is not known yet receives nothing. A media session ends when the voice membership ends; late packets for a removed ssrc are dropped as unknown.

### Client voice state machine

`Idle → Joining (JoinVoice sent) → Ready (VoiceReady received; UDP socket bound and pinging) → Media (first pong received) → Idle (LeaveVoice sent, or the WebSocket ended)`.

"In voice" is user intent and survives reconnects: after every `Welcome` a client that was in voice sends `JoinVoice` again and rebuilds its media path with the new key. The old socket and key are discarded on disconnect. A non-fatal `ERROR_CODE_NOT_A_MEMBER` or `ERROR_CODE_VOICE_UNAVAILABLE` in answer to `JoinVoice` clears the intent.

Within `Media` a client may also be **Sharing** — `StartShare` sent once its own capture is running, ended by `StopShare` or by the voice session ending — and **Watching{user}** — `WatchShare` sent, confirmed by `WatchState` naming that user. On confirmation the client sends one keyframe request, and one more after every detected loss, at most one per 500 ms; after a loss it discards non-keyframe video until the next keyframe arrives.

The receive side keeps one jitter buffer per ssrc: adaptive 60–100 ms, packet loss concealment for missing sequence numbers, late packets dropped. Push-to-talk gates sending only; pings continue while muted or deafened.

## Server session state machine (per connection)

1. **AwaitingHello** — starts at upgrade, 5 s deadline. The bearer token of the upgrade request already identified the user. The first frame must be `Hello{protocol_version = 1}`; `nickname` is ignored. `Hello` may also carry `client_version` (e.g. `"0.2.0"`) and `client_platform` (e.g. `"linux-x86_64"`), both optional: the server logs them and stores them on the account (`users.last_client_version`/`last_client_platform`/`last_seen_at`) for the admin CLI's `users list` and `users outdated`. Neither field is enforced — a client that omits them (any build before the updater) still connects normally. The server replies `Welcome{latest_message_id, member_id, username}` (`latest_message_id` is 0 when no message exists yet) immediately followed by the Hello sequence described under Rooms and presence — `RoomState`+`VoiceState` per room, then `RoomList` — then moves to Ready. Anything else (other frame, bad version, timeout) gets `Error{fatal = true}` with `ERROR_CODE_PROTOCOL`, then close code 1008.
2. **Ready** — handles `JoinRoom`, `LeaveRoom`, `CreateRoom`, `OpenDm` and `MarkRead` as described under Rooms and presence, `SendMessage`, `EditMessage`, `DeleteMessage` and `React` as described under Messages, `JoinVoice`/`LeaveVoice` as described under Voice, `StartShare`/`StopShare`/`WatchShare`/`UnwatchShare` as described under Screen share, and `Ping`/`Pong`. `Ping` gets `Pong` echoing `sent_at_unix_ms`. A second `Hello` is a fatal `ERROR_CODE_PROTOCOL`.
3. **Any state** — unparsable bytes or a text frame: fatal protocol error, close 1008. No frame received for 120 s: close 1001. A connection whose outbound queue exceeds 256 frames is closed with 1013. On server shutdown every socket is closed with 1001. If a connection's outbound queue is already full when a fatal error occurs, the `Error` frame may be dropped and only the close frame (1013 or 1008) is delivered: a slow consumer is closed as a slow consumer.

The server also sends WebSocket-level keep-alive pings every 30 s; clients answer with pong frames automatically.

## Ordering

`ChatMessage.id` is the only order. Live frames may arrive out of id order under concurrent senders, and a reconnect may deliver a message both live and in history. Clients keep messages in a map keyed by id and render in id order.

## Client connection loop

`Disconnected → Connecting → AwaitingWelcome → Connected → (close, error or 75 s of silence) → Backoff → Connecting`.

- Before every connection attempt the client refreshes the access token when fewer than 60 s remain; a REST call that answers 401 with a Bearer challenge is retried once after a refresh. On a `401` with `WWW-Authenticate: Bearer` it refreshes once and retries that request or upgrade. When a refresh itself answers `401` the loop stops for good and the UI returns to the sign-in screen. A bare `401` (door key) means a stale build: the client keeps retrying at the 30 s cap and shows "Unauthorized".
- After `Welcome` the client fetches `GET /api/users`, then merges live frames. History is fetched per room on demand (`GET /api/messages?room=…`, the newest 100 messages) when a room is first opened.
- Gap-fill: per room it has already loaded, the client remembers the newest message id it has delivered there. If the newest page starts after that id + 1 and `has_more` is true, it pages backwards with `before` until the pages overlap that id, up to 5 pages in total (500 messages); beyond that a gap may remain. "Load older" uses `before = <oldest known id>`. The client keeps at most 2000 messages per room, dropping the oldest.
- The client sends `MarkRead` for a room while it is viewing that room at the bottom of the list and the window is focused, debounced to at most one per second per room.
- `ERROR_CODE_SESSION_REPLACED` stops the loop for good; the UI returns to sign-in.
- The client sends `Ping` every 30 s while connected.
- Backoff: 1, 2, 4, 8, 16, 30 s (cap) with ±20 % jitter, reset after `Welcome`.

## Forward compatibility

A client that receives a `ServerFrame` whose payload it does not recognise — including an empty payload — logs it and ignores it. Only undecodable bytes are a protocol error. Servers may therefore add new `ServerFrame` payloads without a version bump. New `ClientFrame` payloads still require server support; an unknown client payload is a fatal `ERROR_CODE_PROTOCOL` as today. The voice frames were added under version 1: a client without voice support ignores them. So were the five server payloads `RoomList`, `RoomUpdated`, `MessageEdited`, `MessageDeleted` and `ReactionsChanged`, and the six client payloads `CreateRoom`, `OpenDm`, `MarkRead`, `EditMessage`, `DeleteMessage` and `React`. A client built before them sees only `general`, never edits, deletes or reacts, and renders `<@id>` tokens raw. The screen share arrived under version 1 as well: the four server payloads `ShareStarted`, `ShareStopped`, `WatchState` and `ShareWatchers`, the four client payloads `StartShare`, `StopShare`, `WatchShare` and `UnwatchShare`, and the two `VoiceMember` fields `sharing` and `share_audio`. A client built before them ignores the frames and reads the two fields as false, so it never sees or joins a share. The release's `min_version` is `0.4.0`, so such a client is asked to update by the manifest rather than refused by the protocol.

## Limits (summary)

| Item | Limit |
|---|---|
| Username | 1..32 scalars after trim, no control chars |
| Password | 8..128 scalars |
| Invite code | 20 chars, single use, 7-day expiry |
| Room id | `^[a-z0-9-]{1,48}$` |
| Room name | 1..32 scalars after trim |
| Room slug | at most 32 chars, non-empty, not `dm-*` |
| Message text | 1..2000 scalars after trim (empty only with attachments) |
| Reply excerpt | first 120 scalars |
| Reactions palette | 8 emoji |
| Attachments per message | 4 |
| Attachment size | 8 MiB |
| Attachment types | png, jpeg, gif, webp |
| Attachment file name | 128 chars after sanitising |
| Attachment storage quota | 2 GiB by default |
| Unlinked attachment sweep | older than 1 h, every 10 min |
| Attachment upload rate limit | 20 uploads/min per user |
| MarkRead debounce | 1 s per room |
| Inbound WebSocket message | 16 KiB |
| REST request body | 16 KiB |
| History page | 1..100, default 100 |
| Gap-fill | max 5 pages |
| Client message cap | 2000 messages per room |
| Access token | 15 min |
| Refresh token | 30 days sliding |
| Auth rate limit | 10 requests/min per IP |
| Login lockout | 5 failures → 60 s, doubling, cap 15 min |
| Hello deadline | 5 s |
| Server idle close | 120 s |
| Client silence watchdog | 75 s |
| Client ping interval | 30 s |
| Outbound queue per connection | 256 frames |
| Voice media port | 5005/udp, configurable |
| Media datagram | 35..1200 bytes |
| Media key | 32 bytes, per voice session |
| Replay window | 1024 sequence numbers |
| Media rate limit | 100 packets/s per session, burst 200 |
| Speaking hysteresis | 250 ms |
| Client media ping | every 5 s |
| Media link timeout | 15 s without pong |
| Jitter buffer | 60–100 ms adaptive |
| Opus frame | 20 ms, 48 kHz mono, 48 kbps CBR |
| Video fragment payload | 1156 bytes of frame data |
| Share audio frame | 20 ms, 48 kHz stereo, 96 kbps CBR |
| Share media budget | 30 Mbit/s per session, configurable |
| Sharers per room | 3, configurable |
| Keyframe requests | 2/s per viewer |
