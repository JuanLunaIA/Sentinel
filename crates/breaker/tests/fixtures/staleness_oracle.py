#!/usr/bin/env python3
"""Independent re-derivation of the SPEC-P14 §3 staleness / tier / epoch
arithmetic, written from the spec text only.

Frozen rules re-implemented here, verbatim from SPEC-P14 §3:

    tier:   Green=0, Yellow=1, Orange=2, Red=3
    stale  <=> age_secs > stale_mult * heartbeat_interval   (STRICT >;
               boundary 3x exactly = NOT stale)
    critical <=> max_tier >= 2
    fire   = stale AND critical
    epoch  = floor((now_ms - last_ts_ms) / interval_ms)

Usage:
    staleness_oracle.py table      # boundary + tier truth table (human readable)
    staleness_oracle.py epoch <now_ms> <last_ts_ms> <interval_ms>
    staleness_oracle.py check      # machine self-check, exit 0 iff all pass
"""

import sys
from fractions import Fraction

TIER = {"Green": 0, "Yellow": 1, "Orange": 2, "Red": 3}


def is_stale(age_secs: float, stale_mult: int, interval_secs: int) -> bool:
    """STRICT greater-than per §3; age == stale_mult*interval is NOT stale."""
    return age_secs > stale_mult * interval_secs


def is_critical(max_tier: int) -> bool:
    """§3: Orange (2) or Red (3)."""
    return max_tier >= 2


def fires(age_secs: float, stale_mult: int, interval_secs: int, max_tier: int) -> bool:
    return is_stale(age_secs, stale_mult, interval_secs) and is_critical(max_tier)


def epoch(now_ms: int, last_ts_ms: int, interval_ms: int) -> int:
    """Floor division on the elapsed ms; Python // floors for negatives too."""
    return (now_ms - last_ts_ms) // interval_ms


def check() -> int:
    failures = []
    checks = []

    def expect(name, got, want):
        ok = got == want
        checks.append((name, got, want, ok))
        if not ok:
            failures.append(name)

    # Spec §3 boundary, interval 60 (default), mult 3: 3x = 180 NOT stale.
    expect("stale(180, 3, 60)", is_stale(180, 3, 60), False)
    expect("stale(181, 3, 60)", is_stale(181, 3, 60), True)
    expect("stale(179.999, 3, 60)", is_stale(179.999, 3, 60), False)
    # Small interval used by the P14 demo (§7, interval 8): 3x = 24 NOT stale.
    expect("stale(24, 3, 8)", is_stale(24, 3, 8), False)
    expect("stale(25, 3, 8)", is_stale(25, 3, 8), True)
    # Tier truth table (§3): critical ⇔ tier >= 2; fire ⇔ stale ∧ critical.
    expect("crit(0)", is_critical(0), False)
    expect("crit(1)", is_critical(1), False)
    expect("crit(2)", is_critical(2), True)
    expect("crit(3)", is_critical(3), True)
    expect("fire(stale,1)", fires(300, 3, 60, 1), False)  # Yellow never fires
    expect("fire(stale,0)", fires(300, 3, 60, 0), False)  # Green never fires
    expect("fire(stale,2)", fires(300, 3, 60, 2), True)   # Orange fires
    expect("fire(stale,3)", fires(300, 3, 60, 3), True)   # Red fires
    expect("fire(fresh,2)", fires(100, 3, 60, 2), False)  # fresh never fires
    # Epoch (§3): floor((now_ms - last_ts_ms)/interval_ms).
    base = 1_791_200_000_000
    expect("epoch=0 just seen", epoch(base, base, 8000), 0)
    expect("epoch=0 at boundary-1", epoch(base + 7999, base, 8000), 0)
    expect("epoch=1 at boundary", epoch(base + 8000, base, 8000), 1)
    expect("epoch=3 at 3x", epoch(base + 24000, base, 8000), 3)
    expect("epoch=3 at 3x+1ms", epoch(base + 24001, base, 8000), 3)
    expect("epoch=floor 3.99x", epoch(base + 31999, base, 8000), 3)
    expect("epoch=4 at 4x", epoch(base + 32000, base, 8000), 4)
    # Exact-rational check that floor, not trunc toward zero, is used on ms.
    expect("epoch exact frac", epoch(base + 17_999, base, 8000), 2)

    width = max(len(name) for name, *_ in checks)
    for name, got, want, ok in checks:
        print(f"[{'ok' if ok else 'FAIL'}] {name:<{width}} got={got!r} want={want!r}")
    print(f"\n{len(checks) - len(failures)}/{len(checks)} checks pass")
    return 0 if not failures else 1


def table() -> int:
    print("age_secs | stale(mult=3,ivl=60) | stale(mult=3,ivl=8) | fire? tiers")
    for age in [0, 179, 180, 181, 24, 25, 1000]:
        s60 = is_stale(age, 3, 60)
        s8 = is_stale(age, 3, 8)
        fires_row = {t: fires(age, 3, 8, TIER[t]) for t in TIER}
        print(f"{age:>8} | {str(s60):>19} | {str(s8):>18} | {fires_row}")
    print()
    print("Boundary emphasised: age == 3*interval is NOT stale (strict >).")
    print("max_tier 1 (Yellow) never fires even stale; 2 (Orange) fires.")
    return 0


def main(argv: list[str]) -> int:
    cmd = argv[1] if len(argv) > 1 else "check"
    if cmd == "check":
        return check()
    if cmd == "table":
        return table()
    if cmd == "epoch" and len(argv) == 5:
        print(epoch(int(argv[2]), int(argv[3]), int(argv[4])))
        return 0
    print(__doc__, file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
