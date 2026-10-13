# PLAN — GAP-009 + GAP-028: volatility-adaptive risk layer

Source: `SENTINEL-SUPREME-ENTERPRISE-EVALUATION.md` Part IX.
GAP-009 is ranked by the evaluation as the highest-leverage math upgrade in
the system ("Recommended #9 overall, #1 among pure-code items"). Its empirical
mandate is F9: the realistic-tape suite showed fixed 25/15/8% bands producing
false-positive trims in 5 of 12 scenarios, two negative-saved rows.

Companion: GAP-028 (time-to-liquidation estimates) shares the volatility
machinery, so it ships in the same slice.

## S0 — Frame

**Acceptance criteria (runnable):**

1. `cargo test --workspace` green, including new unit + adversarial suites.
2. On the realistic suite: `false_positive_reduces` drops >= 50% relative to the
   frozen baseline (26 -> <= 13) **while `liquidations_avoided` does not drop**
   (baseline 1 -> still 1).
3. On the curated suite: `total_saved_usd` does not regress (baseline $5,972.14)
   and liquidations avoided stay 11.
4. Bands are volatility-scaled with hard clamps [4%, 40%]; `hard < warn < soft`
   holds after scaling for every input in the property sweep.
5. Every intent journals the band values and sigma estimate in force
   (threshold provenance).
6. `time_to_liq` is a pure function with monotonicity tests (higher sigma =>
   shorter time; larger distance => longer time).

**Non-goals (explicit):**

- No ML model (GAP-009 v2 is gated behind the GAP-038 pipeline; the evaluation
  itself says keep v1 deterministic and explainable).
- No book-depth input (that is GAP-006, needs the order-book feed).
- No funding term (GAP-008, separate slice).
- No change to the policy gate semantics, executor, or audit chain format.

**Unknowns + resolution:**

| Unknown | Resolution |
|---|---|
| Where the tape for sigma comes from at runtime | The pipeline already fans marks into `LiveState`; add a per-market ring of recent marks. Backtest feeds the same path via `Sim::step`. |
| Whether scaling breaks existing golden fixtures | Fixtures pin tier math on fixed thresholds; keep fixed bands as the config default and make scaling opt-in per config (`adaptive_enabled`), so fixtures stay valid and the backtest proves the delta. |

## S1 — Data & invariants

**New types (pure core, `sentinel-core/src/vol.rs`):**

- `VolConfig { lambda: Decimal (0.94), min_samples: u32 (20), jump_k: Decimal (4),
   horizon_ms: u64 (600_000), k_hard/k_warn/k_soft: Decimal, floor_pct: Decimal (4),
   ceil_pct: Decimal (40) }`
- `VolState { market_id, ewma_var: Decimal, last_mark: Option<Decimal>, samples: u32, regime: Regime }`
- `Regime { Calm, Elevated, Stress, Dislocation }`
- `sigma_per_sqrt_ms(&self) -> Option<Decimal>` — the volatility in the unit the
  horizon scaling needs.

**Formulas (all `rust_decimal`, no float):**

- EWMA variance: `v_t = λ·v_{t-1} + (1-λ)·r_t²`, `r_t = ln(P_t/P_{t-1})`
  — needs a ln; `Decimal` has no ln. Resolution: compute the return as the
  **simple return** `(P_t - P_{t-1}) / P_{t-1}`, which is the standard
  RiskMetrics discrete form and avoids transcendental functions entirely.
  Documented as the deliberate choice.
- Band: `band_pct = k · sigma_horizon · 100`, where
  `sigma_horizon = sigma_step · sqrt(horizon_ms / step_ms)`.
  sqrt on Decimal: use `Decimal::sqrt()` (available in rust_decimal with the
  `maths` feature — verify before use; fallback: Newton iteration on Decimal).
- Clamp: `band = min(max(band, floor), ceil)`.
- Ordering: after clamping, enforce `soft > warn > hard` by construction
  (compute soft then derive warn/hard by fixed ratios, or clamp each and
  re-assert). Invariant asserted.

**Invariant table:**

| Input | Shape | Invariant |
|---|---|---|
| marks | strictly increasing ts, > 0 | reject non-positive; ignore out-of-order |
| < min_samples | warmup | return `None` => caller keeps configured static bands |
| sigma = 0 (flat tape) | degenerate | clamp to floor (never zero-width band) |
| jump (|r| > k·sigma) | outlier | promote regime to Stress for the window; band widens |
| scaled bands | any | `0 < hard < warn < soft <= ceil` |

## S2 — Interfaces

```rust
// sentinel-core/src/vol.rs
pub struct VolConfig { .. }
impl Default for VolConfig { .. }
pub struct VolState { .. }
impl VolState {
    pub fn new(market_id: MarketId) -> Self;
    pub fn observe(&mut self, ts_ms: u64, mark: Decimal, cfg: &VolConfig);
    pub fn sigma_step(&self, cfg: &VolConfig) -> Option<Decimal>;   // per-tick
    pub fn regime(&self) -> Regime;
}
pub fn adaptive_bands(cfg: &VolConfig, vol: &VolState) -> Option<RiskThresholds>;
pub fn time_to_liq_pct(sigma_step: Decimal, step_ms: u64, distance_pct: Decimal) -> Option<Decimal>; // ms
```

**Module map / dependency arrows (no cycles):**

```
risk.rs  <-- vol.rs        (vol uses RiskThresholds; risk does not know vol)
reflex.rs --> vol.rs       (reflex holds VolState per market, reads bands)
sim/engine.rs --> vol.rs   (same path, so the backtest measures production behaviour)
```

`RiskThresholds` gains no fields; `vol.rs` *produces* one, so every existing
call site and fixture keeps working.

## S3 — Pre-mortem (failure modes -> decision)

| Failure mode | Decision |
|---|---|
| Warmup window: bands undefined for the first N ticks | **Label:** fall back to configured static bands; journal `vol: warmup`. Never widen protection to zero. |
| Division by zero on `P_{t-1} = 0` | **Reject** the sample (marks > 0 enforced upstream too). |
| `Decimal::sqrt` unavailable without a feature flag | **Check first**; fallback is a documented Newton iteration. |
| Adaptive bands make a *later* tier than static in a crash (widening when it matters) | **Constrain by construction:** the stress/dislocation regime may only *tighten*; calm may only *loosen* up to the ceiling. Assert with a property test. |
| Backtest numbers regress | **That is the experiment.** If liquidations-avoided drops, the feature fails its acceptance check and gets reported as such, not tuned away. |
| Threshold provenance bloats the journal | Bands are 3 decimals + sigma + regime = bounded, fixed-size record. |

## S4 — Slices

| # | Slice | Turns green |
|---|---|---|
| 1 | `vol.rs` + unit/property tests (pure, no wiring) | criteria 4, 6 |
| 2 | Wire into `reflex.rs` + `sim/engine.rs`, config flag default-off | workspace green |
| 3 | Threshold provenance in intent/journal | criterion 5 |
| 4 | Re-run both suites with `adaptive=true`; diff vs frozen | criteria 2, 3 |
| 5 | Evidence file + report | criterion 1, all |

Each slice is independently runnable; checks run after every slice.

## S5 — Build order
Strictly slice order. While a check is red, only the fix is allowed.

## S6 — Attack notes
Walked the falsification bank after slice 4. Recorded hits:

**HIT (acceptance criterion #2 falsified).** "FP ↓≥50% while protection
unchanged" cannot be satisfied on the realistic suite: the harness defines a
false positive as *any executed reduce in a scenario whose baseline never
liquidated* (`engine.rs:636`), so on this suite protection and false positive
are the same event. The measured result is +9 trims for +$256 preserved
(correlation +0.561; the same 8 scenarios move in both columns). The criterion
was mis-specified. Reported in `docs/evidence/p22-adaptive-bands.txt` rather
than tuned away; tuning the floor until the number moved would have been
metric-gaming.

**Criterion #3 PASSES with an improvement:** the curated suite gained +7.3%
capital preserved with the 11 avoided liquidations intact and the FP count
unchanged at 3.

**Bank question 1 (the load-bearing assumption).** "Scaling bands by realized
volatility improves the protection economics." Supported on both suites by the
dollar metric; the FP metric is a definitional counterexample, not evidence
against it.

**Bank question 2 (would a wrong core logic fail a runnable check?).** Yes:
`reference_volatility_reproduces_the_base_band` pins the calibration, and
`violent_regimes_never_loosen_below_static` pins the safety asymmetry. Both are
runnable today.

**Bank question 3 (boundary sweep).** Covered by the two proptest sweeps
(1e-3× to 20× reference sigma): ordering, clamps and the safety property hold
across the range; degenerate marks and out-of-order timestamps are rejected by
test.

**Carried forward, not fixed:** the P21 Finding 1 (a 10x–12x position opens
inside any band in `[4%, 40%]`) is untouched by this layer and needs
position-relative normalization; noted in the evidence file.

## S7 — Evidence
`docs/evidence/p22-adaptive-bands.txt` with the before/after tables, the
per-scenario FP/savings diff, and the falsified criterion.

