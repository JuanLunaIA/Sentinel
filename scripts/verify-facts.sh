#!/usr/bin/env bash
# scripts/verify-facts.sh — re-run the P01 live checks (no secrets required).
# Exit 0 = all core facts verified; evidence written to docs/evidence/p01-verify-latest.txt
# Usage: bash scripts/verify-facts.sh
set -uo pipefail

# Common tool locations on this host (cast/cre installed by P01)
export PATH="$HOME/.local/bin:$HOME/.foundry/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/docs/evidence/p01-verify-latest.txt"
mkdir -p "$ROOT/docs/evidence"

PASS=0; FAIL=0
ok() { echo "PASS  $*"; PASS=$((PASS+1)); }
no() { echo "FAIL  $*"; FAIL=$((FAIL+1)); }

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

{
echo "== sentinel verify-facts  $(date -u +%Y-%m-%dT%H:%M:%SZ) =="

echo "-- toolchain --"
echo "rustc: $(rustc --version 2>&1)"
echo "cargo: $(cargo --version 2>&1)"
if command -v cast >/dev/null 2>&1; then echo "cast:  $(cast --version 2>&1 | head -1)"; else echo "cast:  MISSING (optional)"; fi
if command -v cre >/dev/null 2>&1; then echo "cre:   present at $(command -v cre)"; else echo "cre:   MISSING (optional)"; fi
echo

echo "-- perpl contexts --"
M=$(curl -s --max-time 15 "https://app.perpl.xyz/api/v1/pub/context")
T=$(curl -s --max-time 15 "https://testnet.perpl.xyz/api/v1/pub/context")
if echo "$M" | jq -e '.chain.chain_id == 143' >/dev/null 2>&1; then ok "mainnet context chain_id=143"; else no "mainnet /v1/pub/context"; fi
if echo "$T" | jq -e '.chain.chain_id == 10143' >/dev/null 2>&1; then ok "testnet context chain_id=10143"; else no "testnet /v1/pub/context"; fi
if echo "$M" | jq -e '[.markets[].id] | index(31) != null' >/dev/null 2>&1; then ok "mainnet market SOL=31 present"; else no "mainnet SOL market id"; fi
if echo "$T" | jq -e '.tokens[0].address | ascii_downcase == "0xa9012a055bd4e0edff8ce09f960291c09d5322dc"' >/dev/null 2>&1; then ok "testnet collateral token 0xa901..22dc"; else no "testnet collateral token address"; fi
if echo "$M" | jq -e '.instances[0].address | ascii_downcase == "0x34b6552d57a35a1d042ccae1951bd1c370112a6f"' >/dev/null 2>&1; then ok "mainnet exchange instance address"; else no "mainnet exchange address"; fi
echo

echo "-- monad rpc --"
CID=$(curl -s --max-time 10 https://rpc.monad.xyz -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' | jq -r '.result' 2>/dev/null)
if [ "$CID" = "0x8f" ]; then ok "monad mainnet eth_chainId=0x8f"; else no "monad mainnet eth_chainId (got: $CID)"; fi
CIDT=$(curl -s --max-time 10 https://testnet-rpc.monad.xyz -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' | jq -r '.result' 2>/dev/null)
if [ "$CIDT" = "0x279f" ]; then ok "monad testnet eth_chainId=0x279f"; else no "monad testnet eth_chainId (got: $CIDT)"; fi
echo

echo "-- nansen x402 --"
CODE=$(curl -s -o "$TMP" -w '%{http_code}' --max-time 15 -X POST "https://api.nansen.ai/api/v1/smart-money/netflow" -H 'content-type: application/json' -d '{"chains":["ethereum"]}')
if [ "$CODE" = "402" ]; then ok "nansen netflow unpaid -> HTTP 402"; else no "nansen netflow (got HTTP $CODE)"; fi
if grep -q 'eip155:143' "$TMP" 2>/dev/null; then ok "nansen 402 offers Monad rail (eip155:143)"; else no "nansen Monad rail missing from 402"; fi
if grep -q '0x754704Bc059F8C67012fEd69BC8A327a5aafb603' "$TMP" 2>/dev/null; then ok "nansen Monad USDC asset address present"; else no "nansen Monad USDC asset address"; fi
echo

echo "-- envio --"
if command -v pnpx >/dev/null 2>&1; then
  EV=$(timeout 120 pnpx envio@latest --version 2>/dev/null | tail -n 1)
  echo "envio: $EV"
else
  echo "envio: pnpx MISSING (optional)"
fi
echo

echo "-- vendor provenance --"
for v in api-docs dex-sdk dex-sdk-examples; do
  if [ -d "$ROOT/vendor/$v/.git" ]; then
    echo "$v: $(git -C "$ROOT/vendor/$v" rev-parse HEAD 2>/dev/null)"
  else
    echo "$v: not cloned (run: git clone into vendor/ — see docs/FACTS.md)"
  fi
done
echo
echo "== SUMMARY: $PASS passed, $FAIL failed =="
} 2>&1 | tee "$OUT"

if [ "$FAIL" -eq 0 ]; then
  echo "verify-facts: OK -> $OUT"
  exit 0
else
  echo "verify-facts: $FAIL FAILURE(S) -> $OUT"
  exit 1
fi
