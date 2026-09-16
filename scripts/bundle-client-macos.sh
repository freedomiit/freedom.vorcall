#!/usr/bin/env bash
# Wraps a release build of the Vorcall client into Vorcall.app and packs it
# into a DMG for the first install on a Mac. A bare Mach-O download has no
# executable bit and no extension, so Finder hands it to TextEdit; an app
# bundle is what a double-click launches. The image holds the app next to an
# Applications shortcut, the usual drag-to-install window, and is read-only,
# which nudges people to drag before running. The in-app updater needs nothing
# new: it fetches the bare vorcall-macos-aarch64 and renames it over
# Contents/MacOS/vorcall (swap.rs); the bundle's seal covers only the files
# around it.
#
# Usage: scripts/bundle-client-macos.sh <binary> <version> <outdir>
#   binary   the built client: client/target/release/vorcall or dist/vorcall-macos-aarch64
#   version  X.Y.Z, checked against what the binary reports
#   outdir   receives Vorcall.app and vorcall-macos-aarch64.dmg
#
# macOS only: iconutil, codesign, hdiutil and plutil ship with the OS;
# rsvg-convert (brew install librsvg) renders the icon.
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <binary> <version> <outdir>" >&2
    exit 2
fi
BINARY="$1"
VERSION="$2"
OUTDIR="$3"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ICON_SVG="$ROOT/assets/brand/icon.svg"
BUNDLE_ID="br.com.freedomit.vorcall"
APP_NAME="Vorcall"
PLATFORM="macos-aarch64"
DMG_NAME="vorcall-$PLATFORM.dmg"

missing=()
for tool in rsvg-convert iconutil codesign hdiutil plutil; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ ${#missing[@]} -gt 0 ]; then
    echo "ERROR: missing tools: ${missing[*]} (macOS, with librsvg from Homebrew)" >&2
    exit 1
fi
[ -f "$BINARY" ] || { echo "ERROR: $BINARY is not a file" >&2; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "ERROR: version '$VERSION' is not X.Y.Z" >&2; exit 1; }

mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd)"
APP="$OUTDIR/$APP_NAME.app"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Icon: the tile fills its SVG, but the Dock expects it inset on the canvas
# like every other app (824 of 1024, the Apple icon grid), so every size is
# rendered with that margin.
ICONSET="$WORK/$APP_NAME.iconset"
mkdir -p "$ICONSET"
for spec in icon_16x16:16 icon_16x16@2x:32 icon_32x32:32 icon_32x32@2x:64 \
            icon_128x128:128 icon_128x128@2x:256 icon_256x256:256 icon_256x256@2x:512 \
            icon_512x512:512 icon_512x512@2x:1024; do
    name="${spec%%:*}"
    size="${spec##*:}"
    inner=$(( size * 824 / 1024 ))
    offset=$(( (size - inner) / 2 ))
    rsvg-convert --width "$inner" --height "$inner" \
        --page-width "$size" --page-height "$size" --left "$offset" --top "$offset" \
        --output "$ICONSET/$name.png" "$ICON_SVG"
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
iconutil --convert icns "$ICONSET" --output "$APP/Contents/Resources/$APP_NAME.icns"
cp "$BINARY" "$APP/Contents/MacOS/vorcall"
chmod 755 "$APP/Contents/MacOS/vorcall"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# ScreenCaptureKit with own-process audio exclusion needs macOS 13.
# NSMicrophoneUsageDescription is not optional: macOS kills a bundled app that
# touches the microphone without one, and push-to-talk does.
# NSScreenCaptureUsageDescription is what macOS shows on the Screen Recording
# prompt when a share starts, and NSCameraUsageDescription the same for the
# camera — without it macOS kills the app the moment AVFoundation opens one.
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>vorcall</string>
    <key>CFBundleIconFile</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.social-networking</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSCameraUsageDescription</key>
    <string>Vorcall uses the camera when you turn it on in a call.</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Vorcall uses the microphone for the voice channel while you hold push-to-talk.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Vorcall records the screen or window you choose to share with your voice channel.</string>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist"

# Ad-hoc: Apple Silicon refuses unsigned arm64 code. Gatekeeper's first-run
# prompt for an app that is not notarised is documented in the README.
codesign --sign - --force "$APP"
codesign --verify --strict "$APP"

reported="$("$APP/Contents/MacOS/vorcall" --version)"
if [ "$reported" != "vorcall $VERSION $PLATFORM" ]; then
    echo "ERROR: the bundle reports '$reported', expected 'vorcall $VERSION $PLATFORM'" >&2
    exit 1
fi

STAGE="$WORK/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/$APP_NAME.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUTDIR/$DMG_NAME"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -format UDZO -ov "$OUTDIR/$DMG_NAME"
hdiutil verify "$OUTDIR/$DMG_NAME"

echo "built $APP"
echo "built $OUTDIR/$DMG_NAME ($(du -h "$OUTDIR/$DMG_NAME" | cut -f1))"
