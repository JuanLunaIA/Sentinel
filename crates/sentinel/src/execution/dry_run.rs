//! DRY_RUN executor — local fills at mark ± slippage, reports persisted.
//!
//! **P05 status:** implemented; interfaces frozen per `SPEC-P05.md` §5.

use std::path::PathBuf;

use rust_decimal::Decimal;
use sentinel_core::order::{CloseSide, OrderRequest};

use super::{
    ExecutionReport, ExecutionStatus, Executor, PositionProbe, SeqCounter, append_jsonl,
    client_order_id, unix_ms,
};
use crate::error::{PerplError, Result};

/// Basis points denominator: 1 bp = 1/10_000.
const BPS_DENOMINATOR: u32 = 10_000;

/// Simulates fills at the current mark with a bps slippage penalty.
///
/// The fill price is `mark × (1 − slippage_bps/10_000)` for a sell
/// ([`CloseSide::CloseLong`]) and `mark × (1 + slippage_bps/10_000)` for a buy
/// ([`CloseSide::CloseShort`]); the whole request fills at that price and the
/// report is appended as one JSON line to `report_path`.
pub struct DryRunExecutor<P> {
    probe: P,
    slippage_bps: u16,
    report_path: PathBuf,
    seq: SeqCounter,
}

impl<P> DryRunExecutor<P> {
    /// Build the executor.
    ///
    /// `slippage_bps` is the configured penalty applied to the mark; every
    /// simulated submit appends one JSON report line to `report_path`
    /// (parent directories are created on demand). `seq_seed` seeds the
    /// client-order-id counter, so the first id is `sentinel-<market>-<seed+1>`.
    pub fn new(probe: P, slippage_bps: u16, report_path: PathBuf, seq_seed: u64) -> Self {
        Self {
            probe,
            slippage_bps,
            report_path,
            seq: SeqCounter::new(seq_seed),
        }
    }
}

impl<P> Executor for DryRunExecutor<P>
where
    P: PositionProbe + Sync,
{
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let position = self.probe.position(order.market_id).await?;
        let mark = position
            .and_then(|position| position.mark_price)
            .ok_or_else(|| {
                PerplError::Order(format!("no mark price for market {}", order.market_id.0))
            })?;

        let slippage = Decimal::from(self.slippage_bps) / Decimal::from(BPS_DENOMINATOR);
        let fill_price = match order.close {
            CloseSide::CloseLong => mark * (Decimal::ONE - slippage),
            CloseSide::CloseShort => mark * (Decimal::ONE + slippage),
        };

        let report = ExecutionReport {
            order: order.clone(),
            status: ExecutionStatus::Simulated,
            filled_size: order.size,
            avg_price: Some(fill_price),
            tx_hash: None,
            client_order_id: client_order_id(order, self.seq.next()),
            detail: format!(
                "dry-run fill at {fill_price} (mark {mark}, {} bps slippage)",
                self.slippage_bps
            ),
            ts_ms: unix_ms(),
        };
        append_jsonl(&self.report_path, &report)?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use sentinel_core::order::OrderType;
    use sentinel_core::types::{MarketId, Position};

    use super::*;
    use crate::error::SentinelError;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).expect("valid decimal literal")
    }

    fn position(size: Decimal, mark: Option<Decimal>) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: d("100"),
            mark_price: mark,
            liq_price: None,
            collateral: d("50"),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: d("5"),
            opened_at: None,
        }
    }

    fn order(close: CloseSide) -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close,
            size: d("0.5"),
            order_type: OrderType::Market,
            max_slippage_bps: 10,
            size_decimals: 3,
        }
    }

    struct FixedProbe {
        position: Option<Position>,
    }

    impl PositionProbe for FixedProbe {
        async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
            Ok(self.position.clone())
        }
    }

    fn decimal_at(value: &serde_json::Value, pointer: &str) -> Decimal {
        serde_json::from_value(value.pointer(pointer).cloned().expect("field present"))
            .expect("decimal field")
    }

    #[tokio::test]
    async fn close_long_sells_below_mark_at_slippage() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nested path exercises parent-directory creation.
        let path = dir.path().join("nested/deep/reports.jsonl");
        let executor = DryRunExecutor::new(
            FixedProbe {
                position: Some(position(d("2"), Some(d("2713.70")))),
            },
            10,
            path.clone(),
            7,
        );

        let order = order(CloseSide::CloseLong);
        let report = executor.submit(&order).await.expect("simulated submit");

        assert_eq!(report.status, ExecutionStatus::Simulated);
        assert_eq!(report.filled_size, order.size, "full fill");
        // 2713.70 × (1 − 10/10_000) = 2710.98630 (Python `decimal` verified).
        assert_eq!(report.avg_price, Some(d("2710.98630")));
        assert_eq!(
            report.client_order_id, "sentinel-32-8",
            "seed 7 → first id 8"
        );
        assert_eq!(report.order, order);
        assert!(report.ts_ms > 0);
        assert!(
            report.detail.contains("dry-run"),
            "detail: {}",
            report.detail
        );

        let text = std::fs::read_to_string(&path).expect("report line written");
        assert_eq!(text.lines().count(), 1);
        let line: serde_json::Value = serde_json::from_str(text.trim()).expect("report JSON");
        assert_eq!(line["status"], "Simulated");
        assert_eq!(decimal_at(&line, "/avg_price"), d("2710.98630"));
        assert_eq!(decimal_at(&line, "/filled_size"), order.size);
        assert!(line["tx_hash"].is_null());
    }

    #[tokio::test]
    async fn close_short_buys_above_mark_at_slippage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = DryRunExecutor::new(
            FixedProbe {
                position: Some(position(d("2"), Some(d("2713.70")))),
            },
            10,
            dir.path().join("reports.jsonl"),
            7,
        );

        let order = order(CloseSide::CloseShort);
        let report = executor.submit(&order).await.expect("simulated submit");
        // 2713.70 × (1 + 10/10_000) = 2716.41370 (Python `decimal` verified).
        assert_eq!(report.avg_price, Some(d("2716.41370")));
        assert_eq!(report.status, ExecutionStatus::Simulated);
    }

    #[tokio::test]
    async fn missing_mark_is_an_order_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Position present but without a mark price.
        let executor = DryRunExecutor::new(
            FixedProbe {
                position: Some(position(d("2"), None)),
            },
            10,
            dir.path().join("a.jsonl"),
            7,
        );
        let err = executor
            .submit(&order(CloseSide::CloseLong))
            .await
            .expect_err("missing mark must fail");
        match err {
            SentinelError::Perpl(PerplError::Order(message)) => {
                assert_eq!(message, "no mark price for market 32");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // No position at all for the market.
        let executor = DryRunExecutor::new(
            FixedProbe { position: None },
            10,
            dir.path().join("b.jsonl"),
            7,
        );
        let err = executor
            .submit(&order(CloseSide::CloseLong))
            .await
            .expect_err("no position must fail");
        assert!(matches!(err, SentinelError::Perpl(PerplError::Order(_))));
        // Nothing was appended for the failed submissions.
        assert!(!dir.path().join("a.jsonl").exists());
        assert!(!dir.path().join("b.jsonl").exists());
    }

    #[tokio::test]
    async fn appends_one_line_per_submission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reports.jsonl");
        let executor = DryRunExecutor::new(
            FixedProbe {
                position: Some(position(d("2"), Some(d("2713.70")))),
            },
            10,
            path.clone(),
            7,
        );

        let first = executor
            .submit(&order(CloseSide::CloseLong))
            .await
            .expect("first");
        let second = executor
            .submit(&order(CloseSide::CloseShort))
            .await
            .expect("second");
        assert_eq!(first.client_order_id, "sentinel-32-8");
        assert_eq!(second.client_order_id, "sentinel-32-9");

        let text = std::fs::read_to_string(&path).expect("report lines written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "each submission appends exactly one line");
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("report JSON");
            assert_eq!(value["status"], "Simulated");
        }
    }
}
