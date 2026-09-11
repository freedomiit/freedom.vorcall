# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Vorcall: a private chat for a friend group, invite-only accounts, rooms (a mandatory `general` plus public rooms anyone can create or join) and two-person DMs, each with a presence sidebar and a voice channel (push-to-talk or voice activation, with echo cancellation and noise suppression on the microphone, over a UDP media relay). Native Rust desktop client (iced), ASP.NET Core (.NET 10) backend, protobuf frames over a single WebSocket. MVP scope only — see `README.md` for what exists and what deliberately doesn't.

## Structure

- `proto/vorcall.proto` — shared schema. Both sides generate code from it at build time.
- `PROTOCOL.md` — wire framing, connection/session state machines, limits. Read it before touching `server/Chat/` or `client/crates/vorcall-core/`. Read `PROTOCOL.md` § Voice before touching `server/Voice/`, `client/crates/vorcall-voice/` or the voice parts of `ConnectionRegistry.cs`.
- `server/` — ASP.NET Core backend, `Vorcall.Server.csproj`. `server/Dockerfile` builds with the **repository root** as context (it needs `proto/`). `server/Auth/` — accounts, tokens, invites. `server/Api/` — REST endpoints, including `AttachmentsEndpoints.cs`. `server/Cli/` — the `invites`/`users` admin subcommands. `server/ServiceSetup.cs` — DI/middleware wiring. `server/Chat/` — `RoomDirectory` (persisted rooms and memberships), `RoomNames` (name/slug grammar), `Mentions` (`<@id>` token extraction), besides the connection registry and message service. `server/Voice/` — the voice relay: `VoiceOptions` (config), `VoiceRelay` (the UDP hosted service), `VoiceSession`, `MediaHeader`, `ReplayWindow`, `TokenBucket`. `server/Attachments/` — `AttachmentsOptions` (config), `AttachmentStore` (streaming upload/download, on-disk paths), `AttachmentSweeper` (the unlinked-upload sweep).
- `client/` — Cargo workspace: `crates/vorcall-proto` (generated protobuf code), `crates/vorcall-core` (connection/protocol logic, including `auth.rs`, `session.rs`, `http.rs`, `attachments.rs` (upload/download REST calls, magic-number sniffing), `mentions.rs`, and `src/update/` — the updater: `version.rs`, `keys.rs`, `manifest.rs`, `hash.rs`, `client.rs`, `swap.rs`, `pending.rs`), `crates/vorcall-voice` (the media engine: packet framing, AEAD, Opus, jitter buffer, UDP engine, and `cleanup.rs`, the `aec3` echo-cancellation/noise-suppression/auto-gain chain — no cpal/rodio/iced, so it builds and tests without an audio device or a GUI), `crates/vorcall-hotkey` (system-wide push-to-talk input listener, no iced — per-platform backends for Windows/macOS/Linux X11/Wayland, with a window-focused fallback when global capture is unavailable), `crates/vorcall-probe` (headless voice probe binary, `vorcall-probe`; the runtime oracle for the voice path — see README § Voice; also carries the `check-update`/`apply-update` subcommands, the runtime oracle for the updater), `crates/vorcall-release` (release-side tool, binary `vorcall-release`: `gen-key`/`manifest`/`sign`/`verify`, driven by `.github/workflows/release.yml`), `crates/vorcall-app` (iced GUI, binary `vorcall`; `src/voice.rs` is the audio thread that owns the cpal/rodio devices; `src/images.rs` is the attachment cache and off-UI-thread decode/downscale; `src/view/` holds `rooms.rs` (room list pane), `message.rs` (message rendering, hover actions, reactions) and `composer.rs` (the message input, attachment picker and drag-and-drop)).
- `client/update-keys.pub` — Ed25519 public keys the client trusts to verify `manifest.json`, baked in at compile time; empty disables the updater.
- `releases/` — gitignored, repo-root local directory the dev server serves under `/api/updates/*` (`Vorcall:ReleasesDir` = `../releases` in `appsettings.Development.json`); in production it is `docker-compose.prod.yml`'s read-only bind mount, filled by `.github/workflows/release.yml`.
- `attachments/` — gitignored, repo-root local directory the dev server serves uploads from (`Vorcall:AttachmentsDir` = `../attachments` in `appsettings.Development.json`); in production it is `docker-compose.prod.yml`'s read-write bind mount, created by `deploy/provision-host.sh`.
- `deploy/` — nginx site configs and `provision-host.sh` (runs on the production host; also opens the voice UDP port on the host firewall).
- `scripts/` — client release build scripts (`build-client-linux.sh`, `build-client-windows.sh`, local fallbacks now that CI also builds and publishes releases), the first-install packagers `release.yml` runs (`package-client-linux.sh` → `vorcall-linux-x86_64.tar.gz`; `bundle-client-macos.sh`, macOS only → `Vorcall.app` and the drag-to-Applications `vorcall-macos-aarch64.dmg`; `make-windows-icon.sh` → the `vorcall.ico` the Windows installer embeds) and `update-oracle.sh` (the runtime oracle for the updater, against a local server).
- `packaging/linux/` — the launcher entry and the per-user `install.sh` the tarball carries; `packaging/windows/vorcall.iss` — the Inno Setup script CI compiles into `vorcall-windows-x86_64-setup.exe`.
- `.github/workflows/ci.yml` — format/lint/build on push and PRs, visibility only. `.github/workflows/deploy.yml` — build + deploy on push to `main`, does not wait on CI. `.github/workflows/release.yml` — builds the client for all three platforms, signs a manifest and (on a `v*` tag, or a checked `publish` dispatch) publishes it to the host's `releases/`.
- `server/Updates/` (`UpdatesOptions`, `UpdateManifestStore`) and `server/Api/UpdatesEndpoints.cs` — serve the signed manifest and the release assets under `/api/updates/*`.

## Commands

Local dev:

```
docker compose up -d db
~/.dotnet/dotnet run --project server/Vorcall.Server.csproj      # http://localhost:5000
cd client && VORCALL_SERVER_URL=http://localhost:5000 cargo run -p vorcall-app
```

Admin CLI (invites, users — see `README.md` § Admin CLI):

```
~/.dotnet/dotnet run --project server/Vorcall.Server.csproj -- invites new
```

Smoke suite (lives outside the repo at `~/.cache/vorcall-smoke`, is the runtime oracle since there are no repo tests; regenerate `vorcall_pb2.py` with grpcio-tools after any proto change; it seeds rows into the local DB; it is voice-aware — it skips voice frames unless a step expects them; it also covers rooms, DMs, rich messages (reply/edit/delete/reactions) and attachment upload/download):

```
uvx --with websockets --with protobuf python ~/.cache/vorcall-smoke/smoke.py full --http http://localhost:5000 --ws ws://localhost:5000/ws --key dev --invites A,B,C
```

Voice probe (two terminals; the runtime oracle for the voice path — see README § Voice for the full recipe, exit codes and JSON fields):

```
cd client && cargo build -p vorcall-probe
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --send-seconds 10 --listen-seconds 14 --expect-peer
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username bob --send-seconds 10 --listen-seconds 14 --expect-peer --tone-hz 660
```

Voice-activation oracle: one probe proves the gate holds silence back; two probes prove the gate lets a tone through.

```
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --send-seconds 4 --listen-seconds 5 --vad --tone-amplitude 0 --expect-silence
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --send-seconds 10 --listen-seconds 14 --vad --tone-amplitude 0.3 --expect-peer
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username bob --send-seconds 10 --listen-seconds 14 --vad --tone-amplitude 0.3 --expect-peer --tone-hz 660
```

Update oracle (runtime oracle for the updater, against a local server; needs an existing account):

```
VORCALL_PROBE_USER=alice VORCALL_PROBE_PASSWORD=... scripts/update-oracle.sh --http http://localhost:5000
```

`vorcall --version` prints `vorcall <version> <platform>` and exits before endpoint resolution and iced ever start — the release workflow uses it to check a build against the tag.

Gates (must pass before considering a change done):

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test -p vorcall-voice -p vorcall-core -p vorcall-release -p vorcall-app -p vorcall-hotkey
dotnet build server/Vorcall.Server.csproj -warnaserror
```

No automated tests exist in the MVP outside `vorcall-voice`, `vorcall-core`, `vorcall-release`, `vorcall-app` and `vorcall-hotkey`'s unit tests.

## Gotchas

- **`client/.cargo/config.toml` only applies with cwd inside `client/`.** It supplies the placeholder `VORCALL_SERVER_KEY=dev` so a bare `cargo build` works there. Running cargo from the repo root does not pick it up.
- **Do not run the GUI binary (`vorcall`, `cargo run -p vorcall-app`).** It opens an iced window on the owner's desktop. Use `cargo build` / `cargo check` / `cargo clippy` to verify the client instead.
- **A push to `main` deploys production immediately.** `.github/workflows/deploy.yml` builds and ships to `vorcall.example.com` on every push to `main`, with no separate approval gate. Do not push to `main` casually.
- **No `aws-lc-rs` in the client dependency graph.** Verify with `cargo tree -i aws-lc-rs` (from `client/`) — it must report no match. The Windows cross build (cargo-xwin) depends on `aws-lc-rs` staying out.
- The server Docker build context is the repository root, not `server/` — `server/Dockerfile` reads `proto/` via `COPY proto/ proto/`.
- Never add `default_server` to `deploy/nginx/vorcall.conf` — that directive is already owned by another site on the production host.
- The client key and server URL are baked in at compile time (`VORCALL_SERVER_KEY`, `VORCALL_SERVER_URL`), overridable at runtime by env vars of the same names. A key rotation therefore requires a client rebuild and redistribution, not just a server-side change.
- `dotnet` may not be on `PATH`; it can live at `~/.dotnet/dotnet`. `dotnet-ef` needs `DOTNET_ROOT=~/.dotnet` and `dotnet` on `PATH`.
- ALSA headers are needed for any client build or clippy run on Linux — rodio/cpal won't compile without them (`alsa-lib-devel` on Fedora, `libasound2-dev` on Debian/Ubuntu).
- `Vorcall:JwtSigningKey` (env `Vorcall__JwtSigningKey`) is required; the server refuses to boot without it. `appsettings.Development.json` carries a dev-only value, so local runs need nothing extra.
- The CLI branch in `Program.cs` (`invites`/`users` as the first argument) never starts Kestrel — it runs migrations and exits. Don't expect the web server to come up under those subcommands.
- Never log tokens, passwords, invite codes, password hashes or the JWT signing key.
- Never run `dotnet build` from two agents at once — MSBuild has no target-dir lock, so concurrent builds of the same project corrupt each other's output.
- `dotnet ef migrations add` runs `Program.cs` at design time with empty `args`, so the CLI branch above must not trigger on an empty argument list.
- Forwarded headers (`UseForwardedHeaders`) trust only the immediate hop (`ForwardLimit = 1`) because the backend is published on host loopback behind nginx, not reachable directly.
- `UseRateLimiter` runs after `UseAuthentication` in `Program.cs` on purpose: the upload rate-limit policy (`AttachmentsEndpoints.RateLimitPolicy`) partitions on the bearer's user id, which `UseAuthentication` must have already resolved.
- `general` and a DM can never be left (`LeaveRoom` answers `ERROR_CODE_FORBIDDEN` for both); only a plain public room can be left.
- The Hello sequence is `Welcome`, then `RoomState`+`VoiceState` per room the account belongs to, then `RoomList`, then `MemberJoined` broadcasts — `RoomState` for a room always arrives before that room's entry in `RoomList`. The client handles this by creating a placeholder `Room` (see `placeholder_room` in `client/crates/vorcall-app/src/app.rs`) the first time it sees a `RoomState` for a room id it does not know yet, replaced once `RoomList` describes it.
- An attachment upload is a raw body, not protobuf, so nginx needs its own `location /api/attachments` with `client_max_body_size 9m` and unbuffered proxying — a plain `/api/` proxy would apply nginx's own default `client_max_body_size` of 1 MiB and buffer the whole request before forwarding it; the endpoint itself already raises Kestrel's limit (`server/Api/AttachmentsEndpoints.cs`, `IHttpMaxRequestBodySizeFeature`).
- The attachment sweeper deletes an upload's row and file together once it has sat unlinked to any message for 1 hour; a client must send the `SendMessage` that references an attachment id well within that window.
- `Command`/`Event` in `connection.rs` derive `Debug` and carry attachment payloads as `Arc<Vec<u8>>` — never log one with `{:?}`, it dumps the raw image bytes.
- The client's image cache (`client/crates/vorcall-app/src/images.rs`) lives under the platform cache directory, is pruned to 500 MiB at startup by oldest modification time, and all decoding happens off the UI thread.
- `MarkRead` is client-debounced to one call per second per room, and only sent for the room currently in view, scrolled to the bottom, in a focused window — see `schedule_mark_read`/`flush_mark_read` in `app.rs`.
- `VORCALL_TEXT_REACTIONS=1` renders the reaction palette as text labels instead of emoji, in `client/crates/vorcall-app/src/view/message.rs`.
- The message list uses iced `Anchor::End`; under that anchor a relative offset of 0 means the bottom of the list, not the top.
- notify-rust's `show()` blocks on D-Bus on Linux — call it off the UI thread — and on macOS the notification is delivered when the returned handle is dropped, not when `show()` returns.
- rodio's `MixerDeviceSink` must stay alive in `App`; dropping it silences the mixer.
- The relay publishes UDP 5005 on `0.0.0.0` in production on purpose; nginx cannot proxy it; the OCI VCN security list is what actually gates it.
- `opus-rs` is a pure-Rust libopus port used in CELT-only mode; do not switch to hybrid/SILK modes (known decoder panics); decode is wrapped in a panic guard; the `[profile.dev.package.*] opt-level = 3` blocks keep debug builds from underrunning.
- `aec3` (pinned `=0.3.2`, pure Rust) is WebRTC's AEC3 + noise suppressor + AGC2 behind `vorcall-voice::cleanup`: 10 ms blocks (two per frame); `InputCleanup` is not `Send`, so the audio thread builds and drops it in place; AEC3 is always in the pipeline and is fed silence when echo cancellation is off; its metrics sink is a capacity-one, latest-only queue, pulled once per block.
- The cleanup chain runs on every captured frame before the gate and the meter, in both transmit modes and while muted (the canceller must stay converged); `voice.rs` guards it with `catch_unwind` like the codec and sends the raw frame for the rest of the session after a panic or error — toggling a switch or rejoining retries.
- The canceller's reference is what `VoiceSource` pulled from `Playout` (zeros while deafened), through a 200 ms ring shared with the audio thread; neither other applications' audio nor the notification chime (its own sink in `audio.rs`) is ever cancelled.
- The cleanup unit tests use synthetic signals with thresholds taken from measurements (echo ≥ 12 dB removed at 100 and 250 ms with the delay estimate within 30 ms, white noise ≥ 6 dB, AGC lift 8–30 dB at −45 dBFS and ≤ 4 dB at −25 dBFS, everything off within 2.5 dB — each a floor under a measured value, never to be loosened); a pure tone is not speech to AGC2's VAD, so the gain tests use the speech-like generator, not `Tone`.
- Adding a `ServerFrame` payload requires arms in **both** dispatchers of `connection.rs` (`await_welcome` and `live_loop`), otherwise a known-but-unhandled payload reconnects the client.
- `VoiceReady.host` empty means the WebSocket host; the client uses `Endpoints::host()`.
- Never log `VoiceReady` frames or `MediaKey`/`VoiceSession.Key`; `connection.rs` logs frames through a redacting `describe()`.
- The audio thread (`voice.rs`) owns cpal/rodio objects; never open devices on the UI thread.
- The macOS Input Monitoring grant for global push-to-talk is tied to the specific (ad-hoc signed) build, so it must be re-granted after every update; Caps Lock also cannot be a global PTT key there — it arrives as a modifier-flag change, not a key event.
- The Wayland `GlobalShortcuts` portal is keyboard-only (no mouse buttons); `WAYLAND_DISPLAY` set selects the portal backend, otherwise `DISPLAY` selects X11.
- `Listener::start` in `vorcall-hotkey` blocks (up to 10 s on Wayland, waiting on the portal dialog), so the app always calls it via `spawn_blocking`, never on the UI thread.
- Hook and event-tap callbacks in `vorcall-hotkey` must stay trivial — the OS drops a slow hook (Windows) or tap (macOS).
- The voice-activation gate runs on every 20 ms frame in both transmit modes, because the input level meter depends on it; only voice-activation mode lets the gate force the talk-spurt marker.
- Per-peer volume/mute in `Playout` is keyed by ssrc; the app re-applies a user's saved setting whenever their ssrc changes (e.g. on rejoin).
- `client/.env.probe` (gitignored) holds the production probe credentials.
- `pkill -f vorcall-probe` from a shell whose own command line contains that string kills the shell; use `pkill -x vorcall-probe`.
- The repo is private, so friends' clients fetch updates from the backend (`/api/updates/*`, door key + bearer), never from GitHub Releases.
- The manifest signature covers the exact bytes of `manifest.json` — never re-serialise it. The server serves the stored bytes verbatim (`GetManifest` in `UpdatesEndpoints.cs`) and `vorcall-release sign` signs the file's bytes, not a re-encoded value.
- `client/update-keys.pub` empty ⇒ the updater is disabled (`disabled_reason` returns "no update keys baked in").
- The signing private key exists only in the GitHub secret `VORCALL_UPDATE_SIGNING_KEY` and the owner's password manager — never in the repo or on the host.
- Update crypto is ring-only (no `sha2`, no `ed25519-dalek`); `serde_json` is the only dependency the updater added to the workspace.
- Windows swap moves the running binary to `vorcall.exe.old` and relaunches into the new one; `.old` is deleted at the *next* start (the process exiting now may still be holding it).
- Unix swap renames the download over the running binary and execs it — safe because the kernel keeps the old inode alive for the process using it.
- `ubuntu-22.04` in `release.yml` is the glibc floor for the Linux binary — do not bump it casually.
- First installs are the packages, never the bare binaries: `vorcall-linux-x86_64.tar.gz` (per-user `install.sh` into `~/.local`), `vorcall-windows-x86_64-setup.exe` (per-user into `%LocalAppData%\Programs\Vorcall` on purpose: Program Files would put the updater into its manual fallback) and `vorcall-macos-aarch64.dmg` (Finder opens a bare Mach-O download in TextEdit). The manifest lists only the three bare binaries, which the updater renames over whatever the package installed; the packages never go to the host.
- `packaging/linux/vorcall.desktop` keeps that file name and `StartupWMClass=vorcall` because they match iced's `application_id`; that pairing is how the launcher gives the window its icon.
- `packaging/linux/install.sh` touches `~/.local/share/icons/hicolor` and rebuilds an `icon-theme.cache` that already sits there. GNOME re-reads a theme only when that directory's own mtime changes, and trusts an existing cache that is at least as new as the directory, so without both steps a cache left by another app hides the icon after a first install. It never creates a cache, which would lay the same trap for other installers.
- The macOS bundle's `Info.plist` must keep `NSMicrophoneUsageDescription`, or macOS kills the app on the first push-to-talk. `packaging/windows/vorcall.iss`'s `AppId` GUID must never change: it is how Windows recognises an upgrade.
- `VORCALL_NO_UPDATE` disables the updater at runtime; a dev-key build or a debug build never updates either.
- `VORCALL_RELAUNCHED=1` is set by the swapper on the process it just started; `vorcall-probe` prints a `{"relaunched": true, ...}` marker and exits when it sees it, which is what the oracle's swap case checks for.
- `client/crates/vorcall-app/src/brand/data.rs` is generated by `assets/brand/gen.py` — regenerate with `uvx --with shapely python3 assets/brand/gen.py assets/brand`; the SVGs it reads must come out byte-identical.
- `users outdated` needs either `--min` or a manifest already on disk at `Vorcall:ReleasesDir` — with neither, it fails and says so.
- `min_version` lives in `client/Cargo.toml`'s `[workspace.metadata.vorcall]`, not next to `version` in `[workspace.package]`.
- `release.yml` refuses to publish unless the tag equals the workspace `version` (tag push, or dispatch with `publish` checked); a dry-run dispatch needs no tag.
- nginx has a dedicated `location /api/updates/` (no buffering, 300 s timeouts — a release binary is a large streamed download) — applied to the host by re-running `deploy/provision-host.sh`.
- After a proto change, regenerate the smoke suite's pb2: `uvx --from grpcio-tools python -m grpc_tools.protoc -I proto --python_out=$HOME/.cache/vorcall-smoke proto/vorcall.proto`.

## Conventions

- English only — code, comments, docs, commits.
- Conventional Commits (`feat:`, `fix:`, `chore:`, ...).
- Comments only where the "why" is non-obvious (an external constraint, a spec citation, a workaround's cause) — not restating what the code already shows.
- No new dependencies without discussion first.
- Never hand-edit generated protobuf code. To change the schema, edit `proto/vorcall.proto` and rebuild both sides (`cargo build` in `client/`, `dotnet build` in `server/`) so the generated code regenerates.
