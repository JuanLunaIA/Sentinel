#!/usr/bin/env bash
# scripts/validate.sh — the per-prompt validation gate (P00 execution protocol):
#   cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
# Per-step output: docs/evidence/validate-{fmt,clippy,test}.txt
# Summary:        docs/evidence/validate-latest.txt
set -uo pipefail

# Common tool locations on this host
export PATH="$HOME/.local/bin:$HOME/.foundry/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
E="$ROOT/docs/evidence"
mkdir -p "$E"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

cargo fmt --check > "$E/validate-fmt.txt" 2>&1; fmt_rc=$?
cargo clippy --workspace --all-targets -- -D warnings > "$E/validate-clippy.txt" 2>&1; clippy_rc=$?
cargo test --workspace > "$E/validate-test.txt" 2>&1; test_rc=$?

{
  echo "== sentinel validate $TS =="
  echo "rustc: $(rustc --version)"
  echo "fmt --check : rc=$fmt_rc   (docs/evidence/validate-fmt.txt)"
  echo "clippy      : rc=$clippy_rc   (docs/evidence/validate-clippy.txt)"
  echo "test        : rc=$test_rc   (docs/evidence/validate-test.txt)"
} | tee "$E/validate-latest.txt"

if [ "$fmt_rc" -eq 0 ] && [ "$clippy_rc" -eq 0 ] && [ "$test_rc" -eq 0 ]; then
  echo "VALIDATION: OK"
  exit 0
fi
echo "VALIDATION: FAILED (see docs/evidence/validate-*.txt)"
exit 1
