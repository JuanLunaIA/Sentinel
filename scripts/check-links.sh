#!/usr/bin/env bash
# ============================================================================
# check-links.sh — P17 markdown link audit (agent R, SPEC-P17 §5)
#
# Extracts markdown links [text](target) (plus [label]: target reference
# definitions) from README.md, docs/architecture.md and docs/submission/*.md,
# then classifies every target:
#
#   * relative file target  -> MUST exist on disk (acceptance gate: exit 1
#                              if any target is missing; resolved relative to
#                              the containing document, with a repo-root
#                              fallback for the repo's root-relative habit)
#   * anchor (#...)         -> heuristic check only (lowercase + token match
#                              against local headings; NO heading-slug
#                              toolchain) — reported as warnings, never fatal
#   * external http(s)      -> curl -I -L --max-time 8, best-effort; status
#                              classes reported; NEVER fatal; every probe is
#                              additionally wrapped in `timeout` so the
#                              script can never hang
#   * other schemes / placeholders -> reported, skipped
#
# Links inside fenced code blocks (``` / ~~~) are ignored.
#
# Usage:  scripts/check-links.sh [file.md ...]
#         no args -> README.md docs/architecture.md docs/submission/*.md
#         (.md files merely REFERENCED by those are out of scope by spec)
# Env:    SKIP_EXTERNAL=1 -> skip curl probes entirely (offline re-run mode;
#                           externals reported as skipped)
#
# Exit codes:
#   0 = every relative file target exists (gate PASS)
#   1 = at least one relative file target is missing (gate FAIL)
#   2 = required input artifact missing / cannot run (e.g. pending sibling)
# ============================================================================
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

CURL_MAX_TIME=8     # seconds, per SPEC-P17 §5
CURL_HARD_CAP=12    # extra wall-clock kill switch on top of curl --max-time
SKIP_EXTERNAL="${SKIP_EXTERNAL:-0}"

# ---------------------------------------------------------------- inputs ----
declare -a INPUTS=()
if [ "$#" -gt 0 ]; then
  INPUTS=("$@")
else
  shopt -s nullglob
  INPUTS=(README.md docs/architecture.md)
  for f in docs/submission/*.md; do
    INPUTS+=("$f")
  done
fi

missing_inputs=()
for f in "${INPUTS[@]}"; do
  [ -f "$f" ] || missing_inputs+=("$f")
done
if [ "${#missing_inputs[@]}" -gt 0 ]; then
  echo "check-links: CANNOT RUN — required input artifact(s) missing:" >&2
  for f in "${missing_inputs[@]}"; do echo "  - $f" >&2; done
  echo "check-links: is a sibling artifact still pending? re-run once landed." >&2
  exit 2
fi

echo "== check-links.sh — markdown link audit ==================================="
echo "date    : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "repo    : $REPO_ROOT"
echo "inputs  : ${INPUTS[*]}"
echo "external: $( [ "$SKIP_EXTERNAL" = "1" ] && echo 'SKIPPED (SKIP_EXTERNAL=1)' || echo "curl -I -L --max-time ${CURL_MAX_TIME} (hard cap ${CURL_HARD_CAP}s)" )"
echo

# ------------------------------------------------------------- extraction ----
# Emits TAB-separated: FILE  LINE  TEXT  TARGET  (links outside code fences).
extract_links() {
  awk '
    function emit(f, n, text, tgt) {
      gsub(/\t/, " ", text); gsub(/\t/, " ", tgt)
      printf "%s\t%d\t%s\t%s\n", f, n, text, tgt
    }
    FNR == 1 { incode = 0 }
    /^[[:space:]]*(```|~~~)/ { incode = 1 - incode; next }
    incode == 1 { next }
    {
      line = $0
      # reference-style definition:  [label]: target
      if (line ~ /^\[[^]]+\]:[ \t]*/) {
        p = index(line, ":"); tgt = substr(line, p + 1)
        sub(/^[ \t]+/, "", tgt); sub(/[ \t]+$/, "", tgt)
        if (tgt ~ /^</) { q = index(tgt, ">"); if (q > 0) tgt = substr(tgt, 2, q - 2) }
        else { sp = index(tgt, " "); if (sp > 0) tgt = substr(tgt, 1, sp - 1) }
        if (tgt != "" && (tgt ~ /^(https?:|#|\/|<)/ || tgt !~ /[ \t]/)) {
          lbl = substr(line, 2, index(line, "]") - 2)
          emit(FILENAME, FNR, lbl, tgt)
        }
        next
      }
      # inline links (and images): [text](target) — several per line supported
      rest = line
      while (1) {
        p = index(rest, "](")
        if (p == 0) break
        lb = 0
        for (i = p - 1; i >= 1; i--) { if (substr(rest, i, 1) == "[") { lb = i; break } }
        if (lb == 0) { rest = substr(rest, p + 2); continue }
        depth = 1; rparen = 0
        for (i = p + 2; i <= length(rest); i++) {
          c = substr(rest, i, 1)
          if (c == "(") depth++
          else if (c == ")") { depth--; if (depth == 0) { rparen = i; break } }
        }
        if (rparen == 0) { rest = substr(rest, p + 2); continue }
        text = substr(rest, lb + 1, p - lb - 1)
        tgt  = substr(rest, p + 2, rparen - p - 2)
        emit(FILENAME, FNR, text, tgt)
        rest = substr(rest, rparen + 1)
      }
    }
  ' "$@"
}

# ------------------------------------------------------------ anchor check ----
# Heuristic only: exact slug match OR token-subset match. Warnings, not gates.
slugify() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' \
    | sed -E 's/[^a-z0-9 -]//g; s/ +/-/g; s/-+/-/g; s/^-+//; s/-+$//'
}
anchor_matches() {
  # $1 = local file, $2 = anchor, $3 = optional anchor=... query (ignored)
  local tgtfile="$1" anchor="$2" want s h
  anchor="${anchor//%20/ }"
  want="$(slugify "$anchor")"
  [ -z "$want" ] && return 0   # nothing but punctuation — cannot judge
  local -a atoks=()
  IFS='-' read -r -a atoks <<< "$want"
  while IFS= read -r h; do
    s="$(slugify "$h")"
    [ "$s" = "$want" ] && return 0
    local allin=1 tok
    for tok in "${atoks[@]}"; do
      [ -z "$tok" ] && continue
      if [[ " ${s//-/ } " != *" $tok "* ]]; then allin=0; break; fi
    done
    [ "$allin" = "1" ] && return 0
  done < <(grep -E '^#{1,6}[[:space:]]+' "$tgtfile" 2>/dev/null | sed -E 's/^#+[[:space:]]+//')
  return 1
}

# ---------------------------------------------------------- external check ----
declare -A EXT_STATUS=() EXT_FILES=()
check_external() {
  local url="$1" out rc code
  if [ -n "${EXT_STATUS[$url]:-}" ]; then return; fi
  out="$(timeout "$CURL_HARD_CAP" curl -I -L --max-time "$CURL_MAX_TIME" -sS \
          -o /dev/null -w '%{http_code}' "$url" 2>/dev/null)" && rc=0 || rc=$?
  code="${out:-000}"
  case "$rc" in
    0) ;;
    28|124|137) EXT_STATUS[$url]="WARN:timeout(cap ${CURL_HARD_CAP}s)"; return ;;
    *) EXT_STATUS[$url]="WARN:curl-rc=${rc}"; return ;;
  esac
  case "$code" in
    2??|3??) EXT_STATUS[$url]="ok:${code}" ;;
    *)       EXT_STATUS[$url]="WARN:${code}" ;;
  esac
}

# --------------------------------------------------------------- process ----
TMPLINKS="$(mktemp "${TMPDIR:-/tmp}/check-links.XXXXXX")"
trap 'rm -f "$TMPLINKS"' EXIT
extract_links "${INPUTS[@]}" > "$TMPLINKS"

n_occ=0 n_rel=0 n_rel_ok=0 n_rel_missing=0 n_anchor=0 n_anchor_warn=0
n_ext=0 n_ext_ok=0 n_ext_warn=0 n_ext_skipped=0 n_other=0 n_ph=0 n_empty=0

declare -A seen_rel=() seen_anchor_warn=() seen_other=() seen_ph=()
declare -a rel_ok_rows=() rel_missing_rows=() anchor_warn_rows=() other_rows=() ph_rows=() empty_rows=()

while IFS=$'\t' read -r file ln _ target; do
  [ -z "${target+x}" ] && continue
  n_occ=$((n_occ + 1))

  raw="$target"
  # trim whitespace
  raw="${raw#"${raw%%[![:space:]]*}"}"
  raw="${raw%"${raw##*[![:space:]]}"}"

  if [ -z "$raw" ]; then
    n_empty=$((n_empty + 1))
    empty_rows+=("$file:$ln")
    continue
  fi

  t="$raw"
  # angle-wrapped targets: <https://...>/<path/...> are unwrapped; a bare
  # <placeholder-token> (no scheme, no slash) is a placeholder, not a path.
  if [[ "$t" == "<"*">" ]]; then
    inner="${t#<}"; inner="${inner%>}"
    if [[ "$inner" == http://* || "$inner" == https://* || "$inner" == */* ]]; then
      t="$inner"
    else
      n_ph=$((n_ph + 1))
      key="$file|$t"
      if [ -z "${seen_ph[$key]:-}" ]; then
        seen_ph[$key]=1; ph_rows+=("$file:$ln  $t  (placeholder — must be PENDING/known before submission)")
      fi
      continue
    fi
  fi
  [[ "$t" == *' "'* ]] && t="${t%% \"*}"

  # placeholders like {url} still embedded in the target
  if [[ "$t" == *"<"* || "$t" == *">"* || "$t" == *"{"* ]]; then
    n_ph=$((n_ph + 1))
    key="$file|$t"
    if [ -z "${seen_ph[$key]:-}" ]; then
      seen_ph[$key]=1; ph_rows+=("$file:$ln  $t  (placeholder — must be PENDING/known before submission)")
    fi
    continue
  fi

  if [[ "$t" == "#"* ]]; then
    # same-file anchor
    n_anchor=$((n_anchor + 1))
    if ! anchor_matches "$file" "${t#\#}"; then
      n_anchor_warn=$((n_anchor_warn + 1))
      key="$file|$t"
      if [ -z "${seen_anchor_warn[$key]:-}" ]; then
        seen_anchor_warn[$key]=1
        anchor_warn_rows+=("$file:$ln  $t  (no heading match in $file — heuristic)")
      fi
    fi
    continue
  fi

  if [[ "$t" == http://* || "$t" == https://* ]]; then
    n_ext=$((n_ext + 1))
    if [ "$SKIP_EXTERNAL" = "1" ]; then
      n_ext_skipped=$((n_ext_skipped + 1))
    else
      check_external "$t"
      case "${EXT_STATUS[$t]}" in
        ok:*) n_ext_ok=$((n_ext_ok + 1)) ;;
        *)    n_ext_warn=$((n_ext_warn + 1)) ;;
      esac
    fi
    if [ -z "${EXT_FILES[$t]:-}" ]; then EXT_FILES[$t]="$file:$ln"; else EXT_FILES[$t]="${EXT_FILES[$t]},$file:$ln"; fi
    continue
  fi

  if [[ "$t" == mailto:* || "$t" == tel:* || "$t" == ftp://* || "$t" == data:* ]]; then
    n_other=$((n_other + 1))
    key="$file|$t"
    if [ -z "${seen_other[$key]:-}" ]; then
      seen_other[$key]=1; other_rows+=("$file:$ln  $t")
    fi
    continue
  fi

  # leading-slash = repo-root path: check existence, warning-only (not "relative")
  if [[ "$t" == /* ]]; then
    n_other=$((n_other + 1))
    if [ ! -e "${t#/}" ]; then
      other_rows+=("$file:$ln  $t  (repo-root path MISSING — warning)")
    else
      other_rows+=("$file:$ln  $t  (repo-root path ok)")
    fi
    continue
  fi

  # ---------------- relative file target (the gate) ----------------
  fp="$t" frag=""
  if [[ "$fp" == *"#"* ]]; then frag="${fp#*#}"; fp="${fp%%#*}"; fi
  [[ "$fp" == *"?"* ]] && fp="${fp%%\?*}"
  fp="${fp//%20/ }"
  n_rel=$((n_rel + 1))

  d="$(dirname "$file")"
  c1="$d/$fp"
  mode="rel"
  if [ -e "$c1" ]; then
    :
  elif [ "$d" != "." ] && [ -e "$fp" ]; then
    mode="root"
  else
    n_rel_missing=$((n_rel_missing + 1))
    key="$file|$t"
    if [ -z "${seen_rel[$key]:-}" ]; then
      seen_rel[$key]="missing|$ln"
      rel_missing_rows+=("$file:$ln  $t  (looked at: $c1)")
    else
      seen_rel[$key]="${seen_rel[$key]},$ln"
    fi
    continue
  fi

  n_rel_ok=$((n_rel_ok + 1))
  key="$file|$t"
  if [ -z "${seen_rel[$key]:-}" ]; then
    seen_rel[$key]="ok|$ln"
    mode_tag=""
    [ "$mode" = "root" ] && mode_tag="(root)"
    rel_ok_rows+=("ok${mode_tag}  $file:$ln  $t")
  else
    seen_rel[$key]="${seen_rel[$key]},$ln"
  fi

  if [ -n "$frag" ]; then
    n_anchor=$((n_anchor + 1))
    resolved="$c1"; [ "$mode" = "root" ] && resolved="$fp"
    if ! anchor_matches "$resolved" "$frag"; then
      n_anchor_warn=$((n_anchor_warn + 1))
      akey="$t#$frag|$file"
      if [ -z "${seen_anchor_warn[$akey]:-}" ]; then
        seen_anchor_warn[$akey]=1
        anchor_warn_rows+=("$file:$ln  $t  (no heading match in $resolved — heuristic)")
      fi
    fi
  fi
done < "$TMPLINKS"

# ---------------------------------------------------------------- report ----
print_block() {  # print_block "HEADER" array...
  local header="$1"; shift
  if [ "$#" -gt 0 ]; then
    echo "$header"
    printf '  %s\n' "$@"
    echo
  fi
}

echo "-- RELATIVE FILE TARGETS (acceptance gate) --------------------------------"
if [ "${#rel_ok_rows[@]}" -gt 0 ]; then
  printf '  %s\n' "${rel_ok_rows[@]}" | sort
fi
if [ "${#rel_missing_rows[@]}" -gt 0 ]; then
  echo "  MISSING:"
  printf '    %s\n' "${rel_missing_rows[@]}" | sort
fi
echo

print_block "-- ANCHOR WARNINGS (heuristic, non-fatal) ---------------------------------" "${anchor_warn_rows[@]}"
print_block "-- PLACEHOLDER TARGETS (must be PENDING/known before submission) ----------" "${ph_rows[@]}"
print_block "-- OTHER / SKIPPED (mailto, repo-root paths, empty) -----------------------" "${other_rows[@]}" "${empty_rows[@]}"

echo "-- EXTERNAL http(s) LINKS (best-effort, non-fatal) ------------------------"
if [ "${#EXT_STATUS[@]}" -eq 0 ]; then
  echo "  (none)"
else
  for url in $(printf '%s\n' "${!EXT_STATUS[@]}" | sort); do
    printf '  %-24s %-34s %s\n' "${EXT_STATUS[$url]}" "${EXT_FILES[$url]:-}" "$url"
  done
fi
echo

echo "============================================================================"
echo "SUMMARY"
echo "---------------------------------------------------------------------------"
printf '  link occurrences extracted ....... %d\n' "$n_occ"
printf '  relative file targets ............ %d  (ok %d / missing %d)\n' "$n_rel" "$n_rel_ok" "$n_rel_missing"
printf '  anchors checked (heuristic) ...... %d  (warnings %d)\n' "$n_anchor" "$n_anchor_warn"
printf '  external http(s) ................. %d  (ok %d / warn %d / skipped %d)\n' "$n_ext" "$n_ext_ok" "$n_ext_warn" "$n_ext_skipped"
printf '  other schemes / repo-root ........ %d\n' "$n_other"
printf '  placeholders ..................... %d\n' "$n_ph"
printf '  empty targets .................... %d\n' "$n_empty"
echo "---------------------------------------------------------------------------"
if [ "$n_rel_missing" -gt 0 ]; then
  echo "RESULT: FAIL — $n_rel_missing missing relative file target(s)  [exit 1]"
  echo "============================================================================"
  exit 1
fi
echo "RESULT: PASS — all $n_rel relative file target(s) exist  [exit 0]"
echo "============================================================================"
exit 0
