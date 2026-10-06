//! Audit-facing HTTP API (merged into the daemon router by `main`; P15 extends).
//!
//! Implemented per `SPEC-P10.md` §7:
//!
//! - `GET /api/audit?from_seq&limit` — windowed journal listing (JSON array);
//!   `from_seq` defaults to 0, `limit` defaults to 50 and is capped at 500;
//! - `GET /api/audit/verify` — `VerifyReport` JSON for the journal's current
//!   UTC-day file (`journal-YYYYMMDD.jsonl`), falling back to the
//!   lexicographically-latest journal file when today's does not exist yet.
//!
//! The frozen core handle (`AuditJournal`) resumes from a directory but does
//! not expose it, so the router is told where the journal lives:
//! [`audit_router`] defaults to [`DEFAULT_AUDIT_DIR`] (override with the
//! `AUDIT_DIR` env var), and [`audit_router_with_dir`] takes an explicit
//! directory (tests, non-default deployments).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use sentinel_core::audit::{AuditJournal, VerifyReport, verify_chain};

/// Default journal directory (the daemon opens `data/audit`, `main.rs`).
pub const DEFAULT_AUDIT_DIR: &str = "data/audit";

/// Page size used when `limit` is absent from the query.
const DEFAULT_LIMIT: usize = 50;

/// Largest page size accepted from the query.
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

/// JSON error body for internal failures.
fn error_json(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use sentinel_core::audit::{IntentRecord, Trigger};

    use super::*;

    const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

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
}
/// Aggregated handles for the dashboard + public API surface (SPEC-P15 §3).
#[derive(Clone)]
pub struct DashboardState {
    /// Health surface (mode, uptime, feed freshness, version).
    pub health: Arc<crate::health::HealthState>,
    /// Audit journal handle (`None`: audit endpoints degrade to 404s).
    pub journal: Option<Arc<Mutex<AuditJournal>>>,
    /// Kill switch shared with the pipeline (pause/resume mutations).
    pub kill: Arc<std::sync::atomic::AtomicBool>,
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

/// Full dashboard + API router (SPEC-P15 §2) — implemented by the P15 wave.
///
/// Until then this serves the audit endpoints (P10 behavior preserved) plus a
/// placeholder `/`. The P15 wave replaces the placeholder with the embedded
/// `dashboard/index.html` and adds the `/api/*` panels.
pub fn dashboard_router(state: DashboardState) -> Router {
    let base = match &state.journal {
        Some(journal) => audit_router(Arc::clone(journal)),
        None => Router::new(),
    };
    base.route("/", get(|| async { "STUB: dashboard pending (P15 wave)" }))
}
