//! Reduce-only order sizing — pure math, real units.
//!
//! Orders produced here can never increase exposure: they close or reduce an
//! existing position, respecting the market's lot grid and minimum size
//! (`SPEC-P05.md` §4). Execution semantics (gateway fields, `CloseLong`/
//! `CloseShort`, `rq`, `lb`) live in `crates/sentinel/src/execution/`.
//!
//! **Skeleton status (P05):** interfaces frozen; implemented by the P05 wave.

use rust_decimal::Decimal;

use crate::types::{Market, MarketId, Position};

/// Which side of an existing position a reduce-only order closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseSide {
    /// Close (part of) a long position (gateway `t = 3`).
    CloseLong,
    /// Close (part of) a short position (gateway `t = 4`).
    CloseShort,
}

/// Execution style of the reduce order.
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
/// **STUB — implemented by the P05 wave.**
pub fn quantize_size_down(size: Decimal, decimals: u32) -> Decimal {
    let _ = (size, decimals);
    todo!("P05 agent order: truncate at the lot grid")
}

/// Size a reduce-only request from a position and a fraction of its size.
///
/// Returns `None` (skip, never an increasing order) when the fraction is
/// outside `(0, 1]`, the position is flat, the quantized size is zero, or it
/// falls below `market.min_size`. The result never exceeds the position's own
/// quantized size.
///
/// **STUB — implemented by the P05 wave.**
pub fn reduce_by_fraction(
    pos: &Position,
    fraction: Decimal,
    market: &Market,
    max_slippage_bps: u16,
) -> Option<OrderRequest> {
    let _ = (pos, fraction, market, max_slippage_bps);
    todo!("P05 agent order: size + side + min-size clamp")
}
