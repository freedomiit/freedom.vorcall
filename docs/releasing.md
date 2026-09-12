# Releasing

How a client release is built, signed and published. This is maintainer documentation — you
only need it if you distribute your own Vorcall clients.

- [How updates reach people](#how-updates-reach-people)
- [One-time setup](#one-time-setup)
- [Cutting a release](#cutting-a-release)
- [First install per platform](#first-install-per-platform)
- [Deploying the server](#deploying-the-server)

---

## How updates reach people

The client checks for a newer build on the first successful connection after it starts, and
every six hours after that while connected. Settings has a manual "Check for updates" too.

A check fetches `/api/updates/manifest` **from the server the client is connected to**,
verifies its Ed25519 signature against the keys compiled in from `client/update-keys.pub`,
and — if there is something newer for the running platform — downloads the asset next to the
running binary. Its size and SHA-256 are checked against the manifest before anything else
happens with it.

- **Optional update:** a banner under the header — "Restart now" or "Later".
- **Required update:** when the manifest's `min_version` is above the running version, the
  chat screen is replaced by an update screen that downloads and restarts on its own.
- **Left for the next launch:** a verified download that was never applied is re-verified
  from disk and installed at the next start, before the window opens. If the install
  directory is not writable the download goes to the user's local data directory and is
  never applied automatically — the UI says where it is.

Swap mechanics differ by platform. Linux and macOS rename the download over the running
binary and re-exec, which is safe because the kernel keeps the old inode alive for the
running process. Windows cannot overwrite a running executable, so it moves `vorcall.exe`
aside to `vorcall.exe.old`, moves the new one into place and relaunches; `.old` is deleted
at the *next* start.

The updater is off entirely for a dev-key build, a debug build, when `VORCALL_NO_UPDATE` is
set, or when `client/update-keys.pub` is empty. Settings says which of those it is.

---

## One-time setup

1. Generate a signing key pair. The public half prints as 64 hex characters:

   ```bash
   cd client && cargo run -p vorcall-release -- gen-key --out ~/vorcall-update-signing.key
   ```

2. **Put the private key file in a password manager.** A lost key strands every client
   already out there: they refuse a manifest signed by any other key.

3. Give CI the secrets:

   ```bash
   gh secret set VORCALL_UPDATE_SIGNING_KEY < ~/vorcall-update-signing.key
   gh secret set VORCALL_SERVER_KEY        # the production door key
   ```

4. Paste the printed public key into `client/update-keys.pub` and commit it.

5. Deploy the server, then make sure its reverse proxy has the `/api/updates/` location
   from [self-hosting.md](self-hosting.md) — a release binary is a large streamed download
   and needs unbuffered proxying with longer timeouts.

**Key rotation:** add the new public key to `client/update-keys.pub` *alongside* the old
one, ship a release, sign later releases with the new key, and drop the old key only once
every client has picked up a build that carries the new one.

---

## Cutting a release

CI builds macOS. The Linux and Windows binaries are built on the maintainer's machine and
pushed to the host first; the workflow fetches them back, compiles the Windows installer
around the pushed exe, and signs one manifest covering all three platforms.

1. Bump `version` in `client/Cargo.toml` (`[workspace.package]`). Bump `min_version` too
   (`[workspace.metadata.vorcall]`) when older clients must be cut off. Commit and push.

2. Optionally dispatch the `release` workflow with `publish` unchecked — a dry run that
   builds macOS, then signs and verifies a macOS-only manifest. It reaches neither the host
   nor a GitHub Release, and needs no tag.

3. Tag it. The annotation becomes the manifest's `notes` and the GitHub Release body, so a
   lightweight tag gives empty notes:

   ```bash
   git tag -a vX.Y.Z -m "notes"
   ```

4. Build and push the two binaries CI does not build:

   ```bash
   scripts/push-client.sh
   ```

   It needs `client/.env.release`, SSH access to the host and the Windows cross toolchain,
   and it refuses to run unless the tag points at `HEAD` (`--no-tag-check` overrides, for a
   scratch build). Override the target with `VORCALL_RELEASE_SERVER=user@host`.

5. `git push origin vX.Y.Z`, then watch the workflow.

6. Verify the manifest landed, and see who has not moved yet:

   ```bash
   ssh "$VORCALL_RELEASE_SERVER" 'head -c 300 ~/freedom.vorcall/releases/manifest.json'
   docker compose exec backend dotnet Vorcall.Server.dll users outdated
   ```

The workflow refuses to publish when the tag does not equal the workspace version, when
`min_version` is above `version`, when the macOS build fails, when the Linux/Windows
binaries are not on the host for that version or their checksums do not match, or when the
signing key's public half is not in `client/update-keys.pub` — which is exactly when clients
would reject the manifest.

> The manifest signature covers the **exact bytes** of `manifest.json`. The server serves
> the stored bytes verbatim and `vorcall-release sign` signs the file's bytes, never a
> re-encoded value.

> Because the Linux binary is built on the maintainer's machine rather than a pinned CI
> runner, its glibc floor is that machine's glibc. Someone on an older distribution can hit
> `GLIBC_2.xx not found` and needs a build made on an older base.

---

## First install per platform

Only the first build on each machine is a manual hand-off; everything after that arrives
through the updater.

**Linux** — `vorcall-linux-x86_64.tar.gz`:

```bash
tar xzf vorcall-linux-x86_64.tar.gz && vorcall-linux-x86_64/install.sh
```

No root. The binary lands in `~/.local/bin`, the launcher entry and icon under
`~/.local/share`, and Vorcall appears in the app launcher. `install.sh --uninstall` removes
the same files.

**Windows** — `vorcall-windows-x86_64-setup.exe`. SmartScreen shows "Windows protected your
PC": "More info" → "Run anyway". It installs per user under `%LocalAppData%\Programs\Vorcall`
with a Start menu entry and no admin prompt — deliberately not Program Files, which would
push the updater into its manual fallback. Updates the app downloads itself carry no
Mark-of-the-Web, so the prompt does not come back.

**macOS (Apple Silicon)** — `vorcall-macos-aarch64.dmg`, never the bare binary (Finder opens
that in TextEdit). Open the image, drag `Vorcall.app` onto the Applications shortcut, eject,
and launch the copy in Applications — the image is read-only, so an app started from it
could not update itself. It is ad-hoc signed and not notarised, so the first launch is
blocked: right-click → Open on macOS 14 and earlier; on macOS 15, open it once, dismiss the
dialog, then System Settings → Privacy & Security → "Open Anyway".
`xattr -dr com.apple.quarantine /Applications/Vorcall.app` is the shortcut.

The Screen Recording and Input Monitoring grants on macOS are tied to the specific ad-hoc
signature, so they have to be re-granted after every update, and take effect only after a
relaunch.

---

## Deploying the server

For a production deployment of your own, [self-hosting.md](self-hosting.md) is the guide —
`docker compose up -d --build` and a reverse proxy.

This repository also carries the maintainer's own pipeline, which is more machinery than a
self-hoster needs:

- `docker-compose.prod.yml` pulls a pre-built image from a registry instead of building.
- `.github/workflows/deploy.yml` builds an arm64 image on every push to `main`, pushes it to
  GHCR and runs `docker compose pull && up -d` over SSH. **A push to `main` deploys
  immediately; there is no approval gate.**
- `deploy/provision-host.sh` sets up one specific host: certbot, the nginx site, the TLS
  renewal hook, the voice UDP port, the nightly backup cron and the production `.env`. Its
  domain, host address and app directory are overridable with `DOMAIN`, `EXPECTED_IP` and
  `APP_DIR`, but it assumes Ubuntu with nginx and Docker already installed.
- `deploy/backup-db.sh` is installed as `/usr/local/bin/vorcall-backup-db` with a cron entry
  at 03:15 UTC, keeping 14 days. The restore recipe is in its header.

The deploy workflow reads its target from the `DEPLOY_SERVER` **secret**, so a fork can point
it at its own host without editing the workflow. It has to be a secret rather than a
repository variable: Actions masks secrets in run logs and masks nothing else, and a
workflow-level `env:` block is printed in full at the head of every step — so a variable
there would publish the host into logs that are world-readable on a public repository.
