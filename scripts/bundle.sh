#!/usr/bin/env bash
# Build a release Chezmoi UI.app (LSUIElement) containing the app and the daemon.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build --release -p czui-app -p czui-daemon

app="$repo_root/target/bundle/Chezmoi UI.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

cp "$repo_root/target/release/chezmoi-ui" "$app/Contents/MacOS/chezmoi-ui"
cp "$repo_root/target/release/chezmoid" "$app/Contents/MacOS/chezmoid"
cp "$repo_root/bundle/Info.plist" "$app/Contents/Info.plist"

codesign --force --deep -s - "$app"

echo "$app"
