#!/usr/bin/env bash
# Clip 2: the fghj UI starting a flow, then the app on its own HTTPS name.
# Opens a clean Chrome window at a fixed spot and records exactly that rectangle.
#   ./record-browser.sh            -> 02-browser.mov (stops after 60 s, or Ctrl-C)
set -euo pipefail
cd "$(dirname "$0")"

X=0; Y=60; W=1200; H=720     # points, not pixels; Y=60 keeps clear of the menu bar
SECONDS_MAX=${SECONDS_MAX:-60}

open -na "Google Chrome" --args \
  --user-data-dir=/tmp/fghj-demo-chrome \
  --no-first-run --no-default-browser-check --hide-crash-restore-bubble \
  --window-position=$X,$Y --window-size=$W,$H \
  https://fghj.internal/

echo "Chrome is open. Arrange the page, then press Enter to start recording."
read -r
echo "Recording ${W}x${H} at ${X},${Y} for up to ${SECONDS_MAX}s. Ctrl-C stops early."
screencapture -v -k -V "$SECONDS_MAX" -R"$X,$Y,$W,$H" 02-browser.mov || true
echo "Saved $(pwd)/02-browser.mov"
