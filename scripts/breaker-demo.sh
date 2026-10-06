#!/usr/bin/env bash
# ==========================================================================
# Sentinel P14 — unattended dead-man's-switch demo (SPEC-P14 §7).
#
# Sequence (all local, no docker, no sudo, no external network):
#   (0) build once: breaker --release + sentinel
#   (1) anvil on 127.0.0.1:8547 -> forge create SentinelAuditAnchor (anvil key #0)
#   (2) daemon: replay tests/fixtures/perpl/crash-scenario.jsonl with the
#       on-chain anchor + heartbeats every 8 s
#   (3) breaker: dry_run, watching the same anvil, interval 8 s, snapshot file
#       tests/fixtures/p14/breaker-snapshot.json (positions converted from the
#       crash scenario; see derivation notes below)
#   (4) show /api/heartbeat-status with stale=false (fresh)
#   (5) kill -9 the daemon (the "unresponsive Sentinel" moment)
#   (6) poll until stale (> 3 x 8 s) -> play the CRE dead-man's-switch hop by
#       POSTing /breaker/trigger with the frozen HMAC signature -> 202 fired ->
#       journal line in data/breaker-journal.jsonl + armed in status
#   (7) summary; cleanup kills every process and removes the /tmp work dir.
#
# Exit code: 0 only when the full sequence succeeds.
#
# ---------------------------------------------------------------------------
# Fixture derivation (tests/fixtures/p14/breaker-snapshot.json): positions are
# the crash-scenario's final REST/WS positions converted exactly like
# crates/sentinel/src/perpl/types.rs::position_from_raw does:
#   ETH mkt 32: size 10000 (3 dec) -> 10, entry 270000 (2 dec) -> 2700,
#               collateral 13560000000 (6 dec) -> 13560, lv 200 -> 2x,
#               last ticker mark 157340 -> 1573.4;
#               derived liq = entry + (entry*|size|*mm - collateral)/|size|
#               with mm = 100/2000 = 0.05 -> 1479
#               => distance_to_liq = |1573.4 - 1479| / 1573.4 = 6.0% (riskiest)
#   BTC mkt 16: size 100 (3 dec) -> 0.1, entry 950000 (1 dec) -> 95000,
#               collateral 3800000000 (6 dec) -> 3800, mark 950000 -> 95000,
#               liq -> 61750 => distance 35%.
# The spec-literal `Position[]` array carries explicit liq prices because the
# array form has no market table (the breaker's documented fallback picks the
# riskiest position from the exchange-style liq price present here).
# ==========================================================================
set -euo pipefail

# ---- transcript wrapper: re-exec under tee so the evidence file is complete --
if [[ "${1:-}" != "--inner" ]]; then
  cd "$(dirname "$0")/.."
  mkdir -p docs/evidence
  EVIDENCE=docs/evidence/p14-breaker-demo.txt
  set +e
  bash "$0" --inner 2>&1 | tee "$EVIDENCE"
  rc=${PIPESTATUS[0]}
  set -e
  echo "[breaker-demo] transcript written to $EVIDENCE (exit $rc)"
  exit "$rc"
fi

cd "$(dirname "$0")/.."
export LC_ALL=C
mkdir -p docs/evidence data

# ---------------------------------------------------------------------------
# Configuration (overridable via env)
# ---------------------------------------------------------------------------
ANVIL_PORT="${ANVIL_PORT:-8547}"
RPC="http://127.0.0.1:${ANVIL_PORT}"
ANVIL_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
GUARDIAN="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
BREAKER_PORT="${BREAKER_PORT:-9090}"
BREAKER_URL="http://127.0.0.1:${BREAKER_PORT}"
ARM_SECRET="${BREAKER_ARM_SECRET:-demo-secret}"
HB_SECS="${HEARTBEAT_INTERVAL_SECS:-8}"
SNAPSHOT_FILE="tests/fixtures/p14/breaker-snapshot.json"
FIXTURE="tests/fixtures/perpl/crash-scenario.jsonl"
JOURNAL="${BREAKER_JOURNAL:-data/breaker-journal.jsonl}"
ALERT_PREFIX="BREAKER: Sentinel unresponsive"
DAEMON_PORT=18080
WORK="$(mktemp -d /tmp/p14-breaker-demo.XXXXXX)"

ANVIL_PID=""
DAEMON_PID=""
BREAKER_PID=""

cleanup() {
  local code=$?
  set +e
  if [ "$code" -ne 0 ]; then
    echo "[cleanup] nonzero exit ($code) — log tails for diagnosis:"
    if [ -f "$WORK/breaker.log" ]; then echo "--- breaker.log ---"; tail -n 40 "$WORK/breaker.log"; fi
    if [ -f "$WORK/daemon.log" ]; then echo "--- daemon.log ---"; tail -n 40 "$WORK/daemon.log"; fi
  fi
  for pid in "$DAEMON_PID" "$BREAKER_PID"; do
    [ -n "$pid" ] || continue
    kill -9 -- "-$pid" 2>/dev/null   # process group (started with setsid)
    kill -9 "$pid" 2>/dev/null
  done
  pkill -9 -f "replay tests/fixtures/perpl/crash-scenario.jsonl" 2>/dev/null
  pkill -9 -f "target/release/breaker" 2>/dev/null
  if [ -n "$ANVIL_PID" ]; then kill -9 "$ANVIL_PID" 2>/dev/null; fi
  rm -rf "$WORK"
  echo "[cleanup] all processes killed; work dir $WORK removed (script exit $code)"
  exit "$code"
}
trap cleanup EXIT

log() { echo "[breaker-demo] $*"; }

wait_status() { # jq-expr max_secs description
  local expr="$1" max="$2" desc="$3" waited=0 tick=0
  while (( waited < max )); do
    if curl -sf "$BREAKER_URL/api/heartbeat-status" | jq -e "$expr" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
    waited=$((waited + 2))
    tick=$((tick + 1))
    if (( tick % 5 == 0 )); then
      echo "    ... waiting for $desc (${waited}s elapsed)"
      curl -sf "$BREAKER_URL/api/heartbeat-status" | jq -c '.guardians[]? | {address, age_secs, max_tier, stale, critical, armed}' 2>/dev/null || true
    fi
  done
  echo "[timeout] waiting for: $desc"
  return 1
}

echo "=================================================================="
echo "  SENTINEL P14 — DEAD-MAN'S-SWITCH DEMO (unattended)"
echo "  date:      $(date -Is)"
echo "  fixture:   $FIXTURE"
echo "  snapshot:  $SNAPSHOT_FILE"
echo "  work dir:  $WORK"
echo "=================================================================="

# --- preflight -------------------------------------------------------------
command -v anvil >/dev/null || { echo "fatal: anvil not found in PATH"; exit 1; }
command -v cast >/dev/null || { echo "fatal: cast not found in PATH"; exit 1; }
command -v jq >/dev/null || { echo "fatal: jq not found in PATH"; exit 1; }
command -v openssl >/dev/null || { echo "fatal: openssl not found in PATH"; exit 1; }
[ -f "$FIXTURE" ] || { echo "fatal: fixture missing: $FIXTURE"; exit 1; }
[ -f "$SNAPSHOT_FILE" ] || { echo "fatal: snapshot missing: $SNAPSHOT_FILE"; exit 1; }
if (exec 3<>"/dev/tcp/127.0.0.1/$BREAKER_PORT") 2>/dev/null; then
  echo "fatal: port $BREAKER_PORT already in use (set BREAKER_PORT)"; exit 1
fi

# How the two rust processes are launched. Default: the freshly built binaries
# (step 0 built them; `cargo run` is equivalent). Set P14_CARGO_RUN=1 to use
# `cargo run` instead — it re-checks freshness and can block on the cargo
# build lock when other cargo jobs are running.
RUN_DAEMON=(./target/debug/sentinel --replay "$FIXTURE")
RUN_BREAKER=(./target/release/breaker)
if [ "${P14_CARGO_RUN:-0}" = "1" ]; then
  RUN_DAEMON=(cargo run --quiet --bin sentinel -- --replay "$FIXTURE")
  RUN_BREAKER=(cargo run --quiet -p breaker --release)
fi

# --- (0) build once --------------------------------------------------------
echo
echo "== (0) build: breaker --release + sentinel =="
(cd contracts && forge build -q) && echo "contracts: build OK"
cargo build --release -p breaker
cargo build --quiet --bin sentinel
[ -x target/release/breaker ] || { echo "fatal: target/release/breaker missing after build"; exit 1; }
[ -x target/debug/sentinel ] || { echo "fatal: target/debug/sentinel missing after build"; exit 1; }
echo "rust: build OK"

# --- (1) anvil + anchor deploy --------------------------------------------
echo
echo "== (1) anvil on $RPC + deploy SentinelAuditAnchor =="
pkill -f "anvil --silent --port $ANVIL_PORT" 2>/dev/null || true
anvil --silent --port "$ANVIL_PORT" >"$WORK/anvil.log" 2>&1 &
ANVIL_PID=$!
for _ in $(seq 1 40); do
  if cast block-number --rpc-url "$RPC" >/dev/null 2>&1; then break; fi
  sleep 0.25
done
cast block-number --rpc-url "$RPC" >/dev/null || { echo "fatal: anvil did not come up"; exit 1; }
echo "anvil up (pid $ANVIL_PID), chain $(cast chain-id --rpc-url "$RPC")"

ADDR="$(cd contracts && forge create src/SentinelAuditAnchor.sol:SentinelAuditAnchor \
  --rpc-url "$RPC" --private-key "$ANVIL_KEY" --broadcast 2>/dev/null \
  | awk '/Deployed to:/ {print $3}')"
[ -n "$ADDR" ] || { echo "fatal: forge create produced no address"; exit 1; }
echo "SentinelAuditAnchor deployed at: $ADDR"

# --- (2) daemon: replay + on-chain heartbeats ------------------------------
echo
echo "== (2) daemon: replay $FIXTURE (heartbeats every ${HB_SECS}s, anchor $ADDR) =="
echo "    (\`cargo run --bin sentinel -- --replay …\` with the same env; the built"
echo "     binary is started directly by default so a concurrent cargo job cannot"
echo "     hold the lock — set P14_CARGO_RUN=1 for literal cargo run)"
setsid env \
  ENABLE_ANCHOR=true \
  ANCHOR_CONTRACT_ADDRESS="$ADDR" \
  RPC_SIGNER_KEY="$ANVIL_KEY" \
  HEARTBEAT_INTERVAL_SECS="$HB_SECS" \
  PERPL_RPC_URL="$RPC" \
  EXECUTION_MODE=DRY_RUN \
  LOG_FORMAT=pretty \
  RUST_LOG="${RUST_LOG:-sentinel=info}" \
  PORT="$DAEMON_PORT" \
  SENTINEL_MOCK_PACE=1 \
  SENTINEL_MOCK_CAP_MS=1200 \
  "${RUN_DAEMON[@]}" \
  >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!
echo "daemon started (pid $DAEMON_PID, logs: $WORK/daemon.log)"

# --- (3) breaker: dry_run watching the same anvil --------------------------
echo
echo "== (3) breaker: dry_run, anchor $ADDR, guardian $GUARDIAN, port $BREAKER_PORT =="
echo "    (\`cargo run -p breaker --release\` with the same env; built binary started directly)"
setsid env \
  BREAKER_RPC_URL="$RPC" \
  BREAKER_ANCHOR_ADDRESS="$ADDR" \
  BREAKER_GUARDIANS="$GUARDIAN" \
  BREAKER_HEARTBEAT_INTERVAL_SECS="$HB_SECS" \
  BREAKER_MODE=dry_run \
  BREAKER_SNAPSHOT_FILE="$SNAPSHOT_FILE" \
  BREAKER_ARM_SECRET="$ARM_SECRET" \
  BREAKER_PORT="$BREAKER_PORT" \
  RUST_LOG="${RUST_LOG_BREAKER:-breaker=info,sentinel::alert=warn}" \
  "${RUN_BREAKER[@]}" \
  >"$WORK/breaker.log" 2>&1 &
BREAKER_PID=$!
echo "breaker started (pid $BREAKER_PID, logs: $WORK/breaker.log)"

echo "waiting for the breaker HTTP surface ..."
if ! wait_status '.guardians' 60 "breaker /api/heartbeat-status"; then
  echo "--- breaker log tail ---"; tail -n 30 "$WORK/breaker.log" || true
  exit 1
fi

# --- (4) fresh heartbeat ----------------------------------------------------
echo
echo "== (4) wait for a fresh heartbeat (stale=false) =="
wait_status '.guardians[0].stale == false' 120 "a fresh heartbeat on the anchor (stale=false)"
echo "status (fresh):"
curl -s "$BREAKER_URL/api/heartbeat-status" | jq .
echo "on-chain Heartbeat events (cast logs):"
cast logs --rpc-url "$RPC" --address "$ADDR" "Heartbeat(address,bytes32,uint32,uint8)" | tail -n 12

# --- (5) kill the daemon ----------------------------------------------------
echo
echo "== (5) kill -9 the daemon (simulated unresponsive Sentinel) =="
kill -9 -- "-$DAEMON_PID" 2>/dev/null || kill -9 "$DAEMON_PID" 2>/dev/null || true
for _ in $(seq 1 20); do
  pgrep -f "replay tests/fixtures/perpl/crash-scenario.jsonl" >/dev/null || break
  sleep 0.5
done
if pgrep -f "replay tests/fixtures/perpl/crash-scenario.jsonl" >/dev/null; then
  echo "fatal: daemon still running after kill -9"; exit 1
fi
echo "daemon dead; no more heartbeats will be posted"

# Journal baseline BEFORE the fire: the first new line IS the fire we want.
BASE_LINES=0
[ -f "$JOURNAL" ] && BASE_LINES="$(wc -l < "$JOURNAL")"
[[ "$BASE_LINES" =~ ^[0-9]+$ ]] || BASE_LINES=0
echo "journal baseline: $BASE_LINES line(s) in $JOURNAL"

# --- (6) staleness -> signed trigger -> fire -> journal + alert -------------
echo
echo "== (6) wait for stale (age > $((3 * HB_SECS))s), then fire =="
wait_status '.guardians[0].stale == true' 90 "staleness (age > 3 x ${HB_SECS}s)"
echo "status (stale):"
curl -s "$BREAKER_URL/api/heartbeat-status" | jq .

NOW_MS="$(date +%s%3N)"
BODY="$(printf '{"guardian":"%s","reason":"sentinel-unresponsive-heartbeat-stale-critical","requested_at_ms":%s}' "$GUARDIAN" "$NOW_MS")"
SIG="$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$ARM_SECRET" -hex | awk '{print $NF}')"
echo "trigger body: $BODY"
echo "X-Breaker-Signature: sha256=$SIG  (HMAC-SHA256 over the raw body)"
echo "(this POST is the frozen CRE dead-man's-switch hop — the exact request"
echo " workflow-cre/sentinel-deadman/ sends when a guardian is stale && critical;"
echo " the breaker's own 5 s auto-path may alternatively fire first — the shared"
echo " one-fire-per-epoch gate makes the two paths idempotent.)"

RESP="$(curl -s -w '\n%{http_code}' -X POST "$BREAKER_URL/breaker/trigger" \
  -H 'Content-Type: application/json' -H "X-Breaker-Signature: sha256=$SIG" \
  --data-raw "$BODY")"
CODE="$(printf '%s' "$RESP" | tail -n1)"
RESP_BODY="$(printf '%s' "$RESP" | sed '$d')"
echo "POST /breaker/trigger -> HTTP $CODE $RESP_BODY"
if [ "$CODE" != "202" ]; then
  echo "fatal: expected HTTP 202 from /breaker/trigger, got $CODE"
  echo "--- breaker log tail ---"; tail -n 40 "$WORK/breaker.log" || true
  exit 1
fi

echo
echo "waiting for the journal line in $JOURNAL ..."
for _ in $(seq 1 60); do
  if [ -f "$JOURNAL" ] && [ "$(wc -l < "$JOURNAL")" -gt "$BASE_LINES" ]; then break; fi
  sleep 1
done
[ -f "$JOURNAL" ] && [ "$(wc -l < "$JOURNAL")" -gt "$BASE_LINES" ] || {
  echo "fatal: no new journal line appeared"; exit 1; }
echo "journal line (first fire):"
LINE="$(sed -n "$((BASE_LINES + 1))p" "$JOURNAL")"
printf '%s' "$LINE" | jq .

echo
echo "waiting for armed=true in the status (fired this epoch) ..."
wait_status '.guardians[0].armed == true' 30 "armed=true in the breaker status"

echo
echo "alert emitted by the breaker (frozen literal '$ALERT_PREFIX'):"
grep -F "$ALERT_PREFIX" "$WORK/breaker.log" | tail -n 2 || echo "  (no alert line found in the breaker log)"

# --- (7) summary ------------------------------------------------------------
echo
echo "== (7) summary =="
echo "  anchor contract:   $ADDR"
echo "  guardian:          $GUARDIAN"
echo "  heartbeats posted: $(cast logs --rpc-url "$RPC" --address "$ADDR" "Heartbeat(address,bytes32,uint32,uint8)" | grep -c 'blockNumber' || true) Heartbeat events on-chain"
echo "  trigger:           HTTP ${CODE:-n/a} (the CRE hop's signed POST /breaker/trigger)"
echo "  journal:           $LINE"
echo "  mode:              dry_run (no live orders; SPEC-P14 §4)"
echo
echo "--- breaker log tail ---"
tail -n 25 "$WORK/breaker.log" || true
echo "--- daemon log tail ---"
tail -n 12 "$WORK/daemon.log" || true
echo
echo "=================================================================="
echo "  DEMO COMPLETE — dead-man's-switch fired with exit 0"
echo "=================================================================="
