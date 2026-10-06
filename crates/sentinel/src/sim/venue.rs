//! Simulated venue: deterministic fills for the backtest engine
//! (SPEC-P13 §4; implements the production [`Executor`] trait).
//!
//! [`SimExecutor`] fills the whole order at `mark × (1 ∓ slippage_bps/1e4)` —
//! sells ([`CloseSide::CloseLong`]) below the mark, buys
//! ([`CloseSide::CloseShort`]) above it — charges the market's
//! `taker_fee_micros` on the fill notional, and derives every field of the
//! [`ExecutionReport`] from logical time: the tick timestamp set via
//! [`SimExecutor::set_tick`] is the report's `ts_ms` and
//! `sim-<market>-<n>` client order ids count fills per market. There is no
//! wall clock, no randomness and no network anywhere in this module.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rust_decimal::Decimal;
use sentinel_core::order::{CloseSide, OrderRequest};
use sentinel_core::types::{Market, MarketId};

use crate::error::{PerplError, Result};
use crate::execution::{ExecutionReport, ExecutionStatus, Executor};

/// Default slippage penalty applied to the mark, basis points (SPEC-P13 §4).
pub const DEFAULT_SLIPPAGE_BPS: u16 = 10;

/// Basis points denominator: 1 bp = 1/10_000.
const BPS_DENOMINATOR: u32 = 10_000;

/// Micros denominator: fee micros are `1e-6` of the fill notional.
const MICROS_DENOMINATOR: u32 = 1_000_000;

/// One deterministic fill produced by [`SimExecutor`].
#[derive(Debug, Clone, PartialEq)]
pub struct SimFill {
    /// Tick timestamp the fill is stamped with, ms.
    pub ts_ms: u64,
    /// Market of the filled order.
    pub market_id: MarketId,
    /// Client order id (`sim-<market>-<n>`).
    pub client_order_id: String,
    /// Position leg that was closed.
    pub side: CloseSide,
    /// Filled size in base units (positive).
    pub size: Decimal,
    /// Fill price in collateral units per base unit.
    pub price: Decimal,
    /// Taker fee charged for this fill, USD.
    pub fee_usd: Decimal,
}

/// Deterministic in-process venue for the backtest engine (SPEC-P13 §4).
///
/// State is interior-mutable so the async [`Executor`] trait can be
/// implemented with `&self` while the tick loop updates marks and logical
/// time synchronously. All arithmetic is exact `Decimal`; identical
/// scenarios produce identical fills, fees, ids and timestamps.
pub struct SimExecutor {
    slippage_bps: u16,
    tick_ms: Cell<u64>,
    marks: RefCell<HashMap<MarketId, Decimal>>,
    markets: RefCell<HashMap<MarketId, Market>>,
    seq: RefCell<HashMap<MarketId, u64>>,
    fills: RefCell<Vec<SimFill>>,
}

impl SimExecutor {
    /// New executor with the given slippage penalty in basis points.
    pub fn new(slippage_bps: u16) -> Self {
        Self {
            slippage_bps,
            tick_ms: Cell::new(0),
            marks: RefCell::new(HashMap::new()),
            markets: RefCell::new(HashMap::new()),
            seq: RefCell::new(HashMap::new()),
            fills: RefCell::new(Vec::new()),
        }
    }

    /// Register a market so its taker fee schedule is available to fills.
    ///
    /// Re-registering a market replaces the previous document.
    pub fn register_market(&self, market: &Market) {
        self.markets.borrow_mut().insert(market.id, market.clone());
    }

    /// Set the logical timestamp (the tick's `ts_ms`) stamped on reports.
    pub fn set_tick(&self, ts_ms: u64) {
        self.tick_ms.set(ts_ms);
    }

    /// Record the latest mark price for `market_id`.
    pub fn set_mark(&self, market_id: MarketId, mark: Decimal) {
        self.marks.borrow_mut().insert(market_id, mark);
    }

    /// Latest recorded mark price, if any.
    pub fn mark(&self, market_id: MarketId) -> Option<Decimal> {
        self.marks.borrow().get(&market_id).copied()
    }

    /// Slippage penalty applied to fills, basis points.
    pub fn slippage_bps(&self) -> u16 {
        self.slippage_bps
    }

    /// Total taker fees accumulated across all fills, USD.
    pub fn total_fees_usd(&self) -> Decimal {
        self.fills
            .borrow()
            .iter()
            .map(|fill| fill.fee_usd)
            .sum::<Decimal>()
    }

    /// All fills in submission order.
    pub fn fills(&self) -> Vec<SimFill> {
        self.fills.borrow().clone()
    }

    /// The most recent fill, if any order was submitted.
    pub fn last_fill(&self) -> Option<SimFill> {
        self.fills.borrow().last().cloned()
    }
}

impl Default for SimExecutor {
    fn default() -> Self {
        Self::new(DEFAULT_SLIPPAGE_BPS)
    }
}

impl Executor for SimExecutor {
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let mark = self
            .marks
            .borrow()
            .get(&order.market_id)
            .copied()
            .ok_or_else(|| {
                PerplError::Order(format!("no mark price for market {}", order.market_id.0))
            })?;
        let taker_fee_micros = self
            .markets
            .borrow()
            .get(&order.market_id)
            .map(|market| market.taker_fee_micros)
            .ok_or_else(|| {
                PerplError::Order(format!(
                    "market {} not registered with the sim venue",
                    order.market_id.0
                ))
            })?;

        let slippage = Decimal::from(self.slippage_bps) / Decimal::from(BPS_DENOMINATOR);
        let price = match order.close {
            CloseSide::CloseLong => mark * (Decimal::ONE - slippage),
            CloseSide::CloseShort => mark * (Decimal::ONE + slippage),
        };
        let notional = order.size.abs() * price;
        let fee_usd =
            Decimal::from(taker_fee_micros) * notional / Decimal::from(MICROS_DENOMINATOR);

        let sequence = {
            let mut seq = self.seq.borrow_mut();
            let next = seq.entry(order.market_id).or_insert(0);
            *next += 1;
            *next
        };
        let client_order_id = format!("sim-{}-{}", order.market_id.0, sequence);
        let ts_ms = self.tick_ms.get();

        self.fills.borrow_mut().push(SimFill {
            ts_ms,
            market_id: order.market_id,
            client_order_id: client_order_id.clone(),
            side: order.close,
            size: order.size,
            price,
            fee_usd,
        });

        Ok(ExecutionReport {
            order: order.clone(),
            status: ExecutionStatus::Filled,
            filled_size: order.size,
            avg_price: Some(price),
            tx_hash: None,
            client_order_id,
            detail: format!(
                "sim executed: full fill at {price} (mark {mark}, {} bps slippage)",
                self.slippage_bps
            ),
            ts_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use sentinel_core::order::OrderType;

    use super::*;

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

    fn order(close: CloseSide, size: Decimal) -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close,
            size,
            order_type: OrderType::Market,
            max_slippage_bps: 10,
            size_decimals: 3,
        }
    }

    fn executor() -> SimExecutor {
        let executor = SimExecutor::default();
        executor.register_market(&market());
        executor.set_mark(MarketId(32), d("3000"));
        executor.set_tick(12_345);
        executor
    }

    #[test]
    fn default_slippage_is_ten_bps() {
        assert_eq!(DEFAULT_SLIPPAGE_BPS, 10);
        assert_eq!(SimExecutor::default().slippage_bps(), 10);
    }

    #[tokio::test]
    async fn sell_fill_is_below_mark_with_fee_and_sim_id() {
        let executor = executor();
        let report = executor
            .submit(&order(CloseSide::CloseLong, d("0.5")))
            .await
            .expect("sim fill");

        assert_eq!(report.status, ExecutionStatus::Filled, "full fill");
        assert_eq!(report.filled_size, d("0.5"), "full size");
        // 3000 × (1 − 10/10000) = 2997.
        assert_eq!(report.avg_price, Some(d("2997")));
        assert_eq!(report.client_order_id, "sim-32-1");
        assert_eq!(report.ts_ms, 12_345, "tick timestamp, not wall clock");
        assert_eq!(report.order, order(CloseSide::CloseLong, d("0.5")));
        assert_eq!(report.tx_hash, None);

        let fills = executor.fills();
        assert_eq!(fills.len(), 1);
        // 345 µs × (0.5 × 2997) / 1e6 = 0.5169825.
        assert_eq!(fills[0].fee_usd, d("0.5169825"));
        assert_eq!(executor.total_fees_usd(), d("0.5169825"));
        assert_eq!(executor.last_fill(), Some(fills[0].clone()));
    }

    #[tokio::test]
    async fn buy_fill_is_above_mark_with_fee() {
        let executor = executor();
        let report = executor
            .submit(&order(CloseSide::CloseShort, d("0.5")))
            .await
            .expect("sim fill");

        // 3000 × (1 + 10/10000) = 3003.
        assert_eq!(report.avg_price, Some(d("3003")));
        assert_eq!(report.status, ExecutionStatus::Filled);
        // 345 µs × (0.5 × 3003) / 1e6 = 0.5180175.
        assert_eq!(executor.total_fees_usd(), d("0.5180175"));
    }

    #[tokio::test]
    async fn client_order_ids_count_per_market() {
        let executor = executor();
        let first = executor
            .submit(&order(CloseSide::CloseLong, d("0.1")))
            .await
            .expect("first");
        let second = executor
            .submit(&order(CloseSide::CloseLong, d("0.2")))
            .await
            .expect("second");
        assert_eq!(first.client_order_id, "sim-32-1");
        assert_eq!(second.client_order_id, "sim-32-2");

        executor.set_mark(MarketId(20), d("100"));
        let mut other = market();
        other.id = MarketId(20);
        executor.register_market(&other);
        let third = executor
            .submit(&OrderRequest {
                market_id: MarketId(20),
                ..order(CloseSide::CloseLong, d("0.1"))
            })
            .await
            .expect("other market");
        assert_eq!(third.client_order_id, "sim-20-1", "per-market sequence");
    }

    #[tokio::test]
    async fn missing_mark_is_an_order_error() {
        let executor = SimExecutor::default();
        executor.register_market(&market());
        let error = executor
            .submit(&order(CloseSide::CloseLong, d("0.5")))
            .await
            .expect_err("no mark");
        assert!(
            matches!(
                error,
                crate::error::SentinelError::Perpl(PerplError::Order(_))
            ),
            "unexpected: {error:?}"
        );
    }

    #[tokio::test]
    async fn unregistered_market_is_an_order_error() {
        let executor = executor();
        let mut other = market();
        other.id = MarketId(20);
        // Mark known, market doc not registered.
        executor.set_mark(MarketId(20), d("100"));
        let error = executor
            .submit(&OrderRequest {
                market_id: MarketId(20),
                ..order(CloseSide::CloseLong, d("0.5"))
            })
            .await
            .expect_err("unregistered market");
        assert!(
            matches!(
                error,
                crate::error::SentinelError::Perpl(PerplError::Order(_))
            ),
            "unexpected: {error:?}"
        );
    }
}
