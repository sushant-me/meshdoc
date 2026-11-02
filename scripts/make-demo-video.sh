#!/usr/bin/env bash
# make-demo-video.sh — record the two-device sync demo to an MP4.
#
# Runs scripts/two-device-demo.sh, captures its output, and renders it as a
# timestamped terminal screencast (30–60 s) so the run can be watched without a
# terminal. Requires ImageMagick and ffmpeg.
#
# Usage:  ./scripts/make-demo-video.sh [output.mp4]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-${ROOT}/demo/two-device-sync.mp4}"
DEMO="${DEMO_SCRIPT:-${ROOT}/scripts/two-device-demo.sh}"
TITLE="${DEMO_TITLE:-meshdoc — two devices, one workspace, no server}"
FONT="/usr/share/fonts/noto/NotoSansMono-Regular.ttf"
[[ -f "$FONT" ]] || FONT="$(fc-match -f '%{file}' monospace)"
W=1280; H=720; PS=14; VISIBLE=36; FPS=3/2

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
mkdir -p "$(dirname "$OUT")"

echo "1/4 running $(basename "$DEMO")…"
# shellcheck disable=SC2016
BIN="${BIN:-${ROOT}/target/release/meshdoc}" "$DEMO" 2>&1 \
  | sed -e 's/\x1b\[[0-9;]*m//g' -e 's/\r//g' \
  > "${WORK}/raw.txt" || { echo "demo failed; see output:"; cat "${WORK}/raw.txt"; exit 1; }

# Fold long lines so nothing is cut off at the right edge.
fold -s -w 150 "${WORK}/raw.txt" > "${WORK}/lines.txt"
LINES=$(wc -l < "${WORK}/lines.txt")
echo "   captured $LINES lines"

echo "2/4 rendering frames…"
frame=0
render() { # render <file-of-visible-lines> <badge>
  local text="$1" badge="$2"
  magick -size ${W}x${H} xc:'#0d1117' \
    -fill '#161b22' -draw "rectangle 0,0 ${W},34" \
    -font "$FONT" -pointsize 15 -fill '#8b949e' -annotate +18+23 "$TITLE" \
    -fill '#238636' -draw "circle $((W-26)),17 $((W-22)),17" \
    -font "$FONT" -pointsize 15 -fill '#e6edf3' -annotate +18+62 "@${WORK}/visible.txt" \
    -font "$FONT" -pointsize 16 -fill '#3fb950' -annotate +18+${H}-22 "$badge" \
    "${WORK}/frame_$(printf '%04d' "$frame").png"
  frame=$((frame+1))
}

: > "${WORK}/visible.txt"
render "${WORK}/visible.txt" ""   # title frame

i=0
while IFS= read -r line; do
  i=$((i+1))
  printf '%s\n' "$line" >> "${WORK}/visible.txt"
  # scroll: keep only the last $VISIBLE lines
  total=$(wc -l < "${WORK}/visible.txt")
  if [[ "$total" -gt "$VISIBLE" ]]; then
    tail -n "$VISIBLE" "${WORK}/visible.txt" > "${WORK}/v2" && mv "${WORK}/v2" "${WORK}/visible.txt"
  fi
  render "${WORK}/visible.txt" ""
done < "${WORK}/lines.txt"

# hold the final state a little longer
for _ in $(seq 1 10); do render "${WORK}/visible.txt" ""; done

echo "3/4 encoding ${frame} frames at ${FPS} fps…"
ffmpeg -loglevel error -y -framerate "$FPS" -i "${WORK}/frame_%04d.png" \
  -vf "format=yuv420p" -c:v libx264 -preset medium -crf 20 \
  -movflags +faststart "$OUT"

echo "4/4 done -> $OUT"
ls -lh "$OUT"
ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$OUT" | \
  awk '{printf "   duration: %.1fs\n", $1}'
