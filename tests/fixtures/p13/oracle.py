#!/usr/bin/env python3
"""P13 independent oracle (SPEC-P13, frozen 2026-10-06).

Recomputes the backtester accounting from a scenario file with Python
`decimal` (context precision 28) — independent of the Rust engine. Written
from SPEC-P13 + the production semantics the spec defers to (P04 risk math,
P05 policy gate), by the P13 verifier. It supports exactly the constructs
used by this corpus: single market, one position, optional recorded CLOSE
decision at a Yellow-entry consult, feed_stale windows.

Frozen rules implemented (SPEC-P13):
  * tick loop in file order; last-known mark per market; per tick every
    position recomputed (unrealized_pnl, distance_to_liq_pct, tier 25/15/8).
  * data quality: Stale{secs=tick_ts-window_start} inside a feed_stale window.
  * reflex: the exact P04 §3.4 state machine (cooldown equality elapses).
  * policy: the exact P05 §3 precedence (first failure wins).
  * SimExecutor fills: full size at mark*(1 -/+ slippage_bps/1e4) for
    sells/buys (10 bps); taker fee = taker_fee_micros * notional / 1e6,
    notional = fill * filled (candidate "fill"); the alternative
    "mark * filled" candidate is emitted for divergence detection.
  * baseline: first tick where the mark crosses the effective liq price
    (long mark <= liq, short mark >= liq) loses ALL collateral; otherwise
    max(0, -(final unrealized pnl)) per position.
  * sentinel loss: max(0, -(realized + final unrealized)) + fees per position.
  * conservation: capital_saved == baseline_loss - sentinel_loss (exact).

Usage:
    python3 oracle.py <scenario.json> [--pretty]
Exit codes: 0 ok; 2 input/parse error; 3 unsupported construct.
"""

from __future__ import annotations

import json
import sys
from decimal import Decimal, ROUND_DOWN, getcontext

getcontext().prec = 28

D = Decimal
ZERO, ONE, HUNDRED = D(0), D(1), D(100)

SOFT = D(25)  # Config defaults (SPEC-P13 §4): thresholds 25/15/8
WARN = D(15)
HARD = D(8)

SLIPPAGE_BPS = D(10)          # SimExecutor default (SPEC-P13 §4)
FEE_DEN = D(1_000_000)


def dec(value) -> Decimal:
    """Parse a decimal from a JSON string or number."""
    return D(str(value))


# --------------------------------------------------------------------------
# core risk math (P04 — the spec defers to these exact semantics)
# --------------------------------------------------------------------------

def effective_liq_price(pos: dict, market: dict) -> Decimal | None:
    if pos["size"] == 0:
        return None
    if pos["liq_price"] is not None:
        return dec(pos["liq_price"])
    size_abs = abs(pos["size"])
    if size_abs == 0 or pos["entry_price"] <= 0:
        return None
    side = ONE if pos["size"] > 0 else -ONE
    requirement = pos["entry_price"] * size_abs * dec(market["maintenance_margin_fraction"])
    return pos["entry_price"] + side * (requirement - pos["collateral"]) / size_abs


def distance_to_liq_pct(pos: dict, market: dict, mark: Decimal) -> Decimal | None:
    if pos["size"] == 0:
        return None
    if mark <= 0:
        return None
    liq = effective_liq_price(pos, market)
    if liq is None:
        return None
    return (mark - liq).copy_abs() / mark * HUNDRED


def tier_of(distance: Decimal) -> str:
    if distance >= SOFT:
        return "GREEN"
    if distance >= WARN:
        return "YELLOW"
    if distance >= HARD:
        return "ORANGE"
    return "RED"


def quantize_down(size: Decimal, decimals: int) -> Decimal:
    if decimals < 0:  # rust trunc_with_scale allows negative scales
        q = D(10) ** (-decimals)
        return (size / q).to_integral_value(rounding=ROUND_DOWN) * q
    return size.quantize(D(1).scaleb(-decimals), rounding=ROUND_DOWN)


def reduce_by_fraction(pos: dict, fraction: Decimal, market: dict) -> Decimal | None:
    if fraction <= 0 or fraction > ONE:
        return None
    position_size = abs(pos["size"])
    if position_size == 0:
        return None
    d = int(market["size_decimals"])
    size = min(quantize_down(position_size * fraction, d), quantize_down(position_size, d))
    if size == 0 or size < dec(market["min_size"]):
        return None
    return size


# --------------------------------------------------------------------------
# reflex state machine (P04 §3.4)
# --------------------------------------------------------------------------

def reflex_intent(pos: dict, tier: str, quality, cfg: dict) -> dict | None:
    if pos["size"] == 0:
        return None
    if tier in ("GREEN", "YELLOW"):
        return None
    stale = isinstance(quality, tuple)
    if tier == "RED":
        if not stale:
            return {"kind": "reduce", "fraction": cfg["reduce_fraction"],
                    "reason": "red tier: first-breach reduce"}
        return stale_result(tier, quality, cfg)
    # ORANGE
    if not stale:
        return {"kind": "reduce", "fraction": cfg["orange_fraction"],
                "reason": "orange tier: de-risking reduce"}
    return stale_result(tier, quality, cfg)


def stale_result(tier: str, quality, cfg: dict) -> dict:
    if cfg["stale_reduce"]:
        return {"kind": "reduce", "fraction": cfg["orange_fraction"],
                "reason": "stale data: gated reduce"}
    secs = quality[1] if isinstance(quality, tuple) else None
    data = "missing data" if secs is None else f"stale data ({secs}s)"
    return {"kind": "alert", "reason": f"{data} at {tier} tier: reduce disabled, manual attention required"}


def reflex_advance(book: dict, market_id, pos: dict, tier: str, quality, cfg: dict, ts: int):
    if pos["size"] == 0:
        return None
    b = book.setdefault(market_id, {"last": None, "red": False})
    if tier in ("GREEN", "YELLOW"):
        b["last"], b["red"] = None, False
        return None
    if b["last"] is not None and (ts - b["last"]) < cfg["cooldown_ms"]:
        return None
    if tier == "RED" and not isinstance(quality, tuple):
        if b["red"]:
            intent = {"kind": "close", "reason": "red tier: still in breach after reduce"}
        else:
            b["red"] = True
            intent = {"kind": "reduce", "fraction": cfg["reduce_fraction"],
                      "reason": "red tier: first-breach reduce"}
    else:
        intent = reflex_intent(pos, tier, quality, cfg)
    if intent is not None:
        b["last"] = ts
    return intent


# --------------------------------------------------------------------------
# policy gate (P05 §3)
# --------------------------------------------------------------------------

def policy_evaluate(intent: dict, pos: dict, cfg: dict, day: dict, source: str, tier: str) -> tuple[str, str]:
    if cfg["kill_switch"]:
        return "deny", "kill switch engaged"
    if pos["market_id"] not in cfg["market_allowlist"]:
        return "deny", f"market {pos['market_id']} not in allowlist"
    if intent["kind"] == "alert":
        return "allow", ""
    if intent["kind"] == "reduce":
        frac = intent["fraction"]
        if frac <= 0 or frac > ONE:
            return "deny", "invalid reduce fraction"
        if pos["mark_price"] is None or pos["mark_price"] <= 0:
            return "deny", "mark unavailable for notional"
        notional = frac * abs(pos["size"]) * pos["mark_price"]
    elif intent["kind"] == "close":
        if pos["size"] == 0:
            return "deny", "position already flat"
        if pos["mark_price"] is None or pos["mark_price"] <= 0:
            return "deny", "mark unavailable for notional"
        notional = abs(pos["size"]) * pos["mark_price"]
    elif intent["kind"] == "add_collateral":
        if intent["amount"] <= 0:
            return "deny", "collateral amount must be positive"
        notional = intent["amount"]
    else:
        return "deny", "unknown intent"
    if notional > cfg["max_order_size_usd"]:
        return "deny", "notional exceeds MAX_ORDER_SIZE_USD"
    reflex_red = source == "REFLEX" and tier == "RED"
    if notional > cfg["require_approval_above_usd"] and not reflex_red:
        return "needs_approval", "notional above approval threshold"
    if day["actions_today"] >= cfg["max_daily_actions"]:
        return "deny", "daily action cap reached"
    return "allow", ""


# --------------------------------------------------------------------------
# scenario walk
# --------------------------------------------------------------------------

def run_oracle(scenario: dict) -> dict:
    positions = []
    for raw in scenario["positions"]:
        positions.append({
            "market_id": raw["market_id"],
            "size": dec(raw["size"]),
            "entry_price": dec(raw["entry_price"]),
            "liq_price": None if raw.get("liq_price") is None else dec(raw["liq_price"]),
            "collateral": dec(raw["collateral"]),
            "mark_price": None,
        })
    if len(scenario["markets"]) != 1 or len(positions) != 1:
        raise SystemExit("oracle supports exactly 1 market + 1 position (corpus construct)")
    market = scenario["markets"][0]
    mid = market["id"]
    pos = positions[0]

    reflex_cfg = {
        "reduce_fraction": dec(scenario["reflex"]["reduce_fraction"]),
        "orange_fraction": dec(scenario["reflex"]["orange_fraction"]),
        "cooldown_ms": int(scenario["reflex"]["cooldown_ms"]),
        "stale_reduce": bool(scenario["reflex"]["stale_reduce"]),
    }
    pol = scenario["policy"]
    policy_cfg = {
        "market_allowlist": [int(x) for x in pol["market_allowlist"]],
        "max_order_size_usd": dec(pol["max_order_size_usd"]),
        "max_daily_actions": int(pol["max_daily_actions"]),
        "require_approval_above_usd": dec(pol["require_approval_above_usd"]),
        "kill_switch": bool(pol["kill_switch"]),
    }

    stale_windows = [
        (int(e["ts_ms"]), int(e["until_ms"]), e.get("note"))
        for e in scenario.get("events", []) if e["kind"] == "feed_stale"
    ]
    trace = {}
    for entry in scenario.get("decision_trace") or []:
        trace[(int(entry["at_ms"]), int(entry["market_id"]))] = entry["decision"]

    book: dict = {}
    day = {"actions_today": 0}
    realized = ZERO
    fees = ZERO
    fees_mark_candidate = ZERO
    executed = []
    skipped = []
    warnings = []
    last_mark = dec(scenario["price_path"][0]["mark_price"])
    prev_tier: dict = {}

    for tick in scenario["price_path"]:
        ts = int(tick["ts_ms"])
        mark = dec(tick["mark_price"])
        if int(tick["market_id"]) != mid:
            raise SystemExit("oracle: foreign market_id in price_path")
        last_mark = mark

        quality = "FRESH"
        for start, end, _note in stale_windows:
            if start <= ts <= end:
                quality = ("STALE", (ts - start) // 1000)

        if pos["size"] == 0:
            continue  # removed at size 0

        pos["mark_price"] = mark
        liq = effective_liq_price(pos, market)
        d = distance_to_liq_pct(pos, market, mark)
        if d is None:
            continue
        tier = tier_of(d)

        # sentinel position crossing its own liq price is not modeled by the
        # frozen §5 formula; flag it so callers do not trust sentinel numbers.
        if liq is not None:
            crossed = (pos["size"] > 0 and mark <= liq) or (pos["size"] < 0 and mark >= liq)
            if crossed:
                warnings.append(f"sentinel position crosses liq at ts={ts} (formula-only accounting)")

        intent = reflex_advance(book, mid, pos, tier, quality, reflex_cfg, ts)
        if intent is not None:
            verdict, reason = policy_evaluate(intent, pos, policy_cfg, day, "REFLEX", tier)
            if verdict == "allow" and intent["kind"] in ("reduce", "close"):
                frac = dec(intent.get("fraction", ONE))
                size = reduce_by_fraction(pos, frac, market)
                if size is None:
                    skipped.append({"ts": ts, "source": "REFLEX",
                                    "detail": f"skipped: size below lot/min ({intent['kind']})"})
                else:
                    sell = pos["size"] > 0
                    fill = mark * (ONE - SLIPPAGE_BPS / D(10000)) if sell else mark * (ONE + SLIPPAGE_BPS / D(10000))
                    signed = size if sell else -size
                    realized += (fill - pos["entry_price"]) * signed
                    notional = fill * size
                    fees += dec(market["taker_fee_micros"]) * notional / FEE_DEN
                    fees_mark_candidate += dec(market["taker_fee_micros"]) * (mark * size) / FEE_DEN
                    pos["size"] = pos["size"] - size if sell else pos["size"] + size
                    day["actions_today"] += 1
                    executed.append({"ts": ts, "source": "REFLEX", "kind": intent["kind"],
                                     "size": str(size), "fill": str(fill), "size_after": str(pos["size"])})
            else:
                skipped.append({"ts": ts, "source": "REFLEX", "detail": f"{verdict}: {reason}"})

        # consult (a): Yellow-entry transition per market
        if pos["size"] != 0 and tier == "YELLOW" and prev_tier.get(mid) == "GREEN":
            decision = trace.get((ts, mid))
            if decision is not None:
                action = decision["action"]
                if action == "CLOSE":
                    intents = [{"kind": "close", "reason": "recorded decision"}]
                elif action == "REDUCE":
                    frac = dec(decision["amount"]) / abs(pos["size"])
                    frac = min(max(frac, ZERO), ONE)
                    intents = [{"kind": "reduce", "fraction": frac, "reason": "recorded decision"}]
                elif action in ("HOLD", "ESCALATE"):
                    intents = []
                else:
                    raise SystemExit(f"oracle: unsupported decision action {action}")
                for intent2 in intents:
                    verdict, reason = policy_evaluate(intent2, pos, policy_cfg, day, "STRATEGY", tier)
                    if verdict == "allow" and intent2["kind"] in ("reduce", "close"):
                        frac = dec(intent2.get("fraction", ONE))
                        size = reduce_by_fraction(pos, frac, market)
                        if size is None:
                            skipped.append({"ts": ts, "source": "STRATEGY",
                                            "detail": f"skipped: size below lot/min ({intent2['kind']})"})
                        else:
                            sell = pos["size"] > 0
                            fill = mark * (ONE - SLIPPAGE_BPS / D(10000)) if sell else mark * (ONE + SLIPPAGE_BPS / D(10000))
                            signed = size if sell else -size
                            realized += (fill - pos["entry_price"]) * signed
                            notional = fill * size
                            fees += dec(market["taker_fee_micros"]) * notional / FEE_DEN
                            fees_mark_candidate += dec(market["taker_fee_micros"]) * (mark * size) / FEE_DEN
                            pos["size"] = pos["size"] - size if sell else pos["size"] + size
                            day["actions_today"] += 1
                            executed.append({"ts": ts, "source": "STRATEGY", "kind": intent2["kind"],
                                             "size": str(size), "fill": str(fill), "size_after": str(pos["size"])})
                    else:
                        skipped.append({"ts": ts, "source": "STRATEGY", "detail": f"{verdict}: {reason}"})
        prev_tier[mid] = tier

    # ---------------- baseline (do nothing) ----------------
    baseline_loss = ZERO
    baseline_liqs = 0
    for raw in scenario["positions"]:
        b = {
            "market_id": raw["market_id"],
            "size": dec(raw["size"]),
            "entry_price": dec(raw["entry_price"]),
            "liq_price": None if raw.get("liq_price") is None else dec(raw["liq_price"]),
            "collateral": dec(raw["collateral"]),
        }
        liq = effective_liq_price(b, market)
        liquidated = False
        for tick in scenario["price_path"]:
            m = dec(tick["mark_price"])
            if liq is None:
                break
            if (b["size"] > 0 and m <= liq) or (b["size"] < 0 and m >= liq):
                liquidated = True
                break
        if liquidated:
            baseline_liqs += 1
            baseline_loss += b["collateral"]
        else:
            final_unrl = (last_mark - b["entry_price"]) * b["size"]
            baseline_loss += max(ZERO, -final_unrl)

    # ---------------- sentinel loss ----------------
    final_unrl = (last_mark - pos["entry_price"]) * pos["size"]
    sentinel_loss = max(ZERO, -(realized + final_unrl)) + fees
    capital_saved = baseline_loss - sentinel_loss

    reduce_count = sum(1 for a in executed if a["kind"] == "reduce")
    return {
        "scenario": scenario["id"],
        "ticks": len(scenario["price_path"]),
        "baseline_loss_usd": str(baseline_loss),
        "sentinel_loss_usd": str(sentinel_loss),
        "capital_saved_usd": str(capital_saved),
        "sim_fees_usd": str(fees),
        "sim_fees_mark_candidate_usd": str(fees_mark_candidate),
        "baseline_liquidations": baseline_liqs,
        "sentinel_liquidations": 0,
        "liquidations_avoided": baseline_liqs,
        "false_positive_reduces": reduce_count if baseline_liqs == 0 else 0,
        "executed_actions": executed,
        "skipped": skipped,
        "warnings": warnings,
    }


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        with open(args[0], encoding="utf-8") as fh:
            scenario = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"oracle: cannot read scenario: {exc}", file=sys.stderr)
        return 2
    try:
        result = run_oracle(scenario)
    except SystemExit as exc:
        print(str(exc), file=sys.stderr)
        return 3
    print(json.dumps(result, indent=2 if "--pretty" in sys.argv else None, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
