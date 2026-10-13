//! Portfolio-level risk aggregation and cascade planning (GAP-007).
//!
//! Isolated margin is per-position, but *ruin* is per-portfolio. The existing
//! reflex tiers every position independently, so a book where five positions
//! go Orange at the same tick fires five independent reduces in state order,
//! with no notion of which reduction buys the most survival per dollar of fees
//! and slippage. This module supplies that ordering.
//!
//! Everything here is pure and takes injected data, so the `sentinel-core`
//! invariant (no network, no clock, no ambient state) holds. The caller feeds
//! returns and positions; the module returns numbers and a plan.
//!
//! # Model, stated plainly
//!
//! These are *decisions under a documented approximation*, not a claim of
//! accurate risk modelling. The approximations are:
//!
//! 1. **EWMA moments (RiskMetrics).** Return mean, variance and pairwise
//!    covariance are exponential moving averages with decay `lambda`. `0.94`
//!    is the RiskMetrics daily default and gives a ~60-observation half-life.
//! 2. **Asynchronous observations.** When market `m` prints a return, the
//!    pairwise covariance with every other market `n` is updated using `n`'s
//!    most recent return. Markets that print at different times therefore
//!    contribute a slightly stale cross term. That is deliberate: the
//!    alternative (buffering to a common clock) adds latency to a risk path
//!    that must stay cheap.
//! 3. **Linear slice effect.** Reducing a position by fraction `f` is assumed
//!    to remove `f` of its expected liquidation loss. True for small `f`,
//!    optimistic for large `f` where slippage compounds.
//! 4. **Normal tail.** `P(liq)` is `1 - Phi(distance / sigma)`, so fat tails
//!    are understated. The P21 tape measurements (ETH excess kurtosis +10.22,
//!    BTC +3.20) are the evidence that this understates real tail risk; the
//!    ranking it produces is what this module actually uses, and a ranking is
//!    far more robust to a wrong tail than an absolute probability is.

use std::collections::BTreeMap;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, MathematicalOps};

use crate::policy::{BudgetLedger, BudgetLimits, BudgetVerdict};
use crate::types::{MarketId, RiskTier};

/// Tunables for the portfolio layer.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioConfig {
    /// EWMA decay. RiskMetrics daily default is `0.94`.
    pub lambda: Decimal,
    /// Observations before a correlation or volatility is reported as
    /// measured. Below this the module returns `None` rather than guessing.
    pub min_samples: u32,
    /// Tier at or above which a position counts as threatened.
    pub trigger_tier: RiskTier,
    /// How many threatened positions it takes to switch from per-position
    /// reflex to a planned cascade.
    pub min_positions: usize,
    /// Fraction of a threatened position a single slice reduces.
    pub slice_fraction: Decimal,
    /// Round-trip fee plus slippage, as a fraction of notional.
    pub cost_fraction: Decimal,
    /// Fraction of the day's action budget held back from the reflex path
    /// while the book looks correlated (GAP-007 slice 3).
    pub reserve_fraction: Decimal,
    /// Mean measured pairwise correlation at or above which the book counts
    /// as correlated and the reserve engages.
    pub reserve_correlation: Decimal,
    /// Positions the book needs before a reserve is considered.
    pub reserve_min_positions: usize,
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            lambda: Decimal::new(94, 2),
            min_samples: 20,
            trigger_tier: RiskTier::Orange,
            min_positions: 2,
            slice_fraction: Decimal::new(5, 1),
            cost_fraction: Decimal::new(7, 4),
            reserve_fraction: Decimal::new(25, 2),
            reserve_correlation: Decimal::new(5, 1),
            reserve_min_positions: 2,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Moments {
    mean: Decimal,
    var: Decimal,
    samples: u32,
}

/// Canonical (a, b) key so both orderings hit the same entry.
fn pair(a: MarketId, b: MarketId) -> (MarketId, MarketId) {
    if a.0 <= b.0 { (a, b) } else { (b, a) }
}

/// Rolling EWMA correlation and volatility per market.
#[derive(Debug, Clone)]
pub struct CorrelationTracker {
    cfg: PortfolioConfig,
    moments: BTreeMap<MarketId, Moments>,
    cov: BTreeMap<(MarketId, MarketId), (Decimal, u32)>,
    observations: u32,
}

impl CorrelationTracker {
    /// Build a tracker with explicit tunables.
    pub fn new(cfg: PortfolioConfig) -> Self {
        Self {
            cfg,
            moments: BTreeMap::new(),
            cov: BTreeMap::new(),
            observations: 0,
        }
    }

    /// Total returns observed, across every market.
    pub fn observations(&self) -> u32 {
        self.observations
    }

    /// Returns observed for one market.
    pub fn samples(&self, market: MarketId) -> u32 {
        self.moments.get(&market).map(|m| m.samples).unwrap_or(0)
    }

    /// Feed one time bucket's returns: every market that printed in it.
    ///
    /// The call *is* the bucket, so no bucket id is taken: a parameter the
    /// tracker does not read would imply it pairs across calls, which is the
    /// defect the next paragraph describes. One call is one bucket, and cross
    /// terms pair returns that genuinely share
    /// a time step. Two earlier designs were rejected for this reason:
    /// pairing against the other market's most recent deviation produced
    /// correlations near zero for series that move together (it compares
    /// returns from different times), and keeping a bounded per-market window
    /// silently dropped pairs once a caller fed more buckets than the window
    /// held, understating the covariance by exactly the dropped fraction. With
    /// a single bucket per call neither failure mode exists: there is nothing
    /// to evict and nothing to pair across time.
    ///
    /// A market that prints more than once in a bucket has its log returns
    /// summed, which is what a return series does over a period. A market
    /// absent from the slice contributes no cross term for that bucket, so a
    /// gap yields no information rather than wrong information.
    pub fn observe(&mut self, returns: &[(MarketId, Decimal)]) {
        let lambda = self.cfg.lambda;
        let one_minus = Decimal::ONE - lambda;

        // Bucket totals first, so every market's deviation is measured
        // against the state before this bucket was applied.
        let mut totals: BTreeMap<MarketId, Decimal> = BTreeMap::new();
        for (market, ret) in returns {
            *totals.entry(*market).or_insert(Decimal::ZERO) += *ret;
        }

        let mut deviations: BTreeMap<MarketId, Decimal> = BTreeMap::new();
        for (market, total) in &totals {
            let entry = self.moments.entry(*market).or_default();
            let deviation = if entry.samples == 0 {
                Decimal::ZERO
            } else {
                *total - entry.mean
            };
            if entry.samples == 0 {
                entry.mean = *total;
                entry.var = Decimal::ZERO;
            } else {
                entry.var = lambda * entry.var + one_minus * deviation * deviation;
                entry.mean = lambda * entry.mean + one_minus * *total;
            }
            entry.samples = entry.samples.saturating_add(1);
            deviations.insert(*market, deviation);
        }

        let ids: Vec<MarketId> = deviations.keys().copied().collect();
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                let term = deviations[a] * deviations[b];
                let slot = self.cov.entry(pair(*a, *b)).or_insert((Decimal::ZERO, 0));
                slot.0 = lambda * slot.0 + one_minus * term;
                slot.1 = slot.1.saturating_add(1);
            }
        }
        self.observations = self.observations.saturating_add(1);
    }

    /// Feed one market's return series, one return per bucket.
    ///
    /// Convenience for tests and for callers with a single-market series.
    pub fn observe_series(&mut self, market: MarketId, rets: &[Decimal]) {
        for ret in rets {
            self.observe(&[(market, *ret)]);
        }
    }

    /// EWMA volatility for one market, or `None` below `min_samples`.
    pub fn volatility(&self, market: MarketId) -> Option<Decimal> {
        let moments = self.moments.get(&market)?;
        if moments.samples < self.cfg.min_samples {
            return None;
        }
        let var = moments.var;
        if var <= Decimal::ZERO {
            return None;
        }
        Some(decimal_sqrt(var))
    }

    /// EWMA correlation for a pair, clamped to `[-1, 1]`, or `None` below
    /// `min_samples` on either series or the pair.
    pub fn correlation(&self, a: MarketId, b: MarketId) -> Option<Decimal> {
        if a == b {
            return Some(Decimal::ONE);
        }
        let (key, samples) = self.cov.get(&pair(a, b)).copied()?;
        if samples < self.cfg.min_samples {
            return None;
        }
        let sigma_a = self.volatility(a)?;
        let sigma_b = self.volatility(b)?;
        let denominator = sigma_a * sigma_b;
        if denominator <= Decimal::ZERO {
            return None;
        }
        let value = key / denominator;
        Some(value.clamp(Decimal::NEGATIVE_ONE, Decimal::ONE))
    }

    /// Correlation if measured, `fallback` otherwise.
    ///
    /// Used where a caller must produce a ranking even during warmup: assuming
    /// full independence (`0`) is the conservative default, because it makes
    /// the concentration term vanish rather than inventing diversification.
    pub fn correlation_or(&self, a: MarketId, b: MarketId, fallback: Decimal) -> Decimal {
        self.correlation(a, b).unwrap_or(fallback)
    }

    /// The config in force.
    pub fn config(&self) -> &PortfolioConfig {
        &self.cfg
    }
}

/// Square root, via the crate's `maths` implementation.
///
/// `rust_decimal` ships no square root in its base feature set, and this
/// workspace enables `maths`, so the tested implementation is used rather than
/// a hand-rolled Newton iteration.
fn decimal_sqrt(value: Decimal) -> Decimal {
    if value <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    value.sqrt().unwrap_or(Decimal::ZERO)
}

/// One position as the portfolio layer sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRisk {
    /// Market the position lives in.
    pub market_id: MarketId,
    /// Position notional, USD.
    pub notional: Decimal,
    /// Collateral posted to this isolated position, USD.
    pub collateral: Decimal,
    /// Adverse fraction that would liquidate it, e.g. `0.25` = 25% move.
    pub distance_to_liq: Decimal,
    /// Current tier from the reflex classifier.
    pub tier: RiskTier,
}

/// Portfolio-level view at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioRisk {
    /// Sum of collateral over threatened positions, each weighted by its
    /// modelled liquidation probability.
    pub equity_at_risk: Decimal,
    /// The naive view: sum of collateral over threatened positions, ignoring
    /// probability. The difference between the two is the correlation-aware
    /// part.
    pub collateral_at_risk: Decimal,
    /// Sum of modelled liquidation probabilities: how many positions are
    /// expected to go at once.
    pub expected_liquidations: Decimal,
    /// Strongest measured correlation among threatened pairs.
    pub worst_pair: Option<(MarketId, MarketId, Decimal)>,
    /// Threatened positions in the book.
    pub threatened: usize,
}

impl PortfolioRisk {
    /// Whether the book has enough simultaneous exposure to warrant a plan.
    pub fn cascade_triggered(&self, cfg: &PortfolioConfig) -> bool {
        self.threatened >= cfg.min_positions
    }
}

fn tier_rank(tier: RiskTier) -> u8 {
    match tier {
        RiskTier::Green => 0,
        RiskTier::Yellow => 1,
        RiskTier::Orange => 2,
        RiskTier::Red => 3,
    }
}

fn is_threatened(tier: RiskTier, trigger: RiskTier) -> bool {
    tier_rank(tier) >= tier_rank(trigger)
}

/// Normal tail probability above `z` (`P(X > z)` for a standard normal).
///
/// Approximated with the logistic form `1 / (1 + exp(-1.702 z))`, which keeps
/// every operation in `Decimal` and is accurate to ~1e-2 on the region this
/// module uses. Approximation 4 in the module header documents why a coarse
/// tail is acceptable here: the output is a ranking, not a probability claim.
fn tail_probability(z: Decimal) -> Decimal {
    let scaled = Decimal::new(-1702, 3) * z;
    let exp = decimal_exp(scaled);
    Decimal::ONE / (Decimal::ONE + exp)
}

/// `exp(x)`, via the crate's `maths` implementation, clamped so the logistic
/// above cannot overflow.
fn decimal_exp(x: Decimal) -> Decimal {
    let x = x.clamp(Decimal::new(-20, 0), Decimal::new(20, 0));
    x.exp()
}

/// Assess the book's aggregate risk.
pub fn assess(
    positions: &[PositionRisk],
    tracker: &CorrelationTracker,
    cfg: &PortfolioConfig,
) -> PortfolioRisk {
    let threatened: Vec<&PositionRisk> = positions
        .iter()
        .filter(|p| is_threatened(p.tier, cfg.trigger_tier))
        .collect();

    let mut equity_at_risk = Decimal::ZERO;
    let mut collateral_at_risk = Decimal::ZERO;
    let mut expected_liquidations = Decimal::ZERO;
    for position in &threatened {
        let sigma = tracker.volatility(position.market_id);
        let probability = match sigma {
            Some(sigma) if sigma > Decimal::ZERO => {
                tail_probability(position.distance_to_liq / sigma)
            }
            // No measured volatility yet: treat the position as likely rather
            // than as safe, which is the conservative direction for a risk
            // layer.
            _ => Decimal::ONE,
        };
        equity_at_risk += position.collateral * probability;
        collateral_at_risk += position.collateral;
        expected_liquidations += probability;
    }

    let mut worst_pair = None;
    for (i, a) in threatened.iter().enumerate() {
        for b in threatened.iter().skip(i + 1) {
            let Some(rho) = tracker.correlation(a.market_id, b.market_id) else {
                continue;
            };
            if worst_pair.is_none_or(|(_, _, best)| rho > best) {
                worst_pair = Some((a.market_id, b.market_id, rho));
            }
        }
    }

    PortfolioRisk {
        equity_at_risk,
        collateral_at_risk,
        expected_liquidations,
        worst_pair,
        threatened: threatened.len(),
    }
}

/// One planned reduction step.
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    /// Market to reduce.
    pub market_id: MarketId,
    /// Fraction of the position to reduce.
    pub reduce_fraction: Decimal,
    /// Expected reduction in portfolio equity-at-risk.
    pub expected_ear_reduction: Decimal,
    /// Estimated fee plus slippage for the slice.
    pub est_cost: Decimal,
    /// `expected_ear_reduction / est_cost`: the ranking key.
    pub ratio: Decimal,
}

/// A planned cascade, or the reasons no plan was produced.
#[derive(Debug, Clone, PartialEq)]
pub struct CascadePlan {
    /// Slices in execution order, best survival bought per dollar first.
    pub slices: Vec<Slice>,
    /// Positions the budget layer refused, with the verdict that refused them.
    pub denied: Vec<(MarketId, BudgetVerdict)>,
}

impl CascadePlan {
    /// Whether the book warrants a plan at all.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty() && self.denied.is_empty()
    }
}

/// Order the book's threatened positions into a cascade plan.
///
/// Ranking key is *expected portfolio survival bought per dollar of cost*:
///
/// ```text
/// ratio_i = (collateral_i * P(liq_i)) / (notional_i * cost_fraction)
///           * (1 + mean correlation of i with the other threatened legs)
/// ```
///
/// The base term is the marginal equity-at-risk removed per unit of cost. The
/// concentration factor is what makes the ordering portfolio-aware: when the
/// threatened legs are the same bet, reducing the leg that shares the most
/// exposure buys the most aggregate survival, and the plan diverges from the
/// naive collateral ordering.
///
/// Budgets are consulted with the pure `BudgetLedger::check`, so ranking
/// candidates never consumes budget. Refused positions land in `denied`, which
/// keeps the plan explicit about what the budget forbade rather than silently
/// dropping it.
pub fn plan_cascade(
    positions: &[PositionRisk],
    tracker: &CorrelationTracker,
    ledger: &BudgetLedger,
    limits: &BudgetLimits,
    cfg: &PortfolioConfig,
) -> CascadePlan {
    let threatened: Vec<&PositionRisk> = positions
        .iter()
        .filter(|p| is_threatened(p.tier, cfg.trigger_tier))
        .collect();
    if threatened.len() < cfg.min_positions {
        return CascadePlan {
            slices: Vec::new(),
            denied: Vec::new(),
        };
    }

    let mut denied = Vec::new();
    let mut candidates: Vec<Slice> = Vec::new();
    for position in &threatened {
        let verdict = ledger.check(position.market_id, limits);
        if !matches!(verdict, BudgetVerdict::Ok) {
            denied.push((position.market_id, verdict));
            continue;
        }
        let sigma = tracker.volatility(position.market_id);
        let probability = match sigma {
            Some(sigma) if sigma > Decimal::ZERO => {
                tail_probability(position.distance_to_liq / sigma)
            }
            _ => Decimal::ONE,
        };
        let peers: Vec<MarketId> = threatened
            .iter()
            .filter(|other| other.market_id != position.market_id)
            .map(|other| other.market_id)
            .collect();
        let concentration = if peers.is_empty() {
            Decimal::ONE
        } else {
            let mut sum = Decimal::ZERO;
            for peer in &peers {
                // Warmup assumes independence, so the factor vanishes rather
                // than inventing shared risk.
                sum += tracker.correlation_or(position.market_id, *peer, Decimal::ZERO);
            }
            Decimal::ONE + sum / Decimal::from(peers.len())
        };

        let expected_loss = position.collateral * probability;
        let fee = position.notional * cfg.cost_fraction;
        if fee <= Decimal::ZERO {
            continue;
        }
        let weighted = expected_loss * concentration;
        candidates.push(Slice {
            market_id: position.market_id,
            reduce_fraction: cfg.slice_fraction,
            expected_ear_reduction: weighted * cfg.slice_fraction,
            est_cost: fee * cfg.slice_fraction,
            ratio: weighted / fee,
        });
    }

    // Best survival per dollar first; ties break by market id so a plan is
    // reproducible rather than dependent on map iteration order.
    candidates.sort_by(|a, b| {
        b.ratio
            .cmp(&a.ratio)
            .then_with(|| a.market_id.0.cmp(&b.market_id.0))
    });

    CascadePlan {
        slices: candidates,
        denied,
    }
}

/// Whether the book looks correlated enough to hold budget back.
///
/// Warmup is deliberately not "watching": with no measured pair the reserve
/// stays off, because engaging it on an unmeasured book would hold actions
/// back on the strength of a correlation nobody measured.
pub fn book_watching(
    tracker: &CorrelationTracker,
    positions: &[PositionRisk],
    cfg: &PortfolioConfig,
) -> bool {
    if positions.len() < cfg.reserve_min_positions {
        return false;
    }
    let mut sum = Decimal::ZERO;
    let mut measured = 0u32;
    for (i, a) in positions.iter().enumerate() {
        for b in positions.iter().skip(i + 1) {
            if let Some(rho) = tracker.correlation(a.market_id, b.market_id) {
                sum += rho;
                measured += 1;
            }
        }
    }
    if measured == 0 {
        return false;
    }
    sum / Decimal::from(measured) >= cfg.reserve_correlation
}

/// Actions to hold back from the reflex path so a cascade still has budget.
///
/// This is the piece the ordering-only planner was missing. Measured on the
/// realistic suite, reordering the evaluation loop moved the result by
/// exactly $0.00, because the day's action is consumed by a single-position
/// event before the cascade ever triggers. A reserve changes that: while the
/// book looks correlated, the reflex path sees a smaller daily cap, so the
/// budget a cascade needs is still there when the cascade fires.
///
/// The reserve is at least one action once it engages, because a fraction of a
/// small budget floors to zero and would reserve nothing exactly when the
/// budget is scarcest.
pub fn reserve_actions(
    tracker: &CorrelationTracker,
    positions: &[PositionRisk],
    actions_available: u32,
    cfg: &PortfolioConfig,
) -> u32 {
    if !book_watching(tracker, positions, cfg) {
        return 0;
    }
    let fraction = Decimal::from(actions_available) * cfg.reserve_fraction;
    let whole = fraction.trunc().to_u32().unwrap_or(0);
    whole.max(1).min(actions_available)
}
