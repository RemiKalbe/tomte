#!/usr/bin/env bash
# The one sanctioned way to cut a release. Every gate is an exit code —
# never a human reading scrollback (2026-08-09: a failing e2e suite got
# tagged because a shell chain eyeballed grep'd test output; the CI gate
# caught it, but the tag should never have left the machine).
set -euo pipefail

die() { echo "release: $*" >&2; exit 1; }

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

version="${1:-}"
[[ -n "$version" ]] || die "usage: scripts/release.sh X.Y.Z"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$version' is not X.Y.Z"

# Gate 0: releasing exactly what origin/main has, nothing local or dirty.
[[ -z "$(git status --porcelain)" ]] || die "working tree not clean"
git fetch origin main --quiet
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] \
    || die "HEAD is not origin/main — push (or pull) first"

# Gate 1: the tag must name the version the binary will report.
ws_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ "$ws_version" == "$version" ]] \
    || die "workspace Cargo.toml says $ws_version — bump it first"
git rev-parse "v$version" >/dev/null 2>&1 && die "tag v$version already exists"
grep -q "^## $version" CHANGELOG.md \
    || die "CHANGELOG.md has no '## $version' section — write the notes first"

# Gate 2: the full local quality bar, exit codes only.
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

git tag -a "v$version" -m "Tomte $version"
git push origin "v$version"
echo "v$version pushed — the Release workflow signs, notarizes, publishes."
