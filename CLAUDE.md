# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Vorcall: a private chat for a friend group, invite-only accounts, one text room (`general`) with a presence sidebar. Native Rust desktop client (iced), ASP.NET Core (.NET 10) backend, protobuf frames over a single WebSocket. MVP scope only — see `README.md` for what exists and what deliberately doesn't.

## Structure

- `proto/vorcall.proto` — shared schema. Both sides generate code from it at build time.
- `PROTOCOL.md` — wire framing, connection/session state machines, limits. Read it before touching `server/Chat/` or `client/crates/vorcall-core/`.
- `server/` — ASP.NET Core backend, `Vorcall.Server.csproj`. `server/Dockerfile` builds with the **repository root** as context (it needs `proto/`). `server/Auth/` — accounts, tokens, invites. `server/Api/` — REST endpoints. `server/Cli/` — the `invites`/`users` admin subcommands. `server/ServiceSetup.cs` — DI/middleware wiring.
- `client/` — Cargo workspace: `crates/vorcall-proto` (generated protobuf code), `crates/vorcall-core` (connection/protocol logic, including `auth.rs`, `session.rs`, `http.rs`), `crates/vorcall-app` (iced GUI, binary `vorcall`).
- `deploy/` — nginx site configs and `provision-host.sh` (runs on the production host).
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

Smoke suite (lives outside the repo, is the runtime oracle since there are no repo tests; regenerate `vorcall_pb2.py` with grpcio-tools after any proto change; it seeds rows into the local DB):

```
uvx --with websockets --with protobuf python ~/.cache/vorcall-smoke/smoke.py full --http http://localhost:5100 --ws ws://localhost:5100/ws --key dev --invites A,B,C
```

Gates (must pass before considering a change done):

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build
dotnet build server/Vorcall.Server.csproj -warnaserror
```

No automated tests exist in the MVP.

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

## Conventions

- English only — code, comments, docs, commits.
- Conventional Commits (`feat:`, `fix:`, `chore:`, ...).
- Comments only where the "why" is non-obvious (an external constraint, a spec citation, a workaround's cause) — not restating what the code already shows.
- No new dependencies without discussion first.
- Never hand-edit generated protobuf code. To change the schema, edit `proto/vorcall.proto` and rebuild both sides (`cargo build` in `client/`, `dotnet build` in `server/`) so the generated code regenerates.
