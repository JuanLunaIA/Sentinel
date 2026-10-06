//! Human-approved order execution (`SPEC-P11.md` §7).
//!
//! A concrete, guarded executor for the human path (the pipeline owns the
//! reflex path). Both variants share the same probe as the pipeline.
//!
//! **Status (P11):** implemented by agent `bot-handlers`.

use sentinel_core::order::OrderRequest;

use crate::error::Result;
use crate::execution::dry_run::DryRunExecutor;
use crate::execution::perpl::PerplExecutor;
use crate::execution::{ExecutionReport, Executor, GuardedExecutor};
use crate::pipeline::StateProbe;

/// The human path's executor (mode-picked by `main.rs`).
#[allow(clippy::large_enum_variant)] // both arms hold a guarded executor; boxing adds indirection for no gain
pub enum HumanExecutor {
    /// DRY_RUN mode (guarded simulation).
    Dry(GuardedExecutor<DryRunExecutor<StateProbe>, StateProbe>),
    /// TESTNET mode (guarded gateway orders).
    Perpl(GuardedExecutor<PerplExecutor<StateProbe>, StateProbe>),
}

impl Executor for HumanExecutor {
    /// Submit `order` through whichever guarded executor the mode selected.
    ///
    /// Both arms delegate to the inner [`GuardedExecutor`]: idempotency
    /// pre-check, inner submission, then post-fill verification against the
    /// shared position probe. Errors (validation, scope, duplicate
    /// suppression) surface unchanged.
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        match self {
            HumanExecutor::Dry(executor) => executor.submit(order).await,
            HumanExecutor::Perpl(executor) => executor.submit(order).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use rust_decimal::Decimal;
    use sentinel_core::order::{CloseSide, OrderType};
    use sentinel_core::types::{AccountState, Market, MarketId, Position};
    use tokio::sync::Mutex;

    use super::*;
    use crate::error::SentinelError;
    use crate::execution::ExecutionStatus;
    use crate::execution::idempotency::IdempotencyStore;
    use crate::perpl::{AccountEvent, FeedEvent};
    use crate::pipeline::LiveState;

    /// Decimal from a literal (test shorthand).
    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn utc(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).expect("valid test timestamp")
    }

    /// ETH-like market on market 32 (mmr 0.05, size grid 3 decimals).
    fn market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH Perp".to_string(),
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

    /// A 10-unit long at entry 2700.00 with a live mark of 2500.00.
    fn position() -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size: d("10"),
            entry_price: d("2700.00"),
            mark_price: Some(d("2500.00")),
            liq_price: None,
            collateral: d("13560"),
            unrealized_pnl: d("-2000.00"),
            margin_ratio: None,
            leverage: d("2"),
            opened_at: None,
        }
    }

    /// Shared live state with the fixture position installed.
    fn live_state() -> Arc<Mutex<LiveState>> {
        let mut live = LiveState::new();
        live.set_markets(vec![market()]);
        let _ = live.apply(&FeedEvent::Account(AccountEvent::Snapshot {
            state: AccountState {
                positions: vec![position()],
                free_balance: d("1000"),
                equity: d("1000"),
                fee_tier: 0,
                snapshot_ts: utc(1_700_000_000_000),
            },
        }));
        Arc::new(Mutex::new(live))
    }

    /// A half-close order on market 32, sized from the fixture position.
    fn order() -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close: CloseSide::CloseLong,
            size: d("0.5"),
            order_type: OrderType::Market,
            max_slippage_bps: 50,
            size_decimals: 3,
        }
    }

    /// A guarded DryRun executor over the shared fixture probe.
    fn dry_executor(
        state: &Arc<Mutex<LiveState>>,
        store_path: &Path,
        report_path: &Path,
    ) -> HumanExecutor {
        let store = IdempotencyStore::load(Duration::from_secs(60), store_path.to_path_buf())
            .expect("idempotency store");
        HumanExecutor::Dry(GuardedExecutor::new(
            DryRunExecutor::new(
                StateProbe::new(Arc::clone(state)),
                10,
                report_path.to_path_buf(),
                0,
            ),
            Arc::new(Mutex::new(store)),
            StateProbe::new(Arc::clone(state)),
            Duration::from_millis(30),
        ))
    }

    #[tokio::test]
    async fn dry_variant_submits_through_the_guard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = live_state();
        let report_path = dir.path().join("reports.jsonl");
        let executor = dry_executor(&state, &dir.path().join("idem.json"), &report_path);

        let report = executor.submit(&order()).await.expect("dry-run submit");
        assert_eq!(report.status, ExecutionStatus::Simulated);
        assert_eq!(report.filled_size, d("0.5"));
        assert_eq!(report.order, order());
        assert!(
            report.client_order_id.starts_with("sentinel-32-"),
            "client order id: {}",
            report.client_order_id
        );
        // 2500.00 × (1 − 10/10_000) = 2497.50.
        assert_eq!(report.avg_price, Some(d("2497.50000")));

        let written = std::fs::read_to_string(&report_path).expect("report line");
        assert_eq!(written.lines().count(), 1);
        assert!(written.contains("Simulated"));
    }

    #[tokio::test]
    async fn dry_variant_surfaces_duplicate_suppression_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = live_state();
        let executor = dry_executor(
            &state,
            &dir.path().join("idem.json"),
            &dir.path().join("reports.jsonl"),
        );

        executor.submit(&order()).await.expect("first submission");
        let error = executor
            .submit(&order())
            .await
            .expect_err("the guard suppresses the duplicate");
        assert!(matches!(error, SentinelError::DuplicateOrder { .. }));
    }
}
