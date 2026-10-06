#!/usr/bin/env bash
# ==========================================================================
# Sentinel — live LLM key probe. Run AFTER adding a real QWEN_API_KEY (and/or
# KIMI_API_KEY) to .env (SETUP-MANUAL). Confirms: which base URL works, the
# model string, and whether `response_format: json_object` is accepted — the
# three unknowns behind STUB-01/STUB-02. Minimal single-call requests.
#
# Usage:   ./scripts/live-qwen-probe.sh
# Output:  docs/evidence/p07-qwen-probe.txt   (key never printed)
# Next:    cargo run --bin brain_eval -- --out docs/evidence/p07-brain-eval.txt
# ==========================================================================
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=docs/evidence/p07-qwen-probe.txt
mkdir -p docs/evidence

# shellcheck disable=SC1091
[ -f .env ] && { set -a; . ./.env; set +a; }

redact_check() {
  # Fail loudly if a key would ever leak into evidence (P00 invariant #5).
  local key="$1"
  if [ -n "$key" ] && grep -q "$key" "$OUT" 2>/dev/null; then
    echo "SECURITY: key material detected in $OUT — aborting" >&2
    exit 1
  fi
}

QWEN="${QWEN_API_KEY:-}"
if [ -z "$QWEN" ] || [ "$QWEN" = "replace-me" ]; then
  echo "QWEN_API_KEY not set (placeholder) — nothing to probe."
  echo "Add the real key to .env (docs/SETUP-MANUAL.md) and re-run." | tee "$OUT"
  exit 0
fi

: > "$OUT"
echo "== Qwen probe $(date -Is) ==" | tee -a "$OUT"
for base in \
  "https://dashscope-intl.aliyuncs.com/compatible-mode/v1" \
  "https://dashscope.aliyuncs.com/compatible-mode/v1"; do
  echo "-- $base --" | tee -a "$OUT"
  curl -sS --max-time 60 "$base/chat/completions" \
    -H "Authorization: Bearer $QWEN" \
    -H 'Content-Type: application/json' \
    -d '{"model":"qwen3.8-max","messages":[{"role":"user","content":"Reply with exactly: ok"}],"max_tokens":16,"temperature":0.1,"response_format":{"type":"json_object"}}' \
    -w '\nHTTP:%{http_code} time:%{time_total}s\n' | tee -a "$OUT" | tail -c 500
  echo | tee -a "$OUT"
done
redact_check "$QWEN"

KIMI="${KIMI_API_KEY:-}"
if [ -n "$KIMI" ] && [ "$KIMI" != "replace-me" ]; then
  echo "== Kimi probe $(date -Is) ==" | tee -a "$OUT"
  curl -sS --max-time 60 "https://api.moonshot.ai/v1/chat/completions" \
    -H "Authorization: Bearer $KIMI" \
    -H 'Content-Type: application/json' \
    -d '{"model":"kimi-k3","messages":[{"role":"user","content":"Reply with exactly: ok"}],"max_tokens":16,"temperature":0.1}' \
    -w '\nHTTP:%{http_code} time:%{time_total}s\n' | tee -a "$OUT" | tail -c 500
  echo | tee -a "$OUT"
  redact_check "$KIMI"
else
  echo "KIMI_API_KEY not set (placeholder) — qwen-only probe." | tee -a "$OUT"
fi

echo "Next: cargo run --bin brain_eval -- --out docs/evidence/p07-brain-eval.txt" | tee -a "$OUT"
