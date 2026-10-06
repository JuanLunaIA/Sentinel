//! Reduce-only order sizing — pure math, real units.
//!
//! Orders produced here can never increase exposure: they close or reduce an
//! existing position, respecting the market's lot grid and minimum size
//! (`SPEC-P05.md` §4). Execution semantics (gateway fields, `CloseLong`/
//! `CloseShort`, `rq`, `lb`) live in `crates/sentinel/src/execution/`.
//!
//! **P05 status:** implemented and unit-tested in this module; the public
//! signatures are frozen per `SPEC-P05.md` §4.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{Market, MarketId, Position};

/// Which side of an existing position a reduce-only order closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseSide {
    /// Close (part of) a long position (gateway `t = 3`).
    CloseLong,
    /// Close (part of) a short position (gateway `t = 4`).
    CloseShort,
}

/// Execution style of the reduce order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OrderType {
    /// Market order (`p = 0`), bounded by the request's slippage cap.
    Market,
    /// Marketable limit order at `limit_price`.
    MarketableLimit {
        /// Limit price in price units.
        limit_price: Decimal,
    },
}

/// A reduce-only order request in real (unscaled) units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    /// Market to act on.
    pub market_id: MarketId,
    /// Position leg being closed.
    pub close: CloseSide,
    /// Size in base units (already quantized to the market's lot grid).
    pub size: Decimal,
    /// Execution style.
    pub order_type: OrderType,
    /// Slippage cap for market execution, bps.
    pub max_slippage_bps: u16,
    /// Size scaling of the market (`size = raw / 10^size_decimals`), carried so
    /// executors can emit the gateway's raw integer size.
    pub size_decimals: u32,
}

/// Quantize `size` **down** (towards zero) to the market's lot grid
/// (`10^-decimals`).
///
/// This is a true truncation — never a round-up — so the result can only
/// shrink a requested magnitude, and an order sized from it can never exceed
/// the size it was derived from. `decimals` is the market's `size_decimals`
/// (`raw = size × 10^decimals` on the gateway); negative values truncate
/// towards zero as well.
pub fn quantize_size_down(size: Decimal, decimals: u32) -> Decimal {
    size.trunc_with_scale(decimals)
}

/// Size a reduce-only request from a position and a fraction of its size.
///
/// The request closes `fraction × |pos.size|`, quantized **down** to
/// `market.size_decimals` and clamped so it never exceeds the position's own
/// quantized size. The side follows the position ([`CloseSide::CloseLong`]
/// for a long, [`CloseSide::CloseShort`] for a short), the style is always
/// [`OrderType::Market`], and `max_slippage_bps` plus `market.size_decimals`
/// are carried into the request verbatim.
///
/// Returns `None` (skip, never an increasing order) when:
///
/// - `fraction` is outside `(0, 1]`,
/// - the position is flat (`|pos.size| == 0`),
/// - the quantized size is zero (below one lot), or
/// - the quantized size is below `market.min_size`.
pub fn reduce_by_fraction(
    pos: &Position,
    fraction: Decimal,
    market: &Market,
    max_slippage_bps: u16,
) -> Option<OrderRequest> {
    if fraction <= Decimal::ZERO || fraction > Decimal::ONE {
        return None;
    }

    let position_size = pos.size.abs();
    if position_size.is_zero() {
        return None;
    }

    let decimals = market.size_decimals;
    let size = quantize_size_down(position_size * fraction, decimals)
        .min(quantize_size_down(position_size, decimals));

    if size.is_zero() || size < market.min_size {
        return None;
    }

    Some(OrderRequest {
        market_id: pos.market_id,
        close: if pos.is_long() {
            CloseSide::CloseLong
        } else {
            CloseSide::CloseShort
        },
        size,
        order_type: OrderType::Market,
        max_slippage_bps,
        size_decimals: decimals,
    })
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    /// ETH-like market fixture on market 32; lot grid and minimum size are
    /// parameterized.
    fn market(size_decimals: u32, min_size: Decimal) -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH".to_string(),
            price_decimals: 2,
            size_decimals,
            initial_margin_fraction: dec!(0.083333),
            maintenance_margin_fraction: dec!(0.05),
            max_leverage: dec!(12),
            min_size,
            tick_size: dec!(0.01),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    /// Position on market 32 with the given signed size.
    fn position(size: Decimal) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: dec!(2500),
            mark_price: Some(dec!(2500)),
            liq_price: None,
            collateral: dec!(1000),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: dec!(10),
            opened_at: None,
        }
    }

    #[test]
    fn quantization_table() {
        // SPEC-P05 §4: truncation towards zero at the lot grid 10^-d.
        assert_eq!(quantize_size_down(dec!(0.1239), 3), dec!(0.123));
        assert_eq!(quantize_size_down(dec!(1.9999), 2), dec!(1.99));
        assert_eq!(quantize_size_down(dec!(7.654), 2), dec!(7.65));
        // Exact lot multiples are unchanged.
        assert_eq!(quantize_size_down(dec!(0.125), 3), dec!(0.125));
        assert_eq!(quantize_size_down(dec!(2.100), 3), dec!(2.1));
        assert_eq!(quantize_size_down(dec!(5), 3), dec!(5));
        // Truncation is towards zero for negatives too.
        assert_eq!(quantize_size_down(dec!(-0.1239), 3), dec!(-0.123));
        assert_eq!(quantize_size_down(dec!(-1.9999), 2), dec!(-1.99));
    }

    #[test]
    fn quantization_below_one_lot_is_zero() {
        assert!(quantize_size_down(dec!(0.0004), 3).is_zero());
        assert!(quantize_size_down(dec!(0.99), 0).is_zero());
        // Exactly one lot survives.
        assert_eq!(quantize_size_down(dec!(0.001), 3), dec!(0.001));
        assert_eq!(quantize_size_down(dec!(12.7), 0), dec!(12));
    }

    #[test]
    fn min_size_skip_and_exact_min_pass() {
        let m = market(3, dec!(0.05));

        // 0.80 × 0.05 = 0.04 < min_size 0.05 ⇒ skip.
        assert_eq!(
            reduce_by_fraction(&position(dec!(0.8)), dec!(0.05), &m, 100),
            None
        );

        // Exactly at min_size passes: 0.10 × 0.5 = 0.05.
        let order = reduce_by_fraction(&position(dec!(0.10)), dec!(0.5), &m, 100)
            .expect("0.05 == min_size must pass");
        assert_eq!(order.size, dec!(0.05));
    }

    #[test]
    fn clamp_never_exceeds_quantized_position() {
        let m = market(3, Decimal::ZERO);
        let pos = position(dec!(0.123456));
        let cap = quantize_size_down(pos.size.abs(), 3);
        assert_eq!(cap, dec!(0.123));

        // Full close truncates to the lot grid, never rounds up.
        let order = reduce_by_fraction(&pos, Decimal::ONE, &m, 100).expect("closable");
        assert_eq!(order.size, dec!(0.123));

        // No fraction ever produces more than the quantized position.
        for fraction in [dec!(0.1), dec!(0.33), dec!(0.5), dec!(0.99), dec!(1)] {
            let order = reduce_by_fraction(&pos, fraction, &m, 100).expect("closable");
            assert!(
                order.size <= cap,
                "fraction {fraction} exceeded the quantized position"
            );
        }
    }

    #[test]
    fn fraction_bounds_rejected() {
        let m = market(3, Decimal::ZERO);
        let pos = position(dec!(1.5));
        for fraction in [dec!(0), dec!(-0.25), dec!(-1), dec!(1.5), dec!(2)] {
            assert_eq!(
                reduce_by_fraction(&pos, fraction, &m, 100),
                None,
                "fraction {fraction} must be skipped"
            );
        }
        // 1 is the inclusive upper bound.
        assert!(reduce_by_fraction(&pos, Decimal::ONE, &m, 100).is_some());
    }

    #[test]
    fn flat_position_is_skipped() {
        let m = market(3, Decimal::ZERO);
        assert_eq!(
            reduce_by_fraction(&position(Decimal::ZERO), dec!(0.5), &m, 100),
            None
        );
        assert_eq!(
            reduce_by_fraction(&position(Decimal::ZERO), Decimal::ONE, &m, 100),
            None
        );
    }

    #[test]
    fn tiny_position_below_one_lot_is_skipped() {
        let m = market(3, Decimal::ZERO);

        // 0.0004 truncates to 0 on a 3-decimal grid ⇒ skip.
        assert_eq!(
            reduce_by_fraction(&position(dec!(0.0004)), Decimal::ONE, &m, 100),
            None
        );
        // A small fraction can push below one lot: 0.0039 × 0.1 = 0.00039 ⇒ 0.
        assert_eq!(
            reduce_by_fraction(&position(dec!(0.0039)), dec!(0.1), &m, 100),
            None
        );
        // The one-lot boundary passes: 0.0039 ⇒ 0.003.
        let order =
            reduce_by_fraction(&position(dec!(0.0039)), Decimal::ONE, &m, 100).expect("one lot");
        assert_eq!(order.size, dec!(0.003));
    }

    #[test]
    fn side_mapping_and_request_shape() {
        let m = market(3, Decimal::ZERO);

        let long = reduce_by_fraction(&position(dec!(2.5)), dec!(0.5), &m, 100).expect("long");
        assert_eq!(long.close, CloseSide::CloseLong);
        assert_eq!(long.order_type, OrderType::Market);
        assert_eq!(long.market_id, MarketId(32));
        assert_eq!(long.size, dec!(1.25));

        // Short positions size from the absolute value and use the short side.
        let short = reduce_by_fraction(&position(dec!(-2.5)), dec!(0.5), &m, 100).expect("short");
        assert_eq!(short.close, CloseSide::CloseShort);
        assert_eq!(short.size, long.size);
    }

    #[test]
    fn carries_size_decimals_and_slippage_verbatim() {
        let m = market(4, Decimal::ZERO);
        let order = reduce_by_fraction(&position(dec!(2.5)), dec!(0.25), &m, 137).expect("sized");
        assert_eq!(order.size_decimals, 4);
        assert_eq!(order.max_slippage_bps, 137);
        assert_eq!(order.size, dec!(0.625));
    }

    #[test]
    fn serde_round_trip_carries_size_decimals_and_ms() {
        let m = market(4, dec!(0.01));
        // 1.23456 × 0.5 = 0.61728 ⇒ truncate @ 4 ⇒ 0.6172.
        let order =
            reduce_by_fraction(&position(dec!(-1.23456)), dec!(0.5), &m, 250).expect("sized");
        assert_eq!(order.size, dec!(0.6172));

        let json = serde_json::to_string(&order).expect("serialize");
        let back: OrderRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, order);
        assert_eq!(back.size_decimals, 4);
        assert_eq!(back.max_slippage_bps, 250);
        assert_eq!(back.close, CloseSide::CloseShort);

        // The enums round-trip on their own too.
        let side = serde_json::to_string(&CloseSide::CloseLong).expect("serialize");
        assert_eq!(side, "\"CloseLong\"");
        assert_eq!(
            serde_json::from_str::<CloseSide>(&side).expect("deserialize"),
            CloseSide::CloseLong
        );

        let limit = OrderType::MarketableLimit {
            limit_price: dec!(99.5),
        };
        let json = serde_json::to_string(&limit).expect("serialize");
        assert_eq!(
            serde_json::from_str::<OrderType>(&json).expect("deserialize"),
            limit
        );
    }
}
