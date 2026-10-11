#!/usr/bin/env bash
# assemble-videos.sh — stitch the recorded clips into the submission videos
# with title cards. Silent screen tracks; narration is added afterwards.
#
#   technical-demo.mp4  ~3 min budget  (reflex -> failover -> audit -> live dashboard)
#   pitch-video.mp4     ~2 min budget  (dashboard hero -> backtest numbers)
#   live-product.mp4    the deployed product walkthrough
#
# Usage: bash tools/video/assemble-videos.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

FF="$HOME/.hermes/tools/ffmpeg-9.0.1-linux-x64/bin/ffmpeg"
FFPROBE="$HOME/.hermes/tools/ffmpeg-9.0.1-linux-x64/bin/ffprobe"
[ -x "$FFPROBE" ] || FFPROBE="$HOME/.hermes/tools/ffmpeg-9.0.1-linux-x64/bin/ffprobe"

REC=docs/video/rendered
CARD=docs/video/cards
OUT=docs/video/final
mkdir -p "$CARD" "$OUT"

W=1600; H=900; FPS=12

# make_card <out.mp4> <line1> <line2>
make_card() {
  local out="$1" l1="$2" l2="$3"
  "$FF" -y -f lavfi -i "color=c=0x0b0b10:s=${W}x${H}:d=3:r=${FPS}" \
    -vf "drawtext=text='${l1}':fontcolor=0xa78bfa:fontsize=64:x=(w-text_w)/2:y=(h-text_h)/2-40:fontfile=/usr/share/fonts/TTF/DejaVuSans-Bold.ttf, \
         drawtext=text='${l2}':fontcolor=0x8a8aa3:fontsize=30:x=(w-text_w)/2:y=(h-text_h)/2+60:fontfile=/usr/share/fonts/TTF/DejaVuSans.ttf" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -crf 20 "$out" >/dev/null 2>&1
  echo "  card: $(basename "$out")"
}

# normalize <in> <out>  — force identical params so concat is clean
norm() {
  "$FF" -y -i "$1" -vf "scale=${W}:${H}:force_original_aspect_ratio=decrease,pad=${W}:${H}:(ow-iw)/2:(oh-ih)/2:color=0x0b0b10,fps=${FPS}" \
    -c:v libx264 -pix_fmt yuv420p -preset medium -crf 20 -an "$2" >/dev/null 2>&1
}

# concat <out> <clip...>
concat() {
  local out="$1"; shift
  local list; list=$(mktemp)
  for c in "$@"; do echo "file '$PWD/$c'" >> "$list"; done
  "$FF" -y -f concat -safe 0 -i "$list" -c copy "$out" >/dev/null 2>&1
  rm -f "$list"
  local d; d=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$out")
  LC_ALL=C printf '  -> %-40s %8ss  %s\n' "$(basename "$out")" "$d" "$(du -h "$out" | cut -f1)"
}

echo "== title cards =="
make_card "$CARD/t-speed.mp4"    "1. SPEED"    "a deterministic reflex, no LLM on the path"
make_card "$CARD/t-judge.mp4"    "2. JUDGMENT" "Qwen primary, KIMI failover, policy-gated"
make_card "$CARD/t-trust.mp4"    "3. TRUST"    "every decision hash-chained and anchored"
make_card "$CARD/t-pitch.mp4"    "SENTINEL"    "a verifiable risk guardian for Perpl"
make_card "$CARD/t-numbers.mp4"  "THE NUMBERS" "14 scenarios - 5972 preserved - 9.93 pct"

echo "== normalizing clips =="
N=docs/video/norm; mkdir -p "$N"
for f in 01-reflex-crash 02-strategy-failover 03-audit-verify 04-backtest 05-dashboard-overview 06-dashboard-scroll live-product; do
  [ -f "$REC/$f.mp4" ] && norm "$REC/$f.mp4" "$N/$f.mp4" && echo "  norm: $f"
done

echo "== technical-demo.mp4 =="
concat "$OUT/technical-demo.mp4" \
  "$CARD/t-speed.mp4" "$N/01-reflex-crash.mp4" \
  "$CARD/t-judge.mp4" "$N/02-strategy-failover.mp4" \
  "$CARD/t-trust.mp4" "$N/03-audit-verify.mp4" \
  "$N/05-dashboard-overview.mp4"

echo "== pitch-video.mp4 =="
concat "$OUT/pitch-video.mp4" \
  "$CARD/t-pitch.mp4" "$N/05-dashboard-overview.mp4" "$N/06-dashboard-scroll.mp4" \
  "$CARD/t-numbers.mp4" "$N/04-backtest.mp4"

echo "== live-product.mp4 =="
concat "$OUT/live-product.mp4" "$CARD/t-pitch.mp4" "$N/live-product.mp4"

echo "== final =="
ls -la "$OUT"/*.mp4
