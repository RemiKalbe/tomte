#!/bin/bash
# Screenshot gallery states of tomte without a human in the loop.
#
#   scripts/shoot.sh                     # every state, dark + light
#   scripts/shoot.sh dashboard           # one state, dark + light
#   scripts/shoot.sh dashboard dark      # one state, one theme
#   scripts/shoot.sh all light           # every state, light only
#   scripts/shoot.sh live dashboard      # REAL app, real daemon + live data
#                                        # (routes: dashboard|review|settings)
#
# PNGs land in shots/<state>-<theme>.png (gitignored). Requires the terminal
# (or whoever spawns this) to have Screen Recording permission — first run
# prompts once via System Settings.
#
# An "OK" line means THIS run captured that file. A capture that fails takes
# any previous PNG at that path with it, so a stale image can never be read
# as evidence for a change it predates.
set -euo pipefail
cd "$(dirname "$0")/.."

STATE="${1:-all}"
THEMES="${2:-dark light}"
OUT="shots"
BIN="target/debug/tomte"

cargo build -p tomte-app --quiet
mkdir -p "$OUT"

# A capture is only OK if THIS run wrote it. Shooting straight at the final
# path meant a failed capture left the previous run's PNG in place, and the
# `-s` check happily called that success — so a stale image could be read as
# evidence for a change it predates (2026-08-31).
#
# Capture to a scratch path, and only publish it once screencapture has both
# exited 0 and produced bytes. On failure the destination is left untouched
# and the old file, if any, is removed so nothing can be mistaken for fresh.
capture_window() {
  local win="$1" png="$2" what="$3"
  local tmp="$png.inflight.$$"
  rm -f "$tmp"
  if ! screencapture -o -x -l "$win" "$tmp" 2>/dev/null || [[ ! -s "$tmp" ]]; then
    rm -f "$tmp" "$png"
    capture_failed "$what"
    return 1
  fi
  mv -f "$tmp" "$png"
  # A sleeping display captures as a valid, uniformly black PNG — bytes on
  # disk, nothing in them. Black compresses to almost nothing, so an
  # implausibly small file is the one cheap signal we have. Warn rather than
  # fail: this is a heuristic, and a legitimately sparse frame is possible.
  local bytes
  bytes=$(stat -f%z "$png" 2>/dev/null || echo 0)
  if (( bytes < 15000 )); then
    echo "WARN  $png is only ${bytes}B — a blank/black frame looks like this." >&2
    echo "      Check the image before trusting it as evidence." >&2
  fi
  return 0
}

# Say what actually goes wrong, in likelihood order. The old message asserted
# a Screen Recording permission problem, which sent at least one debugging
# session down the wrong path when the real cause was a sleeping display.
capture_failed() {
  echo "FAIL  $1: screencapture produced nothing." >&2
  echo "      Possible causes, most common first:" >&2
  echo "        - the display is asleep or locked (this script runs" >&2
  echo "          \`caffeinate -u\` to wake it; that can fail over SSH or" >&2
  echo "          with the lid shut)" >&2
  echo "        - the window closed before the capture ran" >&2
  echo "        - the terminal lacks Screen Recording permission" >&2
  echo "          (System Settings -> Privacy & Security -> Screen Recording)" >&2
}

# Nudge the display awake up front. A sleeping display is the failure this
# script hits most, and it is silent: the capture succeeds and the PNG is
# black.
caffeinate -u -t 1 2>/dev/null || true

if [[ "$STATE" == "all" ]]; then
  STATES=$("$BIN" --gallery-list | cut -f1)
else
  STATES="$STATE"
fi

# Live mode: launch the real boot path (`--live <route>`), wait for both the
# window id AND the daemon hello (LIVE_CONNECTED), give the status hydrate a
# beat, then shoot. Follows the system theme — no forcing.
shoot_live() {
  local route="${1:-dashboard}"
  local log png pid win
  log=$(mktemp)
  png="$OUT/live-$route.png"

  "$BIN" --live "$route" >"$log" 2>&1 &
  pid=$!

  win=""
  for _ in $(seq 1 100); do
    win=$(sed -n 's/^GALLERY_WINDOW_ID: //p' "$log" | head -1)
    [[ -n "$win" ]] && break
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "FAIL  live-$route: app exited early:" >&2
      cat "$log" >&2
      rm -f "$log"
      return 1
    fi
    sleep 0.1
  done
  if [[ -z "$win" ]]; then
    echo "FAIL  live-$route: no window id after 10s" >&2
    kill "$pid" 2>/dev/null || true
    rm -f "$log"
    return 1
  fi

  connected=""
  for _ in $(seq 1 150); do
    if grep -q '^LIVE_CONNECTED$' "$log"; then
      connected=yes
      break
    fi
    sleep 0.1
  done
  [[ -z "$connected" ]] && echo "WARN  live-$route: no daemon hello after 15s — shooting anyway" >&2

  # hello landed; let the status reply + render settle
  sleep 1.5
  local ok=0
  capture_window "$win" "$png" "live-$route" || ok=1
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$log"

  if (( ok == 0 )); then
    echo "OK    $png"
  else
    return 1
  fi
}

if [[ "$STATE" == "live" ]]; then
  shoot_live "${2:-dashboard}"
  exit $?
fi

shoot_one() {
  local state="$1" theme="$2"
  local log png pid win
  log=$(mktemp)
  png="$OUT/${state//:/-}-$theme.png"

  "$BIN" --gallery "$state" "--$theme" >"$log" 2>&1 &
  pid=$!

  # Wait for the window id (the app prints it right after the window opens).
  win=""
  for _ in $(seq 1 100); do
    win=$(sed -n 's/^GALLERY_WINDOW_ID: //p' "$log" | head -1)
    [[ -n "$win" ]] && break
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "FAIL  $state-$theme: app exited early:" >&2
      cat "$log" >&2
      rm -f "$log"
      return 1
    fi
    sleep 0.1
  done
  if [[ -z "$win" ]]; then
    echo "FAIL  $state-$theme: no window id after 10s" >&2
    kill "$pid" 2>/dev/null || true
    rm -f "$log"
    return 1
  fi

  # Let the first real frames land before capturing.
  sleep 0.8
  local ok=0
  capture_window "$win" "$png" "$state-$theme" || ok=1
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$log"

  if (( ok == 0 )); then
    echo "OK    $png"
  else
    return 1
  fi
}

fails=0
for state in $STATES; do
  for theme in $THEMES; do
    shoot_one "$state" "$theme" || fails=$((fails + 1))
  done
done

exit "$fails"
