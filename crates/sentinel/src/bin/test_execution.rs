//! `test_execution` — P05 acceptance proof: place a small reduce-only order
//! against the seeded position, in DRY_RUN (offline, synthetic fixture) and
//! TESTNET (gateway, needs a real API key — STUB-09 / P05 FALLBACK).
//!
//! DRY_RUN output: two submissions of the same 25 % reduce — the first is
//! simulated, the second is suppressed by the idempotency window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rust_decimal::Decimal;
use sentinel::config::Config;
use sentinel::error::SentinelError;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::idempotency::IdempotencyStore;
use sentinel::execution::{Executor, GuardedExecutor, PositionProbe};
use sentinel::perpl::{MockPerpl, PerplFeed};
use sentinel_core::order::reduce_by_fraction;
use sentinel_core::types::MarketId;
use tokio::sync::Mutex;

const ETH_TESTNET: MarketId = MarketId(32);

/// 25 % of the position size (acceptance value from the P05 prompt); a fn
/// because `Decimal::new` is not a const fn in this rust_decimal release.
fn reduce_fraction() -> Decimal {
    Decimal::new(25, 2)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load().context("load configuration")?;
    match cfg.execution.mode {
        sentinel_core::types::ExecutionMode::DryRun => run_dry(&cfg).await,
        sentinel_core::types::ExecutionMode::Testnet => run_testnet(&cfg).await,
        sentinel_core::types::ExecutionMode::Mainnet => {
            anyhow::bail!("mainnet mode is not used by test_execution")
        }
    }
}

/// Offline DRY_RUN proof against the synthetic account fixture.
async fn run_dry(cfg: &Config) -> anyhow::Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/perpl/synthetic-account.jsonl");
    let feed = MockPerpl::from_fixture(&fixture).context("load synthetic fixture")?;
    let probe = MockPerpl::from_fixture(&fixture).context("load probe fixture")?;

    let markets = feed.context().await.context("context")?;
    let market = markets
        .iter()
        .find(|m| m.id == ETH_TESTNET)
        .context("ETH market in fixture")?;
    let position = probe
        .position(ETH_TESTNET)
        .await
        .context("probe ETH")?
        .context("ETH position in fixture")?;

    let order = reduce_by_fraction(&position, reduce_fraction(), market, 50)
        .context("size the 25 % reduce (should be well above lot/min)")?;

    let store = Arc::new(Mutex::new(IdempotencyStore::load(
        Duration::from_secs(cfg.risk.idempotency_window_secs),
        PathBuf::from("data/test-execution-idempotency.json"),
    )?));
    let executor = GuardedExecutor::new(
        DryRunExecutor::new(probe, 10, PathBuf::from("data/dryrun-reports.jsonl"), 0),
        Arc::clone(&store),
        MockPerpl::from_fixture(&fixture).context("load guard probe fixture")?,
        Duration::from_secs(2),
    );

    println!(
        "DRY_RUN reduce: market {} size {} (position |{}|)",
        order.market_id.0, order.size, position.size
    );

    let first = executor.submit(&order).await;
    match &first {
        Ok(report) => println!(
            "submit #1: {:?} id={} filled={} avg={:?}",
            report.status, report.client_order_id, report.filled_size, report.avg_price
        ),
        Err(err) => println!("submit #1 error: {err}"),
    }

    let second = executor.submit(&order).await;
    match &second {
        Err(SentinelError::DuplicateOrder { window_secs, key }) => {
            println!("submit #2: suppressed as duplicate (window {window_secs}s, key {key})");
        }
        other => println!("submit #2: UNEXPECTED {other:?}"),
    }

    anyhow::ensure!(first.is_ok(), "first submission must succeed");
    anyhow::ensure!(
        matches!(second, Err(SentinelError::DuplicateOrder { .. })),
        "second submission must be suppressed"
    );
    println!("DRY_RUN acceptance OK (idempotency window suppresses the repeat)");
    Ok(())
}

/// Live TESTNET proof — requires a real API key and funded exchange account.
async fn run_testnet(cfg: &Config) -> anyhow::Result<()> {
    use sentinel::execution::perpl::PerplExecutor;
    use sentinel::perpl::LivePerpl;
    use sentinel::perpl::auth::ApiKeySigner;

    let feed = LivePerpl::new(cfg).context("build live feed")?;
    let markets = feed
        .context()
        .await
        .context("context (network/key required)")?;
    let market = markets
        .iter()
        .find(|m| m.id == ETH_TESTNET)
        .context("ETH market listed")?;
    let position = feed
        .position(ETH_TESTNET)
        .await
        .context("probe position (needs key + exchange account)")?
        .context("seeded ETH position present (docs/SETUP-MANUAL.md)")?;
    let account_id = feed
        .primary_account_id()
        .await
        .context("wallet account id (needs key)")?;

    let order = reduce_by_fraction(&position, reduce_fraction(), market, 50)
        .context("size the 25 % reduce")?;
    let signer = ApiKeySigner::from_config(&cfg.perpl).context("build signer")?;
    let executor = GuardedExecutor::new(
        PerplExecutor::new(
            cfg.perpl.api_url.clone(),
            signer,
            account_id,
            LivePerpl::new(cfg)?,
            0,
        )
        .context("build Perpl executor")?,
        Arc::new(Mutex::new(IdempotencyStore::load(
            Duration::from_secs(cfg.risk.idempotency_window_secs),
            PathBuf::from("data/test-execution-idempotency.json"),
        )?)),
        LivePerpl::new(cfg)?,
        Duration::from_secs(5),
    );

    println!(
        "TESTNET reduce: market {} size {} (position |{}|)",
        order.market_id.0, order.size, position.size
    );
    let report = executor
        .submit(&order)
        .await
        .context("submit reduce-only order")?;
    println!(
        "report: {:?} id={} detail={:?}",
        report.status, report.client_order_id, report.detail
    );
    match &report.tx_hash {
        Some(hash) => println!("explorer: https://testnet.monadexplorer.com/tx/{hash}"),
        None => println!(
            "forwarded by the exchange (gasless); verify the size decrease by re-reading the position \
             (explorer base: https://testnet.monadexplorer.com)"
        ),
    }
    Ok(())
}
