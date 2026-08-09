#!/usr/bin/env bash
# Rasterize the app icon from bundle/Tomte.icon (the Icon Composer source
# of truth) into bundle/AppIcon.icns, and the menubar template glyph into
# crates/app/assets/menubar@2x.png.
#
# The .icns is a flattened approximation of the Icon Composer document:
# macOS squircle (Big Sur grid: 824/1024 with 100px margins), automatic
# gradient derived from the document's fill color, white glyph at the
# document's scale. Pre-Tahoe systems and our hand-rolled bundle consume
# .icns; the .icon bundle stays for a future Xcode/actool pipeline.
#
# Requires: rsvg-convert (brew install librsvg), iconutil (macOS).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

glyph="bundle/Tomte.icon/Assets/gnome.svg"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# White-glyph composite on the gradient squircle. Glyph paths are inlined
# from the source SVG with the fill overridden to white (mirrors the
# icon.json layer fill), drawn at the document's scale (5.5 × 150 = 825px
# on the 1024 canvas, centered).
{
    echo '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">'
    echo '  <defs><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">'
    echo '    <stop offset="0" stop-color="#5A6AC6"/>'
    echo '    <stop offset="1" stop-color="#4351AF"/>'
    echo '  </linearGradient></defs>'
    echo '  <rect x="100" y="100" width="824" height="824" rx="185" fill="url(#bg)"/>'
    echo '  <g transform="translate(99.5,99.5) scale(5.5)" fill="#ffffff">'
    sed -n 's/.*\(<path[^>]*>\).*/\1/p' "$glyph" | sed 's/class="[^"]*"//'
    echo '  </g>'
    echo '</svg>'
} > "$work/appicon.svg"

iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    rsvg-convert -w "$size" -h "$size" "$work/appicon.svg" \
        -o "$iconset/icon_${size}x${size}.png"
    double=$((size * 2))
    rsvg-convert -w "$double" -h "$double" "$work/appicon.svg" \
        -o "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o bundle/AppIcon.icns

# Menubar template: alpha is the only channel macOS uses; 36px backing for
# an 18pt status item.
rsvg-convert -w 36 -h 36 "$glyph" -o crates/app/assets/menubar@2x.png

echo "bundle/AppIcon.icns + crates/app/assets/menubar@2x.png"
