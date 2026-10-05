#!/usr/bin/env python3
"""P03 verifier hygiene scan — report-only (SPEC §7).

Checks, for the four perception-layer files:
  1. `unwrap(` / `expect(` occurrences OUTSIDE `#[cfg(test)]` regions
     (the workspace lint denies them in non-test builds);
  2. any `pub fn` in types.rs returning a raw `serde_json::Value`.

Usage (from the repository root):
    python3 tests/fixtures/perpl/verifier/p03_hygiene_scan.py
"""
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[4]
PERPL = ROOT / "crates/sentinel/src/perpl"

print("### HYGIENE SCAN (report only)")
for name in ["auth", "types", "rest", "ws"]:
    src = (PERPL / f"{name}.rs").read_text()
    lines = src.splitlines()
    cfg = [i + 1 for i, l in enumerate(lines) if "#[cfg(test)]" in l]
    mod_tests = [i + 1 for i, l in enumerate(lines) if re.match(r"\s*(pub\s+)?mod\s+tests", l)]
    boundary = min(cfg) if cfg else (min(mod_tests) if mod_tests else None)
    matches = [(i + 1, l.strip()) for i, l in enumerate(lines) if re.search(r"\bunwrap\(|\bexpect\(", l)]
    non_test = [(n, t) for n, t in matches if boundary is None or n < boundary]
    print(
        f"== {name}.rs: lines={len(lines)} cfg(test)@{cfg[:3]} boundary={boundary} "
        f"unwrap/expect total={len(matches)}; outside #[cfg(test)]={len(non_test)}"
    )
    for n, t in non_test:
        print(f"   NON-TEST line {n}: {t[:140]}")

print()
print("### types.rs: public fns returning raw serde_json::Value")
src = (PERPL / "types.rs").read_text()
hits = 0
for m in re.finditer(r"pub (?:async )?fn [^{;]*", src):
    snippet = " ".join(m.group(0).split())
    line = src[: m.start()].count("\n") + 1
    if "->" in snippet and "Value" in snippet.split("->", 1)[1]:
        hits += 1
        print(f"   line {line}: {snippet[:180]}")
print(f"   hits={hits}")
