#!/usr/bin/env bash
# ==========================================================================
# Sentinel P10 — anvil end-to-end: build the contract, deploy it to a LOCAL
# anvil chain, run the anchor service against the real contract, and verify
# the anchors via eth_getLogs. Fully offline.
#
# This script is also the runbook for the LIVE testnet leg (PENDING-WALLET):
# swap RPC + key + chain and re-run the same steps manually:
#   1) forge create contracts/src/SentinelAuditAnchor.sol:SentinelAuditAnchor \
#        --rpc-url https://testnet-rpc.monad.xyz --private-key $RPC_SIGNER_KEY
#   2) put the address into .env (ANCHOR_CONTRACT_ADDRESS) + docs/FACTS.md
#   3) cargo run --bin sentinel -- --mode dry-run   (anchor task spawns)
#   4) cargo run --bin audit-verify                 (Trust beat, live)
# ==========================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

RPC="${RPC:-http://127.0.0.1:8545}"
# anvil account #0's well-known throwaway key (never holds value).
ANVIL_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
EVIDENCE=docs/evidence/p10-anvil-e2e.txt
mkdir -p docs/evidence

{
echo "# Sentinel P10 — anvil end-to-end ($(date -Is))"

echo "== forge build =="
(cd contracts && forge build -q)
echo "build: OK"

echo "== anvil up on $RPC =="
pkill -f "anvil --silent --port 8545" 2>/dev/null || true
anvil --silent --port 8545 &
ANVIL_PID=$!
trap 'kill "$ANVIL_PID" 2>/dev/null || true' EXIT
sleep 1

echo "== deploy SentinelAuditAnchor =="
ADDR=$(cd contracts && forge create src/SentinelAuditAnchor.sol:SentinelAuditAnchor \
  --rpc-url "$RPC" --private-key "$ANVIL_KEY" --broadcast 2>/dev/null \
  | awk '/Deployed to:/ {print $3}')
echo "contract: ${ADDR:-DEPLOY-FAILED}"
[ -n "$ADDR" ] || exit 1

echo "== anchor service e2e (ignored test against the real contract) =="
ANVIL_RPC_URL="$RPC" ANVIL_CONTRACT="$ADDR" \
  cargo test -p sentinel --test p10_anvil_e2e -- --ignored --nocapture

echo "== all green =="
} 2>&1 | tee "$EVIDENCE"
