# Vorcall wire protocol (v1)

Schema: `proto/vorcall.proto` (proto3). Both the .NET server and the Rust client generate code from it at build time. Never hand-edit generated code.

## Transport

Live traffic: one WebSocket at `/ws`. Every WebSocket message is a **binary** frame holding exactly one `ClientFrame` (client to server) or one `ServerFrame` (server to client). Text frames are a protocol error. Max inbound WebSocket message: 16 KiB; larger messages are a fatal protocol error.

REST: every request and response body is `application/x-protobuf`. Request bodies are capped at 16 KiB; a larger body gets `413`. The exceptions are the attachment and image upload bodies (raw image bytes, capped at 8 MiB), the diagnostics upload body (raw UTF-8 text, capped at 4 MiB), the Prometheus exposition `GET /metrics` answers with, and the admin endpoints, which speak JSON to the admin CLI only.

| Endpoint | Request | Success | Failure |
|---|---|---|---|
| `GET /api/messages?channel=<id>&limit=100&before=<id>` | — | 200 `MessagePage` | 400 when `channel` is missing or is not a positive integer; 403 `ApiError` when the bearer user may not view that channel |
| `GET /api/users` | — | 200 `MemberList` | — |
| `POST /api/attachments?channel=<id>` | raw file bytes | 201 `Attachment` | 400 / 403 / 411 / 413 / 507 `ApiError` |
| `GET /api/attachments/{id}` | — | 200 the bytes | 403 / 404 `ApiError` |
| `POST /api/streams?channel=<id>` | `StreamOffer` | 201 `StreamedFile` | 400 / 403 / 413 `ApiError` |
| `GET /api/streams/{id}` | — | 200 / 206 the bytes | 403 / 404 / 409 / 410 / 416 / 503 / 504 `ApiError` |
| `POST /api/streams/{id}/chunks?transfer=<id>` | raw bytes of the range | 204 | 400 / 404 / 409 / 410 `ApiError` |
| `POST /api/streams/{id}/decline?transfer=<id>` | — | 204 | 400 / 404 / 409 `ApiError` |
| `POST /api/images?purpose=avatar\|banner\|server_icon\|role_icon` | raw image bytes | 201 `Image` | 400 / 403 / 411 / 413 / 415 / 507 `ApiError` |
| `GET /api/images/{id}` | — | 200 the bytes | 404 `ApiError` |
| `GET /api/invites` | — | 200 `InviteList` | 403 `ApiError` |
| `POST /api/invites` | `CreateInviteRequest` | 201 `InviteCreated` | 400 / 403 `ApiError` |
| `DELETE /api/invites/{id}` | — | 204 | 403 / 404 `ApiError` |
| `GET /api/bans` | — | 200 `BanList` | 403 `ApiError` |
| `POST /api/auth/register` | `RegisterRequest` | 201 `TokenResponse` | 400 / 403 / 409 / 429 `ApiError` |
| `POST /api/auth/login` | `LoginRequest` | 200 `TokenResponse` | 401 / 403 / 429 `ApiError` |
| `POST /api/auth/refresh` | `RefreshRequest` | 200 `TokenResponse` | 401 / 403 / 429 `ApiError` |
| `POST /api/auth/logout` | `LogoutRequest` | 204 | — |
| `POST /api/auth/password` | `ChangePasswordRequest` | 204 | 400 / 401 `ApiError` |
| `POST /api/diagnostics?kind=log\|crash` | raw UTF-8 text | 204 | 400 / 413 / 429 `ApiError` |
| `GET /metrics` | — | 200 Prometheus text (`text/plain; version=0.0.4`) | 404 unless the source is private |
| `POST /api/admin/kick` | JSON `{"userId","code","reason"}` | 200 JSON `{"closed":true\|false}` | 400 JSON; 404 unless the source is private and the admin key matches |
| `POST /api/admin/refresh-account` | JSON `{"userId"}` | 204 | 400 JSON; 404 unless the source is private and the admin key matches |
| `GET /health` | — | 200 `{"status":"ok"}` | 503 |

- `GET /api/messages`: `channel` is required — there is no default channel — and must be a positive integer naming a text or DM channel the bearer user may view, else `400 ApiError{"channel"}` (malformed, or a voice channel) or `403 ApiError{"VIEW_CHANNEL"}` (unknown or not viewable). `limit` is clamped to 1..100 (default 100); a non-numeric `limit`, or a `before` below 1, is a bare `400` with no body. `before` is optional and exclusive: only messages with `id < before` are returned. Messages come back **ascending by id**; without `before` the page is the newest `limit` messages. `has_more` is true when older messages exist before `messages[0]`.
- `GET /api/users`: every member of the server as a `Profile`, ordered by username case-insensitively. Any bearer; the member list is not permission-filtered, channel visibility is.
- `POST /api/attachments?channel=<id>`: the body is the raw file, of any type. `Content-Type` and `X-Vorcall-Filename` are both optional. See Attachments for the failure rules. Rate limited to 20 uploads per minute per user.
- `GET /api/attachments/{id}`: the bytes with `Content-Length`, a strong `ETag` (`"<id>-<size>"`), `Cache-Control: private, max-age=31536000, immutable`, and Range supported. `404` when the id is unknown; `403` when the caller may not see it (see Attachments).
- `POST /api/images` and `GET /api/images/{id}`: see Profiles and images.
- The four `/api/streams` endpoints: see Streamed files. Only the offer is rate limited, and it draws on the upload bucket above.
- `GET /api/invites`, `POST /api/invites`, `DELETE /api/invites/{id}`, `GET /api/bans`: see Invites and bans.
- `POST /api/auth/register`: `400` when the username, password or invite code fail the format rules; `403` when the invite is unknown, used, revoked or expired; `409` when the username is taken — and then the invite is **not** consumed; `429` when rate-limited.
- `POST /api/auth/login`: `401` is always `ApiError{"invalid username or password"}`, the same body whether the user exists or not. `403` is `ApiError{"banned"}`. `429` carries `Retry-After: <seconds>` when the username is locked or the IP is rate-limited.
- `POST /api/auth/refresh`: `401` when the token is unknown, expired, revoked or already rotated (see Sessions); `403` `ApiError{"banned"}` when the account is banned; `429` when rate-limited.
- `POST /api/auth/logout`: always `204`, idempotent; it revokes the presented refresh token. No bearer needed.
- `POST /api/auth/password`: bearer required. `400` when the new password fails the policy; `401` when the current password is wrong or the bearer is invalid.
- `POST /api/diagnostics?kind=log|crash`: door key and bearer. The body is the raw text of one client log file or crash report, `text/plain` UTF-8, at most 4 MiB; `X-Vorcall-Filename` names it. `400` `ApiError{"not text"}`, `{"invalid file name"}` or `{"invalid kind"}`; `413` `{"reports must be 4 MiB or smaller"}`; `429` beyond 10 reports per hour per account (`Vorcall:DiagnosticsReportsPerHour`). Stored under `Vorcall:DiagnosticsDir` as `<userId>-<username>-<yyyyMMddTHHmmssZ>-<kind>-<name>` and deleted after 30 days. There is no listing endpoint: the owner reads the files on the host.
- `GET /metrics`: outside `/api`, so neither the door key nor a bearer gates it; what gates it is the source address — loopback, RFC 1918, link-local and their IPv6 counterparts (`PrivateSource.IsPrivate`) get the exposition, anything else gets the same `404` as an unknown path. nginx never proxies it; on the host it is `curl -s localhost:5004/metrics`.
- `POST /api/admin/kick` and `POST /api/admin/refresh-account`: the admin CLI's way to reach the live server for `users kick`/`users disable`/`users enable`. Door key plus `X-Vorcall-Admin-Key: <Vorcall:AdminKey>` compared in constant time; the source must also be private (`PrivateSource.IsPrivate`), otherwise — and whenever `Vorcall:AdminKey` is unset, in which case the endpoints are not mapped at all — the answer is `404`, indistinguishable from an unknown path. `kick` closes the user's live connection with `code` 4001 (kicked) or 4003 (account disabled; anything else is `400`) and reports whether one was open; `refresh-account` drops the cached lock state of that user so a `users disable` or a `users enable` applies to the next request rather than at the end of the cache's 30 s. nginx answers `404` for `/api/admin/` itself.
- `GET /health` needs neither the door key nor a bearer.

## Access

Two gates, both checked before any WebSocket upgrade:

1. **Door key** — the header `X-Vorcall-Key: <pre-shared key>` on every `/ws` and `/api/*` request. It is compared in constant time. A missing or wrong key gets an empty `401` with `WWW-Authenticate: X-Vorcall-Key`. It is a door key, not identity.
2. **Bearer access token** — `Authorization: Bearer <jwt>` on `/ws`, `/api/messages`, `/api/users`, `/api/attachments`, `/api/attachments/*`, `/api/streams`, `/api/streams/*`, `/api/images`, `/api/images/*`, `/api/invites`, `/api/invites/*`, `/api/bans`, `/api/diagnostics`, `/api/updates/*` and `/api/auth/password`. Failure is `401` with `WWW-Authenticate: Bearer ...`. A bearer whose account the admin CLI has disabled (`users disable`) fails validation and is refused with `401` within 30 s of the lock, the `/ws` upgrade included; a banned account is turned away at the upgrade with a bare `403` (see Moderation).

`/api/auth/register`, `/api/auth/login`, `/api/auth/refresh` and `/api/auth/logout` need only the door key. `/api/admin/*` needs the door key, the admin key and a private source (see the endpoint notes above); `/metrics` sits outside `/api` and is gated by source alone.

A request refused by the door key produces no request log line on the server; every other request produces exactly one.

Clients tell the gates apart by the `WWW-Authenticate` scheme: `X-Vorcall-Key` means a stale build (wrong key); `Bearer` means refresh or sign in again; a 401 with no challenge is an application answer from the auth endpoints (`ApiError`, e.g. wrong password or refused refresh token).

A banned account passes neither gate in practice: the `/ws` upgrade answers `403` and so do login and refresh (see Moderation).

Permissions are a third, per-frame and per-endpoint gate, described under Roles and permissions. A missing permission is never a `401`: on the WebSocket it is `ERROR_CODE_PERMISSION_DENIED`, on REST it is `403 ApiError{detail = <permission name>}`.

## Identity and accounts

Accounts are invite-only. Every account is a member of the one server from registration on; there is nothing to join.

- **Username** — trimmed, 1..32 Unicode scalars, no control characters, unique case-insensitively (compared after uppercasing with invariant culture). The display name is the username in its registered casing.
- **Nickname** — optional, same grammar as a username but not unique. When set it replaces the username everywhere the member is shown; empty means "show the username".
- **Password** — 8..128 Unicode scalars, no other rules.
- **Invite code** — 20 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, shown as four groups of five separated by `-`. Input is uppercased and stripped of `-` and whitespace. Single use. The admin CLI creates one with a 7-day expiry by default; `POST /api/invites` takes an explicit validity of 1..365 days. The server stores only the code's hash, so the plaintext is shown once, at creation.

Registration signs the user in and returns tokens. `Hello.nickname` is deprecated and ignored. A revoked invite — `invites revoke <id>` or the Invites page, the same operation either way — is refused like an unknown one (`403`).

**Disabled accounts** — an account lock the admin CLI writes, and a different thing from a ban: `users disable` sets `users.disabled_at`, revokes every refresh token of the account and closes its live connection with close code 4003 "account disabled". From then on login and refresh answer `403` `ApiError{"account disabled"}`, and a still-valid access token fails bearer validation with `401` within 30 s (`DisabledAccounts`, a 30 s cache the CLI's `refresh-account` call invalidates at once). `users enable` clears the flag; the sessions it revoked are not restored. A **ban** is moderation instead, issued from inside the app: a `bans` row, every message of the target tombstoned, the account out of the server — see Moderation.

## Sessions (tokens)

**Access token** — JWT HS256, 15 minutes (`expires_in = 900`), claims `sub` (user id), `name` (username), `iat`, `exp`, `jti`, issuer and audience `vorcall`. A WebSocket authenticated at upgrade time stays valid after its access token expires; only new requests need a fresh token.

**Refresh token** — opaque, 32 random bytes base64url; the server stores only its SHA-256. Each token belongs to a family created at login or registration. A refresh rotates the token (the presented one is marked rotated, a new one is issued in the same family) and slides the expiry to now + 30 days. Presenting a token that was already rotated or revoked revokes the whole family and answers `401` (reuse detection). Logout revokes the presented token's whole family (so a token the client had already rotated still ends the session). Changing the password revokes every refresh token of the user except the live token of the family the presented one belongs to. A kick or a ban revokes every refresh token of the account. The admin CLI can revoke all tokens of a user.

**Rate limits** — `/api/auth/register|login|refresh` allow 10 requests per minute per client IP (sliding window; `429` with `Retry-After: 60`; `Vorcall:AuthRequestsPerWindow`). Login locks a username after 5 consecutive failures for 60 s, doubling on each further lock up to 15 minutes, cleared by a successful login. Attachment uploads allow 20 per minute per account (`Vorcall:UploadRequestsPerWindow`) and diagnostics uploads 10 per hour per account (`Vorcall:DiagnosticsReportsPerHour`). All three windows are configuration, so a test host can raise them.

**Sweeps** — a refresh token row is deleted 7 days after it expired or was revoked, by a daily sweep (`RefreshTokenSweeper`); a revoked token is refused the moment it is revoked, the row only lingers for the reuse-detection window.

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

## Server, channels and categories

There is exactly one server. `Server{name, description, icon_image_id, owner_id, general_channel_id}` carries its shared facts; `owner_id` names the single account that bypasses every permission check, and `general_channel_id` the text channel that always exists.

**Channels** have numeric ids (`int64`, server-assigned) and one of three kinds:

- `CHANNEL_KIND_TEXT` — messages, history, read counters.
- `CHANNEL_KIND_VOICE` — a voice session and screen shares; no messages (`SendMessage` on one is `ERROR_CODE_INVALID_ARGUMENT`).
- `CHANNEL_KIND_DM` — exactly two permanent members, both of whom may always view it, send in it and connect to its voice session. `name` is empty; clients show the other member's display name. `dm_member_ids` holds both user ids.

**Categories** group the non-DM channels shown in the sidebar. A category has a `name` and a `position`; a channel has a `category_id` (`0` = no category, listed above the categories) and a `position` within it. Positions are dense integers the server rewrites on every reorder; clients sort by them and never invent their own.

There is no join and no leave. Every account is a member of the server, and a channel is **visible** to a member exactly when `VIEW_CHANNEL` resolves for them in it (see Roles and permissions). "Who is in this channel" is therefore "everyone who may view it". DM channels are visible to their two members only.

`general` is the text channel named by `general_channel_id`. It cannot be deleted (`ERROR_CODE_FORBIDDEN`) and its `VIEW_CHANNEL` cannot be denied to `@everyone`: that override is refused at write time with `ERROR_CODE_INVALID_ARGUMENT`, and resolution forces `VIEW_CHANNEL` on for every member anyway.

Names — channels, categories and roles alike — are trimmed, 1..32 Unicode scalars, no control characters, and need **not** be unique; anything else is `ERROR_CODE_INVALID_NAME`. Topics and descriptions are 0..256 scalars. At most 50 categories, 200 channels and 100 overrides per channel exist; a create that would exceed a cap is refused with `ERROR_CODE_INVALID_ARGUMENT` naming the collection (`"categories"`, `"channels"`, `"overrides"`). The channel cap counts the non-DM channels only — a DM is always openable — and the override cap is not charged when the frame merely rewrites or deletes an override that is already there.

Management frames, all requiring `MANAGE_CHANNELS` (resolved in the channel for an existing channel, at server level for a create or a category operation) and all answering with a broadcast rather than a reply:

- `CreateChannel{kind, name, topic, category_id}` — `kind` must be `TEXT` or `VOICE` (a DM is opened with `OpenDm`, never created here), `category_id` `0` or an existing category (`ERROR_CODE_UNKNOWN_CATEGORY`). The channel is appended to its category. Everyone who may view it receives `ChannelUpserted`.
- `UpdateChannel{id, name, topic}` — renames and re-topics. `ChannelUpserted` to its viewers.
- `DeleteChannel{id}` — `general` is refused with `ERROR_CODE_FORBIDDEN`. Otherwise its messages, attachments, read cursors and overrides go with it, any voice session in it ends (`VoiceMemberLeft`, then `VoiceMoved{0}` to each member that was in it), and everyone who could view it receives `ChannelDeleted`.
- `CreateCategory{name}`, `UpdateCategory{id, name}` — `CategoryUpserted` to everyone.
- `DeleteCategory{id}` — its channels move to "no category" with their positions appended there; `CategoryDeleted` plus a `ChannelOrder` to everyone.
- `ReorderChannels{positions}` — the full list of non-DM channels as `ChannelPosition{id, category_id, position}`; the server stores it verbatim after validating every id and category, then broadcasts `ChannelOrder` with the same list to everyone.
- `ReorderCategories{ids}` — the full list of category ids; `position` is the index. There is no category-order frame: every category whose position changed is broadcast to everyone as `CategoryUpserted`.
- `SetOverride{channel_id, override}` — upserts one `Override{role_id | user_id, allow, deny}` on that channel; `allow == 0 && deny == 0` deletes it. A DM channel has nothing an override could decide — its two members see it by membership — so one is refused with `ERROR_CODE_INVALID_ARGUMENT{"channel"}`, checked before `MANAGE_CHANNELS`. An unknown `role_id` is `ERROR_CODE_UNKNOWN_ROLE` and an unknown `user_id` `ERROR_CODE_UNKNOWN_USER`. The remaining rules are under Roles and permissions. `ChannelUpserted` (overrides included) to the channel's viewers.

`OpenDm{user_id}` needs no permission: the caller's own id, an unknown user or a banned user is non-fatal `ERROR_CODE_UNKNOWN_USER`; a DM that already exists answers `ChannelUpserted` + `VoiceState` to the caller only (idempotent resync); otherwise a DM channel is created with both members (read cursor 0) and each party that is online receives `ChannelUpserted` + `VoiceState` — never the whole server.

Visibility is maintained per member: when a role, override, channel or category change makes a channel visible to a member who could not see it, that member receives `ChannelUpserted`; when it stops being visible, that member alone receives `ChannelDeleted` and treats the channel as gone. Role, category and server deltas go to every online member regardless of channel visibility.

## Roles and permissions

A `Role` is `{id, name, color, icon_emoji, icon_image_id, position, permissions, hoist, everyone}`. `color` and `accent_color` are `0xRRGGBB` with `0` meaning "none". `position` orders the hierarchy: higher outranks lower. `hoist` asks clients to list the role's members as their own group. At most 100 roles exist, `@everyone` counted among them, so 99 are creatable; a create beyond that is `ERROR_CODE_INVALID_ARGUMENT{"roles"}`.

`@everyone` is the row with `everyone = true`. It sits at position `0`, every member holds it implicitly, it cannot be deleted, and only its `permissions` may be edited — a frame touching its name, colour, icon, hoist or position is `ERROR_CODE_INVALID_ARGUMENT`. Its default permissions are `VIEW_CHANNEL | SEND_MESSAGES | ATTACH_FILES | ADD_REACTIONS | CONNECT | SPEAK | SHARE_SCREEN | CHANGE_NICKNAME`.

A member holds any number of further roles (`Profile.role_ids`). The name is painted by the highest-positioned role of that member whose `color` is non-zero; if none has one, the client's default text colour applies.

`Permission` is a bit set (`uint64` on the wire, 21 bits defined). Bits split into two scopes:

- **server-scoped** — `MANAGE_SERVER`, `MANAGE_ROLES`, `MANAGE_MEMBERS`, `MANAGE_INVITES`, `KICK_MEMBERS`, `BAN_MEMBERS`, `CHANGE_NICKNAME`. They are held server-wide and **never** appear in an override; an override carrying one has that bit masked away.
- **channel-scoped** — every other bit: `MANAGE_CHANNELS`, `MANAGE_MESSAGES`, `VIEW_CHANNEL`, `SEND_MESSAGES`, `ATTACH_FILES`, `ADD_REACTIONS`, `MENTION_EVERYONE`, `CONNECT`, `SPEAK`, `SHARE_SCREEN`, `MUTE_MEMBERS`, `DEAFEN_MEMBERS`, `MOVE_MEMBERS`, `PRIORITY_SPEAKER`.

### Resolution

Resolving a member's permissions, with or without a channel, is exactly this — both sides implement it (`server/Permissions/`, `vorcall-core/src/permissions.rs`) and both are tested against the same matrix:

1. The member is the owner (`Server.owner_id`) → every bit, whatever the overrides say.
2. `base = @everyone.permissions | OR(permissions of every role the member holds)`.
3. No channel (a server-level question) → return `base`.
4. Otherwise `p = base & CHANNEL_SCOPED`; the server-scoped half of `base` is kept aside.
5. Apply the `@everyone` override of that channel, if any: `p = (p & ~deny) | allow`.
6. Apply the overrides of the member's other roles **in ascending position order, one after another**, each as `p = (p & ~deny) | allow`. A higher role's override therefore wins a conflict with a lower one; this is deliberately sequential, not a union of allows and denies.
7. Apply the member's own override, if any, the same way. It wins over every role.
8. If the channel is `general`, force `VIEW_CHANNEL` on.
9. If `VIEW_CHANNEL` is off at this point, drop every channel-scoped bit: the result is the server-scoped half of `base` alone.
10. Return `p | (base & SERVER_SCOPED)`. Server-scoped bits are never removed by a channel.

An override is `{exactly one of role_id / user_id, allow, deny}` with `allow & deny == 0` (otherwise `ERROR_CODE_INVALID_ARGUMENT`) and only channel-scoped bits.

### Hierarchy

`highest_position(member)` is the greatest `position` among the member's roles, and `0` when the member holds `@everyone` only. From it:

- **Managing a role** (create, edit, delete, assign, reorder): the caller must hold `MANAGE_ROLES` and the role's position must be strictly below `highest_position(caller)`. The owner may manage every role. `@everyone`'s permissions are the exception: any `MANAGE_ROLES` holder may edit them. A refusal is `ERROR_CODE_HIERARCHY`.
- **Granting bits**: a caller may put into a role or an override only bits it holds itself at server level. The owner is exempt. A refusal is `ERROR_CODE_PERMISSION_DENIED` naming the bit the caller does not hold. What exactly has to be held differs per frame: `CreateRole` needs every bit in `permissions`; `UpdateRole` needs only the bits being **added** (taking one away needs nothing); `SetOverride` needs every bit in `allow | deny`, since a deny is as much a decision about a bit as an allow.
- **Targeting a member** (kick, ban, nickname, server mute, server deafen, move, role change): the target must be strictly below the caller (`highest_position(caller) > highest_position(target)`), never the owner, and never the caller itself. The owner may target everyone but itself. Targeting the owner or yourself is `ERROR_CODE_FORBIDDEN`; being outranked is `ERROR_CODE_HIERARCHY`. Two exceptions let a member act on itself: its own nickname (with `CHANGE_NICKNAME`) and assigning itself a role it may manage.

Role frames: `CreateRole{name, color, icon_emoji, icon_image_id, permissions, hoist}` inserts the role below the caller's highest position and broadcasts `RoleUpserted`; `UpdateRole{role}` replaces it (its `position` is ignored — reordering is a separate frame) and broadcasts `RoleUpserted`; `DeleteRole{id}` broadcasts `RoleDeleted` and drops the role from every member and every override; `ReorderRoles{ids}` takes the full list excluding `@everyone`, **bottom first**, so `position = index + 1`, and broadcasts `RoleOrder` with the same list; `SetMemberRoles{user_id, role_ids}` replaces a member's roles and broadcasts `MemberUpdated`. Every role frame goes to every online member. `icon_emoji` is at most 2 scalars, and a role shows at most one icon — an emoji or an image.

`RoleOrder` is not only a reorder frame: a `CreateRole` or `DeleteRole` that renumbered the other roles broadcasts it right after the `RoleUpserted`/`RoleDeleted`, so a client never has to guess the new positions. `ReorderRoles` must carry the **complete** list of non-`@everyone` roles, each exactly once (anything else is `ERROR_CODE_INVALID_ARGUMENT{"ids"}`, an unknown id `ERROR_CODE_UNKNOWN_ROLE`), and every role whose position actually changes must be strictly below the caller's highest position both before and after the move, else `ERROR_CODE_HIERARCHY`. The owner moves anything.

Every permission failure is non-fatal `ERROR_CODE_PERMISSION_DENIED` whose `detail` is the missing bit's name without the `PERMISSION_` prefix (`SEND_MESSAGES`, `MANAGE_CHANNELS`, …). Write-time refusals that are not about a missing bit are `ERROR_CODE_INVALID_ARGUMENT` with `detail` naming the field or collection at fault — a `VIEW_CHANNEL` deny for `@everyone` on `general` (`"deny"`), a name/colour/icon/hoist change on `@everyone` (`"role"`), `allow & deny != 0` (`"override"`), a role carrying both an emoji and an image icon (`"icon"`), an unknown `CreateChannel.kind` (`"kind"`), an over-long `topic`/`description`/ban `reason`, an `icon_emoji` past 2 scalars, a missing nested `override`/`role` message, a cap exceeded. The fatal codes `KICKED`, `BANNED` and `SESSION_REPLACED` carry no `detail` (the one exception being the session replacement the handshake itself performs).

Any change to roles, overrides, channels or memberships is re-resolved immediately: channel visibility deltas go out as described above, a member that loses `CONNECT` or `VIEW_CHANNEL` in a channel it is in voice in is disconnected from it, and `VoiceMember.priority` is recomputed and the channel's `VoiceState` re-sent.

## Hello sequence

One live connection per user. A `Hello` from a user who already has a live connection replaces it: the older connection receives fatal `ERROR_CODE_SESSION_REPLACED` then close 1008 "session replaced". The account never stopped being online, so no presence frame goes out.

After `Welcome{latest_message_id, member_id, username}` the server sends, in this order:

1. `ServerSnapshot` — `server`; **every** role; **every** category; every non-DM channel the reader may view, each with its `overrides`, plus the reader's own DM channels; every member of the server as a `Profile` with `online` set from presence; and `read_states` for every viewable text channel and DM.
2. One `VoiceState` per voice or DM channel that has at least one voice member. Channels with nobody in voice are not announced.
3. `MemberUpdated{member.online = true}` to every **other** online member. A session replacement skips this step; the rest of the sequence is identical.

The snapshot is the whole world as that member may see it — a client needs no REST call to fill it in. Everything after it is a delta:

| Change | Frame | Audience |
|---|---|---|
| server facts | `ServerUpdated` | everyone |
| role created/edited, order | `RoleUpserted`, `RoleDeleted`, `RoleOrder` | everyone |
| category | `CategoryUpserted`, `CategoryDeleted` | everyone |
| channel created/edited/overrides | `ChannelUpserted` | the channel's viewers |
| channel deleted, or no longer viewable | `ChannelDeleted` | its viewers / that one member |
| channel order | `ChannelOrder` | everyone |
| roles, nickname, profile, presence, server mute flags | `MemberUpdated` | everyone |
| banned | `MemberRemoved` | everyone |

When a connection ends, every other online member receives `MemberUpdated` with `online = false` for it, after the `VoiceMemberLeft` of any voice session it held.

## Messages

- `SendMessage{channel_id, text, reply_to_id, attachment_ids, streamed_file_ids}` — `channel_id` is required; `0`, unknown, or a channel the sender may not view is non-fatal `ERROR_CODE_UNKNOWN_CHANNEL` (a hidden channel is indistinguishable from a missing one on purpose). A voice channel is `ERROR_CODE_INVALID_ARGUMENT`. Without `SEND_MESSAGES` in that channel: `ERROR_CODE_PERMISSION_DENIED{"SEND_MESSAGES"}`; with attachment ids or streamed file ids but without `ATTACH_FILES`: `…{"ATTACH_FILES"}` — one bit covers both kinds. `text` is 1..2000 Unicode scalars after trimming, or empty only when `attachment_ids` or `streamed_file_ids` is non-empty; anything else is non-fatal `ERROR_CODE_INVALID_MESSAGE`. A `reply_to_id` other than 0 must name a message of the same channel — a tombstone is allowed — else `ERROR_CODE_UNKNOWN_MESSAGE`. At most 4 attachment ids, each of which must exist, have been uploaded by the sender, belong to this channel, not be linked to a message yet, and not repeat within the frame, else `ERROR_CODE_INVALID_ATTACHMENT`. `streamed_file_ids` follows the same rule word for word — at most 4, offered by the sender for this channel, unlinked, distinct — and fails with non-fatal `ERROR_CODE_INVALID_STREAM` (28), detail `"streamed file is unknown, not yours, not in this channel or already used"`. The two lists are counted separately, so a message may carry four of each. The text check runs **before** the two permission checks, so a malformed text in a channel the sender may not write in answers `ERROR_CODE_INVALID_MESSAGE`, not `ERROR_CODE_PERMISSION_DENIED`. The message and the links to its attachments and streamed files are persisted in one transaction, then broadcast as `ChatMessage` to every member who may view the channel, **including the sender**. The sender renders only the echo.
- Mentions. The `<@id>` tokens of the text whose id names an existing user become `mention_ids`, distinct; an unknown id stays plain text and is not stored. The literal words `@everyone` and `@here` set `mention_everyone` / `mention_here` — but only when the sender holds `MENTION_EVERYONE` in that channel; without the bit they stay plain text and both flags are false (the message is **not** refused). `@here` asks clients to notify the members who are online; `@everyone` all of them.
- `ChatMessage` carries `channel_id`, `edited_at_unix_ms` (0 when never edited), `deleted` (a tombstone: empty text, no attachments, no streamed files, no reactions — tombstones stay in history and in pages), `reply_to`, `mention_ids`, the two mention flags, `reactions` (grouped by emoji, user ids ascending), `attachments` and `streamed_files` (`StreamedFile{id, file_name, content_type, size, owner_id}`, each readable only while `owner_id` is online — see Streamed files). `reply_to` is a `ReplyRef` the server fills at read time from the target's **current** state: its id, author, the first 120 scalars of its text and whether it is a tombstone.
- `EditMessage{id, text}` — invalid text: non-fatal `ERROR_CODE_INVALID_MESSAGE`. Unknown message or a tombstone: non-fatal `ERROR_CODE_UNKNOWN_MESSAGE`. A channel the caller may no longer view: `ERROR_CODE_UNKNOWN_CHANNEL`. Not the author: non-fatal `ERROR_CODE_FORBIDDEN` — `MANAGE_MESSAGES` does not grant editing someone else's text. Otherwise the text and `edited_at_unix_ms` are set, `mention_ids` and the mention flags are recomputed, and `MessageEdited{message}` is broadcast to the channel's viewers.
- `DeleteMessage{id}` — unknown message or a tombstone: `ERROR_CODE_UNKNOWN_MESSAGE`. A channel the caller may no longer view: `ERROR_CODE_UNKNOWN_CHANNEL`. Neither the author nor a holder of `MANAGE_MESSAGES` in that channel: `ERROR_CODE_PERMISSION_DENIED{"MANAGE_MESSAGES"}`. Otherwise the message becomes a tombstone: text, mentions, reactions, attachments and streamed files are cleared, and the attachment files are removed — a streamed file has none here to remove. `MessageDeleted{channel_id, id}` is broadcast to the channel's viewers.
- `React{message_id, emoji, remove}` — `emoji` must be one of the eight the server accepts (👍 ❤️ 😂 😮 😢 🔥 🎉 👀), else non-fatal `ERROR_CODE_INVALID_REACTION`. An unknown message: `ERROR_CODE_UNKNOWN_MESSAGE`. A channel the caller may not view: `ERROR_CODE_UNKNOWN_CHANNEL`. Without `ADD_REACTIONS`: `ERROR_CODE_PERMISSION_DENIED{"ADD_REACTIONS"}` — removing one needs the bit too, and this check comes before the tombstone verdict, so reacting to a tombstone without the bit answers the permission error rather than `ERROR_CODE_UNKNOWN_MESSAGE`. A tombstone with the bit held: `ERROR_CODE_UNKNOWN_MESSAGE`. Otherwise the reader's reaction is added, or removed when `remove` is true; both are idempotent. `ReactionsChanged{channel_id, message_id, reactions}` carrying the full grouped set is broadcast to the channel's viewers.
- `MarkRead{channel_id, message_id}` — a channel the caller may not view: `ERROR_CODE_UNKNOWN_CHANNEL`; a voice channel, which holds no messages and therefore no cursor: `ERROR_CODE_INVALID_ARGUMENT{"channel"}`. Otherwise the cursor becomes `max(cursor, min(message_id, newest id in the channel))`. There is no reply frame.

Counters live in `ReadState{channel_id, unread, mentions, last_message_id}`, one per viewable text channel and DM, and are per reader: `unread` counts the channel's messages with `id > cursor` that are not tombstones and not written by the reader; `mentions` counts those among them whose `mention_ids` contain the reader **or** whose `mention_everyone` is set. `@here` is live-only and is never counted.

Mentions on the wire are the token `<@user_id>`. Clients render it as the mentioned user's display name and convert a typed `@username` into the token before sending; the server never parses usernames out of text. `@everyone` and `@here` are plain words, not tokens.

Every error above is non-fatal. Every broadcast in this section is serialized under the same server lock as every other mutation.

**Write rate limit** — the five frames any member may send without holding a permission draw from one token bucket per account: 20 tokens, refilling 2 per second (`Vorcall:MessageBurst`, `Vorcall:MessagesPerSecond`). Charged are `SendMessage`, `EditMessage`, `DeleteMessage`, `React` and `OpenDm`. A charged frame that finds the bucket empty is not handled at all and answers non-fatal `ERROR_CODE_RATE_LIMITED` (27) with detail `"too many messages, slow down"`; the client shows it as a notice. Reads, `MarkRead`, presence, voice and share signalling, pings, and every management and moderation frame (`CreateChannel`, `UpdateChannel`, `DeleteChannel`, `CreateCategory`, `UpdateCategory`, `DeleteCategory`, `ReorderChannels`, `ReorderCategories`, `SetOverride`, `CreateRole`, `UpdateRole`, `DeleteRole`, `ReorderRoles`, `SetMemberRoles`, `SetNickname`, `UpdateProfile`, `KickMember`, `BanMember`, `UnbanMember`, `VoiceModerate`, `UpdateServer` and `TransferOwnership`) are never charged — the last group because each sits behind a permission such as `MANAGE_CHANNELS` or `MANAGE_ROLES` that only a trusted member holds, so a throttled account can still navigate and a settings page can save a burst of changes.

## Attachments

Both attachment endpoints need the door key and a bearer access token.

An attachment is **any file of any type**. Nothing on this path sniffs, decodes or renders one: the declared type is metadata, stored, echoed back to clients and handed to the download exactly as it arrived. Pictures used as an avatar, a banner, the server icon or a role icon are a different store under much narrower rules — see Profiles and images.

**Upload** — `POST /api/attachments?channel=<id>`, the body being the raw file bytes:

- `channel` missing or not a positive integer: `400 ApiError{"channel"}`; a voice channel, which holds no messages to attach to: `400 ApiError{"channel"}` as well. A channel the bearer user may not view: `403 ApiError{"VIEW_CHANNEL"}`. One it may view without holding `ATTACH_FILES`: `403 ApiError{"ATTACH_FILES"}`.
- `Content-Type` is optional. When present it must be an RFC 9110 § 5.6.2 `type/subtype` and nothing more — both halves non-empty tokens, no parameters, at most 128 characters — else `400 ApiError{"malformed content type"}`; a parameter such as a `charset` is ignored, and the media type is stored lower-cased. Absent or blank, it is stored as `application/octet-stream`. There is no `415`: the only thing an attachment's type can be wrong about is its own shape.
- No `Content-Length`: `411 ApiError{"length required"}`. `Content-Length` above 2 GiB: `413 ApiError{"files must be 2 GiB or smaller"}` — a larger file cannot be an attachment at all and is offered as a streamed file instead.
- Total stored attachment bytes plus the declared length above the quota `Vorcall:AttachmentsMaxBytes` (200 GiB by default): `507` `ApiError{"attachment storage is full"}`. Images are charged to the same quota.
- Otherwise the body is streamed to `<Vorcall:AttachmentsDir>/<id>.bin` — one extension for every attachment whatever its type, so the type is never part of a path. The upload is aborted with `413` if the body exceeds the declared length while streaming, or `400 ApiError{"body ended early"}` if it ends before reaching it. Success is `201` `Attachment{id, file_name, content_type, size}`.
- `X-Vorcall-Filename` is optional: a bare file name, at most 255 characters after sanitising (path separators and control characters removed). The default is `file.bin`.
- Rate limit: 20 uploads per minute per user, shared with `POST /api/images` and `POST /api/streams`.

An upload is **unlinked** until a `SendMessage` names its id, and **incomplete** until its body has arrived in full. The sweeper runs every 10 minutes: an unlinked row is taken an hour after it was written, file and row together, while a row still marked incomplete is given 24 hours instead, because a 2 GiB body over a thin link can outlive the shorter cutoff several times over.

**Download** — `GET /api/attachments/{id}`: unknown id `404`; unlinked and the caller is not the uploader `403`; linked and the caller may not view the message's channel `403`. Otherwise the bytes with `Content-Length`, the strong `ETag` `"<id>-<size>"`, `Cache-Control: private, max-age=31536000, immutable`, and Range supported. A row written before 0.6.0 sits under the `<id>.<ext>` name its content type gave it; the store resolves either name, so nothing has to be moved.

Deleting a message removes its attachments, rows and files alike.

## Streamed files

A **streamed file** is a file the sender's own client keeps and serves on demand. The server records the offer, asks the owning client for each range a reader wants, and copies the bytes from that request into the reader's response as they arrive; it never writes them to disk and never holds more than one copy buffer of them. Nothing is charged to `Vorcall:AttachmentsMaxBytes`, and the file is readable **only while its owner has a live WebSocket**.

A file above the 2 GiB attachment ceiling can be sent no other way; below it either kind will do and the sending client chooses. `Vorcall:StreamsEnabled = false` leaves all four endpoints unmapped, so nothing can be offered and therefore no `SendMessage` can name one.

All four endpoints need the door key and a bearer access token.

**Offer** — `POST /api/streams?channel=<id>`, the body a `StreamOffer{file_name, content_type, size}` → `201` `StreamedFile{id, file_name, content_type, size, owner_id}`. No bytes move.

- The `channel`, `VIEW_CHANNEL` and `ATTACH_FILES` rules, the media-type grammar (blank meaning `application/octet-stream`) and the 255-character file-name sanitising are the attachment upload's, unchanged.
- `size` must be positive — `400 ApiError{"size"}` — and at most 1 TiB, else `413 ApiError{"files must be 1 TiB or smaller"}`.
- Rate limit: the upload bucket, 20 per minute per user.

**Read** — `GET /api/streams/{id}` → `200`, or `206` for a single satisfiable `Range` in bytes (several ranges, another unit or a malformed header are read as the whole file). The response carries `Content-Length`, `Accept-Ranges: bytes`, `Cache-Control: private, no-store`, `X-Content-Type-Options: nosniff` and `Content-Disposition: attachment`. There is no `ETag`: these bytes are somebody else's disk, not this server's.

- Unknown id: `404 ApiError{"no such streamed file"}`. While no message names the offer only its owner may read it (`403 ApiError{"not yours"}`); once one does, anyone who may view that message's channel may read it, and nobody else (`403 ApiError{"VIEW_CHANNEL"}`).
- A range starting past the end: `416` with `Content-Range: bytes */<size>`; one ending past it is clamped.
- The owner's account deleted: `410 ApiError{"the sender no longer has this file"}`. The owner offline, or its socket unable to take the request: `409 ApiError{"the sender is offline"}`. The owner already serving `Vorcall:StreamMaxTransfersPerOwner` transfers (8 by default, 1..64): `503 ApiError{"the sender is serving too many transfers"}`.
- Otherwise the server sends the owner `ServerFrame.stream_request` = `StreamRequest{stream_id, transfer_id, offset, length}` and waits `Vorcall:StreamSenderTimeoutSeconds` (30 s by default, 1..600) for an answer. Nothing: `504 ApiError{"the sender did not answer"}`. A decline: `410` carrying the decline's reason. The host stopping: `503 ApiError{"server shutting down"}`.
- The status, `Content-Type` and `Content-Length` go out before the first byte exists, so the reader sees its `206` while the owner is still seeking. A push that then ends short aborts the connection rather than letting a truncated body read as a whole file.

**Push** — `POST /api/streams/{id}/chunks?transfer=<transfer_id>`, the body exactly the bytes of the range asked for → `204`. `transfer` must be a positive run of ASCII digits, else `400 ApiError{"transfer"}`. A transfer unknown, not this bearer's or not this stream's: `404 ApiError{"no such transfer"}`; one already pushed or declined: `409 ApiError{"transfer already answered"}`; the reader gone first: `410 ApiError{"the reader went away"}`; a body ending before the range does: `400 ApiError{"body ended early"}`.

**Decline** — `POST /api/streams/{id}/decline?transfer=<transfer_id>&reason=<phrase>` → `204`, with the same `400` / `404` / `409` rules. `reason` is optional: it is stripped of control characters and cut to 200 characters, and becomes the detail of the reader's `410`; empty or absent means `"the sender declined"`.

An offer no message ever named is swept an hour later on the attachment sweeper's ten-minute schedule — rows only, since it never had bytes here. Deleting a message drops its streamed files with the rest.

## Profiles and images

A member is a `Profile{user_id, username, nickname, avatar_image_id, banner_image_id, description, accent_color, role_ids, online, server_muted, server_deafened}`. Clients show `nickname` when it is non-empty and `username` otherwise, everywhere a member appears.

- `UpdateProfile{description, accent_color, avatar_image_id, banner_image_id}` — own profile only, no permission needed. `description` is 0..256 scalars, `accent_color` is `0xRRGGBB` (`0` = none). The two image fields are sentinel-coded: `0` keeps the current image, `-1` clears it, any other value must name an image of the right purpose uploaded by the caller (else `ERROR_CODE_UNKNOWN_IMAGE`). Answers `MemberUpdated` to every online member.
- `SetNickname{user_id, nickname}` — `user_id = 0` means self. On self the caller needs `CHANGE_NICKNAME`; on another member `MANAGE_MEMBERS` plus the hierarchy rule (`ERROR_CODE_HIERARCHY` / `ERROR_CODE_FORBIDDEN`). An empty `nickname` clears it; otherwise the name grammar applies (`ERROR_CODE_INVALID_NAME`). Answers `MemberUpdated` to every online member.

**Images** are a separate store from attachments, with a purpose attached to each upload, and they are the one place the old picture rules still hold: an attachment may be any file of any type up to 2 GiB, an image may not.

**Upload** — `POST /api/images?purpose=avatar|banner|server_icon|role_icon`, the raw image bytes as the body. `Content-Type` must be one of `image/png`, `image/jpeg`, `image/gif`, `image/webp`, else `415` `ApiError{"unsupported image type"}`, and the body's first bytes must match that type's magic number, else `400` `ApiError{"body is not the declared image type"}` — checked while the body streams in, so a renamed file is refused rather than stored. No `Content-Length`: `411`. Above 8 MiB: `413` `ApiError{"images must be 8 MiB or smaller"}`. The storage quota and the 20-uploads-per-minute-per-user rate limit are the attachment upload's, unchanged: images and attachments share `Vorcall:AttachmentsMaxBytes` and share the bucket. `purpose` missing or not one of the four: `400`. `avatar` and `banner` need a bearer and nothing more; `server_icon` needs `MANAGE_SERVER` and `role_icon` needs `MANAGE_ROLES`, a missing bit being `403 ApiError{detail = <bit name>}`. Success is `201 Image{id, content_type, size}`.

**Download** — `GET /api/images/{id}`: any bearer may read any image; `404` when the id is unknown. The response carries `Content-Length`, the strong `ETag` `"<id>"` — the id alone, unlike an attachment's `"<id>-<size>"`, because an image's bytes never change (a new picture is a new row) — `Cache-Control: private, max-age=31536000, immutable`, and Range support. Clients may therefore cache an image id for good.

An image referenced by a profile, the server or a role is kept; an unreferenced one — never linked, or dropped when its reference was replaced or cleared — is swept with the unlinked attachments once it is older than 1 hour. A frame referencing an image id that does not exist, carries the wrong purpose, or (for `avatar`/`banner`) was not uploaded by the caller is refused with `ERROR_CODE_UNKNOWN_IMAGE`.

Clients downscale before uploading: avatar and server icon to at most 512×512, banner to 1600×600, role icon to 128×128. The server enforces bytes and type only, never dimensions.

## Moderation

- `KickMember{user_id}` — needs `KICK_MEMBERS` and the hierarchy rule. The account keeps existing; its live connection receives fatal `ERROR_CODE_KICKED` then close 1008, every refresh token of the account is revoked, and the other online members receive `MemberUpdated{online = false}`. The kicked account may sign in again immediately with its password.
- `BanMember{user_id, reason}` — needs `BAN_MEMBERS` and the hierarchy rule; `reason` is 0..256 scalars, an over-long one `ERROR_CODE_INVALID_ARGUMENT{"reason"}`, and an unknown or already-banned target `ERROR_CODE_UNKNOWN_USER`. In one transaction a ban row is written (`Ban{user_id, username, reason, banned_by, banned_at_unix_ms}`), **every message the target ever sent becomes a tombstone** with its attachment files removed, the target's per-user channel overrides are dropped, and its refresh tokens are revoked. Then the frames: one `MessageDeleted` **per tombstoned message** to that message's channel's viewers, a `ChannelUpserted` to the viewers of each channel an override was removed from, the kick itself with `ERROR_CODE_BANNED`, the `MemberUpdated{online = false}` that ending a session produces, and finally `MemberRemoved{user_id}` to everyone. From then on `POST /api/auth/login` and `POST /api/auth/refresh` answer `403 ApiError{"banned"}` and the `/ws` upgrade answers a bare `403`.
- `UnbanMember{user_id}` — needs `BAN_MEMBERS`; no hierarchy check, the target is not a member any more. The ban row is removed and `MemberUpdated` carrying the profile goes to everyone. Messages stay tombstones.
- `VoiceModerate{user_id, channel_id, set_muted, muted, set_deafened, deafened, move, move_to}` — each flag is applied only when its `set_*` companion is true, so one frame can mute, deafen, move, or any combination. The bits are resolved **in the channel the target is currently in**: `MUTE_MEMBERS` for mute, `DEAFEN_MEMBERS` for deafen, `MOVE_MEMBERS` for a move (in both the old and the new channel), and the hierarchy rule applies to the target throughout. A target not in that channel's voice session is `ERROR_CODE_NOT_IN_VOICE`.
  - **Server mute and deafen** persist on the account (`Profile.server_muted` / `server_deafened`) and apply to the live session at once: the relay drops audio from a muted session, and skips a deafened recipient on fan-out — audio and share media alike. Both flags are broadcast in the channel's `VoiceState` and in `MemberUpdated`, so every client shows them.
  - **Move** ends the target's session in the old channel (`VoiceMemberLeft` to that channel's viewers) and sends the target `VoiceMoved{move_to}`; the target then sends `JoinVoice{move_to}` itself, with a fresh key and ssrc. A move needs the target to resolve `CONNECT` in `move_to`. `move_to = 0` disconnects instead: `VoiceMoved{0}` and no rejoin.
- `JoinVoice` by a member without `SPEAK` in that channel still creates the session, but muted: the relay drops its audio and `VoiceMember.server_muted` is true for it. Granting `SPEAK` later unmutes it without a rejoin.
- `VoiceMember.priority` mirrors `PRIORITY_SPEAKER` resolved in that channel. It is recomputed whenever roles or overrides change, and a fresh `VoiceState` goes to the channel's viewers when any member's flags move.
- `TransferOwnership{user_id}` — owner only (`ERROR_CODE_FORBIDDEN` for anyone else, the target being the owner included); the target must exist and not be banned (`ERROR_CODE_UNKNOWN_USER`). `Server.owner_id` is persisted and `ServerUpdated` goes to everyone.
- `UpdateServer{name, description, icon_image_id}` — needs `MANAGE_SERVER`; name and description follow the usual grammars (an over-long description is `ERROR_CODE_INVALID_ARGUMENT{"description"}`). `icon_image_id` is **not** sentinel-coded the way `UpdateProfile`'s image fields are: `0` clears the icon, any other value must name a `server_icon` image (`ERROR_CODE_UNKNOWN_IMAGE`), and a negative value is `ERROR_CODE_INVALID_ARGUMENT{"icon_image_id"}`. `ServerUpdated` to everyone.

## Invites and bans (REST)

All four endpoints need the door key, a bearer access token and the permission named below. A missing permission is `403 ApiError{detail = <bit name>}`.

| Endpoint | Permission | Behaviour |
|---|---|---|
| `GET /api/invites` | `MANAGE_INVITES` | `200 InviteList` — every invite, used and revoked ones included; `used_by` is 0 while unused, `created_by` is 0 for an invite the admin CLI made, `revoked_at_unix_ms` is 0 unless the invite was revoked |
| `POST /api/invites` | `MANAGE_INVITES` | body `CreateInviteRequest{days}`, `days` 1..365 (`400` otherwise) → `201 InviteCreated{id, code, expires_at_unix_ms}`. The plaintext `code` appears here and nowhere else: the server keeps only its hash |
| `DELETE /api/invites/{id}` | `MANAGE_INVITES` | `204` — revoking **marks** the row (`revoked_at_unix_ms`) and keeps it in the list; no row is ever deleted. Also `204` for an already-revoked invite, which changes nothing, and for a used one, which is not revocable at all: that row is the audit trail of the account it admitted. `404` when the id is unknown |
| `GET /api/bans` | `BAN_MEMBERS` | `200 BanList` — `banned_by` is 0 when the account that issued the ban no longer exists |

## Voice

Voice lives in voice channels and in DM channels; a text channel has none. A member is in a channel's voice session only after `JoinVoice`, and signalling for it rides the same WebSocket. Media does not — see Media transport.

### Signalling

- `JoinVoice{channel_id}` — the channel must be kind `VOICE` or `DM`. Id 0, unknown, or not visible to the caller: non-fatal `ERROR_CODE_UNKNOWN_CHANNEL`. A text channel: non-fatal `ERROR_CODE_INVALID_ARGUMENT`. Without `CONNECT` in it (a DM member always has it): non-fatal `ERROR_CODE_PERMISSION_DENIED{"CONNECT"}`. Relay disabled: non-fatal `ERROR_CODE_VOICE_UNAVAILABLE`. Already in that voice session: the existing media session ends first — every viewer of the channel, the caller included, receives `VoiceMemberLeft` — and a new one is created exactly like a fresh join, so a `VoiceReady` always carries a key and ssrc that were never used before. Otherwise the server creates a media session, sends the caller `VoiceReady` then `VoiceState`, and every other viewer of the channel receives `VoiceMemberJoined`. A joiner without `SPEAK`, or one the moderators have muted or deafened, has those flags on its session from the first packet. `self_muted` and `self_deafened` carry the joiner's own switches, so a client that rejoins muted is never drawn unmuted while it announces them.
- `LeaveVoice{channel_id}` — unknown or invisible channel: `ERROR_CODE_UNKNOWN_CHANNEL`. Not in that channel's voice session: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Otherwise every viewer of the channel **including the leaver** receives `VoiceMemberLeft`, and the media session is invalidated.
- `VoiceSelfState{channel_id, muted, deafened}` — the caller's own mute and deafen switches, which every other client draws and nothing else reads: the relay is never told, so a self-mute stays the member's own to undo. No permission is involved — the caller is describing itself. Unknown or invisible channel: `ERROR_CODE_UNKNOWN_CHANNEL`. Not in that channel's voice session: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Otherwise, when either flag actually moved, every viewer of the channel receives a fresh `VoiceState`; a frame repeating what the server already holds is answered with nothing at all, and success has no reply either way.

Audience: `VoiceState`, `VoiceMemberJoined`, `VoiceMemberLeft` and `Speaking` go to every member who may view the channel, whether or not they are in voice. `VoiceReady` and `VoiceMoved` go only to the member they are about — `VoiceReady` carries that session's key and ssrc.

`VoiceMember` is `{user_id, username, ssrc, sharing, share_audio, server_muted, server_deafened, priority, self_muted, self_deafened}`; `user_id` is the same stable id as in `Profile`. The two share flags are described under Screen share, the three moderation flags under Moderation. The two self flags are the member's own switches, as its `JoinVoice` and its `VoiceSelfState` frames reported them: display only, never enforced anywhere.

`VoiceState` arrives at `Hello` (one per voice or DM channel with at least one member), to the joiner right after its `VoiceReady`, to both parties when a DM is opened, and to a channel's viewers whenever a member's moderation, priority or own mute/deafen flags change — possibly with zero members, so a client always learns the occupancy of a channel it can see.

- When a connection ends, each channel it had a voice session in receives `VoiceMemberLeft`, before the `MemberUpdated{online = false}`.
- Session replacement: the replaced connection's voice sessions end with `VoiceMemberLeft` to the channel. Voice membership is **not** transferred — the media key and ssrc belong to the old session. The new connection must send `JoinVoice` again.
- A permission change that takes `VIEW_CHANNEL` or `CONNECT` away from a member in that channel's voice session, or the channel being deleted, ends the session: `VoiceMemberLeft` to the channel's viewers and `VoiceMoved{0}` to that member.

`Speaking{channel_id, user_id, speaking}` is server-derived, never sent by clients. `true` on the first authenticated audio packet of a session that was not speaking; `false` once 250 ms pass without an audio packet, evaluated every 100 ms; `false` is also sent when a speaking session ends. A server-muted session never speaks — its audio is dropped before it counts.

Ordering: voice membership changes and their broadcasts are serialized with every other mutation under the same server lock. A connection never sees a voice frame before its `ServerSnapshot`.

### Screen share

Sharing rides the voice session: a member may only share in a channel it already has a voice session in, and the share ends with that session.

- `StartShare{channel_id, audio}` — unknown or invisible channel: non-fatal `ERROR_CODE_UNKNOWN_CHANNEL`. Not in that channel's voice session: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Without `SHARE_SCREEN` in it: non-fatal `ERROR_CODE_PERMISSION_DENIED{"SHARE_SCREEN"}`. Sharing disabled on the server: non-fatal `ERROR_CODE_SHARE_UNAVAILABLE`. The channel already at its sharer limit: non-fatal `ERROR_CODE_SHARE_LIMIT`. Otherwise the session is marked as sharing, `ShareStarted{channel_id, user_id, audio}` is broadcast to every viewer of the channel, **the sharer included**, and the sharer receives `ShareWatchers{channel_id, count}`. The frame is idempotent: repeating it — including with a different `audio` — re-sends both frames and does not count against the limit again.
- `StopShare{channel_id}` — unknown or invisible channel: `ERROR_CODE_UNKNOWN_CHANNEL`. Not in that channel's voice session: non-fatal `ERROR_CODE_NOT_IN_VOICE`. In voice but not sharing: non-fatal `ERROR_CODE_NOT_SHARING`. Otherwise `ShareStopped{channel_id, user_id}` is broadcast to the same audience and every watcher of that share receives `WatchState{channel_id, user_id = 0}`.
- `WatchShare{channel_id, user_id}` — unknown or invisible channel: `ERROR_CODE_UNKNOWN_CHANNEL`. The viewer not in that channel's voice session: `ERROR_CODE_NOT_IN_VOICE`. The named user not sharing in that channel, or the viewer itself: `ERROR_CODE_NOT_SHARING`. Otherwise the watch replaces whatever the viewer was watching before — a viewer watches at most one share per voice session (the app holds one voice session, so one share at a time) — the viewer receives `WatchState{channel_id, user_id}`, and both the new sharer and the one the viewer left receive a fresh `ShareWatchers`.
- `UnwatchShare{channel_id}` — unknown or invisible channel: `ERROR_CODE_UNKNOWN_CHANNEL`. Not in that channel's voice session: non-fatal `ERROR_CODE_NOT_IN_VOICE`. Otherwise idempotent: the caller receives `WatchState{channel_id, user_id = 0}` and the sharer it was watching, if any, a fresh `ShareWatchers`.

Audience: `ShareStarted` and `ShareStopped` go to every viewer of the channel, in voice or not. `WatchState` goes only to the viewer it describes; `ShareWatchers` only to the sharer, on every change to its watcher count.

`VoiceMember.sharing` and `VoiceMember.share_audio` carry the same facts in `VoiceState` and `VoiceMemberJoined`, so a client that arrives late learns who is already sharing without waiting for a `ShareStarted`.

A share ends whenever the voice session behind it ends — `LeaveVoice`, a moderator's move or disconnect, a lost `VIEW_CHANNEL`/`CONNECT`, the connection ending, or a session replacement. `ShareStopped` is then sent **before** `VoiceMemberLeft`, and the share's watchers receive `WatchState{user_id = 0}`. A viewer's watch ends the same way when the viewer's own voice session ends, silently: no frame is sent for it.

Share audio reaches that share's watchers only — it is never mixed into the channel's voice audio, so a member who is in voice but not watching hears nothing of it.

The server keeps at most `Vorcall:MaxSharersPerRoom` (default 3) sharers per channel, and `Vorcall:ShareEnabled` (default true) is the kill switch that makes every `StartShare` answer `ERROR_CODE_SHARE_UNAVAILABLE`.

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
- **Relay to client, audio** — the relay opens the packet with the sender's key, re-seals the plaintext with the recipient's key under the unchanged header, so the recipient sees the original sender's `ssrc`, `seq` and `ts`, and forwards it to every other voice member of the channel that is not deafened. No mixing, no transcoding.
- **Relay to client, pong** — the same header as the ping except `type = 3` and `seq = ping.seq | (1 << 63)`, sealed with that session's key. The bit keeps the pong nonce distinct from the ping nonce.
- **Relay to client, share media** — types 4 and 5 are accepted only from a session that is sharing (type 5 only when that share was started with `audio`), and are re-sealed per recipient under the unchanged header and forwarded **only to that sharer's watchers**, the deafened ones skipped, never to the rest of the channel. They leave from a per-session sender worker rather than the receive path, which stays free for audio.
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

The relay processes each inbound datagram in this order: size (≤ 1200 bytes, ≥ 35 bytes) → header (`ver = 1`, `type ∈ {1, 2, 4, 5, 6}`) → session lookup by ssrc → per-session rate limit — share media (types 4 and 5) is charged to a byte budget (`Vorcall:ShareMaxKbps`, default 30000 kbit/s, burst the larger of 1.5 MiB and half a second of that rate) instead of the packet bucket, and types 1, 2 and 6 to the packet bucket (100 packets/s sustained, burst 200) → AEAD open → server mute (a type 1 packet from a muted session is dropped here and never marks the session speaking) → replay window (1024 sequence numbers; a seq already seen or older than the window is dropped) → the source address is learned from this packet, and re-learned whenever an authenticated packet arrives from a new address, which is how NAT rebinding is survived → dispatch. Every failure drops the datagram silently; the relay counts drops by reason — `size`, `header`, `unknown_ssrc`, `rate`, `bad_tag`, `replay`, `no_address`, `muted` (a server-muted session's audio), the share-media reasons `share_rate`, `not_sharing`, `not_watching`, `queue_full`, and the send-side reasons `send_error`, `channel_gone`, `seal_failed` — logs per-channel counters every 30 s while the channel has voice members, and exposes the totals as `vorcall_relay_drops_total{reason}` on `GET /metrics`.

`Speaking` is derived from type 1 alone: share media never marks a session as speaking. The relay's UDP socket buffers are 8 MiB in each direction, which the host sysctl in `deploy/provision-host.sh` has to allow.

The relay only ever sends to an address it learned this way: a member whose address is not known yet receives nothing. A media session ends when the voice membership ends; late packets for a removed ssrc are dropped as unknown.

### Client voice state machine

`Idle → Joining (JoinVoice sent) → Ready (VoiceReady received; UDP socket bound and pinging) → Media (first pong received) → Idle (LeaveVoice sent, or the WebSocket ended)`.

"In voice" is user intent and survives reconnects: after every `Welcome` a client that was in voice sends `JoinVoice` again and rebuilds its media path with the new key. The old socket and key are discarded on disconnect. A non-fatal `ERROR_CODE_UNKNOWN_CHANNEL`, `ERROR_CODE_PERMISSION_DENIED` or `ERROR_CODE_VOICE_UNAVAILABLE` in answer to `JoinVoice` clears the intent.

`VoiceMoved{channel_id}` rewrites that intent from the outside: the client drops its current session and, when `channel_id` is non-zero, sends `JoinVoice{channel_id}` for the new channel; `0` leaves it idle. A client shows its own `server_muted` / `server_deafened` flags as moderator-imposed and does not let the user clear them, and it ducks every other peer by 12 dB while a `priority` speaker is speaking. Its own switches are a separate pair it does own: it announces them on `JoinVoice` and again with a `VoiceSelfState` whenever the user flips one, and it draws a peer's `self_muted` / `self_deafened` as that peer's own choice rather than a moderator's. They are display state on every client — a self-muted member simply stops sending.

Within `Media` a client may also be **Sharing** — `StartShare` sent once its own capture is running, ended by `StopShare` or by the voice session ending — and **Watching{user}** — `WatchShare` sent, confirmed by `WatchState` naming that user. On confirmation the client sends one keyframe request, and one more after every detected loss, at most one per 500 ms; after a loss it discards non-keyframe video until the next keyframe arrives.

The receive side keeps one jitter buffer per ssrc: adaptive 60–100 ms, packet loss concealment for missing sequence numbers, late packets dropped. Push-to-talk gates sending only; pings continue while muted or deafened.

## Server session state machine (per connection)

1. **AwaitingHello** — starts at upgrade, 5 s deadline. The bearer token of the upgrade request already identified the user; a banned account never gets that far, the upgrade itself answers `403`, and a disabled one fails bearer validation with `401`. The first frame must be `Hello{protocol_version = 1}`; `nickname` is ignored. `Hello` may also carry `client_version` (e.g. `"0.5.0"`) and `client_platform` (e.g. `"linux-x86_64"`), both optional: the server logs them and stores them on the account (`users.last_client_version`/`last_client_platform`/`last_seen_at`) for the admin CLI's `users list` and `users outdated`. Neither field is enforced — a client that omits them still connects normally. The server replies `Welcome{latest_message_id, member_id, username}` (`latest_message_id` is 0 when no message exists yet) immediately followed by the Hello sequence — `ServerSnapshot`, then the `VoiceState`s — then moves to Ready. Anything else (other frame, bad version, timeout) gets `Error{fatal = true}` with `ERROR_CODE_PROTOCOL`, then close code 1008.
2. **Ready** — handles `OpenDm` and `MarkRead` as described under Server, channels and categories and Messages; `SendMessage`, `EditMessage`, `DeleteMessage` and `React` as described under Messages; `JoinVoice`/`LeaveVoice`/`VoiceSelfState` as described under Voice; `StartShare`/`StopShare`/`WatchShare`/`UnwatchShare` as described under Screen share; the channel, category and override frames under Server, channels and categories; the role, member-role and nickname frames under Roles and permissions and Profiles and images; `UpdateProfile` under Profiles and images; `KickMember`, `BanMember`, `UnbanMember`, `VoiceModerate`, `UpdateServer` and `TransferOwnership` under Moderation; and `Ping`, which gets a `Pong` echoing `sent_at_unix_ms`. `SendMessage`, `EditMessage`, `DeleteMessage`, `React` and `OpenDm` are subject to the write rate limit under Messages; every management and moderation frame is not, because each is already gated behind a permission. A second `Hello` is a fatal `ERROR_CODE_PROTOCOL`.
3. **Any state** — unparsable bytes or a text frame: fatal protocol error, close 1008. No frame received for 120 s: close 1001. A connection whose outbound queue exceeds 256 frames is closed with 1013. On server shutdown every socket is closed with 1001. If a connection's outbound queue is already full when a fatal error occurs, the `Error` frame may be dropped and only the close frame (1013 or 1008) is delivered: a slow consumer is closed as a slow consumer. The admin CLI closes a connection with the application close codes 4001 "kicked by admin" (`users kick`) or 4003 "account disabled" (`users disable` — an account lock, not a ban; the same 4003 also reaches a socket that was already open, at its next write frame, because the handler asks the disabled-account cache before charging the write limiter), no `Error` frame first; the channels the account was in see the usual `VoiceMemberLeft` and the `MemberUpdated{online = false}`. A client must not reconnect on either: 4001 means sign in again by hand, 4003 means the account is locked and cannot sign in at all until the owner enables it again.

The server also sends WebSocket-level keep-alive pings every 30 s; clients answer with pong frames automatically.

## Ordering

`ChatMessage.id` is the only order. Live frames may arrive out of id order under concurrent senders, and a reconnect may deliver a message both live and in history. Clients keep messages in a map keyed by id and render in id order.

## Client connection loop

`Disconnected → Connecting → AwaitingWelcome → Connected → (close, error or 75 s of silence) → Backoff → Connecting`.

- Before every connection attempt the client refreshes the access token when fewer than 60 s remain; a REST call that answers 401 with a Bearer challenge is retried once after a refresh. On a `401` with `WWW-Authenticate: Bearer` it refreshes once and retries that request or upgrade. When a refresh itself answers `401` the loop stops for good and the UI returns to the sign-in screen. A bare `401` (door key) means a stale build: the client keeps retrying at the 30 s cap and shows "Unauthorized". A `403` on login, refresh or the upgrade means the account is banned or disabled: the loop stops and the UI says so.
- `ServerSnapshot` replaces the client's whole model of the server, so no REST call follows `Welcome`. History is fetched per channel on demand (`GET /api/messages?channel=…`, the newest 100 messages) when a channel is first opened.
- Gap-fill: per channel it has already loaded, the client remembers the newest message id it has delivered there. If the newest page starts after that id + 1 and `has_more` is true, it pages backwards with `before` until the pages overlap that id, up to 5 pages in total (500 messages); beyond that a gap may remain. "Load older" uses `before = <oldest known id>`. The client keeps at most 2000 messages per channel, dropping the oldest.
- The client sends `MarkRead` for a channel while it is viewing that channel at the bottom of the list and the window is focused, debounced to at most one per second per channel.
- `ERROR_CODE_SESSION_REPLACED`, `ERROR_CODE_KICKED` and `ERROR_CODE_BANNED` stop the loop for good; the UI returns to sign-in naming the reason. So do the close codes 4001 (kicked) and 4003 (account disabled), which the client reads as `DisconnectReason::Kicked` and `DisconnectReason::Disabled` — a third terminal reason, distinct from the `Banned` that `ERROR_CODE_BANNED` carries.
- `ERROR_CODE_RATE_LIMITED` is shown as a notice; the connection stays up.
- The client sends `Ping` every 30 s while connected.
- Backoff: 1, 2, 4, 8, 16, 30 s (cap) with ±20 % jitter, reset after `Welcome`.

## Forward compatibility

A client that receives a `ServerFrame` whose payload it does not recognise — including an empty payload — logs it and ignores it. Only undecodable bytes are a protocol error. Servers may therefore add new `ServerFrame` payloads without a version bump. New `ClientFrame` payloads still require server support; an unknown client payload is a fatal `ERROR_CODE_PROTOCOL` as today.

The 0.5.0 release replaced rooms with channels and **removed** the room frames rather than keeping them: `ClientFrame` tags 4, 5 and 8 (`join_room`, `leave_room`, `create_room`), `ServerFrame` tags 5, 6, 7, 13 and 14 (`room_state`, `member_joined`, `member_left`, `room_list`, `room_updated`), `ChatMessage` field 5 (`room_id`) and `ErrorCode` 6 and 10 (`NOT_A_MEMBER`, `ROOM_EXISTS`) are all reserved by number and by name, and none of them will ever be reused. `ERROR_CODE_UNKNOWN_ROOM` (5) and `ERROR_CODE_INVALID_ROOM_NAME` (13) kept their numbers under the names `ERROR_CODE_UNKNOWN_CHANNEL` and `ERROR_CODE_INVALID_NAME`, and every `string room_id` became an `int64 channel_id` at its old tag.

The 0.6.0 release is additive: `ChatMessage.streamed_files` (16), `SendMessage.streamed_file_ids` (5), `ServerFrame.stream_request` (35) and `ERROR_CODE_INVALID_STREAM` (28) are new, and nothing was removed or renumbered. A pre-0.6.0 client therefore still connects; it ignores the new payload, and a message carrying nothing but streamed files reads to it as an empty message.

The protocol version stays 1, but a 0.4.x client cannot be served: it knows no channels and would read every id as an empty string. The release's `min_version` is therefore `0.5.0`, so such a client is asked to update by the manifest rather than refused by the protocol.

## Limits (summary)

| Item | Limit |
|---|---|
| Username | 1..32 scalars after trim, no control chars |
| Password | 8..128 scalars |
| Invite code | 20 chars, single use, 7-day expiry |
| Invite validity | 1..365 days |
| Channel / category / role name | 1..32 scalars after trim, no control chars |
| Topic / description / ban reason | 0..256 scalars |
| Roles | ≤ 100, `@everyone` included |
| Categories | ≤ 50 |
| Channels | ≤ 200, DMs not counted |
| Overrides per channel | ≤ 100 |
| Role icon emoji | ≤ 2 scalars |
| Message text | 1..2000 scalars after trim (empty only with attachments or streamed files) |
| Reply excerpt | first 120 scalars |
| Reactions palette | 8 emoji |
| Attachments per message | 4 |
| Attachment size | 2 GiB |
| Attachment types | any; the declared media type is an RFC 9110 token pair, ≤ 128 chars |
| Attachment file name | 255 chars after sanitising |
| Attachment storage quota | 200 GiB by default, shared with images |
| Unlinked attachment sweep | older than 1 h, every 10 min |
| Incomplete attachment sweep | older than 24 h, every 10 min |
| Attachment upload rate limit | 20 uploads/min per user, configurable, shared with images and stream offers |
| Streamed files per message | 4 |
| Streamed file size | 1 TiB |
| Streamed file storage | none; readable only while the owner is online |
| Streamed transfers per owner | 8 at once, configurable 1..64 |
| Streamed sender answer deadline | 30 s, configurable 1..600 |
| Unlinked stream offer sweep | older than 1 h, every 10 min |
| Image upload | 8 MiB, png/jpeg/gif/webp, magic-checked (avatar / server icon 512², banner 1600×600, role icon 128², client-side) |
| Unreferenced image sweep | older than 1 h |
| Write frame rate limit | bucket of 20 per account, refilling 2/s, configurable |
| Diagnostics report | 4 MiB per file, text only |
| Diagnostics rate limit | 10 reports/hour per account, configurable |
| Diagnostics retention | 30 days |
| Refresh token row sweep | 7 days after expiry or revocation, daily |
| Disabled-account visibility to a live bearer | within 30 s |
| MarkRead debounce | 1 s per channel |
| Inbound WebSocket message | 16 KiB |
| REST request body | 16 KiB |
| History page | 1..100, default 100 |
| Gap-fill | max 5 pages |
| Client message cap | 2000 messages per channel |
| Access token | 15 min |
| Refresh token | 30 days sliding |
| Auth rate limit | 10 requests/min per IP, configurable |
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
| Sharers per channel | 3, configurable |
| Keyframe requests | 2/s per viewer |
