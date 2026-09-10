# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Vorcall: a private chat for a friend group, invite-only accounts, one text room (`general`) with a presence sidebar and a voice channel (push-to-talk, over a UDP media relay). Native Rust desktop client (iced), ASP.NET Core (.NET 10) backend, protobuf frames over a single WebSocket. MVP scope only — see `README.md` for what exists and what deliberately doesn't.

## Structure

- `proto/vorcall.proto` — shared schema. Both sides generate code from it at build time.
- `PROTOCOL.md` — wire framing, connection/session state machines, limits. Read it before touching `server/Chat/` or `client/crates/vorcall-core/`. Read `PROTOCOL.md` § Voice before touching `server/Voice/`, `client/crates/vorcall-voice/` or the voice parts of `ConnectionRegistry.cs`.
- `server/` — ASP.NET Core backend, `Vorcall.Server.csproj`. `server/Dockerfile` builds with the **repository root** as context (it needs `proto/`). `server/Auth/` — accounts, tokens, invites. `server/Api/` — REST endpoints. `server/Cli/` — the `invites`/`users` admin subcommands. `server/ServiceSetup.cs` — DI/middleware wiring. `server/Voice/` — the voice relay: `VoiceOptions` (config), `VoiceRelay` (the UDP hosted service), `VoiceSession`, `MediaHeader`, `ReplayWindow`, `TokenBucket`.
- `client/` — Cargo workspace: `crates/vorcall-proto` (generated protobuf code), `crates/vorcall-core` (connection/protocol logic, including `auth.rs`, `session.rs`, `http.rs`), `crates/vorcall-voice` (the media engine: packet framing, AEAD, Opus, jitter buffer, UDP engine — no cpal/rodio/iced, so it builds and tests without an audio device or a GUI), `crates/vorcall-probe` (headless voice probe binary, `vorcall-probe`; the runtime oracle for the voice path — see README § Voice), `crates/vorcall-app` (iced GUI, binary `vorcall`; `src/voice.rs` is the audio thread that owns the cpal/rodio devices).
- `deploy/` — nginx site configs and `provision-host.sh` (runs on the production host; also opens the voice UDP port on the host firewall).
- `scripts/` — client release builds (`build-client-linux.sh`, `build-client-windows.sh`).
- `.github/workflows/ci.yml` — format/lint/build on push and PRs, visibility only. `.github/workflows/deploy.yml` — build + deploy on push to `main`, does not wait on CI.

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

Smoke suite (lives outside the repo at `~/.cache/vorcall-smoke`, is the runtime oracle since there are no repo tests; regenerate `vorcall_pb2.py` with grpcio-tools after any proto change; it seeds rows into the local DB; it is voice-aware — it skips voice frames unless a step expects them):

```
uvx --with websockets --with protobuf python ~/.cache/vorcall-smoke/smoke.py full --http http://localhost:5000 --ws ws://localhost:5000/ws --key dev --invites A,B,C
```

Voice probe (two terminals; the runtime oracle for the voice path — see README § Voice for the full recipe, exit codes and JSON fields):

```
cd client && cargo build -p vorcall-probe
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username alice --send-seconds 10 --listen-seconds 14 --expect-peer
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=... ./target/debug/vorcall-probe --username bob --send-seconds 10 --listen-seconds 14 --expect-peer --tone-hz 660
```

Gates (must pass before considering a change done):

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test -p vorcall-voice
dotnet build server/Vorcall.Server.csproj -warnaserror
```

No automated tests exist in the MVP outside `vorcall-voice`'s unit tests (`cargo test -p vorcall-voice`).

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
- The message list uses iced `Anchor::End`; under that anchor a relative offset of 0 means the bottom of the list, not the top.
- notify-rust's `show()` blocks on D-Bus on Linux — call it off the UI thread — and on macOS the notification is delivered when the returned handle is dropped, not when `show()` returns.
- rodio's `MixerDeviceSink` must stay alive in `App`; dropping it silences the mixer.
- The relay publishes UDP 5005 on `0.0.0.0` in production on purpose; nginx cannot proxy it; the OCI VCN security list is what actually gates it.
- `opus-rs` is a pure-Rust libopus port used in CELT-only mode; do not switch to hybrid/SILK modes (known decoder panics); decode is wrapped in a panic guard; the `[profile.dev.package.*] opt-level = 3` blocks keep debug builds from underrunning.
- Adding a `ServerFrame` payload requires arms in **both** dispatchers of `connection.rs` (`await_welcome` and `live_loop`), otherwise a known-but-unhandled payload reconnects the client.
- `VoiceReady.host` empty means the WebSocket host; the client uses `Endpoints::host()`.
- Never log `VoiceReady` frames or `MediaKey`/`VoiceSession.Key`; `connection.rs` logs frames through a redacting `describe()`.
- The audio thread (`voice.rs`) owns cpal/rodio objects; never open devices on the UI thread.
- `client/.env.probe` (gitignored) holds the production probe credentials.
- `pkill -f vorcall-probe` from a shell whose own command line contains that string kills the shell; use `pkill -x vorcall-probe`.

## Conventions

- English only — code, comments, docs, commits.
- Conventional Commits (`feat:`, `fix:`, `chore:`, ...).
- Comments only where the "why" is non-obvious (an external constraint, a spec citation, a workaround's cause) — not restating what the code already shows.
- No new dependencies without discussion first.
- Never hand-edit generated protobuf code. To change the schema, edit `proto/vorcall.proto` and rebuild both sides (`cargo build` in `client/`, `dotnet build` in `server/`) so the generated code regenerates.
