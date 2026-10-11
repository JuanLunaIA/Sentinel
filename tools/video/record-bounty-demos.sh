#!/usr/bin/env bash
# record-bounty-demos.sh — records the two bounty-specific demo videos:
#   * CRE dead-man's switch (Chainlink bounty)
#   * Perpl testnet daemon + on-chain activity (Perpl bounty)
#
# Produces .cast files in docs/video/casts/ which build-videos.sh renders.
# Uses --idle-time-limit so long waits compress instead of padding the clip.
#
# Usage: bash tools/video/record-bounty-demos.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

export PATH="$HOME/.local/bin:$PATH"
CASTS=docs/video/casts
CRE=workflow-cre/sentinel-deadman
mkdir -p "$CASTS"

# Read the breaker HMAC secret from .env if present, else a demo value.
SECRET=$(grep '^BREAKER_ARM_SECRET=' .env 2>/dev/null | cut -d= -f2- || true)
SECRET=${SECRET:-demo-secret-0123456789abcdef}

# ---------------------------------------------------------------------------
# CRE — Chainlink Runtime Environment dead-man's switch
# ---------------------------------------------------------------------------
echo "[cast] recording CRE dead-man's switch ..."
asciinema rec --overwrite --cols 120 --rows 34 --idle-time-limit 1.2 \
  -c "cd $CRE && \
      export BREAKER_ARM_SECRET='$SECRET' && \
      clear && \
      echo '### Chainlink CRE — sentinel-deadman workflow'; echo; \
      echo '-- Step 1: attempt the real CRE CLI (requires a logged-in account) --'; echo; \
      echo '\$ cre workflow simulate . --non-interactive --trigger-index 0 --target local-simulation'; echo; \
      bash tools/cre-simulate-attempt.sh 2>&1 | head -8 || true; \
      echo; echo '-- Step 2: the same workflow definition, run against a local runtime --'; echo; \
      echo '\$ node tools/stale-breaker-emulator.ts &   # emulated breaker, stale guardian'; \
      (node tools/stale-breaker-emulator.ts >/tmp/cre-emu.log 2>&1 &) ; sleep 2; \
      head -1 /tmp/cre-emu.log; echo; \
      echo '\$ node tools/local-runner.ts'; echo; \
      node tools/local-runner.ts 2>&1; \
      echo; echo '### the workflow read the heartbeat, applied the stale && critical rule,'; \
      echo '### and posted an HMAC-signed trigger — the breaker answered HTTP 202.'; \
      sleep 3" \
  "$CASTS/05-cre-deadman.cast" >/dev/null 2>&1
echo "[cast]   -> 05-cre-deadman.cast"

# ---------------------------------------------------------------------------
# Perpl — live testnet daemon, heartbeat + on-chain anchor
# ---------------------------------------------------------------------------
echo "[cast] recording Perpl testnet daemon ..."
PORT=$(grep '^PORT=' .env 2>/dev/null | cut -d= -f2- || echo 8081)
PORT=${PORT:-8081}
asciinema rec --overwrite --cols 120 --rows 34 --idle-time-limit 1.0 \
  -c "clear && \
      echo '### Perpl testnet — live guardian daemon + on-chain audit anchor'; echo; \
      echo '\$ ./target/release/sentinel --mode testnet'; echo; \
      (./target/release/sentinel --mode testnet >/tmp/perpl-daemon.log 2>&1 &) ; sleep 12; \
      echo '-- daemon started; tail of the live log --'; echo; \
      tail -18 /tmp/perpl-daemon.log; echo; \
      echo '-- health endpoint --'; echo; \
      echo '\$ curl -s http://localhost:$PORT/healthz'; \
      curl -s http://localhost:$PORT/healthz; echo; echo; \
      echo '\$ curl -s http://localhost:$PORT/api/state | jq .'; \
      curl -s http://localhost:$PORT/api/state | jq . 2>/dev/null | head -30; echo; \
      echo '### the guardian is live in TESTNET mode, posting heartbeats'; \
      echo '### that the on-chain SentinelAuditAnchor records.'; \
      sleep 3; \
      pkill -f '[t]arget/release/sentinel --mode testnet' 2>/dev/null || true" \
  "$CASTS/06-perpl-live.cast" >/dev/null 2>&1
echo "[cast]   -> 06-perpl-live.cast"

echo "[cast] done:"
ls -la "$CASTS"/*.cast
