# Vorcall

A private chat for a friend group: one room, native Rust desktop client, .NET backend, protobuf over a WebSocket.

## Status

MVP. Present: one shared room, a nickname, live text messages, the last 100 messages on join.

Deliberately absent: accounts, multiple channels, presence/typing indicators, attachments, voice.

## Repository layout

```
proto/vorcall.proto        shared schema; generated into both server and client at build time
PROTOCOL.md                wire framing, state machines, limits
server/                    ASP.NET Core (.NET 10) backend, Vorcall.Server.csproj
client/                    Cargo workspace (crates/vorcall-proto, crates/vorcall-core, crates/vorcall-app -> binary `vorcall`)
deploy/                    nginx site configs and the host provisioning script
scripts/                   client release build scripts (Linux, Windows cross-build)
docker-compose.yml         local dev: Postgres only
docker-compose.prod.yml    production stack: Postgres + backend, pulled from GHCR
.env.production.example    template for the production .env (never commit the real one)
.github/workflows/         ci.yml (format/lint/build) and deploy.yml (build + deploy on push to main)
```

## Prerequisites

- **Linux (build client + server, run local dev):** Rust via rustup (stable, 1.88+ — MSRV set by iced 0.14), .NET SDK 10.0.x, Docker (for the local Postgres).
- **Cross-build for Windows (from Linux/Fedora):** the above, plus `sudo dnf install -y clang lld llvm`, `rustup target add x86_64-pc-windows-msvc`, `cargo install cargo-xwin`.
- **macOS (build client from source only):** Xcode command-line tools, rustup.

`dotnet` may not be on `PATH`; it can live at `~/.dotnet/dotnet`. The `dotnet-ef` global tool needs `DOTNET_ROOT=~/.dotnet` and `dotnet` on `PATH` (e.g. `PATH=~/.dotnet:$PATH`).

## Build the client

The server key and server URL are baked in at compile time (see [Client key and URL](#client-key-and-url) below); they can also be overridden at runtime by environment variables of the same names.

### Linux

```
scripts/build-client-linux.sh
```

Output: `dist/vorcall-linux-x86_64`.

### Windows (cross-built from Linux)

```
scripts/build-client-windows.sh
```

Output: `dist/vorcall-windows-x86_64.exe`. The binary is unsigned: the first run needs a right-click "Open".

### macOS (from source; this repo produces no macOS binaries)

```
cd client
VORCALL_SERVER_KEY=<key> cargo build --release -p vorcall-app
```

Binary: `client/target/release/vorcall`.

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

Client config on disk (only the nickname): `~/.config/vorcall/config.toml` on Linux (via `directories::ProjectDirs::from("br.com", "freedomit", "vorcall")`). The nickname can also be changed with the "Change name" button in the app.

## Protocol

See [`PROTOCOL.md`](PROTOCOL.md) for framing, connection state machines and limits, and [`proto/vorcall.proto`](proto/vorcall.proto) for the message schema. Both sides generate code from the `.proto` file at build time; never hand-edit generated code.

## Production

- Domain `vorcall.example.com` points DNS-only (no Cloudflare proxy) at the Oracle ARM64 host `user@host`, app directory `/opt/vorcall`.
- `deploy/provision-host.sh` runs on the host: installs certbot, issues the Let's Encrypt certificate, installs the nginx site (`deploy/nginx/vorcall.conf`, backend proxied from loopback `127.0.0.1:5004`), sets up the certbot renewal hook, and generates the host's `.env` from `.env.production.example` (random Postgres password and `Vorcall__ServerKey`). See the script header for the exact invocation.
- Deploy pipeline (`.github/workflows/deploy.yml`): a push to `main` builds an arm64 backend image on GitHub Actions, pushes it to `ghcr.io/freedomiit/vorcall-backend`, then copies `docker-compose.prod.yml` to the host and runs `docker compose pull && up -d` over SSH. The backend applies its own EF Core migrations on boot. A push to `main` deploys production immediately; there is no separate approval step.
- The pre-shared door key lives only in the host's `.env` (`Vorcall__ServerKey`) and must be baked into client builds as `VORCALL_SERVER_KEY`. Rotating it means: generate a new value, update the host `.env`, restart the backend, and rebuild/redistribute the client with the new key.

## Development gates

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build
```

```
dotnet build server/Vorcall.Server.csproj -warnaserror
```

There are no automated tests in the MVP. `cargo tree -i aws-lc-rs` (run from `client/`) must report no match — the Windows cross build depends on `aws-lc-rs` staying out of the dependency graph.
