#!/usr/bin/env bash
# ==========================================================================
# Sentinel P16 — docker build + compose cycle + journal seq continuity.
#
# Proves the SPEC-P16 §1 acceptance substitute (evidence:
# docs/evidence/p16-docker.txt):
#   1. `docker build` — clean multi-stage release build of the workspace.
#   2. `docker compose up -d` — sentinel + breaker on the ./data volume.
#   3. GET /healthz answers on the host-mapped port.
#   4. The daemon runs in replay inside the container, journaling entries to
#      the mounted volume: `down` → record last seq → `up` → the first
#      appended entry carries seq = last + 1 (journal survives container
#      lifecycles and resumes — the P10 hash chain continues).
#
# Why the compose override:
# - The image CMD (the live daemon) exits at the first signed Perpl call
#   (HTTP 401) when only placeholder credentials exist, so a bare compose-up
#   on this host cannot keep /healthz up; the verify overlay pins the service
#   command to a deterministic replay (the exact acceptance wording: "start,
#   run replay producing entries").
# - Port 8080 may be occupied on this host (vaultwarden binds 127.0.0.1:8080)
#   → the script auto-selects 18080 via the same overlay. Override with
#   SENTINEL_HOST_PORT=<port>.
# - The image is built with SENTINEL_UID/GID = $(id -u)/$(id -g) so the
#   non-root container user can write the bind-mounted ./data directory.
#
# Docker access: `sudo -n docker` (this user is not in the docker group;
# NOPASSWD sudo is configured). Socket-relay fallback: indexer/README.md.
#
# Env knobs: SENTINEL_SKIP_BUILD=1 reuses an existing sentinel:p16 image
# (debug convenience — the evidence run must NOT set it), SENTINEL_IMAGE,
# SENTINEL_COMPOSE_PROJECT, SENTINEL_FIXTURE, SENTINEL_HOST_PORT,
# WAIT_HEALTHZ_SECS, WAIT_ENTRY_SECS.
#
# Evidence: the full transcript is teed to docs/evidence/p16-docker.txt.
# ==========================================================================
set -euo pipefail

# ---- transcript wrapper ---------------------------------------------------
if [[ "${1:-}" != "--inner" ]]; then
  cd "$(dirname "$0")/.."
  mkdir -p docs/evidence data
  EVIDENCE=docs/evidence/p16-docker.txt
  set +e
  bash "$0" --inner 2>&1 | tee "$EVIDENCE"
  rc=${PIPESTATUS[0]}
  set -e
  echo "[docker-verify] transcript written to $EVIDENCE (exit $rc)"
  exit "$rc"
fi

cd "$(dirname "$0")/.."
export LC_ALL=C

# ---- configuration ---------------------------------------------------------
IMAGE="${SENTINEL_IMAGE:-sentinel:p16}"
PROJECT="${SENTINEL_COMPOSE_PROJECT:-sentinel-p16}"
FIXTURE="${SENTINEL_FIXTURE:-tests/fixtures/perpl/crash-scenario.jsonl}"
WAIT_HEALTHZ_SECS="${WAIT_HEALTHZ_SECS:-120}"
WAIT_ENTRY_SECS="${WAIT_ENTRY_SECS:-240}"
SKIP_BUILD="${SENTINEL_SKIP_BUILD:-0}"
DOCKER=(sudo -n docker)

WORK="$(mktemp -d "${TMPDIR:-/tmp}/p16-docker.XXXXXX")"
OVERRIDE="$WORK/verify-override.yml"
BUILD_LOG="$WORK/build.log"
STACK_UP=0

# ---- helpers ---------------------------------------------------------------
COMPOSE=("${DOCKER[@]}" compose -p "$PROJECT" -f docker-compose.yml -f "$OVERRIDE")

cleanup() {
  local rc=$?
  if [[ "$STACK_UP" == "1" ]]; then
    echo "[cleanup] compose down (script exited with status $rc)"
    "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
  exit "$rc"
}
trap cleanup EXIT

port_busy() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${port}\$"
  else
    (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null
  fi
}

journal_files() {
  ls -1 data/audit/journal-*.jsonl 2>/dev/null | sort || true
}

journal_count() {
  local total=0 file
  for file in $(journal_files); do
    total=$(( total + $(awk 'END {print NR}' "$file") ))
  done
  echo "$total"
}

last_seq() {
  local file="$1"
  if [[ ! -f "$file" ]]; then
    echo "none"
    return 0
  fi
  python3 - "$file" <<'PYEOF'
import json, sys
seq = None
with open(sys.argv[1], "r", errors="replace") as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except Exception:
            continue
        if isinstance(value, dict) and "seq" in value:
            seq = value["seq"]
print(seq if seq is not None else "none")
PYEOF
}

wait_healthz() {
  local deadline=$(( SECONDS + WAIT_HEALTHZ_SECS )) body=""
  echo "waiting for /healthz on http://127.0.0.1:${HOST_PORT}/healthz (<= ${WAIT_HEALTHZ_SECS}s)"
  while :; do
    if body="$(curl -fsS --max-time 3 "http://127.0.0.1:${HOST_PORT}/healthz" 2>/dev/null)"; then
      echo "healthz: ${body}"
      return 0
    fi
    if (( SECONDS >= deadline )); then
      echo "FATAL: /healthz did not answer within ${WAIT_HEALTHZ_SECS}s"
      "${COMPOSE[@]}" ps || true
      "${COMPOSE[@]}" logs --tail=40 sentinel || true
      return 1
    fi
    sleep 1
  done
}

wait_entries_above() {
  local threshold="$1" deadline=$(( SECONDS + WAIT_ENTRY_SECS )) count
  echo "waiting for journal entries above ${threshold} on the mounted ./data (<= ${WAIT_ENTRY_SECS}s)"
  while :; do
    count="$(journal_count)"
    if (( count > threshold )); then
      echo "journal now holds ${count} entries (> ${threshold})"
      return 0
    fi
    if (( SECONDS >= deadline )); then
      echo "FATAL: no new journal entries within ${WAIT_ENTRY_SECS}s (count=${count})"
      "${COMPOSE[@]}" logs --tail=60 sentinel || true
      return 1
    fi
    sleep 2
  done
}

# ---- preflight -------------------------------------------------------------
echo "=================================================================="
echo "  SENTINEL P16 — docker build + compose cycle + seq continuity"
echo "  date: $(date -Is)  host: $(uname -n)  kernel: $(uname -r)"
echo "  user: $(id -un) uid=$(id -u) gid=$(id -g)   LC_ALL=$LC_ALL"
echo "  image: ${IMAGE}   compose project: ${PROJECT}"
TREE_HASH="$(find crates Cargo.toml Cargo.lock -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) 2>/dev/null | sort | xargs md5sum 2>/dev/null | md5sum | awk '{print $1}')"
echo "  workspace source hash (crates + Cargo files): ${TREE_HASH}"
echo "=================================================================="
echo
echo "== [0/6] preflight =="
"${DOCKER[@]}" version --format 'docker server: {{.Server.Version}}'
"${DOCKER[@]}" compose version
for tool in curl python3; do
  command -v "$tool" >/dev/null || { echo "FATAL: ${tool} missing"; exit 1; }
done
if [[ ! -f .env ]]; then
  echo "note: .env missing — copying the placeholder template (.env.example)"
  cp .env.example .env
fi
mkdir -p data

HOST_PORT="${SENTINEL_HOST_PORT:-}"
if [[ -z "$HOST_PORT" ]]; then
  if port_busy 8080; then
    HOST_PORT=18080
    echo "note: host port 8080 is busy — verifying on host port 18080 (compose overlay)"
  else
    HOST_PORT=8080
  fi
fi
echo "host port for the verify run: ${HOST_PORT}"

F1_BASE="$(journal_files | tail -1 || true)"
BASE_COUNT="$(journal_count)"
BASE_SEQ="none"
if [[ -n "$F1_BASE" ]]; then
  BASE_SEQ="$(last_seq "$F1_BASE")"
fi
echo "journal baseline: files=$(journal_files | tr '\n' ' ' | sed 's/ $//') entries=${BASE_COUNT} last_seq=${BASE_SEQ}"

# ---- compose verify overlay -------------------------------------------------
cat > "$OVERRIDE" <<EOF
# Generated by scripts/docker-verify.sh — host-side verify overlay.
# NOT deploy config: it pins the sentinel service to a deterministic replay
# (placeholder creds cannot keep the live daemon alive) and supplies
# demo-safe breaker config.
services:
  sentinel:
    command: ["/usr/local/bin/sentinel", "--mode", "dry-run", "--replay", "${FIXTURE}"]
    ports: !override
      - "127.0.0.1:${HOST_PORT}:8080"
    environment:
      EXECUTION_MODE: "DRY_RUN"
      RUST_LOG: "sentinel=info"
      MARKET_ALLOWLIST: "32,16"
      REFLEX_COOLDOWN_SECS: "170"
      MAX_ORDER_SIZE_USD: "100000"
      REQUIRE_APPROVAL_ABOVE_USD: "100000"
      # crash-demo pacing: one replay cycle runs ~75 s of wall time
      SENTINEL_MOCK_PACE: "1"
      SENTINEL_MOCK_CAP_MS: "3200"
  breaker:
    environment:
      BREAKER_MODE: "dry_run"
      BREAKER_ANCHOR_ADDRESS: "0x0000000000000000000000000000000000000001"
      BREAKER_GUARDIANS: "0x0000000000000000000000000000000000000001"
      BREAKER_ARM_SECRET: "docker-verify-demo-secret"
      BREAKER_RPC_URL: "http://127.0.0.1:8545"
EOF
echo
echo "--- compose overlay (generated) ---"
cat "$OVERRIDE"
"${COMPOSE[@]}" config -q
echo "compose overlay validates (config -q)"

# ---- [1/6] build ------------------------------------------------------------
echo
echo "== [1/6] docker build (multi-stage, release) =="
if [[ "$SKIP_BUILD" == "1" ]]; then
  echo "SENTINEL_SKIP_BUILD=1 — reusing existing image ${IMAGE}"
  "${DOCKER[@]}" image inspect "$IMAGE" --format 'image: {{.Id}}' >/dev/null
else
  # --network=host: on this host a system nftables `forward` policy `drop`
  # blocks container-bridge egress (apt/crates.io fetches would hang);
  # host networking sidesteps it (see indexer/README.md §local stack).
  if "${DOCKER[@]}" build \
      --network=host \
      --build-arg "SENTINEL_UID=$(id -u)" \
      --build-arg "SENTINEL_GID=$(id -g)" \
      -t "$IMAGE" . >"$BUILD_LOG" 2>&1; then
    echo "build: exit 0 (clean)"
  else
    echo "build: FAILED — tail follows"
    tail -60 "$BUILD_LOG" | tr '\r' '\n' | grep -v '^$' | tail -50
    exit 1
  fi
  echo "--- build tail (last 30 lines, \\r-split) ---"
  tail -60 "$BUILD_LOG" | tr '\r' '\n' | grep -v '^$' | tail -30
  echo "--- error scan (cargo build errors would have failed the build) ---"
  grep -nE '^error|error\[|ERROR:' "$BUILD_LOG" | head -10 || echo "(no error lines)"
fi
"${DOCKER[@]}" image inspect "$IMAGE" \
  --format 'image: {{.Id}}  size={{.Size}} bytes  created={{.Created}}'

# ---- [2/6] compose up (cycle 1) ---------------------------------------------
echo
echo "== [2/6] docker compose up -d (cycle 1) =="
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
"${COMPOSE[@]}" up -d
STACK_UP=1
"${COMPOSE[@]}" ps

# ---- [3/6] healthz + replay entries (cycle 1) -------------------------------
echo
echo "== [3/6] healthz + replay journaling (cycle 1) =="
wait_healthz
wait_entries_above "$BASE_COUNT"
echo "--- sentinel log tail (cycle 1) ---"
"${COMPOSE[@]}" logs --tail=12 sentinel | tail -12
echo "--- journal stats before down ---"
echo "entries=$(journal_count) last_seq=$(last_seq "$(journal_files | tail -1)")"

# ---- [4/6] down → stable read → up (cycle 2) --------------------------------
echo
echo "== [4/6] docker compose down (stop writers; stable journal read) =="
"${COMPOSE[@]}" down --remove-orphans
STACK_UP=0
F1="$(journal_files | tail -1)"
COUNT1="$(journal_count)"
S1="$(last_seq "$F1")"
echo "stable read: file=${F1} entries=${COUNT1} last_seq=${S1}"
if [[ "$S1" == "none" ]]; then
  echo "FATAL: journal has no parsable entries after cycle 1"
  exit 1
fi

echo
echo "== docker compose up -d (cycle 2) =="
"${COMPOSE[@]}" up -d
STACK_UP=1
wait_healthz
wait_entries_above "$COUNT1"

# ---- [5/6] seq continuity assertion -----------------------------------------
echo
echo "== [5/6] seq continuity: first new entry must be seq $((S1 + 1)) =="
python3 - "$COUNT1" "$S1" <<'PYEOF'
import glob, json, sys

count_before = int(sys.argv[1])
last_before = int(sys.argv[2])

entries = []
for path in sorted(glob.glob("data/audit/journal-*.jsonl")):
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            if isinstance(obj, dict) and "seq" in obj:
                entries.append((path, obj["seq"]))

print(f"journal entries now: {len(entries)} (before cycle 2: {count_before})")
print(f"last seq before cycle 2: {last_before}")

new = [(path, seq) for (path, seq) in entries if seq > last_before]
if not new:
    print("FAIL: no entries appended after the down/up cycle")
    sys.exit(1)

first_path, first_seq = new[0]
print(f"first entry after cycle 2: file={first_path} seq={first_seq} (expected {last_before + 1})")
print(f"entries appended in cycle 2 so far: {len(new)}")

if first_seq != last_before + 1:
    print(f"FAIL: seq continuity broken — first new seq {first_seq} != {last_before + 1}")
    sys.exit(1)
print("PASS: journal resumed across the compose down/up cycle (new seq = old + 1)")
PYEOF

# ---- [6/6] stable chain verification + teardown ------------------------------
echo
echo "== [6/6] audit chain verify (stable journal, in-image audit-verify) =="
"${COMPOSE[@]}" down --remove-orphans
STACK_UP=0
"${COMPOSE[@]}" run --rm -T --no-deps sentinel audit-verify --no-chain

echo
echo "=== P16 DOCKER EVIDENCE SUMMARY ==="
echo "build: docker build --network=host --build-arg SENTINEL_UID=$(id -u) --build-arg SENTINEL_GID=$(id -g) -t ${IMAGE} .  → exit 0"
echo "compose: up → /healthz ok on :${HOST_PORT} → replay journaled entries → down → up"
echo "seq continuity: last=${S1} then first-new=$((S1 + 1)) → PASS"
echo "evidence: docs/evidence/p16-docker.txt"
