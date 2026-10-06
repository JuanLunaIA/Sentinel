//! Do-nothing baseline (SPEC-P13 §5).
//!
//! Walks the scenario price path with no actions: a position liquidates at the
//! first tick where the mark crosses its effective liquidation price
//! (long: `mark <= liq`; short: `mark >= liq`; `liq = pos.liq_price` else the
//! first-principles derivation [`implied_liq_price`]), losing all its isolated
//! collateral. When nothing liquidates the loss is the clamped end-of-path
//! unrealized loss. Everything is exact `Decimal`.

use std::collections::HashMap;

use rust_decimal::Decimal;
use sentinel_core::risk::implied_liq_price;
use sentinel_core::types::{Market, MarketId, Position};

use crate::sim::scenario::Scenario;

/// Baseline outcome for one scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineOutcome {
    /// USD lost by doing nothing (collateral of liquidated positions, or
    /// end-of-path unrealized loss when nothing liquidates).
    pub loss_usd: Decimal,
    /// Number of positions that liquidate along the path.
    pub liquidations: u32,
}

/// Walk the scenario path with no actions (SPEC-P13 §5).
pub fn simulate(scenario: &Scenario) -> BaselineOutcome {
    let markets: HashMap<MarketId, &Market> = scenario
        .markets
        .iter()
        .map(|market| (market.id, market))
        .collect();

    // Last-known mark per market: seeded from the scenario's position
    // documents, then updated by every price-path entry.
    let mut marks: HashMap<MarketId, Decimal> = HashMap::new();
    for position in &scenario.positions {
        if let Some(mark) = position.mark_price {
            marks.entry(position.market_id).or_insert(mark);
        }
    }

    let mut liquidated = vec![false; scenario.positions.len()];
    for tick in &scenario.price_path {
        marks.insert(MarketId(tick.market_id), tick.mark_price);
        for (index, position) in scenario.positions.iter().enumerate() {
            if liquidated[index] {
                continue;
            }
            let Some(mark) = marks.get(&position.market_id).copied() else {
                continue;
            };
            let Some(liq) = markets
                .get(&position.market_id)
                .and_then(|market| liq_price_of(position, market))
            else {
                continue;
            };
            if crossed(position, mark, liq) {
                liquidated[index] = true;
            }
        }
    }

    let liquidations =
        u32::try_from(liquidated.iter().filter(|flag| **flag).count()).unwrap_or(u32::MAX);

    let loss_usd = if liquidations > 0 {
        // Isolated margin: a liquidated position loses ALL its collateral.
        scenario
            .positions
            .iter()
            .zip(&liquidated)
            .filter(|(_, flag)| **flag)
            .map(|(position, _)| position.collateral)
            .sum::<Decimal>()
    } else {
        // Nothing liquidates: the loss is the clamped end-of-path unrealized
        // loss per position (+0 fees, SPEC-P13 §5).
        scenario
            .positions
            .iter()
            .map(|position| {
                let mark = marks
                    .get(&position.market_id)
                    .copied()
                    .or(position.mark_price);
                match mark {
                    Some(mark) => {
                        let unrealized = position.size * (mark - position.entry_price);
                        (-unrealized).max(Decimal::ZERO)
                    }
                    None => Decimal::ZERO,
                }
            })
            .sum::<Decimal>()
    };

    BaselineOutcome {
        loss_usd,
        liquidations,
    }
}

/// Effective liquidation price: the exchange's value when present, else the
/// first-principles derivation (SPEC-P13 §5).
pub(crate) fn liq_price_of(position: &Position, market: &Market) -> Option<Decimal> {
    if let Some(price) = position.liq_price {
        return Some(price);
    }
    implied_liq_price(position, market)
}

/// Whether `mark` has crossed `liq` for the position's side (SPEC-P13 §5):
/// long liquidates at `mark <= liq`, short at `mark >= liq`.
pub(crate) fn crossed(position: &Position, mark: Decimal, liq: Decimal) -> bool {
    if position.size > Decimal::ZERO {
        mark <= liq
    } else if position.size < Decimal::ZERO {
        mark >= liq
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use sentinel_core::types::{Market, MarketId, Position};

    use super::*;
    use crate::sim::scenario::{PriceTick, Scenario, ScenarioLabel};

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH".to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: d("0.083333"),
            maintenance_margin_fraction: d("0.05"),
            max_leverage: d("12"),
            min_size: Decimal::ZERO,
            tick_size: d("0.01"),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    fn position(
        size: Decimal,
        entry: Decimal,
        collateral: Decimal,
        liq: Option<Decimal>,
    ) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: entry,
            mark_price: Some(entry),
            liq_price: liq,
            collateral,
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: d("10"),
            opened_at: None,
        }
    }

    fn scenario(positions: Vec<Position>, marks: &[(i64, &str)]) -> Scenario {
        Scenario {
            id: "baseline-test".to_string(),
            label: ScenarioLabel::Synthetic,
            description: "baseline unit test".to_string(),
            start_free_balance: d("10000"),
            markets: vec![market()],
            positions,
            price_path: marks
                .iter()
                .map(|(ts_ms, mark)| PriceTick {
                    ts_ms: *ts_ms,
                    market_id: 32,
                    mark_price: d(mark),
                })
                .collect(),
            events: Vec::new(),
            reflex: None,
            policy: None,
            decision_trace: None,
        }
    }

    #[test]
    fn long_liquidates_at_the_cross_and_loses_all_collateral() {
        // liq = 100 + (100·1·0.05 − 10)/1 = 95.
        let outcome = simulate(&scenario(
            vec![position(d("1"), d("100"), d("10"), None)],
            &[(1_000, "99"), (2_000, "96"), (3_000, "94")],
        ));
        assert_eq!(outcome.liquidations, 1);
        assert_eq!(outcome.loss_usd, d("10"), "all collateral is lost");
    }

    #[test]
    fn long_liquidates_exactly_at_the_boundary() {
        // mark == liq (95) is a crossing (`mark <= liq`).
        let outcome = simulate(&scenario(
            vec![position(d("1"), d("100"), d("10"), None)],
            &[(1_000, "95")],
        ));
        assert_eq!(outcome.liquidations, 1);
        assert_eq!(outcome.loss_usd, d("10"));
    }

    #[test]
    fn short_liquidates_on_the_upward_cross() {
        // liq = 100 + (100·(−1)·0.05 − 10)/1 = 105.
        let outcome = simulate(&scenario(
            vec![position(d("-1"), d("100"), d("10"), None)],
            &[(1_000, "104"), (2_000, "106")],
        ));
        assert_eq!(outcome.liquidations, 1);
        assert_eq!(outcome.loss_usd, d("10"));
    }

    #[test]
    fn exchange_liq_price_is_preferred_over_the_derivation() {
        // Derived liq would be 95; the exchange says 98 and wins.
        let outcome = simulate(&scenario(
            vec![position(d("1"), d("100"), d("10"), Some(d("98")))],
            &[(1_000, "99"), (2_000, "98")],
        ));
        assert_eq!(outcome.liquidations, 1, "cross at 98");
        assert_eq!(outcome.loss_usd, d("10"));
    }

    #[test]
    fn no_liquidation_uses_clamped_final_unrealized_loss() {
        // Deep collateral, small drift: never crosses; final −20 runs as loss 20.
        let outcome = simulate(&scenario(
            vec![position(d("2"), d("100"), d("500"), None)],
            &[(1_000, "95"), (2_000, "90")],
        ));
        assert_eq!(outcome.liquidations, 0);
        assert_eq!(outcome.loss_usd, d("20"));

        // A profitable path never produces a negative (clamped at 0).
        let outcome = simulate(&scenario(
            vec![position(d("2"), d("100"), d("500"), None)],
            &[(1_000, "110")],
        ));
        assert_eq!(outcome.liquidations, 0);
        assert_eq!(outcome.loss_usd, Decimal::ZERO);
    }

    #[test]
    fn positions_without_marks_never_liquidate_and_contribute_zero() {
        // Position on market 20; the path only ever ticks market 32.
        let mut other = market();
        other.id = MarketId(20);
        let mut scenario = scenario(vec![], &[(1_000, "90")]);
        scenario.markets.push(other);
        scenario.positions.push(Position {
            market_id: MarketId(20),
            ..position(d("1"), d("100"), d("10"), None)
        });
        scenario.positions[0].mark_price = None;

        let outcome = simulate(&scenario);
        assert_eq!(outcome.liquidations, 0);
        assert_eq!(outcome.loss_usd, Decimal::ZERO);
    }
}
