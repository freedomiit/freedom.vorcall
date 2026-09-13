# Development

Building Vorcall from source, running it locally, and the gates a change has to pass.

- [Prerequisites](#prerequisites)
- [Running locally](#running-locally)
- [Building the client](#building-the-client)
- [Tests and gates](#tests-and-gates)
- [Runtime oracles](#runtime-oracles)
- [Repository layout](#repository-layout)
- [Working on the protocol](#working-on-the-protocol)

---

## Prerequisites

**Linux** — everything builds here, including the Windows cross-build.

```bash
# Fedora
sudo dnf install -y alsa-lib-devel pipewire-devel clang-devel gcc-c++

# Debian / Ubuntu
sudo apt install -y libasound2-dev libpipewire-0.3-dev libclang-dev g++
```

Plus [rustup](https://rustup.rs) (stable, 1.89+ — the floor comes from notify-rust), the
[.NET 10 SDK](https://dotnet.microsoft.com/download) and Docker for the local PostgreSQL.

Why each native dependency: ALSA headers for cpal/rodio (capture and playback), PipeWire
headers and libclang for the screen-capture bindings, a C++ compiler for OpenH264, which
the `cc` crate builds from vendored sources — no cmake, no nasm. Opus is a pure-Rust port,
so it adds nothing. `vorcall-clipboard` adds nothing either: its Wayland backend `dlopen`s
`libwayland-client` and its X11 backend goes through x11rb, so neither wants a header at
build time.

**macOS** — Xcode command-line tools and rustup. macOS 13 or newer to run the result;
screen capture uses ScreenCaptureKit.

**Windows cross-build from Linux** — additionally:

```bash
sudo dnf install -y clang lld llvm
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
```

> `dotnet` may not be on your `PATH`; it is often at `~/.dotnet/dotnet`. The `dotnet-ef`
> tool needs `DOTNET_ROOT=~/.dotnet` and `dotnet` on `PATH`.

---

## Running locally

**Database** — the compose file's `db` service on its own:

```bash
docker compose up -d db
```

**Server** — from the repository root. Listens on `http://localhost:5000` in the
`Development` environment, with door key `dev` and the connection string from
`server/appsettings.Development.json`:

```bash
dotnet run --project server/Vorcall.Server.csproj
```

**Client** — from `client/`, so `client/.cargo/config.toml` supplies the placeholder key:

```bash
cd client
VORCALL_SERVER_URL=http://localhost:5000 cargo run -p vorcall-app
```

Then make an invite and register:

```bash
dotnet run --project server/Vorcall.Server.csproj -- invites new
```

The dev server writes JSON logs to `logs/` at the repository root, uploads to
`attachments/`, and problem reports to `diagnostics/` — all gitignored. `curl -s
localhost:5000/metrics` prints the Prometheus exposition, and `Vorcall:AdminKey` is
`dev-admin` so the admin CLI's live-kick path works locally.

The client always writes a debug-level `vorcall.log` next to its `config.toml`
(`~/.config/vorcall/` on Linux). `RUST_LOG` filters what reaches stderr, `RUST_LOG_FILE`
what reaches the file.

### Where the client keeps its state

| | Path |
| --- | --- |
| Linux | `~/.config/vorcall/` |
| Windows | `%APPDATA%\freedomit\vorcall\config\` |
| macOS | `~/Library/Application Support/br.com.freedomit.vorcall/` |

`config.toml` holds preferences, including the saved server address and door key.
`session.toml` (mode `0600` on Unix) holds the tokens. Messages are never written to disk.
Custom themes are `themes/<slug>.json` beside them.

---

## Building the client

The door key and server URL are compiled in and can be overridden at runtime by environment
variables of the same names. Since 0.5.2 they can also be set from inside the app, so a
build made for one server can be pointed at another without recompiling.

```bash
scripts/build-client-linux.sh     # dist/vorcall-linux-x86_64 and the install tarball
scripts/build-client-windows.sh   # dist/vorcall-windows-x86_64.exe, cross-built
```

Both scripts source `client/.env.release` if it exists (gitignored, `export
VORCALL_SERVER_KEY=…`) and refuse to build with an unset or `dev` key. For a one-off build
that is not a release, a plain `cargo build --release -p vorcall-app` from inside `client/`
is enough.

On macOS, `scripts/bundle-client-macos.sh` wraps a built binary into `Vorcall.app` and a
drag-to-Applications DMG the same way CI does; it needs `brew install librsvg` for the icon.

`vorcall --version` prints `vorcall <version> <platform>` and exits before any window opens.

> **Do not run the GUI binary from an agent or a headless shell** — it opens a window on the
> desktop. Use `cargo build`, `cargo check` or `cargo clippy` to verify client changes.

---

## Tests and gates

Everything below must pass before a change is done. This is exactly what CI runs.

```bash
# Client: format, lint, build, unit tests across all seven library crates
cd client
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test -p vorcall-voice -p vorcall-core -p vorcall-release \
           -p vorcall-app -p vorcall-hotkey -p vorcall-screen \
           -p vorcall-clipboard
cd ..

# Server: build with warnings as errors, then the full suite
dotnet build server/Vorcall.Server.csproj -warnaserror
docker compose up -d db && dotnet test tests/Vorcall.Server.Tests
```

The server suite is the primary runtime oracle for the chat and REST surface. It has pure
unit tests (the permission matrix, the name grammar, validation) plus an integration suite
that boots the backend in-process against the compose PostgreSQL, speaking the real
protocol with the server's own generated protobuf types. Each run creates and drops a
`vorcall_test_<hex>` database of its own and never touches the development one.

`VORCALL_TEST_ADMIN` overrides the admin connection string; it defaults to
`Host=localhost;Port=5433;Username=vorcall;Password=vorcall;Database=postgres`.

Two platform-specific checks cannot run on Linux: the Windows capture backend is covered by
`cargo xwin clippy --target x86_64-pc-windows-msvc -p vorcall-screen`, and the macOS backend
compiles only on a macOS runner.

### What has no automated test

The voice and screen-share media paths. They are covered by runtime oracles instead.

The soundpad is different: it has unit tests on both sides (mixing, the container parser,
playback state) but no runtime oracle. Playing a clip never touches the media relay — it is
a broadcast frame plus local playback — so `vorcall-probe` has nothing new to prove.

---

## Runtime oracles

`vorcall-probe` is a headless client that exercises the media paths and the updater. Build
it once with `cd client && cargo build -p vorcall-probe`.

**Voice** — two terminals, against a local server with two existing accounts:

```bash
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=… \
  ./target/debug/vorcall-probe --username alice --send-seconds 10 --listen-seconds 14 --expect-peer
VORCALL_SERVER_URL=http://localhost:5000 VORCALL_PROBE_PASSWORD=… \
  ./target/debug/vorcall-probe --username bob --send-seconds 10 --listen-seconds 14 --expect-peer --tone-hz 660
```

With neither `--channel` nor `--channel-name` it joins the voice channel called `General`.

**Voice activation** — one probe proves the gate holds silence back, two prove it lets a
tone through:

```bash
… --username alice --send-seconds 4  --listen-seconds 5  --vad --tone-amplitude 0   --expect-silence
… --username alice --send-seconds 10 --listen-seconds 14 --vad --tone-amplitude 0.3 --expect-peer
… --username bob   --send-seconds 10 --listen-seconds 14 --vad --tone-amplitude 0.3 --expect-peer --tone-hz 660
```

**Screen share** — one shares, one watches:

```bash
… --username alice --share-seconds 10 --share-audio --listen-seconds 14
… --username bob --watch alice --expect-video --expect-share-audio --listen-seconds 14
```

**Capture alone**, needing no server at all — opens the portal picker on Linux, runs
ScreenCaptureKit on macOS. Exit 0 means capture ran and the stop path completed:

```bash
./target/debug/vorcall-probe --capture-seconds 5 --capture-audio
```

**The updater** — drives `check-update` and `apply-update` end to end against a local
server, using an account that already exists:

```bash
VORCALL_PROBE_USER=alice VORCALL_PROBE_PASSWORD=… scripts/update-oracle.sh --http http://localhost:5000
```

---

## Repository layout

```
proto/vorcall.proto     the shared schema; both sides generate from it at build time
PROTOCOL.md             wire framing, state machines, permission resolution, limits

server/                 ASP.NET Core (.NET 10)
  Permissions/            the pure permission engine — no database, no wire
  Chat/                   the in-memory mirror of the server, and the directories behind it
  Voice/                  the UDP media relay
  Attachments/            uploads, images and the shared soundpad's clip store
  Api/                    REST endpoints
  Auth/                   accounts, tokens, invites, the door key
  Data/                   entities, DbContext, seeding
  Cli/                    the invites/users/server subcommands

client/                 Cargo workspace
  vorcall-proto           generated protobuf code
  vorcall-core            connection and protocol logic, the permission mirror, the updater
  vorcall-voice           media engine: framing, AEAD, Opus, jitter buffer, echo cancellation,
                          the soundpad's clip codec and sound-effect mixing
  vorcall-screen          screen capture backends and the OpenH264 codec
  vorcall-hotkey          system-wide input listener for push-to-talk and friends
  vorcall-clipboard       reading files and images off the clipboard, one backend per platform
  vorcall-app             the iced GUI, binary `vorcall`
  vorcall-probe           headless oracle for voice, share and updates
  vorcall-release         release signing tool

tests/                  Vorcall.Server.Tests (xunit) — outside server/ on purpose, so the
                        SDK's compile glob does not swallow it into the server project
deploy/                 nginx configs and host provisioning
scripts/                client build, packaging and release scripts
assets/                 brand geometry and the generated UI icon set
```

---

## Working on the protocol

Both sides generate their code from `proto/vorcall.proto` at build time. **Never hand-edit
generated code.** Change the schema, then rebuild both sides — `cargo build` in `client/`,
`dotnet build` in `server/` — so the generated code regenerates.

Read [`PROTOCOL.md`](../PROTOCOL.md) before touching `server/Chat/`, `server/Permissions/`
or `client/crates/vorcall-core/`. Its § Voice, § Screen share and § Media transport cover
`server/Voice/`, `client/crates/vorcall-voice/` and `client/crates/vorcall-screen/`.

Two constraints worth knowing before you start:

- **The permission engine is mirrored.** `client/crates/vorcall-core/src/permissions.rs`
  must match `server/Permissions/PermissionEngine.cs` bit for bit, and both are tested
  against the same matrix. The server is the boundary; the client mirror only hides and
  disables.
- **Adding a `ServerFrame` payload needs arms in both dispatchers** of
  `client/crates/vorcall-core/src/connection.rs` (`await_welcome` and `live_loop`).
  A known-but-unhandled payload reconnects the client.

The UI icon set is generated, not hand-drawn. Edit the geometry in `assets/brand/gen.py`
and regenerate — every SVG must come out byte-identical:

```bash
python3 assets/brand/gen.py icons assets/icons
```
