#!/usr/bin/env bash
# Release build of the Vorcall client for Linux x86_64.
#
# The server key and server URL are baked in at compile time, so they must be
# in the environment BEFORE cargo runs. Provide them either by exporting them
# or through client/.env.release (gitignored), whose format is:
#
#   export VORCALL_SERVER_KEY=<the Vorcall__ServerKey from the host .env>
#   export VORCALL_SERVER_URL=https://vorcall.example.com   # optional
#
# Output: dist/vorcall-linux-x86_64 and the first-install tarball
#         dist/vorcall-linux-x86_64.tar.gz (scripts/package-client-linux.sh)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

SERVER_URL="${VORCALL_SERVER_URL:-https://vorcall.example.com}"
export VORCALL_SERVER_URL="$SERVER_URL"
echo "server URL: $SERVER_URL"
echo "server key: set (not printed)"

CARGO=cargo
command -v cargo >/dev/null 2>&1 || CARGO="$HOME/.cargo/bin/cargo"
if ! command -v "$CARGO" >/dev/null 2>&1; then
    echo "ERROR: cargo not found on PATH nor at $HOME/.cargo/bin/cargo" >&2
    exit 1
fi

mkdir -p dist
( cd client && "$CARGO" build --release -p vorcall-app )

OUT="dist/vorcall-linux-x86_64"
cp client/target/release/vorcall "$OUT"

echo "built $OUT ($(du -h "$OUT" | cut -f1))"

VERSION="$("$OUT" --version | cut -d' ' -f2)"
scripts/package-client-linux.sh "$OUT" "$VERSION" dist
