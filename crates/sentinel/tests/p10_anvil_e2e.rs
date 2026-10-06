//! P10 anvil end-to-end (`#[ignore]`): anchors a fabricated journal against
//! a REAL deployed `SentinelAuditAnchor` on a local anvil chain, cross-checks
//! the emitted `DecisionAnchored` events with `eth_getLogs`, and finally runs
//! the real `audit-verify` binary against the same journal + chain, asserting
//! the exact Trust-beat line.
//!
//! Run via `scripts/p10-anvil-e2e.sh` (the parent runbook: forge build →
//! anvil → deploy → this test with `--ignored`). Environment:
//!
//! - `ANVIL_RPC_URL` — default `http://127.0.0.1:8545`;
//! - `ANVIL_CONTRACT` — deployed contract address (**required**; without it
//!   the test skips with a clear message so a bare
//!   `cargo test -- --ignored` stays green);
//! - `ANVIL_KEY` — funded signer key (default: anvil account #0).
//!
//! Prerequisites: a *freshly deployed* contract (zero anchors). A contract
//! that already carries anchors is skipped with a message — the runbook
//! always deploys fresh; re-run `scripts/p10-anvil-e2e.sh`.
//!
//! Expected: a batch of five fabricated entries (seqs 0..=4) anchors as
//! on-chain `fromSeq = 1`, emitting `DecisionAnchored(toSeq = 5,
//! entryHash = entry[4].entry_hash, runningRoot = merkle_root(all five))`,
//! and `audit-verify` prints
//! `✅ journal consistent; 5 entries; 5 anchored; root matches at seq 5`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::{Provider as _, RootProvider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use chrono::Utc;
use serde_json::json;
use tokio::sync::{Mutex, watch};

use sentinel::anchor::{
    AlloyAnchorSink, AnchorRunReport, abi::SentinelAuditAnchor, merkle_root, run,
};
use sentinel::config::Config;
use sentinel_core::audit::{AuditJournal, IntentRecord, Trigger};

/// Well-known anvil account #0 key (local-only; never funded on mainnet).
const ANVIL_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Journal account label for the fabricated entries.
const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

/// 32 bytes of zero, hex (the config layer wants 64 hex chars).
const ZERO_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[tokio::test]
#[ignore = "requires anvil + a deployed SentinelAuditAnchor; run scripts/p10-anvil-e2e.sh"]
async fn anchors_a_five_entry_journal_on_anvil() {
    let contract = match std::env::var("ANVIL_CONTRACT") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!(
                "SKIP p10_anvil_e2e: set ANVIL_CONTRACT (and ANVIL_RPC_URL) — \
                 run scripts/p10-anvil-e2e.sh to deploy and execute"
            );
            return;
        }
    };
    let rpc_url =
        std::env::var("ANVIL_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545".to_string());
    let key = std::env::var("ANVIL_KEY").unwrap_or_else(|_| ANVIL_KEY_0.to_string());

    // 1. Fabricated journal: five entries (seq 0..=4).
    let workdir = tempfile::tempdir().expect("tempdir");
    let audit_dir = workdir.path().join("audit");
    let mut journal = AuditJournal::open(&audit_dir).expect("open journal");
    let mut recorded = Vec::new();
    for index in 0..5_u64 {
        let record = IntentRecord {
            trigger: Trigger::Reflex,
            account: ACCOUNT.to_string(),
            market_id: Some(32),
            input_hash: format!("{:064x}", index + 1),
            decision: json!({ "index": index }),
            policy_verdict: json!({ "verdict": "allow" }),
        };
        recorded.push(journal.record_intent(&record, Utc::now()).expect("record"));
    }
    let journal = Arc::new(Mutex::new(journal));

    // 2. Real sink against anvil + the deployed contract.
    let cfg = anvil_cfg(&rpc_url, &contract, &key);
    let sink = AlloyAnchorSink::new(&cfg).expect("build alloy sink against anvil");

    let url = rpc_url.parse().expect("valid anvil url");
    let provider: RootProvider = RootProvider::new_http(url);
    let address: Address = contract.trim().parse().expect("valid contract address");

    // 3. The runbook deploys a fresh contract; a reused one invalidates the
    //    from-seq-1 expectations below, so skip with instructions instead.
    let anchored_so_far = SentinelAuditAnchor::new(address, &provider)
        .lastSeq(sink.signer_address())
        .call()
        .await
        .expect("read lastSeq from anvil");
    if anchored_so_far > 0 {
        eprintln!(
            "SKIP p10_anvil_e2e: contract {address} already has {anchored_so_far} anchor(s); \
             re-run scripts/p10-anvil-e2e.sh for a fresh chain"
        );
        return;
    }

    // 4. Run the service; it anchors the un-anchored tail immediately. Wait
    //    (bounded) until the batch lands on chain, then flip shutdown — the
    //    short heartbeat interval still exercises the cadence meanwhile.
    let sink_address = sink.signer_address();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner = {
        let cfg = cfg.clone();
        let journal = Arc::clone(&journal);
        tokio::spawn(async move { run(&cfg, journal, sink, shutdown_rx).await })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let anchored = SentinelAuditAnchor::new(address, &provider)
            .lastSeq(sink_address)
            .call()
            .await
            .expect("read lastSeq while waiting for the batch");
        if anchored >= 5 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "anchor service did not anchor within 10 s (contract lastSeq = {anchored})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    shutdown_tx.send(true).expect("flip shutdown");
    let report: AnchorRunReport = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("run stops within 5 s of shutdown")
        .expect("task joins")
        .expect("run is Ok");

    assert!(
        report.batches >= 1,
        "at least one batch anchored: {report:?}"
    );
    assert!(
        report.entries_anchored >= 5,
        "all five entries anchored: {report:?}"
    );

    // 5. eth_getLogs cross-check of the batch event.
    let filter = Filter::new()
        .address(address)
        .from_block(0_u64)
        .event_signature(SentinelAuditAnchor::DecisionAnchored::SIGNATURE_HASH);
    let logs = provider.get_logs(&filter).await.expect("eth_getLogs");

    let hashes: Vec<String> = recorded
        .iter()
        .map(|entry| entry.entry_hash.clone())
        .collect();
    let root = merkle_root(&hashes);
    let last_hash = hashes.last().expect("five hashes").clone();

    let mut matched = false;
    for log in &logs {
        let decoded = log
            .log_decode::<SentinelAuditAnchor::DecisionAnchored>()
            .expect("decode DecisionAnchored");
        let event = decoded.data();
        if event.seq == 5 {
            assert_eq!(
                normalize(&event.entryHash.to_string()),
                normalize(&last_hash),
                "event carries the batch's last entry hash"
            );
            assert_eq!(
                normalize(&event.runningRoot.to_string()),
                normalize(&root),
                "event root equals the local merkle root"
            );
            matched = true;
        }
    }
    assert!(
        matched,
        "DecisionAnchored(toSeq = 5) not found among {} log(s)",
        logs.len()
    );

    // 6. The real binary: exact Trust-beat line + explorer link + exit 0.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_audit-verify"))
        .arg("--journal")
        .arg(journal_file(&audit_dir))
        .arg("--from-block")
        .arg("0")
        .env("ANCHOR_CONTRACT_ADDRESS", contract.trim())
        .env("PERPL_RPC_URL", &rpc_url)
        .output()
        .expect("spawn audit-verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "audit-verify must exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        stdout.lines().next().unwrap_or_default(),
        "✅ journal consistent; 5 entries; 5 anchored; root matches at seq 5",
        "exact Trust-beat line\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("https://testnet.monadexplorer.com/tx/0x"),
        "explorer link for the anchor tx\nstdout: {stdout}"
    );

    println!(
        "p10 anvil e2e ok: {report:?}; {} DecisionAnchored event(s); audit-verify: {}",
        logs.len(),
        stdout.trim().replace('\n', " | ")
    );
}

/// The single `journal-*.jsonl` file under `dir`.
fn journal_file(dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("audit dir readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "exactly one journal file expected");
    files.pop().expect("journal file written")
}

/// Lowercase hex without a `0x` prefix (journal hashes have neither).
fn normalize(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

/// Minimal config pointed at anvil (all `Config` invariants satisfied).
fn anvil_cfg(rpc_url: &str, contract: &str, key: &str) -> Config {
    let base: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", ZERO_SECRET),
        ("PERPL_ACCOUNT", ACCOUNT),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("EXECUTION_MODE", "DRY_RUN"),
        ("HEARTBEAT_INTERVAL_SECS", "1"),
        ("PERPL_RPC_URL", rpc_url),
        ("ANCHOR_CONTRACT_ADDRESS", contract),
        ("RPC_SIGNER_KEY", key),
    ];
    Config::from_vars(
        base.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
    )
    .expect("anvil config must load")
}
