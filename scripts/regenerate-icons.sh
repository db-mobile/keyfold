#!/usr/bin/env bash
# Regenerate every icon bitmap in assets/icons/ from the master
# assets/icons/icon.svg: the PNG sizes, the macOS iconset, icon.icns and
# icon.ico. Needs rsvg-convert (librsvg2-bin) and the Tauri CLI.
# Usage: scripts/regenerate-icons.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICONS_DIR="$REPO_ROOT/assets/icons"
ICON_SVG="$ICONS_DIR/icon.svg"
ICONSET_DIR="$ICONS_DIR/icon.iconset"

if [[ ! -f "$ICON_SVG" ]]; then
  echo "Expected icon SVG not found: $ICON_SVG" >&2
  exit 1
fi

if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "rsvg-convert is required but was not found on PATH" >&2
  exit 1
fi

if [[ $# -gt 0 ]]; then
  echo "Unknown argument: $1" >&2
  exit 1
fi

cd "$REPO_ROOT"

# Regenerate icon_<size>x<size>.png variants
for s in 16 32 48 64 128 256 512 1024; do
  rsvg-convert -w "$s" -h "$s" "$ICON_SVG" -o "$ICONS_DIR/icon_${s}x${s}.png"
done

# Regenerate macOS icon.iconset
mkdir -p "$ICONSET_DIR"

rsvg-convert -w 16 -h 16 "$ICON_SVG" -o "$ICONSET_DIR/icon_16x16.png"
rsvg-convert -w 32 -h 32 "$ICON_SVG" -o "$ICONSET_DIR/icon_16x16@2x.png"
rsvg-convert -w 32 -h 32 "$ICON_SVG" -o "$ICONSET_DIR/icon_32x32.png"
rsvg-convert -w 64 -h 64 "$ICON_SVG" -o "$ICONSET_DIR/icon_32x32@2x.png"
rsvg-convert -w 128 -h 128 "$ICON_SVG" -o "$ICONSET_DIR/icon_128x128.png"
rsvg-convert -w 256 -h 256 "$ICON_SVG" -o "$ICONSET_DIR/icon_128x128@2x.png"
rsvg-convert -w 256 -h 256 "$ICON_SVG" -o "$ICONSET_DIR/icon_256x256.png"
rsvg-convert -w 512 -h 512 "$ICON_SVG" -o "$ICONSET_DIR/icon_256x256@2x.png"
rsvg-convert -w 512 -h 512 "$ICON_SVG" -o "$ICONSET_DIR/icon_512x512.png"
rsvg-convert -w 1024 -h 1024 "$ICON_SVG" -o "$ICONSET_DIR/icon_512x512@2x.png"

# Regenerate icon.ico and icon.icns without keeping extra generated assets
TMP_DIR="$ICONS_DIR/.tmp-tauri-icon-output"
rm -rf "$TMP_DIR"
mkdir -p "$TMP_DIR"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

if ! npx tauri icon -o "$TMP_DIR" "$ICON_SVG" >"$TMP_DIR/tauri-icon.log" 2>&1; then
  cat "$TMP_DIR/tauri-icon.log" >&2
  exit 1
fi

if [[ ! -f "$TMP_DIR/icon.ico" ]]; then
  echo "tauri icon did not produce: $TMP_DIR/icon.ico" >&2
  exit 1
fi
if [[ ! -f "$TMP_DIR/icon.icns" ]]; then
  echo "tauri icon did not produce: $TMP_DIR/icon.icns" >&2
  exit 1
fi

cp "$TMP_DIR/icon.ico" "$ICONS_DIR/icon.ico"
cp "$TMP_DIR/icon.icns" "$ICONS_DIR/icon.icns"

echo "Icons regenerated in $ICONS_DIR"
