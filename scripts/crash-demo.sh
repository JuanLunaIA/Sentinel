#!/usr/bin/env bash
# ==========================================================================
# Sentinel P06 — crash rehearsal (the golden-path demo).
#
# Replays tests/fixtures/perpl/crash-scenario.jsonl (SPEC-P06.md §7): an ETH
# long sliding 30 % → 6 % distance to liquidation in ~80 s of wall time
# (600 s of logical market time), DRY_RUN executor, zero manual steps.
#
# Expected save sequence:
#   🟡 Yellow  → consult scheduled (strategy brain lands in P07)
#   🟠 Orange  → reflex reduce 25 % (dry-run fill)
#   🔴 Red     → reflex reduce 50 % (dry-run fill)
#   alert per step; process exits cleanly at fixture end.
#
# Demo thresholds are widened ON PURPOSE (tiny isolated-margin account, no
# approval queue until P11): the production defaults live in .env.example.
# ==========================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

export EXECUTION_MODE=DRY_RUN
export RUST_LOG="${RUST_LOG:-sentinel=info}"
export LOG_FORMAT=pretty
# Replay pacing: 1:1 logical ms, capped at 3.2 s per step (~80 s total).
export SENTINEL_MOCK_PACE=1
export SENTINEL_MOCK_CAP_MS="${SENTINEL_MOCK_CAP_MS:-3200}"
# Demo policy: testnet markets only, caps high enough for the save sequence.
# Cooldown 170 s (logical): evaluations run per feed event, and multi-market
# frames are iterated in sorted id order (BTC before ETH), so the Orange
# cadence must be gated across the whole 150 s crash gap; the Red first-breach
# reduce then fires at the next eligible evaluation (+175 s).
export MARKET_ALLOWLIST=32,16
export REFLEX_COOLDOWN_SECS="${REFLEX_COOLDOWN_SECS:-170}"
export MAX_ORDER_SIZE_USD=100000
export REQUIRE_APPROVAL_ABOVE_USD=100000

EVIDENCE=docs/evidence/p06-crash-demo.txt
mkdir -p docs/evidence data

echo "=================================================================="
echo "  SENTINEL CRASH REHEARSAL — replay, DRY_RUN, zero manual steps"
echo "  fixture: tests/fixtures/perpl/crash-scenario.jsonl"
echo "  ETH long: 30% distance → crash → 6% distance (logical 600s)"
echo "=================================================================="

cargo run --quiet --bin sentinel -- \
  --mode dry-run \
  --replay tests/fixtures/perpl/crash-scenario.jsonl 2>&1 | tee "$EVIDENCE"

echo "=================================================================="
echo "  rehearsal complete — evidence: $EVIDENCE"
echo "=================================================================="
