#!/usr/bin/env python3
"""Generate realistic replay scenarios for the P13 backtester.

Method (documented for the evidence trail):

  1. Real tape. The return series comes from real Perpl testnet 1-minute
     candles (ETH market 32, BTC market 16) fetched live from
     https://testnet.perpl.xyz/api and stored in
     docs/evidence/p21-perpl-real-candles.json.

  2. Block bootstrap. New paths are built by sampling contiguous blocks of
     real returns (default 30 bars) with replacement and concatenating them.
     Block sampling preserves the empirical properties that matter for a
     risk engine: fat tails (excess kurtosis ~ +10 for ETH), negative skew,
     and volatility clustering (acf1 of |returns| ~ +0.26).

  3. Cross-asset structure. Multi-asset scenarios sample the SAME block
     indices for both assets, so the real ETH/BTC correlation (~ +0.76)
     carries into the generated path without any model of it.

  4. Regime injections. Each scenario adds one calibrated event on top of
     the bootstrapped path (gap, crash, bleed, squeeze, vol spike, ...).
     Magnitudes are drawn to match the tails actually observed in the tape
     (largest single-minute moves were 5.5 to 7.6 sigma).

  5. Positions and policy. Position leverage, side and collateral are chosen
     per scenario; policy caps scale with the position notional the way a
     real deployment would set them (the engine defaults of $1000/$500 are
     sized for the tiny hand-authored scenarios, not for a $5k book).

Outputs P13-format scenario JSONs plus a manifest, under
tests/scenarios-realistic/. Nothing here reads a wall clock: prices come
from the stored candles, timestamps advance at a fixed 60 s cadence from a
fixed anchor, and the RNG is seeded per scenario.

Usage:
    python3 tools/scenarios/generate_realistic.py [--out DIR] [--seed N]
"""

from __future__ import annotations

import argparse
import json
import math
import random
from decimal import Decimal, ROUND_DOWN
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CANDLES = ROOT / "docs" / "evidence" / "p21-perpl-real-candles.json"

# Real market documents (recorded testnet values, see docs/FACTS.md §1.2 and
# tests/scenarios/correlated-dump.json). Do not invent new ones.
MARKETS = {
    32: {
        "id": 32, "symbol": "ETH", "base": "ETH",
        "price_decimals": 2, "size_decimals": 3,
        "initial_margin_fraction": "0.083333", "maintenance_margin_fraction": "0.05",
        "max_leverage": "12", "min_size": "0", "tick_size": "0.01",
        "maker_fee_micros": 45, "taker_fee_micros": 345, "order_ttl_blocks": 20,
    },
    16: {
        "id": 16, "symbol": "BTC", "base": "BTC",
        "price_decimals": 1, "size_decimals": 5,
        "initial_margin_fraction": "0.066667", "maintenance_margin_fraction": "0.04",
        "max_leverage": "15", "min_size": "0", "tick_size": "0.1",
        "maker_fee_micros": 45, "taker_fee_micros": 345, "order_ttl_blocks": 20,
    },
}

ANCHOR_MS = 1_791_854_400_000   # fixed logical anchor (first real candle window)
BAR_MS = 60_000                 # 1-minute bars, matching the real candle tape
BLOCK = 30                      # bootstrap block length, bars


# ---------------------------------------------------------------- tape -----

def load_returns() -> tuple[dict[str, list[float]], dict[str, float]]:
    """Log returns per minute plus the last real close, per asset."""
    data = json.loads(CANDLES.read_text())
    out: dict[str, list[float]] = {}
    lasts: dict[str, float] = {}
    for key, name in (("eth_1m", "ETH"), ("btc_1m", "BTC")):
        closes = [c["c"] for c in data[key]]
        out[name] = [math.log(closes[i] / closes[i - 1]) for i in range(1, len(closes))]
        decimals = MARKETS[32 if name == "ETH" else 16]["price_decimals"]
        lasts[name] = closes[-1] / (10 ** decimals)   # scaled int -> real price
    return out, lasts


def bootstrap(n_bars: int, series: list[float], rng: random.Random,
              blocks: list[int] | None = None) -> tuple[list[float], list[int]]:
    """Concatenate contiguous blocks of real returns, with replacement."""
    n = len(series)
    out: list[float] = []
    used: list[int] = []
    while len(out) < n_bars:
        start = rng.randrange(0, n - BLOCK)
        used.append(start)
        out.extend(series[start:start + BLOCK])
    return out[:n_bars], used


def apply_regime(rets: list[float], kind: str, rng: random.Random) -> list[float]:
    """Inject one calibrated event into the bootstrapped path."""
    r = list(rets)
    n = len(r)
    if kind == "none":
        return r

    if kind == "drift_down":            # sustained bleed, like the live -3.1% session
        target = rng.uniform(0.03, 0.055)
        per = target / n
        return [x - per for x in r]

    if kind == "quiet":                 # pull extreme bars toward the mean
        return [x * 0.55 for x in r]

    if kind == "gap_down":              # single-minute tail event
        sigma = rng.uniform(5.5, 7.5)   # matches the observed 5.5-7.6 sigma tails
        idx = rng.randrange(n // 4, 3 * n // 4)
        sd = 0.000775                   # real ETH per-minute sigma
        r[idx] -= sigma * sd
        return r

    if kind == "crash":                 # fast, multi-bar decline
        target = rng.uniform(0.07, 0.14)
        span = rng.randint(8, 20)
        start = rng.randrange(n // 4, n - span - 1)
        per = target / span
        for i in range(start, start + span):
            r[i] -= per
        return r

    if kind == "whipsaw":               # crash then full recovery
        target = rng.uniform(0.06, 0.12)
        span = rng.randint(10, 18)
        start = max(1, n // 3)
        per = target / span
        for i in range(start, start + span):
            r[i] -= per
        for i in range(start + span, min(n, start + 2 * span)):
            r[i] += per
        return r

    if kind == "squeeze_up":            # upward move against shorts
        target = rng.uniform(0.05, 0.10)
        span = rng.randint(8, 18)
        start = rng.randrange(n // 4, n - span - 1)
        per = target / span
        for i in range(start, start + span):
            r[i] += per
        return r

    if kind == "vol_spike":             # clustering: amplify a window
        start = rng.randrange(n // 4, n // 2)
        span = rng.randint(20, 40)
        for i in range(start, min(n, start + span)):
            r[i] *= rng.uniform(3.0, 5.0)
        return r

    if kind == "gap_through_liq":       # instant gap beyond any reactive plan
        # Injected immediately: with the gap at bar 2 there is no pre-gap tape
        # to trim on, so this is a true negative control (a reactive engine
        # cannot save a position whose liquidation price is crossed before it
        # has seen a single tick).
        idx = rng.randint(1, 3)
        r[idx] -= 0.16                  # far past a 12x long's -3.5% liquidation
        return r

    raise ValueError(f"unknown regime {kind}")


# ------------------------------------------------------------- helpers -----

def q(value: float, decimals: int) -> Decimal:
    """Quantize a float down to `decimals` (like the engine's quantize-down)."""
    return Decimal(str(value)).quantize(Decimal(1).scaleb(-decimals), rounding=ROUND_DOWN)


def fmt(value: Decimal) -> str:
    return format(value, "f")


def build_path(start_price: float, rets: list[float], market_id: int,
               decimals: int, offset_ms: int = 0) -> list[dict]:
    ticks = []
    price = start_price
    for i, r in enumerate(rets):
        price = price * math.exp(r)
        ticks.append({
            "ts_ms": ANCHOR_MS + offset_ms + i * BAR_MS,
            "market_id": market_id,
            "mark_price": fmt(q(price, decimals)),
        })
    return ticks


def position(market_id: int, price: float, leverage: float, size: float) -> dict:
    m = MARKETS[market_id]
    notional = price * size
    collateral = notional / leverage
    return {
        "market_id": market_id,
        "symbol": m["symbol"],
        "size": fmt(q(size, m["size_decimals"])),
        "entry_price": fmt(q(price, m["price_decimals"])),
        "mark_price": fmt(q(price, m["price_decimals"])),
        "liq_price": None,
        "collateral": fmt(q(collateral, 2)),
        "unrealized_pnl": "0",
        "margin_ratio": None,
        "leverage": str(Decimal(str(leverage)).normalize()),
        "opened_at": None,
    }


def policy_for(notional: float, markets: list[int]) -> dict:
    """Caps sized like a real deployment running this book."""
    return {
        "market_allowlist": markets,
        "max_order_size_usd": fmt(q(notional, 0)),
        "max_daily_actions": 10,
        "require_approval_above_usd": fmt(q(notional * 0.5, 0)),
        "kill_switch": False,
    }


# ------------------------------------------------------------ scenarios ----

def scenario(sid: str, desc: str, regimes: dict[str, str], n_bars: int,
             holds: list[dict], rng: random.Random, tapes: dict[str, list[float]],
             lasts: dict[str, float], start_free: float = 10_000.0) -> dict:
    """Build one scenario document.

    `regimes` maps asset -> regime kind; `holds` is a list of
    {market, leverage, size, base} describing each position.
    """
    # Bootstrap once per asset; multi-asset scenarios reuse the same block
    # indices so the real cross-correlation survives.
    primary = "ETH"
    base_rets, used = bootstrap(n_bars, tapes[primary], rng)
    paths: dict[str, list[float]] = {primary: apply_regime(base_rets, regimes.get(primary, "none"), rng)}
    for asset in regimes:
        if asset == primary:
            continue
        # same blocks, own series -> real correlation without modelling it
        series = tapes[asset]
        raw = []
        for start in used:
            raw.extend(series[start:start + BLOCK])
        raw = raw[:n_bars]
        paths[asset] = apply_regime(raw, regimes[asset], rng)

    markets_used = sorted({MARKETS[h["market"]]["id"] for h in holds})
    start_prices = {asset: lasts[asset] for asset in paths}

    price_path = []
    offsets = {"ETH": 0, "BTC": BAR_MS // 2, "SOL": BAR_MS // 3}
    for asset, rets in paths.items():
        mid = next(h["market"] for h in holds if h["base"] == asset)
        price_path.extend(build_path(start_prices[asset], rets, mid,
                                     MARKETS[mid]["price_decimals"], offsets.get(asset, 0)))
    price_path.sort(key=lambda t: t["ts_ms"])

    positions = [position(h["market"], start_prices[h["base"]], h["leverage"], h["size"]) for h in holds]
    notional = sum(float(p["entry_price"]) * float(p["size"]) for p in positions)

    return {
        "id": sid,
        "label": "realistic-bootstrap",
        "description": desc,
        "start_free_balance": fmt(q(start_free, 2)),
        "markets": [MARKETS[m] for m in markets_used],
        "positions": positions,
        "price_path": price_path,
        "policy": policy_for(notional, markets_used),
    }


def build_all(seed: int) -> list[dict]:
    tapes, lasts = load_returns()
    out = []
    plan = [
        # id, description, regimes per asset, bars, positions (base, lev, size)
        ("rb-01-drift-down-10x",
         "block-bootstrap of the real ETH tape with the live session's downward drift, 10x long (liq ~ -5.3%)",
         {"ETH": "drift_down"}, 240, [("ETH", 32, 10, 0.4)]),
        ("rb-02-quiet-range-12x",
         "quiet bootstrapped range (vol damped to 55%) with a 12x long: a 12x position opens ~3.5% from liquidation, inside the engine's red band, so the correct read is 'trim because the band is tight', not 'trim because the tape moved'",
         {"ETH": "quiet"}, 240, [("ETH", 32, 12, 0.8)]),
        ("rb-03-tail-gap-8x",
         "one 5.5-7.5 sigma minute drawn from the observed tail injected mid-path, 8x long (liq ~ -7.9%)",
         {"ETH": "gap_down"}, 240, [("ETH", 32, 8, 0.6)]),
        ("rb-04-whipsaw-recovery-6x",
         "bootstrapped crash followed by a full recovery, 6x long; measures what the protection costs when the market comes back",
         {"ETH": "whipsaw"}, 240, [("ETH", 32, 6, 1.0)]),
        ("rb-05-slow-bleed-4x",
         "slow multi-hour decline, 4x long (liq ~ -21%): does the engine trim early or wait for the cliff",
         {"ETH": "drift_down"}, 480, [("ETH", 32, 4, 2.5)]),
        ("rb-06-correlated-eth-btc-5x",
         "same bootstrap blocks replayed on ETH and BTC, so the real +0.76 correlation drives both legs down together",
         {"ETH": "crash", "BTC": "crash"}, 240, [("ETH", 32, 5, 1.0), ("BTC", 16, 5, 0.03)]),
        ("rb-07-short-squeeze-10x",
         "upward squeeze against a 10x short, the mirror image of the long crash narrative",
         {"ETH": "squeeze_up"}, 240, [("ETH", 32, 10, 0.4)]),
        ("rb-08-vol-cluster-8x",
         "volatility-clustering window (returns x3-5 for 20-40 minutes, as the real acf(+0.26) implies), 8x long",
         {"ETH": "vol_spike"}, 240, [("ETH", 32, 8, 0.7)]),
        ("rb-09-noop-active-3x",
         "undamped bootstrapped tape (the real volatility, no regime injection) with a conservative 3x long sitting ~30% from liquidation: the correct behaviour is to do nothing even while the tape moves",
         {"ETH": "none"}, 240, [("ETH", 32, 3, 3.0)]),
        ("rb-10-gap-through-liq-12x",
         "instant -16% gap past a 12x long's liquidation (-3.5%): honest negative control, no reactive engine can save this",
         {"ETH": "gap_through_liq"}, 240, [("ETH", 32, 12, 0.5)]),
        ("rb-11-trim-then-recover-5x",
         "bootstrapped drawdown deep enough to force a trim, then a recovery: measures the cost of being protected",
         {"ETH": "whipsaw"}, 480, [("ETH", 32, 5, 1.2)]),
        ("rb-12-two-leg-portfolio",
         "two-leg book (ETH 8x, BTC 6x) over the same correlated tape, exercising the multi-market allowlist and daily cap",
         {"ETH": "drift_down", "BTC": "drift_down"}, 300, [("ETH", 32, 8, 0.8), ("BTC", 16, 6, 0.05)]),
    ]
    for sid, desc, regimes, bars, holds in plan:
        rng = random.Random(f"{seed}:{sid}")
        parsed = [{"base": b, "market": m, "leverage": lv, "size": sz} for b, m, lv, sz in holds]
        out.append(scenario(sid, desc, regimes, bars, parsed, rng, tapes, lasts))
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", default=str(ROOT / "tests" / "scenarios-realistic"))
    ap.add_argument("--seed", type=int, default=20261012)
    args = ap.parse_args()

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    scenarios = build_all(args.seed)
    for s in scenarios:
        path = out_dir / f"{s['id']}.json"
        path.write_text(json.dumps(s, indent=2) + "\n")
        ticks = len(s["price_path"])
        notional = sum(float(p["entry_price"]) * float(p["size"]) for p in s["positions"])
        print(f"{s['id']:32} ticks={ticks:4} positions={len(s['positions'])} notional=${notional:,.0f}")
    print(f"\nwrote {len(scenarios)} scenarios to {out_dir}")


if __name__ == "__main__":
    main()
