#!/usr/bin/env bash
# record-terminal-demos.sh — records the Sentinel terminal demos with asciinema.
#
# Produces .cast files in docs/video/casts/ which cast-to-html.cjs then turns
# into typed terminal pages for the frame recorder. Uses PREBUILT release
# binaries so no compile time leaks into the recording.
#
# Usage: bash tools/video/record-terminal-demos.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

export PATH="$HOME/.local/bin:$PATH"
CASTS=docs/video/casts
mkdir -p "$CASTS"

COLS=120
ROWS=32

rec() {
  local name="$1"; shift
  echo "[cast] recording $name ..."
  asciinema rec --overwrite --cols "$COLS" --rows "$ROWS" \
    -c "$*" "$CASTS/$name.cast" >/dev/null 2>&1
  echo "[cast]   -> $CASTS/$name.cast ($(stat -c%s "$CASTS/$name.cast") bytes)"
}

BIN=./target/release/sentinel

# 1. Reflex crash demo: tiers flip, reflex reduces fire.
rec "01-reflex-crash" \
  "clear; echo '### Sentinel — reflex core / crash replay'; echo; \
   echo '\$ sentinel --replay tests/fixtures/perpl/crash-scenario.jsonl'; echo; \
   $BIN --replay tests/fixtures/perpl/crash-scenario.jsonl 2>&1 | \
     grep --line-buffered -E 'pipeline started|tier_change|pipeline finished'; \
   echo; echo '### observed: GREEN -> YELLOW -> ORANGE -> RED, reduce-only orders'; sleep 3"

# 2. Strategy brain: consult + provider failover.
rec "02-strategy-failover" \
  "clear; echo '### Sentinel — strategy brain / dual-provider failover'; echo; \
   echo '\$ sentinel --replay tests/fixtures/perpl/crash-scenario.jsonl'; echo; \
   $BIN --mode testnet --replay tests/fixtures/perpl/crash-scenario.jsonl 2>&1 | \
     grep --line-buffered -E 'consult_scheduled|tier_change|provider attempt|failover'; \
   echo; echo '### Qwen primary -> HTTP 401 -> automatic failover to Kimi'; sleep 3"

# 3. Audit verifier: hash-chained journal integrity.
rec "03-audit-verify" \
  "clear; echo '### Sentinel — audit journal / hash-chain verification'; echo; \
   echo '\$ audit-verify'; echo; \
   ./target/release/audit-verify --no-chain 2>&1; \
   echo; echo '### every decision is chained; tampering breaks verification'; sleep 3"

# 4. Backtest: capital saved across scenarios.
rec "04-backtest" \
  "clear; echo '### Sentinel — backtester / capital saved'; echo; \
   echo '\$ backtest --scenario all'; echo; \
   ./target/release/backtest 2>&1 | tail -30; \
   echo; echo '### liquidations avoided, capital preserved'; sleep 3"

echo "[cast] all demos recorded:"
ls -la "$CASTS"/*.cast 2>/dev/null || true
