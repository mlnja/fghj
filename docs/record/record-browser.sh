#!/usr/bin/env bash
# Clips 2 and 4, in one take: the fghj UI downloading the repos the flow
# needs, then switching to the flow, then the app on its own HTTPS name.
# stitch.sh cuts this take in two at B_CUT and puts the `ls` clip between.
#
# Opens a clean Chrome window at a fixed spot and records exactly that rectangle.
#   ./record-browser.sh            -> browser.mov (stops after 4 min, or Ctrl-C)
set -euo pipefail
cd "$(dirname "$0")"

X=0; Y=60; W=1200; H=720     # points, not pixels; Y=60 keeps clear of the menu bar
SECONDS_MAX=${SECONDS_MAX:-240}

open -na "Google Chrome" --args \
  --user-data-dir=/tmp/fghj-demo-chrome \
  --no-first-run --no-default-browser-check --hide-crash-restore-bubble \
  --window-position=$X,$Y --window-size=$W,$H \
  https://fghj.internal/

echo "Chrome is open. Arrange the page, then press Enter to start recording."
read -r
echo "Recording ${W}x${H} at ${X},${Y} for up to ${SECONDS_MAX}s. Ctrl-C stops early."
trap 'true' INT              # Ctrl-C stops screencapture, not this script
screencapture -v -k -V "$SECONDS_MAX" -R"$X,$Y,$W,$H" browser.mov || true
echo "Saved $(pwd)/browser.mov"
