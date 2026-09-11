#!/usr/bin/env bash
# Renders the Windows icon (vorcall.ico) from the brand tile: every size the
# shell asks for, each rendered from the SVG rather than downscaled, packed by
# Pillow as plain bitmaps for the widest compatibility. release.yml runs it on
# Linux, where an SVG renderer is an apt package away, and hands the file to
# the Windows build for the installer and the shortcuts.
#
# Usage: scripts/make-windows-icon.sh <out.ico>
# Needs rsvg-convert (librsvg2-bin) and python3 with Pillow (python3-pil).
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <out.ico>" >&2
    exit 2
fi
OUT="$1"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$ROOT/assets/brand/icon.svg"
SIZES="16 24 32 48 64 128 256"

command -v rsvg-convert >/dev/null 2>&1 || { echo "ERROR: rsvg-convert not found (librsvg2-bin)" >&2; exit 1; }
python3 -c "import PIL" 2>/dev/null || { echo "ERROR: python3 cannot import PIL (python3-pil)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for size in $SIZES; do
    rsvg-convert --width "$size" --height "$size" --output "$WORK/$size.png" "$SVG"
done

mkdir -p "$(dirname "$OUT")"
# shellcheck disable=SC2086 # SIZES is a word list on purpose
python3 - "$WORK" "$OUT" $SIZES <<'PY'
import sys
from PIL import Image

work, out, *sizes = sys.argv[1:]
images = [Image.open(f"{work}/{size}.png").convert("RGBA") for size in sizes]
# Pillow keeps a provided image for each listed size and only resizes what is missing
images[-1].save(out, format="ICO", sizes=[im.size for im in images], append_images=images[:-1], bitmap_format="bmp")
PY

echo "built $OUT ($(du -h "$OUT" | cut -f1))"
