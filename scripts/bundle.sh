#!/usr/bin/env bash
# Build a release Tomte.app (LSUIElement) containing the app and the daemon.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build --release -p tomte-app -p tomte-daemon

app="$repo_root/target/bundle/Tomte.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

cp "$repo_root/target/release/tomte" "$app/Contents/MacOS/tomte"
cp "$repo_root/target/release/tomted" "$app/Contents/MacOS/tomted"
cp "$repo_root/bundle/Info.plist" "$app/Contents/Info.plist"

codesign --force --deep -s - "$app"

echo "$app"
