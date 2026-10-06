//! Audit-facing HTTP API (merged into the daemon router by `main`) plus the
//! P15 dashboard surface.
//!
//! Implemented per `SPEC-P10.md` §7 (audit listing + verify) and
//! `SPEC-P15.md` §2-§3 (dashboard panels, kill switch, CORS, rate limit):
//!
//! - `GET /` — the embedded single-file dashboard (`include_str!` of
//!   `dashboard/index.html`, authored separately);
//! - `GET /api/state` — health + live state + heartbeat file + journal tail,
//!   with per-position distance-to-liquidation and tier computed by
//!   `sentinel-core` (thresholds 25/15/8) at the freshest mark;
//! - `GET /api/decisions?limit` — newest-first journal window (default 20,
//!   cap 100);
//! - `GET /api/audit?from_seq&limit` + `GET /api/audit/verify` — P10 behavior
//!   unchanged;
//! - `GET /api/backtest` — raw P13 report file (`BACKTEST_REPORT_PATH`,
//!   default `docs/backtest-report.json`), JSON 404 when absent;
//! - `GET /api/nansen/spend` — spend-ledger totals + sliding windows;
//! - `GET /api/breaker-status` — breaker state file + journal tail;
//! - `POST /api/pause` / `POST /api/resume` — `?key=` gated kill switch
//!   (constant-time compare against `DASHBOARD_ADMIN_KEY`).
//!
//! `/api/*` responses carry the hand-rolled CORS headers
//! (`Access-Control-Allow-Origin: *` + methods/headers) and pass a hand-rolled
//! per-IP token bucket (60 req/min, burst 120; IP = first `X-Forwarded-For`
//! value else `ConnectInfo` socket addr) answering `429 {"error":"rate
//! limited"}`. `/healthz` (a separate router merged by `main`) and `GET /`
//! are exempt.
//!
//! The frozen core handle (`AuditJournal`) resumes from a directory but does
//! not expose it, so the router is told where the journal lives:
//! [`audit_router`] defaults to [`DEFAULT_AUDIT_DIR`] (override with the
//! `AUDIT_DIR` env var), and [`audit_router_with_dir`] takes an explicit
//! directory (tests, non-default deployments).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use sentinel_core::audit::{
    AuditEntry, AuditJournal, OutcomeRecord, Trigger, VerifyReport, hash_input, verify_chain,
};
use sentinel_core::risk::{RiskThresholds, distance_to_liq_pct, effective_liq_price, tier};
use sentinel_core::types::{Position, RiskTier};

use crate::nansen::spend::SpendLedger;

/// Default journal directory (the daemon opens `data/audit`, `main.rs`).
pub const DEFAULT_AUDIT_DIR: &str = "data/audit";

/// Page size used when `limit` is absent from the audit query.
const DEFAULT_LIMIT: usize = 50;

/// Largest page size accepted from the audit query.
const MAX_LIMIT: usize = 500;

/// Router state: the journal handle + the directory its files live in.
#[derive(Clone)]
struct AuditState {
    journal: Arc<Mutex<AuditJournal>>,
    dir: Arc<PathBuf>,
}

/// Router exposing `GET /api/audit?from_seq&limit` and
/// `GET /api/audit/verify`.
///
/// The journal directory defaults to `AUDIT_DIR` (env) or
/// [`DEFAULT_AUDIT_DIR`].
pub fn audit_router(journal: Arc<Mutex<AuditJournal>>) -> Router {
    let dir = std::env::var_os("AUDIT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_AUDIT_DIR));
    audit_router_with_dir(dir, journal)
}

/// [`audit_router`] with an explicit journal directory (used by tests and
/// deployments that open the journal elsewhere).
pub fn audit_router_with_dir(dir: impl Into<PathBuf>, journal: Arc<Mutex<AuditJournal>>) -> Router {
    let state = AuditState {
        journal,
        dir: Arc::new(dir.into()),
    };
    Router::new()
        .route("/api/audit", get(list_audit))
        .route("/api/audit/verify", get(verify_audit))
        .with_state(state)
}

/// `from_seq` + `limit` query parameters (both optional).
#[derive(Debug, Deserialize)]
struct AuditQuery {
    from_seq: Option<u64>,
    limit: Option<usize>,
}

/// Clamp the requested page size into `[1, MAX_LIMIT]` (`DEFAULT_LIMIT` when
/// absent).
fn effective_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// `GET /api/audit` — windowed journal listing as a JSON array.
async fn list_audit(State(state): State<AuditState>, Query(query): Query<AuditQuery>) -> Response {
    let from_seq = query.from_seq.unwrap_or(0);
    let limit = effective_limit(query.limit);
    let result = state.journal.lock().await.read_entries(from_seq, limit);
    match result {
        Ok(entries) => Json(entries).into_response(),
        Err(err) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("journal read failed: {err}"),
        ),
    }
}

/// `GET /api/audit/verify` — full-file `VerifyReport` for the current day.
async fn verify_audit(State(state): State<AuditState>) -> Response {
    let Some(path) = current_day_file(&state.dir) else {
        return Json(empty_report(&format!(
            "no journal-*.jsonl file under {}",
            state.dir.display()
        )))
        .into_response();
    };
    match verify_chain(&path) {
        Ok(report) => Json(report).into_response(),
        Err(err) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("verify {} failed: {err}", path.display()),
        ),
    }
}

/// Today's `journal-YYYYMMDD.jsonl` under `dir`, falling back to the
/// lexicographically-latest journal file when today's does not exist yet
/// (no entries written today). `None` when the directory holds none.
fn current_day_file(dir: &Path) -> Option<PathBuf> {
    let today = dir.join(format!(
        "journal-{}.jsonl",
        chrono::Utc::now().format("%Y%m%d")
    ));
    if today.is_file() {
        return Some(today);
    }
    latest_journal_file(dir)
}

/// Lexicographically-latest `journal-*.jsonl` file under `dir`.
///
/// The same ordering [`AuditJournal::open`] uses to resume.
pub fn latest_journal_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("journal-") && name.ends_with(".jsonl")) {
            continue;
        }
        let path = entry.path();
        best = match best {
            Some(current) if current >= path => Some(current),
            _ => Some(path),
        };
    }
    best
}

/// Zero-entry report used when the directory holds no journal file yet.
fn empty_report(detail: &str) -> VerifyReport {
    VerifyReport {
        entries: 0,
        first_seq: None,
        valid_up_to_seq: None,
        last_hash: None,
        broken_at: None,
        detail: Some(detail.to_owned()),
    }
}

/// JSON error body for failures.
fn error_json(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

// ---------------------------------------------------------------------------
// P15 dashboard surface (SPEC-P15 §2-§3)
// ---------------------------------------------------------------------------

/// Default page size for `/api/decisions`.
const DECISIONS_DEFAULT_LIMIT: usize = 20;

/// Largest page size accepted for `/api/decisions`.
const DECISIONS_MAX_LIMIT: usize = 100;

/// Journal entries scanned for `/api/state` (per-position `last_action`).
const STATE_JOURNAL_TAIL: usize = 50;

/// Spend-ledger entries returned by `/api/nansen/spend`.
const SPEND_RECENT: usize = 20;

/// Breaker-journal lines returned by `/api/breaker-status`.
const BREAKER_TAIL: usize = 3;

/// One hour in milliseconds (Nansen `calls_1h` window).
const HOUR_MS: u64 = 3_600_000;

/// One day in milliseconds (Nansen `cost_24h_usd` window).
const DAY_MS: u64 = 86_400_000;

/// Requests per minute refilled by the dashboard token bucket.
const RATE_PER_MIN: u64 = 60;

/// Burst capacity of the dashboard token bucket.
const RATE_BURST: u64 = 120;

/// Milli-token scale: one token is [`TOKEN_SCALE`] units.
const TOKEN_SCALE: u64 = 1_000;

/// Milli-tokens refilled per millisecond (`RATE_PER_MIN` over a minute).
const REFILL_PER_MS: u64 = RATE_PER_MIN * TOKEN_SCALE / 60_000;

/// Upper bound on tracked per-IP buckets (spoofed `X-Forwarded-For` guard).
const MAX_BUCKETS: usize = 4_096;

/// Idle time after which a bucket is pruned when the map is full.
const BUCKET_IDLE_TTL: Duration = Duration::from_secs(600);

/// Aggregated handles for the dashboard + public API surface (SPEC-P15 §3).
#[derive(Clone)]
pub struct DashboardState {
    /// Health surface (mode, uptime, feed freshness, version).
    pub health: Arc<crate::health::HealthState>,
    /// Audit journal handle (`None`: audit endpoints degrade to 404s).
    pub journal: Option<Arc<Mutex<AuditJournal>>>,
    /// Kill switch shared with the pipeline (pause/resume mutations).
    pub kill: Arc<AtomicBool>,
    /// Live market/account state (`None` until wired; replay wires it too).
    pub live_state: Option<Arc<Mutex<crate::pipeline::LiveState>>>,
    /// Filesystem-backed panels (P13/P14/nansen/heartbeat).
    pub paths: DashboardPaths,
}

/// Filesystem paths backing dashboard panels (SPEC-P15 §2).
#[derive(Debug, Clone)]
pub struct DashboardPaths {
    /// Journal directory (`AUDIT_DIR` env honored by [`Default`]).
    pub audit_dir: PathBuf,
    /// Nansen spend ledger JSONL.
    pub spend_ledger: PathBuf,
    /// P13 backtest report JSON (`BACKTEST_REPORT_PATH` env honored).
    pub backtest_report: PathBuf,
    /// Breaker state JSON.
    pub breaker_state: PathBuf,
    /// Breaker journal JSONL.
    pub breaker_journal: PathBuf,
    /// Anchor heartbeat status JSON (written by the anchor task).
    pub heartbeat: PathBuf,
}

impl Default for DashboardPaths {
    fn default() -> Self {
        Self {
            audit_dir: std::env::var("AUDIT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_AUDIT_DIR)),
            spend_ledger: PathBuf::from("data/nansen-spend.jsonl"),
            backtest_report: std::env::var("BACKTEST_REPORT_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("docs/backtest-report.json")),
            breaker_state: PathBuf::from("data/breaker-state.json"),
            breaker_journal: PathBuf::from("data/breaker-journal.jsonl"),
            heartbeat: PathBuf::from("data/heartbeat.json"),
        }
    }
}

/// Full dashboard + API router (SPEC-P15 §2-§3).
///
/// Serves the embedded dashboard, every `/api/*` panel, the P10 audit
/// endpoints (`state.journal: Some`) and the key-gated kill switch. All
/// `/api/*` routes answer CORS headers and pass the hand-rolled per-IP token
/// bucket; `/healthz` (merged in from `health::router` by `main`) and
/// `GET /` are exempt.
pub fn dashboard_router(state: DashboardState) -> Router {
    let limiter = Arc::new(RateLimiter::new());

    // P10 behavior: audit endpoints only exist while a journal handle is wired.
    let audit = state
        .journal
        .as_ref()
        .map(|journal| audit_router_with_dir(state.paths.audit_dir.clone(), Arc::clone(journal)));

    let mut router = Router::new()
        .route("/", get(index_html))
        .route("/api/state", get(get_state))
        .route("/api/decisions", get(get_decisions))
        .route("/api/backtest", get(get_backtest))
        .route("/api/nansen/spend", get(get_nansen_spend))
        .route("/api/breaker-status", get(get_breaker_status))
        .route("/api/pause", post(post_pause))
        .route("/api/resume", post(post_resume))
        .with_state(state);

    if let Some(audit) = audit {
        router = router.merge(audit);
    }

    router.layer(middleware::from_fn_with_state(limiter, api_guard))
}

/// `GET /` — the embedded single-file dashboard.
async fn index_html() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html")],
        include_str!("../../../dashboard/index.html"),
    )
        .into_response()
}

/// `GET /api/state` — dashboard snapshot (SPEC-P15 §2).
///
/// Health fields come from [`crate::health::HealthState::render`]; live
/// account/positions come from the pipeline's shared
/// [`crate::pipeline::LiveState`] with marks refreshed and distance-to-liq /
/// tier computed by `sentinel-core` at thresholds 25/15/8; `heartbeat` reads
/// the anchor task's status file (nulls when absent); `last_action` per
/// position is the newest journal-tail entry for that market.
async fn get_state(State(state): State<DashboardState>) -> Response {
    let now_ms = wall_now_ms();
    let mut body = state.health.render(now_ms);
    let paused = state.kill.load(Ordering::Relaxed);

    let tail = match &state.journal {
        Some(journal) => journal_tail(journal, STATE_JOURNAL_TAIL).await,
        None => Vec::new(),
    };

    let mut feed_fresh = false;
    let mut account = Value::Null;
    let mut positions = Vec::new();
    if let Some(live_state) = &state.live_state {
        let live = live_state.lock().await;
        feed_fresh = live.stale_secs.is_none();
        if let Some(snapshot) = &live.account {
            account = json!({
                "free_balance": snapshot.free_balance.to_string(),
                "equity": snapshot.equity.to_string(),
            });
            positions = snapshot
                .positions
                .iter()
                .map(|position| position_json(&live, position, &tail))
                .collect();
        }
    }

    let heartbeat = heartbeat_json(&state.paths.heartbeat, now_ms);
    if let Some(object) = body.as_object_mut() {
        object.insert("feed_fresh".to_owned(), json!(feed_fresh));
        object.insert("paused".to_owned(), json!(paused));
        object.insert("heartbeat".to_owned(), heartbeat);
        object.insert("account".to_owned(), account);
        object.insert("positions".to_owned(), Value::Array(positions));
    }
    Json(body).into_response()
}

/// Render one position for `/api/state` (decimals as strings, SPEC-P15 §2).
fn position_json(
    live: &crate::pipeline::LiveState,
    position: &Position,
    tail: &[AuditEntry],
) -> Value {
    // The mark map is authoritative (it tracks every mark event); the
    // position's own mark is the fallback.
    let mark = live
        .marks
        .get(&position.market_id)
        .copied()
        .or(position.mark_price);
    let mut refreshed = position.clone();
    refreshed.mark_price = mark;

    let market = live
        .markets
        .iter()
        .find(|market| market.id == position.market_id);
    let mut distance_pct = None;
    let mut tier_name = None;
    let liq_price = match market {
        Some(market) => {
            let distance = distance_to_liq_pct(&refreshed, market);
            distance_pct = distance;
            tier_name = distance.map(|value| tier_label(tier(value, &dashboard_thresholds())));
            effective_liq_price(&refreshed, market).map(|liq| liq.price)
        }
        // No market metadata: still surface an exchange-provided liq price.
        None => position.liq_price,
    };

    json!({
        "market_id": position.market_id.0,
        "symbol": position.symbol.clone(),
        "side": if position.size > Decimal::ZERO { "long" } else { "short" },
        "size": position.size.to_string(),
        "entry_price": position.entry_price.to_string(),
        "mark_price": mark.map(|value| value.to_string()),
        "distance_pct": distance_pct.map(|value| value.to_string()),
        "liq_price": liq_price.map(|value| value.to_string()),
        "tier": tier_name,
        "collateral": position.collateral.to_string(),
        "unrealized_pnl": position.unrealized_pnl.to_string(),
        "leverage": position.leverage.to_string(),
        "last_action": last_action_for(tail, position.market_id.0),
    })
}

/// Frozen dashboard risk thresholds (`soft`/`warn`/`hard` = 25/15/8).
fn dashboard_thresholds() -> RiskThresholds {
    RiskThresholds {
        soft: Decimal::new(25, 0),
        warn: Decimal::new(15, 0),
        hard: Decimal::new(8, 0),
    }
}

/// Lowercase wire name of a [`RiskTier`] (`green|yellow|orange|red`).
fn tier_label(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Green => "green",
        RiskTier::Yellow => "yellow",
        RiskTier::Orange => "orange",
        RiskTier::Red => "red",
    }
}

/// Lowercase wire name of a [`Trigger`].
fn trigger_label(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Reflex => "reflex",
        Trigger::Strategy => "strategy",
        Trigger::Human => "human",
        Trigger::Backtest => "backtest",
        Trigger::System => "system",
    }
}

/// Newest journal-tail entry for `market_id`, rendered as a short action
/// string (`decision.action`, else the lowercase trigger).
fn last_action_for(tail: &[AuditEntry], market_id: u32) -> Option<String> {
    tail.iter()
        .rev()
        .find(|entry| entry.market_id == Some(market_id))
        .map(|entry| {
            entry
                .decision
                .get("action")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| trigger_label(entry.trigger).to_owned())
        })
}

/// The journal's newest `max` entries (empty on read failure).
async fn journal_tail(journal: &Arc<Mutex<AuditJournal>>, max: usize) -> Vec<AuditEntry> {
    let guard = journal.lock().await;
    let from_seq = guard.seq().saturating_sub(max as u64);
    match guard.read_entries(from_seq, max) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(error = %err, "dashboard journal tail read failed");
            Vec::new()
        }
    }
}

/// `limit` query parameter for `/api/decisions`.
#[derive(Debug, Deserialize)]
struct DecisionsQuery {
    limit: Option<usize>,
}

/// Clamp the requested decisions page size into `[1, DECISIONS_MAX_LIMIT]`
/// (`DECISIONS_DEFAULT_LIMIT` when absent).
fn decisions_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DECISIONS_DEFAULT_LIMIT)
        .clamp(1, DECISIONS_MAX_LIMIT)
}

/// `GET /api/decisions?limit=` — newest-first journal window (SPEC-P15 §2).
///
/// Without a journal handle the panel degrades to an empty array (audit
/// endpoints keep their P10 404 degradation).
async fn get_decisions(
    State(state): State<DashboardState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let Some(journal) = &state.journal else {
        return Json(Value::Array(Vec::new())).into_response();
    };
    let limit = decisions_limit(query.limit);
    let guard = journal.lock().await;
    let from_seq = guard.seq().saturating_sub(limit as u64);
    match guard.read_entries(from_seq, limit) {
        Ok(entries) => {
            let items: Vec<Value> = entries.iter().rev().map(decision_entry_json).collect();
            Json(Value::Array(items)).into_response()
        }
        Err(err) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("journal read failed: {err}"),
        ),
    }
}

/// One `/api/decisions` item: exactly the frozen eight keys.
fn decision_entry_json(entry: &AuditEntry) -> Value {
    json!({
        "seq": entry.seq,
        "ts": entry.ts,
        "trigger": entry.trigger,
        "market_id": entry.market_id,
        "decision": entry.decision.clone(),
        "policy_verdict": entry.policy_verdict.clone(),
        "execution": entry.execution.clone(),
        "entry_hash": entry.entry_hash.clone(),
    })
}

/// `GET /api/backtest` — raw contents of the P13 report JSON (SPEC-P15 §2).
async fn get_backtest(State(state): State<DashboardState>) -> Response {
    match std::fs::read_to_string(&state.paths.backtest_report) {
        Ok(contents) => ([(header::CONTENT_TYPE, "application/json")], contents).into_response(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            error_json(StatusCode::NOT_FOUND, "backtest report not found")
        }
        Err(err) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("backtest report unreadable: {err}"),
        ),
    }
}

/// `GET /api/nansen/spend` — spend-ledger totals + sliding windows
/// (SPEC-P15 §2). A missing ledger degrades to zeros; the live TTL cache is
/// not reachable from here, so `cache` stats stay null.
async fn get_nansen_spend(State(state): State<DashboardState>) -> Response {
    let ledger = SpendLedger::new(state.paths.spend_ledger.clone());
    let entries = ledger.load();
    let now_ms = wall_now_ms();

    let total_calls = entries.len() as u64;
    let total_cost = entries
        .iter()
        .filter_map(|entry| Decimal::from_str(entry.cost_usd.trim()).ok())
        .fold(Decimal::ZERO, |total, cost| total + cost);
    let calls_1h = ledger.calls_since(now_ms, HOUR_MS);
    let cost_24h = ledger.cost_since(now_ms, DAY_MS);

    let recent_start = entries.len().saturating_sub(SPEND_RECENT);
    let recent: Vec<Value> = entries[recent_start..]
        .iter()
        .map(|entry| {
            json!({
                "ts_ms": entry.ts_ms,
                "endpoint": entry.endpoint.clone(),
                "cost_usd": entry.cost_usd.clone(),
                "tx_hash": entry.tx_hash.clone(),
            })
        })
        .collect();

    let max_calls_per_hour = std::env::var("NANSEN_MAX_CALLS_PER_HOUR")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());

    Json(json!({
        "total_calls": total_calls,
        "total_cost_usd": total_cost.to_string(),
        "calls_1h": calls_1h,
        "cost_24h_usd": cost_24h.to_string(),
        "max_calls_per_hour": max_calls_per_hour,
        "recent": recent,
        "cache": { "hits": null, "misses": null },
    }))
    .into_response()
}

/// `GET /api/breaker-status` — breaker state file + last journal lines
/// (SPEC-P15 §2). `available` is false (and `state` null) while the state
/// file is absent or unparseable.
async fn get_breaker_status(State(state): State<DashboardState>) -> Response {
    let breaker_state = read_json_file(&state.paths.breaker_state);
    let available = breaker_state.is_some();
    let journal_tail = breaker_journal_tail(&state.paths.breaker_journal, BREAKER_TAIL);
    Json(json!({
        "available": available,
        "state": breaker_state,
        "journal_tail": journal_tail,
    }))
    .into_response()
}

/// Last `max` parseable JSON values of a JSONL file (file order preserved).
fn breaker_journal_tail(path: &Path, max: usize) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut tail: Vec<Value> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            tail.push(value);
        }
    }
    let start = tail.len().saturating_sub(max);
    tail.split_off(start)
}

/// `key` query parameter for the kill-switch mutations.
#[derive(Debug, Deserialize)]
struct AdminQuery {
    key: Option<String>,
}

/// `POST /api/pause` — engage the kill switch (admin key required).
async fn post_pause(
    State(state): State<DashboardState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    set_kill_switch(state, query.key, true).await
}

/// `POST /api/resume` — release the kill switch (admin key required).
async fn post_resume(
    State(state): State<DashboardState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    set_kill_switch(state, query.key, false).await
}

/// Shared pause/resume logic: constant-time key check against
/// `DASHBOARD_ADMIN_KEY` (unset ⇒ 503, mismatch ⇒ 401), then flip the shared
/// kill flag and best-effort journal a SYSTEM entry that never fails the
/// response.
async fn set_kill_switch(state: DashboardState, key: Option<String>, paused: bool) -> Response {
    match std::env::var("DASHBOARD_ADMIN_KEY") {
        Ok(expected) if !expected.is_empty() => {
            let provided = key.unwrap_or_default();
            if !constant_time_eq(&provided, &expected) {
                return error_json(StatusCode::UNAUTHORIZED, "unauthorized");
            }
        }
        _ => {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "admin key not configured");
        }
    }

    state.kill.store(paused, Ordering::SeqCst);
    record_kill_switch(&state, paused).await;
    Json(json!({ "paused": paused })).into_response()
}

/// Best-effort SYSTEM journal entry for a kill-switch mutation (same shape
/// the Telegram kill switch writes; errors are logged, never propagated).
async fn record_kill_switch(state: &DashboardState, paused: bool) {
    let Some(journal) = &state.journal else {
        return;
    };
    let payload = json!({
        "action": "kill_switch",
        "state": if paused { "paused" } else { "resumed" },
    });
    let record = OutcomeRecord {
        trigger: Trigger::System,
        account: "dashboard".to_owned(),
        market_id: None,
        input_hash: hash_input(&[&payload]),
        decision: payload.clone(),
        policy_verdict: json!({ "verdict": "kill_switch" }),
        execution: payload,
    };
    let mut guard = journal.lock().await;
    if let Err(err) = guard.record_outcome(&record, chrono::Utc::now()) {
        tracing::warn!(error = %err, "dashboard kill-switch journal write failed");
    }
}

/// Constant-time string comparison (hash-free XOR fold; the loop length
/// depends only on the longer input, never on where a mismatch happens).
fn constant_time_eq(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();
    let mut diff = provided.len() ^ expected.len();
    let longest = provided.len().max(expected.len());
    for index in 0..longest {
        let left = provided.get(index).copied().unwrap_or(0);
        let right = expected.get(index).copied().unwrap_or(0);
        diff |= (left ^ right) as usize;
    }
    diff == 0
}

/// Read + parse a JSON file (`None` when absent or malformed).
fn read_json_file(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `/api/state` heartbeat block from `data/heartbeat.json`
/// (`{"ts_ms":u64,"tx_hash":str|null,"seq":u64}`, SPEC-P16); nulls when the
/// file is absent or a field is missing.
fn heartbeat_json(path: &Path, now_ms: u64) -> Value {
    let parsed = read_json_file(path);
    let timestamp_ms = parsed
        .as_ref()
        .and_then(|value| value.get("ts_ms"))
        .and_then(Value::as_u64);
    let age_s = timestamp_ms.map(|ts| now_ms.saturating_sub(ts) / 1_000);
    json!({
        "age_s": age_s,
        "tx_hash": parsed
            .as_ref()
            .and_then(|value| value.get("tx_hash"))
            .and_then(Value::as_str),
        "seq": parsed
            .as_ref()
            .and_then(|value| value.get("seq"))
            .and_then(Value::as_u64),
    })
}

/// Current wall-clock time in milliseconds since the Unix epoch (0 before it).
fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_millis() as u64)
}

// ---------------------------------------------------------------------------
// CORS + per-IP rate limit middleware (SPEC-P15 §2; hand-rolled, no deps)
// ---------------------------------------------------------------------------

/// Hand-rolled per-IP token bucket (60 req/min, burst 120).
///
/// Tokens are tracked in milli-units: the bucket refills one unit per
/// millisecond (= 60 per minute) up to the burst cap.
#[derive(Debug)]
struct RateLimiter {
    buckets: std::sync::Mutex<HashMap<String, Bucket>>,
}

/// One client's token bucket.
#[derive(Debug)]
struct Bucket {
    /// Available tokens in milli-units (`TOKEN_SCALE` = one request).
    tokens_milli: u64,
    /// Monotonic timestamp of the last refill.
    last: Instant,
}

impl RateLimiter {
    /// Empty limiter (buckets are created per client on first use).
    fn new() -> Self {
        Self {
            buckets: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Consume one token for `ip`; false when the bucket is exhausted.
    fn allow(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buckets.len() >= MAX_BUCKETS {
            // Bound memory under sustained X-Forwarded-For spoofing: drop
            // idle buckets first, then the least-recently-used one.
            buckets
                .retain(|_, bucket| now.saturating_duration_since(bucket.last) < BUCKET_IDLE_TTL);
        }
        if buckets.len() >= MAX_BUCKETS {
            let oldest = buckets
                .iter()
                .min_by_key(|(_, bucket)| bucket.last)
                .map(|(ip, _)| ip.clone());
            if let Some(oldest) = oldest {
                buckets.remove(&oldest);
            }
        }
        let bucket = buckets.entry(ip.to_owned()).or_insert_with(|| Bucket {
            tokens_milli: RATE_BURST * TOKEN_SCALE,
            last: now,
        });
        let elapsed_ms = u64::try_from(now.saturating_duration_since(bucket.last).as_millis())
            .unwrap_or(u64::MAX);
        bucket.tokens_milli =
            (bucket.tokens_milli + elapsed_ms * REFILL_PER_MS).min(RATE_BURST * TOKEN_SCALE);
        bucket.last = now;
        if bucket.tokens_milli >= TOKEN_SCALE {
            bucket.tokens_milli -= TOKEN_SCALE;
            true
        } else {
            false
        }
    }
}

/// Middleware for every `/api/*` request: rate limit first, answer CORS
/// preflights, and stamp CORS headers on the way out. Non-`/api` paths
/// (`/healthz` lives in a separate merged router; `GET /` is exempt) pass
/// through untouched.
async fn api_guard(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }

    if !limiter.allow(&client_ip(&request)) {
        let mut response = error_json(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        add_cors_headers(&mut response);
        return response;
    }

    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_cors_headers(&mut response);
        return response;
    }

    let mut response = next.run(request).await;
    add_cors_headers(&mut response);
    response
}

/// Client identity for the rate limiter: first `X-Forwarded-For` value, else
/// the `ConnectInfo` socket address, else a shared fallback bucket.
fn client_ip(request: &Request) -> String {
    if let Some(first) = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|first| !first.is_empty())
    {
        return first.to_owned();
    }
    if let Some(connect) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        return connect.0.to_string();
    }
    "unknown".to_owned()
}

/// Stamp the frozen CORS headers on `/api/*` responses.
fn add_cors_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use axum::body::Body;
    use chrono::Utc;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt;

    use sentinel_core::audit::{AuditJournal, IntentRecord, Trigger};
    use sentinel_core::types::{AccountState, ExecutionMode, Market, MarketId};

    use crate::health::HealthState;
    use crate::nansen::spend::SpendEntry;
    use crate::pipeline::LiveState;

    use super::*;

    const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

    // ---- P10 fixtures (unchanged) -----------------------------------------

    fn fixture(entries: usize) -> (TempDir, Arc<Mutex<AuditJournal>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = AuditJournal::open(dir.path()).expect("open journal");
        for index in 0..entries {
            let record = IntentRecord {
                trigger: Trigger::Reflex,
                account: ACCOUNT.to_string(),
                market_id: Some(32),
                input_hash: format!("{:064x}", index + 1),
                decision: json!({ "index": index }),
                policy_verdict: json!({ "verdict": "allow" }),
            };
            journal.record_intent(&record, Utc::now()).expect("record");
        }
        (dir, Arc::new(Mutex::new(journal)))
    }

    #[test]
    fn effective_limit_defaults_and_caps() {
        assert_eq!(effective_limit(None), 50);
        assert_eq!(effective_limit(Some(10)), 10);
        assert_eq!(effective_limit(Some(500)), 500);
        assert_eq!(effective_limit(Some(100_000)), 500, "capped at 500");
        assert_eq!(effective_limit(Some(0)), 1, "degenerate pages clamped");
    }

    #[test]
    fn latest_journal_file_picks_the_lexicographic_maximum() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(latest_journal_file(dir.path()), None);
        std::fs::write(dir.path().join("journal-20260101.jsonl"), "").expect("write");
        std::fs::write(dir.path().join("journal-20260301.jsonl"), "").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "").expect("write");
        assert_eq!(
            latest_journal_file(dir.path()),
            Some(dir.path().join("journal-20260301.jsonl"))
        );
    }

    /// Serve the router on an ephemeral port and exercise both endpoints.
    #[tokio::test]
    async fn audit_router_serves_windows_and_the_verify_report() {
        let (dir, journal) = fixture(4);
        let app = audit_router_with_dir(dir.path(), Arc::clone(&journal));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Full listing.
        let full: Value = reqwest::get(format!("http://{addr}/api/audit"))
            .await
            .expect("GET /api/audit")
            .json()
            .await
            .expect("JSON array");
        let full = full.as_array().expect("array").clone();
        assert_eq!(full.len(), 4);
        let seqs: Vec<u64> = full
            .iter()
            .map(|entry| entry["seq"].as_u64().expect("seq"))
            .collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        assert!(full[0]["entry_hash"].as_str().is_some());
        assert!(full[0]["prev_hash"].as_str().is_some());

        // Window: from_seq=2, limit=2.
        let window: Value = reqwest::get(format!("http://{addr}/api/audit?from_seq=2&limit=2"))
            .await
            .expect("GET window")
            .json()
            .await
            .expect("JSON");
        let window = window.as_array().expect("array").clone();
        assert_eq!(window.len(), 2);
        assert_eq!(window[0]["seq"].as_u64(), Some(2));
        assert_eq!(window[1]["seq"].as_u64(), Some(3));

        // from_seq beyond the head ⇒ empty array.
        let beyond: Value = reqwest::get(format!("http://{addr}/api/audit?from_seq=99"))
            .await
            .expect("GET beyond")
            .json()
            .await
            .expect("JSON");
        assert_eq!(beyond.as_array().expect("array").len(), 0);

        // Verify report shape + verdict.
        let report: Value = reqwest::get(format!("http://{addr}/api/audit/verify"))
            .await
            .expect("GET verify")
            .json()
            .await
            .expect("JSON");
        assert_eq!(report["entries"], 4);
        assert_eq!(report["first_seq"], 0);
        assert_eq!(report["valid_up_to_seq"], 3);
        assert_eq!(report["broken_at"], Value::Null);
        assert!(report["last_hash"].as_str().is_some());
        assert!(report["detail"].is_null());

        server.abort();
    }

    // ---- P15 helpers ------------------------------------------------------

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).expect("decimal")
    }

    fn paths_in(root: &Path) -> DashboardPaths {
        DashboardPaths {
            audit_dir: root.join("audit"),
            spend_ledger: root.join("nansen-spend.jsonl"),
            backtest_report: root.join("backtest-report.json"),
            breaker_state: root.join("breaker-state.json"),
            breaker_journal: root.join("breaker-journal.jsonl"),
            heartbeat: root.join("heartbeat.json"),
        }
    }

    /// State with no journal/live-state; paths rooted in `dir`.
    fn base_state(dir: &Path) -> (DashboardState, Arc<AtomicBool>) {
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let kill = Arc::new(AtomicBool::new(false));
        let state = DashboardState {
            health,
            journal: None,
            kill: Arc::clone(&kill),
            live_state: None,
            paths: paths_in(dir),
        };
        (state, kill)
    }

    fn market_fixture(id: u32, symbol: &str, mmr: &str) -> Market {
        Market {
            id: MarketId(id),
            symbol: symbol.to_string(),
            base: symbol.to_string(),
            price_decimals: 4,
            size_decimals: 3,
            initial_margin_fraction: dec("0.08333"),
            maintenance_margin_fraction: dec(mmr),
            max_leverage: dec("12"),
            min_size: Decimal::ZERO,
            tick_size: dec("0.0001"),
            maker_fee_micros: 0,
            taker_fee_micros: 0,
            order_ttl_blocks: 100,
        }
    }

    fn position_fixture(
        market_id: u32,
        symbol: &str,
        size: &str,
        entry: &str,
        collateral: &str,
        mark: Option<&str>,
    ) -> Position {
        Position {
            market_id: MarketId(market_id),
            symbol: symbol.to_string(),
            size: dec(size),
            entry_price: dec(entry),
            mark_price: mark.map(dec),
            liq_price: None,
            collateral: dec(collateral),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: dec("10"),
            opened_at: None,
        }
    }

    async fn open_journal(dir: &Path) -> Arc<Mutex<AuditJournal>> {
        let journal = AuditJournal::open(dir).expect("open journal");
        Arc::new(Mutex::new(journal))
    }

    async fn record_decision(
        journal: &Arc<Mutex<AuditJournal>>,
        market_id: Option<u32>,
        action: &str,
        trigger: Trigger,
    ) {
        let mut guard = journal.lock().await;
        let record = IntentRecord {
            trigger,
            account: ACCOUNT.to_string(),
            market_id,
            input_hash: format!("{:064x}", guard.seq()),
            decision: json!({ "action": action }),
            policy_verdict: json!({ "verdict": "allow" }),
        };
        guard.record_intent(&record, Utc::now()).expect("record");
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        forwarded_for: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(ip) = forwarded_for {
            builder = builder.header("x-forwarded-for", ip);
        }
        let request = builder.body(Body::empty()).expect("build request");
        app.clone().oneshot(request).await.expect("router call")
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn header_text(response: &axum::response::Response, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    // ---- environment-gated admin key (serialized per process) ------------

    /// Serializes the tests that mutate `DASHBOARD_ADMIN_KEY`. Async-aware
    /// (tokio mutex) so the guard may be held across await points.
    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().await
    }

    fn write_admin_key(value: Option<&std::ffi::OsStr>) {
        // SAFETY: every caller holds ENV_LOCK for the whole test and no other
        // thread in this test process reads DASHBOARD_ADMIN_KEY.
        unsafe {
            match value {
                Some(value) => std::env::set_var("DASHBOARD_ADMIN_KEY", value),
                None => std::env::remove_var("DASHBOARD_ADMIN_KEY"),
            }
        }
    }

    /// RAII admin-key override: restores the previous value on drop (also on
    /// panic), so serialized env tests cannot leak state into each other.
    struct AdminKeyGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl AdminKeyGuard {
        fn new(value: Option<&str>) -> Self {
            let previous = std::env::var_os("DASHBOARD_ADMIN_KEY");
            write_admin_key(value.map(std::ffi::OsStr::new));
            Self { previous }
        }

        fn update(&self, value: Option<&str>) {
            write_admin_key(value.map(std::ffi::OsStr::new));
        }
    }

    impl Drop for AdminKeyGuard {
        fn drop(&mut self) {
            write_admin_key(self.previous.as_deref());
        }
    }

    // ---- P15 endpoint tests ----------------------------------------------

    #[tokio::test]
    async fn index_serves_the_embedded_html() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let response = call(&app, "GET", "/", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_text(&response, "content-type").as_deref(),
            Some("text/html")
        );
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read body");
        assert!(!bytes.is_empty(), "embedded dashboard.html is non-empty");
        std::str::from_utf8(&bytes).expect("embedded dashboard is UTF-8");
    }

    #[tokio::test]
    async fn state_merges_health_live_state_heartbeat_and_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, kill) = base_state(dir.path());
        state.health.touch_feed(wall_now_ms() - 3_000);

        // Journal: one decision per market for last_action.
        let journal = open_journal(&dir.path().join("audit")).await;
        record_decision(&journal, Some(32), "reduce", Trigger::Reflex).await;
        record_decision(&journal, Some(64), "close", Trigger::Strategy).await;
        state.journal = Some(journal);

        // Heartbeat file (SPEC-P16 shape) 5 s old.
        let heartbeat = json!({
            "ts_ms": wall_now_ms() - 5_000,
            "tx_hash": "0xabc",
            "seq": 7,
        });
        std::fs::write(
            dir.path().join("heartbeat.json"),
            serde_json::to_string(&heartbeat).expect("serialize"),
        )
        .expect("write heartbeat");

        // Live state: four markets, five positions (one without a market).
        let mut live = LiveState::new();
        live.set_markets(vec![
            market_fixture(32, "ETH", "0.05"),
            market_fixture(64, "BTC", "0.05"),
            market_fixture(65, "SOL", "0.05"),
            market_fixture(77, "MON", "0.05"),
        ]);
        let positions = vec![
            // long, liq 95, mark 100 ⇒ 5% ⇒ red; stale mark 999 must be
            // replaced by the marks map value.
            position_fixture(32, "ETH", "1", "100", "10", Some("999")),
            // long, liq 75, mark 100 ⇒ 25% ⇒ green (soft boundary).
            position_fixture(64, "BTC", "1", "100", "30", Some("100")),
            // long, liq 83, mark 100 ⇒ 17% ⇒ yellow.
            position_fixture(65, "SOL", "1", "100", "22", Some("100")),
            // short: liq 220, mark 100 ⇒ (220-100)/100 ⇒ 120% ⇒ green.
            position_fixture(77, "MON", "-2", "200", "60", Some("100")),
            // market 99 is unknown ⇒ no distance/liq/tier.
            position_fixture(99, "???", "1", "100", "10", None),
        ];
        live.account = Some(AccountState {
            positions,
            free_balance: dec("1000"),
            equity: dec("1002.5"),
            fee_tier: 0,
            snapshot_ts: Utc::now(),
        });
        live.marks.insert(MarketId(32), dec("100"));
        live.marks.insert(MarketId(64), dec("100"));
        live.marks.insert(MarketId(65), dec("100"));
        live.marks.insert(MarketId(77), dec("100"));
        state.live_state = Some(Arc::new(Mutex::new(live)));

        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/state", None).await).await;
        let object = body.as_object().expect("state object");
        assert_eq!(object.len(), 9, "no extra keys: {object:?}");
        for key in [
            "mode",
            "version",
            "uptime_s",
            "feed_age_s",
            "feed_fresh",
            "paused",
            "heartbeat",
            "account",
            "positions",
        ] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(body["mode"], "DRY_RUN");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body["uptime_s"].as_u64().is_some());
        let feed_age = body["feed_age_s"].as_u64().expect("feed touched");
        assert!(
            (3..=5).contains(&feed_age),
            "feed_age_s ≈ 3, got {feed_age}"
        );
        assert_eq!(body["feed_fresh"], json!(true));
        assert_eq!(body["paused"], json!(false));
        assert_eq!(body["heartbeat"]["seq"], 7);
        assert_eq!(body["heartbeat"]["tx_hash"], "0xabc");
        let heartbeat_age = body["heartbeat"]["age_s"].as_u64().expect("age");
        assert!(
            (4..=10).contains(&heartbeat_age),
            "heartbeat age ≈ 5, got {heartbeat_age}"
        );
        assert_eq!(body["account"]["free_balance"], "1000");
        assert_eq!(body["account"]["equity"], "1002.5");

        let positions = body["positions"].as_array().expect("positions array");
        assert_eq!(positions.len(), 5);
        let find = |id: u64| {
            positions
                .iter()
                .find(|position| position["market_id"] == json!(id))
                .expect("position present")
        };

        let eth = find(32);
        assert_eq!(eth["symbol"], "ETH");
        assert_eq!(eth["side"], "long");
        assert_eq!(eth["size"], "1", "decimals serialize as strings");
        assert_eq!(eth["collateral"], "10");
        assert_eq!(
            dec(eth["distance_pct"].as_str().expect("distance string")),
            dec("5")
        );
        assert_eq!(eth["tier"], "red");
        assert_eq!(
            dec(eth["liq_price"].as_str().expect("liq string")),
            dec("95"),
            "derived liquidation price"
        );
        assert_eq!(
            eth["mark_price"], "100",
            "mark refreshed from the marks map"
        );
        assert_eq!(eth["last_action"], "reduce");

        let btc = find(64);
        assert_eq!(
            dec(btc["distance_pct"].as_str().expect("distance")),
            dec("25"),
            "exactly at soft"
        );
        assert_eq!(btc["tier"], "green");
        assert_eq!(btc["last_action"], "close");

        let sol = find(65);
        assert_eq!(
            dec(sol["distance_pct"].as_str().expect("distance")),
            dec("17")
        );
        assert_eq!(sol["tier"], "yellow");
        assert_eq!(sol["last_action"], Value::Null);

        let mon = find(77);
        assert_eq!(mon["side"], "short");
        assert_eq!(
            dec(mon["distance_pct"].as_str().expect("distance")),
            dec("120"),
            "short distance uses the short liq"
        );
        assert_eq!(mon["tier"], "green");

        let unknown = find(99);
        assert_eq!(unknown["distance_pct"], Value::Null);
        assert_eq!(unknown["liq_price"], Value::Null);
        assert_eq!(unknown["tier"], Value::Null);
        assert_eq!(unknown["mark_price"], Value::Null);

        // Kill flag flips `paused`.
        kill.store(true, Ordering::SeqCst);
        let body = body_json(call(&app, "GET", "/api/state", None).await).await;
        assert_eq!(body["paused"], json!(true));
    }

    #[tokio::test]
    async fn state_reports_stale_feed_when_stale_secs_is_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, _kill) = base_state(dir.path());
        let live = Arc::new(Mutex::new(LiveState::new()));
        state.live_state = Some(Arc::clone(&live));
        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/state", None).await).await;
        assert_eq!(body["feed_fresh"], json!(true));

        live.lock().await.stale_secs = Some(12);
        let body = body_json(call(&app, "GET", "/api/state", None).await).await;
        assert_eq!(body["feed_fresh"], json!(false));
    }

    #[tokio::test]
    async fn state_degrades_to_nulls_without_live_state_or_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/state", None).await).await;
        assert_eq!(body["account"], Value::Null);
        assert_eq!(body["positions"], json!([]));
        assert_eq!(
            body["heartbeat"],
            json!({ "age_s": null, "tx_hash": null, "seq": null })
        );
        assert_eq!(body["feed_fresh"], json!(false));
        assert_eq!(body["paused"], json!(false));
        assert_eq!(body["feed_age_s"], Value::Null);
    }

    #[tokio::test]
    async fn decisions_defaults_caps_and_returns_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, _kill) = base_state(dir.path());
        let journal = open_journal(&dir.path().join("audit")).await;
        for index in 0..130 {
            record_decision(&journal, Some(32), &format!("act-{index}"), Trigger::Reflex).await;
        }
        state.journal = Some(journal);
        let app = dashboard_router(state);

        // Default: newest 20, descending seq.
        let body = body_json(call(&app, "GET", "/api/decisions", None).await).await;
        let items = body.as_array().expect("decisions array");
        assert_eq!(items.len(), 20);
        assert_eq!(items[0]["seq"], 129);
        assert_eq!(items[19]["seq"], 110);
        assert_eq!(items[0]["decision"]["action"], "act-129");
        let first = items[0].as_object().expect("entry object");
        assert_eq!(first.len(), 8, "only the frozen eight keys");
        for key in [
            "seq",
            "ts",
            "trigger",
            "market_id",
            "decision",
            "policy_verdict",
            "execution",
            "entry_hash",
        ] {
            assert!(first.contains_key(key), "missing key {key}");
        }
        assert_eq!(items[0]["trigger"], "REFLEX");
        assert_eq!(items[0]["market_id"], 32);
        assert!(items[0]["ts"].is_string());
        assert_eq!(
            items[0]["entry_hash"].as_str().expect("hash").len(),
            64,
            "sha256 hex"
        );

        // Explicit window.
        let body = body_json(call(&app, "GET", "/api/decisions?limit=5", None).await).await;
        let items = body.as_array().expect("array");
        assert_eq!(items.len(), 5);
        assert_eq!(items[0]["seq"], 129);
        assert_eq!(items[4]["seq"], 125);

        // Cap at 100.
        let body = body_json(call(&app, "GET", "/api/decisions?limit=1000", None).await).await;
        let items = body.as_array().expect("array");
        assert_eq!(items.len(), 100);
        assert_eq!(items[0]["seq"], 129);
        assert_eq!(items[99]["seq"], 30);

        // Degenerate limit clamps to 1.
        let body = body_json(call(&app, "GET", "/api/decisions?limit=0", None).await).await;
        let items = body.as_array().expect("array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["seq"], 129);
    }

    #[tokio::test]
    async fn decisions_and_audit_degrade_without_a_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        // Decisions panel: empty array (no journal wired).
        let response = call(&app, "GET", "/api/decisions", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!([]));

        // P10 behavior: audit endpoints are not mounted ⇒ 404.
        let response = call(&app, "GET", "/api/audit", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = call(&app, "GET", "/api/audit/verify", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audit_endpoints_keep_p10_behavior_through_dashboard_router() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, _kill) = base_state(dir.path());
        let journal = open_journal(&dir.path().join("audit")).await;
        for index in 0..4 {
            record_decision(&journal, Some(32), &format!("act-{index}"), Trigger::Reflex).await;
        }
        state.journal = Some(journal);
        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/audit", None).await).await;
        let items = body.as_array().expect("array");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["seq"], 0);
        assert!(items[0]["prev_hash"].as_str().is_some(), "full AuditEntry");
        assert!(items[0]["account"].as_str().is_some());

        let body = body_json(call(&app, "GET", "/api/audit/verify", None).await).await;
        assert_eq!(body["entries"], 4);
        assert_eq!(body["valid_up_to_seq"], 3);
        assert_eq!(body["broken_at"], Value::Null);
    }

    #[tokio::test]
    async fn backtest_serves_raw_report_and_404s_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let report_path = state.paths.backtest_report.clone();
        let app = dashboard_router(state);

        let response = call(&app, "GET", "/api/backtest", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "backtest report not found" })
        );

        let raw = "{\n  \"scenarios\": [],\n  \"capital_saved_usd\": \"0\"\n}\n";
        std::fs::write(&report_path, raw).expect("write report");
        let response = call(&app, "GET", "/api/backtest", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_text(&response, "content-type").as_deref(),
            Some("application/json")
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        assert_eq!(bytes.as_ref(), raw.as_bytes(), "raw file contents served");
    }

    #[tokio::test]
    async fn nansen_spend_reports_totals_windows_and_recent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let ledger_path = state.paths.spend_ledger.clone();
        let app = dashboard_router(state);

        let now_ms = wall_now_ms();
        let ledger = SpendLedger::new(ledger_path);
        // 25 entries one minute apart (all inside the hour) + one two hours
        // old: 26 total.
        for index in 0..25u64 {
            ledger
                .append(&SpendEntry {
                    ts_ms: now_ms - index * 60_000,
                    endpoint: "/api/v1/smart-money/netflow".to_string(),
                    cost_usd: "0.01".to_string(),
                    tx_hash: Some(format!("0x{:064x}", index)),
                    payer: None,
                    network: None,
                })
                .expect("append");
        }
        ledger
            .append(&SpendEntry {
                ts_ms: now_ms - 2 * HOUR_MS,
                endpoint: "/api/v1/profiler".to_string(),
                cost_usd: "1.00".to_string(),
                tx_hash: None,
                payer: None,
                network: None,
            })
            .expect("append");

        let body = body_json(call(&app, "GET", "/api/nansen/spend", None).await).await;
        let object = body.as_object().expect("spend object");
        assert_eq!(object.len(), 7, "frozen keys");
        assert_eq!(body["total_calls"], 26);
        assert_eq!(
            dec(body["total_cost_usd"].as_str().expect("cost string")),
            dec("1.25")
        );
        assert_eq!(body["calls_1h"], 25);
        assert_eq!(
            dec(body["cost_24h_usd"].as_str().expect("cost string")),
            dec("1.25")
        );
        match std::env::var("NANSEN_MAX_CALLS_PER_HOUR")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            Some(max) => assert_eq!(body["max_calls_per_hour"], json!(max)),
            None => assert_eq!(body["max_calls_per_hour"], Value::Null),
        }
        assert_eq!(body["cache"], json!({ "hits": null, "misses": null }));

        let recent = body["recent"].as_array().expect("recent array");
        assert_eq!(recent.len(), SPEND_RECENT, "last 20 entries");
        assert_eq!(recent[0]["endpoint"], "/api/v1/smart-money/netflow");
        assert_eq!(recent[19]["endpoint"], "/api/v1/profiler");
        assert_eq!(recent[19]["tx_hash"], Value::Null);
        assert_eq!(recent[0]["ts_ms"], json!(now_ms - 6 * 60_000));
    }

    #[tokio::test]
    async fn nansen_spend_degrades_to_zeros_without_a_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/nansen/spend", None).await).await;
        assert_eq!(body["total_calls"], 0);
        assert_eq!(body["total_cost_usd"], "0");
        assert_eq!(body["calls_1h"], 0);
        assert_eq!(body["cost_24h_usd"], "0");
        assert_eq!(body["recent"], json!([]));
        assert_eq!(body["cache"], json!({ "hits": null, "misses": null }));
    }

    #[tokio::test]
    async fn breaker_status_absent_then_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let state_path = state.paths.breaker_state.clone();
        let journal_path = state.paths.breaker_journal.clone();
        let app = dashboard_router(state);

        let body = body_json(call(&app, "GET", "/api/breaker-status", None).await).await;
        assert_eq!(body["available"], json!(false));
        assert_eq!(body["state"], Value::Null);
        assert_eq!(body["journal_tail"], json!([]));

        let breaker_state = json!({
            "guardians": { "0xabc": { "last_ts_ms": 1, "fired_epoch": 2 } }
        });
        std::fs::write(
            &state_path,
            serde_json::to_string(&breaker_state).expect("serialize"),
        )
        .expect("write state");
        let mut lines = String::new();
        for index in 1..=5 {
            lines.push_str(&format!("{{\"line\":{index}}}\n"));
        }
        lines.push_str("{ not json\n\n");
        std::fs::write(&journal_path, lines).expect("write journal");

        let body = body_json(call(&app, "GET", "/api/breaker-status", None).await).await;
        assert_eq!(body["available"], json!(true));
        assert_eq!(body["state"], breaker_state);
        assert_eq!(
            body["journal_tail"],
            json!([{ "line": 3 }, { "line": 4 }, { "line": 5 }])
        );
    }

    #[tokio::test]
    async fn pause_resume_require_the_admin_key_and_toggle_the_kill_flag() {
        let _env = env_guard().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, kill) = base_state(dir.path());
        let journal = open_journal(&dir.path().join("audit")).await;
        record_decision(&journal, Some(32), "act", Trigger::Reflex).await;
        state.journal = Some(Arc::clone(&journal));
        let app = dashboard_router(state);

        let admin = AdminKeyGuard::new(None);
        // Unset key ⇒ 503 for both mutations, with or without ?key=.
        let response = call(&app, "POST", "/api/pause?key=whatever", None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "admin key not configured" })
        );
        let response = call(&app, "POST", "/api/resume", None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!kill.load(Ordering::SeqCst));

        admin.update(Some("s3cret"));
        // Wrong key ⇒ 401, flag untouched.
        let response = call(&app, "POST", "/api/pause?key=wrong", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "unauthorized" })
        );
        // Missing key ⇒ 401 too.
        let response = call(&app, "POST", "/api/pause", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!kill.load(Ordering::SeqCst));

        // Correct key ⇒ 200 + flag + SYSTEM journal entry.
        let response = call(&app, "POST", "/api/pause?key=s3cret", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "paused": true }));
        assert!(kill.load(Ordering::SeqCst));

        let entries = {
            let guard = journal.lock().await;
            guard.read_entries(0, 10).expect("read")
        };
        let last = entries.last().expect("kill-switch entry");
        assert_eq!(last.trigger, Trigger::System);
        assert_eq!(last.decision["action"], "kill_switch");
        assert_eq!(last.decision["state"], "paused");

        // Resume ⇒ 200 + flag cleared + entry.
        let response = call(&app, "POST", "/api/resume?key=s3cret", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "paused": false }));
        assert!(!kill.load(Ordering::SeqCst));
        let entries = {
            let guard = journal.lock().await;
            guard.read_entries(0, 10).expect("read")
        };
        assert_eq!(entries.len(), 3, "seed + pause + resume");
        let last = entries.last().expect("kill-switch entry");
        assert_eq!(last.decision["state"], "resumed");
    }

    #[tokio::test]
    async fn pause_without_a_journal_still_toggles_the_kill_flag() {
        let _env = env_guard().await;
        let _admin = AdminKeyGuard::new(Some("k"));
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let response = call(&app, "POST", "/api/pause?key=k", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "paused": true }));
        assert!(kill.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn pause_survives_a_failing_journal_write() {
        let _env = env_guard().await;
        let _admin = AdminKeyGuard::new(Some("k"));
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut state, kill) = base_state(dir.path());
        let audit_dir = dir.path().join("audit");
        let journal = open_journal(&audit_dir).await;
        state.journal = Some(journal);
        let app = dashboard_router(state);

        // A journal whose directory vanished: record_outcome errors out and
        // the best-effort write must keep the response at 200.
        std::fs::remove_dir_all(&audit_dir).expect("remove audit dir");
        let response = call(&app, "POST", "/api/resume?key=k", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!({ "paused": false }));
        assert!(!kill.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn api_rate_limit_bursts_per_ip_and_exempts_the_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let ip = "203.0.113.7";
        for _ in 0..RATE_BURST {
            let response = call(&app, "GET", "/api/state", Some(ip)).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = call(&app, "GET", "/api/state", Some(ip)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            header_text(&response, "access-control-allow-origin").as_deref(),
            Some("*"),
            "429s still carry CORS headers"
        );
        assert_eq!(
            body_json(response).await,
            json!({ "error": "rate limited" })
        );

        // Other clients keep their own bucket.
        let response = call(&app, "GET", "/api/state", Some("203.0.113.8")).await;
        assert_eq!(response.status(), StatusCode::OK);

        // GET / is exempt for the exhausted IP.
        for _ in 0..5 {
            let response = call(&app, "GET", "/", Some(ip)).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn healthz_is_exempt_from_the_dashboard_rate_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let health = Arc::clone(&state.health);
        let app = crate::health::router(health).merge(dashboard_router(state));

        for _ in 0..130 {
            let response = call(&app, "GET", "/healthz", Some("203.0.113.99")).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn api_responses_carry_cors_headers_and_answer_preflight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let response = call(&app, "GET", "/api/state", None).await;
        assert_eq!(
            header_text(&response, "access-control-allow-origin").as_deref(),
            Some("*")
        );
        assert_eq!(
            header_text(&response, "access-control-allow-methods").as_deref(),
            Some("GET, POST, OPTIONS")
        );
        assert!(
            header_text(&response, "access-control-allow-headers").is_some(),
            "allow-headers present"
        );

        // Preflight is answered before routing, so the auth gate never sees it.
        let response = call(&app, "OPTIONS", "/api/pause", None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header_text(&response, "access-control-allow-origin").as_deref(),
            Some("*")
        );
        assert_eq!(
            header_text(&response, "access-control-allow-methods").as_deref(),
            Some("GET, POST, OPTIONS")
        );
    }

    #[tokio::test]
    async fn unknown_routes_are_404s() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _kill) = base_state(dir.path());
        let app = dashboard_router(state);

        let response = call(&app, "GET", "/api/nope", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = call(&app, "GET", "/definitely-not-a-route", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
