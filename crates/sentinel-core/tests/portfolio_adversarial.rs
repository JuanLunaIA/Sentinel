//! Adversarial suite for the portfolio layer (GAP-007).
//!
//! The load-bearing claims, in order of how much they matter:
//!
//! 1. **The correlation term changes the plan.** Two positions with identical
//!    collateral and notional must rank differently when one shares more
//!    exposure with the rest of the book. If this did not hold, the module
//!    would be a dressed-up collateral sort.
//! 2. **Nothing is invented during warmup.** Correlations and volatilities are
//!    `None` below `min_samples`; the planner then assumes independence rather
//!    than fabricating shared risk, and the assessor treats an unmeasured
//!    position as likely rather than safe.
//! 3. **Budgets are honoured without being consumed.** Refused positions are
//!    reported, and ranking never spends budget.
//! 4. **Plans are reproducible.** Ties break by market id, not map order.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sentinel_core::policy::{BudgetLedger, BudgetLimits};
use sentinel_core::portfolio::{
    CorrelationTracker, PortfolioConfig, PositionRisk, assess, plan_cascade,
};
use sentinel_core::types::{MarketId, RiskTier};

const ETH: MarketId = MarketId(32);
const BTC: MarketId = MarketId(16);
const SOL: MarketId = MarketId(48);

fn cfg() -> PortfolioConfig {
    PortfolioConfig {
        lambda: dec!(0.94),
        min_samples: 5,
        trigger_tier: RiskTier::Orange,
        min_positions: 2,
        slice_fraction: dec!(0.5),
        cost_fraction: dec!(0.0007),
    }
}

/// Feed several markets' series that share bucket indices, one bucket per call.
/// This is how the simulator feeds the tracker.
fn feed_aligned(tracker: &mut CorrelationTracker, series: &[(MarketId, &[Decimal])]) {
    let buckets = series.iter().map(|(_, r)| r.len()).max().unwrap_or(0);
    for bucket in 0..buckets {
        let returns: Vec<(MarketId, Decimal)> = series
            .iter()
            .filter_map(|(market, rets)| rets.get(bucket).map(|ret| (*market, *ret)))
            .collect();
        tracker.observe(&returns);
    }
}

fn limits() -> BudgetLimits {
    BudgetLimits {
        global_max_order_size_usd: dec!(100000),
        per_market_max_order_size_usd: BTreeMap::new(),
        global_daily_actions: 100,
        per_market_daily_actions: BTreeMap::new(),
        max_book_daily_actions: None,
    }
}

fn position(
    market: MarketId,
    collateral: Decimal,
    notional: Decimal,
    tier: RiskTier,
) -> PositionRisk {
    PositionRisk {
        market_id: market,
        notional,
        collateral,
        distance_to_liq: dec!(0.12),
        tier,
    }
}

#[test]
fn a_market_is_perfectly_correlated_with_itself() {
    let tracker = CorrelationTracker::new(cfg());
    assert_eq!(tracker.correlation(ETH, ETH), Some(Decimal::ONE));
}

#[test]
fn nothing_is_reported_before_min_samples() {
    let mut tracker = CorrelationTracker::new(cfg());
    feed_aligned(
        &mut tracker,
        &[
            (ETH, &[dec!(0.01), dec!(0.02), dec!(0.01)]),
            (BTC, &[dec!(0.01), dec!(0.02), dec!(0.01)]),
        ],
    );
    assert_eq!(tracker.volatility(ETH), None, "below min_samples");
    assert_eq!(tracker.correlation(ETH, BTC), None, "below min_samples");

    // Cross the threshold and the measurement appears.
    feed_aligned(
        &mut tracker,
        &[
            (ETH, &[dec!(0.01), dec!(0.02)]),
            (BTC, &[dec!(0.01), dec!(0.02)]),
        ],
    );
    assert!(tracker.volatility(ETH).is_some());
    assert!(tracker.correlation(ETH, BTC).is_some());
}

#[test]
fn correlation_tracks_direction_and_is_symmetric() {
    let mut tracker = CorrelationTracker::new(cfg());
    let up: Vec<Decimal> = [1, 2, -1, 3, -2, 1, 2, -1, 2, 1]
        .iter()
        .map(|v| Decimal::from(*v) / Decimal::from(1000))
        .collect();
    let down: Vec<Decimal> = up.iter().map(|v| -*v).collect();
    feed_aligned(&mut tracker, &[(ETH, &up), (BTC, &up), (SOL, &down)]);

    let same = tracker.correlation(ETH, BTC).expect("measured");
    assert!(same > dec!(0.9), "co-moving series correlate high: {same}");
    assert_eq!(
        tracker.correlation(ETH, BTC),
        tracker.correlation(BTC, ETH),
        "symmetric"
    );

    let opposite = tracker.correlation(ETH, SOL).expect("measured");
    assert!(
        opposite < dec!(-0.9),
        "mirror series correlate low: {opposite}"
    );
}

#[test]
fn assess_weights_by_tail_probability_and_never_exceeds_collateral() {
    let mut tracker = CorrelationTracker::new(cfg());
    let rets: Vec<Decimal> = [1, -2, 3, -1, 2, -3, 1, 2, -1, 1]
        .iter()
        .map(|v| Decimal::from(*v) / Decimal::from(1000))
        .collect();
    feed_aligned(&mut tracker, &[(ETH, &rets), (BTC, &rets)]);

    let book = vec![
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
        position(BTC, dec!(100), dec!(1000), RiskTier::Red),
        position(SOL, dec!(100), dec!(1000), RiskTier::Green), // not threatened
    ];
    let risk = assess(&book, &tracker, &cfg());
    assert_eq!(risk.threatened, 2, "Green is below the trigger tier");
    assert_eq!(risk.collateral_at_risk, dec!(200));
    assert!(
        risk.equity_at_risk <= risk.collateral_at_risk,
        "probability weighting cannot exceed the collateral"
    );
    assert!(risk.equity_at_risk > Decimal::ZERO);
    let (_, _, rho) = risk.worst_pair.expect("correlated pair measured");
    assert!(rho > dec!(0.9));
}

#[test]
fn an_unmeasured_position_is_treated_as_likely_not_safe() {
    let tracker = CorrelationTracker::new(cfg()); // no observations at all
    let book = vec![position(ETH, dec!(100), dec!(1000), RiskTier::Orange)];
    let risk = assess(&book, &tracker, &cfg());
    assert_eq!(
        risk.equity_at_risk,
        dec!(100),
        "with no measured volatility the loss is assumed, not discounted"
    );
}

#[test]
fn cascade_triggers_only_with_enough_threatened_positions() {
    let tracker = CorrelationTracker::new(cfg());
    let one = vec![position(ETH, dec!(100), dec!(1000), RiskTier::Orange)];
    let two = vec![
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
        position(BTC, dec!(100), dec!(1000), RiskTier::Red),
    ];
    assert!(!assess(&one, &tracker, &cfg()).cascade_triggered(&cfg()));
    assert!(assess(&two, &tracker, &cfg()).cascade_triggered(&cfg()));
}

#[test]
fn the_plan_ranks_survival_bought_per_dollar() {
    let tracker = CorrelationTracker::new(cfg());
    let ledger = BudgetLedger::default();
    // Same tier; ETH has more collateral behind the same notional, so it buys
    // more survival per dollar of fee.
    let book = vec![
        position(BTC, dec!(60), dec!(1000), RiskTier::Orange),
        position(ETH, dec!(120), dec!(1000), RiskTier::Orange),
    ];
    let plan = plan_cascade(&book, &tracker, &ledger, &limits(), &cfg());
    assert_eq!(plan.slices.len(), 2);
    assert_eq!(plan.slices[0].market_id, ETH, "highest ratio first");
    assert!(plan.slices[0].ratio > plan.slices[1].ratio);
    assert!(plan.denied.is_empty());
}

#[test]
fn the_correlation_term_changes_the_order() {
    // The design claim, in its sharpest form: SOL carries the MOST collateral
    // with the SAME notional, so a sort by collateral-bought-per-fee-dollar
    // ranks it first. It is also the only leg that does not share the book's
    // exposure, so a portfolio-aware planner must rank it LAST. The two
    // orderings must differ, or the correlation term is decoration.
    let mut tracker = CorrelationTracker::new(cfg());
    let moves: Vec<Decimal> = [3, -2, 4, -3, 2, -4, 3, 2, -3, 4, -2, 3]
        .iter()
        .map(|v| Decimal::from(*v) / Decimal::from(1000))
        .collect();
    let noise: Vec<Decimal> = [1, -4, 2, -1, 4, -2, 1, -3, 3, -1, 2, -4]
        .iter()
        .map(|v| Decimal::from(*v) / Decimal::from(1000))
        .collect();
    feed_aligned(&mut tracker, &[(ETH, &moves), (BTC, &moves), (SOL, &noise)]);

    let correlated = tracker.correlation(ETH, BTC).expect("measured");
    let independent = tracker.correlation(SOL, BTC).expect("measured");
    assert!(
        correlated > independent,
        "fixture must separate the two: {correlated} vs {independent}"
    );

    let ledger = BudgetLedger::default();
    let book = vec![
        position(SOL, dec!(130), dec!(1000), RiskTier::Orange),
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
        position(BTC, dec!(100), dec!(1000), RiskTier::Orange),
    ];
    let plan = plan_cascade(&book, &tracker, &ledger, &limits(), &cfg());
    assert_eq!(plan.slices.len(), 3);

    // A collateral sort would emit SOL, then BTC, then ETH. The planner must
    // not: SOL is the independent leg.
    assert_eq!(
        plan.slices[2].market_id, SOL,
        "the highest-collateral leg ranks last because it shares no exposure"
    );
    assert_eq!(plan.slices[0].market_id, BTC, "correlated pair ranks first");
    assert_eq!(plan.slices[1].market_id, ETH);
    assert!(
        plan.slices[2].ratio < plan.slices[0].ratio,
        "the concentration term must outweigh SOL's larger collateral"
    );
}

#[test]
fn warmup_assumes_independence_rather_than_inventing_shared_risk() {
    let tracker = CorrelationTracker::new(cfg()); // unmeasured
    let ledger = BudgetLedger::default();
    let book = vec![
        position(SOL, dec!(100), dec!(1000), RiskTier::Orange),
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
    ];
    let plan = plan_cascade(&book, &tracker, &ledger, &limits(), &cfg());
    // Identical inputs, no correlation information: the tie breaks by market
    // id so the plan is reproducible.
    assert_eq!(plan.slices[0].market_id, ETH, "id 32 before id 48");
    assert_eq!(plan.slices[0].ratio, plan.slices[1].ratio);
}

#[test]
fn budget_refusals_are_reported_and_ranking_does_not_consume_budget() {
    let tracker = CorrelationTracker::new(cfg());
    let mut ledger = BudgetLedger::default();
    let mut limits = limits();
    limits.global_daily_actions = 2;
    limits.per_market_daily_actions.insert(ETH, 1);
    ledger.record(ETH, &limits); // ETH's market cap is now spent

    let book = vec![
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
        position(BTC, dec!(100), dec!(1000), RiskTier::Orange),
    ];
    let before = ledger.total_actions();
    let plan = plan_cascade(&book, &tracker, &ledger, &limits, &cfg());

    assert_eq!(plan.slices.len(), 1, "only BTC is admissible");
    assert_eq!(plan.slices[0].market_id, BTC);
    assert_eq!(plan.denied.len(), 1);
    assert_eq!(plan.denied[0].0, ETH);
    assert_eq!(
        ledger.total_actions(),
        before,
        "planning is a query, not a commitment"
    );
}

#[test]
fn a_single_threatened_position_produces_no_plan() {
    let tracker = CorrelationTracker::new(cfg());
    let ledger = BudgetLedger::default();
    let book = vec![
        position(ETH, dec!(100), dec!(1000), RiskTier::Orange),
        position(BTC, dec!(100), dec!(1000), RiskTier::Green),
    ];
    let plan = plan_cascade(&book, &tracker, &ledger, &limits(), &cfg());
    assert!(plan.is_empty(), "one position stays on the reflex path");
}
