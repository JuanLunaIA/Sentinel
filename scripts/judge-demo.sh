#!/usr/bin/env bash
# ==========================================================================
# Sentinel P20 §4 — unattended judge demo (zero keys, deterministic replay).
#
# The tour (default path — no keys, no network, no docker):
#   [0/5] builds once: cargo build --release --bin sentinel --bin backtest
#         --bin audit-verify   (skip with JUDGE_DEMO_SKIP_BUILD=1)
#   [1/5] starts the daemon in REPLAY (tests/fixtures/perpl/crash-scenario.jsonl)
#         serving /healthz + the dashboard on http://127.0.0.1:${JUDGE_DEMO_PORT:-8090},
#         paced so the whole tier cascade is visible inside the hold window
#         (SENTINEL_MOCK_PACE=1, SENTINEL_MOCK_CAP_MS=800 ≈ 21 s wall: Yellow
#         +7 s → Orange +18 s → Red +20 s → replay end; override the pacing
#         with SENTINEL_MOCK_PACE / SENTINEL_MOCK_CAP_MS for a slower watch)
#   [2/5] waits for GET /healthz, then prints the TOUR block
#   [3/5] runs `backtest --scenario all` (fresh report; the dashboard Backtest
#         panel reads docs/backtest-report.json) and `audit-verify --no-chain`
#   [4/5] holds the dashboard open for JUDGE_DEMO_HOLD_SECS (default 20;
#         0 = no hold) so a viewer can click through the panels
#   [5/5] cleans up (trap) and exits 0
#
# `--docker` mode instead ends by shelling to compose: image build (unless
# JUDGE_DEMO_SKIP_BUILD=1) then `docker compose up -d`, a bounded /healthz
# wait on :8080 and the docker TOUR block; the stack is left running
# (`docker compose down` stops it).
#
# Env knobs:
#   JUDGE_DEMO_PORT         dashboard port for the local tour (default 8090)
#   JUDGE_DEMO_HOLD_SECS    seconds to keep the dashboard open after the
#                           report step (default 20; 0 = no hold)
#   JUDGE_DEMO_CAPTURE      1 => tee the full transcript to
#                           docs/evidence/p20-judge-demo.txt
#   JUDGE_DEMO_SKIP_BUILD   1 => reuse the existing release binaries / image
#   JUDGE_DEMO_IMAGE        docker image tag (default sentinel:p16)
#   SENTINEL_MOCK_PACE / SENTINEL_MOCK_CAP_MS   replay pacing overrides
#
# Exit codes: 0 = tour completed; 1 = precondition/step failure; 2 = usage.
# ==========================================================================
set -euo pipefail

cd "$(dirname "$0")/.."
export LC_ALL=C

DOCKER_MODE=0
for arg in "$@"; do
  case "$arg" in
    --docker) DOCKER_MODE=1 ;;
    *)
      echo "judge-demo: unknown argument ${arg} (supported: --docker)" >&2
      exit 2
      ;;
  esac
done

# ---- transcript wrapper (JUDGE_DEMO_CAPTURE=1) -----------------------------
if [ "${JUDGE_DEMO_CAPTURE:-0}" = "1" ] && [ "${JUDGE_DEMO_INNER:-0}" != "1" ]; then
  mkdir -p docs/evidence
  EVIDENCE=docs/evidence/p20-judge-demo.txt
  set +e
  JUDGE_DEMO_INNER=1 bash "$0" "$@" 2>&1 | tee "$EVIDENCE"
  rc=${PIPESTATUS[0]}
  set -e
  echo "[judge-demo] transcript written to $EVIDENCE (exit $rc)"
  exit "$rc"
fi

# ---- configuration ----------------------------------------------------------
PORT="${JUDGE_DEMO_PORT:-8090}"
HOLD="${JUDGE_DEMO_HOLD_SECS:-20}"
FIXTURE="tests/fixtures/perpl/crash-scenario.jsonl"
DAEMON_URL="http://127.0.0.1:${PORT}"
DASHBOARD_URL="http://localhost:${PORT}/"
DAEMON_LOG="logs/p20-judge-demo-daemon.log"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/p20-judge-demo.XXXXXX")"
DAEMON_PID=""

if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  echo "judge-demo: JUDGE_DEMO_PORT must be a port number (got '${PORT}')" >&2
  exit 2
fi
if ! [[ "$HOLD" =~ ^[0-9]+$ ]]; then
  echo "judge-demo: JUDGE_DEMO_HOLD_SECS must be a non-negative integer (got '${HOLD}')" >&2
  exit 2
fi

mkdir -p docs/evidence data logs

# ---- helpers ------------------------------------------------------------------
port_busy() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${port}\$"
  else
    (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null
  fi
}

cleanup() {
  local code=$?
  set +e
  if [ -n "$DAEMON_PID" ]; then
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
      kill "$DAEMON_PID" 2>/dev/null
      for _ in $(seq 1 20); do
        kill -0 "$DAEMON_PID" 2>/dev/null || break
        sleep 0.25
      done
      kill -9 "$DAEMON_PID" 2>/dev/null
    fi
    wait "$DAEMON_PID" 2>/dev/null
  fi
  if [ "$code" -ne 0 ] && [ -f "$DAEMON_LOG" ]; then
    echo "[cleanup] nonzero exit ($code) — daemon log tail:"
    tail -n 30 "$DAEMON_LOG" || true
  fi
  rm -rf "$WORK"
  echo "[cleanup] daemon stopped; work dir removed (script exit $code)"
  exit "$code"
}
trap cleanup EXIT

wait_healthz() { # base_url max_secs
  local url="$1" max="$2" waited=0 body
  while [ "$waited" -lt "$max" ]; do
    if body="$(curl -fsS --max-time 2 "$url/healthz" 2>/dev/null)"; then
      echo "healthz: $body"
      return 0
    fi
    if [ -n "$DAEMON_PID" ] && ! kill -0 "$DAEMON_PID" 2>/dev/null; then
      echo "[judge-demo] FATAL: the daemon exited before /healthz answered" >&2
      tail -n 30 "$DAEMON_LOG" || true
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "[judge-demo] FATAL: /healthz did not answer within ${max}s" >&2
  return 1
}

# ---- preflight ----------------------------------------------------------------
command -v curl >/dev/null || { echo "judge-demo: FATAL: curl is required" >&2; exit 1; }

echo "=================================================================="
echo "  SENTINEL — JUDGE DEMO (zero keys · deterministic replay)"
echo "  date: $(date -Is)"
echo "=================================================================="

# ---- docker mode (--docker): ends by shelling to compose ------------------------
if [ "$DOCKER_MODE" = "1" ]; then
  echo
  echo "== docker mode: image + compose =="
  # Docker access: plain docker, else the NOPASSWD `sudo -n docker` relay used
  # by scripts/docker-verify.sh on hosts where the user is not in the group.
  DOCKER=(docker)
  if ! docker info >/dev/null 2>&1; then
    if sudo -n docker info >/dev/null 2>&1; then
      DOCKER=(sudo -n docker)
      echo "   note: talking to dockerd via 'sudo -n docker'"
    else
      echo "judge-demo: FATAL: cannot talk to the docker daemon (docker info failed)" >&2
      exit 1
    fi
  fi
  if port_busy 8080; then
    echo "   note: host port 8080 is busy — compose will fail to bind (see scripts/docker-verify.sh for a port-override overlay)"
  fi
  IMAGE="${JUDGE_DEMO_IMAGE:-sentinel:p16}"
  if [ "${JUDGE_DEMO_SKIP_BUILD:-0}" = "1" ]; then
    echo "   JUDGE_DEMO_SKIP_BUILD=1 — reusing image ${IMAGE}"
    "${DOCKER[@]}" image inspect "$IMAGE" --format '   image: {{.Id}}' >/dev/null
  else
    echo "== docker build (multi-stage release) =="
    # --network=host: some hosts (this one) drop container-bridge egress.
    "${DOCKER[@]}" build --network=host \
      --build-arg "SENTINEL_UID=$(id -u)" \
      --build-arg "SENTINEL_GID=$(id -g)" \
      -t "$IMAGE" .
  fi
  if [ ! -f .env ]; then
    echo "   note: .env missing — copying the placeholder template (.env.example)"
    cp .env.example .env
  fi
  echo "== docker compose up -d =="
  "${DOCKER[@]}" compose up -d
  echo "waiting for /healthz on http://127.0.0.1:8080/ (<= 120s) ..."
  DOCKER_HEALTH=""
  for _ in $(seq 1 120); do
    if DOCKER_HEALTH="$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null)"; then
      break
    fi
    sleep 1
  done
  if [ -z "$DOCKER_HEALTH" ]; then
    echo "judge-demo: FATAL: /healthz did not answer within 120s — compose ps + logs follow" >&2
    "${DOCKER[@]}" compose ps || true
    "${DOCKER[@]}" compose logs --tail=40 sentinel || true
    exit 1
  fi
  echo "healthz: ${DOCKER_HEALTH}"
  cat <<'EOF'

==================================================================
  THE TOUR (docker compose) — open http://localhost:8080/
==================================================================
  Compose started sentinel + breaker from the compose stack
  (docker-compose.yml; journal/state ride the ./data volume).

  READ THE PANELS IN THIS ORDER
    1) POSITIONS — ETH #32 / BTC #16 cards with distance-to-liquidation
       and the tier badge (live modes flip tiers on real moves).
    2) LIVE DECISION FEED — evaluations/alerts as they happen.
    3) AUDIT VERIFIER — the SHA-256 journal chain the daemon is writing;
       "verify now" re-walks ./data/audit.
    4) BACKTEST — docs/backtest-report.json (regenerate on the host with
       `cargo run --bin backtest -- --scenario all`).

  With only placeholder keys in .env the sentinel container exits at the
  first signed Perpl call — the zero-key end-to-end tour is the default
  path (`bash scripts/judge-demo.sh`, no --docker). STUB-17/24 apply.
==================================================================
EOF
  echo
  echo "the compose stack is left running — stop it with: docker compose down"
  echo "exit 0 (docker tour complete)"
  exit 0
fi

# ---- local path ----------------------------------------------------------------
[ -f "$FIXTURE" ] || { echo "judge-demo: FATAL: fixture missing: $FIXTURE" >&2; exit 1; }
if port_busy "$PORT"; then
  echo "judge-demo: FATAL: port $PORT is already in use — set JUDGE_DEMO_PORT to a free port" >&2
  exit 1
fi

echo
echo "== [0/5] build (release: sentinel + backtest + audit-verify) =="
if [ "${JUDGE_DEMO_SKIP_BUILD:-0}" = "1" ]; then
  echo "   JUDGE_DEMO_SKIP_BUILD=1 — reusing existing target/release binaries"
else
  cargo build --release --bin sentinel --bin backtest --bin audit-verify
fi
for bin in sentinel backtest audit-verify; do
  [ -x "target/release/$bin" ] || {
    echo "judge-demo: FATAL: target/release/$bin missing (rerun without JUDGE_DEMO_SKIP_BUILD=1)" >&2
    exit 1
  }
done

echo
echo "== [1/5] daemon: replay $FIXTURE on $DAEMON_URL =="
(
  export EXECUTION_MODE=DRY_RUN
  export LOG_FORMAT=pretty
  export RUST_LOG="${RUST_LOG:-sentinel=info}"
  # Visible demo pace: 1:1 logical ms capped per step — ≈21 s wall for the
  # whole fixture, with the cascade inside the hold window (Yellow ≈ +7 s,
  # Orange ≈ +18 s, Red + the 50% reduce ≈ +20 s, replay end ≈ +21 s).
  # Override SENTINEL_MOCK_CAP_MS (e.g. 3200 ≈ 75 s) for a slower watch.
  export SENTINEL_MOCK_PACE="${SENTINEL_MOCK_PACE:-1}"
  export SENTINEL_MOCK_CAP_MS="${SENTINEL_MOCK_CAP_MS:-800}"
  # Demo policy (same widened thresholds as scripts/crash-demo.sh): the tiny
  # isolated-margin fixture account must produce the full save sequence.
  export MARKET_ALLOWLIST="${MARKET_ALLOWLIST:-32,16}"
  export REFLEX_COOLDOWN_SECS="${REFLEX_COOLDOWN_SECS:-170}"
  export MAX_ORDER_SIZE_USD="${MAX_ORDER_SIZE_USD:-100000}"
  export REQUIRE_APPROVAL_ABOVE_USD="${REQUIRE_APPROVAL_ABOVE_USD:-100000}"
  export PORT="$PORT"
  exec ./target/release/sentinel --mode dry-run --replay "$FIXTURE"
) >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
echo "   daemon pid $DAEMON_PID · log: $DAEMON_LOG"

echo
echo "== [2/5] wait for /healthz =="
wait_healthz "$DAEMON_URL" 60 || exit 1

cat <<EOF

==================================================================
  THE TOUR — open ${DASHBOARD_URL}
==================================================================
  The daemon is replaying a recorded ETH crash (≈30% → 6% distance
  to liquidation) through the real decision pipeline: DRY_RUN
  executor, zero keys, zero network. At default pacing the replay
  runs ≈21 s: Yellow ≈ +7 s, Orange ≈ +18 s, Red + the 50 % reduce
  ≈ +20 s. (Prefer a slower watch? SENTINEL_MOCK_CAP_MS=3200 ≈ 75 s.)

  READ THE PANELS IN THIS ORDER
  ------------------------------------------------------------------
  1) POSITIONS (top left)
     ETH #32 / BTC #16 cards: entry → mark, distance-to-liquidation,
     collateral and the tier badge. Watch ETH slip
     GREEN → YELLOW → ORANGE → RED as the crash replays; BTC stays
     GREEN as the contrast.
  2) LIVE DECISION FEED (below positions)
     Every evaluation as it happens: tier_change alerts,
     consult_scheduled on Yellow, reflex_action reduces with DRY_RUN
     fills (sentinel-32-1, sentinel-32-2).
  3) AUDIT VERIFIER (right column)
     The SHA-256 journal chain: verified entries, valid-up-to seq,
     first seq, anchored seq / running root / latest anchor tx.
     Zero-key local replay: the chain verifies locally (0 anchored —
     no wallet configured; live anchors are PENDING-WALLET, STUB-17).
     Press "verify now" — it re-walks data/audit in place.
  4) BACKTEST (bottom)
     Fresh output from this run (regenerated in the next step): 14
     crash scenarios, \$60,163.37 notional, \$5,972.14 saved (9.93%),
     baseline liquidations 11 → with Sentinel: 0.
  ------------------------------------------------------------------
  Also on the page: the Resilience strip (heartbeat/breaker/CRE),
  the Nansen x402 spend panel and the kill switch (PAUSE needs
  DASHBOARD_ADMIN_KEY; it is unset here, so mutations stay disabled).

  The daemon exits when the replay fixture ends (≈21 s at default
  pacing). This script holds the dashboard open for ${HOLD} more
  second(s) after the report step (JUDGE_DEMO_HOLD_SECS), then stops
  it.
==================================================================
EOF

echo
echo "== [3/5] fresh report: backtest --scenario all =="
# Default --out docs/backtest-report (json + md) — deterministic and exactly
# what the dashboard Backtest panel serves.
./target/release/backtest --scenario all
echo
echo "== [3/5] fresh report: audit-verify --no-chain =="
./target/release/audit-verify --no-chain

echo
echo "== [4/5] hold: dashboard live for ${HOLD}s (JUDGE_DEMO_HOLD_SECS) =="
if [ "$HOLD" -gt 0 ]; then
  if kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "   dashboard: ${DASHBOARD_URL} — click through the panels now"
  else
    echo "   note: the replay already finished (daemon exited); rerun for another pass"
  fi
  waited=0
  while [ "$waited" -lt "$HOLD" ]; do
    sleep 1
    waited=$((waited + 1))
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
      echo "   replay finished at ${waited}s into the hold; the dashboard is now offline"
      break
    fi
  done
else
  echo "   JUDGE_DEMO_HOLD_SECS=0 — no hold"
fi

echo
echo "== [5/5] summary =="
echo "   dashboard:    ${DASHBOARD_URL} (stopping now)"
echo "   daemon log:   ${DAEMON_LOG}"
echo "--- daemon alerts so far ---"
grep -E "sentinel::alert|pipeline finished" "$DAEMON_LOG" | tail -n 12 || true
echo
echo "=================================================================="
echo "  JUDGE TOUR COMPLETE (exit 0)"
echo "  fresh report:  docs/backtest-report.{json,md} (Backtest panel source)"
echo "  verify again:  ./target/release/audit-verify --no-chain"
echo "  replay again:  bash scripts/judge-demo.sh"
echo "  transcript:    JUDGE_DEMO_CAPTURE=1 bash scripts/judge-demo.sh"
echo "=================================================================="
