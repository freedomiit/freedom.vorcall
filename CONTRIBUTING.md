# Contributing to Vorcall

Thanks for looking. Vorcall is a small project with a deliberately small scope, so the most
useful thing you can do before writing code is to open an issue and check the change fits.

## Scope

Vorcall is a private chat for a group of people who already know each other. That shapes
what belongs here. Things that are **out of scope**, and will be closed as such:

- Federation, multiple servers per deployment, public server directories
- A web client, a mobile client, or a browser-based anything
- A plugin system, a bot API, webhooks
- Group DMs, threads, message search, pins, custom status
- Non-image attachments, OAuth, 2FA

That list is not a judgement about those features — they are good features. They are just
not what this is. See the README's "Deliberately absent" list.

Everything else is fair game: bugs, performance, platform support, accessibility, the
client's look and feel, documentation, and rough edges in self-hosting.

## Before you start

- **Open an issue first** for anything beyond a bug fix or a typo. A design that does not
  fit is much cheaper to discover in an issue than in a review.
- **No new dependencies without discussion.** The dependency graph is kept deliberately
  small, and two constraints are load-bearing: `aws-lc-rs` must stay out of the client graph
  (the Windows cross-build depends on it), and the updater's crypto is ring-only.

## Getting set up

[docs/development.md](docs/development.md) covers the native dependencies, running the
server and client locally, and the test suite. The short version on Fedora:

```bash
sudo dnf install -y alsa-lib-devel pipewire-devel clang-devel gcc-c++
docker compose up -d db
dotnet run --project server/Vorcall.Server.csproj
cd client && VORCALL_SERVER_URL=http://localhost:5000 cargo run -p vorcall-app
```

## The gates

Every change has to pass all of this. It is exactly what CI runs, so running it locally
saves a round trip:

```bash
cd client
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test -p vorcall-voice -p vorcall-core -p vorcall-release \
           -p vorcall-app -p vorcall-hotkey -p vorcall-screen \
           -p vorcall-clipboard
cd ..

dotnet build server/Vorcall.Server.csproj -warnaserror
docker compose up -d db && dotnet test tests/Vorcall.Server.Tests
```

The voice and screen-share media paths have no automated test. If you touch them, say in
the pull request which [runtime oracle](docs/development.md#runtime-oracles) you ran and
what it reported.

## House style

- **English only** — code, comments, documentation, commit messages.
- **[Conventional Commits](https://www.conventionalcommits.org/)**: `feat:`, `fix:`,
  `chore:`, `docs:`, `refactor:`, `test:`.
- **Comments explain why, not what.** An external constraint, a spec citation, the cause a
  workaround exists for. A comment restating the line below it will be asked about in
  review.
- **Match the surrounding code.** Its naming, its comment density, its idioms.
- **Never hand-edit generated code.** To change the wire format, edit
  [`proto/vorcall.proto`](proto/vorcall.proto) and rebuild both sides.

Two invariants that are easy to break without noticing:

- `client/crates/vorcall-core/src/permissions.rs` mirrors
  `server/Permissions/PermissionEngine.cs` **bit for bit**, and both are tested against the
  same matrix. The server is the security boundary; the client mirror only hides and
  disables.
- A new `ServerFrame` payload needs arms in **both** dispatchers of
  `client/crates/vorcall-core/src/connection.rs`. A known-but-unhandled payload makes the
  client reconnect in a loop.

## Pull requests

Keep them focused — one concern per pull request. Say what changed and why, and how you
verified it. If the change is visible in the client, a screenshot helps a lot.

Changes touching the wire protocol, the permission engine or the voice relay get more
scrutiny than the rest, because a mistake there is either a security hole or a
compatibility break. [`PROTOCOL.md`](PROTOCOL.md) is the reference and should be updated in
the same pull request.

## Security

Do not open a public issue for a security problem. [SECURITY.md](SECURITY.md) has the
details.

## License

By contributing you agree that your contribution is licensed under the
[MIT License](LICENSE), like the rest of the project.
