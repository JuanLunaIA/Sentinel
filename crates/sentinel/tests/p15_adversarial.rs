//! P15 dashboard adversarial verification — independent black-box corpus (`SPEC-P15.md`
//! §1–§4).
//!
//! Written from `SPEC-P15.md` only: the API implementation
//! (`crates/sentinel/src/api.rs`) and the dashboard artifact are treated as black
//! boxes. The router is exercised over a real localhost listener
//! (`health::router(...).merge(api::dashboard_router(...))`, mirroring `main.rs`);
//! `DashboardPaths` are injected so every fixture source is controlled by the test.
//!
//! Coverage map (task requirements):
//! (1) `/api/state` shape+types — empty state (no journal/live_state) and a populated
//!     mock with pinned distance/tier (25/15/8 boundaries + mark refresh + heartbeat).
//! (2) `/api/decisions` — newest-first, default 20, cap 100, null-safety.
//! (3) `/api/audit` + `/api/audit/verify` — window, cap tolerance, VerifyReport shape
//!     (good chain + tamper detected at the exact seq).
//! (4) `/api/backtest` — 404 JSON when absent, raw pass-through when present.
//! (5) `/api/nansen/spend` — zero-ledger zeros; hand-built ledger with exact
//!     totals/calls_1h/cost_24h.
//! (6) `/api/breaker-status` — `available:false` when absent; parse of realistic
//!     state + journal tail.
//! (7) pause/resume — 503 without `DASHBOARD_ADMIN_KEY`, 401 wrong key, 200 + kill
//!     `AtomicBool` flips, idempotent re-pause.
//! (8) rate limit — >120 rapid requests from one forwarded IP ⇒ 429; another IP
//!     unaffected; `/healthz` + `GET /` exempt.
//! (9) CORS — `Access-Control-Allow-Origin: *` on reads + OPTIONS preflight.
//! (10) HTML static analysis (no browser): 8 section hooks, <= 204800 bytes, no
//!     external URL outside comments, PAUSE/RESUME/admin-key flow strings, no
//!     localStorage, no `innerHTML` with template data.
//!
//! Fixtures: `tests/fixtures/p15/**`. The static hash-chained journal is written by
//! the `#[ignore]`d `generate_static_fixtures` test (run it once after checkout):
//! `cargo test -p sentinel --test p15_adversarial -- --ignored generate_static_fixtures`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use sentinel::api::{DashboardPaths, DashboardState, dashboard_router};
use sentinel::health::{HealthState, router as health_router};
use sentinel::pipeline::LiveState;
use sentinel_core::audit::{AuditEntry, AuditJournal, GENESIS_PREV_HASH, Trigger, append_line};
use sentinel_core::types::{AccountState, ExecutionMode, Market, MarketId, Position};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;

/// Demo account the fixtures journal against.
const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

/// The P15 fixture day file (all fixture timestamps are 2026-10-06 UTC).
const JOURNAL_FILE: &str = "journal-20261006.jsonl";

/// Serializes every `std::env` mutation in this binary (Rust 2024 marks
/// `set_var`/`remove_var` unsafe because the environment is process-global).
static ENV_LOCK: TokioMutex<()> = TokioMutex::const_new(());

/// Documentation-range IPs (RFC 5737) — never collide with real traffic.
const RATE_IP: &str = "203.0.113.77";
const RATE_IP_FRESH: &str = "198.51.100.42";

// ===========================================================================
// Environment helpers (guarded, restoring)
// ===========================================================================

struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: process-global env; all env mutation in this binary is
        // serialized through ENV_LOCK and restored on drop.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }

    fn unset(name: &'static str) -> Self {
        let previous = std::env::var(name).ok();
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(name) };
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            // SAFETY: see `set`.
            Some(value) => unsafe { std::env::set_var(self.name, value) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}

// ===========================================================================
// Generic fixtures / DSL
// ===========================================================================

fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap_or_else(|err| panic!("bad decimal literal {text:?}: {err}"))
}

fn dec_value(value: &Value, context: &str) -> Decimal {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("{context}: decimals must be strings, got {value}"));
    dec(text)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_millis() as u64)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/sentinel has a repo root")
        .to_path_buf()
}

/// The static fixture root (`tests/fixtures/p15` at the repo root).
fn fixtures_root() -> PathBuf {
    repo_root().join("tests/fixtures/p15")
}

/// `DashboardPaths` for a temp root: every path resolves under `root`.
fn temp_paths(root: &Path) -> DashboardPaths {
    DashboardPaths {
        audit_dir: root.join("audit"),
        backtest_report: root.join("backtest-report.json"),
        breaker_journal: root.join("breaker-journal.jsonl"),
        breaker_state: root.join("breaker-state.json"),
        heartbeat: root.join("heartbeat.json"),
        spend_ledger: root.join("nansen-spend.jsonl"),
    }
}

/// `DashboardPaths` for the committed static fixtures.
fn fixture_paths() -> DashboardPaths {
    let root = fixtures_root();
    DashboardPaths {
        audit_dir: root.join("journal"),
        backtest_report: root.join("backtest-report.json"),
        breaker_journal: root.join("breaker-journal.jsonl"),
        breaker_state: root.join("breaker-state.json"),
        heartbeat: root.join("heartbeat.json"),
        spend_ledger: root.join("nansen-spend.jsonl"),
    }
}

fn assert_keys_exact(value: &Value, expected: &[&str], context: &str) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{context}: not a JSON object: {value}"));
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want, "{context}: key set mismatch: {value}");
}

/// Assert `value` is null; else fail with the context.
fn assert_null(value: &Value, context: &str) {
    assert!(value.is_null(), "{context}: expected null, got {value}");
}

// ===========================================================================
// Journal fixtures
// ===========================================================================

fn ts_at(seq: u64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-06T02:00:00Z")
        .expect("rfc3339 literal")
        .with_timezone(&Utc)
        + TimeDelta::seconds(seq as i64)
}

/// Deterministic hash-chained journal entries. seq 2 is an all-null SYSTEM entry
/// and seq 4 a mixed null entry (null-safety fixtures); every other seq carries
/// normal decision/verdict/execution documents.
fn chain_entries(count: u64) -> Vec<AuditEntry> {
    let mut entries = Vec::with_capacity(count as usize);
    let mut prev = GENESIS_PREV_HASH.to_string();
    for seq in 0..count {
        let (trigger, market_id, decision, verdict, execution) = match seq {
            2 => (Trigger::System, None, Value::Null, Value::Null, Value::Null),
            4 => (
                Trigger::Human,
                Some(16),
                Value::Null,
                json!({"id": "v4", "verdict": "approved"}),
                json!({"id": "x4", "status": "none"}),
            ),
            s if s % 3 == 0 => (
                Trigger::Reflex,
                Some(32),
                json!({"action": "reduce", "fraction": "0.5", "id": format!("d{s}")}),
                json!({"id": format!("v{s}"), "verdict": "approved"}),
                json!({"id": format!("x{s}"), "status": "executed"}),
            ),
            s if s % 3 == 1 => (
                Trigger::Strategy,
                Some(32),
                json!({"action": "hold", "id": format!("d{s}")}),
                json!({"id": format!("v{s}"), "verdict": "none"}),
                json!({"id": format!("x{s}"), "status": "none"}),
            ),
            s => (
                Trigger::Backtest,
                Some(16),
                json!({"action": "hold", "id": format!("d{s}")}),
                json!({"id": format!("v{s}"), "verdict": "none"}),
                json!({"id": format!("x{s}"), "status": "pending"}),
            ),
        };
        let entry = AuditEntry::new(
            seq,
            ts_at(seq),
            trigger,
            ACCOUNT.to_string(),
            market_id,
            format!("{:064x}", seq.saturating_mul(7).saturating_add(1)),
            decision,
            verdict,
            execution,
            prev.clone(),
        );
        prev = entry.entry_hash.clone();
        entries.push(entry);
    }
    entries
}

/// Write a hash-chained journal file into `dir`.
fn write_chain(dir: &Path, count: u64) -> Vec<AuditEntry> {
    let entries = chain_entries(count);
    std::fs::create_dir_all(dir).expect("create journal dir");
    let path = dir.join(JOURNAL_FILE);
    for entry in &entries {
        append_line(&path, entry).expect("append journal line");
    }
    entries
}

/// Builder for the ignored static-fixture generator (keeps the committed
/// `tests/fixtures/p15/journal/journal-20261006.jsonl` reproducible).
#[test]
#[ignore = "fixture generator: run once to (re)write tests/fixtures/p15/journal/"]
fn generate_static_fixtures() {
    let dir = fixtures_root().join("journal");
    let path = dir.join(JOURNAL_FILE);
    // Regenerate from scratch so the file is byte-stable regardless of reruns.
    if path.exists() {
        std::fs::remove_file(&path).expect("remove stale fixture journal");
    }
    let entries = write_chain(&dir, 8);
    for entry in &entries {
        println!("seq {} entry_hash {}", entry.seq, entry.entry_hash);
    }
    println!("wrote {}", path.display());
}

// ===========================================================================
// Live-state fixtures (P15 §2: distance/tier against the live marks)
// ===========================================================================

fn market(id: u32, symbol: &str) -> Market {
    Market {
        id: MarketId(id),
        symbol: symbol.to_string(),
        base: symbol.to_string(),
        price_decimals: 1,
        size_decimals: 3,
        initial_margin_fraction: dec("0.08333333"),
        maintenance_margin_fraction: dec("0.05"),
        max_leverage: dec("12"),
        min_size: dec("0"),
        tick_size: dec("0.1"),
        maker_fee_micros: 0,
        taker_fee_micros: 0,
        order_ttl_blocks: 100,
    }
}

#[allow(clippy::too_many_arguments)] // fixture DSL mirrors the field set by design
fn position(
    market_id: u32,
    symbol: &str,
    size: &str,
    entry: &str,
    mark: &str,
    liq: &str,
    collateral: &str,
    upnl: &str,
    leverage: &str,
) -> Position {
    Position {
        market_id: MarketId(market_id),
        symbol: symbol.to_string(),
        size: dec(size),
        entry_price: dec(entry),
        mark_price: Some(dec(mark)),
        liq_price: Some(dec(liq)),
        collateral: dec(collateral),
        unrealized_pnl: dec(upnl),
        margin_ratio: None,
        leverage: dec(leverage),
        opened_at: None,
    }
}

fn live_state(
    markets: Vec<Market>,
    account: Option<AccountState>,
    marks: HashMap<MarketId, Decimal>,
) -> Arc<TokioMutex<LiveState>> {
    let mut state = LiveState::new();
    state.set_markets(markets);
    state.account = account;
    state.marks = marks;
    Arc::new(TokioMutex::new(state))
}

fn account(positions: Vec<Position>) -> AccountState {
    AccountState {
        positions,
        free_balance: dec("1000.25"),
        equity: dec("1015.75"),
        fee_tier: 0,
        snapshot_ts: ts_at(0),
    }
}

// ===========================================================================
// Router / server harness
// ===========================================================================

fn dashboard_state(
    health: Arc<HealthState>,
    kill: Arc<AtomicBool>,
    live: Option<Arc<TokioMutex<LiveState>>>,
    journal: Option<Arc<TokioMutex<AuditJournal>>>,
    paths: DashboardPaths,
) -> DashboardState {
    DashboardState {
        health,
        journal,
        kill,
        live_state: live,
        paths,
    }
}

struct Server {
    base: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(app: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("listener address");
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve dashboard router");
    });
    Server {
        base: format!("http://{addr}"),
        handle,
    }
}

/// Build the full app exactly like `main.rs`: health routes merged with the
/// dashboard router.
async fn serve_dashboard(
    health: Arc<HealthState>,
    kill: Arc<AtomicBool>,
    live: Option<Arc<TokioMutex<LiveState>>>,
    journal: Option<Arc<TokioMutex<AuditJournal>>>,
    paths: DashboardPaths,
) -> Server {
    let app = health_router(Arc::clone(&health)).merge(dashboard_router(dashboard_state(
        health, kill, live, journal, paths,
    )));
    serve(app).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// GET with connect retry (the listener is ready before `serve` returns, but be
/// robust to slow CI).
async fn get(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, String) {
    for attempt in 0..50 {
        match client.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return (status, body);
            }
            Err(err) if attempt < 49 => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => panic!("GET {url} never connected: {err}"),
        }
    }
    unreachable!("retry loop returns or panics")
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Value) {
    for attempt in 0..50 {
        match client.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                let body = response.text().await.unwrap_or_default();
                let value: Value = serde_json::from_str(&body).unwrap_or_else(|err| {
                    panic!("GET {url}: non-JSON body ({err}): {body:?}");
                });
                return (status, headers, value);
            }
            Err(err) if attempt < 49 => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => panic!("GET {url} never connected: {err}"),
        }
    }
    unreachable!("retry loop returns or panics")
}

// ===========================================================================
// (1) /api/state — shape + types
// ===========================================================================

#[tokio::test]
async fn api_state_empty_shape_and_types() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");

    assert_keys_exact(
        &body,
        &[
            "mode",
            "version",
            "uptime_s",
            "feed_age_s",
            "feed_fresh",
            "paused",
            "heartbeat",
            "account",
            "positions",
        ],
        "/api/state (empty)",
    );
    assert_eq!(body["mode"], "DRY_RUN");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["uptime_s"].as_u64().is_some(), "{body}");
    assert_null(&body["feed_age_s"], "feed_age_s (no feed yet)");
    assert_eq!(body["feed_fresh"], false, "{body}");
    assert_eq!(body["paused"], false, "{body}");
    assert_keys_exact(
        &body["heartbeat"],
        &["age_s", "tx_hash", "seq"],
        "heartbeat (absent file)",
    );
    assert_null(&body["heartbeat"]["age_s"], "heartbeat.age_s");
    assert_null(&body["heartbeat"]["tx_hash"], "heartbeat.tx_hash");
    assert_null(&body["heartbeat"]["seq"], "heartbeat.seq");
    assert_null(&body["account"], "account (no live state)");
    assert_eq!(
        body["positions"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
}

#[tokio::test]
async fn api_state_live_state_without_account_yields_no_positions() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::Testnet));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        Some(live_state(vec![market(32, "ETH")], None, HashMap::new())),
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_eq!(body["mode"], "TESTNET");
    assert_null(&body["account"], "account (no snapshot yet)");
    assert_eq!(
        body["positions"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
}

#[tokio::test]
async fn api_state_feed_age_and_fresh_after_touch() {
    let http = client();
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    health.touch_feed(now_ms() - 5_000);
    // feed_fresh is sourced from the live stream (the pipeline's stale flag):
    // a recent health touch alone cannot make a stream with no live state
    // "fresh" (replay/standalone), while a live stream without a stale
    // episode reports fresh.
    let live = live_state(vec![], None, HashMap::new());
    {
        let mut guard = live.lock().await;
        guard.now_ms = now_ms();
    }
    let server = serve_dashboard(
        Arc::clone(&health),
        Arc::new(AtomicBool::new(false)),
        Some(Arc::clone(&live)),
        None,
        temp_paths(dir.path()),
    )
    .await;

    let (status, _, body) = get_json(&http, &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    let age = body["feed_age_s"].as_u64().expect("feed_age_s number");
    assert!((4..=6).contains(&age), "feed_age_s ≈ 5 s, got {age}");
    assert_eq!(
        body["feed_fresh"], true,
        "live stream with no stale flag must be fresh: {body}"
    );

    // A stale episode flips freshness off.
    {
        let mut guard = live.lock().await;
        guard.stale_secs = Some(42);
    }
    let (_, _, stale) = get_json(&http, &server.url("/api/state")).await;
    assert_eq!(
        stale["feed_fresh"], false,
        "stale live state must not be fresh: {stale}"
    );

    // No live state at all ⇒ not fresh, even with a recent health touch.
    let dir2 = TempDir::new().expect("tempdir");
    let health2 = Arc::new(HealthState::new(ExecutionMode::DryRun));
    health2.touch_feed(now_ms() - 5_000);
    let server2 = serve_dashboard(
        health2,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir2.path()),
    )
    .await;
    let (_, _, body2) = get_json(&http, &server2.url("/api/state")).await;
    assert_eq!(
        body2["feed_fresh"], false,
        "no live state ⇒ not fresh: {body2}"
    );
}

#[tokio::test]
async fn api_state_paused_reflects_kill_flag() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let kill = Arc::new(AtomicBool::new(false));
    let server = serve_dashboard(
        health,
        Arc::clone(&kill),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();

    let (_, _, before) = get_json(&http, &server.url("/api/state")).await;
    assert_eq!(before["paused"], false, "{before}");

    kill.store(true, Ordering::SeqCst);
    let (_, _, paused) = get_json(&http, &server.url("/api/state")).await;
    assert_eq!(paused["paused"], true, "kill flag must surface: {paused}");
}

#[tokio::test]
async fn api_state_populated_positions_distance_tier_boundaries() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    // Four markets, all marked at 100. Exchange liquidation prices chosen so the
    // distance-to-liq lands exactly on the 25/15/8 cuts and just below 8:
    //   |100 − 75| / 100 = 25.0 → green (boundary belongs to the safer tier)
    //   |100 − 85| / 100 = 15.0 → yellow
    //   |100 − 92| / 100 =  8.0 → orange
    //   |100 − 92.5| / 100 = 7.5 → red
    let positions = vec![
        position(32, "ETH", "1", "95", "100", "75", "50", "5", "5"),
        position(33, "BTC", "2", "90", "100", "85", "60", "20", "5"),
        position(34, "SOL", "3", "88", "100", "92", "40", "36", "10"),
        position(35, "MON", "-4", "110", "100", "92.5", "30", "-40", "4"),
    ];
    let markets = vec![
        market(32, "ETH"),
        market(33, "BTC"),
        market(34, "SOL"),
        market(35, "MON"),
    ];
    let mut marks = HashMap::new();
    for id in [32_u32, 33, 34, 35] {
        marks.insert(MarketId(id), dec("100"));
    }
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        Some(live_state(markets, Some(account(positions)), marks)),
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");

    let account_json = &body["account"];
    assert_keys_exact(account_json, &["free_balance", "equity"], "account object");
    assert_eq!(
        dec_value(&account_json["free_balance"], "free_balance"),
        dec("1000.25")
    );
    assert_eq!(dec_value(&account_json["equity"], "equity"), dec("1015.75"));

    let positions_json = body["positions"].as_array().expect("positions array");
    assert_eq!(positions_json.len(), 4, "{body}");

    // (market_id, side, distance_pct, tier, liq_price)
    let expected = [
        (32_u64, "long", "25", "green", "75"),
        (33, "long", "15", "yellow", "85"),
        (34, "long", "8", "orange", "92"),
        (35, "short", "7.5", "red", "92.5"),
    ];
    for (market_id, side, distance, tier, liq) in expected {
        let found = positions_json
            .iter()
            .find(|p| p["market_id"].as_u64() == Some(market_id))
            .unwrap_or_else(|| panic!("position {market_id} missing: {body}"));
        assert_keys_exact(
            found,
            &[
                "market_id",
                "symbol",
                "side",
                "size",
                "entry_price",
                "mark_price",
                "distance_pct",
                "liq_price",
                "tier",
                "collateral",
                "unrealized_pnl",
                "leverage",
                "last_action",
            ],
            &format!("position {market_id}"),
        );
        assert_eq!(found["side"], side, "position {market_id}: {found}");
        assert_eq!(
            dec_value(&found["distance_pct"], "distance_pct"),
            dec(distance),
            "position {market_id}: pinned distance {distance}: {found}"
        );
        assert_eq!(found["tier"], tier, "position {market_id}: tier: {found}");
        assert_eq!(found["mark_price"], "100", "position {market_id}");
        assert_eq!(dec_value(&found["liq_price"], "liq_price"), dec(liq));
        assert_eq!(found["last_action"], Value::Null, "no journal ⇒ null");
    }
}

#[tokio::test]
async fn api_state_distance_uses_live_marks_not_stale_snapshot() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    // The snapshot's mark is stale (100); the live mark map says 50. Distance
    // must be recomputed against the live marks: |50 − 44| / 50 = 12 % ⇒ orange
    // (stale 100 would give 56 % ⇒ green).
    let positions = vec![position(32, "ETH", "1", "95", "100", "44", "50", "5", "5")];
    let mut marks = HashMap::new();
    marks.insert(MarketId(32), dec("50"));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        Some(live_state(
            vec![market(32, "ETH")],
            Some(account(positions)),
            marks,
        )),
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    let pos = &body["positions"][0];
    assert_eq!(
        dec_value(&pos["distance_pct"], "distance_pct"),
        dec("12"),
        "distance must be computed at the live mark (50), not the stale snapshot mark: {body}"
    );
    assert_eq!(
        pos["tier"], "orange",
        "12 % ⇒ orange per thresholds 25/15/8: {body}"
    );
}

#[tokio::test]
async fn api_state_heartbeat_from_fixture_and_live_writer_shape() {
    // Static fixture: P16 B3 writer shape {"ts_ms", "tx_hash", "seq"} must be
    // rendered as {"age_s", "tx_hash", "seq"} with age computed at wall time.
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        Arc::clone(&health),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        fixture_paths(),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/state")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    let heartbeat = &body["heartbeat"];
    assert_keys_exact(heartbeat, &["age_s", "tx_hash", "seq"], "heartbeat");
    assert_eq!(
        heartbeat["tx_hash"], "0x4f1e8a2c3b4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7",
        "tx_hash passes through: {body}"
    );
    assert_eq!(heartbeat["seq"].as_u64(), Some(123), "{body}");
    let expected_age = (now_ms().saturating_sub(1_700_000_000_000)) / 1_000;
    let age = heartbeat["age_s"].as_u64().expect("age_s number");
    assert!(
        age.abs_diff(expected_age) <= 5,
        "age_s must be (wall_now − ts_ms)/1000: got {age}, expected ~{expected_age}"
    );

    // Runtime heartbeat written 5 s ago (fresh writer output).
    let dir = TempDir::new().expect("tempdir");
    let heartbeat_path = dir.path().join("heartbeat.json");
    std::fs::write(
        &heartbeat_path,
        format!(
            "{{\"ts_ms\":{},\"tx_hash\":null,\"seq\":7}}",
            now_ms() - 5_000
        ),
    )
    .expect("write heartbeat");
    let server2 = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (_, _, body2) = get_json(&client(), &server2.url("/api/state")).await;
    let age2 = body2["heartbeat"]["age_s"].as_u64().expect("age_s number");
    assert!(
        (4..=6).contains(&age2),
        "fresh heartbeat age ≈ 5, got {age2}"
    );
    assert_eq!(body2["heartbeat"]["seq"].as_u64(), Some(7), "{body2}");
    assert_null(
        &body2["heartbeat"]["tx_hash"],
        "null tx_hash passes through",
    );
}

// ===========================================================================
// (2) /api/decisions — ordering, limits, null-safety
// ===========================================================================

fn seqs_of(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected array: {value}"))
        .iter()
        .map(|entry| {
            entry["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("seq: {entry}"))
        })
        .collect()
}

#[tokio::test]
async fn api_decisions_newest_first_default_limit_20() {
    let dir = TempDir::new().expect("tempdir");
    let entries = write_chain(dir.path(), 25);
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(dir.path()).expect("open journal"),
    ));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/decisions")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");

    let expected: Vec<u64> = (5..=24).rev().collect();
    assert_eq!(
        seqs_of(&body),
        expected,
        "default limit 20, newest first (tail of the journal): {body}"
    );
    let array = body.as_array().expect("array");
    assert_eq!(
        array[0]["entry_hash"], entries[24].entry_hash,
        "first element is the newest entry"
    );
    assert_eq!(
        array[19]["entry_hash"], entries[5].entry_hash,
        "last element is the 20th newest entry"
    );
}

#[tokio::test]
async fn api_decisions_limit_cap_100_at_1000() {
    let dir = TempDir::new().expect("tempdir");
    write_chain(dir.path(), 120);
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(dir.path()).expect("open journal"),
    ));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        temp_paths(dir.path()),
    )
    .await;
    let http = client();

    // Default: 20 newest.
    let (_, _, default_body) = get_json(&http, &server.url("/api/decisions")).await;
    assert_eq!(
        seqs_of(&default_body),
        (100..=119).rev().collect::<Vec<u64>>()
    );

    // Explicit small limit: newest 5.
    let (_, _, small) = get_json(&http, &server.url("/api/decisions?limit=5")).await;
    assert_eq!(seqs_of(&small), vec![119, 118, 117, 116, 115]);

    // Absurd limit: capped at 100, still the NEWEST 100 (not the oldest).
    let (_, _, capped) = get_json(&http, &server.url("/api/decisions?limit=1000")).await;
    assert_eq!(seqs_of(&capped), (20..=119).rev().collect::<Vec<u64>>());
}

#[tokio::test]
async fn api_decisions_null_fields_render_as_null() {
    // Static fixture chain: seq 2 is an all-null SYSTEM entry, seq 4 mixed.
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(fixtures_root().join("journal")).expect("open fixture journal"),
    ));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        fixture_paths(),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/decisions?limit=8")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");

    let array = body.as_array().expect("array");
    assert_eq!(array.len(), 8, "{body}");
    for entry in array {
        assert_keys_exact(
            entry,
            &[
                "seq",
                "ts",
                "trigger",
                "market_id",
                "decision",
                "policy_verdict",
                "execution",
                "entry_hash",
            ],
            "decision entry",
        );
        assert!(entry["entry_hash"].as_str().is_some_and(|h| h.len() == 64));
        assert!(
            entry["ts"].as_str().is_some(),
            "ts is RFC3339 string: {entry}"
        );
    }
    // Newest first: seqs 7..0.
    assert_eq!(seqs_of(&body), (0..8).rev().collect::<Vec<u64>>());

    let null_entry = array
        .iter()
        .find(|entry| entry["seq"].as_u64() == Some(2))
        .expect("seq 2 present");
    assert_eq!(null_entry["trigger"], "SYSTEM");
    assert_null(&null_entry["market_id"], "market_id");
    assert_null(&null_entry["decision"], "decision");
    assert_null(&null_entry["policy_verdict"], "policy_verdict");
    assert_null(&null_entry["execution"], "execution");

    let mixed = array
        .iter()
        .find(|entry| entry["seq"].as_u64() == Some(4))
        .expect("seq 4 present");
    assert_eq!(mixed["trigger"], "HUMAN");
    assert_null(&mixed["decision"], "mixed decision null");
    assert_eq!(mixed["policy_verdict"]["verdict"], "approved");
    assert_eq!(mixed["execution"]["status"], "none");
}

// ===========================================================================
// (3) /api/audit + /api/audit/verify — unchanged P10 behavior
// ===========================================================================

#[tokio::test]
async fn api_audit_window_limit_and_cap() {
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(fixtures_root().join("journal")).expect("open fixture journal"),
    ));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        fixture_paths(),
    )
    .await;
    let http = client();

    // Default: the whole window, ascending.
    let (status, _, all) = get_json(&http, &server.url("/api/audit")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {all}");
    assert_eq!(seqs_of(&all), (0..8).collect::<Vec<u64>>());
    let parsed: Vec<AuditEntry> =
        serde_json::from_value(all.clone()).expect("entries are full AuditEntry JSON");
    assert_eq!(parsed[0].prev_hash, GENESIS_PREV_HASH);

    // limit.
    let (_, _, two) = get_json(&http, &server.url("/api/audit?limit=2")).await;
    assert_eq!(seqs_of(&two), vec![0, 1]);

    // from_seq is inclusive.
    let (_, _, from5) = get_json(&http, &server.url("/api/audit?from_seq=5")).await;
    assert_eq!(seqs_of(&from5), vec![5, 6, 7]);

    let (_, _, windowed) = get_json(&http, &server.url("/api/audit?from_seq=2&limit=2")).await;
    assert_eq!(seqs_of(&windowed), vec![2, 3]);

    // Window beyond the head ⇒ empty, no error.
    let (_, _, empty) = get_json(&http, &server.url("/api/audit?from_seq=99")).await;
    assert_eq!(seqs_of(&empty), Vec::<u64>::new());

    // Absurd limit must neither error nor drop entries.
    let (_, _, capped) = get_json(&http, &server.url("/api/audit?limit=1000000")).await;
    assert_eq!(seqs_of(&capped), (0..8).collect::<Vec<u64>>());
}

#[tokio::test]
async fn api_audit_verify_report_shape_and_values() {
    let _env = ENV_LOCK.lock().await;
    let journal_dir = fixtures_root().join("journal");
    let _audit_env = EnvGuard::set("AUDIT_DIR", journal_dir.to_str().expect("utf8 path"));

    let entries = chain_entries(8);
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(&journal_dir).expect("open fixture journal"),
    ));
    let mut paths = fixture_paths();
    paths.audit_dir = journal_dir.clone();
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        paths,
    )
    .await;
    let (status, _, report) = get_json(&client(), &server.url("/api/audit/verify")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {report}");
    assert_keys_exact(
        &report,
        &[
            "entries",
            "first_seq",
            "valid_up_to_seq",
            "last_hash",
            "broken_at",
            "detail",
        ],
        "VerifyReport",
    );
    assert_eq!(report["entries"].as_u64(), Some(8), "{report}");
    assert_eq!(report["first_seq"].as_u64(), Some(0), "{report}");
    assert_eq!(report["valid_up_to_seq"].as_u64(), Some(7), "{report}");
    assert_null(&report["broken_at"], "clean chain has no break");
    assert_eq!(
        report["last_hash"].as_str(),
        Some(entries[7].entry_hash.as_str()),
        "{report}"
    );
}

#[tokio::test]
async fn api_audit_verify_detects_tamper_at_exact_seq() {
    let _env = ENV_LOCK.lock().await;
    let dir = TempDir::new().expect("tempdir");
    let journal_dir = dir.path().join("audit");
    std::fs::create_dir_all(&journal_dir).expect("mkdir");
    // Copy the static chain, then flip one byte inside seq 5's decision payload
    // without recomputing the hash ⇒ the chain must break at exactly seq 5.
    let source = fixtures_root().join("journal").join(JOURNAL_FILE);
    let target = journal_dir.join(JOURNAL_FILE);
    let content = std::fs::read_to_string(&source).expect("read static journal");
    let mut patched = String::new();
    let mut hits = 0;
    for line in content.lines() {
        if line.contains("\"seq\":5,") {
            assert!(line.contains("\"d5\""), "tamper marker missing: {line}");
            patched.push_str(&line.replace("\"d5\"", "\"X5\""));
            hits += 1;
        } else {
            patched.push_str(line);
        }
        patched.push('\n');
    }
    assert_eq!(hits, 1, "exactly one seq-5 line");
    std::fs::write(&target, patched).expect("write tampered journal");

    let _audit_env = EnvGuard::set("AUDIT_DIR", journal_dir.to_str().expect("utf8 path"));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let journal = Arc::new(TokioMutex::new(
        AuditJournal::open(&journal_dir).expect("open tampered journal"),
    ));
    let mut paths = temp_paths(dir.path());
    paths.audit_dir = journal_dir.clone();
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        Some(journal),
        paths,
    )
    .await;
    let (status, _, report) = get_json(&client(), &server.url("/api/audit/verify")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {report}");
    assert_eq!(
        report["broken_at"].as_u64(),
        Some(5),
        "tamper must be detected at the exact seq: {report}"
    );
    assert_eq!(report["valid_up_to_seq"].as_u64(), Some(4), "{report}");
    assert!(
        report["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "break detail present: {report}"
    );
}

// ===========================================================================
// (4) /api/backtest — 404 + raw pass-through
// ===========================================================================

#[tokio::test]
async fn api_backtest_404_json_when_absent_and_raw_passthrough_when_present() {
    let _env = ENV_LOCK.lock().await;
    let dir = TempDir::new().expect("tempdir");
    let absent = dir.path().join("nope-report.json");
    let http = client();

    {
        // Phase 1: absent file ⇒ 404 with the frozen JSON error.
        let _env_bt = EnvGuard::set("BACKTEST_REPORT_PATH", absent.to_str().expect("utf8"));
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let mut paths = temp_paths(dir.path());
        paths.backtest_report = absent.clone();
        let server =
            serve_dashboard(health, Arc::new(AtomicBool::new(false)), None, None, paths).await;
        let (status, _, body) = get_json(&http, &server.url("/api/backtest")).await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "body: {body}");
        assert_keys_exact(&body, &["error"], "backtest 404 body");
        assert_eq!(body["error"], "backtest report not found");
    }

    {
        // Phase 2: present file ⇒ raw contents, byte-for-byte.
        let present = fixtures_root().join("backtest-report.json");
        let _env_bt = EnvGuard::set("BACKTEST_REPORT_PATH", present.to_str().expect("utf8"));
        let expected = std::fs::read_to_string(&present).expect("read fixture");
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let mut paths = temp_paths(dir.path());
        paths.backtest_report = present.clone();
        let server =
            serve_dashboard(health, Arc::new(AtomicBool::new(false)), None, None, paths).await;
        let (status, body) = get(&http, &server.url("/api/backtest")).await;
        assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
        assert_eq!(
            body, expected,
            "the report must pass through raw (byte-for-byte)"
        );
    }
}

// ===========================================================================
// (5) /api/nansen/spend
// ===========================================================================

#[tokio::test]
async fn api_nansen_spend_zero_ledger_is_zeros() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/nansen/spend")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_keys_exact(
        &body,
        &[
            "total_calls",
            "total_cost_usd",
            "calls_1h",
            "cost_24h_usd",
            "max_calls_per_hour",
            "recent",
            "cache",
        ],
        "spend (absent ledger)",
    );
    assert_eq!(body["total_calls"].as_u64(), Some(0), "{body}");
    assert_eq!(
        dec_value(&body["total_cost_usd"], "total_cost_usd"),
        Decimal::ZERO
    );
    assert_eq!(body["calls_1h"].as_u64(), Some(0), "{body}");
    assert_eq!(
        dec_value(&body["cost_24h_usd"], "cost_24h_usd"),
        Decimal::ZERO
    );
    assert_null(&body["max_calls_per_hour"], "max_calls_per_hour");
    assert_eq!(body["recent"].as_array().map(Vec::len), Some(0), "{body}");
    assert_keys_exact(&body["cache"], &["hits", "misses"], "cache");
    assert_null(&body["cache"]["hits"], "cache.hits");
    assert_null(&body["cache"]["misses"], "cache.misses");
}

#[tokio::test]
async fn api_nansen_spend_static_ledger_totals_and_expired_windows() {
    // Static fixture: two 2026-01-01 entries. Totals count everything ever
    // settled; the 1 h/24 h windows exclude entries older than the window.
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        fixture_paths(),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/nansen/spend")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_eq!(body["total_calls"].as_u64(), Some(2), "{body}");
    assert_eq!(
        dec_value(&body["total_cost_usd"], "total_cost_usd"),
        dec("0.15")
    );
    assert_eq!(body["calls_1h"].as_u64(), Some(0), "old entries: {body}");
    assert_eq!(
        dec_value(&body["cost_24h_usd"], "cost_24h_usd"),
        Decimal::ZERO
    );
    let recent = body["recent"].as_array().expect("recent array");
    assert_eq!(recent.len(), 2, "{body}");
    for item in recent {
        assert_keys_exact(
            item,
            &["ts_ms", "endpoint", "cost_usd", "tx_hash"],
            "recent item",
        );
    }
}

#[tokio::test]
async fn api_nansen_spend_populated_exact_windows_and_recent_order() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    let now = now_ms();
    // Hand-built ledger (chronological append order):
    //   a) now−6 h   $0.05   (inside 24 h, outside 1 h)
    //   b) now−30 m  $0.10   (inside both)
    //   c) now−5 m   $0.02   (inside both)
    //   d) now−2 m   $0.03   (inside both)
    //   e) now−25 h  $1.00   (outside both)
    let lines = [
        json!({"ts_ms": now - 21_600_000, "endpoint": "/a", "cost_usd": "0.05", "tx_hash": "0xaa", "payer": null, "network": null}),
        json!({"ts_ms": now - 1_800_000, "endpoint": "/b", "cost_usd": "0.10", "tx_hash": null, "payer": null, "network": null}),
        json!({"ts_ms": now - 300_000, "endpoint": "/c", "cost_usd": "0.02", "tx_hash": "0xcc", "payer": null, "network": null}),
        json!({"ts_ms": now - 120_000, "endpoint": "/d", "cost_usd": "0.03", "tx_hash": null, "payer": null, "network": null}),
        json!({"ts_ms": now - 90_000_000, "endpoint": "/e", "cost_usd": "1.00", "tx_hash": null, "payer": null, "network": null}),
    ];
    let mut ledger = String::new();
    for line in &lines {
        ledger.push_str(&serde_json::to_string(line).expect("serialize ledger line"));
        ledger.push('\n');
    }
    std::fs::write(root.join("nansen-spend.jsonl"), ledger).expect("write ledger");

    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(root),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/nansen/spend")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");

    assert_eq!(
        body["total_calls"].as_u64(),
        Some(5),
        "totals count the ledger: {body}"
    );
    assert_eq!(
        dec_value(&body["total_cost_usd"], "total_cost_usd"),
        dec("1.20")
    );
    assert_eq!(body["calls_1h"].as_u64(), Some(3), "1 h window: {body}");
    assert_eq!(
        dec_value(&body["cost_24h_usd"], "cost_24h_usd"),
        dec("0.20"),
        "{body}"
    );

    let recent = body["recent"].as_array().expect("recent array");
    assert_eq!(recent.len(), 5, "all five entries are recent: {body}");
    // "(last 20)" — the tail of the ledger in append order.
    let endpoints: Vec<&str> = recent
        .iter()
        .map(|item| item["endpoint"].as_str().expect("endpoint string"))
        .collect();
    assert_eq!(
        endpoints,
        vec!["/a", "/b", "/c", "/d", "/e"],
        "recent must be the ledger tail in append order: {body}"
    );
    assert_eq!(recent[4]["tx_hash"], Value::Null, "null tx_hash preserved");
}

// ===========================================================================
// (6) /api/breaker-status
// ===========================================================================

#[tokio::test]
async fn api_breaker_status_available_false_when_files_absent() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/breaker-status")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_keys_exact(
        &body,
        &["available", "state", "journal_tail"],
        "breaker-status (absent)",
    );
    assert_eq!(body["available"], false, "{body}");
    assert_null(&body["state"], "state");
    assert_eq!(
        body["journal_tail"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
}

#[tokio::test]
async fn api_breaker_status_parses_state_and_journal_tail() {
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        fixture_paths(),
    )
    .await;
    let (status, _, body) = get_json(&client(), &server.url("/api/breaker-status")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_keys_exact(
        &body,
        &["available", "state", "journal_tail"],
        "breaker-status (present)",
    );
    assert_eq!(body["available"], true, "{body}");

    // State passes through as parsed JSON.
    let expected_state: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_root().join("breaker-state.json")).expect("read state"),
    )
    .expect("parse state");
    assert_eq!(body["state"], expected_state, "{body}");

    // Journal tail: the LAST 3 parsed lines, in file order.
    let journal_raw =
        std::fs::read_to_string(fixtures_root().join("breaker-journal.jsonl")).expect("read");
    let parsed: Vec<Value> = journal_raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("journal line parses"))
        .collect();
    assert_eq!(parsed.len(), 5, "fixture sanity");
    let tail = body["journal_tail"].as_array().expect("journal_tail array");
    assert_eq!(tail.len(), 3, "last 3 parsed lines: {body}");
    assert_eq!(tail[0], parsed[2], "file order: line 3 first");
    assert_eq!(tail[1], parsed[3]);
    assert_eq!(tail[2], parsed[4]);
}

// ===========================================================================
// (7) pause / resume — auth matrix + kill flag
// ===========================================================================

async fn post(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    for attempt in 0..50 {
        match client.post(url).send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let value: Value = serde_json::from_str(&body).unwrap_or_else(|err| {
                    panic!("POST {url}: non-JSON body ({err}): {body:?}");
                });
                return (status, value);
            }
            Err(err) if attempt < 49 => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => panic!("POST {url} never connected: {err}"),
        }
    }
    unreachable!("retry loop returns or panics")
}

#[tokio::test]
async fn api_pause_503_without_admin_key() {
    let _env = ENV_LOCK.lock().await;
    let _key = EnvGuard::unset("DASHBOARD_ADMIN_KEY");
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let kill = Arc::new(AtomicBool::new(false));
    let server = serve_dashboard(
        health,
        Arc::clone(&kill),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;

    let (status, body) = post(&client(), &server.url("/api/pause")).await;
    assert_eq!(
        status,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "body: {body}"
    );
    assert_keys_exact(&body, &["error"], "503 body");
    assert_eq!(body["error"], "admin key not configured");
    assert!(!kill.load(Ordering::SeqCst), "no flag flip without a key");
}

#[tokio::test]
async fn api_pause_401_wrong_or_missing_key() {
    let _env = ENV_LOCK.lock().await;
    let _key = EnvGuard::set("DASHBOARD_ADMIN_KEY", "hunter2-secret");
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let kill = Arc::new(AtomicBool::new(false));
    let server = serve_dashboard(
        health,
        Arc::clone(&kill),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();

    let (status, body) = post(&http, &server.url("/api/pause?key=wrong")).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "body: {body}");
    assert_keys_exact(&body, &["error"], "401 body");
    assert_eq!(body["error"], "unauthorized");

    // No key at all is also unauthorized when one is configured.
    let (status2, body2) = post(&http, &server.url("/api/pause")).await;
    assert_eq!(status2, reqwest::StatusCode::UNAUTHORIZED, "body: {body2}");
    assert_eq!(body2["error"], "unauthorized");

    // Resume is gated identically.
    let (status3, body3) = post(&http, &server.url("/api/resume?key=wrong")).await;
    assert_eq!(status3, reqwest::StatusCode::UNAUTHORIZED, "body: {body3}");

    assert!(!kill.load(Ordering::SeqCst), "no flag flip on 401");
}

#[tokio::test]
async fn api_pause_resume_flip_kill_flag_and_idempotent() {
    let _env = ENV_LOCK.lock().await;
    let _key = EnvGuard::set("DASHBOARD_ADMIN_KEY", "s3cret-admin");
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let kill = Arc::new(AtomicBool::new(false));
    let server = serve_dashboard(
        health,
        Arc::clone(&kill),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();

    let (status, body) = post(&http, &server.url("/api/pause?key=s3cret-admin")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    assert_keys_exact(&body, &["paused"], "pause body");
    assert_eq!(body["paused"], true, "{body}");
    assert!(kill.load(Ordering::SeqCst), "kill flag must flip true");

    // Idempotent re-pause.
    let (status2, body2) = post(&http, &server.url("/api/pause?key=s3cret-admin")).await;
    assert_eq!(status2, reqwest::StatusCode::OK, "body: {body2}");
    assert_eq!(body2["paused"], true, "{body2}");
    assert!(kill.load(Ordering::SeqCst));

    // The flag surfaces through /api/state.
    let (_, _, state) = get_json(&http, &server.url("/api/state")).await;
    assert_eq!(state["paused"], true, "paused state visible: {state}");

    let (status3, body3) = post(&http, &server.url("/api/resume?key=s3cret-admin")).await;
    assert_eq!(status3, reqwest::StatusCode::OK, "body: {body3}");
    assert_eq!(body3["paused"], false, "{body3}");
    assert!(!kill.load(Ordering::SeqCst), "kill flag must flip false");

    // Idempotent resume.
    let (status4, body4) = post(&http, &server.url("/api/resume?key=s3cret-admin")).await;
    assert_eq!(status4, reqwest::StatusCode::OK, "body: {body4}");
    assert_eq!(body4["paused"], false, "{body4}");
}

// ===========================================================================
// (8) rate limit — per-IP token bucket (60/min, burst 120)
// ===========================================================================

#[tokio::test]
async fn api_rate_limit_429_per_ip_with_exemptions() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();
    let url = server.url("/api/state");

    let mut successes = 0_u32;
    let mut limited = 0_u32;
    let mut first_429_body = String::new();
    let mut last_status = reqwest::StatusCode::OK;
    for index in 0..250_u32 {
        let response = http
            .get(&url)
            .header("X-Forwarded-For", RATE_IP)
            .send()
            .await
            .unwrap_or_else(|err| panic!("request {index}: {err}"));
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if index < 100 {
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "burst 120: request {index} must not be limited yet: {body}"
            );
        }
        if status == reqwest::StatusCode::OK {
            successes += 1;
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
            if first_429_body.is_empty() {
                first_429_body = body.clone();
            }
        } else {
            panic!("request {index}: unexpected status {status}: {body}");
        }
        last_status = status;
    }
    assert!(
        successes >= 100,
        "burst 120 must allow ≥100 rapid requests, got {successes}"
    );
    assert!(
        limited >= 80,
        "60 req/min bucket must shed most of 250 rapid requests, got {limited} limited"
    );
    assert_eq!(
        last_status,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "bucket must stay exhausted at the end of the burst"
    );
    let error: Value = serde_json::from_str(&first_429_body)
        .unwrap_or_else(|err| panic!("429 body must be JSON ({err}): {first_429_body:?}"));
    assert_keys_exact(&error, &["error"], "429 body");
    assert_eq!(error["error"], "rate limited");
    println!("rate limit: {successes} ok, {limited} limited (250 requests)");

    // Same IP stays limited; the first X-Forwarded-For value is the bucket key.
    let same = http
        .get(&url)
        .header("X-Forwarded-For", RATE_IP)
        .send()
        .await
        .expect("same ip");
    assert_eq!(same.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    let chained = http
        .get(&url)
        .header("X-Forwarded-For", format!("{RATE_IP}, 10.1.2.3"))
        .send()
        .await
        .expect("chained xff");
    assert_eq!(
        chained.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "the first X-Forwarded-For value is the client identity"
    );

    // A different IP has its own bucket.
    let fresh = http
        .get(&url)
        .header("X-Forwarded-For", RATE_IP_FRESH)
        .send()
        .await
        .expect("fresh ip");
    assert_eq!(
        fresh.status(),
        reqwest::StatusCode::OK,
        "a different forwarded IP must be unaffected"
    );

    // /healthz and GET / are exempt.
    let healed = http
        .get(server.url("/healthz"))
        .header("X-Forwarded-For", RATE_IP)
        .send()
        .await
        .expect("healthz");
    assert_eq!(
        healed.status(),
        reqwest::StatusCode::OK,
        "/healthz must stay exempt"
    );
    let root = http
        .get(server.url("/"))
        .header("X-Forwarded-For", RATE_IP)
        .send()
        .await
        .expect("root");
    assert_eq!(
        root.status(),
        reqwest::StatusCode::OK,
        "GET / must stay exempt"
    );
}

// ===========================================================================
// (9) CORS
// ===========================================================================

#[tokio::test]
async fn api_cors_allow_origin_on_reads() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();
    for path in ["/api/state", "/api/decisions"] {
        let (status, headers, _) = get_json(&http, &server.url(path)).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{path}");
        let origin = headers
            .get("access-control-allow-origin")
            .unwrap_or_else(|| panic!("{path}: missing Access-Control-Allow-Origin"));
        assert_eq!(origin, "*", "{path}: allow-origin");
    }
}

#[tokio::test]
async fn api_cors_preflight_options() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let http = client();
    let response = http
        .request(reqwest::Method::OPTIONS, server.url("/api/pause"))
        .header("Origin", "https://ops.example.test")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .expect("preflight");
    let status = response.status();
    assert!(status.is_success(), "preflight must succeed, got {status}");
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("*"),
        "preflight allow-origin"
    );
    let methods = headers
        .get("access-control-allow-methods")
        .unwrap_or_else(|| panic!("preflight must advertise methods"))
        .to_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        methods == "*" || methods.contains("POST"),
        "allow-methods must cover POST, got {methods:?}"
    );
    let allowed_headers = headers
        .get("access-control-allow-headers")
        .unwrap_or_else(|| panic!("preflight must advertise headers"))
        .to_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        allowed_headers == "*" || allowed_headers.contains("content-type"),
        "allow-headers must cover content-type, got {allowed_headers:?}"
    );
}

// ===========================================================================
// (10) HTML artifact — static analysis only (SPEC-P15 §1/§4)
// ===========================================================================

fn html_path() -> PathBuf {
    repo_root().join("dashboard/index.html")
}

fn load_html() -> String {
    std::fs::read_to_string(html_path()).expect("dashboard/index.html must be readable")
}

/// Strip HTML comments (`<!-- -->`) and C-style block comments (`/* */`),
/// preserving everything else byte-for-byte.
fn strip_block_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with("<!--") {
            match input[index..].find("-->") {
                Some(end) => index += end + 3,
                None => break,
            }
        } else if input[index..].starts_with("/*") {
            match input[index..].find("*/") {
                Some(end) => index += end + 2,
                None => break,
            }
        } else {
            let ch = input[index..].chars().next().expect("char boundary");
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    out
}

/// The code portion of a line: everything before the first `//` that starts a
/// comment (preceded by whitespace/brace/paren/semicolon or at line start; not
/// `://` from a URL).
fn code_part_of_line(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'/' && bytes[index + 1] == b'/' {
            let is_comment = index == 0
                || matches!(
                    bytes[index - 1],
                    b' ' | b'\t' | b'(' | b'{' | b';' | b',' | b')'
                );
            if is_comment {
                return &line[..index];
            }
        }
        index += 1;
    }
    line
}

/// The HTML with block comments and line comments removed (code only).
fn strip_all_comments(input: &str) -> String {
    strip_block_comments(input)
        .lines()
        .map(code_part_of_line)
        .collect::<Vec<&str>>()
        .join("\n")
}

fn attr_values(html: &str, attr: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut values = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        if let Some(end) = after.find('"') {
            values.push(after[..end].to_string());
        }
        rest = after;
    }
    values
}

#[test]
fn html_size_within_limit() {
    let html = load_html();
    let bytes = html.len();
    assert!(bytes > 0, "dashboard page must not be empty");
    assert!(
        bytes <= 204_800,
        "dashboard/index.html must stay <= 204800 bytes (wc -c), got {bytes}"
    );
}

#[test]
fn html_eight_section_hooks_present() {
    let html = load_html();
    // SPEC-P15 §1 freezes eight sections; §4 requires their markers/data hooks.
    // The section-hook vocabulary below mirrors the section names in §1.
    let required = [
        "header",
        "positions",
        "feed",
        "audit",
        "resilience",
        "backtest",
        "nansen",
        "killswitch",
    ];
    let hooks = attr_values(&html, "data-hook");
    let unique: std::collections::BTreeSet<&str> = hooks.iter().map(String::as_str).collect();
    assert!(
        unique.len() >= required.len(),
        "expected >= {} distinct data-hook markers, got {:?}",
        required.len(),
        unique
    );
    for name in required {
        assert!(
            unique.contains(name),
            "missing section hook {name:?}; present: {:?}",
            unique
        );
    }
    let sections = attr_values(&html, "data-section");
    assert_eq!(
        sections.len(),
        8,
        "expected 8 data-section markers, got {}",
        sections.len()
    );
}

#[test]
fn html_no_external_urls_outside_comments() {
    let html = load_html();
    let code = strip_all_comments(&html);
    let mut violations = Vec::new();
    for line in code.lines() {
        for needle in [
            "http://",
            "https://",
            "src=\"//",
            "src='//",
            "href=\"//",
            "href='//",
            "url(//",
            "url(\"//",
            "url('//",
        ] {
            if line.contains(needle) {
                violations.push(format!("{needle} in {line:?}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "no external URL may appear outside comments (SPEC-P15 §1/§4): {violations:?}"
    );
}

#[test]
fn html_kill_switch_flow_strings_present() {
    let html = load_html();
    for required in ["PAUSE", "RESUME", "GUARDIAN PAUSED"] {
        assert!(
            html.contains(required),
            "kill-switch flow string {required:?} missing"
        );
    }
    // POSTs /api/pause|resume (§1): either literals or a assembled "/api/" + verb.
    let literal = html.contains("/api/pause") && html.contains("/api/resume");
    let assembled = html.contains("/api/") && html.contains("pause") && html.contains("resume");
    assert!(
        literal || assembled,
        "kill switch must POST /api/pause and /api/resume"
    );
    // Admin key input (§1: session-only key input in the confirm dialog).
    let mut has_key_input = false;
    let mut rest = html.as_str();
    while let Some(at) = rest.find("<input") {
        let after = &rest[at..];
        let end = after.find('>').map_or(after.len(), |end| end + 1);
        let tag = after[..end].to_ascii_lowercase();
        if tag.contains("key") || tag.contains("admin") {
            has_key_input = true;
            break;
        }
        rest = &after[end..];
    }
    assert!(
        has_key_input || html.to_ascii_lowercase().contains("admin key"),
        "confirm dialog must offer the admin key input"
    );
    // The confirm step requires typing the word.
    assert!(
        html.to_ascii_lowercase().contains("confirm"),
        "confirm dialog flow missing"
    );
}

#[test]
fn html_no_localstorage_and_no_innerhtml_template_data() {
    let html = load_html();
    let code = strip_all_comments(&html).to_ascii_lowercase();
    assert!(
        !code.contains("localstorage"),
        "localStorage usage is forbidden (admin key is session-only)"
    );
    // innerHTML is acceptable only with fully static markup — flag template data
    // (`${…}`) or non-literal right-hand sides.
    let mut cursor = 0;
    while let Some(at) = code[cursor..].find("innerhtml") {
        let absolute = cursor + at;
        let tail = &code[absolute..];
        if let Some(eq) = tail.find('=') {
            let rhs: &str = tail[eq + 1..].split([';', '\n']).next().unwrap_or("");
            let rhs = rhs.trim();
            let static_literal = (rhs.starts_with('"')
                || rhs.starts_with('\'')
                || (rhs.starts_with('`') && !rhs.contains("${")))
                && !rhs.contains("${");
            assert!(
                static_literal,
                "innerHTML with template data is forbidden: {rhs:?}"
            );
        }
        cursor = absolute + "innerhtml".len();
    }
}

#[test]
fn html_wiring_strings_present() {
    let html = load_html();
    let code = strip_all_comments(&html);
    for endpoint in [
        "/api/state",
        "/api/decisions",
        "/api/audit/verify",
        "/api/breaker-status",
        "/api/backtest",
        "/api/nansen/spend",
    ] {
        assert!(code.contains(endpoint), "missing endpoint {endpoint}");
    }
    assert!(
        code.contains("textContent"),
        "all dynamic text via textContent"
    );
    assert!(
        code.contains("data-cre"),
        "data-cre status block hook missing"
    );
    assert!(
        code.contains("unavailable"),
        "muted unavailable state missing"
    );
    assert!(code.contains("setInterval"), "poll loops missing");
    assert!(
        html.contains("2000") || html.contains("2_000"),
        "2 s fast poll constant missing"
    );
    assert!(
        html.contains("15000") || html.contains("15_000"),
        "15 s slow poll constant missing"
    );
    let lower = html.to_ascii_lowercase();
    assert!(
        lower.contains("ui-monospace") || lower.contains("sfmono") || lower.contains("fira code"),
        "monospace font stack missing"
    );
    assert!(
        lower.contains("836ef9"),
        "Monad purple accent #836EF9 missing"
    );
    assert!(lower.contains("<style"), "inline CSS missing");
    assert!(lower.contains("<!doctype html>"), "doctype missing");
}

#[tokio::test]
async fn api_root_serves_dashboard_file_bytes() {
    let dir = TempDir::new().expect("tempdir");
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let server = serve_dashboard(
        health,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        temp_paths(dir.path()),
    )
    .await;
    let response = client().get(server.url("/")).send().await.expect("GET /");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.contains("text/html"),
        "content-type: {content_type}"
    );
    let body = response.text().await.expect("body");
    let file = load_html();
    assert_eq!(body, file, "GET / must serve dashboard/index.html verbatim");
}
