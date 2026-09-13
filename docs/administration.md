# Administration

Running a Vorcall server day to day: accounts, invites, ownership and moderation.

---

## The admin CLI

The server binary is also the admin tool. When the first argument is `invites`, `users` or
`server`, it runs pending migrations, does the work and exits — it never starts the web
server.

```bash
# Docker
docker compose exec backend dotnet Vorcall.Server.dll <subcommand>

# Local development
dotnet run --project server/Vorcall.Server.csproj -- <subcommand>
```

### Invites

Registration is invite-only. There is no way to open it up.

| Command | What it does |
| --- | --- |
| `invites new [--days N]` | Prints a one-time code. Default expiry 7 days. |
| `invites list` | Every code with its state: used, revoked or expired. |
| `invites revoke <id>` | Kills an unused code for good. The row stays for the record. |

Codes look like `F6M5F-J5GK4-Y4YP4-XG5PW` and are spent by the first account that registers
with them. Someone with the **Manage Invites** permission can also create and revoke them
from inside the app.

### Accounts

| Command | What it does |
| --- | --- |
| `users list` | Every account, with client version, platform and last-seen. |
| `users outdated [--min X.Y.Z]` | Accounts below a version floor. Reads the release manifest when `--min` is absent, and fails if there is neither. |
| `users kick <username>` | Closes the live connection (close code 4001). |
| `users disable <username>` | Locks the account out, revokes its sessions, closes the socket (4003). |
| `users enable <username>` | Lets a disabled account sign in again. |
| `users revoke-sessions <username>` | Signs it out everywhere without locking it. |
| `users set-password <username>` | Prompts for a new password. |

`kick`, `disable` and `enable` reach the running server through the admin endpoint to close
the connection immediately, which needs `Vorcall__AdminKey` to be set. Without it the CLI
says so and the lock still takes effect — the bearer check picks it up within 30 seconds,
and an open socket is closed at its next write frame.

### The server

| Command | What it does |
| --- | --- |
| `server show` | Name, owner and general channel id. |
| `server set-owner <username>` | Hands the server to that account. |

`set-owner` takes effect at the next backend restart: the registry reads the owner once at
boot.

On a fresh install the server row has no owner, and **the first account to register takes
it**. You only need `set-owner` to move ownership afterwards, or to repair a server that
somehow ended up without one.

---

## Disabled is not banned

Two different things, easy to confuse:

|  | Disabled | Banned |
| --- | --- | --- |
| Issued from | The admin CLI, on the host | Inside the app, by a member with the permission |
| Records | `users.disabled_at` | A row in `bans` |
| Their messages | Untouched | Tombstoned |
| Their channel overrides | Untouched | Dropped |
| Membership | Kept | Removed from the server |
| Reversible | `users enable` | Unban, from inside the app |

A disable is an operator's lock — the right tool for a compromised account or someone who
asked to be locked out. A ban is a moderation decision about a person.

---

## Roles and permissions

Roles carry 21 permissions, a colour, an icon, a position in a hierarchy and a "hoist" flag
that decides whether their members are listed separately. A role can only be managed by
someone whose own highest role sits above it, and the owner bypasses every check.

Permissions resolve in this order, which is deliberately **not** Discord's union-of-allows
rule:

1. The base permissions of `@everyone` plus every role the member holds.
2. The channel's `@everyone` override.
3. The member's other roles' overrides, in ascending position order.
4. The member's own override.

Each override step is `(permissions & ~deny) | allow`, so a **higher role's override wins a
conflict** rather than any allow beating any deny.

Two things cannot be changed: the general channel cannot be deleted, and `VIEW_CHANNEL` on
it cannot be denied to `@everyone` — refused when you try to write it, and forced back on
during resolution if it somehow got there.

The full matrix, including how each permission behaves per channel type, is in
[`PROTOCOL.md`](../PROTOCOL.md) § Roles and permissions.

---

## Moderation from inside the app

Members with the right permissions can, without touching the host:

- **Kick** — removes the account from the server; it can come back with a new invite.
- **Ban** — removes it, tombstones its messages and keeps it out. Bans are listed and
  liftable in the server settings.
- **Server mute / deafen** — moderator state, not a local preference: the client shows both
  flags and refuses to let the person clear them.
- **Move** — pulls someone into another voice channel.
- **Priority speaker** — quietens everyone else while they talk, for whoever is listening
  with priority ducking on.

---

## Operational endpoints

Both are served only to loopback and private-range sources, and both answer 404 to anything
else — the same 404 as an unknown path, so they do not advertise their own existence. The
reverse proxy configs in [self-hosting.md](self-hosting.md) return 404 for `/api/admin/`
regardless.

| | |
| --- | --- |
| `GET /metrics` | Prometheus exposition. Outside `/api` on purpose, so the door key does not gate it. |
| `POST /api/admin/kick`, `/api/admin/refresh-account` | What the CLI calls. Needs `X-Vorcall-Admin-Key` to match `Vorcall__AdminKey`; unmapped entirely when that is unset. |

---

## Files members send

An **attachment** is any file of any type, up to 2 GiB, four per message. Its bytes live in
the `attachments` volume under the row id, and they count — together with the avatars,
banners and icons stored beside them — against `Vorcall__AttachmentsMaxBytes`, 200 GiB by
default. Past the quota an upload is refused with 507 and the sender is told the store is
full, so set that key from the real size of the disk. Pictures used as an avatar, a banner,
the server icon or a role icon keep the older, stricter rules of their own: PNG, JPEG, GIF
or WebP, checked against the type's magic number, 8 MiB each.

A **streamed file** is the other half. Above 2 GiB a file cannot be uploaded at all, and
below it the sender may choose to stream anyway: the server records the offer, asks the
sender's own client for each range a reader wants, and proxies the bytes through without
ever writing them down. Nothing of it reaches this disk and nothing counts against the
quota — but it can only be read while the sender is online, and readers see a plain "the
sender is offline" the rest of the time. `Vorcall__StreamsEnabled=false` turns the feature
off server-wide.

Three sweeps run every 10 minutes and need no attention:

| Swept | After |
| --- | --- |
| An upload no message ever named, file and row together; likewise a picture nothing references any more | 1 hour |
| An upload that never finished streaming in — a 2 GiB body over a thin link outlives the shorter cutoff | 24 hours |
| An offer no message ever named; rows only, since it never had bytes here | 1 hour |

---

## Problem reports

The client has a "Report a problem" button that uploads its rolling log and any crash
reports to `POST /api/diagnostics`. They land in the `diagnostics` volume, one file per
upload, named `<userId>-<username>-<utc>-<kind>-<name>`, and are swept after 30 days. Read
them with `ls` and `less` on the host.

Each file is capped at 4 MiB; the client trims to the last 4 MiB before uploading, so a
larger log still gets through.
