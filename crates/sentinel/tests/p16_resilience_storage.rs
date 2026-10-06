//! SPEC-P16 §2 resilience: audit-journal degrade mode, anchor heartbeat
//! status file + RPC-outage drain, and websocket reconnect storms
//! (agent L, storage+feed).
//!
//! Everything here runs against real files and real processes:
//!
//! * the degrade test chmods a journal directory read-only (skipped when the
//!   process can still write, e.g. as root) and watches memory fallback,
//!   recovery ordering and the one-error-per-transition alarm;
//! * the anchor test runs a real `anvil` chain behind an RPC port that starts
//!   DEAD (bogus URL) and is later restored by starting anvil on that same
//!   port from a state file captured after `forge`-deployed the real
//!   `SentinelAuditAnchor` contract — so the restored chain carries the
//!   contract from its first block and there is no deploy window;
//! * the storm test drives the real `perpl/ws.rs` reconnect loops against a
//!   local tokio-tungstenite server that kills the socket ten times in a row.
//!
//! Prerequisites: foundry `anvil` (resolved from `ANVIL_BIN`, else
//! `~/.local/bin/anvil`, else `PATH`); `forge` only when the artifact
//! `contracts/out/SentinelAuditAnchor.sol/SentinelAuditAnchor.json` is
//! missing. Retry *spacing* (1 s → 30 s backoff, for both batches and
//! heartbeats) is pinned precisely in the `anchor` unit tests; this file
//! pins the end-to-end flow: journal untouched during the outage, queue
//! drains after the restore, status file written after successful work.
//!
//! Run: `cargo test -p sentinel --test p16_resilience_storage`

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};
use tokio_tungstenite::tungstenite::Message;

use alloy::primitives::Address;
use alloy::providers::RootProvider;

use sentinel::anchor::{self, AlloyAnchorSink, abi::SentinelAuditAnchor};
use sentinel::config::Config;
use sentinel::perpl::auth::ApiKeySigner;
use sentinel::perpl::ws::{self, FeedEvent, MarketEvent, WsConfig};
use sentinel_core::audit::{AuditEntry, AuditJournal, IntentRecord, Trigger, verify_chain};
use sentinel_core::types::{Market, MarketId};

// ===========================================================================
// Shared fixtures
// ===========================================================================

/// Journal account label used across the fixtures.
const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

/// Well-known anvil account #0 key (local-only; never funded on mainnet).
const ANVIL_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Well-known anvil account #0 address (lowercase, as JSON-RPC expects).
const ANVIL_ACCOUNT_0: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";

/// 32 bytes of zero, hex (the config layer wants 64 hex chars).
const ZERO_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

/// Fixed UTC timestamp (deterministic journal day files).
fn ts(y: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, month, day, hour, minute, second)
        .single()
        .expect("valid test timestamp")
}

/// One reflex intent record; `index` keeps entries distinct.
fn intent(index: u64) -> IntentRecord {
    IntentRecord {
        trigger: Trigger::Reflex,
        account: ACCOUNT.to_string(),
        market_id: Some(32),
        input_hash: format!("{index:064x}"),
        decision: json!({ "index": index }),
        policy_verdict: json!({ "verdict": "ALLOW" }),
    }
}

/// Record one entry at an explicit timestamp.
fn record_at(journal: &mut AuditJournal, index: u64, at: DateTime<Utc>) -> AuditEntry {
    journal
        .record_intent(&intent(index), at)
        .expect("journal record must not block")
}

/// Exit-status sequence numbers of a journal window.
fn seqs(entries: &[AuditEntry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.seq).collect()
}

/// A free localhost port (bound then released; anvil takes it next).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Repository root (`crates/sentinel` -> `crates` -> repo).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root resolves")
}

/// Resolve a foundry binary: `<NAME>_BIN` env, else `~/<home_relative>`,
/// else `PATH`.
fn executable(name: &str, home_relative: &str) -> String {
    let env_key = format!("{}_BIN", name.to_ascii_uppercase());
    if let Ok(value) = std::env::var(&env_key)
        && !value.trim().is_empty()
    {
        return value;
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(home).join(home_relative);
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    name.to_string()
}

/// `anvil` binary path.
fn anvil_binary() -> String {
    executable("anvil", ".local/bin/anvil")
}

/// `forge` binary path (only used when the contract artifact is missing).
fn forge_binary() -> String {
    executable("forge", ".local/bin/forge")
}

// ===========================================================================
// Minimal JSON-RPC / anvil harness
// ===========================================================================

/// One JSON-RPC call; `Err` on transport failure or a JSON-RPC error object.
async fn rpc_opt(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("{method}: {err}"))?;
    let value: Value = response
        .json()
        .await
        .map_err(|err| format!("{method}: {err}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("{method}: {error}"));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// JSON-RPC call that panics on failure.
async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    rpc_opt(client, url, method, params)
        .await
        .unwrap_or_else(|err| panic!("rpc {err}"))
}

/// A spawned `anvil` chain; killed on drop.
struct Anvil {
    child: Child,
    port: u16,
}

impl Anvil {
    /// Spawn anvil on `port` with extra CLI args and wait until its RPC
    /// answers `eth_chainId`.
    async fn start(
        client: &reqwest::Client,
        port: u16,
        workdir: &Path,
        tag: &str,
        extra: &[&str],
    ) -> Self {
        let log = workdir.join(format!("anvil-{tag}.log"));
        let stdout = fs::File::create(&log).expect("anvil log");
        let stderr = stdout.try_clone().expect("anvil log dup");
        let mut command = Command::new(anvil_binary());
        command.arg("--silent").arg("--port").arg(port.to_string());
        for arg in extra {
            command.arg(arg);
        }
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let child = command.spawn().unwrap_or_else(|err| {
            panic!(
                "spawning anvil failed ({err}); foundry `anvil` must be installed \
                 (resolved as {:?})",
                anvil_binary()
            )
        });
        let anvil = Self { child, port };
        anvil.wait_ready(client).await;
        anvil
    }

    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn wait_ready(&self, client: &reqwest::Client) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if rpc_opt(client, &self.rpc_url(), "eth_chainId", json!([]))
                .await
                .is_ok()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "anvil :{} never became ready (see anvil-*.log)",
                self.port
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Deploy `SentinelAuditAnchor` from the forge artifact with anvil account 0
/// (the account's first transaction on a fresh chain); returns the contract
/// address from the receipt.
async fn deploy_contract(client: &reqwest::Client, url: &str) -> String {
    let artifact =
        repo_root().join("contracts/out/SentinelAuditAnchor.sol/SentinelAuditAnchor.json");
    if !artifact.exists() {
        // Fallback: build the Foundry project once (writes contracts/out).
        let status = Command::new(forge_binary())
            .args(["build", "-q"])
            .current_dir(repo_root().join("contracts"))
            .status()
            .expect("forge build failed to spawn; foundry `forge` must be installed");
        assert!(
            status.success(),
            "forge build failed; artifact missing: {artifact:?}"
        );
    }
    let artifact_json: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).expect("read contract artifact"))
            .expect("artifact JSON");
    let bytecode = artifact_json["bytecode"]["object"]
        .as_str()
        .expect("artifact bytecode.object")
        .to_string();

    let tx = rpc(
        client,
        url,
        "eth_sendTransaction",
        json!([{
            "from": ANVIL_ACCOUNT_0,
            "data": bytecode,
            "gas": "0x1312D00",
        }]),
    )
    .await;
    let hash = tx.as_str().expect("deploy tx hash").to_string();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let receipt = rpc(client, url, "eth_getTransactionReceipt", json!([hash])).await;
        if !receipt.is_null() {
            return receipt["contractAddress"]
                .as_str()
                .expect("contractAddress in receipt")
                .to_string();
        }
        assert!(Instant::now() < deadline, "deploy receipt never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ===========================================================================
// (a) AuditJournal degrade mode (`SPEC-P16` §2)
// ===========================================================================

/// Minimal `MakeWriter` for `tracing_subscriber` that collects formatted
/// events in memory (used to count the degrade alarm).
#[derive(Clone, Default)]
struct CaptureWriter(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Number of captured ERROR lines about degrading to in-memory.
fn degrade_error_count(capture: &CaptureWriter) -> usize {
    let bytes = capture.0.lock().expect("capture lock").clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains("degrading to in-memory"))
        .count()
}

/// chmod `dir` to `mode`.
#[cfg(unix)]
fn set_dir_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).expect("chmod test dir");
}

/// Make `dir` read-only and confirm writes now fail. `false` means the
/// process can still write (root, or a mode-ignoring filesystem), so the
/// caller must skip; the directory is restored to 0o755 before returning
/// `false`.
#[cfg(unix)]
fn lock_readonly(dir: &Path) -> bool {
    set_dir_mode(dir, 0o555);
    let probe = dir.join(".write-probe");
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            set_dir_mode(dir, 0o755);
            false
        }
        Err(_) => true,
    }
}

/// Persistence failure degrades to memory (flag set, flow unblocked, reads
/// complete, one `tracing::error!` per transition); the next append retries
/// and both the degraded entries and the new one land on disk, in order.
///
/// A read-only directory only blocks *creating* a day file (appending to an
/// existing one needs no directory write bit — POSIX), so the degraded
/// entries target day files that do not exist yet; the second episode
/// rotates into a brand-new day file for the same reason.
#[cfg(unix)]
#[test]
fn audit_degrade_persist_failure_keeps_flow_recovers_in_order_and_alarms_once() {
    // Capture ERROR events (thread-local subscriber: all journal calls below
    // run synchronously on this thread).
    let capture = CaptureWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().expect("audit dir");
    let day1 = dir.path().join("journal-20261006.jsonl");
    let day2 = dir.path().join("journal-20261007.jsonl");
    let mut journal = AuditJournal::open(dir.path()).expect("open journal");
    assert!(!journal.degraded(), "healthy journal is not degraded");

    if !lock_readonly(dir.path()) {
        eprintln!(
            "SKIP audit_degrade_persist_failure_keeps_flow_recovers_in_order_and_alarms_once: \
             cannot simulate EACCES (running as root?)"
        );
        return;
    }

    // Persistence fails (no day file can be created): records still succeed,
    // in memory only, and the alarm fires exactly once per transition.
    let e0 = record_at(&mut journal, 0, ts(2026, 10, 6, 12, 0, 0));
    assert!(journal.degraded(), "degraded flag set after the failure");
    assert_eq!(
        degrade_error_count(&capture),
        1,
        "one error on the transition"
    );
    let e1 = record_at(&mut journal, 1, ts(2026, 10, 6, 12, 0, 1));
    assert!(journal.degraded(), "still degraded on repeated failure");
    assert_eq!(
        degrade_error_count(&capture),
        1,
        "no second alarm while already degraded"
    );

    // Reads through the handle see the in-memory tail, in order; the disk
    // has nothing yet.
    let window = journal.read_entries(0, 64).expect("degraded read");
    assert_eq!(seqs(&window), vec![0, 1]);
    assert_eq!(
        journal.read_entries(0, 1).expect("limited read").len(),
        1,
        "limit still honored"
    );
    assert!(!day1.exists(), "no journal file while degraded");

    // Unlock: the NEXT append retries the queued entries first, then itself.
    set_dir_mode(dir.path(), 0o755);
    let e2 = record_at(&mut journal, 2, ts(2026, 10, 6, 12, 0, 2));
    assert!(!journal.degraded(), "recovered after the successful flush");
    assert_eq!(e2.prev_hash, e1.entry_hash, "in-memory chain preserved");
    assert_eq!(degrade_error_count(&capture), 1, "recovery is not an error");

    let raw = fs::read_to_string(&day1).expect("read recovered file");
    let parsed: Vec<AuditEntry> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("line parses"))
        .collect();
    assert_eq!(
        seqs(&parsed),
        vec![0, 1, 2],
        "degraded entries then the new one"
    );
    for pair in parsed.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].entry_hash, "chain links");
    }
    assert_eq!(parsed[0], e0, "the exact degraded entry reached the disk");
    assert_eq!(parsed[1], e1, "the exact degraded entry reached the disk");
    let verify = verify_chain(&day1).expect("verify recovered");
    assert!(verify.broken_at.is_none(), "{verify:?}");
    assert_eq!(verify.entries, 3);

    // The handle and the disk agree again (no duplicates, no phantoms).
    let window = journal.read_entries(0, 64).expect("post-recovery read");
    assert_eq!(seqs(&window), vec![0, 1, 2]);

    // A second episode (a new day file cannot be created) degrades and
    // recovers the same way, and the alarm count reflects the second
    // transition only.
    if lock_readonly(dir.path()) {
        record_at(&mut journal, 3, ts(2026, 10, 7, 0, 0, 0));
        assert!(journal.degraded());
        assert_eq!(
            degrade_error_count(&capture),
            2,
            "second transition, second alarm"
        );
        set_dir_mode(dir.path(), 0o755);
        record_at(&mut journal, 4, ts(2026, 10, 7, 0, 0, 1));
        assert!(!journal.degraded());
        assert_eq!(
            degrade_error_count(&capture),
            2,
            "no extra alarm on recovery"
        );
        let verify = verify_chain(&day2).expect("verify second day file");
        assert!(verify.broken_at.is_none(), "{verify:?}");
        assert_eq!(verify.first_seq, Some(3));
        assert_eq!(verify.valid_up_to_seq, Some(4));
    }
}

// ===========================================================================
// (b) Anchor heartbeat status file + RPC outage drain (`SPEC-P16` §2)
// ===========================================================================

/// Minimal config pointed at a local anvil (all `Config` invariants
/// satisfied; no process env is read by `from_vars`).
fn anchor_cfg(rpc_url: &str, contract: &str, key: &str) -> Config {
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
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    )
    .expect("anvil config must load")
}

/// RPC outage: a bogus RPC URL (dead port) makes beats and batches fail with
/// backoff while the journal keeps accepting entries; restoring anvil on the
/// same port drains the whole queue, and the heartbeat status file appears
/// only after successful work.
#[tokio::test]
async fn anchor_survives_rpc_outage_then_drains_after_restore_and_writes_status() {
    let start_ms = unix_ms();
    let client = reqwest::Client::new();
    let workdir = tempfile::tempdir().expect("workdir");

    // --- fixture: a chain whose state already carries the deployed contract
    // (dumped periodically by anvil, so the later restore has no deploy
    // window at all). ---
    let scratch_port = free_port();
    let state_path = workdir.path().join("anvil-state.json");
    let state_arg = state_path.display().to_string();
    let scratch = Anvil::start(
        &client,
        scratch_port,
        workdir.path(),
        "scratch",
        &["--state", state_arg.as_str(), "--state-interval", "1"],
    )
    .await;
    let contract = deploy_contract(&client, &scratch.rpc_url()).await;
    {
        // Wait for the periodic dump to include the deployment.
        let addr_body = contract.trim_start_matches("0x").to_ascii_lowercase();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = fs::read_to_string(&state_path)
                && text.contains(&addr_body)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "state dump never included the deployment"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    drop(scratch); // scratch chain done; the state file stays.

    // --- the service runs against a DEAD port (bogus RPC URL) ---
    let live_port = free_port();
    let rpc_url = format!("http://127.0.0.1:{live_port}");
    let cfg = anchor_cfg(&rpc_url, &contract, ANVIL_KEY_0);

    let audit_dir = workdir.path().join("audit");
    let mut journal = AuditJournal::open(&audit_dir).expect("open journal");
    for index in 0..3u64 {
        record_at(&mut journal, index, ts(2026, 10, 6, 12, 0, index as u32));
    }
    let journal = Arc::new(Mutex::new(journal));

    let status_path = workdir.path().join("heartbeat.json");
    // SAFETY: `set_var` is unsafe in Rust 2024 (the environment is
    // process-global). This is the only test in this binary that reads or
    // writes `HEARTBEAT_PATH`; it is set before the anchor task starts and
    // removed after the task has stopped.
    unsafe { std::env::set_var("HEARTBEAT_PATH", &status_path) };

    let sink = AlloyAnchorSink::new(&cfg).expect("build alloy sink");
    let signer_address = sink.signer_address();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn({
        let cfg = cfg.clone();
        let journal = Arc::clone(&journal);
        async move { anchor::run(&cfg, journal, sink, shutdown_rx).await }
    });

    // --- outage phase: attempts fail; the journal must not ---
    tokio::time::sleep(Duration::from_millis(2500)).await;
    {
        let mut guard = journal.lock().await;
        for index in 3..5u64 {
            record_at(&mut guard, index, ts(2026, 10, 6, 12, 0, index as u32));
        }
        assert!(
            !guard.degraded(),
            "an RPC outage is not a journal persistence failure"
        );
    }
    let window = journal
        .lock()
        .await
        .read_entries(0, 64)
        .expect("read journal");
    assert_eq!(
        seqs(&window),
        vec![0, 1, 2, 3, 4],
        "the journal keeps accepting entries during the outage"
    );
    let day_file = audit_dir.join("journal-20261006.jsonl");
    let verify = verify_chain(&day_file).expect("verify journal");
    assert!(verify.broken_at.is_none(), "{verify:?}");
    assert_eq!(verify.entries, 5);
    assert!(
        !status_path.exists(),
        "no successful beat or batch -> no status file during the outage"
    );

    // --- restore: anvil comes up on the SAME previously-dead port with the
    // contract already in the loaded state ---
    let restored = Anvil::start(
        &client,
        live_port,
        workdir.path(),
        "restored",
        &["--load-state", state_arg.as_str()],
    )
    .await;
    let code = rpc(
        &client,
        &restored.rpc_url(),
        "eth_getCode",
        json!([contract, "latest"]),
    )
    .await;
    assert!(
        code.as_str().is_some_and(|hex| hex.len() > 2),
        "restored chain carries the contract code"
    );

    // --- drain: the retry loop anchors the whole queue, outage entries
    // included, in order ---
    let provider: RootProvider = RootProvider::new_http(rpc_url.parse().expect("valid rpc url"));
    let address: Address = contract.parse().expect("valid contract address");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let anchored = SentinelAuditAnchor::new(address, &provider)
            .lastSeq(signer_address)
            .call()
            .await
            .unwrap_or_default();
        if anchored >= 5 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queue drains within 45 s of the restore (lastSeq = {anchored})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // --- heartbeat status file: written after successful work; the journal
    // head (5) is the steady-state value because beats keep firing ---
    let deadline = Instant::now() + Duration::from_secs(10);
    let status: Value = loop {
        if let Ok(raw) = fs::read_to_string(&status_path)
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
            && value["seq"] == json!(5)
        {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "heartbeat status file written after recovery"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let tx_hash = status["tx_hash"]
        .as_str()
        .expect("tx_hash string")
        .to_string();
    assert!(
        tx_hash.starts_with("0x") && tx_hash.len() == 66,
        "tx hash shape: {tx_hash}"
    );
    let status_ms = status["ts_ms"].as_u64().expect("ts_ms u64");
    assert!(status_ms >= start_ms, "ts_ms is a fresh wall-clock stamp");
    assert!(
        !status_path.with_extension("json.tmp").exists(),
        "the atomic write leaves no tmp file"
    );

    // --- shutdown: clean, counted, nothing lost ---
    let _ = shutdown_tx.send(true);
    let report = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("anchor stops within 5 s of shutdown")
        .expect("anchor task joins")
        .expect("anchor run is Ok");
    assert_eq!(
        report.entries_anchored, 5,
        "every journal entry anchored (outage queue included): {report:?}"
    );
    assert!(
        report.batches >= 2,
        "the queue drained in ordered passes (3 pre-outage + 2 outage entries): {report:?}"
    );
    assert!(
        report.failures >= 1,
        "outage attempts were counted as failures: {report:?}"
    );

    // The journal survived the whole cycle untouched.
    let window = journal
        .lock()
        .await
        .read_entries(0, 64)
        .expect("final read");
    assert_eq!(seqs(&window), vec![0, 1, 2, 3, 4]);
    let verify = verify_chain(&day_file).expect("final verify");
    assert!(verify.broken_at.is_none(), "{verify:?}");

    drop(restored);
    // SAFETY: as above — remove the variable this test set, once the only
    // reader (the anchor task) has stopped.
    unsafe { std::env::remove_var("HEARTBEAT_PATH") };
}

// ===========================================================================
// (c) WebSocket reconnect storm (`SPEC-P16` §2)
// ===========================================================================

/// Minimal core `Market` for socket-layer tests (SPEC v1.0.1 §11c).
fn test_market(id: u32, price_decimals: u32) -> Market {
    Market {
        id: MarketId(id),
        symbol: format!("T{id}"),
        base: format!("T{id}"),
        price_decimals,
        size_decimals: 3,
        initial_margin_fraction: Decimal::new(1, 1),
        maintenance_margin_fraction: Decimal::new(5, 2),
        max_leverage: Decimal::new(10, 0),
        min_size: Decimal::ZERO,
        tick_size: Decimal::new(1, price_decimals),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

/// The market-data socket survives ten consecutive server-side kill cycles
/// (a storm), honoring the reconnect backoff floor, and a market event is
/// delivered after recovery — with no panic anywhere.
#[tokio::test]
async fn ws_market_socket_survives_ten_kill_cycles_backs_off_and_recovers() {
    const KILL_CYCLES: usize = 10;
    const MARK_RAW: i64 = 271_370;

    // --- local tokio-tungstenite server: kills the first KILL_CYCLES
    // market-data sessions (after reading their subscribe frame, dropped
    // without a close handshake), then serves one market-state frame and
    // stays open. The trading socket is held open for the whole test. ---
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws test server");
    let addr = listener.local_addr().expect("local addr");
    let market_accepts: Arc<StdMutex<Vec<Instant>>> = Arc::new(StdMutex::new(Vec::new()));

    let server = {
        let market_accepts = Arc::clone(&market_accepts);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let accepts = Arc::clone(&market_accepts);
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    // Classify by first frame: mt:5 = market-data (the
                    // subscribe), mt:29 = trading (sign-in). Unclassifiable
                    // connections are dropped so the client retries.
                    let first = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
                    let first_mt = match &first {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            serde_json::from_str::<Value>(text.as_str())
                                .ok()
                                .and_then(|value| value.get("mt").and_then(Value::as_u64))
                        }
                        _ => None,
                    };
                    match first_mt {
                        Some(5) => {
                            let kill_index = {
                                let mut accepts = accepts.lock().expect("accepts lock");
                                accepts.push(Instant::now());
                                accepts.len()
                            };
                            if kill_index <= KILL_CYCLES {
                                // The storm kill: drop the socket mid-session.
                                drop(socket);
                                return;
                            }
                            // Recovered: deliver one mark-price frame, keep
                            // the socket open afterwards.
                            let frame = json!({
                                "mt": 9,
                                "d": { "32": { "mrk": MARK_RAW, "at": { "t": unix_ms() } } }
                            })
                            .to_string();
                            let _ = socket.send(Message::text(frame)).await;
                            while socket.next().await.is_some() {}
                        }
                        Some(29) => {
                            // Trading socket: hold it open (the client pings
                            // only after 30 s; the test is far shorter).
                            while socket.next().await.is_some() {}
                        }
                        _ => {
                            drop(socket);
                        }
                    }
                });
            }
        })
    };

    // --- the real socket loops against the local server ---
    let ws_cfg = WsConfig {
        ws_url: format!("ws://{addr}"),
        chain_id: 10_143,
        markets: vec![test_market(32, 2)],
        stale_after: Duration::from_secs(30),
    };
    let signer = ApiKeySigner::from_parts("ws-storm-token", &"ab".repeat(32), 10_143)
        .expect("signer from parts");
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(async move { ws::run(ws_cfg, &signer, tx, shutdown_rx).await });

    // --- wait for the event delivered after recovery ---
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut reconnects = 0_usize;
    let price = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("market event within 30 s of storm reconnects")
            .expect("feed event bus stays open");
        match event {
            FeedEvent::Market(MarketEvent::MarkPrice {
                market_id, price, ..
            }) => {
                assert_eq!(market_id, MarketId(32));
                break price;
            }
            FeedEvent::Reconnected { attempt } => {
                assert_eq!(
                    attempt, 0,
                    "only clean (killed) sessions, no failed connects"
                );
                reconnects += 1;
            }
            FeedEvent::Account(_) | FeedEvent::FeedStale { .. } => {}
        }
    };
    assert_eq!(
        price,
        Decimal::new(MARK_RAW, 2),
        "the mark price from the recovered session"
    );

    // --- backoff honored: every reconnect waited at least the 500 ms floor
    // (backoff_base(0)) and none ran away ---
    let accepts = market_accepts.lock().expect("accepts lock").clone();
    assert!(
        accepts.len() > KILL_CYCLES,
        "10 kills + the recovery session; saw {}",
        accepts.len()
    );
    for (index, pair) in accepts.windows(2).enumerate() {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(480),
            "reconnect #{} honored the backoff floor: {gap:?}",
            index + 2
        );
        assert!(
            gap <= Duration::from_secs(3),
            "no runaway reconnect delay: {gap:?}"
        );
    }
    assert!(
        reconnects >= KILL_CYCLES,
        "one Reconnected per kill cycle: {reconnects}"
    );

    // --- shutdown: no panic, graceful stop ---
    let _ = shutdown_tx.send(true);
    let result = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("ws::run stops after shutdown")
        .expect("ws task joins");
    assert!(result.is_ok(), "graceful stop without panic: {result:?}");
    server.abort();
}
