#!/usr/bin/env bash
# Glues the three clips into demo.mp4 (for Reddit) and demo.gif (for the README).
#
#   ./stitch.sh
#
# Clip 2 is trimmed and its slow middle (containers starting) sped up.
# Tune these from the timestamps you see in QuickTime (seconds, decimals ok):
B_START=${B_START:-0}      # where clip 2 starts being interesting
B_FAST_FROM=${B_FAST_FROM:-4}   # flow picked, nodes start turning
B_FAST_TO=${B_FAST_TO:-30}      # last node green
B_FAST_X=${B_FAST_X:-10}        # how much faster that stretch plays
B_END=${B_END:-40}         # app page loaded, padlock visible
#
# Output size: 1200 px wide; drops to 960 and fewer fps if the GIF is over MAX_MB.
W=1200; H=720; FPS=30; GIF_FPS=${GIF_FPS:-12}; MAX_MB=${MAX_MB:-5}
XF=0.35                    # crossfade between clips, seconds
HOLD=1.5                   # freeze the last frame so the loop doesn't snap
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build

dur() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }
fit="scale=${W}:${H}:force_original_aspect_ratio=decrease,pad=${W}:${H}:(ow-iw)/2:(oh-ih)/2:color=0x1e1e2e,setsar=1,fps=${FPS},format=yuv420p"
enc=(-c:v libx264 -preset slow -crf 18 -an -movflags +faststart)

echo "1/4 normalising clips"
ffmpeg -y -v error -i 01-config.mp4   -vf "$fit" "${enc[@]}" build/1.mp4
ffmpeg -y -v error -i 03-same-url.mp4 -vf "$fit" "${enc[@]}" build/3.mp4
ffmpeg -y -v error -i 02-browser.mov -filter_complex "
  [0:v]trim=${B_START}:${B_FAST_FROM},setpts=PTS-STARTPTS[a];
  [0:v]trim=${B_FAST_FROM}:${B_FAST_TO},setpts=(PTS-STARTPTS)/${B_FAST_X}[b];
  [0:v]trim=${B_FAST_TO}:${B_END},setpts=PTS-STARTPTS[c];
  [a][b][c]concat=n=3:v=1,${fit}[v]" -map "[v]" "${enc[@]}" build/2.mp4

echo "2/4 joining"
d1=$(dur build/1.mp4); d2=$(dur build/2.mp4)
o1=$(awk "BEGIN{print $d1-$XF}"); o2=$(awk "BEGIN{print $d1+$d2-2*$XF}")
ffmpeg -y -v error -i build/1.mp4 -i build/2.mp4 -i build/3.mp4 -filter_complex "
  [0:v][1:v]xfade=transition=fade:duration=${XF}:offset=${o1}[x];
  [x][2:v]xfade=transition=fade:duration=${XF}:offset=${o2},tpad=stop_mode=clone:stop_duration=${HOLD}[v]" \
  -map "[v]" "${enc[@]}" demo.mp4
echo "   demo.mp4: $(awk "BEGIN{printf \"%.1f\", $(dur demo.mp4)}") s"

make_gif() { # width fps
  rm -rf build/frames && mkdir build/frames
  ffmpeg -y -v error -i demo.mp4 -vf "fps=$2,scale=$1:-2:flags=lanczos" build/frames/f%04d.png
  if command -v gifski >/dev/null; then
    gifski --quiet --fps "$2" --width "$1" --quality 80 -o demo.gif build/frames/f*.png
  else
    ffmpeg -y -v error -i demo.mp4 -vf "fps=$2,scale=$1:-2:flags=lanczos,split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=4" demo.gif
  fi
  awk "BEGIN{printf \"%.1f\", $(wc -c < demo.gif)/1048576}"
}

echo "3/4 making the GIF"
for try in "1200 $GIF_FPS" "1200 10" "960 10" "800 10"; do
  set -- $try
  mb=$(make_gif "$1" "$2")
  echo "   ${1}px @ ${2}fps: ${mb} MB"
  awk "BEGIN{exit !($mb <= $MAX_MB)}" && break
done

cp demo.gif ../public/demo.gif
echo "4/4 done: $(pwd)/demo.mp4, and demo.gif copied to docs/public/demo.gif"
