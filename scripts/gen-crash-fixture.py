#!/usr/bin/env python3
"""Generate tests/fixtures/perpl/crash-scenario.jsonl (SPEC-P06.md §7).

Replays an ETH (testnet market 32) long sliding from 30 % to 6 % distance to
liquidation over 600 logical seconds (24 mark steps), crossing soft 25 %
(Yellow), warn 15 % (Orange) and hard 8 % (Red). BTC (market 16) is the Green
contrast. Deterministic: every number below is derived, never hand-typed.

Liquidation price (risk.rs, verified against the SDK vector):
    liq_long = entry * (1 + mmr) - collateral / |size|
Regenerate with:  python3 scripts/gen-crash-fixture.py
"""

import json
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
OUT = REPO / "tests" / "fixtures" / "perpl" / "crash-scenario.jsonl"

# --- ETH (market 32): env by the margins below (SPEC-P06 §7) ---
ETH_ENTRY = Decimal("2700.00")
ETH_SIZE = Decimal("10.000")
ETH_COLL = Decimal("13560")
ETH_MMR = Decimal("0.05")
ETH_LIQ = ETH_ENTRY * (1 + ETH_MMR) - ETH_COLL / ETH_SIZE  # 1479.00

# --- BTC (market 16): well-collateralized contrast, stays Green ---
BTC_ENTRY = Decimal("95000.0")
BTC_SIZE = Decimal("0.100")
BTC_COLL = Decimal("3800")
BTC_LIQ = BTC_ENTRY * (1 + ETH_MMR) - BTC_COLL / BTC_SIZE  # 61750.0

STEPS = 24
M_FIRST = Decimal("2112.86")
M_LAST = Decimal("1573.40")
BASE_TS = 1791240000000  # epoch-ms anchor for the logical timeline


def raw_int(value: Decimal, decimals: int) -> int:
    return int((value * (10**decimals)).to_integral_value(ROUND_HALF_UP))


def distance_pct(mark: Decimal, liq: Decimal) -> Decimal:
    return (mark - liq) / mark * 100


def rest(path: str, payload: dict) -> dict:
    return {"kind": "rest", "path": path, "resp": payload}


def ws(t_ms: int, msg: dict) -> dict:
    return {"kind": "ws", "t_ms": t_ms, "msg": msg}


def context_payload() -> dict:
    return {
        "chain": {"chain_id": 10143, "name": "Monad Testnet"},
        "instances": [
            {"id": 12, "address": "0x1964c32f0be608e7d29302aff5e61268e72080cc"}
        ],
        "tokens": [{"id": 1, "symbol": "AUSD", "decimals": 6}],
        "markets": [
            {
                "ver": 1,
                "id": 32,
                "instance_id": 12,
                "perpetual_id": 32,
                "symbol": "ETH",
                "name": "ETH Perp",
                "funding_interval_sec": 2580,
                "order_ttl_blocks": 20,
                "config": {
                    "price_decimals": 2,
                    "size_decimals": 3,
                    "min_posting_amount": "0",
                    "initial_margin": 1200,
                    "maintenance_margin": 2000,
                    "maker_fee": 45,
                    "taker_fee": 345,
                },
            },
            {
                "ver": 1,
                "id": 16,
                "instance_id": 12,
                "perpetual_id": 16,
                "symbol": "BTC",
                "name": "BTC Perp",
                "funding_interval_sec": 2580,
                "order_ttl_blocks": 20,
                "config": {
                    "price_decimals": 1,
                    "size_decimals": 3,
                    "min_posting_amount": "0",
                    "initial_margin": 1000,
                    "maintenance_margin": 2000,
                    "maker_fee": 45,
                    "taker_fee": 345,
                },
            },
        ],
    }


def wallet_payload() -> dict:
    return {
        "mt": 19,
        "sn": 1,
        "at": {"b": 1, "t": BASE_TS},
        "addr": "0x0000000000000000000000000000000000000007",
        "n": 12,
        "fl": 0,
        "as": [
            {
                "mt": 19,
                "in": 12,
                "id": 7,
                "fr": False,
                "fw": True,
                "ft": 0,
                "lfr": 41,
                "b": "5000000000",
                "lb": "0",
            }
        ],
        "sts": [],
    }


def positions_payload(sn: int) -> dict:
    return {
        "mt": 26,
        "sn": sn,
        "at": {"b": sn, "t": BASE_TS},
        "d": [
            {
                "at": {"b": 1, "t": BASE_TS - 1000000},
                "mkt": 32,
                "acc": 7,
                "pid": 1001,
                "rq": 41,
                "oid": 555,
                "st": 1,
                "sr": 21,
                "sd": 1,
                "c": str(ETH_COLL * 10**6),  # "13560000000"
                "ep": raw_int(ETH_ENTRY, 2),
                "s": raw_int(ETH_SIZE, 3),
                "fee": "0",
                "cfee": "0",
                "efs": 0,
                "lv": 200,
                "dpnl": "0",
                "fnd": "0",
                "ots": {"b": 1, "t": BASE_TS - 2000000},
            },
            {
                "at": {"b": 1, "t": BASE_TS - 1000000},
                "mkt": 16,
                "acc": 7,
                "pid": 1002,
                "rq": 42,
                "oid": 556,
                "st": 1,
                "sr": 21,
                "sd": 1,
                "c": str(BTC_COLL * 10**6),  # "3800000000"
                "ep": raw_int(BTC_ENTRY, 1),
                "s": raw_int(BTC_SIZE, 3),
                "fee": "0",
                "cfee": "0",
                "efs": 0,
                "lv": 1000,
                "dpnl": "0",
                "fnd": "0",
                "ots": {"b": 1, "t": BASE_TS - 2000000},
            },
        ],
    }


def mark_frame(sn: int, eth_raw: int, at_t: int) -> dict:
    return {
        "mt": 9,
        "sn": sn,
        "d": {
            "32": {"at": {"b": sn, "t": at_t}, "mrk": eth_raw},
            "16": {"at": {"b": sn, "t": at_t}, "mrk": raw_int(Decimal("95000.0"), 1)},
        },
    }


def main() -> None:
    lines = [
        rest("/v1/pub/context", context_payload()),
        rest(
            "/v1/market-data/ticker",
            {
                "mt": 9,
                "sn": 1,
                "d": {
                    "32": {
                        "at": {"b": 1, "t": BASE_TS},
                        "mrk": raw_int(M_FIRST, 2),
                    },
                    "16": {
                        "at": {"b": 1, "t": BASE_TS},
                        "mrk": raw_int(Decimal("95000.0"), 1),
                    },
                },
            },
        ),
        rest("/v1/trading/wallet", wallet_payload()),
        rest("/v1/trading/positions", positions_payload(1)),
    ]

    # Initial WS snapshot: wallet then positions (MockPerpl composes on both).
    lines.append(ws(100, wallet_payload()))
    lines.append(ws(200, positions_payload(2)))

    print(f"ETH liq = {ETH_LIQ}  | BTC liq = {BTC_LIQ}")
    prev_tier = None
    for k in range(STEPS):
        t_ms = 1000 + k * 25000
        at_t = BASE_TS + t_ms
        mark = (M_FIRST + (M_LAST - M_FIRST) * Decimal(k) / Decimal(STEPS - 1)).quantize(
            Decimal("0.01")
        )
        lines.append(ws(t_ms, mark_frame(100 + k, raw_int(mark, 2), at_t)))
        if k in (6, 12, 18):
            lines.append(ws(t_ms + 1, positions_payload(200 + k)))
        dist = distance_pct(mark, ETH_LIQ)
        tier = (
            "Green"
            if dist >= 25
            else "Yellow"
            if dist >= 15
            else "Orange"
            if dist >= 8
            else "Red"
        )
        if tier != prev_tier:
            print(f"  step {k:2d}  t=+{t_ms:6d}ms  mark {mark}  dist {dist:.4f}%  -> {tier}")
            prev_tier = tier

    tail = distance_pct(M_LAST, ETH_LIQ)
    assert tail < 8, f"scenario must end in Red, got {tail}%"
    assert distance_pct(M_FIRST, ETH_LIQ) >= 25, "scenario must start Green"

    OUT.parent.mkdir(parents=True, exist_ok=True)
    with OUT.open("w", encoding="utf-8") as handle:
        for line in lines:
            handle.write(json.dumps(line, separators=(",", ":")) + "\n")
    print(f"wrote {len(lines)} lines -> {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    main()
