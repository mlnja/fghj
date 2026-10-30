#!/usr/bin/env bash
# Clips 2 and 4, in one take: the fghj UI downloading the repos the flow
# needs, then switching to the flow, then the app on its own HTTPS name.
# stitch.sh cuts this take in two at B_CUT and puts the `ls` clip between.
#
# Opens a clean Chrome window at a fixed spot and records exactly that
# rectangle with ffmpeg, which writes a complete file when stopped.
#   ./record-browser.sh            -> browser.mov (press q to stop)
set -euo pipefail
cd "$(dirname "$0")"

X=0; Y=60; W=1200; H=720     # points, not pixels; Y=60 keeps clear of the menu bar
SCALE=${SCALE:-2}            # pixels per point: 2 on a Retina screen, 1 otherwise

open -na "Google Chrome" --args \
  --user-data-dir=/tmp/fghj-demo-chrome \
  --no-first-run --no-default-browser-check --hide-crash-restore-bubble \
  --window-position=$X,$Y --window-size=$W,$H \
  https://fghj.internal/

# avfoundation numbers cameras and screens together; find the main screen.
# Listing devices always exits non-zero, hence the || true.
screen=$( { ffmpeg -hide_banner -f avfoundation -list_devices true -i "" 2>&1 || true; } \
  | sed -n 's/.*\[\([0-9]*\)\] Capture screen 0.*/\1/p')
[ -n "$screen" ] || { echo "ffmpeg can't see the screen: give your terminal Screen Recording permission" >&2; exit 1; }

echo "Chrome is open. Arrange the page, then press Enter to start recording."
read -r
echo "Recording. Press q in this terminal to stop and save."
ffmpeg -hide_banner -loglevel error -stats -y \
  -f avfoundation -capture_cursor 1 -capture_mouse_clicks 1 -framerate 30 -i "${screen}:none" \
  -vf "crop=$((W*SCALE)):$((H*SCALE)):$((X*SCALE)):$((Y*SCALE))" \
  -c:v h264_videotoolbox -b:v 20M -pix_fmt yuv420p browser.mov
echo "Saved $(pwd)/browser.mov"
