# P13 scenario suite (`tests/scenarios/`)

Frozen input suite for the SPEC-P13 backtester (`cargo run --release --bin backtest`).
Every file is a self-contained scenario document in the SPEC-P13 §3 format:
full `sentinel_core` Market/Position documents, a strictly increasing
`price_path`, optional `events` / `reflex` / `policy` overrides and an optional
recorded `decision_trace`. All timestamps are fixed logical milliseconds
(anchor `1791240000000`, 2026-10-05T22:40:00Z) — nothing here reads a wall
clock, and no file contains network- or time-dependent data.

**Label honesty.** `label` renders verbatim in the report:

- **synthetic** (12 files): hand-authored stress scenarios. The ETH market
  document mirrors the recorded testnet values (market 32: `price_decimals` 2,
  `size_decimals` 3, initial margin `1200` → 12x, maintenance `2000` → 5%,
  taker fee 345 µs, `order_ttl_blocks` 20); prices are representative, not
  recorded.
- **recorded** (1 file): `recorded-session-20261005.json` — the price path is
  the ETH mark observations actually present in
  `tests/fixtures/perpl/session-20261005T222712Z.jsonl` (real testnet
  recording; duplicate venue timestamps collapsed to keep `ts_ms` strictly
  increasing). The recording carried no position rows, so one representative
  4x long is marked at the recorded price — the file-level claim is "recorded
  marks", never "recorded positions". The recorded window was quiet (flat
  2717.29); the scenario therefore demonstrates the no-op path.
- **reconstructed** (1 file): `reconstructed-testnet-eth.json` — market and
  position shapes come from recorded testnet data (market 32; the 2x long
  mirrors the testnet-shaped P06 crash position, entry 2700, size 10,
  collateral 13560); the price path is **synthesized** to cross liquidation.
  Description carries the SPEC wording: "reconstructed from recorded testnet
  snapshots; path synthesized to cross liquidation".

**Market coverage.** ETH (market 32) everywhere; `correlated-dump` adds BTC
(market 16, recorded session values: 15x initial / 4% maintenance) and extends
the policy allowlist to `[32, 16]`. Position sides: mostly longs;
`gap-through-liq` and `repeated-orange-cooldown` are shorts (up-gap squeeze).

**Policy arithmetic.** Every intended reduce respects the scenario's policy:
order notional ≤ `max_order_size_usd`, and notional above
`require_approval_above_usd` is only executed where the Reflex+Red asymmetry
applies (`RISKTIER` Red from the reflex engine). Where a scenario overrides the
caps, the description says so (`whale-vs-caps`, `reconstructed-testnet-eth`).
`policy_violations` must stay 0 across the suite.

## The 12 synthetic scenarios

| scenario | intent |
|---|---|
| `flash-crash-30` | Flash crash −30% in 5 min over 61 ticks (5 s), 10x long. Price path crosses the implied liq (2584) at tick 10; baseline liquidates. The scenario configures `reduce_fraction` 0.75 because at the 0.5 default the reduced position's liq (2312) would still sit inside the crash depth — the breach must be decisive; a re-entry trim follows when the tier resets through Yellow. |
| `slow-bleed-15` | −15% over 24 h in coarse 2 h steps, 4x long. No liquidation on either path (min 2312 > liq 2176); one Orange trim at the Yellow→Orange transition. Demonstrates gradual de-risking against a bleed. |
| `wick-both` | Two-sided wick: −16% spike through the long's liq (bottom 2290 ≤ liq 2312 → baseline liquidates), then a rebound to +7%. Sentinel trims at Orange, then again exactly at the cooldown boundary; carries a `decision_trace` with two post-review HOLDs (consult path (b), SPEC-P13 §4). |
| `funding-squeeze` | Steady −16% grind with funding-rate spikes over 12 h, 5x long. `funding` events are accepted and **ignored** in v1.0 (they surface in the report notes). Sentinel trims twice at Orange; baseline liquidates at 2298.33. |
| `gap-through-liq` | Short (0.5 ETH, 5x): a single-tick +18% up-gap (2960 → 3200) jumps through the baseline liq (3128) in one tick — baseline liquidates. An Orange trim before the gap raises the reduced liq to 3309 and the sentinel survives: gap risk is contained by acting *before* the gap. |
| `stale-feed-outage` | A `feed_stale` window (60 s → 600 s) during which the feed is `Stale{secs}`: with the production `stale_reduce: false` the reflex is alert-only — no reduce on stale data (the alert still stamps the cooldown). After the window the sentinel resumes and trims at the cooldown boundary; baseline liquidates at the 2310 bottom, sentinel survives. |
| `correlated-dump` | ETH + BTC long legs fall together (both markets on an interleaved path). Both baselines liquidate (ETH 2300 ≤ 2312, BTC 79500 ≤ 79800); both sentinel legs trim twice at Orange (each trim re-arms after the tier returns to Yellow). Exercises multi-market last-known-mark handling. |
| `recovery-v` | V-shaped recovery: dips to Orange (bottom 2220, dist 14.09%) then fully recovers to 2725. **The false-positive showcase**: the baseline never liquidates, so both executed reduces (the Yellow-entry `decision_trace` REDUCE at 2540 and the reflex Orange reduce at 2220) are false positives and `capital_saved_usd` is honestly negative (defense cost). |
| `dust-position` | One lot (0.001 ETH, ~$2.7 notional, 10x). The 50% reduce quantizes below one lot (`reduce_by_fraction` → `None`) and is recorded as a skip; the cooldown escalation then closes the single lot (`fraction = 1` sizes exactly one lot) at a small net defense cost — saved is honestly slightly negative and the size floor is documented rather than hidden. |
| `whale-vs-caps` | $13.6k-notional 5x long. The scenario raises the per-order cap/approval threshold to the whale regime ($8000/$5000) — under the default $1000 cap the 2.5 ETH trim (notional 6275) would be denied; the description documents the knob. Baseline liquidates at 2310; sentinel trims at Orange and survives. |
| `repeated-orange-cooldown` | Short squeeze: repeated Orange signatures 60 s apart inside the 10 min cooldown — exactly one action per cooldown window, the second fires exactly at the cooldown boundary (equality counts as elapsed). Baseline liquidates at the 3270 top (liq 3264); sentinel survives. |
| `black-swan-60` | −60% over 60 min (2 min ticks), 10x long. Baseline liquidates at −6% (2556.8 ≤ 2584). The sentinel trims at the first breach, then escalates to a `Close` at the cooldown boundary (t+10 min) and keeps most of the collateral. |

## Recorded + reconstructed

| scenario | intent |
|---|---|
| `recorded-session-20261005` | Faithful replay of the real recorded testnet session `tests/fixtures/perpl/session-20261005T222712Z.jsonl`: the three unique ETH mark observations (2717.29 throughout). Quiet market → no actions, zero loss on both paths. Honesty note: no cherry-picking — the suite ships the recording as it was. |
| `reconstructed-testnet-eth` | "reconstructed from recorded testnet snapshots; path synthesized to cross liquidation." Recorded market/position shapes (ETH 2x long, entry 2700, collateral 13560, liq 1479) with a synthesized −49% decline from the recorded 2717.29 mark to 1373.29; baseline liquidates at 1474.09 while the sentinel's Orange trim (policy raised to the position's scale, $10000/$5000) moves its liq to 1027 and survives. |

## Verifying the suite by hand

The baseline crossing math is `liq_long = entry·(1 + mmf) − collateral/|size|`
(`liq_short = entry·(1 − mmf) + collateral/|size|`), the exact formula in
`sentinel_core::risk::implied_liq_price`. The suite was authored against exactly
this formula and every file was arithmetically checked before shipping:
**liquidation scenarios must cross** (long `mark ≤ liq`, short `mark ≥ liq`)
and **non-liquidation scenarios must never cross**: flash-crash-30,
wick-both, funding-squeeze, gap-through-liq, stale-feed-outage,
correlated-dump (both legs), whale-vs-caps, repeated-orange-cooldown,
black-swan-60 and reconstructed-testnet-eth cross; slow-bleed-15, recovery-v,
dust-position and the recorded session do not.

Run the whole suite (deterministic, offline, < 60 s):

```text
cargo run --release --bin backtest -- --scenario all --out /tmp/p13-report
```

Run one scenario: `--scenario flash-crash-30` (id) or `--scenario tests/scenarios/flash-crash-30.json` (path).
