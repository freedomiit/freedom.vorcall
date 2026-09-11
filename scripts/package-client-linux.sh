#!/usr/bin/env bash
# Packs a release build of the Vorcall client into the Linux tarball: the
# binary with its executable bit (a raw download loses it), the launcher entry,
# the icon and a per-user install script. The in-app updater needs nothing
# new: it fetches the bare vorcall-linux-x86_64 and renames it over the
# installed binary (swap.rs).
#
# Usage: scripts/package-client-linux.sh <binary> <version> <outdir>
#   binary   the built client: client/target/release/vorcall or dist/vorcall-linux-x86_64
#   version  X.Y.Z, checked against what the binary reports
#   outdir   receives vorcall-linux-x86_64.tar.gz
#
# Linux only (GNU tar). desktop-file-validate checks the entry when installed.
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <binary> <version> <outdir>" >&2
    exit 2
fi
BINARY="$1"
VERSION="$2"
OUTDIR="$3"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLATFORM="linux-x86_64"
NAME="vorcall-$PLATFORM"
TARBALL="$NAME.tar.gz"

[ -f "$BINARY" ] || { echo "ERROR: $BINARY is not a file" >&2; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "ERROR: version '$VERSION' is not X.Y.Z" >&2; exit 1; }

mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STAGE="$WORK/$NAME"
mkdir -p "$STAGE"
install -m755 "$BINARY" "$STAGE/vorcall"
install -m644 "$ROOT/assets/brand/icon.svg" "$STAGE/vorcall.svg"
install -m644 "$ROOT/packaging/linux/vorcall.desktop" "$STAGE/vorcall.desktop"
install -m755 "$ROOT/packaging/linux/install.sh" "$STAGE/install.sh"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$STAGE/vorcall.desktop"
fi

reported="$("$STAGE/vorcall" --version)"
if [ "$reported" != "vorcall $VERSION $PLATFORM" ]; then
    echo "ERROR: the binary reports '$reported', expected 'vorcall $VERSION $PLATFORM'" >&2
    exit 1
fi

rm -f "$OUTDIR/$TARBALL"
tar -C "$WORK" --owner=0 --group=0 --numeric-owner -czf "$OUTDIR/$TARBALL" "$NAME"

echo "built $OUTDIR/$TARBALL ($(du -h "$OUTDIR/$TARBALL" | cut -f1))"
