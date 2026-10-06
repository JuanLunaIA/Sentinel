# SPEC-P13 — Backtester / Liquidation Replay ("Capital Saved" report)

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 5h. Depends: P04 (core math), P05/P06 (policy/reflex semantics), P12 (no live rows — recorded/reconstructed sources only).

## 0. Deviation note (frozen)

The prompt names `sentinel-core::sim`; it is frozen as **`sentinel::sim`** (app crate).
Why: `sentinel-core` is zero-dependency pure (P00 invariant #4, verified on disk) and the
feed/executor traits (`PerplFeed`, `Executor`, `PositionProbe`) live in the `sentinel` app crate.
The sim drives the same PURE core functions the production pipeline uses.

## 1. Goal / non-goals

Goal: quantify, deterministically and offline, what the real decision path saves vs a do-nothing
baseline on a scripted or recorded market path. Non-goals: funding modeling, fees beyond taker fee
on simulated fills, multi-account portfolios, real venue order latency.

## 2. File ownership (disjoint; children never run git)

Agent A (sim-core): `crates/sentinel/src/sim/{mod.rs, scenario.rs, engine.rs, baseline.rs, venue.rs, report.rs}`
Agent B (scenarios+bin): `tests/scenarios/*.json` (14 files), `tests/scenarios/README.md`, `crates/sentinel/src/bin/backtest.rs`
Agent C (verifier): `crates/sentinel/tests/p13_adversarial.rs`, `tests/fixtures/p13/**` (own corpus/oracle)
Parent: Cargo.toml `[[bin]]` entry + `sim/` stub signature files (already committed in prep).

## 3. Scenario format (JSON, normative example)

```json
{
  "id": "flash-crash-30",
  "label": "synthetic",
  "description": "flash crash -30% in 5 min, 10x long",
  "start_free_balance": "10000",
  "markets": [ { ... full sentinel_core::types::Market JSON, e.g. id 32 ETH testnet ... } ],
  "positions": [ { ... full sentinel_core::types::Position JSON ... } ],
  "price_path": [ {"ts_ms": 1791200000000, "market_id": 32, "mark_price": "3000.00"} ],
  "events": [ {"ts_ms": 1791200060000, "kind": "feed_stale", "until_ms": 1791200120000} ],
  "reflex": {"reduce_fraction":"0.5","orange_fraction":"0.25","cooldown_ms":600000,"stale_reduce":false},
  "policy": {"market_allowlist":[32],"max_order_size_usd":"1000","max_daily_actions":10,"require_approval_above_usd":"500","kill_switch":false},
  "decision_trace": [ {"at_ms": 1791200060000, "market_id": 32, "decision": { ... Decision v3 JSON ... }} ]
}
```

- `label` in {"synthetic","recorded","reconstructed"}; rendered verbatim in the report.
- `reflex`/`policy`/`decision_trace`/`events` optional; defaults: production defaults
  (`ReflexConfig::default()`-equivalent from `Config` values 0.5/0.25/600000/false;
  policy defaults above). `events` kinds: "feed_stale" (window by `until_ms`; engine honors it),
  "funding"/"big_fill" accepted and **ignored** in v1.0 (documented, counted in notes).
- Validation (typed `ScenarioError`): price_path strictly increasing ts_ms; every market_id used
  must exist in `markets`; mark_price > 0; fractions in (0,1]; at least one position and one tick.

## 4. Engine semantics (frozen)

- Tick = one price_path entry applied in ts_ms order; each tick evaluates ALL positions with the
  last-known mark per market. Per position recompute: `unrealized_pnl = size * (mark - entry_price)`,
  mark_price field, `distance_to_liq_pct`, `tier(...)` with thresholds 25/15/8 (Config defaults).
- Data quality: `Fresh`, or `Stale{secs}` while inside a feed_stale window (secs = tick_ts - window_start).
- Reflex: `ReflexState::advance(&pos, tier, quality, &reflex_cfg, tick_ts_ms as u64)` — EXACT
  production path (same core fn the pipeline calls). None => no action.
- Strategy/decision replay (deterministic, no LLM): when `decision_trace` is present, consults occur
  at (a) Yellow-entry transition per market, (b) after each executed reflex action (post-review).
  A trace entry at the consult tick applies: convert Decision -> Intent (REDUCE => fraction =
  amount/|size| clamped to (0,1]; CLOSE => close intent; ADD_COLLATERAL => add-collateral intent;
  HOLD/ESCALATE => recorded no-action). Then `PolicyEngine::evaluate` with
  `PolicySource::Strategy`; Allow => executor; NeedsApproval/Deny => recorded skip with the verdict.
  Intent variant names: match `sentinel_core::types::Intent` exhaustively from disk (report mismatch
  in open_issues if this spec's semantics cannot be expressed).
- Policy: `PolicyEngine::evaluate(&intent, &current_account_state, &policy_cfg, &day_state,
  &PolicyContext{source, tier, market_id})`. `actions_today` starts 0 per scenario and increments
  per allowed+executed action. Reflex runs with `PolicySource::Reflex`.
- Executor: `SimExecutor` (owns sim/venue.rs) implements `Executor`. Fill: full size at
  `mark*(1 - slippage_bps/1e4)` for sells, `mark*(1 + ...)` for buys, slippage default 10 bps;
  taker fee = `taker_fee_micros * notional / 1e6` accumulated. `ExecutionReport`:
  status `Executed`-equivalent, `client_order_id = "sim-<market>-<n>"`, `ts_ms = tick_ts`.
  After fill: size reduced (long: size - filled; short: size + filled); realized_pnl +=
  `(fill - entry) * reduced_signed_size`; position removed at size 0.

## 5. Baseline + accounting (frozen, exact Decimal)

- Baseline (do-nothing): walk the same path; a position liquidates at the first tick where the mark
  crosses effective liq price (long: mark <= liq; short: mark >= liq; liq = pos.liq_price else
  `implied_liq_price(pos, market)`), losing ALL its collateral (isolated margin).
  `baseline_loss_usd` = sum of collateral of liquidated positions; if nothing liquidates:
  `max(0, -(final unrealized pnl))` summed per position (+0 fees).
- Sentinel loss: `sentinel_loss_usd = max(0, -(realized_pnl + final_unrealized_pnl)) + sim_fees_usd`
  per position, summed; positions closed at 0 contribute realized only.
- Conservation invariant (tested): `capital_saved_usd == baseline_loss_usd - sentinel_loss_usd`
  EXACTLY (Decimal equality). Negative saved (defense cost) is allowed and reported honestly.
- `liquidations_avoided = baseline_liquidations - sentinel_liquidations` (sentinel liquidations
  should be 0 in all current scenarios; report counts).
- `false_positive_reduces` = number of executed reduces in scenarios where the baseline NEVER
  liquidates (the trigger-happy cost test; recovery-V scenario is the showcase).

## 6. SimReport (JSON) + determinism

Fields: `scenario_id, label, ticks, actions[ {ts_ms, market_id, tier, source, order, verdict,
fill_detail, size_after} ], baseline_liquidations, sentinel_liquidations, liquidations_avoided,
baseline_loss_usd, sentinel_loss_usd, capital_saved_usd, sim_fees_usd, false_positive_reduces,
policy_violations (must be 0), notes[], metrics{ reflex_eval_eval: p50/p90/p99 µs }`.
Determinism: everything EXCEPT `metrics` must be byte-identical for the same scenario (sorted keys,
no timestamps); the determinism test compares that section. Reports: per-scenario + aggregate:
`{scenarios, total_notional_usd, baseline_liquidations, sentinel_liquidations, total_saved_usd,
pct_saved}`; the md renders the frozen literal line:
"Across N scenarios representing $X notional, Sentinel preserved $Y (Z%); baseline liquidations: A -> with Sentinel: B".

## 7. backtest bin (`crates/sentinel/src/bin/backtest.rs`)

`--scenario <file|all>` (default all), `--scenarios-dir tests/scenarios` (default), `--mode
replay|live-brain` (default replay), `--out docs/backtest-report` (writes `.json` + `.md`).
Exit codes: 0 ok; 2 missing key/config (live-brain without QWEN_API_KEY); 1 internal error.
live-brain: real `QwenProvider` consults at consult points through the real engine (rate limited
by STRATEGY_MIN_INTERVAL_SECS, budget guard); curated 3-scenario run only; offline default stays
deterministic. All 14 scenarios must run in < 60 s total (acceptance).

## 8. Acceptance / evidence

- 14 scenarios: 12 synthetic (flash-crash-30, slow-bleed-15, wick-both, funding-squeeze,
  gap-through-liq, stale-feed-outage, correlated-dump, recovery-v, dust-position, whale-vs-caps,
  repeated-orange-cooldown, black-swan-60) + 1 recorded (from `tests/fixtures/perpl/` real session)
  + 1 reconstructed (testnet data, labeled). Aggregate saved > 0; policy_violations == 0.
- `docs/evidence/p13-backtest-report.md` (the md) + `docs/evidence/p13-validate.txt`.
- Tests: conservation exact, cooldown respected, determinism (rerun byte-compare), label honesty,
  < 60 s (measured in the bin run inside the suite).

## 9. Standing rules (all agents)

Latest versions via live registries (standing user rule); never run git; loud stubs
(`tracing::warn!("STUB: ...")` + docs/STUBS.md entry owned by parent); no network in tests;
clippy `-D warnings` + rustfmt clean are gates (tests included); evidence-shaped report
{files_created, tests[{cmd,exit,observed}], open_issues}; fixed runtime determinism (LC_ALL=C,
no wall-clock in artifacts).
