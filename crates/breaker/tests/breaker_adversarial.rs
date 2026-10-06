//! SPEC-P14 adversarial suite for the `breaker` binary (agent F / verifier).
//!
//! BLACK-BOX: this file never imports the breaker crate. It spawns the real
//! `breaker` binary (`CARGO_BIN_EXE_breaker`) with env configuration per
//! SPEC-P14 §2, against a real anvil chain running the real
//! `SentinelAuditAnchor` contract (heartbeats are real `beat(...)` txs; the
//! watcher's `eth_getLogs` intake is exercised end to end). All assertions are
//! derived from SPEC-P14 text only (see per-test comments); the independent
//! Python oracles under `tests/fixtures/` cross-check the HMAC vector and the
//! staleness/epoch arithmetic.
//!
//! Calibration notes (observed semantics the harness relies on, not spec
//! claims): the watcher's first poll is immediate at startup and then every
//! 15 s; `last_ts_ms` is the heartbeat block's timestamp ×1000; `age_secs` is
//! an integer; the breaker runs a 5 s "auto-fire" ticker that fires each
//! stale+critical+unfired-epoch guardian on its own (this is the §7 demo
//! behaviour). Tests here therefore use backdated anvil genesis for
//! "already stale" scenarios (no timing races) and epoch-keyed journal
//! assertions wherever the ticker can add lines.
//!
//! Run: `cargo test -p breaker --test breaker_adversarial`

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Spec constants
// ---------------------------------------------------------------------------

/// Well-known anvil dev accounts (lowercase; local-only throwaway keys).
const ANVIL_ACCOUNTS: [&str; 10] = [
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
    "0x70997970c51812dc3a010c7d01b50e0d17dc79c8",
    "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc",
    "0x90f79bf6eb2c4f870365e785982e1f101e93b906",
    "0x15d34aaf54267db7d7c367839aaf71a00a2c6a65",
    "0x9965507d1a55bcc2695c58ba16fb37d819b0a4dc",
    "0x976ea74026e726554db657fa54763abd0c3a0aa9",
    "0x14dc79964da2c08b23698b3d3cc7ca32193d9955",
    "0x23618e81e3f5cdf7f54c3d65f7fbc0abf5b21e8f",
    "0xa0ee7a142d267c1f36714e4a8f75612f20a79720",
];

/// SPEC §5: test secret, exact body bytes, frozen expected signature.
const SPEC_SECRET: &[u8] = b"spec-test-secret";
const SPEC_BODY: &str = r#"{"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","reason":"spec-vector","requested_at_ms":1791200000000}"#;
const SPEC_SIG_HEX: &str = "af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04";
const SPEC_REQUESTED_AT_MS: u64 = 1_791_200_000_000;

/// keccak256("beat(bytes32,uint32,uint8)")[..4] — `cast sig` verified.
const BEAT_SELECTOR: &str = "b35316d3";
/// keccak256("Heartbeat(address,bytes32,uint32,uint8)") — `cast keccak` verified.
const HEARTBEAT_TOPIC: &str = "0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee";

/// Contract artifact used for raw-RPC deployment (Foundry output).
const ARTIFACT_REL: &str = "contracts/out/SentinelAuditAnchor.sol/SentinelAuditAnchor.json";

/// House secret used by the suite (config for every spawned breaker).
const ARM_SECRET: &[u8] = b"adversarial-arm-secret";

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64
}

/// A genesis timestamp ~5 minutes in the past: heartbeats land old, so
/// guardians are "already stale" without wall-clock races.
fn backdated_genesis() -> u64 {
    now_ms() / 1000 - 300
}

fn wait_until<T>(
    what: &str,
    timeout: Duration,
    step: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> Result<T, String> {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return Ok(v);
        }
        if start.elapsed() > timeout {
            return Err(format!("timed out after {timeout:?} waiting for {what}"));
        }
        std::thread::sleep(step);
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local addr").port()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root resolves")
}

/// Base directory for per-test scratch dirs. Honors `P14_TEST_TMPDIR`; falls
/// back to a repo-local dir under `target/`, then the system temp dir.
fn test_base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("P14_TEST_TMPDIR") {
        return PathBuf::from(p);
    }
    let local = repo_root().join("target/p14-breaker-tests");
    if fs::create_dir_all(&local).is_ok() {
        return local;
    }
    let tmp = std::env::temp_dir().join("p14-breaker-tests");
    fs::create_dir_all(&tmp).expect("create fallback test dir");
    tmp
}

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique, auto-removing test scratch directory.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let unique = format!(
            "{name}-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = test_base_dir().join(unique);
        fs::create_dir_all(&path).expect("create test dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // Best effort; keep contents on failure by honoring an env flag.
        if std::env::var("P14_KEEP_TEST_DIRS").is_err() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client (std only)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
    body_text: String,
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    timeout: Duration,
) -> std::io::Result<HttpResponse> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no HTTP header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line: {status_line:?}")))?;
    let mut rest = &raw[header_end + 4..];
    let lower = head.to_ascii_lowercase();
    let dechunked;
    if lower.contains("transfer-encoding: chunked") {
        dechunked = dechunk(rest)?;
        rest = &dechunked;
    }
    let body = rest.to_vec();
    let body_text = String::from_utf8_lossy(&body).to_string();
    Ok(HttpResponse {
        status,
        body,
        body_text,
    })
}

fn dechunk(mut data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| std::io::Error::other("chunk size line missing"))?;
        let size_str = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| std::io::Error::other("bad chunk size"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err(std::io::Error::other("chunk truncated"));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn json_get(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

/// Numeric JSON value as f64, tolerating number or numeric string.
fn as_num(v: &Value) -> f64 {
    if let Some(n) = v.as_f64() {
        return n;
    }
    v.as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("value is not numeric: {v:?}"))
}

/// Numeric value with JSON null/absent treated as 0.0 (alert-only journal
/// lines carry `null` size/notional).
fn num_or_zero(v: &Value) -> f64 {
    if v.is_null() { 0.0 } else { as_num(v) }
}

// ---------------------------------------------------------------------------
// HMAC helper (spec §5: HMAC_SHA256(secret, raw body bytes), lowercase hex)
// ---------------------------------------------------------------------------

fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn sig_header(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hmac_sha256_hex(secret, body))
}

// ---------------------------------------------------------------------------
// Anvil chain harness (raw JSON-RPC; no forge binary needed at runtime)
// ---------------------------------------------------------------------------

fn rpc_raw(port: u16, payload: &Value) -> std::io::Result<Value> {
    let body = payload.to_string();
    let resp = http_request(
        port,
        "POST",
        "/",
        &[("Content-Type", "application/json".to_string())],
        body.as_bytes(),
        Duration::from_secs(10),
    )?;
    let v: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| std::io::Error::other(format!("bad rpc json: {e}")))?;
    Ok(v)
}

fn rpc(port: u16, method: &str, params: Value) -> Value {
    let v = rpc_raw(
        port,
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
    )
    .unwrap_or_else(|e| panic!("rpc {method} transport: {e}"));
    if let Some(err) = v.get("error") {
        panic!("rpc {method} error: {err}");
    }
    json_get(&v, "result")
}

struct Anvil {
    child: Child,
    port: u16,
    contract: String,
    #[allow(dead_code)]
    log: PathBuf,
}

impl Anvil {
    fn start(dir: &TestDir, genesis_ts_secs: Option<u64>) -> Self {
        let port = free_port();
        let log = dir.join("anvil.log");
        let stdout = fs::File::create(&log).expect("anvil log");
        let stderr = stdout.try_clone().expect("anvil log dup");
        let mut cmd = Command::new("anvil");
        cmd.arg("--silent")
            .arg("--port")
            .arg(port.to_string())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(ts) = genesis_ts_secs {
            cmd.arg("--timestamp").arg(ts.to_string());
        }
        let child = cmd.spawn().unwrap_or_else(|e| {
            panic!("spawning anvil failed ({e}); foundry `anvil` must be on PATH")
        });
        let mut anvil = Self {
            child,
            port,
            contract: String::new(),
            log,
        };
        wait_until(
            "anvil node",
            Duration::from_secs(20),
            Duration::from_millis(100),
            || {
                if let Ok(v) = rpc_raw(
                    anvil.port,
                    &json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
                ) && v.get("result").is_some()
                {
                    return Some(());
                }
                None
            },
        )
        .unwrap_or_else(|e| {
            panic!(
                "{e}\n--- anvil.log ---\n{}",
                fs::read_to_string(&anvil.log).unwrap_or_default()
            )
        });
        anvil.contract = anvil.deploy_contract();
        anvil
    }

    fn deploy_contract(&self) -> String {
        let artifact = repo_root().join(ARTIFACT_REL);
        if !artifact.exists() {
            // Fallback: build the Foundry project once (writes contracts/out).
            let status = Command::new("forge")
                .args(["build", "-q"])
                .current_dir(repo_root().join("contracts"))
                .status()
                .expect("forge build fallback failed to spawn");
            assert!(
                status.success(),
                "forge build failed; artifact missing: {artifact:?}"
            );
        }
        let text = fs::read_to_string(&artifact).expect("read contract artifact");
        let v: Value = serde_json::from_str(&text).expect("artifact JSON");
        let bytecode = json_get(&json_get(&v, "bytecode"), "object")
            .as_str()
            .expect("artifact bytecode.object")
            .to_string();
        let tx = rpc(
            self.port,
            "eth_sendTransaction",
            json!([{
                "from": ANVIL_ACCOUNTS[0],
                "data": bytecode,
                "gas": "0x1312D00"
            }]),
        );
        let hash = tx.as_str().expect("deploy tx hash").to_string();
        let receipt = wait_until(
            "deploy receipt",
            Duration::from_secs(15),
            Duration::from_millis(50),
            || {
                let r = rpc(self.port, "eth_getTransactionReceipt", json!([hash]));
                if r.is_null() { None } else { Some(r) }
            },
        )
        .expect("deploy receipt");
        let addr = json_get(&receipt, "contractAddress")
            .as_str()
            .expect("contractAddress in receipt")
            .to_string();
        println!(
            "[harness] anvil :{} deployed SentinelAuditAnchor at {addr}",
            self.port
        );
        addr
    }

    /// ABI-encoded calldata for `beat(bytes32,uint32,uint8)`.
    fn beat_calldata(risk_hash: &str, open_positions: u32, max_tier: u8) -> String {
        let hash = risk_hash.trim_start_matches("0x");
        assert_eq!(hash.len(), 64, "risk hash must be 32 bytes");
        format!(
            "0x{BEAT_SELECTOR}{hash}{:064x}{:064x}",
            open_positions, max_tier
        )
    }

    /// Post a heartbeat from anvil account `from_idx` (the event's guardian is
    /// `msg.sender` per the contract); waits for the receipt.
    fn beat(&self, from_idx: usize, max_tier: u8, open_positions: u32) {
        let calldata = Self::beat_calldata(
            "0x2222222222222222222222222222222222222222222222222222222222222222",
            open_positions,
            max_tier,
        );
        let tx = rpc(
            self.port,
            "eth_sendTransaction",
            json!([{
                "from": ANVIL_ACCOUNTS[from_idx],
                "to": self.contract,
                "data": calldata,
                "gas": "0x30000"
            }]),
        );
        let hash = tx.as_str().expect("beat tx hash").to_string();
        wait_until(
            "beat receipt",
            Duration::from_secs(15),
            Duration::from_millis(50),
            || {
                let r = rpc(self.port, "eth_getTransactionReceipt", json!([hash]));
                if r.is_null() { None } else { Some(()) }
            },
        )
        .expect("beat receipt");
    }

    fn block_timestamp(&self) -> u64 {
        let block = rpc(self.port, "eth_getBlockByNumber", json!(["latest", false]));
        u64::from_str_radix(
            json_get(&block, "timestamp")
                .as_str()
                .expect("block timestamp")
                .trim_start_matches("0x"),
            16,
        )
        .expect("parse block ts")
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Breaker process harness
// ---------------------------------------------------------------------------

struct BreakerSpec {
    name: String,
    rpc_url: String,
    anchor: String,
    guardians: Vec<String>,
    interval_secs: u64,
    stale_mult: u64,
    fraction: String,
    max_reduce_usd: String,
    port: u16,
    arm_secret: String,
    snapshot: Option<PathBuf>,
    state: PathBuf,
    journal: PathBuf,
}

struct Breaker {
    child: Child,
    port: u16,
    out_log: PathBuf,
    err_log: PathBuf,
    state: PathBuf,
    journal: PathBuf,
}

impl Breaker {
    fn start(dir: &TestDir, spec: &BreakerSpec) -> Self {
        let out_log = dir.join(&format!("{}.out.log", spec.name));
        let err_log = dir.join(&format!("{}.err.log", spec.name));
        let stdout = fs::File::create(&out_log).expect("breaker stdout log");
        let stderr = fs::File::create(&err_log).expect("breaker stderr log");
        let bin = env!("CARGO_BIN_EXE_breaker");
        let mut cmd = Command::new(bin);
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("RUST_LOG", "info")
            .env("BREAKER_RPC_URL", &spec.rpc_url)
            .env("BREAKER_ANCHOR_ADDRESS", &spec.anchor)
            .env("BREAKER_GUARDIANS", spec.guardians.join(","))
            .env(
                "BREAKER_HEARTBEAT_INTERVAL_SECS",
                spec.interval_secs.to_string(),
            )
            .env("BREAKER_STALE_MULT", spec.stale_mult.to_string())
            .env("BREAKER_MODE", "dry_run")
            .env("BREAKER_FRACTION", &spec.fraction)
            .env("BREAKER_MAX_REDUCE_USD", &spec.max_reduce_usd)
            .env("BREAKER_PORT", spec.port.to_string())
            .env("BREAKER_ARM_SECRET", &spec.arm_secret)
            .env("BREAKER_STATE_FILE", &spec.state)
            .env("BREAKER_JOURNAL", &spec.journal)
            .current_dir(dir.path())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(snapshot) = &spec.snapshot {
            cmd.env("BREAKER_SNAPSHOT_FILE", snapshot);
        }
        let child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {bin}: {e}"));
        let mut breaker = Self {
            child,
            port: spec.port,
            out_log,
            err_log,
            state: spec.state.clone(),
            journal: spec.journal.clone(),
        };
        breaker.wait_ready();
        breaker
    }

    fn wait_ready(&mut self) {
        let started = Instant::now();
        let result = wait_until(
            "breaker HTTP ready",
            Duration::from_secs(30),
            Duration::from_millis(100),
            || {
                if let Ok(Some(code)) = self.try_exit_code() {
                    return Some(Err(format!("breaker exited early with {code:?}")));
                }
                match self.raw_status() {
                    Ok((200, _)) => Some(Ok(())),
                    _ => None,
                }
            },
        );
        match result {
            Ok(Ok(())) => println!(
                "[harness] breaker '{}' ready on :{} after {:?}",
                self.port,
                self.port,
                started.elapsed()
            ),
            Ok(Err(e)) | Err(e) => panic!("{e}\n{}", self.diagnostics()),
        }
    }

    fn try_exit_code(&mut self) -> Result<Option<std::process::ExitStatus>, ()> {
        self.child.try_wait().map_err(|_| ())
    }

    fn raw_status(&self) -> Result<(u16, Value), String> {
        match http_request(
            self.port,
            "GET",
            "/api/heartbeat-status",
            &[],
            &[],
            Duration::from_secs(5),
        ) {
            Ok(r) if r.status == 200 => match serde_json::from_slice::<Value>(&r.body) {
                Ok(v) => Ok((r.status, v)),
                Err(e) => Err(format!("status 200 but invalid JSON: {e}")),
            },
            Ok(r) => Err(format!("status code {}", r.status)),
            Err(e) => Err(format!("http error {e}")),
        }
    }

    fn status(&self) -> Value {
        match self.raw_status() {
            Ok((_, v)) => v,
            Err(e) => panic!(
                "GET /api/heartbeat-status failed: {e}\n{}",
                self.diagnostics()
            ),
        }
    }

    fn guardian_entry(&self, address: &str) -> Option<Value> {
        let s = self.status();
        s.get("guardians")?.as_array()?.iter().find_map(|g| {
            let a = g.get("address")?.as_str()?;
            if a.eq_ignore_ascii_case(address) {
                Some(g.clone())
            } else {
                None
            }
        })
    }

    /// Raw POST /breaker/trigger with explicit body bytes and optional
    /// X-Breaker-Signature header value.
    fn trigger_raw(&self, body: &[u8], signature: Option<&str>) -> (u16, String) {
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/json".to_string())];
        if let Some(sig) = signature {
            headers.push(("X-Breaker-Signature", sig.to_string()));
        }
        let r = http_request(
            self.port,
            "POST",
            "/breaker/trigger",
            &headers,
            body,
            Duration::from_secs(10),
        )
        .unwrap_or_else(|e| panic!("trigger transport error: {e}\n{}", self.diagnostics()));
        (r.status, r.body_text)
    }

    /// Signed POST with the spec body shape.
    fn trigger(
        &self,
        secret: &[u8],
        guardian: &str,
        reason: &str,
        requested_at_ms: u64,
    ) -> (u16, Value) {
        let body = format!(
            r#"{{"guardian":"{guardian}","reason":"{reason}","requested_at_ms":{requested_at_ms}}}"#
        );
        let sig = sig_header(secret, body.as_bytes());
        let (code, text) = self.trigger_raw(body.as_bytes(), Some(&sig));
        let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (code, parsed)
    }

    fn log_text(&self) -> String {
        let mut s = String::new();
        for p in [&self.out_log, &self.err_log] {
            if let Ok(t) = fs::read_to_string(p) {
                s.push_str(&t);
            }
        }
        s
    }

    fn diagnostics(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("breaker state file: {:?}\n", self.state));
        if let Ok(t) = fs::read_to_string(&self.state) {
            s.push_str(&format!("state content: {t}\n"));
        }
        s.push_str(&format!("journal path: {:?}\n", self.journal));
        if let Ok(t) = fs::read_to_string(&self.journal) {
            s.push_str(&format!("journal content:\n{t}\n"));
        }
        s.push_str("--- breaker stdout/stderr ---\n");
        s.push_str(&self.log_text());
        s
    }

    fn journal_lines(&self) -> Vec<Value> {
        match fs::read_to_string(&self.journal) {
            Ok(t) => t
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    serde_json::from_str(l)
                        .unwrap_or_else(|e| panic!("journal line not JSON ({e}): {l}"))
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn state_text(&self) -> String {
        fs::read_to_string(&self.state).unwrap_or_default()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Breaker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Shared scenario helpers
// ---------------------------------------------------------------------------

fn spec_for(
    name: &str,
    anvil: &Anvil,
    dir: &TestDir,
    guardians: &[&str],
    interval_secs: u64,
    stale_mult: u64,
) -> BreakerSpec {
    BreakerSpec {
        name: name.to_string(),
        rpc_url: format!("http://127.0.0.1:{}", anvil.port),
        anchor: anvil.contract.clone(),
        guardians: guardians.iter().map(|g| g.to_string()).collect(),
        interval_secs,
        stale_mult,
        fraction: "0.5".to_string(),
        max_reduce_usd: "1000".to_string(),
        port: free_port(),
        arm_secret: "adversarial-arm-secret".to_string(),
        snapshot: None,
        state: dir.join(&format!("{name}-state.json")),
        journal: dir.join(&format!("{name}-journal.jsonl")),
    }
}

/// Wait until the guardian is reported stale=true (bounded).
fn wait_stale(breaker: &Breaker, guardian: &str) -> Value {
    let entry = wait_until(
        "guardian stale=true",
        Duration::from_secs(70),
        Duration::from_millis(150),
        || {
            breaker
                .guardian_entry(guardian)
                .filter(|e| json_get(e, "stale").as_bool() == Some(true))
        },
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", breaker.diagnostics()));
    println!("[harness] {guardian} stale: {entry}");
    entry
}

/// Journal lines belonging to `guardian` (case-insensitive).
fn lines_for(breaker: &Breaker, guardian: &str) -> Vec<Value> {
    breaker
        .journal_lines()
        .into_iter()
        .filter(|l| {
            json_get(l, "guardian")
                .as_str()
                .unwrap_or_default()
                .eq_ignore_ascii_case(guardian)
        })
        .collect()
}

/// Response assertion for a fire attempt that races the breaker's own 5s
/// auto-fire ticker: either our trigger fired (`fired:true`), or the ticker
/// already fired the same epoch (`fired:false` with a journal line at that
/// epoch). Anything else fails.
fn assert_fired_or_auto(breaker: &Breaker, guardian: &str, code: u16, resp: &Value, tag: &str) {
    assert_eq!(code, 202, "[{tag}] expected 202, got {code}: {resp}");
    assert_eq!(resp["accepted"], json!(true), "[{tag}] accepted:{resp}");
    assert!(
        resp["epoch"].is_u64(),
        "[{tag}] epoch must be an integer: {resp}"
    );
    if resp["fired"] == json!(true) {
        return;
    }
    let epoch = resp["epoch"].as_u64().expect("epoch u64");
    let auto = lines_for(breaker, guardian)
        .iter()
        .any(|l| json_get(l, "epoch").as_u64() == Some(epoch));
    assert!(
        auto,
        "[{tag}] fired=false with no journal line at epoch {epoch}: {resp}\n{}",
        breaker.diagnostics()
    );
    println!("[{tag}] auto-fire ticker won the race; dedupe honored at epoch {epoch}");
}

// ===========================================================================
// Tests
// ===========================================================================

/// The two independent Python oracles must reproduce the SPEC §5 vector and
/// the §3 arithmetic exactly. This guards the constants embedded in this file.
#[test]
fn oracles_reproduce_spec_vector_and_arithmetic() {
    let fixtures = repo_root().join("crates/breaker/tests/fixtures");
    let py = "python3";
    let hmac = Command::new(py)
        .arg(fixtures.join("hmac_oracle.py"))
        .arg("vector")
        .output()
        .unwrap_or_else(|e| panic!("python3 unavailable for oracle cross-check: {e}"));
    let hmac_out = String::from_utf8_lossy(&hmac.stdout).to_string();
    println!("{hmac_out}");
    assert!(
        hmac.status.success(),
        "hmac_oracle.py vector failed: {hmac_out}"
    );
    assert!(
        hmac_out.contains(&format!("hmac_sha256     = {SPEC_SIG_HEX}")),
        "oracle digest does not match the frozen SPEC §5 signature"
    );

    let arith = Command::new(py)
        .arg(fixtures.join("staleness_oracle.py"))
        .arg("check")
        .output()
        .unwrap_or_else(|e| panic!("python3 unavailable for oracle cross-check: {e}"));
    let arith_out = String::from_utf8_lossy(&arith.stdout).to_string();
    println!("{arith_out}");
    assert!(
        arith.status.success(),
        "staleness_oracle.py check failed:\n{arith_out}"
    );
    assert!(
        arith_out.contains("22/22 checks pass"),
        "arith oracle check count moved"
    );

    // Our in-test HMAC must agree with the independent Python oracle on a
    // second, differently-shaped body (cross-implementation agreement).
    let body2 = br#"{"guardian":"0x70997970C51812dc3A010C7d01b50e0d17dc79C8","reason":"oracle-cross","requested_at_ms":1791200000001}"#;
    let ours = hmac_sha256_hex(SPEC_SECRET, body2);
    let theirs = Command::new(py)
        .arg(fixtures.join("hmac_oracle.py"))
        .arg("sign-arg")
        .arg(hex::encode(SPEC_SECRET))
        .arg(std::str::from_utf8(body2).expect("utf8 body"))
        .output()
        .expect("run hmac oracle");
    let theirs_out = String::from_utf8_lossy(&theirs.stdout).to_string();
    assert_eq!(
        format!("sha256={ours}"),
        theirs_out.trim(),
        "in-test HMAC disagrees with the independent Python oracle"
    );
}

/// SPEC §5: valid signature on the frozen vector -> 202 accepted on a stale
/// guardian; the fire path runs (journal line) and the alert literal appears.
/// The chain is started with a genesis timestamp well before the vector's
/// `requested_at_ms` so the heartbeat age is beyond 3x in every reading.
#[test]
fn spec_vector_accepted_on_stale_guardian_and_fires() {
    let dir = TestDir::new("p14_spec_vector");
    let anvil = Anvil::start(&dir, Some(SPEC_REQUESTED_AT_MS / 1000 - 600));
    anvil.beat(0, 2, 1); // guardian = anvil #0 = the vector's address; Orange.
    let mut spec = spec_for("spec-vector", &anvil, &dir, &[ANVIL_ACCOUNTS[0]], 1, 3);
    // The frozen §5 signature is HMAC over the vector body with the §5 secret;
    // the breaker must be configured with exactly that secret.
    spec.arm_secret = "spec-test-secret".to_string();
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[0]);

    // Harness self-check: the chain's newest block is well older than the
    // vector's requested_at_ms, so the guardian is stale under every reading.
    let block_ts = anvil.block_timestamp();
    assert!(
        block_ts + 60 < SPEC_REQUESTED_AT_MS / 1000,
        "anvil block ts {block_ts} must predate the vector"
    );

    let (code, raw_body) = breaker.trigger_raw(
        SPEC_BODY.as_bytes(),
        Some(&format!("sha256={SPEC_SIG_HEX}")),
    );
    println!("[spec-vector] HTTP {code}: {raw_body}");
    assert_eq!(
        code, 202,
        "valid signature on stale guardian must be accepted"
    );
    let body: Value = serde_json::from_str(&raw_body).expect("trigger response is JSON");
    assert_eq!(body["accepted"], json!(true));
    assert!(
        body["epoch"].is_u64(),
        "epoch must be an integer, got {body}"
    );
    assert_eq!(body["fired"], json!(true), "stale critical guardian fires");

    let lines = breaker.journal_lines();
    assert_eq!(
        lines.len(),
        1,
        "one fire must produce exactly one journal line"
    );
    let text = breaker.log_text();
    assert!(
        text.contains("BREAKER: Sentinel unresponsive"),
        "alert literal must appear in the process log (no telegram configured):\n{text}"
    );
}

/// SPEC §5: wrong secret -> 401; the correctly signed retry then succeeds.
#[test]
fn wrong_secret_rejected_with_401() {
    let dir = TestDir::new("p14_wrong_key");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(1, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("wrong-key", &anvil, &dir, &[ANVIL_ACCOUNTS[1]], 1, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[1]);

    let body = format!(
        r#"{{"guardian":"{}","reason":"wrong-key","requested_at_ms":{}}}"#,
        ANVIL_ACCOUNTS[1],
        now_ms()
    );
    let bad_sig = sig_header(b"not-the-arm-secret", body.as_bytes());
    let (code, text) = breaker.trigger_raw(body.as_bytes(), Some(&bad_sig));
    println!("[wrong-key] bad sig -> HTTP {code}: {text}");
    assert_eq!(code, 401, "wrong secret must be rejected with 401");
    assert!(breaker.journal_lines().is_empty(), "401 must not fire");

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[1], "correct-key", now_ms());
    println!("[wrong-key] correct sig -> HTTP {code}: {body}");
    assert_eq!(code, 202, "correct signature must be accepted");
    assert_eq!(
        body["fired"],
        json!(true),
        "stale critical guardian must fire"
    );
    assert_eq!(breaker.journal_lines().len(), 1);
}

/// SPEC §5: signature over different bytes (tampered body) -> 401; missing or
/// malformed signature header -> 401; the exact signed body then succeeds.
#[test]
fn tampered_body_and_missing_signature_rejected_with_401() {
    let dir = TestDir::new("p14_tampered");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(2, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("tampered", &anvil, &dir, &[ANVIL_ACCOUNTS[2]], 1, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[2]);

    let signed_body = format!(
        r#"{{"guardian":"{}","reason":"original","requested_at_ms":{}}}"#,
        ANVIL_ACCOUNTS[2],
        now_ms()
    );
    let sig = sig_header(ARM_SECRET, signed_body.as_bytes());
    let tampered = signed_body.replace("original", "tampered!");

    let (code, text) = breaker.trigger_raw(tampered.as_bytes(), Some(&sig));
    println!("[tampered] HTTP {code}: {text}");
    assert_eq!(code, 401, "signature over different bytes must be rejected");

    let (code, text) = breaker.trigger_raw(signed_body.as_bytes(), None);
    println!("[missing-sig] HTTP {code}: {text}");
    assert_eq!(code, 401, "missing signature header must be rejected");

    let (code, text) = breaker.trigger_raw(signed_body.as_bytes(), Some("sha256=zz-not-hex"));
    println!("[malformed-sig] HTTP {code}: {text}");
    assert_eq!(code, 401, "malformed signature header must be rejected");

    assert!(
        breaker.journal_lines().is_empty(),
        "rejected requests must not fire"
    );

    let (code, body) = breaker.trigger_raw(signed_body.as_bytes(), Some(&sig));
    println!("[tampered] exact signed body -> HTTP {code}: {body}");
    assert_eq!(code, 202, "the exact signed body must be accepted");
}

/// SPEC §5: body that is not the exact JSON shape -> 400, even when the
/// signature itself is computed correctly over the raw bytes.
#[test]
fn malformed_body_rejected_with_400() {
    let dir = TestDir::new("p14_bad_body");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(3, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("bad-body", &anvil, &dir, &[ANVIL_ACCOUNTS[3]], 1, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[3]);

    let not_json = b"this is not json";
    let sig = sig_header(ARM_SECRET, not_json);
    let (code, text) = breaker.trigger_raw(not_json, Some(&sig));
    println!("[bad-body] non-JSON -> HTTP {code}: {text}");
    assert_eq!(code, 400, "non-JSON body must be 400");

    let missing_field = br#"{"reason":"no guardian field","requested_at_ms":1}"#;
    let sig = sig_header(ARM_SECRET, missing_field);
    let (code, text) = breaker.trigger_raw(missing_field, Some(&sig));
    println!("[bad-body] missing field -> HTTP {code}: {text}");
    assert_eq!(code, 400, "structurally invalid body must be 400");

    assert!(breaker.journal_lines().is_empty(), "400s must not fire");
}

/// SPEC §5: 423 when the guardian is fresh (stale=false), even with a valid
/// signature. interval=10 makes the freshness window (30s) longer than the
/// watcher's 15s poll period, so a beat is observed fresh deterministically.
#[test]
fn fresh_guardian_rejected_with_423() {
    let dir = TestDir::new("p14_fresh_423");
    let anvil = Anvil::start(&dir, None);
    anvil.beat(4, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("fresh", &anvil, &dir, &[ANVIL_ACCOUNTS[4]], 10, 3),
    );

    // Re-beat until the status endpoint reports the guardian fresh.
    let mut fresh = false;
    for _ in 0..6 {
        anvil.beat(4, 2, 1);
        let observed = wait_until(
            "fresh after re-beat",
            Duration::from_secs(17),
            Duration::from_millis(150),
            || {
                breaker
                    .guardian_entry(ANVIL_ACCOUNTS[4])
                    .filter(|e| json_get(e, "stale").as_bool() == Some(false))
            },
        );
        if observed.is_ok() {
            fresh = true;
            break;
        }
    }
    assert!(
        fresh,
        "could not observe a fresh guardian\n{}",
        breaker.diagnostics()
    );

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[4], "fresh-check", now_ms());
    println!("[fresh-423] HTTP {code}: {body}");
    assert_eq!(
        code, 423,
        "fresh guardian must be rejected with 423 (no fire)"
    );
    assert!(breaker.journal_lines().is_empty(), "423 must not fire");
}

/// SPEC §3: `stale <=> age > stale_mult * interval` (STRICT >). With
/// interval=1 and stale_mult=3 the boundary is 3.000s: no staleness at or
/// below 3x; stale by 3x+1s; the observed transition must fall inside
/// [3x, 3x+1s]. Elapsed time is measured against the exposed `last_ts_ms`.
#[test]
fn staleness_boundary_exact_3x_not_stale_3x_plus_1_stale() {
    let dir = TestDir::new("p14_boundary");
    let anvil = Anvil::start(&dir, None);
    anvil.beat(5, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("boundary", &anvil, &dir, &[ANVIL_ACCOUNTS[5]], 1, 3),
    );

    // Wait until the heartbeat is observed (last_ts_ms > 0).
    wait_until(
        "heartbeat observed",
        Duration::from_secs(45),
        Duration::from_millis(100),
        || {
            breaker
                .guardian_entry(ANVIL_ACCOUNTS[5])
                .filter(|e| json_get(e, "last_ts_ms").as_u64().unwrap_or(0) > 0)
        },
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", breaker.diagnostics()));

    // Sample live (age_secs, stale) pairs. Each sample's elapsed is computed
    // from that same response's last_ts_ms (fresh anchor per sample).
    let mut samples: Vec<(u64, f64, bool, bool)> = Vec::new(); // elapsed_ms, age_secs, stale, critical
    let sampling_start = Instant::now();
    while sampling_start.elapsed() < Duration::from_millis(7000) {
        if let Some(e) = breaker.guardian_entry(ANVIL_ACCOUNTS[5])
            && let Some(last_ts) = json_get(&e, "last_ts_ms").as_u64()
        {
            let age = as_num(&json_get(&e, "age_secs"));
            let stale = json_get(&e, "stale").as_bool().unwrap_or(false);
            let critical = json_get(&e, "critical").as_bool().unwrap_or(false);
            samples.push((now_ms().saturating_sub(last_ts), age, stale, critical));
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    for (elapsed, age, stale, critical) in &samples {
        println!(
            "[boundary] elapsed_ms={elapsed} age_secs={age} stale={stale} critical={critical}"
        );
    }
    assert!(!samples.is_empty(), "no samples collected");

    // (1) No staleness at or below the 3x boundary (catches >= instead of >).
    for (elapsed, age, stale, _) in &samples {
        if *elapsed <= 2950 {
            assert!(
                !stale,
                "stale=true at elapsed {elapsed}ms (<= 3x); age_secs={age}"
            );
        }
    }
    // (2) Stale by 3x+1s at the latest (50ms slack at the exact boundary).
    for (elapsed, age, stale, _) in &samples {
        if *elapsed >= 4050 {
            assert!(
                stale,
                "stale=false at elapsed {elapsed}ms (>= 3x+1s); age_secs={age}"
            );
        }
    }
    // (3) The transition happened inside the sampling window, near the 3x mark.
    let first_stale = samples.iter().find(|(_, _, stale, _)| *stale);
    assert!(
        first_stale.is_some(),
        "guardian never went stale within the sampling window; last sample {:?}\n{}",
        samples.last(),
        breaker.diagnostics()
    );
    let first_elapsed = first_stale.map(|(e, ..)| *e).unwrap_or_default();
    assert!(
        first_elapsed <= 5500,
        "first stale at {first_elapsed}ms; expected shortly after the 3x boundary"
    );
    // (4) The frozen equivalence holds on every reported pair:
    //     stale <=> age_secs > stale_mult * interval (interval=1, mult=3).
    for (elapsed, age, stale, _) in &samples {
        assert_eq!(
            *stale,
            *age > 3.0,
            "pair violates stale <=> age_secs > 3: age={age} stale={stale} (elapsed {elapsed}ms)"
        );
    }
    // (5) Both boundary readings observed: 3x exactly NOT stale, 3x+1s stale.
    assert!(
        samples
            .iter()
            .any(|(_, age, stale, _)| *age == 3.0 && !*stale),
        "no sample observed at age_secs == 3 with stale=false (3x exactly must not be stale)"
    );
    assert!(
        samples
            .iter()
            .any(|(_, age, stale, _)| *age >= 4.0 && *stale),
        "no sample observed with age_secs >= 4 and stale=true (3x+1s must be stale)"
    );
    // (6) Reported age_secs must track wall elapsed within a second.
    for (elapsed, age, _, _) in &samples {
        let delta = (*elapsed as f64 / 1000.0 - age).abs();
        assert!(
            delta < 1.6,
            "age_secs {age} inconsistent with elapsed {elapsed}ms"
        );
    }
}

/// SPEC §5 status contract: guardians[] entries carry address/last_ts_ms/
/// age_secs/max_tier/stale/critical/armed with correct JSON types, and the
/// values agree with the §3 arithmetic (critical == tier>=2; armed flips with
/// staleness; age tracks now - last_ts_ms).
#[test]
fn status_contract_types_and_age_arithmetic() {
    let dir = TestDir::new("p14_status_contract");
    let anvil = Anvil::start(&dir, None);
    anvil.beat(6, 3, 3); // Red
    let breaker = Breaker::start(
        &dir,
        &spec_for("status", &anvil, &dir, &[ANVIL_ACCOUNTS[6]], 1, 3),
    );

    let entry = wait_until(
        "guardian entry",
        Duration::from_secs(45),
        Duration::from_millis(100),
        || breaker.guardian_entry(ANVIL_ACCOUNTS[6]),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", breaker.diagnostics()));
    println!("[status] entry: {entry}");

    let status = breaker.status();
    let generated = json_get(&status, "generated_at_ms");
    assert!(
        generated.is_u64(),
        "generated_at_ms must be an integer: {generated}"
    );
    assert!(
        now_ms().saturating_sub(generated.as_u64().expect("u64")) < 5_000,
        "generated_at_ms must be recent: {generated}"
    );
    assert!(status["guardians"].is_array(), "guardians must be an array");

    let address = json_get(&entry, "address");
    assert!(address.is_string(), "address must be a string");
    let addr = address.as_str().expect("str");
    assert!(
        ANVIL_ACCOUNTS.contains(&addr.to_ascii_lowercase().as_str()),
        "address must round-trip the configured guardian, got {addr}"
    );
    let last_ts = json_get(&entry, "last_ts_ms");
    assert!(last_ts.is_u64(), "last_ts_ms must be an integer: {last_ts}");
    assert!(last_ts.as_u64().expect("u64") > 0, "last_ts_ms must be set");
    let age = json_get(&entry, "age_secs");
    assert!(age.is_number(), "age_secs must be a number: {age}");
    let max_tier = json_get(&entry, "max_tier");
    assert!(max_tier.is_u64(), "max_tier must be an integer: {max_tier}");
    assert_eq!(
        max_tier.as_u64().expect("u64"),
        3,
        "beat carried maxTier=3 (Red)"
    );
    for flag in ["stale", "critical", "armed"] {
        assert!(
            json_get(&entry, flag).is_boolean(),
            "{flag} must be a boolean"
        );
    }
    assert_eq!(
        json_get(&entry, "critical"),
        json!(true),
        "tier 3 => critical"
    );
    // age_secs ≈ (now - last_ts_ms)/1000 (allow 1.6s clock/rounding slack).
    let age_expected = now_ms().saturating_sub(last_ts.as_u64().expect("u64")) as f64 / 1000.0;
    assert!(
        (as_num(&age) - age_expected).abs() < 1.6,
        "age_secs {age} vs derived {age_expected}"
    );
    // While fresh: stale=false; critical=true; armed=false (would not fire
    // now, not yet fired this epoch).
    if json_get(&entry, "stale").as_bool() == Some(false) {
        assert_eq!(
            json_get(&entry, "armed"),
            json!(false),
            "fresh => not armed"
        );
    }
    // Once stale (=3x+), the Red guardian is armed (would fire now).
    let stale_entry = wait_stale(&breaker, ANVIL_ACCOUNTS[6]);
    assert_eq!(
        json_get(&stale_entry, "critical"),
        json!(true),
        "stale tier3 stays critical"
    );
    assert_eq!(
        json_get(&stale_entry, "armed"),
        json!(true),
        "stale+critical => armed (would fire now / already fired)"
    );
}

/// SPEC §3 tier mapping round trip: Green=0 .. Red=3 via maxTier in the beat
/// events; critical == (tier >= 2); armed == (tier >= 2) when stale. The
/// automatic fire path must only fire the critical guardians (the 5s auto
/// ticker is observed adding journal lines for Orange/Red and never for
/// Green/Yellow).
#[test]
fn tier_mapping_green_to_red_roundtrip_and_fire_matrix() {
    let dir = TestDir::new("p14_tiers");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    for (idx, tier) in [(1usize, 0u8), (2, 1), (3, 2), (4, 3)] {
        anvil.beat(idx, tier, 1);
    }
    let guardians: Vec<&str> = vec![
        ANVIL_ACCOUNTS[1],
        ANVIL_ACCOUNTS[2],
        ANVIL_ACCOUNTS[3],
        ANVIL_ACCOUNTS[4],
    ];
    let breaker = Breaker::start(&dir, &spec_for("tiers", &anvil, &dir, &guardians, 1, 3));

    for (idx, tier) in [(1usize, 0u64), (2, 1), (3, 2), (4, 3)] {
        let addr = ANVIL_ACCOUNTS[idx];
        wait_stale(&breaker, addr);
        let entry = breaker.guardian_entry(addr).expect("entry");
        assert_eq!(
            json_get(&entry, "max_tier").as_u64(),
            Some(tier),
            "tier {tier} round trip"
        );
        assert_eq!(
            json_get(&entry, "critical").as_bool(),
            Some(tier >= 2),
            "critical must be tier>=2 for tier {tier}"
        );
        assert_eq!(
            json_get(&entry, "armed").as_bool(),
            Some(tier >= 2),
            "armed (would fire now) for stale tier {tier}"
        );
    }

    // Manual triggers for the critical guardians only (this test covers the
    // criticality matrix; non-critical manual-trigger behaviour is asserted
    // separately in `stale_yellow_and_green_manual_trigger_must_not_order`,
    // and autonomous §7 behaviour in `stale_critical_guardian_fires_autonomously`).
    for idx in [3usize, 4] {
        let addr = ANVIL_ACCOUNTS[idx];
        let (code, body) = breaker.trigger(ARM_SECRET, addr, "tier-matrix", now_ms());
        println!("[tiers] tier {} -> HTTP {code}: {body}", idx - 1);
        assert_fired_or_auto(&breaker, addr, code, &body, "tier-matrix");
    }

    let lines = breaker.journal_lines();
    let fired_guardians: Vec<String> = lines
        .iter()
        .map(|l| {
            json_get(l, "guardian")
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
        })
        .collect();
    println!("[tiers] journal guardians: {fired_guardians:?}");
    assert!(
        !fired_guardians.contains(&ANVIL_ACCOUNTS[1].to_string())
            && !fired_guardians.contains(&ANVIL_ACCOUNTS[2].to_string()),
        "Green/Yellow must never fire (auto or manual in this test): {fired_guardians:?}"
    );
    // One fire per (guardian, epoch) for the critical guardians: at most one
    // journal line per epoch (holds whether or not the auto ticker also fires).
    for addr in [ANVIL_ACCOUNTS[3], ANVIL_ACCOUNTS[4]] {
        let addr_lines = lines_for(&breaker, addr);
        assert!(
            !addr_lines.is_empty(),
            "critical guardian {addr} must have fired (manual trigger)"
        );
        let mut epochs: Vec<u64> = addr_lines
            .iter()
            .filter_map(|l| json_get(l, "epoch").as_u64())
            .collect();
        epochs.sort_unstable();
        let before = epochs.len();
        epochs.dedup();
        assert_eq!(
            before,
            epochs.len(),
            "duplicate epoch fire for {addr}: {addr_lines:?}"
        );
    }
}

/// SPEC §7 (demo premise) / §5 "armed": after the daemon's heartbeats go
/// stale, the breaker must fire ON ITS OWN — no /breaker/trigger POST — and
/// record the fire in the journal, with the alert literal in the log. This is
/// exactly what `scripts/breaker-demo.sh` step (6) waits for ("poll until
/// stale (>24 s) -> fired -> journal line + alert printed"). Cadence mirrors
/// the demo: interval 8s / stale_mult 3 (stale at >24s).
#[test]
fn stale_critical_guardian_fires_autonomously() {
    let dir = TestDir::new("p14_autofire");
    let anvil = Anvil::start(&dir, None);
    anvil.beat(3, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("autofire", &anvil, &dir, &[ANVIL_ACCOUNTS[3]], 8, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[3]);

    let fired = wait_until(
        "autonomous fire after staleness (this test NEVER posts a trigger)",
        Duration::from_secs(90),
        Duration::from_millis(250),
        || {
            let lines = lines_for(&breaker, ANVIL_ACCOUNTS[3]);
            if lines.is_empty() { None } else { Some(lines) }
        },
    );
    match fired {
        Ok(lines) => {
            println!("[autofire] autonomous fire observed: {lines:?}");
            assert!(
                breaker
                    .log_text()
                    .contains("BREAKER: Sentinel unresponsive"),
                "autonomous fire must emit the alert literal:\n{}",
                breaker.log_text()
            );
        }
        Err(e) => panic!(
            "{e}\nno autonomous fire within 90s of staleness; SPEC §7 step (6) expects the breaker to fire by itself\n{}",
            breaker.diagnostics()
        ),
    }
}

/// SPEC §3/§5 idempotency at a wide epoch bucket (interval=60, stale_mult=1):
/// at most one fire per (guardian, epoch) — the second trigger in the same
/// epoch returns 202 with fired=false and produces no second journal line;
/// the fired epoch is persisted to BREAKER_STATE_FILE.
#[test]
fn duplicate_epoch_single_fire_202_fired_false() {
    let dir = TestDir::new("p14_dup_epoch");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(5, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("dup", &anvil, &dir, &[ANVIL_ACCOUNTS[5]], 60, 1),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[5]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[5], "first", now_ms());
    println!("[dup] first -> HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[5], code, &body, "dup-first");
    let epoch = body["epoch"].as_u64().expect("epoch u64");

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[5], "duplicate", now_ms());
    println!("[dup] second -> HTTP {code}: {body}");
    assert_eq!(code, 202, "duplicate epoch is still accepted");
    assert_eq!(body["accepted"], json!(true));
    assert_eq!(
        body["epoch"].as_u64(),
        Some(epoch),
        "both triggers must land in the same epoch bucket"
    );
    assert_eq!(
        body["fired"],
        json!(false),
        "duplicate epoch must not fire twice"
    );

    // Exactly one journal line for this (guardian, epoch) — and it survives a
    // full auto-fire tick cycle (the ticker must skip the fired epoch).
    let count_for_epoch = |epoch: u64| {
        lines_for(&breaker, ANVIL_ACCOUNTS[5])
            .iter()
            .filter(|l| json_get(l, "epoch").as_u64() == Some(epoch))
            .count()
    };
    assert_eq!(count_for_epoch(epoch), 1, "exactly one fire for the epoch");
    std::thread::sleep(Duration::from_millis(6000));
    assert_eq!(
        count_for_epoch(epoch),
        1,
        "still one fire for the epoch after a ticker cycle\n{}",
        breaker.diagnostics()
    );

    // Persistence: the state file records the fired epoch for this guardian.
    let state_text = breaker.state_text();
    println!("[dup] state: {state_text}");
    assert!(
        !state_text.trim().is_empty(),
        "state file must be persisted"
    );
    let state: Value = serde_json::from_str(&state_text).expect("state is JSON");
    let gstate = state
        .get("guardians")
        .and_then(|g| g.as_object())
        .and_then(|map| {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(ANVIL_ACCOUNTS[5]))
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| panic!("state must include the guardian: {state_text}"));
    let persisted_epochs: Vec<u64> = gstate
        .as_object()
        .map(|o| o.values().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    assert!(
        persisted_epochs.contains(&epoch),
        "state must persist fired epoch {epoch}: {state_text}"
    );
}

/// SPEC §3: state persisted (tmp+rename) to BREAKER_STATE_FILE; a restart must
/// NOT re-fire in the same epoch (interval=60 keeps the bucket open; the fire
/// happens right after the breaker is ready, long before the bucket rolls).
#[test]
fn restart_with_persisted_state_no_refire_same_epoch() {
    let dir = TestDir::new("p14_restart");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(6, 2, 1);
    let mut spec = spec_for("restart", &anvil, &dir, &[ANVIL_ACCOUNTS[6]], 60, 1);
    let mut breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[6]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[6], "before-restart", now_ms());
    println!("[restart] first -> HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[6], code, &body, "restart-first");
    let epoch = body["epoch"].as_u64().expect("epoch u64");

    // Persistence evidence: the state file must exist and be non-empty.
    let state_text = breaker.state_text();
    println!("[restart] state file:\n{state_text}");
    assert!(
        !state_text.trim().is_empty(),
        "state must be persisted to BREAKER_STATE_FILE"
    );

    // Restart quickly, while still inside the same epoch bucket.
    breaker.kill();
    spec.port = free_port();
    let restarted = Breaker::start(&dir, &spec);
    let entry = restarted
        .guardian_entry(ANVIL_ACCOUNTS[6])
        .expect("entry after restart");
    println!("[restart] after restart: {entry}");
    assert_eq!(
        json_get(&entry, "stale").as_bool(),
        Some(true),
        "still stale after restart"
    );

    let (code, body) = restarted.trigger(ARM_SECRET, ANVIL_ACCOUNTS[6], "after-restart", now_ms());
    println!("[restart] after restart -> HTTP {code}: {body}");
    assert_eq!(code, 202);
    assert_eq!(
        body["epoch"].as_u64(),
        Some(epoch),
        "the restart must still be inside the same epoch bucket"
    );
    assert_eq!(
        body["fired"],
        json!(false),
        "must not re-fire in the same epoch after restart: {body}"
    );
    let line_count = lines_for(&restarted, ANVIL_ACCOUNTS[6])
        .iter()
        .filter(|l| json_get(l, "epoch").as_u64() == Some(epoch))
        .count();
    assert_eq!(
        line_count, 1,
        "no second journal line for the epoch after restart"
    );
}

/// SPEC §3: a fresh heartbeat (new last_ts) resets the epoch, so a later
/// stale period CAN fire again (interval=8, stale_mult=3; the second fire may
/// be won by the auto ticker, in which case the journal line at the new epoch
/// is the evidence the reset allowed a fire).
#[test]
fn new_heartbeat_resets_epoch_allows_refire() {
    let dir = TestDir::new("p14_refire");
    let anvil = Anvil::start(&dir, None);
    anvil.beat(7, 2, 1);
    let mut spec = spec_for("refire", &anvil, &dir, &[ANVIL_ACCOUNTS[7]], 8, 3);
    let mut breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[7]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[7], "first-fire", now_ms());
    println!("[refire] first -> HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[7], code, &body, "refire-first");
    let before = lines_for(&breaker, ANVIL_ACCOUNTS[7]).len();

    // New heartbeat; restart to force prompt re-observation by the watcher.
    anvil.beat(7, 2, 1);
    breaker.kill();
    spec.port = free_port();
    let breaker = Breaker::start(&dir, &spec);

    let fresh = wait_until(
        "fresh after new heartbeat",
        Duration::from_secs(40),
        Duration::from_millis(150),
        || {
            breaker
                .guardian_entry(ANVIL_ACCOUNTS[7])
                .filter(|e| json_get(e, "stale").as_bool() == Some(false))
        },
    );
    assert!(
        fresh.is_ok(),
        "new heartbeat must reset to fresh\n{}",
        breaker.diagnostics()
    );

    wait_stale(&breaker, ANVIL_ACCOUNTS[7]);
    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[7], "second-fire", now_ms());
    println!("[refire] second -> HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[7], code, &body, "refire-second");
    let after = lines_for(&breaker, ANVIL_ACCOUNTS[7]).len();
    assert!(
        after > before,
        "a second fire must appear in the journal after the reset (before={before}, after={after})\n{}",
        breaker.diagnostics()
    );
}

// --- snapshot / executor behaviours (§4) ----------------------------------

/// Build one snapshot Position (decimal fields as strings — the
/// `serde-with-str` encoding of `rust_decimal::Decimal`; the repo's own P14
/// fixture `tests/fixtures/p14/breaker-snapshot.json` uses the same shape).
fn snapshot_entry(
    market_id: u32,
    symbol: &str,
    size: &str,
    entry: &str,
    mark: &str,
    liq: &str,
    collateral: &str,
) -> Value {
    json!({
        "market_id": market_id,
        "symbol": symbol,
        "size": size,
        "entry_price": entry,
        "mark_price": mark,
        "liq_price": liq,
        "collateral": collateral,
        "unrealized_pnl": "0",
        "margin_ratio": null,
        "leverage": "5",
        "opened_at": null
    })
}

fn write_snapshot(dir: &TestDir, name: &str, positions: &[Value]) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        serde_json::to_string(&Value::Array(positions.to_vec())).expect("json"),
    )
    .expect("write snapshot");
    path
}

/// SPEC §4: fraction*size above BREAKER_MAX_REDUCE_USD clamps DOWN so
/// notional <= cap; the journal records the clamped order.
#[test]
fn clamp_fraction_above_max_reduce_usd() {
    let dir = TestDir::new("p14_clamp");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(8, 2, 1);
    // size 100 @ mark 50 => notional 5000; fraction 0.5 => 2500 > 1000 cap.
    let snapshot = write_snapshot(
        &dir,
        "snapshot.json",
        &[snapshot_entry(32, "ETH", "100", "50", "50", "45", "20")],
    );

    let mut spec = spec_for("clamp", &anvil, &dir, &[ANVIL_ACCOUNTS[8]], 1, 3);
    spec.snapshot = Some(snapshot);
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[8]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[8], "clamp", now_ms());
    println!("[clamp] HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[8], code, &body, "clamp");

    let lines = lines_for(&breaker, ANVIL_ACCOUNTS[8]);
    assert_eq!(lines.len(), 1, "one journal line: {lines:?}");
    let line = &lines[0];
    let size = num_or_zero(&json_get(line, "size"));
    let notional = num_or_zero(&json_get(line, "notional_usd"));
    println!("[clamp] journal size={size} notional_usd={notional}");
    assert!(
        notional > 0.0 && notional <= 1000.9,
        "notional must be clamped to BREAKER_MAX_REDUCE_USD (1000), got {notional}"
    );
    assert!(
        size > 15.0 && size < 30.0,
        "size must be clamped down from the unclamped 50 (100 * 0.5), got {size}"
    );
    assert_eq!(
        json_get(line, "fraction")
            .as_f64()
            .map(|f| (f * 10.0).round()),
        Some(5.0)
    );
    assert_eq!(json_get(line, "mode").as_str(), Some("dry_run"));
    assert_eq!(json_get(line, "market_id").as_u64(), Some(32));
}

/// SPEC §4: a reduction that quantizes below the market minimum is
/// alert-only: NO order, and the journal records the outcome. (Snapshot
/// positions use fallback market metadata `size_decimals=3, min_size=0`, so
/// the reachable below-minimum case is a size that quantizes to zero lots.)
#[test]
fn sub_minimum_reduction_is_alert_only_no_order() {
    let dir = TestDir::new("p14_min_size");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(9, 2, 1);
    // fraction × size = 0.0004 → quantizes to 0.000 (below one lot).
    let snapshot = write_snapshot(
        &dir,
        "snapshot.json",
        &[snapshot_entry(
            32, "ETH", "0.0004", "50", "50", "45", "0.02",
        )],
    );

    let mut spec = spec_for("min-size", &anvil, &dir, &[ANVIL_ACCOUNTS[9]], 1, 3);
    spec.fraction = "1".to_string();
    spec.snapshot = Some(snapshot);
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[9]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[9], "min-size", now_ms());
    println!("[min-size] HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[9], code, &body, "min-size");

    let lines = lines_for(&breaker, ANVIL_ACCOUNTS[9]);
    assert_eq!(
        lines.len(),
        1,
        "the degraded outcome must still be recorded in the journal: {lines:?}\n{}",
        breaker.diagnostics()
    );
    let line = &lines[0];
    println!("[min-size] line: {line}");
    let size = num_or_zero(&json_get(line, "size"));
    let notional = num_or_zero(&json_get(line, "notional_usd"));
    let status = json_get(line, "status")
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detail = json_get(line, "detail")
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        size == 0.0 && notional == 0.0,
        "no order may be placed below the minimum: {line}"
    );
    assert!(
        status.contains("alert")
            || detail.contains("alert")
            || detail.contains("below the minimum")
            || detail.contains("skip"),
        "journal must mark the alert-only/skip outcome: {line}"
    );
}

/// SPEC §4 snapshot degrade order: with no live Perpl source and no snapshot
/// file, the fire is alert-only with NO order (safe degrade), still journaled.
#[test]
fn no_snapshot_source_is_alert_only_no_order() {
    let dir = TestDir::new("p14_degrade_none");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(0, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("degrade", &anvil, &dir, &[ANVIL_ACCOUNTS[0]], 1, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[0]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[0], "degrade", now_ms());
    println!("[degrade] HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[0], code, &body, "degrade");

    let lines = lines_for(&breaker, ANVIL_ACCOUNTS[0]);
    assert_eq!(
        lines.len(),
        1,
        "degraded fire must be journaled: {lines:?}\n{}",
        breaker.diagnostics()
    );
    let line = &lines[0];
    println!("[degrade] line: {line}");
    let size = num_or_zero(&json_get(line, "size"));
    let notional = num_or_zero(&json_get(line, "notional_usd"));
    let status = json_get(line, "status")
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detail = json_get(line, "detail")
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        size == 0.0 && notional == 0.0,
        "safe degrade must place NO order: {line}"
    );
    assert!(
        status.contains("alert")
            || detail.contains("alert")
            || detail.contains("snapshot=none")
            || detail.contains("no snapshot"),
        "journal must mark the alert-only outcome: {line}"
    );
    assert!(
        breaker
            .log_text()
            .contains("BREAKER: Sentinel unresponsive"),
        "alert literal logged"
    );
}

/// SPEC §4: when the snapshot file is present, the fire uses it and picks the
/// riskiest position = smallest distance_to_liq_pct.
#[test]
fn snapshot_file_picks_riskiest_position_by_distance() {
    let dir = TestDir::new("p14_riskiest");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(1, 2, 1);
    // distances = |mark - liq|/mark*100 with mark 100: 5%, 10%, 2%.
    let snapshot = write_snapshot(
        &dir,
        "snapshot.json",
        &[
            snapshot_entry(16, "BTC", "10", "100", "100", "95", "5"),
            snapshot_entry(8, "SOL", "1", "100", "100", "90", "1"),
            snapshot_entry(32, "ETH", "4", "100", "100", "98", "1"),
        ],
    );

    let mut spec = spec_for("riskiest", &anvil, &dir, &[ANVIL_ACCOUNTS[1]], 1, 3);
    spec.snapshot = Some(snapshot);
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[1]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[1], "riskiest", now_ms());
    println!("[riskiest] HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[1], code, &body, "riskiest");

    let lines = lines_for(&breaker, ANVIL_ACCOUNTS[1]);
    assert_eq!(
        lines.len(),
        1,
        "journal: {lines:?}\n{}",
        breaker.diagnostics()
    );
    let line = &lines[0];
    println!("[riskiest] line: {line}");
    assert_eq!(
        json_get(line, "market_id").as_u64(),
        Some(32),
        "the 2% distance position (market 32) is the riskiest: {line}"
    );
    assert!(
        num_or_zero(&json_get(line, "size")) > 0.0,
        "order was attempted with a positive size: {line}"
    );
}

/// SPEC §4: equal distance_to_liq_pct ties break toward the larger |size|*mark
/// notional.
#[test]
fn snapshot_riskiest_tie_breaks_toward_larger_notional() {
    let dir = TestDir::new("p14_tie");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(2, 2, 1);
    // Both 5% distance (|100-95|/100, |100-105|/100); notional 1000 vs 3000.
    let snapshot = write_snapshot(
        &dir,
        "snapshot.json",
        &[
            snapshot_entry(16, "BTC", "10", "100", "100", "95", "5"),
            snapshot_entry(32, "ETH", "30", "100", "100", "105", "3"),
        ],
    );

    let mut spec = spec_for("tie", &anvil, &dir, &[ANVIL_ACCOUNTS[2]], 1, 3);
    spec.snapshot = Some(snapshot);
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[2]);

    let (code, body) = breaker.trigger(ARM_SECRET, ANVIL_ACCOUNTS[2], "tie", now_ms());
    println!("[tie] HTTP {code}: {body}");
    assert_fired_or_auto(&breaker, ANVIL_ACCOUNTS[2], code, &body, "tie");

    let lines = lines_for(&breaker, ANVIL_ACCOUNTS[2]);
    assert_eq!(
        lines.len(),
        1,
        "journal: {lines:?}\n{}",
        breaker.diagnostics()
    );
    let line = &lines[0];
    println!("[tie] line: {line}");
    assert_eq!(
        json_get(line, "market_id").as_u64(),
        Some(32),
        "distance tie must break toward the larger notional (market 32): {line}"
    );
}

/// Defensive check (spec silent): a trigger for a guardian that is NOT in
/// BREAKER_GUARDIANS must never execute an order or journal a fire.
#[test]
fn unknown_guardian_trigger_does_not_fire() {
    let dir = TestDir::new("p14_unknown");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(3, 2, 1);
    let breaker = Breaker::start(
        &dir,
        &spec_for("unknown", &anvil, &dir, &[ANVIL_ACCOUNTS[3]], 1, 3),
    );
    wait_stale(&breaker, ANVIL_ACCOUNTS[3]);

    let stranger = ANVIL_ACCOUNTS[7];
    let (code, body) = breaker.trigger(ARM_SECRET, stranger, "stranger", now_ms());
    println!("[unknown] HTTP {code}: {body}");
    assert!(
        (400..500).contains(&code),
        "unknown guardian should be rejected (observed 400), got {code}: {body}"
    );
    assert!(
        lines_for(&breaker, stranger).is_empty(),
        "unknown guardian must not be journaled"
    );
}

/// SPEC §3 vs §5 (adversarial edge): `Fire = stale AND critical` is frozen,
/// yet §5 only freezes the fresh->423 guard for /breaker/trigger. A stale
/// Yellow/Green guardian must not produce a defensive order: the automatic
/// path refuses non-critical guardians, and any manual trigger must not
/// execute a reduce order for them. This test encodes the strict §3 reading;
/// a failure flags the trigger-gating question for adjudication (see
/// docs/evidence/p14-validate.txt).
#[test]
fn stale_yellow_and_green_manual_trigger_must_not_order() {
    let dir = TestDir::new("p14_yellow_order");
    let anvil = Anvil::start(&dir, Some(backdated_genesis()));
    anvil.beat(1, 1, 1); // Yellow
    anvil.beat(2, 0, 1); // Green
    let snapshot = write_snapshot(
        &dir,
        "snapshot.json",
        &[snapshot_entry(32, "ETH", "10", "100", "100", "98", "3")],
    );
    let mut spec = spec_for(
        "yellow-order",
        &anvil,
        &dir,
        &[ANVIL_ACCOUNTS[1], ANVIL_ACCOUNTS[2]],
        1,
        3,
    );
    spec.snapshot = Some(snapshot);
    let breaker = Breaker::start(&dir, &spec);
    wait_stale(&breaker, ANVIL_ACCOUNTS[1]);
    wait_stale(&breaker, ANVIL_ACCOUNTS[2]);

    for addr in [ANVIL_ACCOUNTS[1], ANVIL_ACCOUNTS[2]] {
        let (code, body) = breaker.trigger(ARM_SECRET, addr, "non-critical-probe", now_ms());
        println!("[yellow-order] {addr} -> HTTP {code}: {body}");
        assert!(
            !(code == 202 && body["fired"] == json!(true)),
            "Fire = stale AND critical (§3): a non-critical guardian must not fire; got {code}: {body}"
        );
        let executed: Vec<Value> = lines_for(&breaker, addr)
            .into_iter()
            .filter(|l| num_or_zero(&json_get(l, "size")) > 0.0)
            .collect();
        assert!(
            executed.is_empty(),
            "no reduce order may be executed for a stale non-critical guardian; journal: {executed:?}"
        );
    }
}

/// Pin the ABI constants used by this harness against keccak, so the harness
/// itself cannot silently drift from the contract's event/function shapes.
#[test]
fn abi_constants_match_keccak_derivations() {
    use alloy::primitives::keccak256;
    let topic = keccak256(b"Heartbeat(address,bytes32,uint32,uint8)");
    assert_eq!(format!("0x{}", hex::encode(topic)), HEARTBEAT_TOPIC);
    let selector = keccak256(b"beat(bytes32,uint32,uint8)");
    assert_eq!(hex::encode(&selector[..4]), BEAT_SELECTOR);
}
