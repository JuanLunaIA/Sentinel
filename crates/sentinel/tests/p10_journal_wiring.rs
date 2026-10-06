//! P10 wiring test (parent-owned): the crash-fixture replay journals an
//! intent/outcome PAIR for every intent-bearing decision, and the resulting
//! file is a valid hash chain (audit-before-action, SPEC-P10 §8).

use std::path::PathBuf;
use std::sync::Arc;

use sentinel::config::Config;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::health::HealthState;
use sentinel::notify::RecordingSink;
use sentinel::perpl::MockPerpl;
use sentinel::pipeline::{LiveState, Pipeline, PipelineEvent, RunMode, StateProbe};
use sentinel_core::audit::{AuditJournal, Trigger, verify_chain};
use tokio::sync::{Mutex, watch};

const SECRET: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/perpl/crash-scenario.jsonl")
}

fn demo_cfg() -> Config {
    let base: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", SECRET),
        (
            "PERPL_ACCOUNT",
            "0x0000000000000000000000000000000000000007",
        ),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("EXECUTION_MODE", "DRY_RUN"),
        ("MARKET_ALLOWLIST", "32,16"),
        ("REFLEX_COOLDOWN_SECS", "170"),
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

#[tokio::test]
async fn crash_replay_journals_intent_outcome_pairs_with_a_valid_chain() {
    // Replay must be instant (no pacing sleeps).
    unsafe {
        std::env::set_var("SENTINEL_MOCK_PACE", "100000");
    }

    let workdir = tempfile::tempdir().expect("tempdir");
    let audit_dir = workdir.path().join("audit");
    let journal = AuditJournal::open(&audit_dir).expect("open journal");
    let journal = Arc::new(Mutex::new(journal));

    let cfg = demo_cfg();
    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));
    let feed = MockPerpl::from_fixture(&fixture()).expect("fixture loads");
    let executor = DryRunExecutor::new(
        StateProbe::new(Arc::clone(&state)),
        10,
        workdir.path().join("dryrun-reports.jsonl"),
        0,
    );
    let sink = RecordingSink::new();
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Replay)
        .with_journal(Arc::clone(&journal));

    let (_tx, rx) = watch::channel(false);
    let outcome = pipeline.run(rx).await.expect("pipeline run");

    let executed = outcome
        .events
        .iter()
        .filter(|event| matches!(event, PipelineEvent::Executed { .. }))
        .count();
    assert_eq!(executed, 2, "golden sequence executes twice");

    // Two intent-bearing decisions ⇒ two full pairs (4 entries, seq 0..=3).
    let entries = journal
        .lock()
        .await
        .read_entries(0, 100)
        .expect("read journal");
    assert_eq!(
        entries.len(),
        4,
        "intent/outcome pair for each save: {entries:#?}"
    );
    assert_eq!(
        entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(entries.iter().all(|entry| entry.trigger == Trigger::Reflex));
    assert!(
        entries
            .iter()
            .all(|entry| entry.account == "0x0000000000000000000000000000000000000007"),
        "account label comes from PERPL_ACCOUNT"
    );
    // Pairing: intent(pending) then outcome(simulated) sharing one input hash.
    let statuses: Vec<&str> = entries
        .iter()
        .map(|entry| entry.execution["status"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        statuses,
        vec!["pending", "simulated", "pending", "simulated"]
    );
    assert_eq!(entries[0].input_hash, entries[1].input_hash);
    assert_eq!(entries[2].input_hash, entries[3].input_hash);
    assert!(!entries[0].input_hash.is_empty());

    // The chain on disk verifies.
    let file = std::fs::read_dir(&audit_dir)
        .expect("audit dir readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("journal file exists");
    let report = verify_chain(&file).expect("verify chain");
    assert!(report.broken_at.is_none(), "chain intact: {report:?}");
    assert_eq!(report.entries, 4);
    assert_eq!(report.valid_up_to_seq, Some(3));
}
