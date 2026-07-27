#!/bin/bash
# Screenshot gallery states of chezmoi-ui without a human in the loop.
#
#   scripts/shoot.sh                     # every state, dark + light
#   scripts/shoot.sh dashboard           # one state, dark + light
#   scripts/shoot.sh dashboard dark      # one state, one theme
#   scripts/shoot.sh all light           # every state, light only
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
