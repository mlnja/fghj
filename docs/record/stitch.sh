#!/usr/bin/env bash
# Glues the clips into demo.mp4 (for Reddit) and demo.gif (for the README):
#
#   1-wire.mp4 -> browser part 1 -> 3-ls.mp4 -> browser part 2 -> 5-same-url.mp4
#
# browser.mov is one take, cut in two at B_CUT. The two waits in it (repos
# downloading, containers starting) are sped up. Read the times off
# QuickTime, in seconds (decimals ok), each one later than the one before:
B_START=${B_START:-0}      # start of the take you want to keep
B_DL_FROM=${B_DL_FROM:-5}  # pressed Download all
B_DL_TO=${B_DL_TO:-20}     # the graph has filled in
B_CUT=${B_CUT:-23}         # cut here to the `ls` clip
B_UP_FROM=${B_UP_FROM:-26} # pressed Switch to flow
B_UP_TO=${B_UP_TO:-80}     # last node green
B_END=${B_END:-90}         # app page loaded, padlock visible
B_FAST_X=${B_FAST_X:-10}   # how much faster the two waits play
#
#   B_DL_FROM=6.5 B_DL_TO=31 B_CUT=34 ... ./stitch.sh
#
# Output size: 1200 px wide; drops to 960 and fewer fps if the GIF is over MAX_MB.
W=1200; H=720; FPS=30; GIF_FPS=${GIF_FPS:-12}; MAX_MB=${MAX_MB:-5}
XF=0.35                    # crossfade between clips, seconds
HOLD=1.5                   # freeze the last frame so the loop doesn't snap
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build

for f in 1-wire.mp4 browser.mov 3-ls.mp4 5-same-url.mp4; do
  [ -f "$f" ] || { echo "missing $f: record it first (see README.md)" >&2; exit 1; }
done
prev=-1
for v in B_START B_DL_FROM B_DL_TO B_CUT B_UP_FROM B_UP_TO B_END; do
  awk "BEGIN{exit !(${!v} > $prev)}" || { echo "$v=${!v} must be later than the time before it" >&2; exit 1; }
  prev=${!v}
done

dur() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }
fit="scale=${W}:${H}:force_original_aspect_ratio=decrease,pad=${W}:${H}:(ow-iw)/2:(oh-ih)/2:color=0x1e1e2e,setsar=1,fps=${FPS},format=yuv420p"
enc=(-c:v libx264 -preset slow -crf 18 -an -movflags +faststart)

# browser_part OUT FROM FAST_FROM FAST_TO TO: plays FAST_FROM..FAST_TO at B_FAST_X.
browser_part() {
  ffmpeg -y -v error -i browser.mov -filter_complex "
    [0:v]trim=$2:$3,setpts=PTS-STARTPTS[a];
    [0:v]trim=$3:$4,setpts=(PTS-STARTPTS)/${B_FAST_X}[b];
    [0:v]trim=$4:$5,setpts=PTS-STARTPTS[c];
    [a][b][c]concat=n=3:v=1,${fit}[v]" -map "[v]" "${enc[@]}" "$1"
}

echo "1/4 normalising clips"
ffmpeg -y -v error -i 1-wire.mp4     -vf "$fit" "${enc[@]}" build/1.mp4
browser_part build/2.mp4 "$B_START" "$B_DL_FROM" "$B_DL_TO" "$B_CUT"
ffmpeg -y -v error -i 3-ls.mp4       -vf "$fit" "${enc[@]}" build/3.mp4
browser_part build/4.mp4 "$B_CUT" "$B_UP_FROM" "$B_UP_TO" "$B_END"
ffmpeg -y -v error -i 5-same-url.mp4 -vf "$fit" "${enc[@]}" build/5.mp4

echo "2/4 joining"
inputs=(); graph=""; last="[0:v]"; offset=0
for i in 1 2 3 4 5; do inputs+=(-i "build/$i.mp4"); done
for i in 1 2 3 4; do
  offset=$(awk "BEGIN{print $offset + $(dur build/$i.mp4) - $XF}")
  graph+="${last}[$i:v]xfade=transition=fade:duration=${XF}:offset=${offset}[x$i];"
  last="[x$i]"
done
graph+="${last}tpad=stop_mode=clone:stop_duration=${HOLD}[v]"
ffmpeg -y -v error "${inputs[@]}" -filter_complex "$graph" -map "[v]" "${enc[@]}" demo.mp4
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
