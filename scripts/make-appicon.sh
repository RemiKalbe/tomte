#!/usr/bin/env bash
# Compile bundle/Tomte.icon (the Icon Composer document) with Apple's own
# renderer — no hand-rolled compositing (2026-08-09: a reverse-engineered
# SVG flattening mangled the glyph scale and lost the glass treatment).
#
#   actool (Xcode 26+) emits BOTH:
#     bundle/AppIcon.icns  flattened fallback  → CFBundleIconFile (macOS 13+)
#     bundle/Assets.car    dynamic Liquid Glass → CFBundleIconName (macOS 26+)
#
# Also renders the menubar template glyph from the document's source SVG.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

xcrun actool bundle/Tomte.icon --compile "$work" \
    --platform macosx --minimum-deployment-target 13.0 \
    --app-icon Tomte \
    --output-partial-info-plist "$work/partial.plist" > "$work/actool.log" \
    || { cat "$work/actool.log" >&2; exit 1; }
[[ -f "$work/Tomte.icns" && -f "$work/Assets.car" ]] \
    || { echo "actool produced no icns/car — Xcode 26+ required" >&2; exit 1; }
cp "$work/Tomte.icns" bundle/AppIcon.icns
cp "$work/Assets.car" bundle/Assets.car

# README header image: Apple's largest flattened rep (256px, shown at 128).
iconutil --convert iconset bundle/AppIcon.icns -o "$work/AppIcon.iconset"
mkdir -p docs/assets
cp "$work/AppIcon.iconset/icon_128x128@2x.png" docs/assets/tomte-icon.png

# Menubar template: alpha is the only channel macOS uses; 36px backing for
# an 18pt status item. Requires rsvg-convert (brew install librsvg).
rsvg-convert -w 36 -h 36 bundle/Tomte.icon/Assets/gnome.svg \
    -o crates/app/assets/menubar@2x.png

echo "bundle/AppIcon.icns + bundle/Assets.car + docs/assets/tomte-icon.png + menubar@2x.png"
