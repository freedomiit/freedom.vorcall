#!/usr/bin/env bash
# Build the Linux and Windows release clients on this machine and push them to
# the production host, where `.github/workflows/release.yml` fetches them back
# when the tag lands. CI builds only macOS, so this script has to run BEFORE
# `git push origin vX.Y.Z`.
#
# The server key and server URL are baked in at compile time; provide them by
# exporting them or through client/.env.release (gitignored):
#
#   export VORCALL_SERVER_KEY=<the Vorcall__ServerKey from the host .env>
#   export VORCALL_SERVER_URL=https://vorcall.example.com   # optional
#
# Optional, same file or environment:
#   VORCALL_RELEASE_SERVER   ssh target        (default user@host)
#   VORCALL_RELEASE_APP_DIR  app dir on it     (default /opt/vorcall)
#   SSH_OPTS                 extra ssh/scp options
#
# Usage: scripts/push-client.sh [--no-tag-check]
#
# Prerequisites: client/.env.release, SSH access to the host, and the Windows
# cross toolchain (`sudo dnf install -y clang lld llvm`, cargo-xwin).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TAG_CHECK=1
for arg in "$@"; do
    case "$arg" in
        --no-tag-check) TAG_CHECK=0 ;;
        *) echo "ERROR: unknown argument '$arg' (usage: scripts/push-client.sh [--no-tag-check])" >&2; exit 1 ;;
    esac
done

if [ -f client/.env.release ]; then
    # shellcheck disable=SC1091
    source client/.env.release
fi

if [ -z "${VORCALL_SERVER_KEY:-}" ] || [ "${VORCALL_SERVER_KEY}" = "dev" ]; then
    echo "ERROR: VORCALL_SERVER_KEY is unset or still the placeholder 'dev'." >&2
    echo "Export it, or put it in client/.env.release:" >&2
    echo "  export VORCALL_SERVER_KEY=<the Vorcall__ServerKey from the host .env>" >&2
    exit 1
fi
export VORCALL_SERVER_KEY

SERVER="${VORCALL_RELEASE_SERVER:-user@host}"
APP_DIR="${VORCALL_RELEASE_APP_DIR:-/opt/vorcall}"
read -r -a SSH_OPT_ARR <<< "${SSH_OPTS:-}"

CARGO=cargo
command -v cargo >/dev/null 2>&1 || CARGO="$HOME/.cargo/bin/cargo"
if ! command -v "$CARGO" >/dev/null 2>&1; then
    echo "ERROR: cargo not found on PATH nor at $HOME/.cargo/bin/cargo" >&2
    exit 1
fi

echo "== version =="
META="$( cd client && "$CARGO" metadata --no-deps --format-version 1 )"
VERSION="$(jq -r '.packages[] | select(.name=="vorcall-app") | .version' <<<"$META")"
[ -n "$VERSION" ] && [ "$VERSION" != null ] || { echo "ERROR: cannot read the workspace version from client/Cargo.toml" >&2; exit 1; }
echo "version: $VERSION"

# The binaries pushed here are the ones the tagged release publishes, so they
# must be built from exactly the commit the tag points at — the same assertion
# release.yml's preflight makes on its side.
if [ "$TAG_CHECK" = 1 ]; then
    if ! git tag --points-at HEAD | grep -qx "v$VERSION"; then
        echo "ERROR: tag v$VERSION does not point at HEAD." >&2
        echo "Create it first (git tag -a v$VERSION -m \"notes\"), or pass --no-tag-check for a scratch build." >&2
        exit 1
    fi
    echo "tag v$VERSION points at HEAD"
else
    echo "tag check skipped"
fi

echo "== build linux =="
scripts/build-client-linux.sh

echo "== build windows =="
scripts/build-client-windows.sh

echo "== aws-lc-rs gate =="
if ( cd client && "$CARGO" tree -i aws-lc-rs >/dev/null 2>&1 ); then
    echo "ERROR: aws-lc-rs is in the dependency graph" >&2
    exit 1
fi
echo "no aws-lc-rs in the graph"

echo "== checksums =="
(
    cd dist
    for f in vorcall-linux-x86_64 vorcall-windows-x86_64.exe vorcall-linux-x86_64.tar.gz; do
        [ -f "$f" ] || { echo "ERROR: dist/$f is missing" >&2; exit 1; }
        sha256sum "$f" > "$f.sha256"
        cat "$f.sha256"
    done
)

echo "== push to $SERVER =="
ssh "${SSH_OPT_ARR[@]}" "$SERVER" "mkdir -p '$APP_DIR/releases/$VERSION'"
# Only the binaries and the Linux tarball: the manifest is assembled and signed
# by release.yml, which alone holds the signing key.
scp "${SSH_OPT_ARR[@]}" \
    dist/vorcall-linux-x86_64 dist/vorcall-linux-x86_64.sha256 \
    dist/vorcall-windows-x86_64.exe dist/vorcall-windows-x86_64.exe.sha256 \
    dist/vorcall-linux-x86_64.tar.gz \
    "$SERVER:$APP_DIR/releases/$VERSION/"

echo
echo "pushed to $SERVER:$APP_DIR/releases/$VERSION/:"
echo "  vorcall-linux-x86_64(.sha256)"
echo "  vorcall-windows-x86_64.exe(.sha256)"
echo "  vorcall-linux-x86_64.tar.gz"
echo
echo "Next: push the tag v$VERSION (git push origin v$VERSION)."
echo "The release workflow builds macOS, fetches these two binaries back,"
echo "compiles the Windows installer, signs the manifest and activates it."
