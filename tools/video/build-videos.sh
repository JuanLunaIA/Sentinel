#!/usr/bin/env bash
# build-videos.sh — produce every submission video from the recorded sources.
#
# Pipeline per clip:
#   terminal demo:  .cast -> .html (typed page) -> frames (CDP) -> .mp4
#   dashboard:      live URL -> frames (CDP)     -> .mp4
#
# Outputs land in docs/video/rendered/ as H.264 MP4 (1920x1080-class, silent —
# the operator records/adds narration on top).
#
# Usage:
#   bash tools/video/build-videos.sh [dashboard|terminal|all]
set -euo pipefail
cd "$(dirname "$0")/../.."

FF="${FFMPEG:-$HOME/.hermes/tools/ffmpeg-9.0.1-linux-x64/bin/ffmpeg}"
FFPROBE="${FFPROBE:-$HOME/.hermes/tools/ffmpeg-9.0.1-linux-x64/bin/ffprobe}"
FPS="${FPS:-12}"

CASTS=docs/video/casts
HTML=docs/video/html
REC=docs/video/recordings
OUT=docs/video/rendered
mkdir -p "$HTML" "$REC" "$OUT"

encode() {
  local frames="$1" out="$2"
  "$FF" -y -framerate "$FPS" -i "$frames/f%06d.jpg" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -crf 20 \
    -movflags +faststart "$out" >/dev/null 2>&1
  local dur; dur=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$out")
  local size; size=$(du -h "$out" | cut -f1)
  LC_ALL=C printf '  -> %-44s %6ss  %s\n' "$out" "$dur" "$size" || true
}

build_terminal() {
  echo "== terminal demos =="
  for cast in "$CASTS"/*.cast; do
    [ -e "$cast" ] || continue
    local name; name=$(basename "$cast" .cast)
    [[ "$name" == smoke ]] && continue
    echo "[term] $name"
    # Stretch the typing to a target so each clip reads comfortably.
    TARGET_SECS=14 node tools/video/cast-to-html.cjs \
      "$cast" "$HTML/$name.html" "Sentinel — $name"
    node tools/video/capture-frames.cjs \
      "file://$PWD/$HTML/$name.html" "$PWD/$REC" 16 "term-$name" >/dev/null
    encode "$REC/term-$name-frames" "$OUT/$name.mp4"
  done
}

build_dashboard() {
  echo "== dashboard clips =="
  local url="${DASHBOARD_URL:-http://localhost:8081/}"
  echo "[dash] live overview ($url)"
  node tools/video/capture-frames.cjs "$url" "$PWD/$REC" 20 "dash-overview" >/dev/null
  encode "$REC/dash-overview-frames" "$OUT/05-dashboard-overview.mp4"

  echo "[dash] full-page scroll"
  node tools/video/capture-frames.cjs "$url" "$PWD/$REC" 24 "dash-scroll" scroll >/dev/null
  encode "$REC/dash-scroll-frames" "$OUT/06-dashboard-scroll.mp4"
}

case "${1:-all}" in
  terminal) build_terminal ;;
  dashboard) build_dashboard ;;
  all) build_terminal; build_dashboard ;;
  *) echo "usage: $0 [terminal|dashboard|all]"; exit 1 ;;
esac

echo
echo "== rendered =="
ls -la "$OUT"/*.mp4 2>/dev/null || echo "(none)"
