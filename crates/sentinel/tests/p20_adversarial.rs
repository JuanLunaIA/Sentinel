//! P20 adversarial verification — verifier-owned (SPEC-P20 §5), black box over
//! the landed P20 public surface.
//!
//! Every expectation below is derived from `SPEC-P20.md` and the frozen
//! interfaces it consumes (`brain::engine`, the policy gate, the audit chain,
//! the notify kinds); the writers' implementation bodies are never inspected.
//!
//! Coverage map (test -> SPEC-P20 clause):
//! - §0/§3 engine absent ⇒ zero consult activity: `engine_none_*`,
//!   `pipeline_engine_none_*`;
//! - §2a Yellow entry / §2 rate limit (suppression inside, re-admission past
//!   the window): `yellow_entry_*`;
//! - §2 budget breach ⇒ synthetic ESCALATE + alert, no order: `budget_*`;
//! - §2.4 Allow ⇒ `ApprovedOrder` (reduce-only, quantized; long + short legs):
//!   `allowed_reduce_*`, `approved_orders_*`;
//! - §2.4 NeedsApproval ⇒ alert, no order: `needs_approval_*`;
//! - §2.4 Deny (kill switch) ⇒ alert, no order: `kill_switch_*`;
//! - §2.3 STRATEGY intent+outcome pair chain-verifiable: `journal_*`;
//! - §2.2 SM unavailable without a Nansen key: `sm_unavailable_*`;
//! - §2b post-reflex review: `post_reflex_*`; §2c periodic review:
//!   `periodic_*` (+ `DEFAULT_REVIEW_INTERVAL_SECS` pin);
//! - §4 judge-demo static contract: `judge_demo_script_*`.
//!
//! Regressions re-run separately (reported in the hand-off, not here):
//!   cargo test -p sentinel --test pipeline_determinism
//!   cargo test -p sentinel --test p11_adversarial

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel::bot::policy_admin::SharedPolicy;
use sentinel::brain::chain::NoProvider;
use sentinel::brain::engine::StrategyEngine;
use sentinel::brain::providers::{MockProvider, Provider, RawCompletion};
use sentinel::config::Config;
use sentinel::consult::{ApprovedOrder, ConsultTask, ConsultTrigger, DEFAULT_REVIEW_INTERVAL_SECS};
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::health::HealthState;
use sentinel::notify::{Alert, AlertKind, RecordingSink};
use sentinel::perpl::MockPerpl;
use sentinel::pipeline::{LiveState, Pipeline, PipelineEvent, RunMode, StateProbe};
use sentinel::reflex::REFLEX_SLIPPAGE_BPS;
use sentinel_core::audit::{AuditEntry, AuditJournal, Trigger, verify_chain};
use sentinel_core::order::{CloseSide, OrderType};
use sentinel_core::types::{AccountState, Market, MarketId, Position, RiskTier};
use tokio::sync::{Mutex, mpsc, watch};

// ---------------------------------------------------------------------------
// Fixtures and shared helpers.
// ---------------------------------------------------------------------------

/// Independent logical clock every consult call uses (epoch ms).
const NOW_MS: u64 = 1_792_000_000_000;

/// A syntactically valid perpl API secret (config validation only).
const SECRET: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Decimal literal shorthand (tests only).
fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal literal")
}

/// ETH-like market (P04 maintenance margin 0.05); any `(id, symbol)` pair.
fn market(id: u32, symbol: &str) -> Market {
    Market {
        id: MarketId(id),
        symbol: symbol.to_string(),
        base: symbol.to_string(),
        price_decimals: 2,
        size_decimals: 3,
        initial_margin_fraction: dec("0.0833"),
        maintenance_margin_fraction: dec("0.05"),
        max_leverage: dec("12"),
        min_size: dec("0.001"),
        tick_size: dec("0.01"),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

/// Position fixture: long, given entry/mark/collateral.
fn position_for(
    market_id: u32,
    symbol: &str,
    size: &str,
    entry: &str,
    mark: &str,
    collateral: &str,
) -> Position {
    Position {
        market_id: MarketId(market_id),
        symbol: symbol.to_string(),
        size: dec(size),
        entry_price: dec(entry),
        mark_price: Some(dec(mark)),
        liq_price: None,
        collateral: dec(collateral),
        unrealized_pnl: Decimal::ZERO,
        margin_ratio: None,
        leverage: dec("10"),
        opened_at: None,
    }
}

/// Market-32 ETH long, size +5.000 at entry 2000.00, collateral 4000
/// (derived liq 1300.00): mark 1700 ⇒ 23.529 % (Yellow), 1900 ⇒ 31.579 %
/// (Green), 1500 ⇒ 13.333 % (Orange), 1380 ⇒ 5.797 % (Red).
fn eth_at(mark: &str) -> Position {
    position_for(32, "ETH", "5", "2000", mark, "4000")
}

/// Market-20 BTC long, size +2.000 at entry 30000.00, collateral 20000.
fn btc_at(mark: &str) -> Position {
    position_for(20, "BTC", "2", "30000", mark, "45000")
}

/// Market-26 SOL long, size +10.000 at entry 150.00, collateral 2000.
fn sol_at(mark: &str) -> Position {
    position_for(26, "SOL", "10", "150", mark, "2000")
}

/// Live state the consult task reads: markets, account, marks, clock.
fn state_with(positions: Vec<Position>, marks: &[(u32, &str)]) -> LiveState {
    let mut state = LiveState::new();
    state.markets = vec![market(32, "ETH"), market(20, "BTC"), market(26, "SOL")];
    state.account = Some(AccountState {
        positions,
        free_balance: dec("10000"),
        equity: dec("25000"),
        fee_tier: 0,
        snapshot_ts: DateTime::<Utc>::from_timestamp_millis(NOW_MS as i64).expect("timestamp"),
    });
    state.marks = marks
        .iter()
        .map(|(id, mark)| (MarketId(*id), dec(mark)))
        .collect();
    state.base_balance = dec("25000");
    state.now_ms = NOW_MS;
    state
}

/// Config built from environment-style vars (the frozen `Config::from_vars`
/// surface P11/P06 tests use), with per-test overrides.
fn cfg_with(overrides: &[(&str, &str)]) -> Config {
    let base: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", SECRET),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("EXECUTION_MODE", "DRY_RUN"),
        ("MARKET_ALLOWLIST", "32,20,26"),
        ("MAX_ORDER_SIZE_USD", "100000"),
        ("REQUIRE_APPROVAL_ABOVE_USD", "100000"),
        ("STRATEGY_MIN_INTERVAL_SECS", "300"),
        ("STRATEGY_CONFIDENCE_FLOOR", "0.5"),
    ];
    let mut vars: HashMap<String, String> = base
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    for (key, value) in overrides {
        vars.insert((*key).to_string(), (*value).to_string());
    }
    Config::from_vars(vars).expect("test config must load")
}

/// A compliant HOLD decision on `market_id` (validated by the engine parser).
fn hold_json(market_id: u32, confidence: &str) -> String {
    format!(
        r#"{{"action":"HOLD","market_id":{market_id},"amount":null,"confidence":{confidence},"urgency":"ROUTINE","reason":"grounded in the snapshot"}}"#
    )
}

/// A REDUCE decision on `market_id`; `amount` is in base units (P07 schema).
fn reduce_json(market_id: u32, amount: &str, confidence: &str) -> String {
    format!(
        r#"{{"action":"REDUCE","market_id":{market_id},"amount":{amount},"confidence":{confidence},"urgency":"ELEVATED","reason":"trim the position"}}"#
    )
}

/// `Provider` shim around a shared [`MockProvider`] so tests keep `calls()`.
struct SharedMock(Arc<MockProvider>);

impl Provider for SharedMock {
    async fn complete(&self, system: &str, user: &str) -> sentinel::error::Result<RawCompletion> {
        self.0.complete(system, user).await
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }
}

/// Mock-backed engine with the given per-market interval and floor.
fn engine(
    mock: &Arc<MockProvider>,
    min_interval_secs: u64,
    floor: &str,
) -> StrategyEngine<SharedMock, NoProvider> {
    StrategyEngine::new(SharedMock(Arc::clone(mock)), min_interval_secs, dec(floor))
}

/// A wired, not-yet-running consult task plus every observation handle.
struct Rig {
    task: ConsultTask<SharedMock, NoProvider, RecordingSink>,
    trigger_tx: mpsc::Sender<ConsultTrigger>,
    orders_rx: mpsc::Receiver<ApprovedOrder>,
    alerts: Arc<std::sync::Mutex<Vec<Alert>>>,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    journal_dir: PathBuf,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    _dir: tempfile::TempDir,
}

/// Build the rig over a tempdir (policy overlay + optional journal).
fn make_rig(
    engine: StrategyEngine<SharedMock, NoProvider>,
    cfg: Config,
    state: LiveState,
    with_journal: bool,
    kill_switch: bool,
    review_interval_secs: u64,
) -> Rig {
    make_rig_shared(
        engine,
        cfg,
        Arc::new(Mutex::new(state)),
        with_journal,
        kill_switch,
        review_interval_secs,
    )
}

/// As [`make_rig`], over a shared live-state handle (clock-bump tests).
fn make_rig_shared(
    engine: StrategyEngine<SharedMock, NoProvider>,
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    with_journal: bool,
    kill_switch: bool,
    review_interval_secs: u64,
) -> Rig {
    let dir = tempfile::tempdir().expect("rig tempdir");
    let policy_path = dir.path().join("policy.json");
    if kill_switch {
        std::fs::write(&policy_path, r#"{"kill_switch":true}"#).expect("policy overlay write");
    }
    let journal = with_journal.then(|| {
        Arc::new(Mutex::new(
            AuditJournal::open(dir.path().join("journal")).expect("journal opens"),
        ))
    });
    let journal_dir = dir.path().join("journal");
    let (trigger_tx, trigger_rx) = mpsc::channel::<ConsultTrigger>(8);
    let (orders_tx, orders_rx) = mpsc::channel::<ApprovedOrder>(8);
    let sink = RecordingSink::new();
    let alerts = Arc::clone(&sink.alerts);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = ConsultTask::new(
        Some(Arc::new(engine)),
        cfg,
        state,
        journal.clone(),
        Arc::new(SharedPolicy::load(&policy_path)),
        Arc::new(Mutex::new(trigger_rx)),
        orders_tx,
        sink,
        None,
        review_interval_secs,
    );
    Rig {
        task,
        trigger_tx,
        orders_rx,
        alerts,
        journal,
        journal_dir,
        shutdown_tx,
        shutdown_rx,
        _dir: dir,
    }
}

/// Poll `cond` every 10 ms until true or `deadline_ms` elapsed.
async fn wait_until(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Flip shutdown and require the consult loop to stop within 3 s.
async fn shutdown(tx: &watch::Sender<bool>, handle: tokio::task::JoinHandle<()>) {
    let _ = tx.send(true);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .is_ok(),
        "consult task must stop on shutdown"
    );
}

/// Alerts snapshot.
fn alerts_now(alerts: &Arc<std::sync::Mutex<Vec<Alert>>>) -> Vec<Alert> {
    alerts.lock().expect("alerts lock").clone()
}

/// Journal files (day files) present in `dir`.
fn journal_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().starts_with("journal-"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// STRATEGY entries currently visible through the journal handle.
async fn strategy_entries(journal: &Arc<Mutex<AuditJournal>>) -> Vec<AuditEntry> {
    journal
        .lock()
        .await
        .read_entries(0, 200)
        .expect("journal read")
        .into_iter()
        .filter(|entry| entry.trigger == Trigger::Strategy)
        .collect()
}

/// Outcome entries that carry a decision: neither a pending intent nor a
/// refused/error attempt (a rate-limited repeat may journal as an error).
fn decided_outcomes(entries: &[AuditEntry]) -> Vec<&AuditEntry> {
    entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.execution["status"].as_str(),
                Some(status) if status != "pending" && status != "error"
            )
        })
        .collect()
}

/// Repo root from the crate manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// P20 replay fixture (Green -> Yellow entry only).
fn p20_fixture() -> PathBuf {
    repo_root().join("tests/fixtures/p20/yellow-entry.jsonl")
}

/// Pin MockPerpl replay pacing for this binary (fixture time compresses to 0).
fn fast_pacing() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `set_var` is unsafe in Rust 2024; `Once` serialises the
        // single write before any feed is built.
        unsafe {
            std::env::set_var("SENTINEL_MOCK_PACE", "100000");
        }
    });
}

// ---------------------------------------------------------------------------
// §0/§3 — engine absent ⇒ zero consult activity.
// ---------------------------------------------------------------------------

/// An engine-less task must be inert: it returns immediately, consumes no
/// queued trigger, emits no order and no alert (SPEC-P20 §0/§3).
#[tokio::test]
async fn engine_none_consult_task_is_inert_and_consumes_nothing() {
    let (trigger_tx, trigger_rx) = mpsc::channel::<ConsultTrigger>(4);
    let triggers = Arc::new(Mutex::new(trigger_rx));
    let (orders_tx, mut orders_rx) = mpsc::channel::<ApprovedOrder>(4);
    let sink = RecordingSink::new();
    let alerts = Arc::clone(&sink.alerts);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let dir = tempfile::tempdir().expect("dir");
    let task = ConsultTask::<SharedMock, NoProvider, RecordingSink>::new(
        None,
        cfg_with(&[]),
        Arc::new(Mutex::new(state_with(
            vec![eth_at("1700")],
            &[(32, "1700")],
        ))),
        None,
        Arc::new(SharedPolicy::load(dir.path().join("policy.json"))),
        Arc::clone(&triggers),
        orders_tx,
        sink,
        None,
        1,
    );

    // A trigger is queued BEFORE the task starts: an inert task must not run it.
    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("queue trigger");
    let handle = tokio::spawn(task.run(shutdown_rx));
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("engine-None task must return immediately")
        .expect("no panic");

    let mut guard = triggers.lock().await;
    assert!(
        guard.try_recv().is_ok(),
        "the queued trigger must still be pending (nothing consumed)"
    );
    drop(guard);
    assert!(orders_rx.try_recv().is_err(), "no orders without an engine");
    assert!(
        alerts_now(&alerts).is_empty(),
        "no alerts without an engine"
    );
    let _ = shutdown_tx.send(true);
}

// ---------------------------------------------------------------------------
// §2a — Yellow entry, exactly one consult per market and per window.
// ---------------------------------------------------------------------------

/// Two YellowEntry triggers for market 32 inside `STRATEGY_MIN_INTERVAL_SECS`
/// must produce exactly one consult; markets 20/26 are independent and each
/// fire once. Trailing firms act as FIFO sentinels proving the duplicate was
/// processed and suppressed (SPEC-P20 §2/§5).
#[tokio::test]
async fn yellow_entry_fires_exactly_one_consult_per_market_and_window() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![
            hold_json(32, "0.72"),
            hold_json(20, "0.72"),
            hold_json(26, "0.72"),
        ],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(
            vec![eth_at("1700"), btc_at("30000"), sol_at("150")],
            &[(32, "1700"), (20, "30000"), (26, "150")],
        ),
        true,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        alerts,
        journal,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let journal = journal.expect("journal attached");
    let handle = tokio::spawn(task.run(shutdown_rx));

    for market_id in [32u32, 32, 20, 26] {
        trigger_tx
            .send(ConsultTrigger::YellowEntry {
                market_id: MarketId(market_id),
            })
            .await
            .expect("trigger");
    }

    assert!(
        wait_until(3_000, || mock.calls().len() == 3).await,
        "expected exactly 3 consults, saw {}",
        mock.calls().len()
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    let calls = mock.calls();
    assert_eq!(
        calls.len(),
        3,
        "a second YellowEntry inside STRATEGY_MIN_INTERVAL_SECS must be suppressed"
    );
    assert!(
        calls[0].1.contains(">> ETH | #32"),
        "first consult focuses market 32"
    );
    assert!(
        calls[1].1.contains(">> BTC | #20"),
        "market 20 is not blocked by market 32's window"
    );
    assert!(
        calls[2].1.contains(">> SOL | #26"),
        "market 26 is not blocked either"
    );
    assert!(
        orders_rx.try_recv().is_err(),
        "HOLD consults must never emit an ApprovedOrder"
    );

    // Every consulted market here is at most Yellow, so §2.5 emits no
    // strategy-decision alerts for them — and a suppressed repeat is not a
    // consult at all.
    let alert_kinds = alerts_now(&alerts);
    assert_eq!(
        alert_kinds
            .iter()
            .filter(|alert| matches!(&alert.kind, AlertKind::StrategyDecision { .. }))
            .count(),
        0,
        "no strategy alerts for Yellow/Green consulted decisions: {alert_kinds:?}"
    );

    // A suppressed repeat is not a consulted decision: the provider logged
    // exactly three calls, so exactly three decision outcomes (one per
    // market) must journal. A refused attempt may journal as an error-status
    // pair (audit-before-action) but can never carry a decision.
    let mut entries = strategy_entries(&journal).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2_000);
    while decided_outcomes(&entries).len() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        entries = strategy_entries(&journal).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let entries = strategy_entries(&journal).await;
    let decided = decided_outcomes(&entries);
    assert_eq!(
        decided.len(),
        3,
        "exactly three consulted decisions journaled: {entries:?}"
    );
    assert_eq!(
        decided
            .iter()
            .filter(|entry| entry.market_id == Some(32))
            .count(),
        1,
        "market 32's suppressed repeat produced no decision outcome"
    );
    assert_eq!(
        decided
            .iter()
            .filter(|entry| entry.market_id == Some(20))
            .count(),
        1,
        "market 20 decided once"
    );
    assert_eq!(
        decided
            .iter()
            .filter(|entry| entry.market_id == Some(26))
            .count(),
        1,
        "market 26 decided once"
    );
    shutdown(&shutdown_tx, handle).await;
}

/// The window is only a window: inside `STRATEGY_MIN_INTERVAL_SECS` repeats
/// are refused, and once the interval elapses the next YellowEntry admits
/// again (SPEC-P20 §2). The interval is shortened to 1 s here; the wall-clock
/// window is exercised with real sleeps.
#[tokio::test]
async fn yellow_entry_suppresses_inside_and_readmits_after_the_window() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![hold_json(32, "0.72"), hold_json(32, "0.72")],
    ));
    let state = Arc::new(Mutex::new(state_with(
        vec![eth_at("1700")],
        &[(32, "1700")],
    )));
    let rig = make_rig_shared(
        engine(&mock, 1, "0.5"),
        cfg_with(&[("STRATEGY_MIN_INTERVAL_SECS", "1")]),
        Arc::clone(&state),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("first trigger");
    assert!(
        wait_until(2_000, || mock.calls().len() == 1).await,
        "the first consult is admitted"
    );

    // Inside the 1 s window: the repeat is refused.
    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("duplicate trigger");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mock.calls().len(),
        1,
        "inside the window the repeat is refused"
    );

    // Past the window: the next trigger is admitted again.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("post-window trigger");
    assert!(
        wait_until(3_000, || mock.calls().len() == 2).await,
        "past the interval the consult must be admitted again, saw {}",
        mock.calls().len()
    );
    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2 — budget breach ⇒ synthetic ESCALATE + alert, no order.
// ---------------------------------------------------------------------------

/// With the hourly budget lowered via the engine seam, the over-budget consult
/// returns the engine's synthetic ESCALATE: the task alerts it, never emits an
/// order, and never calls the provider (SPEC-P20 §2, SPEC-P08 §3).
#[tokio::test]
async fn budget_breach_escalates_with_alert_and_no_order() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![hold_json(32, "0.72")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5").with_budget(1, 200_000),
        cfg_with(&[]),
        state_with(
            vec![eth_at("1700"), btc_at("30000")],
            &[(32, "1700"), (20, "30000")],
        ),
        true,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        alerts,
        journal,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("first trigger");
    assert!(
        wait_until(2_000, || mock.calls().len() == 1).await,
        "the first consult is admitted"
    );

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(20),
        })
        .await
        .expect("second trigger");
    assert!(
        wait_until(2_000, || alerts_now(&alerts).iter().any(|alert| matches!(
            &alert.kind,
            AlertKind::StrategyDecision { action, .. } if action == "ESCALATE"
        )))
        .await,
        "budget breach must alert an ESCALATE strategy decision: {:?}",
        alerts_now(&alerts)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        mock.calls().len(),
        1,
        "the synthetic outcome must not reach the provider"
    );
    assert!(
        orders_rx.try_recv().is_err(),
        "a budget ESCALATE must never emit an ApprovedOrder"
    );

    // The synthetic consult is still a consulted decision: STRATEGY pair.
    let journal = journal.expect("journal attached");
    let mut entries = strategy_entries(&journal).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_500);
    while entries.len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        entries = strategy_entries(&journal).await;
    }
    assert!(
        entries.len() >= 2,
        "the budget consult must journal a STRATEGY intent+outcome pair: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.market_id == Some(20) && entry.execution["status"] != "pending"),
        "a market-20 outcome entry must exist after the budget breach: {entries:?}"
    );

    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2.4 — Allow ⇒ ApprovedOrder (reduce-only, quantized).
// ---------------------------------------------------------------------------

/// An allowed reduce decision must reach the pipeline as a reduce-only
/// `ApprovedOrder` sized by truncation to the lot grid (SPEC-P20 §2.4,
/// SPEC-P05 §4): 1.7777 base units → 1.777 at size_decimals 3.
#[tokio::test]
async fn allowed_reduce_sends_reduce_only_quantized_approved_order() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "1.7777", "0.8")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        // Orange tier (13.33 % at mark 1500): §2.5 alerts every consulted
        // decision in Orange+, so the allow outcome must alert too.
        state_with(vec![eth_at("1500")], &[(32, "1500")]),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        alerts,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");

    let approved = tokio::time::timeout(Duration::from_secs(2), orders_rx.recv())
        .await
        .expect("an allowed reduce must emit an ApprovedOrder")
        .expect("orders channel open");
    assert!(
        approved.decision_id.starts_with("s-"),
        "decision id {} must carry the s-<n> prefix",
        approved.decision_id
    );
    let order = approved.order;
    assert_eq!(order.market_id, MarketId(32));
    assert_eq!(
        order.close,
        CloseSide::CloseLong,
        "a long reduce closes the long leg"
    );
    assert_eq!(order.order_type, OrderType::Market);
    assert_eq!(order.size, dec("1.777"), "1.7777 truncates DOWN to 1.777");
    assert_eq!(order.size_decimals, 3);
    assert_eq!(order.max_slippage_bps, REFLEX_SLIPPAGE_BPS);
    assert!(
        orders_rx.try_recv().is_err(),
        "exactly one order per consult"
    );
    assert!(
        alerts_now(&alerts).iter().any(|alert| matches!(
            &alert.kind,
            AlertKind::StrategyDecision { status, .. } if status.starts_with("allow")
        )),
        "the approved consult must alert its status: {:?}",
        alerts_now(&alerts)
    );
    shutdown(&shutdown_tx, handle).await;
}

/// The same allowed reduce on a SHORT position closes the short leg
/// (SPEC-P20 §2.4 via the frozen reduce-only sizing, SPEC-P05 §4).
#[tokio::test]
async fn allowed_reduce_on_a_short_closes_the_short_leg() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "1.7777", "0.8")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(
            vec![position_for(32, "ETH", "-5", "2000", "1500", "4000")],
            &[(32, "1500")],
        ),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");
    let approved = tokio::time::timeout(Duration::from_secs(2), orders_rx.recv())
        .await
        .expect("a short reduce must emit an ApprovedOrder")
        .expect("orders channel open");
    assert_eq!(
        approved.order.close,
        CloseSide::CloseShort,
        "a short reduce closes the short leg"
    );
    assert_eq!(approved.order.size, dec("1.777"));
    assert_eq!(approved.order.market_id, MarketId(32));
    shutdown(&shutdown_tx, handle).await;
}

/// Two allowed consults produce distinct decision ids (correlation contract).
#[tokio::test]
async fn approved_orders_carry_distinct_decision_ids() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "1.0", "0.8"), reduce_json(20, "0.5", "0.8")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(
            vec![eth_at("1900"), btc_at("30000")],
            &[(32, "1900"), (20, "30000")],
        ),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger 32");
    let first = tokio::time::timeout(Duration::from_secs(2), orders_rx.recv())
        .await
        .expect("first order")
        .expect("channel open");
    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(20),
        })
        .await
        .expect("trigger 20");
    let second = tokio::time::timeout(Duration::from_secs(2), orders_rx.recv())
        .await
        .expect("second order")
        .expect("channel open");

    assert_ne!(
        first.decision_id, second.decision_id,
        "consult decision ids must correlate uniquely"
    );
    assert_eq!(first.order.market_id, MarketId(32));
    assert_eq!(second.order.market_id, MarketId(20));
    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2.4 — NeedsApproval ⇒ alert, no order; kill-switch deny ⇒ alert, no order.
// ---------------------------------------------------------------------------

/// A reduce above `REQUIRE_APPROVAL_ABOVE_USD` must journal + alert without a
/// keyboard from the daemon path and must NOT emit an ApprovedOrder
/// (SPEC-P20 §2.4). Notional: 1.7 × 1900 = 3230 > 2500 threshold.
#[tokio::test]
async fn needs_approval_alerts_without_order() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "1.7", "0.8")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[("REQUIRE_APPROVAL_ABOVE_USD", "2500")]),
        state_with(vec![eth_at("1900")], &[(32, "1900")]),
        true,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        alerts,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");
    assert!(
        wait_until(2_000, || alerts_now(&alerts).iter().any(|alert| matches!(
            &alert.kind,
            AlertKind::StrategyDecision { status, .. } if status.contains("needs_approval")
        )))
        .await,
        "needs-approval must alert: {:?}",
        alerts_now(&alerts)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        orders_rx.try_recv().is_err(),
        "needs-approval must not emit an ApprovedOrder"
    );
    shutdown(&shutdown_tx, handle).await;
}

/// With the kill switch engaged every consult is policy-denied: alert + no
/// order (SPEC-P20 §2.4 Deny branch).
#[tokio::test]
async fn kill_switch_denies_every_consult_without_order() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "0.5", "0.9")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![eth_at("1900")], &[(32, "1900")]),
        false,
        true,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        alerts,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");
    assert!(
        wait_until(2_000, || alerts_now(&alerts).iter().any(|alert| matches!(
            &alert.kind,
            AlertKind::StrategyDecision { status, .. } if status.starts_with("deny")
        )))
        .await,
        "a denied consult must alert the deny: {:?}",
        alerts_now(&alerts)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        orders_rx.try_recv().is_err(),
        "a denied consult must never emit an ApprovedOrder"
    );
    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2.3 — STRATEGY journal intent+outcome pair, chain-verifiable.
// ---------------------------------------------------------------------------

/// A consulted decision journals a STRATEGY intent before the action and its
/// outcome after; the pair sits on a chain `verify_chain` accepts
/// (SPEC-P20 §2.3, SPEC-P10 §2).
#[tokio::test]
async fn journal_strategy_pair_is_chain_verifiable() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![reduce_json(32, "1.0", "0.8")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![eth_at("1900")], &[(32, "1900")]),
        true,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        mut orders_rx,
        journal,
        journal_dir,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let journal = journal.expect("journal attached");
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), orders_rx.recv())
            .await
            .expect("an order proves the decision was consulted")
            .is_some(),
        "orders channel stays open"
    );

    let mut entries = strategy_entries(&journal).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2_000);
    while entries.len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        entries = strategy_entries(&journal).await;
    }
    assert!(
        entries.len() >= 2,
        "intent+outcome pair must land: {entries:?}"
    );

    let intent = &entries[0];
    let outcome = &entries[1];
    assert_eq!(intent.trigger, Trigger::Strategy);
    assert_eq!(intent.market_id, Some(32));
    assert_eq!(intent.execution["status"], "pending", "audit-before-action");
    assert_eq!(
        intent.seq + 1,
        outcome.seq,
        "the outcome directly follows its intent"
    );
    assert_eq!(outcome.market_id, Some(32));
    assert_ne!(
        outcome.execution["status"], "pending",
        "the outcome must carry the execution status"
    );

    let files = journal_files(&journal_dir);
    assert!(
        !files.is_empty(),
        "a day file must exist in {journal_dir:?}"
    );
    for file in files {
        let report = verify_chain(&file).expect("chain readable");
        assert_eq!(
            report.broken_at, None,
            "chain broken in {file:?}: {:?}",
            report.detail
        );
        assert!(
            report.detail.is_none()
                || report
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("torn")),
            "unexpected chain detail: {:?}",
            report.detail
        );
        assert!(report.entries >= 2, "both entries must be on disk");
    }

    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2.2 — SM unavailable without a Nansen key.
// ---------------------------------------------------------------------------

/// With no Nansen client the consult still runs end-to-end and the prompt
/// carries the frozen unavailable line (SPEC-P20 §2.2, SPEC-P07 §5).
#[tokio::test]
async fn sm_unavailable_without_nansen_still_consults() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![hold_json(32, "0.72")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![eth_at("1700")], &[(32, "1700")]),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::YellowEntry {
            market_id: MarketId(32),
        })
        .await
        .expect("trigger");
    assert!(
        wait_until(2_000, || mock.calls().len() == 1).await,
        "the consult must complete without smart-money data"
    );
    let prompt = &mock.calls()[0].1;
    assert!(
        prompt.contains("- smart-money data unavailable — decide without it"),
        "the frozen unavailable line must be rendered"
    );
    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §2b/§2c — post-reflex review and periodic portfolio review triggers.
// ---------------------------------------------------------------------------

/// A post-reflex Orange reduce review consults the market it targeted
/// (SPEC-P20 §2b).
#[tokio::test]
async fn post_reflex_reduce_trigger_fires_a_consult() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![hold_json(20, "0.72")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![btc_at("20000")], &[(20, "20000")]),
        false,
        false,
        3600,
    );
    let Rig {
        task,
        trigger_tx,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    trigger_tx
        .send(ConsultTrigger::PostReflexReduce {
            market_id: MarketId(20),
            tier: RiskTier::Orange,
        })
        .await
        .expect("trigger");
    assert!(
        wait_until(2_000, || mock.calls().len() == 1).await,
        "the post-reflex review must consult"
    );
    assert!(
        mock.calls()[0].1.contains(">> BTC | #20"),
        "the review focuses the reduced market"
    );
    shutdown(&shutdown_tx, handle).await;
}

/// The periodic review cadence default is the frozen 1800 s (SPEC-P20 §2c).
#[test]
fn review_interval_default_is_frozen() {
    assert_eq!(DEFAULT_REVIEW_INTERVAL_SECS, 1800);
}

/// While every position is Green, periodic ticks must not consult
/// (SPEC-P20 §2c: review runs "while any position >= Yellow").
#[tokio::test]
async fn periodic_review_skips_a_fully_green_portfolio() {
    let mock = Arc::new(MockProvider::canned_named("mock", vec![]));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![eth_at("2900")], &[(32, "2900")]),
        false,
        false,
        1,
    );
    let Rig {
        task,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    // Two full review intervals with a Green-only portfolio.
    tokio::time::sleep(Duration::from_millis(2_400)).await;
    assert_eq!(
        mock.calls().len(),
        0,
        "no consult may fire while every position is Green"
    );
    shutdown(&shutdown_tx, handle).await;
}

/// A Yellow position makes the next periodic tick consult it (SPEC-P20 §2c).
#[tokio::test]
async fn periodic_review_consults_while_yellow() {
    let mock = Arc::new(MockProvider::canned_named(
        "mock",
        vec![hold_json(32, "0.72")],
    ));
    let rig = make_rig(
        engine(&mock, 300, "0.5"),
        cfg_with(&[]),
        state_with(vec![eth_at("1700")], &[(32, "1700")]),
        false,
        false,
        1,
    );
    let Rig {
        task,
        shutdown_tx,
        shutdown_rx,
        ..
    } = rig;
    let handle = tokio::spawn(task.run(shutdown_rx));

    assert!(
        wait_until(4_000, || !mock.calls().is_empty()).await,
        "a Yellow position must be reviewed on the periodic tick"
    );
    assert!(
        mock.calls()[0].1.contains(">> ETH | #32"),
        "the periodic review focuses the Yellow market"
    );
    shutdown(&shutdown_tx, handle).await;
}

// ---------------------------------------------------------------------------
// §0/§3 — engine-None pipeline keeps the legacy path, byte-identical.
// ---------------------------------------------------------------------------

/// Pipeline replay with no strategy receiver (default and explicit `None`)
/// must stay byte-identical to itself, keep the legacy `consult_scheduled`
/// announcement for Yellow entries, and never show strategy activity
/// (SPEC-P20 §0/§3, §5).
#[tokio::test]
async fn pipeline_engine_none_keeps_legacy_path_byte_identical() {
    fast_pacing();
    let (jsonl_a, alerts_a, outcome_a, strategy_a) = run_pipeline_once(false).await;
    let (jsonl_b, alerts_b, _, strategy_b) = run_pipeline_once(true).await;

    assert_eq!(
        jsonl_a, jsonl_b,
        "an explicit None receiver must not change the replay"
    );
    assert_eq!(alerts_a, alerts_b, "alerts must be identical too");

    // Legacy Yellow-entry behavior is preserved.
    assert!(
        outcome_a.events.iter().any(|event| matches!(
            event,
            PipelineEvent::ConsultScheduled {
                market_id: 32,
                tier: RiskTier::Yellow,
                ..
            }
        )),
        "Yellow entry must keep scheduling the consult: {:?}",
        outcome_a.events
    );
    assert!(
        alerts_a.iter().any(|alert| matches!(
            &alert.kind,
            AlertKind::ConsultScheduled {
                tier: RiskTier::Yellow
            }
        )),
        "the consult_scheduled alert must stay: {alerts_a:?}"
    );

    // No new event kinds beyond the frozen legacy vocabulary.
    let legacy = [
        "started",
        "tier_changed",
        "consult_scheduled",
        "decision",
        "executed",
        "submit_failed",
        "duplicate_suppressed",
        "alert",
        "feed_stale",
        "reconnected",
        "shutdown",
    ];
    let lines: Vec<serde_json::Value> = jsonl_a
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    for line in &lines {
        let kind = line["event"].as_str().expect("event kind");
        assert!(
            legacy.contains(&kind),
            "engine-None replay must not introduce the new event kind {kind:?}"
        );
    }

    // Zero strategy activity anywhere: no strategy alerts, no STRATEGY
    // journal entries, no s-prefixed decision events.
    assert!(
        alerts_a
            .iter()
            .all(|alert| !matches!(&alert.kind, AlertKind::StrategyDecision { .. })),
        "no strategy alerts without an engine: {alerts_a:?}"
    );
    assert!(
        strategy_a.is_empty(),
        "no STRATEGY journal entries without an engine: {strategy_a:?}"
    );
    assert!(
        strategy_b.is_empty(),
        "no STRATEGY journal entries on the explicit None path: {strategy_b:?}"
    );
    for line in &lines {
        if let Some(decision_id) = line["decision_id"].as_str() {
            assert!(
                !decision_id.starts_with("s-"),
                "strategy decision ids cannot appear on the legacy path: {line}"
            );
        }
    }
}

/// One replay run; `explicit_none` exercises `.with_strategy_rx(None)`.
async fn run_pipeline_once(
    explicit_none: bool,
) -> (
    String,
    Vec<Alert>,
    sentinel::pipeline::PipelineOutcome,
    Vec<AuditEntry>,
) {
    let cfg_vars: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", SECRET),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("MARKET_ALLOWLIST", "32,16"),
        ("REFLEX_COOLDOWN_SECS", "170"),
        ("MAX_ORDER_SIZE_USD", "100000"),
        ("REQUIRE_APPROVAL_ABOVE_USD", "100000"),
    ];
    let cfg = cfg_with(cfg_vars);
    let dir = tempfile::tempdir().expect("pipeline tempdir");
    let journal = Arc::new(Mutex::new(
        AuditJournal::open(dir.path().join("journal")).expect("journal opens"),
    ));
    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));
    let feed = MockPerpl::from_fixture(&p20_fixture()).expect("p20 fixture loads");
    let executor = DryRunExecutor::new(
        StateProbe::new(Arc::clone(&state)),
        10,
        dir.path().join("dry-run.jsonl"),
        0,
    );
    let sink = RecordingSink::new();
    let alerts = Arc::clone(&sink.alerts);
    let mut pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Replay)
        .with_journal(Arc::clone(&journal));
    if explicit_none {
        pipeline = pipeline.with_strategy_rx(None);
    }
    let (_tx, rx) = watch::channel(false); // keep the sender alive for the run
    let outcome = pipeline.run(rx).await.expect("pipeline run");

    let journal_entries = journal
        .lock()
        .await
        .read_entries(0, 500)
        .expect("journal read")
        .into_iter()
        .filter(|entry| entry.trigger == Trigger::Strategy)
        .collect();
    (
        outcome.to_jsonl(),
        alerts.lock().expect("alerts lock").clone(),
        outcome,
        journal_entries,
    )
}

// ---------------------------------------------------------------------------
// §4 — judge-demo static contract.
// ---------------------------------------------------------------------------

/// `scripts/judge-demo.sh` must exist, be executable, pass `bash -n`, and carry
/// the trap-based cleanup plus the frozen default port / healthz wait
/// (SPEC-P20 §4). The full behavioral run is executed separately (transcript).
#[test]
fn judge_demo_script_satisfies_static_contract() {
    let script = repo_root().join("scripts/judge-demo.sh");
    assert!(script.is_file(), "scripts/judge-demo.sh must exist");

    let syntax = Command::new("bash")
        .arg("-n")
        .arg(&script)
        .output()
        .expect("bash must be available");
    assert!(
        syntax.status.success(),
        "bash -n must be clean: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    let text = std::fs::read_to_string(&script).expect("script readable");
    assert!(text.contains("trap "), "cleanup trap missing");
    assert!(text.contains("healthz"), "/healthz wait missing");
    assert!(text.contains("8090"), "default port 8090 missing");
    assert!(text.contains("--docker"), "--docker flag missing");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "script must be executable");
    }
}
