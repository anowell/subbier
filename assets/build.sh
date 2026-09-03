#!/usr/bin/env bash
# Regenerate every subbier icon asset from the SVG sources.
# Requires `resvg` (`cargo install resvg`) plus macOS `sips` and `iconutil`.
# resvg is used because `sips` rasterises SVGs at intrinsic size and downsamples,
# which visibly muddies the mark at 18px.
set -euo pipefail
cd "$(dirname "$0")"

command -v resvg   >/dev/null || { echo "missing: resvg (cargo install resvg)" >&2; exit 1; }
command -v sips    >/dev/null || { echo "missing: sips" >&2; exit 1; }
command -v iconutil>/dev/null || { echo "missing: iconutil" >&2; exit 1; }

# Menu bar template images. sr.svg is on an 18-unit grid, so only render at
# multiples of 18 or edges land off-pixel.
resvg sr.svg sr-18.png --width 18 --height 18
resvg sr.svg sr-36.png --width 36 --height 36
resvg sr.svg sr-54.png --width 54 --height 54

# App bundle icon.
ICONSET=Subbier.iconset
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
for sz in 16 32 128 256 512; do
  resvg sr-color.svg "$ICONSET/icon_${sz}x${sz}.png"    --width "$sz"
  resvg sr-color.svg "$ICONSET/icon_${sz}x${sz}@2x.png" --width "$((sz * 2))"
done
iconutil -c icns "$ICONSET" -o Subbier.icns
rm -rf "$ICONSET"

echo "wrote assets/sr-{18,36,54}.png and assets/Subbier.icns"
