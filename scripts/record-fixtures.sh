#!/usr/bin/env bash
# scripts/record-fixtures.sh — record a live Perpl testnet session into a JSONL
# fixture (format: SPEC.md §3.4) that powers MockPerpl, the backtester and the
# deterministic demo.
#
#   Usage: bash scripts/record-fixtures.sh [output.jsonl]
#   Env:   PERPL_WS_URL (default testnet), PERPL_CHAIN_ID (default 10143),
#          WS_SECS (default 20)
#
# Records today: public REST (/v1/pub/context, /v1/market-data/ticker) and the
# market-data WebSocket (market-state + heartbeat + order-book@32).
# TODO(P03 integration): also record signed REST (/v1/trading/wallet,
# /positions, /fills) via the read-positions binary once PERPL_API_KEY exists
# (SETUP-MANUAL step 3) — see STUB-09.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/tests/fixtures/perpl/session-$(date -u +%Y%m%dT%H%M%SZ).jsonl}"
API_URL="${PERPL_API_URL:-https://testnet.perpl.xyz/api}"
WS_URL="${PERPL_WS_URL:-wss://testnet.perpl.xyz}"
CHAIN_ID="${PERPL_CHAIN_ID:-10143}"
WS_SECS="${WS_SECS:-20}"

mkdir -p "$(dirname "$OUT")"
: > "$OUT"
echo "[record] -> $OUT"

record_rest() {
  local path="$1" body
  body=$(curl -fsS --max-time 15 "$API_URL$path")
  printf '{"kind":"rest","path":"%s","resp":%s}\n' "$path" "$body" >> "$OUT"
  echo "[record] rest $path ($(printf '%s' "$body" | wc -c) bytes)"
}

record_rest "/v1/pub/context"
record_rest "/v1/market-data/ticker"

WS_BASE="$WS_URL" OUT_FILE="$OUT" CHAIN="$CHAIN_ID" SECS="$WS_SECS" node <<'EOF'
const fs = require('fs');
const base = process.env.WS_BASE, out = process.env.OUT_FILE;
const chain = Number(process.env.CHAIN), secs = Number(process.env.SECS);
const start = Date.now();
let count = 0;
const ws = new WebSocket(`${base}/ws/v1/market-data`);
ws.onopen = () => ws.send(JSON.stringify({
  mt: 5,
  subs: [
    { stream: `market-state@${chain}`, subscribe: true },
    { stream: `heartbeat@${chain}`, subscribe: true },
    { stream: `order-book@32`, subscribe: true },
  ],
}));
ws.onmessage = (ev) => {
  try {
    const msg = JSON.parse(ev.data);
    fs.appendFileSync(out, JSON.stringify({ kind: 'ws', t_ms: Date.now() - start, msg }) + '\n');
    count++;
  } catch (e) { /* ignore non-JSON frames */ }
};
ws.onerror = (e) => console.error('[record] ws error:', e.message || e);
setTimeout(() => {
  console.log(`[record] ws messages captured: ${count}`);
  ws.close();
  process.exit(0);
}, secs * 1000);
EOF

echo "[record] done ($(wc -l < "$OUT") lines)"
