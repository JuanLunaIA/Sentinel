//! Volatility-adaptive risk bands and liquidation-horizon estimates (P22).
//!
//! Implements the GAP-009 / GAP-028 upgrade specified in the enterprise
//! evaluation: the shipped `soft/warn/hard` thresholds are global constants,
//! applied identically in a dead market and in a cascade. The realistic-tape
//! suite measured the cost of that (`docs/evidence/p21-realistic-bootstrap.txt`:
//! false-positive trims in 5 of 12 scenarios, two negative-saved rows).
//!
//! The model is deliberately deterministic and explainable; there is no ML
//! here, and there is none in this module's future. Two ideas:
//!
//! 1. **Bands scale with realized volatility.** The evaluation specifies
//!    `hard = k_hard * sigma * sqrt(T_rescue)`: the band means "at the current
//!    volatility, price covers this distance in the time it takes me to act".
//!    This module computes exactly that, with `k_hard` folded into a calibrated
//!    reference so that a market sitting at its reference volatility
//!    reproduces the operator's configured base band (8% by default) and every
//!    other regime scales linearly around it:
//!
//!    ```text
//!    sigma_h   = sigma_step * sqrt(horizon_ms / step_ms)      // move over the horizon
//!    hard_pct  = base_hard_pct * (sigma_h / sigma_h_ref)      // linear in sigma
//!              = k_hard * sigma_h * 100                       // the document's form
//!    ```
//!
//!    with `k_hard = base_hard_pct / (100 * sigma_h_ref)`. Clamped to
//!    `[floor_pct, ceil_pct]` so a flat tape cannot produce a zero-width band
//!    and a dislocation cannot produce an unbounded one.
//!
//! 2. **Stress may only tighten protection, never loosen it.** In the Stress
//!    and Dislocation regimes the computed `hard` is floored at the operator's
//!    static band: the adaptive layer is allowed to act *earlier* than the
//!    static configuration when the tape is violent, and allowed to act
//!    *later* only when the tape is calm. That asymmetry is asserted by
//!    `stress_can_only_tighten_never_loosen` and is the structural safety
//!    property of the whole feature.
//!
//! Everything here is pure: no clock, no I/O, no randomness. The caller
//! supplies timestamps and marks, exactly like the rest of `sentinel-core`.

use rust_decimal::{Decimal, MathematicalOps};
use serde::{Deserialize, Serialize};

use crate::risk::RiskThresholds;
use crate::types::MarketId;

/// Default EWMA decay (RiskMetrics-class, 0.94 at the daily horizon).
pub const DEFAULT_LAMBDA: &str = "0.94";

/// Volatility regime, classified from the ratio of current to reference sigma.
///
/// Deterministic state machine, not a model: the boundaries are constants and
/// the classification is a pure function of the observed volatility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    /// Well below reference volatility: positions can sit near the static band
    /// without the time-to-liquidation being short. Loosens up to the ceiling.
    Calm,
    /// Around reference volatility: reproduced the operator's base band.
    Elevated,
    /// Clearly above reference: protection tightens.
    Stress,
    /// Far above reference, or a jump was detected inside the window:
    /// protection tightens hard.
    Dislocation,
}

impl Regime {
    /// Wire/report string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Elevated => "elevated",
            Self::Stress => "stress",
            Self::Dislocation => "dislocation",
        }
    }
}

/// Configuration for the volatility layer (P22 §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolConfig {
    /// EWMA decay factor for the variance update, strictly in `(0, 1)`.
    pub lambda: Decimal,
    /// Samples required before sigma is published; below this the caller keeps
    /// the operator's static bands (documented warmup behavior).
    pub min_samples: u32,
    /// Jump threshold in sigma units: a single-step move beyond this raises
    /// the regime to [`Regime::Dislocation`] for [`Self::jump_hold_ms`].
    pub jump_k: Decimal,
    /// How long a detected jump keeps the dislocation classification.
    pub jump_hold_ms: u64,
    /// The action horizon the band is sized for, milliseconds.
    pub horizon_ms: u64,
    /// Assumed mark cadence, milliseconds (scales sigma to the horizon).
    pub step_ms: u64,
    /// The operator's static hard band in percent; reproduced when the market
    /// sits at [`Self::reference_sigma_step`].
    pub base_hard_pct: Decimal,
    /// Reference per-step sigma (fraction, not percent) that maps to
    /// [`Self::base_hard_pct`]. Defaults to the measured 1-minute ETH sigma
    /// from the real Perpl tape.
    pub reference_sigma_step: Decimal,
    /// Lower clamp on every adaptive band, percent.
    pub floor_pct: Decimal,
    /// Upper clamp on the soft band, percent.
    pub ceil_pct: Decimal,
    /// `warn = hard * warn_ratio`, preserving the shipped 8 -> 15 ratio.
    pub warn_ratio: Decimal,
    /// `soft = hard * soft_ratio`, preserving the shipped 8 -> 25 ratio.
    pub soft_ratio: Decimal,
}

impl Default for VolConfig {
    fn default() -> Self {
        Self {
            lambda: Decimal::new(94, 2),
            min_samples: 20,
            jump_k: Decimal::new(4, 0),
            jump_hold_ms: 900_000,
            horizon_ms: 600_000,
            step_ms: 60_000,
            base_hard_pct: Decimal::new(8, 0),
            reference_sigma_step: Decimal::new(775, 6),
            floor_pct: Decimal::new(4, 0),
            ceil_pct: Decimal::new(40, 0),
            warn_ratio: Decimal::new(1875, 3),
            soft_ratio: Decimal::new(3125, 3),
        }
    }
}

impl VolConfig {
    /// Validate the invariants the layer depends on.
    ///
    /// # Errors
    ///
    /// Returns the offending field name when a value is out of range.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.lambda <= Decimal::ZERO || self.lambda >= Decimal::ONE {
            return Err("lambda");
        }
        if self.step_ms == 0 || self.horizon_ms == 0 {
            return Err("step_ms/horizon_ms");
        }
        if self.jump_k <= Decimal::ZERO {
            return Err("jump_k");
        }
        if self.base_hard_pct <= Decimal::ZERO || self.reference_sigma_step <= Decimal::ZERO {
            return Err("base_hard_pct/reference_sigma_step");
        }
        if self.floor_pct <= Decimal::ZERO || self.ceil_pct < self.floor_pct {
            return Err("floor_pct/ceil_pct");
        }
        if self.warn_ratio <= Decimal::ONE || self.soft_ratio <= self.warn_ratio {
            return Err("warn_ratio/soft_ratio");
        }
        Ok(())
    }

    /// `sqrt(horizon_ms / step_ms)` as a `Decimal`.
    fn horizon_sqrt(&self) -> Option<Decimal> {
        let ratio = Decimal::from(self.horizon_ms) / Decimal::from(self.step_ms);
        ratio.sqrt()
    }
}

/// Rolling volatility state for one market (P22 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolState {
    market_id: MarketId,
    /// EWMA of squared simple returns.
    ewma_var: Decimal,
    samples: u32,
    last_mark: Option<Decimal>,
    last_ts_ms: Option<u64>,
    /// Until this timestamp, the regime is pinned to Dislocation.
    dislocated_until_ms: u64,
}

impl VolState {
    /// Fresh state for one market.
    pub fn new(market_id: MarketId) -> Self {
        Self {
            market_id,
            ewma_var: Decimal::ZERO,
            samples: 0,
            last_mark: None,
            last_ts_ms: None,
            dislocated_until_ms: 0,
        }
    }

    /// Market this state tracks.
    pub fn market_id(&self) -> MarketId {
        self.market_id
    }

    /// Number of return observations folded in so far.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// Fold one mark observation in. Non-positive marks and out-of-order
    /// timestamps are ignored (the state stays valid, it just does not learn
    /// from a bad observation).
    pub fn observe(&mut self, ts_ms: u64, mark: Decimal, cfg: &VolConfig) {
        if mark <= Decimal::ZERO {
            return;
        }
        let previous = match (self.last_mark, self.last_ts_ms) {
            (Some(prev_mark), Some(prev_ts)) => {
                if ts_ms <= prev_ts {
                    return;
                }
                prev_mark
            }
            _ => {
                self.last_mark = Some(mark);
                self.last_ts_ms = Some(ts_ms);
                return;
            }
        };

        // Simple returns rather than log returns: `Decimal` has no `ln`, and
        // the RiskMetrics discrete form is the standard for EWMA variance.
        let ret = (mark - previous) / previous;
        let ret_sq = ret * ret;
        let one_minus = Decimal::ONE - cfg.lambda;
        self.ewma_var = if self.samples == 0 {
            ret_sq
        } else {
            cfg.lambda * self.ewma_var + one_minus * ret_sq
        };
        self.samples = self.samples.saturating_add(1);
        self.last_mark = Some(mark);
        self.last_ts_ms = Some(ts_ms);

        // Jump detector: a single-step move beyond `jump_k` sigma pins the
        // dislocation regime for `jump_hold_ms`.
        if let Some(sigma) = self.sigma_step(cfg)
            && sigma > Decimal::ZERO
        {
            let ratio = ret.abs() / sigma;
            if ratio > cfg.jump_k {
                self.dislocated_until_ms = ts_ms.saturating_add(cfg.jump_hold_ms);
            }
        }
    }

    /// Per-step sigma as a fraction (`0.000775` = 0.0775%), once warmed up.
    ///
    /// Returns `None` during warmup, which the caller reads as "keep the
    /// operator's static bands".
    pub fn sigma_step(&self, cfg: &VolConfig) -> Option<Decimal> {
        if self.samples < cfg.min_samples {
            return None;
        }
        self.ewma_var.sqrt()
    }

    /// Effective sigma for the configured horizon.
    pub fn sigma_horizon(&self, cfg: &VolConfig) -> Option<Decimal> {
        let sigma = self.sigma_step(cfg)?;
        let sqrt = cfg.horizon_sqrt()?;
        Some(sigma * sqrt)
    }

    /// Classify the current regime.
    pub fn regime(&self, cfg: &VolConfig, now_ms: u64) -> Regime {
        if now_ms < self.dislocated_until_ms {
            return Regime::Dislocation;
        }
        let Some(sigma) = self.sigma_step(cfg) else {
            return Regime::Elevated;
        };
        let ratio = sigma / cfg.reference_sigma_step;
        if ratio >= Decimal::new(3, 0) {
            Regime::Dislocation
        } else if ratio >= Decimal::new(16, 1) {
            Regime::Stress
        } else if ratio >= Decimal::new(85, 2) {
            Regime::Elevated
        } else {
            Regime::Calm
        }
    }
}

/// Compute volatility-adapted bands, or `None` during warmup.
///
/// The returned thresholds always satisfy `hard < warn < soft` and the clamps.
/// In the Stress and Dislocation regimes `hard` is floored at
/// `static_hard_pct`, so the adaptive layer can only ever act *earlier* than
/// the operator's static configuration when the tape is violent.
pub fn adaptive_bands(
    vol: &VolState,
    cfg: &VolConfig,
    now_ms: u64,
    static_hard_pct: Decimal,
) -> Option<RiskThresholds> {
    let sigma_h = vol.sigma_horizon(cfg)?;
    let sigma_h_ref = cfg.reference_sigma_step * cfg.horizon_sqrt()?;
    if sigma_h_ref <= Decimal::ZERO {
        return None;
    }

    // hard_pct = base_hard * (sigma_h / sigma_h_ref), i.e. the document's
    // `k_hard * sigma_h * 100` with k_hard folded into the reference.
    let scaled = cfg.base_hard_pct * (sigma_h / sigma_h_ref);

    let regime = vol.regime(cfg, now_ms);
    let mut hard = scaled;
    if matches!(regime, Regime::Stress | Regime::Dislocation) && hard < static_hard_pct {
        hard = static_hard_pct;
    }
    if hard < cfg.floor_pct {
        hard = cfg.floor_pct;
    }

    // Derive the wider bands, then pull everything down together if `soft`
    // would exceed the ceiling (keeps the ratios exact instead of clipping
    // one band and breaking the ordering).
    let mut warn = hard * cfg.warn_ratio;
    let mut soft = hard * cfg.soft_ratio;
    if soft > cfg.ceil_pct {
        let scale = cfg.ceil_pct / soft;
        hard *= scale;
        warn *= scale;
        soft = cfg.ceil_pct;
    }

    let thresholds = RiskThresholds { soft, warn, hard };
    thresholds.validate().ok()?;
    Some(thresholds)
}

/// Estimated milliseconds until the price covers `distance_pct` at the current
/// volatility (P22 §2, GAP-028).
///
/// Model: driftless random walk. The estimate is the horizon at which the
/// distance equals one standard deviation, `t = (d / sigma_step)^2` steps.
/// It answers "how long is this distance at this volatility", which is a
/// scaling indicator for prioritization, not a forecast. A position 9% away in
/// a dead tape and a position 9% away in a dislocation get very different
/// numbers, which is the entire point.
///
/// Returns `None` for non-positive distance or sigma.
pub fn time_to_liq_ms(distance_pct: Decimal, sigma_step: Decimal, step_ms: u64) -> Option<Decimal> {
    if distance_pct <= Decimal::ZERO || sigma_step <= Decimal::ZERO || step_ms == 0 {
        return None;
    }
    let d = distance_pct / Decimal::from(100);
    let steps = (d / sigma_step) * (d / sigma_step);
    Some(steps * Decimal::from(step_ms))
}
