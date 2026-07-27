#!/bin/bash
# Screenshot gallery states of chezmoi-ui without a human in the loop.
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
set -euo pipefail
cd "$(dirname "$0")/.."

STATE="${1:-all}"
THEMES="${2:-dark light}"
OUT="shots"
BIN="target/debug/chezmoi-ui"

cargo build -p czui-app --quiet
mkdir -p "$OUT"

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
  screencapture -o -x -l "$win" "$png"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$log"

  if [[ -s "$png" ]]; then
    echo "OK    $png"
  else
    echo "FAIL  live-$route: empty capture (Screen Recording permission?)" >&2
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
  png="$OUT/$state-$theme.png"

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
  screencapture -o -x -l "$win" "$png"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$log"

  if [[ -s "$png" ]]; then
    echo "OK    $png"
  else
    echo "FAIL  $state-$theme: empty capture (Screen Recording permission?)" >&2
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
