#!/usr/bin/env python3
"""Generate the contention scenario family (GAP-007 slice 4).

Why this family exists: on the realistic suite the daily action budget is never
scarce (max 4 executed orders against a cap of 10), so ordering, reserve and
segregation are all provably inert -- there is nothing to allocate. This family
makes the cap bind, which is what turns the portfolio layer's effect into a
measured number instead of a claim.

It is a real-tape generator, like `generate_realistic.py`: ETH and BTC 1-minute
closes come from the Perpl testnet tape captured in
`docs/evidence/p21-perpl-real-candles.json`, and the warmup bars are real
returns. The stress segment amplifies real tail-quantile moves so positions
re-enter the danger bands repeatedly instead of being trimmed once.

Structure per scenario:
  - 24 warmup bars. This is load-bearing: an earlier attempt skipped it and the
    first single-position action landed inside the tracker's 20-bucket warmup,
    so the reserve was zero by construction and the fixture was degenerate.
  - 12 correlated stress cycles: both legs take an amplified tail down move, a
    partial recovery, then a further down move.
  - max_daily_actions = 3, which binds.
"""
import json
import os
import random
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CANDLES = os.path.join(REPO, "docs", "evidence", "p21-perpl-real-candles.json")
TEMPLATE = os.path.join(REPO, "tests", "scenarios-realistic", "rb-06-correlated-eth-btc-5x.json")
OUT = os.path.join(REPO, "tests", "scenarios-contention")

WARMUP_BARS = 24
CYCLES = 12
DAILY_CAP = 3
BAR_MS = 60_000
T0 = 1_791_900_000_000


def returns(key, candles):
    closes = [float(c["c"]) for c in candles[key]]
    return [closes[i] / closes[i - 1] - 1 for i in range(1, len(closes))]


def tails(series, quantile=0.01):
    ordered = sorted(series)
    k = max(1, int(len(ordered) * quantile))
    return ordered[:k], ordered[-k:]


def main():
    candles = json.load(open(CANDLES))
    template = json.load(open(TEMPLATE))
    eth, btc = returns("eth_1m", candles), returns("btc_1m", candles)
    down_e, up_e = tails(eth)
    down_b, up_b = tails(btc)
    os.makedirs(OUT, exist_ok=True)

    for idx in range(6):
        rnd = random.Random(500 + idx)
        seq_e = list(eth[idx * 40 : idx * 40 + WARMUP_BARS])
        seq_b = list(btc[idx * 40 : idx * 40 + WARMUP_BARS])
        for _ in range(CYCLES):
            d = down_e[rnd.randrange(len(down_e))] * 3.0
            d2 = down_b[rnd.randrange(len(down_b))] * 3.0
            u = up_e[rnd.randrange(len(up_e))] * 1.2
            u2 = up_b[rnd.randrange(len(up_b))] * 1.2
            seq_e += [d, u, d * 0.8]
            seq_b += [d2, u2, d2 * 0.8]

        entry_e = float(template["positions"][0]["entry_price"])
        entry_b = float(template["positions"][1]["entry_price"])

        def path(seq, base, market):
            out, price = [], base
            for k, r in enumerate(seq):
                price *= 1 + r
                out.append(
                    {"ts_ms": T0 + k * BAR_MS + market, "market_id": market, "mark_price": f"{price:.4f}"}
                )
            return out

        price_path = sorted(path(seq_e, entry_e, 32) + path(seq_b, entry_b, 16), key=lambda t: t["ts_ms"])
        policy = dict(template["policy"])
        policy["max_daily_actions"] = DAILY_CAP

        scenario = dict(template)
        scenario.update(
            id=f"ct-{idx + 1:02d}-contention-5x",
            label="synthetic",
            description=(
                f"contention {idx + 1}: {WARMUP_BARS} warmup bars then {CYCLES} correlated "
                f"stress cycles, daily cap {DAILY_CAP}"
            ),
            price_path=price_path,
            policy=policy,
        )
        scenario.pop("reflex", None)
        with open(os.path.join(OUT, f"ct-{idx + 1:02d}-contention-5x.json"), "w") as handle:
            json.dump(scenario, handle, indent=1)

    print(f"wrote 6 scenarios to {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
