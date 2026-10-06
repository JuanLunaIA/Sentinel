//! P06 acceptance (parent): replay determinism + shutdown contract.
//!
//! SPEC-P06.md §6/§9: the same fixture + the same config must produce an
//! identical `PipelineOutcome::to_jsonl()` (and identical alerts), and a
//! shutdown flip must drain to exactly one trailing `Shutdown` event.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sentinel::config::Config;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::health::HealthState;
use sentinel::notify::{Alert, RecordingSink};
use sentinel::perpl::MockPerpl;
use sentinel::pipeline::{LiveState, Pipeline, PipelineEvent, PipelineOutcome, RunMode, StateProbe};
use tokio::sync::{Mutex, watch};

const SECRET: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/perpl/crash-scenario.jsonl")
}

fn demo_cfg() -> Config {
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
        ("MARKET_ALLOWLIST", "32,16"),
        ("REFLEX_COOLDOWN_SECS", "150"),
        ("MAX_ORDER_SIZE_USD", "100000"),
        ("REQUIRE_APPROVAL_ABOVE_USD", "100000"),
    ];
    Config::from_vars(
        base.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    )
    .expect("demo config must load")
}

/// Fixture replay must be instant in tests (no pacing sleeps).
fn fast_pacing() {
    // Rust 2024: `set_var` is unsafe (thread safety). Both tests set the same
    // value, so the benign race is acceptable in a test binary.
    unsafe {
        std::env::set_var("SENTINEL_MOCK_PACE", "100000");
    }
}

fn dry_run_report_path() -> PathBuf {
    std::env::temp_dir().join("sentinel-p06-dryrun-reports.jsonl")
}

async fn run_once() -> (String, Vec<Alert>, PipelineOutcome) {
    let cfg = demo_cfg();
    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));
    let feed = MockPerpl::from_fixture(&fixture()).expect("fixture loads");
    let executor = DryRunExecutor::new(StateProbe::new(Arc::clone(&state)), 10, dry_run_report_path(), 0);
    let sink = RecordingSink::new();
    let alerts = Arc::clone(&sink.alerts);
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Replay);
    let (_tx, rx) = watch::channel(false); // keep the sender alive for the run
    let outcome = pipeline.run(rx).await.expect("pipeline run");
    let captured = alerts.lock().expect("alerts lock").clone();
    (outcome.to_jsonl(), captured, outcome)
}

fn as_f64(value: &serde_json::Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .expect("numeric")
}

#[tokio::test]
async fn replay_is_deterministic_and_hits_the_golden_sequence() {
    fast_pacing();
    let (jsonl_a, alerts_a, _) = run_once().await;
    let (jsonl_b, alerts_b, _) = run_once().await;
    assert_eq!(jsonl_a, jsonl_b, "same fixture must yield an identical event stream");
    assert_eq!(alerts_a, alerts_b, "same fixture must yield identical alerts");
    assert!(!jsonl_a.is_empty());

    let events: Vec<serde_json::Value> = jsonl_a
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();

    // Tier transitions, in order, with the from/to chain intact.
    let tier_seq: Vec<String> = events
        .iter()
        .filter(|event| event["event"] == "tier_changed")
        .map(|event| {
            format!(
                "{}->{}",
                event["from"].as_str().unwrap_or("null"),
                event["to"].as_str().unwrap_or("?")
            )
        })
        .collect();
    assert_eq!(
        tier_seq,
        vec!["Green->Yellow", "Yellow->Orange", "Orange->Red"],
        "golden tier chain"
    );

    // Consult scheduled on Yellow entry.
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "consult_scheduled" && event["tier"] == "Yellow"),
        "Yellow entry must schedule a consult"
    );

    // Two reflex executes: 25 % then 50 % of 10.000 = 2.5 and 5.0.
    let fills: Vec<f64> = events
        .iter()
        .filter(|event| event["event"] == "executed")
        .map(|event| as_f64(&event["filled_size"]))
        .collect();
    assert_eq!(fills.len(), 2, "exactly the two saves");
    assert!((fills[0] - 2.5).abs() < 1e-9 && (fills[1] - 5.0).abs() < 1e-9);

    // Exactly one shutdown, at the very end.
    let shutdowns = events.iter().filter(|event| event["event"] == "shutdown").count();
    assert_eq!(shutdowns, 1);
    assert_eq!(events.last().expect("non-empty")["event"], "shutdown");
}

#[tokio::test]
async fn shutdown_drains_and_reports_once() {
    fast_pacing();
    let cfg = demo_cfg();
    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));
    let feed = MockPerpl::from_fixture(&fixture()).expect("fixture loads");
    let executor = DryRunExecutor::new(StateProbe::new(Arc::clone(&state)), 10, dry_run_report_path(), 0);
    let sink = RecordingSink::new();
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Replay);
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move { pipeline.run(rx).await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = tx.send(true); // may race with a fixture end; both paths are valid

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run must end within 5 s")
        .expect("task must not panic")
        .expect("run must return Ok");
    let shutdowns = outcome
        .events
        .iter()
        .filter(|event| matches!(event, PipelineEvent::Shutdown { .. }))
        .count();
    assert_eq!(shutdowns, 1, "exactly one shutdown event");
    assert!(matches!(outcome.events.last(), Some(PipelineEvent::Shutdown { .. })));
}
