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
THEME="$DATA/icons/hicolor"
ICON="$THEME/scalable/apps/vorcall.svg"

refresh() {
    # The entry is noticed on its own; the icon is not. GNOME re-reads a theme
    # only when the theme directory's own mtime changes (a file added two
    # levels down does not change it), and it trusts an icon-theme.cache over
    # the directory whenever the cache is at least as new, so a cache left
    # there by another app hides the icon until it is rebuilt. Only an
    # existing cache is rebuilt: creating one would lay the same trap for the
    # next installer that skips this step.
    update-desktop-database "$DATA/applications" 2>/dev/null || true
    touch -c "$THEME" 2>/dev/null || true
    if [ -f "$THEME/icon-theme.cache" ]; then
        gtk-update-icon-cache -q -t -f "$THEME" 2>/dev/null || true
    fi
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
        if ! ldconfig -p 2>/dev/null | grep -q 'libpipewire-0.3.so.0'; then
            echo "warning: libpipewire-0.3.so.0 not found; screen sharing needs PipeWire (package \"pipewire\") — Vorcall may not start without it" >&2
        fi
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
