#!/usr/bin/env bash
# Installs Vorcall for the current user, no root needed: the binary goes to
# ~/.local/bin, the launcher entry and the icon under ~/.local/share. All of it
# stays writable by the account, which is what the in-app updater needs to
# replace the binary in place. `install.sh --uninstall` removes the same files.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
BINARY="$BIN_DIR/vorcall"
ENTRY="$DATA/applications/vorcall.desktop"
ICON="$DATA/icons/hicolor/scalable/apps/vorcall.svg"

refresh() {
    # launchers notice the files on their own; this only makes it quicker
    update-desktop-database "$DATA/applications" 2>/dev/null || true
}

case "${1:-}" in
    "")
        install -Dm755 "$HERE/vorcall" "$BINARY"
        install -Dm644 "$HERE/vorcall.svg" "$ICON"
        mkdir -p "$(dirname "$ENTRY")"
        # the launcher does not search ~/.local/bin, so Exec gets the full path
        sed "s|^Exec=.*|Exec=\"$BINARY\"|" "$HERE/vorcall.desktop" > "$ENTRY"
        refresh
        echo "Vorcall installed: look for it in the app launcher, or run $BINARY"
        ;;
    --uninstall)
        # the updater's leftovers live next to the binary
        rm -f "$BINARY" "$BIN_DIR"/.vorcall-update-* "$ENTRY" "$ICON"
        refresh
        echo "Vorcall removed. The sign-in under ~/.config/vorcall is kept; delete it to forget the account."
        ;;
    *)
        echo "usage: $0 [--uninstall]" >&2
        exit 2
        ;;
esac
