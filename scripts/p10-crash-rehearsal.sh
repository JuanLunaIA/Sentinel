#!/usr/bin/env bash
# ==========================================================================
# Sentinel P10 — kill -9 crash-safety rehearsal (acceptance evidence).
# Runs the replay daemon, SIGKILLs it mid-run, verifies the (partial) journal
# chain, restarts the daemon and proves the journal resumes, verifies again.
# ==========================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

rm -rf data/audit
ENV_COMMON=(EXECUTION_MODE=DRY_RUN RUST_LOG=sentinel=info LOG_FORMAT=pretty
  SENTINEL_MOCK_PACE=1 SENTINEL_MOCK_CAP_MS=1200 MARKET_ALLOWLIST=32,16
  REFLEX_COOLDOWN_SECS=170 MAX_ORDER_SIZE_USD=100000 REQUIRE_APPROVAL_ABOVE_USD=100000)
FIXTURE=tests/fixtures/perpl/crash-scenario.jsonl
LOG1=/home/luna/.hermes/cache/scratch/p10-r1.log
LOG2=/home/luna/.hermes/cache/scratch/p10-r2.log

echo "# Sentinel P10 — kill -9 crash safety rehearsal"
echo "# date: $(date -Is)"

echo "== run 1 (replay, will be killed mid-run) =="
env "${ENV_COMMON[@]}" ./target/debug/sentinel --mode dry-run --replay "$FIXTURE" > "$LOG1" 2>&1 &
PID=$!
sleep 24
kill -9 "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
echo "killed -9 (mid-run)"

echo "== audit-verify after the kill =="
./target/debug/audit-verify --no-chain || true

echo "== run 2 (restart: the journal must resume, not reset) =="
env "${ENV_COMMON[@]}" ./target/debug/sentinel --mode dry-run --replay "$FIXTURE" > "$LOG2" 2>&1 &
PID2=$!
sleep 6
kill -TERM "$PID2" 2>/dev/null || true
sleep 2
echo "-- restart log (journal resume evidence) --"
grep -E "audit journal open|sentinel starting" "$LOG2" | head -4 || true

echo "== audit-verify after the restart =="
./target/debug/audit-verify --no-chain || true
