#!/usr/bin/env bash
# ==========================================================================
# `cre workflow simulate` attempt wrapper for sentinel-deadman (SPEC-P14 §6).
#
# Records the exact command + verbatim output + exit code. In this environment
# the command is expected to fail at the authentication gate (no CRE account;
# STUB-03). On a host with `cre login` (or CRE_API_KEY) it is the entry point
# to the real simulation — with the stale-state emulator providing the input:
#
#   terminal A: node tools/stale-breaker-emulator.ts
#   terminal B: tools/cre-simulate-attempt.sh
#
# Usage: tools/cre-simulate-attempt.sh [extra cre flags...]
# ==========================================================================
set -uo pipefail
cd "$(dirname "$0")/.."   # the workflow folder (sentinel-deadman/)
export LC_ALL=C

# `cre` needs to find bun for TypeScript compilation; it may not be on PATH
# when invoked from a non-login shell.
if ! command -v bun >/dev/null 2>&1 && [ -x "$HOME/.bun/bin/bun" ]; then
  export PATH="$HOME/.bun/bin:$PATH"
fi

CMD=(cre workflow simulate . --non-interactive --trigger-index 0 --target local-simulation "$@")
printf '### command:'
printf ' %q' "${CMD[@]}"
printf '\n### cwd: %s\n' "$PWD"
printf '### date: %s\n' "$(date -Is)"
"${CMD[@]}" </dev/null
rc=$?
echo "### EXIT=$rc"
exit "$rc"
