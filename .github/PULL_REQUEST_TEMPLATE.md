## What and why

<!-- What changed, and what problem it solves. Link the issue if there is one. -->

## How it was verified

<!-- Which gates you ran, and anything you checked by hand. -->

- [ ] `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` (from `client/`)
- [ ] `cargo test` for the affected crates
- [ ] `dotnet build server/Vorcall.Server.csproj -warnaserror`
- [ ] `dotnet test tests/Vorcall.Server.Tests`
- [ ] Voice or screen share touched → which [runtime oracle](../blob/main/docs/development.md#runtime-oracles) was run, and what it reported

## Notes

<!-- Screenshots for anything visible in the client. Protocol, permission-engine or
     relay changes: say what a mismatched older client does. -->
