<div align="center">

<img src="assets/brand/mark.svg" alt="" width="88">

# Vorcall

**A small, self-hosted voice and text chat for people who already know each other.**

Text and voice channels, screen sharing, DMs, roles and permissions — on a server you run,
for a group you invite by hand.

[![ci](https://github.com/freedomiit/freedom.vorcall/actions/workflows/ci.yml/badge.svg)](https://github.com/freedomiit/freedom.vorcall/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![release](https://img.shields.io/github/v/release/freedomiit/freedom.vorcall)](https://github.com/freedomiit/freedom.vorcall/releases/latest)

**English** · [Português (BR)](README.pt-BR.md)

</div>

---

## What it is

Vorcall is a chat server and a desktop client for a group of friends — a gaming group, a
study group, a small team. One server, invite-only, no directory to browse and no
community to discover. You run the server; nobody else can read what happens on it.

It is deliberately small. There is no federation, no plugin system, no bot API, no mobile
app. What there is:

- **Text channels and voice channels**, grouped under categories, plus two-person DMs.
- **Voice** in every voice channel and DM: push-to-talk or voice activation, echo
  cancellation, noise suppression, per-person volume, priority speaker, and sound effects
  for joining, leaving, muting and deafening.
- **Screen sharing** — a monitor or a single window, with its audio, watched by everyone
  else in the channel.
- **Camera** — a second video stream beside (or instead of) a screen share, watched
  independently, up to four cameras at once per viewer.
- **A shared soundpad** — short clips anyone may upload once and anyone may fire into a
  voice channel, played by every client in it.
- **A sticker library** — small images anyone may send in place of a message, shared by
  the whole server.
- **Roles and permissions**: 25 of them, with colours, icons and a hierarchy, overridable
  per channel and per member.
- **Messages** with replies, edits, reactions, stickers and attachments of any file type —
  up to 2 GiB kept on the server, anything larger streamed straight off the sender's
  machine; selectable text, clickable links, paste a file to attach it; unread and mention
  counts; `@everyone` and `@here`.
- **Profiles**: avatar, banner, nickname, description, accent colour.
- **Moderation**: kick, ban, server mute and deafen, move, invite revocation.
- **Desktop notifications** for messages that arrive while the window isn't focused.
- **A native desktop client** for Linux, Windows and macOS — Rust and `iced`, not a browser
  in a box. Themable, with a quick switcher and rebindable keys.

Deliberately absent: more than one server per deployment, group DMs, threads, message
search, pins, custom status, OAuth or 2FA, and any web UI.

## Install the client

Download the latest build for your platform from
[**Releases**](https://github.com/freedomiit/freedom.vorcall/releases/latest):

| Platform | File | Notes |
| --- | --- | --- |
| Linux (x86_64) | `vorcall-linux-x86_64.tar.gz` | `tar xzf …` then `vorcall-linux-x86_64/install.sh` — installs into `~/.local`, no root. Needs PipeWire. |
| Windows (x86_64) | `vorcall-windows-x86_64-setup.exe` | Installs per user, no admin prompt. SmartScreen: "More info" → "Run anyway". |
| macOS (Apple Silicon) | `vorcall-macos-aarch64.dmg` | Drag to Applications. Ad-hoc signed, so the first launch needs right-click → Open. macOS 13+. |

On Linux emoji come from a bundled copy of Noto Color Emoji; the system one is not
used, because most distributions now ship a COLRv1-only build the client cannot
rasterise.

The client updates itself from whichever server it is connected to, so this download is a
one-time step for people on a server that publishes releases. See
[Client updates](docs/self-hosting.md#client-updates) for what that means when you host
your own.

Prefer to build it yourself? See [docs/development.md](docs/development.md).

## Run your own server

You need a machine with Docker and about 1 GB of free memory. Then:

```bash
git clone https://github.com/freedomiit/freedom.vorcall.git
cd freedom.vorcall
docker compose up -d
```

That is the whole install. It builds the backend from source, starts PostgreSQL beside it,
applies the migrations and generates the two secrets that have no safe default.

**Read the door key it generated** — every client needs it to reach your server:

```bash
docker compose logs backend | grep "Server key"
```

**Make the first invite.** Accounts are invite-only, and the first account to register
becomes the server owner:

```bash
docker compose exec backend dotnet Vorcall.Server.dll invites new
```

**Connect.** In the client's sign-in screen, open the **Server** section, put in your
address and the door key, then create an account with the invite code.

By default the server listens on `127.0.0.1:5000` and is only reachable from that machine —
which is what you want behind a reverse proxy. To put it on the internet with a domain and
TLS, and for backups, upgrades and every configuration knob, read:

### 📘 [**docs/self-hosting.md**](docs/self-hosting.md) · [em português](docs/self-hosting.pt-BR.md)

## How it fits together

```
┌──────────────────┐        WebSocket (protobuf frames)       ┌──────────────────┐
│  Desktop client  │ ───────────────────────────────────────▶ │                  │
│                  │        REST (auth, uploads, history)     │   ASP.NET Core   │
│   Rust + iced    │ ───────────────────────────────────────▶ │     backend      │
│                  │                                          │                  │
│  voice · screen  │        UDP media relay (encrypted)       │   .NET 10        │
│      share       │ ◀──────────────────────────────────────▶ │                  │
└──────────────────┘                                          └────────┬─────────┘
                                                                       │
                                                              ┌────────▼─────────┐
                                                              │    PostgreSQL    │
                                                              └──────────────────┘
```

One WebSocket carries every live event. Audio, video and share audio go over a separate UDP
relay because a reverse proxy cannot carry them and latency matters. Both sides generate
their code from the same [`proto/vorcall.proto`](proto/vorcall.proto).

| | |
| --- | --- |
| `server/` | ASP.NET Core (.NET 10) — API, permission engine, voice relay |
| `client/` | Cargo workspace — the `iced` desktop app and its media crates |
| `proto/` | The shared schema both sides compile from |
| `tests/` | xunit suite: permission matrix, wire protocol, REST surface |
| `deploy/` | nginx configs and host provisioning for a production install |

## Documentation

| | |
| --- | --- |
| [**Self-hosting**](docs/self-hosting.md) | Deploy, expose, configure, back up and upgrade a server |
| [**Development**](docs/development.md) | Build from source, run locally, the test suite and the gates |
| [**Protocol**](PROTOCOL.md) | Wire framing, state machines, permission resolution, limits |
| [**Administration**](docs/administration.md) | The admin CLI: invites, accounts, ownership |
| [**Releasing**](docs/releasing.md) | For maintainers: cutting and signing a client release |

## Contributing

Issues and pull requests are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for how to
build the project and what the gates are. Security problems go to
[SECURITY.md](SECURITY.md), not to a public issue.

## License

[MIT](LICENSE) © Freedom IT
