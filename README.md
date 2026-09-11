# Vorcall

A private chat for a friend group: rooms and DMs, native Rust desktop client, .NET backend, protobuf over a WebSocket.

## Status

MVP. Present: invite-only accounts, a mandatory `general` room plus public rooms anyone can create or join and two-person DMs, a presence sidebar, live messages with reply/edit/delete and reactions, image attachments, unread and mention counts, older history, notifications, voice channels per room with push-to-talk (global per platform, with a window-focused fallback) or voice activation, input cleanup (echo cancellation, noise suppression and an optional automatic gain), per-user volume and mute, and screen sharing (a monitor or a window, with its audio, watched by other voice members through an H.264 stage rendered in the app).

Deliberately absent: private (invite-only) rooms, file attachments beyond images, OAuth/2FA.

## Repository layout

```
proto/vorcall.proto        shared schema; generated into both server and client at build time
PROTOCOL.md                wire framing, state machines, limits
server/                    ASP.NET Core (.NET 10) backend, Vorcall.Server.csproj (server/Voice/ is the voice UDP relay,
                            server/Updates/ + server/Api/UpdatesEndpoints.cs serve /api/updates/*)
client/                    Cargo workspace (crates/vorcall-proto, crates/vorcall-core -> incl. the `update` module,
                            crates/vorcall-voice -> media engine, incl. video.rs (fragmentation/reassembly)
                            and stereo share audio, crates/vorcall-screen -> screen capture backends
                            (Windows/macOS/Linux) + the OpenH264 codec, no GUI or audio device,
                            crates/vorcall-probe -> headless voice probe binary, also carries the
                            check-update/apply-update oracle subcommands and the share/watch oracle,
                            crates/vorcall-release -> release-side signing tool, binary `vorcall-release`,
                            crates/vorcall-app -> binary `vorcall`, incl. share.rs -> the share pipeline
                            and decode threads, view/stage.rs -> the wgpu shader widget)
client/update-keys.pub     Ed25519 public keys the client trusts for release manifests; empty disables the updater
deploy/                    nginx site configs and the host provisioning script
scripts/                   client release build scripts (Linux, Windows cross-build), push-client.sh (build + push both to the host) and update-oracle.sh
releases/                  gitignored; local dir the dev server serves under /api/updates/*; production's is the host's
docker-compose.yml         local dev: Postgres only
docker-compose.prod.yml    production stack: Postgres + backend, pulled from GHCR
.env.production.example    template for the production .env (never commit the real one)
.github/workflows/         ci.yml (format/lint/build), deploy.yml (build + deploy on push to main),
                            release.yml (build, sign and publish a client release)
```

## Prerequisites

- **Linux (build client + server, run local dev):** Rust via rustup (stable, 1.89+ — MSRV set by notify-rust), .NET SDK 10.0.x, Docker (for the local Postgres), a C++ compiler (`g++` or `clang++`, for OpenH264), ALSA headers for rodio/cpal: Fedora `sudo dnf install -y alsa-lib-devel`, Debian/Ubuntu `libasound2-dev` (CI installs it), PipeWire headers and libclang for the screen-share capture bindings: Fedora `sudo dnf install -y pipewire-devel clang-devel`, Debian/Ubuntu `libpipewire-0.3-dev libclang-dev` (CI installs these too).
- **Cross-build for Windows (from Linux/Fedora):** the above, plus `sudo dnf install -y clang lld llvm`, `rustup target add x86_64-pc-windows-msvc`, `cargo install cargo-xwin`.
- **macOS (build client from source only):** Xcode command-line tools (a C++ compiler for OpenH264), rustup. macOS 13 or newer to run the built client: screen sharing needs ScreenCaptureKit, and the app bundle's `LSMinimumSystemVersion` is 13.0.

Voice adds no build prerequisite beyond the above: `opus-rs` is a pure-Rust codec (no cmake, no system libopus). Screen share adds the C++ compiler (OpenH264 is built from vendored C++ by the `cc` crate, no cmake, no nasm) and, on Linux, the PipeWire/libclang packages above; at runtime the Linux binary links `libpipewire-0.3.so.0` (any desktop since 2022 has it — `install.sh` warns if it is missing after a first install). ALSA headers remain a separate Linux-specific requirement, needed for cpal (capture/playback) as well as rodio.

`dotnet` may not be on `PATH`; it can live at `~/.dotnet/dotnet`. The `dotnet-ef` global tool needs `DOTNET_ROOT=~/.dotnet` and `dotnet` on `PATH` (e.g. `PATH=~/.dotnet:$PATH`).

## Build the client

The server key and server URL are baked in at compile time (see [Client key and URL](#client-key-and-url) below); they can also be overridden at runtime by environment variables of the same names. `vorcall --version` prints `vorcall <version> <platform>` and exits, useful to check what a build reports.

The Linux and Windows release binaries are built here, on the owner's machine: `.github/workflows/release.yml` builds only macOS and fetches the other two from the host (see [Cutting a release](#cutting-a-release)). `scripts/push-client.sh` runs both scripts below and pushes their output; running either on its own is for a one-off build that is not a release.

### Linux

```
scripts/build-client-linux.sh
```

Output: `dist/vorcall-linux-x86_64` and `dist/vorcall-linux-x86_64.tar.gz`, the first-install tarball: `scripts/package-client-linux.sh` packs the binary with its executable bit, the launcher entry and icon from `packaging/linux/`, and a per-user `install.sh`.

### Windows (cross-built from Linux)

```
scripts/build-client-windows.sh
```

Output: `dist/vorcall-windows-x86_64.exe`. The binary is unsigned: SmartScreen's "More info" → "Run anyway" on the first run. The per-user installer (`packaging/windows/vorcall.iss`, Inno Setup) is built only by CI on the Windows runner, around this exact exe after `scripts/push-client.sh` has pushed it, with the icon `scripts/make-windows-icon.sh` renders on Linux.

### macOS

macOS is the one platform CI builds. It produces two files for Apple Silicon (ad-hoc signed — see [Releases and updates](#releases-and-updates)): `vorcall-macos-aarch64.dmg`, the `Vorcall.app` bundle a friend installs, and the bare `vorcall-macos-aarch64` the updater fetches. Building from source still works, and `scripts/bundle-client-macos.sh` wraps the result the same way CI does (it needs `brew install librsvg` for the icon):

```
cd client && VORCALL_SERVER_KEY=<key> cargo build --release -p vorcall-app && cd ..
scripts/bundle-client-macos.sh client/target/release/vorcall <version> dist
```

Binary: `client/target/release/vorcall`; bundle: `dist/Vorcall.app` and `dist/vorcall-macos-aarch64.dmg`.

### Client key and URL

- `VORCALL_SERVER_KEY` is required at compile time. `client/.cargo/config.toml` supplies the placeholder `dev` so a plain `cargo build` from inside `client/` succeeds; it only applies when the working directory is inside `client/`.
- `VORCALL_SERVER_URL` defaults to `https://vorcall.example.com` when unset.
- Both `scripts/build-client-*.sh` scripts source `client/.env.release` if present (gitignored, format `export VORCALL_SERVER_KEY=...` / `export VORCALL_SERVER_URL=...`) and refuse to build if the key is unset or still `dev`.
- Rotating the key means rebuilding and redistributing the client, because the key is compiled in.

## Run locally

Database:

```
docker compose up -d db
```

Server (from the repository root; listens on `http://localhost:5000`, `Development` environment, key `dev`, connection string from `server/appsettings.Development.json`):

```
~/.dotnet/dotnet run --project server/Vorcall.Server.csproj
```

Client (from `client/`, so `client/.cargo/config.toml` supplies the placeholder key):

```
cd client
VORCALL_SERVER_URL=http://localhost:5000 cargo run -p vorcall-app
```

Set `RUST_LOG=vorcall_core=debug` to see the client's connection log.

Client config on disk: `~/.config/vorcall/config.toml` on Linux (via `directories::ProjectDirs::from("br.com", "freedomit", "vorcall")`), keys `username`, `notifications` and `sound` (the old `nickname` key is still read). `notifications` (default on) and `sound` (default off) are also toggled by the two switches at the bottom of the sidebar. Session tokens are stored separately, next to it, in `session.toml` — see [Accounts](#accounts).

## Accounts

Registration is invite-only. The "Create account" screen in the app takes a username, password, confirmation and invite code, and signs the new account in. Username: 1..32 Unicode scalars, unique case-insensitively. Password: 8..128 chars. The pre-shared door key (`X-Vorcall-Key`, compiled into the client — see [Client key and URL](#client-key-and-url)) still gates every request; accounts add identity on top of it, they don't replace it.

Sessions: a JWT access token (15 min) plus a rotating refresh token (30 days sliding), stored by the client in `session.toml` next to `config.toml`:

- Linux: `~/.config/vorcall/session.toml`, mode `0600`.
- Windows: `%APPDATA%\freedomit\vorcall\config\session.toml`.
- macOS: `~/Library/Application Support/br.com.freedomit.vorcall/session.toml`.

(Windows and macOS paths are the platform config dir of the same `directories::ProjectDirs::from("br.com", "freedomit", "vorcall")`.)

Logout revokes the refresh token and deletes `session.toml`. Changing the password (from the header) revokes every other session of the account. Only one connection per account is live at a time: signing in on a second device disconnects the first, which returns to the sign-in screen ("connected from another device").

### Admin CLI

Invites and account maintenance run through the server binary's own subcommands. The subcommand is the first argument; it runs pending migrations first and never starts the web server:

```
invites new [--days N]           # prints a one-time invite code, default 7-day expiry
invites list
users list                       # includes  client <version> <platform>  seen <ts>
users outdated [--min X.Y.Z]     # accounts below the floor; reads the manifest when --min is absent
users revoke-sessions <username>
users set-password <username>   # prompts for the new password
```

`users list` shows each account's last-reported client version, platform and connection time (from the `Hello` frame — see [`PROTOCOL.md`](PROTOCOL.md#server-session-state-machine-per-connection)). `users outdated` lists accounts below `--min`, or below the version published in the current release manifest when `--min` is omitted (`Vorcall:ReleasesDir`, mounted read-only in the CLI container too); with neither a manifest nor `--min` it fails and says so.

Production:

```
cd /opt/vorcall && docker compose -f docker-compose.prod.yml run --rm backend invites new
```

Local:

```
~/.dotnet/dotnet run --project server/Vorcall.Server.csproj -- invites new
```

## Voice

One voice channel per text room. Join it from the sidebar; talk with push-to-talk (default Ctrl) or switch to voice activation; mute and deafen are separate switches. The sidebar shows who is in voice and highlights who is speaking. Media rides a direct UDP path to the server host — not Cloudflare, not nginx — encrypted per voice session.

**Settings:** the "Settings" button in the header opens a full-screen page with the input device, the output device, the transmit mode, the three input-cleanup switches, and (for push-to-talk) the key or mouse button to bind ("Change", then press a key or click a mouse button; Esc cancels). These are stored in `config.toml` as `input_device`, `output_device`, `transmit_mode` (`"push_to_talk"` or `"voice_activation"`) and `ptt_key` (e.g. `"Control"`, `"F8"`, `"a"`, `"MouseBack"`) and the input-cleanup switches `noise_suppression`, `echo_cancellation` (both default `true`) and `auto_gain` (default `false`). A key bound by an earlier version that is not in the bindable list below (Enter or an arrow key, for example) keeps working while the window is focused, but system-wide capture needs one of the listed keys, so re-bind it in Settings.

**Transmit modes:** "Push to talk" (default) sends while the bound key or mouse button is held. "Voice activation" opens a noise gate instead — a threshold slider (−60 to −20 dBFS, default −45) plus a live input level meter showing whether the gate is open; a 20 ms frame opens the gate once its RMS reaches the threshold and closes it 300 ms after dropping 6 dB below threshold, with each reopen starting a new talk spurt. Mute and Deafen work the same in both modes. Bindable inputs: Ctrl, Alt, Shift, Super, Space, Tab, Caps Lock, Insert, Delete, Home, End, Page Up/Down, F1–F24, ASCII letters and digits, and mouse Back, Forward and Middle (character keys assume the US layout on macOS).

**Input cleanup:** three switches in Settings run WebRTC's voice chain (through the pure-Rust `aec3` crate) on the microphone before the level meter, the threshold and the encoder ever see a frame, in both transmit modes and while muted, so the canceller stays converged between sentences. *Echo cancellation* (default on) subtracts what Vorcall plays from the voice channel through the speakers (the notification chime is not part of the reference), learning the delay by itself whatever the devices, so a laptop or desk speakers no longer feed the room back; audio from other applications is not part of the reference and still comes through. *Noise suppression* (default on) takes the fan, hum and hiss out — about 9 dB on steady noise, less on keyboard clicks. *Automatic gain* (default off) lifts a quiet talker towards a normal level and leaves a loud one alone. All three together cost about 2 % of one core while in voice. If the chain fails to build or crashes, the client falls back to the raw microphone for that session and says so in the notice line; toggling any switch retries. With `RUST_LOG=vorcall_app=debug` the canceller logs its delay estimate and echo return loss every 10 s.

**Global capture:** push-to-talk listens system-wide, not only while the window is focused, through a per-platform backend: Windows low-level keyboard/mouse hooks, macOS a listen-only `CGEventTap` gated by the Input Monitoring permission, Linux X11 raw XInput2 events on the root window, and Linux Wayland the `org.freedesktop.portal.GlobalShortcuts` portal (KDE Plasma 5.27+, GNOME 48+, Hyprland — keyboard only, bound in the compositor's own dialog; `WAYLAND_DISPLAY` selects the portal backend over X11). When global capture is unavailable (no portal, permission denied, hook failure) the client silently falls back to window-focused push-to-talk; Settings shows the reason under the push-to-talk row with a Retry button, and the status line shows "PTT: window only".

**macOS caveat:** the app bundle is ad-hoc signed, so the Input Monitoring grant is tied to that specific build and must be re-granted in System Settings → Privacy & Security → Input Monitoring after every update — until then push-to-talk is window-only. Caps Lock cannot be a global push-to-talk key on macOS: it arrives as a modifier-flag change, not a key event. F21–F24 also have no macOS key code, so those bindings stay window-only there too.

**Per-user volume and mute:** click another member in the voice list to expand a volume slider (0–200%) and a Mute button for them. Both are local only — the server never learns about it — persisted in `config.toml` under `[peer_audio.<user id>]` (`volume`, `muted`) and re-applied whenever that user rejoins voice.

**Network:** media goes over UDP 5005 to the server host (`VoiceReady` tells the client the exact host and port). Production needs an ingress rule for UDP 5005 in the OCI VCN security list **and** the host firewall step of `deploy/provision-host.sh`. Server config keys: `Vorcall__VoiceEnabled`, `Vorcall__VoicePort`, `Vorcall__VoiceHost` (all optional, with defaults). Local dev needs nothing extra: the relay binds `0.0.0.0:5005` as soon as the server starts.

**Status line:** "voice N ms · loss x%" is the healthy state (round-trip time and packet loss to the relay); "voice: connecting" means no pong has arrived yet; "voice: no media" means 15 s have passed without a pong — check the VCN rule or host firewall.

**Probe (runtime oracle):** two headless probes exchanging tone are the way to verify the voice path end to end, locally or in production, without opening the GUI.

```
cd client && cargo build -p vorcall-probe
```

Two terminals, local server:

```
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --send-seconds 10 --listen-seconds 14 --expect-peer
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username bob --send-seconds 10 --listen-seconds 14 --expect-peer --tone-hz 660
```

Each probe prints one JSON line to stdout: `packets_sent`/`packets_received`, `decoded_seconds` and `tone_seconds` (how much of the peer's tone was actually decoded), `gaps`/`late`, `rtt_ms` (min/avg/max/last/samples), `link`, one `peers` entry per remote ssrc (received/lost/late/decoded_frames/decoder_resets), `speaking_events`, `frames_sent` (audio frames actually sent, excluding pings) and `frames_gated` (frames the voice-activation gate held back before encoding). Exit codes: `0` ran, `1` `--expect-peer` heard less than 1 s of tone or `--expect-silence` saw audio go out, `2` usage, sign-in, connection or media failure.

Voice-activation flags: `--vad` runs the tone through the same noise gate the app uses before encoding, `--vad-threshold <db>` sets its threshold (default −45), `--tone-amplitude <0..1>` sets the tone's amplitude (default 0.3; `0` is digital silence), and `--expect-silence` exits 1 if any audio frame went out. Oracle recipe: `--vad --tone-amplitude 0 --expect-silence` exits 0 with `frames_sent: 0` (the gate holds silence back); `--tone-amplitude 0 --expect-silence` without `--vad` exits 1 (negative control — without the gate, silence still goes out); two probes run with `--vad --tone-amplitude 0.3 --expect-peer` hear each other normally.

Against production, run the same two-terminal recipe with the two test accounts kept in the gitignored `client/.env.probe` (`export PROBE_A_USER=… PROBE_A_PASS=… PROBE_B_USER=… PROBE_B_PASS=…`), `VORCALL_SERVER_URL` left at its default, and the real key in `VORCALL_SERVER_KEY`.

**Limits worth knowing:** 20 ms Opus frames at 48 kbps CBR; one voice channel per room; echo cancellation covers only what Vorcall plays from the voice channel (the notification chime and music from another application still reach the microphone); mouse-button push-to-talk bindings are window-only on Wayland (the portal is keyboard-only).

## Screen share

Any member of a room's voice channel can share a monitor or a window, with its audio, to the other members already in that channel; several members can share at once, and a viewer watches one share at a time. Video is H.264 (OpenH264, a pure-Rust-built BSD-2 port of Cisco's encoder/decoder, no cmake) fragmented over the same UDP session and key as voice, rendered through a wgpu texture in the app. A share ends the moment its voice session does.

**Settings:** the "Screen share" section of the full-screen Settings page holds `share_resolution` (`"source"`, `"720p"`, `"1080p"`, `"1440p"`, `"2160p"`; default `"720p"`), `share_fps` (15/30/60; default 30), an Auto/manual bitrate choice (`share_bitrate_kbps`, absent for Auto, otherwise 1000..30000 kbit/s), `share_audio` (default on) and `share_volume` for what is currently watched (0..200%, default 100%) — all in `config.toml`.

**Quality:** the source is scaled down to fit inside the chosen resolution's box, aspect kept, never upscaled (a `"source"` share is capped at 3840×2160, the largest picture OpenH264 accepts). Auto bitrate reads a table by output size and frame rate (kbit/s): 720p 1500/2500/4000, 1080p 3000/4500/7000, 1440p 5000/8000/12000, 2160p 10000/15000/24000 for 15/30/60 fps; a manual value overrides it. The encoder skips frames rather than exceed the target rate, and the stage shows the rate actually achieved.

**Capture backends:** Windows captures a monitor with Desktop Duplication (the cursor composited in, since Desktop Duplication does not include it) and a window with Windows.Graphics.Capture, falling back to Windows.Graphics.Capture for a monitor too if Desktop Duplication is refused; an unpackaged app still gets the yellow WGC capture border on a captured window, whatever the request. macOS captures both kinds with ScreenCaptureKit. Linux goes through the desktop portal's picker (`org.freedesktop.portal.ScreenCast`) and PipeWire, so Vorcall never lists sources of its own there — the portal's own dialog does, and its choice is remembered (`PersistMode::ExplicitlyRevoked`) so a later share in the same run does not re-prompt; a compositor that only offers DMA-BUF frames ends the capture with a clear reason instead of failing silently.

**macOS caveat:** the Screen Recording permission is tied to the specific (ad-hoc signed) build, so — like Input Monitoring for push-to-talk — it must be re-granted in System Settings → Privacy & Security → Screen Recording after every update, and only takes effect after relaunching. macOS 13 or newer is required.

**Audio:** macOS excludes Vorcall's own playout from a share's audio natively (ScreenCaptureKit); Windows tries process-loopback capture (excluding this process, build 20348+) and falls back to plain loopback; Linux captures the default sink's monitor, which is always the whole desktop. Where the capture includes Vorcall's own voice audio, a second echo canceller (AEC3, echo cancellation only — no noise suppression, no automatic gain, since a share carries music and game sound as well as speech) subtracts it before encoding, so a watcher does not hear the room's own voices coming back through the share.

**Watching:** "Share screen" in the voice controls opens Vorcall's own picker on Windows and macOS (a list of monitors and windows plus a share-audio checkbox) or goes straight to the desktop portal's dialog on Linux. A sharer shows a ▣ badge next to their name in the voice roster; clicking it, or "Watch" in the expanded member panel, opens the stage in the centre pane: a sharer picker (when more than one member is sharing), a volume slider when the share has audio, live stats, Pop out (its own window; closing it brings the stage back to the centre pane), Fullscreen (Esc exits) and Stop watching.

**Network:** share media rides the same UDP path as voice (see Network above), billed against its own byte budget rather than the packet-rate limit voice and pings use. Server options: `Vorcall__ShareEnabled` (default true, the kill switch), `Vorcall__ShareMaxKbps` (default 30000, the per-sharer ceiling), `Vorcall__MaxSharersPerRoom` (default 3). `deploy/provision-host.sh` raises the host's `net.core.rmem_max`/`wmem_max` to 16 MiB so the relay's 8 MiB socket buffers actually take; without that step the kernel clamps them silently. A room's worst-case bandwidth is sharers × watchers × `ShareMaxKbps`.

**Status line:** " · sharing N kbit/s" is appended while a share is active, N being the encoder's currently achieved rate.

**Probe (runtime oracle):** the same two-terminal idea as voice, one probe sharing a synthetic test pattern and the other watching it.

```
cd client && cargo build -p vorcall-probe
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --share-seconds 10 --share-audio --listen-seconds 14
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username bob --watch alice --expect-video --expect-share-audio --listen-seconds 14
```

`--share-size WxH` (default 1280x720), `--share-fps` (15/30/60), `--encode-threads` (H.264 slice threads) and `--bitrate-kbps` tune the sharing side; `--share-seconds` and `--watch` are mutually exclusive within one probe. The sharing probe reports `"share": {frames_encoded, keyframes, keyframe_requests, encode_fps, kbps, watchers_max, bytes, threads, skipped}`; the watching probe reports `"watch": {user, user_id, pictures, keyframes, dropped, decode_errors, first_picture_ms, width, height, decode_fps, keyframe_requests_sent, share_tone_seconds}`. Exit codes add to the voice probe's: `1` also on `--expect-video` with fewer than 5 pictures decoded, or `--expect-share-audio` with less than 1 s of share tone heard.

**Limits worth knowing:** one watched share per viewer; the source is never upscaled; keyframe requests are capped at 2 per second per viewer; a share ends the instant its voice session does, with no local self-preview for the sharer; there is no congestion control or retransmission — a lossy link shows as dropped frames and a black stage until the next keyframe.

See [`PROTOCOL.md`](PROTOCOL.md#screen-share) for the signalling frames, audience rules and error codes, and § Media transport there for the datagram types and the limits table for the full set of numbers.

## Rooms and messages

`general` is created for every account and cannot be left. Beyond it, rooms are public: anyone can browse the room list and join with `CreateRoom`/`JoinRoom`; a room's id is a slug of its name (`"Team  Chat_2"` → `team-chat-2`), and there is no rename or delete. A DM is a two-person room opened with `OpenDm` (id `dm-<lower user id>-<higher user id>`); it cannot be left either, but the client can hide one from the room list until the next message arrives in it. Each room, DMs included, has its own voice channel.

The room list pane shows a badge per room — grey for unread, red for a mention — kept by a server-persisted read cursor per user and room (`MarkRead`, debounced to one call per second); a room only counts as read while it is in view, at the bottom of the message list, in a focused window. History for a room loads on first view (`GET /api/messages?room=…`).

Messages support a reply (`reply_to_id`, shown as a quoted excerpt of up to 120 characters), edit (author only) and delete (a tombstone: the row stays, its text/attachments/reactions are gone), and reactions from a fixed palette — 👍 ❤️ 😂 😮 😢 🔥 🎉 👀 — toggled per user and grouped by emoji. `VORCALL_TEXT_REACTIONS=1` renders that palette as text labels instead of emoji, for terminals or fonts that cannot draw them. Hovering a message in the app surfaces reply/react/edit/delete actions (delete asks for a second click). `<@id>` mention tokens are rendered as `@username` and autocompleted after typing `@`; a mention or a DM notifies unless that room is already in view in a focused window, and anything else notifies only when the window is unfocused. Message text is capped at 2000 characters; the client keeps at most 2000 messages per room in memory.

Attachments are images only (png, jpeg, gif, webp), up to 8 MiB each and 4 per message, picked with a file dialog (`rfd`) or dragged and dropped, downscaled to 1600 px on the longest side for display and cached under the platform cache directory (`vorcall/attachments`, pruned to 500 MiB at startup by oldest modification time). They upload as a raw body to `POST /api/attachments?room=<id>` ahead of the message that references them (rate limited to 20 uploads per minute per user) and are stored on the server under `Vorcall:AttachmentsDir` (env `Vorcall__AttachmentsDir`, default `/attachments`) up to a total quota `Vorcall:AttachmentsMaxBytes` (env `Vorcall__AttachmentsMaxBytes`, default 2 GiB — uploads are refused with `507` once it would be exceeded). An upload never linked to a message within 1 hour is swept (first sweep 1 minute after boot, then every 10 minutes). Deleting a message removes its attachment files with it.

See [`PROTOCOL.md`](PROTOCOL.md) § Rooms and presence, § Messages and § Attachments for the wire-level rules, and the limits table there for the full set of numbers.

## Protocol

See [`PROTOCOL.md`](PROTOCOL.md) for framing, connection state machines and limits, and [`proto/vorcall.proto`](proto/vorcall.proto) for the message schema. Both sides generate code from the `.proto` file at build time; never hand-edit generated code.

## Production

- Domain `vorcall.example.com` points DNS-only (no Cloudflare proxy) at the Oracle ARM64 host `user@host`, app directory `/opt/vorcall`.
- `deploy/provision-host.sh` runs on the host: installs certbot, issues the Let's Encrypt certificate, installs the nginx site (`deploy/nginx/vorcall.conf`, backend proxied from loopback `127.0.0.1:5004`, `listen 443 ssl http2` to match the other sites on the host and silence the "protocol options redefined" warning), sets up the certbot renewal hook, and generates the host's `.env` from `.env.production.example` (random Postgres password, `Vorcall__ServerKey` and `Vorcall__JwtSigningKey`). See the script header for the exact invocation. nginx config changes on an existing host are applied by hand — the deploy workflow only copies the compose file.
- Deploy pipeline (`.github/workflows/deploy.yml`): a push to `main` builds an arm64 backend image on GitHub Actions, pushes it to `ghcr.io/freedomiit/vorcall-backend`, then copies `docker-compose.prod.yml` to the host and runs `docker compose pull && up -d` over SSH. The backend applies its own EF Core migrations on boot. A push to `main` deploys production immediately; there is no separate approval step.
- The pre-shared door key lives only in the host's `.env` (`Vorcall__ServerKey`) and must be baked into client builds as `VORCALL_SERVER_KEY`. Rotating it means: generate a new value, update the host `.env`, restart the backend, and rebuild/redistribute the client with the new key.
- `Vorcall:JwtSigningKey` (env `Vorcall__JwtSigningKey`) is required — base64 of 32 random bytes; the server refuses to boot without it. `.env.production.example` carries a placeholder and `deploy/provision-host.sh` generates a real value for fresh hosts; on an existing host, append it by hand (`openssl rand -base64 32`). Rotating it signs every client out within 15 minutes (the access token lifetime).
- Image attachments live on the host under `$APP_DIR/attachments`, bind-mounted read-write into the backend as `/attachments` (`docker-compose.prod.yml`); `deploy/provision-host.sh` creates the directory. nginx has a dedicated `location /api/attachments` (`client_max_body_size 9m`, unbuffered proxying, 300 s timeouts) alongside `location /api/updates/` — both are applied to an existing host by re-running `deploy/provision-host.sh` after copying `deploy/` there.

## Releases and updates

### How updates reach friends

Vorcall checks for a newer build automatically: on the first successful connection after the app starts, and every 6 hours after that while connected and in the chat room. A manual check is also available from Settings ("Check for updates").

A check fetches `/api/updates/manifest`, verifies its signature against the keys baked in at compile time from `client/update-keys.pub`, and — if there is something newer for the running platform — downloads the asset next to the running binary as `.vorcall-update-<version>`. Its size and SHA-256 are checked against the manifest before anything else happens with it.

- **Optional update:** a banner appears under the header — "Vorcall {version} is ready." with "Restart now" and "Later". "Later" hides the banner until the next check finds something new; "Restart now" leaves the room and voice channel cleanly, then swaps the binary and relaunches.
- **Required update:** when the manifest's `min_version` is above the running version, the chat screen is replaced by an "Update required" screen ("Vorcall {version} is needed to keep chatting.") that downloads and restarts on its own; a failure shows the error with a Retry button. The connection underneath is left alone, so a failed restart returns to a room that is still there.
- **Left for the next launch:** a verified download next to the binary that was never applied (the app was closed first) is re-verified from disk and installed at the next start, before the window opens. When the install directory is not writable, the download lands in the user's local data directory instead; that copy is never applied automatically — the UI says where it is and asks to install it by hand.
- Settings also shows `Version {current} ({platform})` and, when the manifest carries notes, "What's new in {version}".

Platform swap mechanics: Linux and macOS rename the downloaded file over the running binary and re-exec — safe, because the kernel keeps the old inode alive for the process that is still running it. On Linux the tarball's install script puts the binary at `~/.local/bin/vorcall`; on macOS it is `Vorcall.app/Contents/MacOS/vorcall`, so the swap happens inside the bundle and leaves the rest of it alone. Windows cannot overwrite a running executable, so it moves `vorcall.exe` to `vorcall.exe.old`, moves the new file into its place and relaunches; `.old` is deleted the next time the new binary starts.

The updater is off entirely for a dev-key build, a debug build, when `VORCALL_NO_UPDATE` is set, or when `client/update-keys.pub` carries no keys — in each case Settings shows why instead of the Check button doing anything.

### One-time setup (owner)

1. `cd client && cargo run -p vorcall-release -- gen-key --out ~/vorcall-update-signing.key` — an Ed25519 key pair; the public half prints as 64 hex characters.
2. Keep the private key file in a password manager. A lost key strands every client already out there: they refuse a manifest signed by any other key.
3. `gh secret set VORCALL_UPDATE_SIGNING_KEY < ~/vorcall-update-signing.key`
4. `gh secret set VORCALL_SERVER_KEY` with the production door key from `client/.env.release`.
5. Paste the printed public key into `client/update-keys.pub` and commit it.
6. Push `main` — deploys the `/api/updates/*` endpoints and the `releases/` compose mount.
7. Copy `deploy/` to the host and re-run `deploy/provision-host.sh` — creates `releases/` and installs the nginx `location /api/updates/`.

Key rotation: add the new public key to `client/update-keys.pub` alongside the old one, ship a release, sign later releases with the new key, then drop the old key from the file once every client has picked up a build that carries the new one.

### Cutting a release

CI builds only macOS. The Linux and Windows binaries are built on the owner's machine and pushed to the host first; the workflow fetches them back, compiles the Windows installer around the pushed exe, and signs one manifest covering all three platforms.

1. Bump `version` in `client/Cargo.toml` (`[workspace.package]`); bump `min_version` too (`[workspace.metadata.vorcall]`) when older clients must be cut off. Commit and push.
2. Optionally dispatch the `release` workflow with `publish` unchecked first — a dry run that builds macOS, then signs and verifies a **macOS-only** manifest. It reaches neither the host nor a GitHub Release, needs no tag, and needs nothing pushed to the host.
3. `git tag -a vX.Y.Z -m "notes"`. The annotation becomes the manifest's `notes` and the GitHub Release body; a lightweight tag (no `-a`/`-m`) gives empty notes.
4. `scripts/push-client.sh` — builds `dist/vorcall-linux-x86_64` (+ tarball) and `dist/vorcall-windows-x86_64.exe`, checksums them and scp's them into `~/freedom.vorcall/releases/<version>/` on the host. It needs `client/.env.release`, SSH access to the host and the Windows cross toolchain (`sudo dnf install -y clang lld llvm`, `cargo install cargo-xwin`), and it refuses to run unless the tag `vX.Y.Z` points at HEAD (`--no-tag-check` overrides, for a scratch build).
5. `git push origin vX.Y.Z`.
6. Watch the workflow run.
7. Verify on the host: `ssh user@host 'head -c 300 ~/freedom.vorcall/releases/manifest.json'`.
8. `users outdated` in the admin CLI (see [Admin CLI](#admin-cli)) lists who has not moved yet.

The workflow refuses to publish when: the tag does not equal the workspace version; `min_version` is above `version`; the macOS build fails; the Linux/Windows binaries are not on the host for that version (run `scripts/push-client.sh` first) or their checksums do not match; or the signing key's public half is not in `client/update-keys.pub` (which is exactly when clients would reject the manifest).

Because the Linux binary is built on the owner's Fedora machine instead of a pinned `ubuntu-22.04` runner, its glibc floor is that machine's glibc: a friend on an older distro can hit `GLIBC_2.xx not found` and needs a build made on an older base.

### First install per platform

Only the first build on each friend's machine is a manual hand-off — later builds arrive through the in-app updater described above.

- **Linux:** hand over `vorcall-linux-x86_64.tar.gz`, then `tar xzf vorcall-linux-x86_64.tar.gz && vorcall-linux-x86_64/install.sh`. No root: the binary lands in `~/.local/bin`, the launcher entry and icon under `~/.local/share`, and Vorcall shows up in the app launcher. `install.sh --uninstall` removes the same files. (The bare `vorcall-linux-x86_64` still works after a `chmod +x`, from any directory the friend can write to.)
- **Windows:** hand over `vorcall-windows-x86_64-setup.exe`. SmartScreen shows "Windows protected your PC" — "More info" then "Run anyway". It installs per user under `%LocalAppData%\Programs\Vorcall` with a Start menu entry, no admin prompt, and the updater keeps replacing `vorcall.exe` there; uninstall from Settings → Apps. Updates the app downloads itself carry no Mark-of-the-Web, so the SmartScreen prompt does not come back. (The bare `vorcall-windows-x86_64.exe` still runs on its own as a portable copy.)
- **macOS (Apple Silicon only):** hand over `vorcall-macos-aarch64.dmg` from the GitHub Release, not the bare binary (Finder opens that one in TextEdit). Open the image, drag `Vorcall.app` onto the Applications shortcut next to it, eject the image and launch the copy in Applications — the image is read-only, so an app started from it could not update itself. The app is ad-hoc signed, not notarised, so macOS blocks the first launch: on macOS 14 and earlier, right-click → Open, then "Open"; on macOS 15, open it once, dismiss the dialog, then System Settings → Privacy & Security → "Open Anyway". `xattr -dr com.apple.quarantine /Applications/Vorcall.app` in Terminal is the shortcut. The updater replaces the binary inside the bundle, so the app has to live where the account can write (Applications is fine for an administrator account).

### Update oracle

`scripts/update-oracle.sh` drives `vorcall-probe check-update`/`apply-update` end to end against a locally running server, using an account that already exists:

```
VORCALL_PROBE_USER=alice VORCALL_PROBE_PASSWORD=... scripts/update-oracle.sh --http http://localhost:5000
```

It publishes a signed `9.9.9` manifest into the local `releases/` directory (backing up and restoring whatever manifest was already there) and cleans up after itself. What it proves:

- **A** — an honest release is accepted, downloaded, and hashes to what the manifest says.
- **B** — a tampered manifest is refused at the signature stage.
- **C** — a tampered asset is refused at the size or hash stage.
- **D** — a `min_version` above the running version is reported as `required`.
- **E** (Linux only) — `apply-update` swaps a throwaway copy of the probe and relaunches it.
- **F** — `--no-download` reports without fetching the asset.

The two subcommands it drives:

```
vorcall-probe check-update --username U [--password P] --platform ID --out PATH [--pubkey HEX ...] [--no-download]
vorcall-probe apply-update --file PATH
```

## Rolling out the accounts release

1. Add `Vorcall__JwtSigningKey` to the host `.env` **before** pushing (see [Production](#production)).
2. Push `main` — this deploys.
3. Generate one invite per friend with the admin CLI (see [Admin CLI](#admin-cli)).
4. Rebuild the clients: `scripts/build-client-linux.sh` / `scripts/build-client-windows.sh`. The Mac friend rebuilds from source.
5. Distribute the new builds. Old clients stop working at deploy time and show "Unauthorized: rebuild the client".

## Rolling out the voice release

1. Open UDP 5005 ingress in the OCI VCN security list.
2. Push `main` — this deploys. Old clients keep working: they ignore the voice frames (see [Forward compatibility](PROTOCOL.md#forward-compatibility)).
3. On the host, run `deploy/provision-host.sh` (adds the iptables rule for UDP 5005; idempotent) — copy the updated `deploy/` to the host first, as the script header says.
4. Verify with two probes against production, using `client/.env.probe` (see [Voice](#voice)).
5. Rebuild and distribute both clients: `scripts/build-client-linux.sh`, `scripts/build-client-windows.sh`. The Mac friend rebuilds from source.
6. Friends check the settings page for their microphone.

## Rolling out the updates release

0.2.0 is the first release with the signed self-updater — the one-time bootstrap below runs once, then every later release only needs [Cutting a release](#cutting-a-release).

1. One-time setup: generate and store the signing key, set the `VORCALL_UPDATE_SIGNING_KEY`/`VORCALL_SERVER_KEY` secrets, paste the public key into `client/update-keys.pub` and commit it (see [One-time setup](#one-time-setup-owner)).
2. Push `main` — deploys the `/api/updates/*` endpoints and the `releases/` compose mount.
3. Copy `deploy/` to the host and re-run `deploy/provision-host.sh` — creates `releases/` and installs the nginx `location /api/updates/`.
4. Dispatch `release` with `publish` unchecked as a dry run first.
5. `git tag -a v0.2.0 -m "..."` && `git push origin v0.2.0`.
6. Verify the manifest on the host: `ssh user@host 'head -c 300 ~/freedom.vorcall/releases/manifest.json'`.
7. Hand-deliver 0.2.0 to each friend — the last manual install: the clients they are running now predate the updater and cannot pick it up themselves.
8. The `Hello` frame changed (`client_version`/`client_platform`) — regenerate the smoke suite's `vorcall_pb2.py` (see `CLAUDE.md` § Commands) and run the full smoke suite before pushing.

## Rolling out the rooms release

0.3.0 adds public rooms, DMs, reply/edit/delete, reactions and image attachments on top of `general`. `min_version` stays at 0.2.0: a client from the updates release keeps chatting in `general` without a rebuild, it just never sees the new frames (see [Forward compatibility](PROTOCOL.md#forward-compatibility)).

1. Push `main` — this deploys; the backend applies the `AddRoomsAndRichMessages` migration (the `rooms`, `room_members`, `reactions` and `attachments` tables) on boot, and serves `/api/attachments/*`.
2. Copy `deploy/` to the host and re-run `deploy/provision-host.sh` — creates the `attachments/` directory and installs the nginx `location /api/attachments` (see [Production](#production)).
3. Verify with a client: create a room or open a DM, send a message with an image attachment, and confirm another member sees it.
4. The proto changed — regenerate the smoke suite's `vorcall_pb2.py` (see `CLAUDE.md` § Commands) and run the full smoke suite, which now covers rooms, DMs, rich messages (reply/edit/delete/reactions) and attachments, before cutting the release.
5. `git tag -a v0.3.0 -m "..."` && `git push origin v0.3.0` (see [Cutting a release](#cutting-a-release)); leave `min_version` at 0.2.0 unless friends still on the pre-rooms build must be forced to update.
6. Rebuild and distribute clients as usual (see [First install per platform](#first-install-per-platform) for anyone not yet on the self-updater).

## Rolling out the screen share release

0.4.0 adds screen sharing on top of voice. `min_version` moves to 0.4.0: an older client is asked to update by the manifest rather than left to ignore the new frames, since it has no way to render a share at all.

1. Push `main` — this deploys once.
2. Copy `deploy/` to the host and re-run `deploy/provision-host.sh` — adds the sysctl step that lets the relay's 8 MiB socket buffers actually take (see [Screen share](#screen-share)).
3. On each desktop: share a window and a monitor, watch from another machine, check audio, pop out, go fullscreen, and (macOS) grant Screen Recording, then relaunch.
4. The proto changed — regenerate the smoke suite's `vorcall_pb2.py` (see `CLAUDE.md` § Commands) and run the full smoke suite, which now covers screen share too, before cutting the release.
5. `git tag -a v0.4.0 -m "..."`, then `scripts/push-client.sh` (builds and pushes the Linux and Windows binaries to the host), then `git push origin v0.4.0` (see [Cutting a release](#cutting-a-release)).
6. Rebuild and distribute clients as usual (see [First install per platform](#first-install-per-platform) for anyone not yet on the self-updater).

## Development gates

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test -p vorcall-voice -p vorcall-core -p vorcall-release -p vorcall-app -p vorcall-hotkey -p vorcall-screen
```

```
dotnet build server/Vorcall.Server.csproj -warnaserror
```

There are no automated tests in the MVP outside `vorcall-voice`, `vorcall-core`, `vorcall-release`, `vorcall-app`, `vorcall-hotkey` and `vorcall-screen`'s unit tests. `cargo tree -i aws-lc-rs` (run from `client/`) must report no match — the Windows cross build depends on `aws-lc-rs` staying out of the dependency graph. `cargo xwin clippy --target x86_64-pc-windows-msvc -p vorcall-screen` (from `client/`) checks the Windows capture backend from Linux; the macOS backend compiles only on `release.yml`'s macOS runner, so a dry-run dispatch (see [Cutting a release](#cutting-a-release)) is the only way to check it without a Mac.
