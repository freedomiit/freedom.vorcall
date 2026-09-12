#!/usr/bin/env bash
# Release build of the Vorcall client for Windows x86_64, cross-compiled from
# Linux with cargo-xwin (MSVC target, no Windows machine needed).
#
# Cross toolchain on Fedora: `sudo dnf install -y clang lld llvm` — clang-cl
# (clang) compiles, lld-link (lld) links, and llvm-lib (llvm) builds the import
# libraries cc-rs needs; missing llvm alone fails the build late, inside cc-rs.
#
# The server key and server URL are baked in at compile time, so they must be
# in the environment BEFORE cargo runs. Provide them either by exporting them
# or through client/.env.release (gitignored), whose format is:
#
#   export VORCALL_SERVER_KEY=<the Vorcall__ServerKey from the host .env>
#   export VORCALL_SERVER_URL=https://chat.example.org           # required
#
# Output: dist/vorcall-windows-x86_64.exe
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

if [ -z "${VORCALL_SERVER_URL:-}" ]; then
    echo "ERROR: VORCALL_SERVER_URL is unset." >&2
    echo "A release build has to name the server it ships pointed at." >&2
    echo "Export it, or put it in client/.env.release:" >&2
    echo "  export VORCALL_SERVER_URL=https://chat.example.org" >&2
    exit 1
fi
SERVER_URL="$VORCALL_SERVER_URL"
export VORCALL_SERVER_URL="$SERVER_URL"
echo "server URL: $SERVER_URL"
echo "server key: set (not printed)"

# cargo-xwin needs the full LLVM cross toolchain: clang-cl to compile, lld-link
# to link, and llvm-lib for the import libraries cc-rs generates.
missing=()
command -v clang-cl >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 || missing+=("clang-cl/clang")
command -v lld-link >/dev/null 2>&1 || command -v ld.lld >/dev/null 2>&1 || missing+=("lld-link/ld.lld")
command -v llvm-lib >/dev/null 2>&1 || missing+=("llvm-lib")
if [ ${#missing[@]} -gt 0 ]; then
    echo "ERROR: missing cross toolchain: ${missing[*]}" >&2
    echo "Install it with:" >&2
    echo "  sudo dnf install -y clang lld llvm" >&2
    exit 1
fi

CARGO=cargo
command -v cargo >/dev/null 2>&1 || CARGO="$HOME/.cargo/bin/cargo"
if ! command -v "$CARGO" >/dev/null 2>&1; then
    echo "ERROR: cargo not found on PATH nor at $HOME/.cargo/bin/cargo" >&2
    exit 1
fi

mkdir -p dist
( cd client && "$CARGO" xwin build --release --target x86_64-pc-windows-msvc -p vorcall-app )

OUT="dist/vorcall-windows-x86_64.exe"
cp client/target/x86_64-pc-windows-msvc/release/vorcall.exe "$OUT"

echo "built $OUT ($(du -h "$OUT" | cut -f1))"
