#!/usr/bin/env bash
# Build a release Tomte.app (LSUIElement) containing the app and the daemon.
#
# Signing (env):
#   SIGN_IDENTITY  "Developer ID Application: …"  — real signing + hardened
#                  runtime (required for notarization). Unset = ad-hoc
#                  (local dev only; Gatekeeper will block it elsewhere).
#
# The bundle version is stamped from the workspace Cargo.toml so the binary
# (CARGO_PKG_VERSION), Info.plist, and the release tag can never disagree.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ -n "$version" ]] || { echo "error: no workspace version in Cargo.toml" >&2; exit 1; }

cargo build --release -p tomte-app -p tomte-daemon

app="$repo_root/target/bundle/Tomte.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

cp "$repo_root/target/release/tomte" "$app/Contents/MacOS/tomte"
cp "$repo_root/target/release/tomted" "$app/Contents/MacOS/tomted"
cp "$repo_root/bundle/Info.plist" "$app/Contents/Info.plist"

plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$plist" 2>/dev/null ||
    /usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $version" "$plist"

if [[ -n "${SIGN_IDENTITY:-}" ]]; then
    # Inner-out: the daemon is not the bundle's main executable, so it must
    # be signed explicitly; signing the .app then seals tomte + resources.
    codesign --force --options runtime --timestamp \
        -s "$SIGN_IDENTITY" "$app/Contents/MacOS/tomted"
    codesign --force --options runtime --timestamp \
        -s "$SIGN_IDENTITY" "$app"
    codesign --verify --strict --deep "$app"
else
    echo "note: SIGN_IDENTITY unset — ad-hoc signing (local dev only)" >&2
    codesign --force --deep -s - "$app"
fi

echo "$app"
