# Vorcall

A private chat for a friend group: one room, native Rust desktop client, .NET backend, protobuf over a WebSocket.

## Status

MVP. Present: invite-only accounts, one room (`general`), a presence sidebar, live messages, older history, notifications, voice rooms with push-to-talk.

Deliberately absent: multiple text rooms, attachments, OAuth/2FA.

## Repository layout

```
proto/vorcall.proto        shared schema; generated into both server and client at build time
PROTOCOL.md                wire framing, state machines, limits
server/                    ASP.NET Core (.NET 10) backend, Vorcall.Server.csproj (server/Voice/ is the voice UDP relay,
                            server/Updates/ + server/Api/UpdatesEndpoints.cs serve /api/updates/*)
client/                    Cargo workspace (crates/vorcall-proto, crates/vorcall-core -> incl. the `update` module,
                            crates/vorcall-voice -> media engine, crates/vorcall-probe -> headless voice probe binary,
                            also carries the check-update/apply-update oracle subcommands,
                            crates/vorcall-release -> release-side signing tool, binary `vorcall-release`,
                            crates/vorcall-app -> binary `vorcall`)
client/update-keys.pub     Ed25519 public keys the client trusts for release manifests; empty disables the updater
deploy/                    nginx site configs and the host provisioning script
scripts/                   client release build scripts (Linux, Windows cross-build) and update-oracle.sh (updater oracle)
releases/                  gitignored; local dir the dev server serves under /api/updates/*; production's is the host's
docker-compose.yml         local dev: Postgres only
docker-compose.prod.yml    production stack: Postgres + backend, pulled from GHCR
.env.production.example    template for the production .env (never commit the real one)
.github/workflows/         ci.yml (format/lint/build), deploy.yml (build + deploy on push to main),
                            release.yml (build, sign and publish a client release)
```

## Prerequisites

- **Linux (build client + server, run local dev):** Rust via rustup (stable, 1.89+ — MSRV set by notify-rust), .NET SDK 10.0.x, Docker (for the local Postgres), ALSA headers for rodio/cpal: Fedora `sudo dnf install -y alsa-lib-devel`, Debian/Ubuntu `libasound2-dev` (CI installs it).
- **Cross-build for Windows (from Linux/Fedora):** the above, plus `sudo dnf install -y clang lld llvm`, `rustup target add x86_64-pc-windows-msvc`, `cargo install cargo-xwin`.
- **macOS (build client from source only):** Xcode command-line tools, rustup.

Voice adds no build prerequisite beyond the above: `opus-rs` is a pure-Rust codec (no cmake, no system libopus). ALSA headers remain the only Linux-specific requirement, needed for cpal (capture/playback) as well as rodio.

`dotnet` may not be on `PATH`; it can live at `~/.dotnet/dotnet`. The `dotnet-ef` global tool needs `DOTNET_ROOT=~/.dotnet` and `dotnet` on `PATH` (e.g. `PATH=~/.dotnet:$PATH`).

## Build the client

The server key and server URL are baked in at compile time (see [Client key and URL](#client-key-and-url) below); they can also be overridden at runtime by environment variables of the same names. `vorcall --version` prints `vorcall <version> <platform>` and exits, useful to check what a build reports.

Since `.github/workflows/release.yml` now builds and signs releases for all three platforms (see [Releases and updates](#releases-and-updates)), these local scripts are mainly a fallback — a one-off build without cutting a release, or a build to debug the pipeline itself.

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

### macOS

CI now produces `vorcall-macos-aarch64` (Apple Silicon only, ad-hoc signed — see [Releases and updates](#releases-and-updates)). Building from source still works:

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

One voice channel per text room. Join it from the sidebar; talk with push-to-talk (hold Ctrl by default, while the window is focused); mute and deafen are separate switches. The sidebar shows who is in voice and highlights who is speaking. Media rides a direct UDP path to the server host — not Cloudflare, not nginx — encrypted per voice session.

**Settings:** the "Settings" button in the header opens a full-screen page with the input device, the output device and the push-to-talk key ("Change", then press a key; Esc cancels). These are stored in `config.toml` as `input_device`, `output_device` and `ptt_key`.

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

Each probe prints one JSON line to stdout: `packets_sent`/`packets_received`, `decoded_seconds` and `tone_seconds` (how much of the peer's tone was actually decoded), `gaps`/`late`, `rtt_ms` (min/avg/max/last/samples), `link`, one `peers` entry per remote ssrc (received/lost/late/decoded_frames/decoder_resets), and `speaking_events`. Exit codes: `0` ran, `1` `--expect-peer` heard less than 1 s of tone, `2` usage, sign-in, connection or media failure.

Against production, run the same two-terminal recipe with the two test accounts kept in the gitignored `client/.env.probe` (`export PROBE_A_USER=… PROBE_A_PASS=… PROBE_B_USER=… PROBE_B_PASS=…`), `VORCALL_SERVER_URL` left at its default, and the real key in `VORCALL_SERVER_KEY`.

**Limits worth knowing:** 20 ms Opus frames at 48 kbps CBR; one voice channel (`general`); no echo cancellation (headsets recommended); no voice activation, only push-to-talk; push-to-talk only fires while the window is focused.

## Protocol

See [`PROTOCOL.md`](PROTOCOL.md) for framing, connection state machines and limits, and [`proto/vorcall.proto`](proto/vorcall.proto) for the message schema. Both sides generate code from the `.proto` file at build time; never hand-edit generated code.

## Production

- Domain `vorcall.example.com` points DNS-only (no Cloudflare proxy) at the Oracle ARM64 host `user@host`, app directory `/opt/vorcall`.
- `deploy/provision-host.sh` runs on the host: installs certbot, issues the Let's Encrypt certificate, installs the nginx site (`deploy/nginx/vorcall.conf`, backend proxied from loopback `127.0.0.1:5004`, `listen 443 ssl http2` to match the other sites on the host and silence the "protocol options redefined" warning), sets up the certbot renewal hook, and generates the host's `.env` from `.env.production.example` (random Postgres password, `Vorcall__ServerKey` and `Vorcall__JwtSigningKey`). See the script header for the exact invocation. nginx config changes on an existing host are applied by hand — the deploy workflow only copies the compose file.
- Deploy pipeline (`.github/workflows/deploy.yml`): a push to `main` builds an arm64 backend image on GitHub Actions, pushes it to `ghcr.io/freedomiit/vorcall-backend`, then copies `docker-compose.prod.yml` to the host and runs `docker compose pull && up -d` over SSH. The backend applies its own EF Core migrations on boot. A push to `main` deploys production immediately; there is no separate approval step.
- The pre-shared door key lives only in the host's `.env` (`Vorcall__ServerKey`) and must be baked into client builds as `VORCALL_SERVER_KEY`. Rotating it means: generate a new value, update the host `.env`, restart the backend, and rebuild/redistribute the client with the new key.
- `Vorcall:JwtSigningKey` (env `Vorcall__JwtSigningKey`) is required — base64 of 32 random bytes; the server refuses to boot without it. `.env.production.example` carries a placeholder and `deploy/provision-host.sh` generates a real value for fresh hosts; on an existing host, append it by hand (`openssl rand -base64 32`). Rotating it signs every client out within 15 minutes (the access token lifetime).

## Releases and updates

### How updates reach friends

Vorcall checks for a newer build automatically: on the first successful connection after the app starts, and every 6 hours after that while connected and in the chat room. A manual check is also available from Settings ("Check for updates").

A check fetches `/api/updates/manifest`, verifies its signature against the keys baked in at compile time from `client/update-keys.pub`, and — if there is something newer for the running platform — downloads the asset next to the running binary as `.vorcall-update-<version>`. Its size and SHA-256 are checked against the manifest before anything else happens with it.

- **Optional update:** a banner appears under the header — "Vorcall {version} is ready." with "Restart now" and "Later". "Later" hides the banner until the next check finds something new; "Restart now" leaves the room and voice channel cleanly, then swaps the binary and relaunches.
- **Required update:** when the manifest's `min_version` is above the running version, the chat screen is replaced by an "Update required" screen ("Vorcall {version} is needed to keep chatting.") that downloads and restarts on its own; a failure shows the error with a Retry button. The connection underneath is left alone, so a failed restart returns to a room that is still there.
- **Left for the next launch:** a verified download next to the binary that was never applied (the app was closed first) is re-verified from disk and installed at the next start, before the window opens. When the install directory is not writable, the download lands in the user's local data directory instead; that copy is never applied automatically — the UI says where it is and asks to install it by hand.
- Settings also shows `Version {current} ({platform})` and, when the manifest carries notes, "What's new in {version}".

Platform swap mechanics: Linux and macOS rename the downloaded file over the running binary and re-exec — safe, because the kernel keeps the old inode alive for the process that is still running it. Windows cannot overwrite a running executable, so it moves `vorcall.exe` to `vorcall.exe.old`, moves the new file into its place and relaunches; `.old` is deleted the next time the new binary starts.

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

1. Bump `version` in `client/Cargo.toml` (`[workspace.package]`); bump `min_version` too (`[workspace.metadata.vorcall]`) when older clients must be cut off. Commit and push.
2. Optionally dispatch the `release` workflow with `publish` unchecked first — a dry run that builds, signs and verifies but reaches neither the host nor a GitHub Release. No tag is needed for the dry run.
3. `git tag -a vX.Y.Z -m "notes"` && `git push origin vX.Y.Z`. The annotation becomes the manifest's `notes` and the GitHub Release body; a lightweight tag (no `-a`/`-m`) gives empty notes.
4. Watch the workflow run.
5. Verify on the host: `ssh user@host 'head -c 300 ~/freedom.vorcall/releases/manifest.json'`.
6. `users outdated` in the admin CLI (see [Admin CLI](#admin-cli)) lists who has not moved yet.

The workflow refuses to publish when: the tag does not equal the workspace version; `min_version` is above `version`; any platform build fails; or the signing key's public half is not in `client/update-keys.pub` (which is exactly when clients would reject the manifest).

### First install per platform

Only the first build on each friend's machine is a manual hand-off — later builds arrive through the in-app updater described above.

- **Linux:** `chmod +x vorcall-linux-x86_64` and run it from a directory the friend can write to (the updater replaces the file in place).
- **Windows:** SmartScreen shows "Windows protected your PC" on first run — "More info" then "Run anyway". Updates the app downloads itself carry no Mark-of-the-Web, so this prompt does not come back.
- **macOS (Apple Silicon only):** `xattr -d com.apple.quarantine vorcall-macos-aarch64 && chmod +x vorcall-macos-aarch64`, or right-click → Open once.

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

## Development gates

```
cd client && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test -p vorcall-voice -p vorcall-core -p vorcall-release -p vorcall-app
```

```
dotnet build server/Vorcall.Server.csproj -warnaserror
```

There are no automated tests in the MVP outside `vorcall-voice`, `vorcall-core`, `vorcall-release` and `vorcall-app`'s unit tests. `cargo tree -i aws-lc-rs` (run from `client/`) must report no match — the Windows cross build depends on `aws-lc-rs` staying out of the dependency graph.
