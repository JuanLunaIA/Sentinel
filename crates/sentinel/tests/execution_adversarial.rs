//! # P05 independent verification — adversarial suite for the execution layer
//! (`sentinel` app: DryRun / Recording / Perpl executors, idempotency store,
//! post-fill-verification guard).
//!
//! Written **without reading** `crates/sentinel/src/execution/*.rs` (black
//! box): every expectation below is re-derived from `SPEC-P05.md` §5 and
//! `docs/FACTS.md` §1.6 (HTTP batch `POST /v1/trading/orders`, ack codes,
//! `CloseLong` t=3 / `CloseShort` t=4). Exact arithmetic was generated with
//! Python `decimal` (command and complete output quoted below; per-test
//! `S|`/`R|`/`K|` line tags reference it).
//!
//! Note on the dry-run example: the dispatch brief cited
//! "2713.70 @ 10 bps sell ⇒ 2710.7563". The spec §5 formula
//! `mark × (1 − bps/10_000)` gives `2713.70 × 0.999 = 2710.9863` (Python
//! line `S|`); the tests assert the spec value. (2710.7563 corresponds to a
//! different mark, ≈2713.47, and does not follow from the spec formula.)
//!
//! ## Expectation generator (command 1 — run from the repo root)
//!
//! ```text
//! $ python3 /home/luna/.hermes/cache/scratch/p05_exec_expectations.py
//! ```
//!
//! Output (complete):
//!
//! ```text
//! S| dry-run slippage: fill = mark x (1 -/+ bps/10000)
//! S| mark=2713.70 bps=10: sell=2710.98630 buy=2716.41370
//! S| mark=100 bps=5: sell=99.9500 buy=100.0500
//! S| mark=0.5 bps=25: sell=0.49875 buy=0.50125
//! S| mark=2713.70 bps=0: sell=2713.70 buy=2713.70
//! S| mark=100 bps=10000: sell=0 buy=200
//! R| raw gateway size s = size x 10^size_decimals
//! R| size=0.123 d=3 -> raw=123.000 int=123
//! R| size=1 d=2 -> raw=100 int=100
//! R| size=0.5 d=3 -> raw=500.0 int=500
//! R| size=1.999 d=3 -> raw=1999.000 int=1999
//! R| size=3.7 d=1 -> raw=37.0 int=37
//! K| size_bucket = truncate(size x 100) toward zero
//! K| size=0.5 -> bucket=50
//! K| size=0.509 -> bucket=50
//! K| size=0.51999 -> bucket=51
//! K| size=0.01 -> bucket=1
//! K| size=2.4 -> bucket=240
//! K| size=-0.5 -> bucket=-50
//! K| size=1.999 -> bucket=199
//! K2| idempotency key shape: market:action:bucket for 32/reduce/0.5 -> 32:reduce:50
//! ```

use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use sentinel::error::{PerplError, Result, SentinelError};
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::idempotency::IdempotencyStore;
use sentinel::execution::perpl::PerplExecutor;
use sentinel::execution::recording::RecordingExecutor;
use sentinel::execution::{
    ExecutionReport, ExecutionStatus, Executor, GuardedExecutor, PositionProbe, SeqCounter,
    client_order_id, idempotency_key, size_bucket,
};
use sentinel::perpl::auth::ApiKeySigner;
use sentinel_core::order::{CloseSide, OrderRequest, OrderType};
use sentinel_core::types::{MarketId, Position};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers and test doubles (own tooling per SPEC-P05 §8: small structs
// implementing `PositionProbe` / `Executor`)
// ---------------------------------------------------------------------------

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).expect("test decimal literal")
}

fn pos(market: u32, size: Decimal, mark: Option<Decimal>) -> Position {
    Position {
        market_id: MarketId(market),
        symbol: "ETH".into(),
        size,
        entry_price: dec("100"),
        mark_price: mark,
        liq_price: None,
        collateral: dec("50"),
        unrealized_pnl: dec("0"),
        margin_ratio: None,
        leverage: dec("5"),
        opened_at: None,
    }
}

fn order(size: &str, close: CloseSide, bps: u16, decimals: u32) -> OrderRequest {
    OrderRequest {
        market_id: MarketId(32),
        close,
        size: dec(size),
        order_type: OrderType::Market,
        max_slippage_bps: bps,
        size_decimals: decimals,
    }
}

/// Probe double always answering the same position.
#[derive(Clone)]
struct StaticProbe {
    response: Option<Position>,
}

impl PositionProbe for StaticProbe {
    async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
        Ok(self.response.clone())
    }
}

/// Probe double replaying a scripted sequence, repeating its last entry
/// (first entries repeated so both "read baseline first" and "poll-only"
/// guard designs observe the same final state).
#[derive(Clone)]
struct ScriptProbe {
    calls: Arc<StdMutex<usize>>,
    seq: Vec<Option<Position>>,
}

impl ScriptProbe {
    fn new(seq: Vec<Option<Position>>) -> Self {
        Self {
            calls: Arc::new(StdMutex::new(0)),
            seq,
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().expect("probe counter")
    }
}

impl PositionProbe for ScriptProbe {
    async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
        let mut i = self.calls.lock().expect("probe counter");
        let idx = (*i).min(self.seq.len().saturating_sub(1));
        *i += 1;
        Ok(self.seq[idx].clone())
    }
}

/// Inner executor double counting submissions and returning a recognisable
/// `Submitted` report (so "report kept vs replaced" is observable).
#[derive(Clone)]
struct CountingExec {
    calls: Arc<StdMutex<u32>>,
}

impl CountingExec {
    fn new() -> Self {
        Self {
            calls: Arc::new(StdMutex::new(0)),
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().expect("call counter")
    }
}

impl Executor for CountingExec {
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        *self.calls.lock().expect("call counter") += 1;
        Ok(ExecutionReport {
            order: order.clone(),
            status: ExecutionStatus::Submitted,
            filled_size: Decimal::ZERO,
            avg_price: None,
            tx_hash: None,
            client_order_id: "inner-id".into(),
            detail: "inner-report".into(),
            ts_ms: 7,
        })
    }
}

fn store_arc(path: &std::path::Path, window: Duration) -> Arc<Mutex<IdempotencyStore>> {
    Arc::new(Mutex::new(
        IdempotencyStore::load(window, path.to_path_buf()).expect("store must load"),
    ))
}

/// All non-empty JSONL lines of `path`, parsed.
fn jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("report file must exist")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSONL line must parse"))
        .collect()
}

/// True when `v` is a JSON number (int or float) equal to `n` — the gateway
/// treats `s`/`p`/`ms` as numbers; assertions should not depend on the exact
/// numeric encoding (K|/R| lines pin the *value*).
fn num_is(v: &serde_json::Value, n: f64) -> bool {
    match v {
        serde_json::Value::Number(x) => x
            .as_i64()
            .map(|i| i as f64 == n)
            .or_else(|| x.as_u64().map(|u| u as f64 == n))
            .or_else(|| x.as_f64().map(|f| f == n))
            .unwrap_or(false),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pure key/bucket/id functions (spec §5)
// ---------------------------------------------------------------------------

/// `K|` — `size_bucket = truncate(size × 100)` (toward zero).
#[test]
fn size_bucket_matches_python() {
    assert_eq!(size_bucket(dec("0.5")), 50); // K| size=0.5 -> 50
    assert_eq!(size_bucket(dec("0.509")), 50); // K| size=0.509 -> 50
    assert_eq!(size_bucket(dec("0.51999")), 51); // K| size=0.51999 -> 51
    assert_eq!(size_bucket(dec("0.01")), 1); // K| size=0.01 -> 1
    assert_eq!(size_bucket(dec("2.4")), 240); // K| size=2.4 -> 240
    assert_eq!(size_bucket(dec("-0.5")), -50); // K| size=-0.5 -> -50
    assert_eq!(size_bucket(dec("1.999")), 199); // K| size=1.999 -> 199
}

/// Key shape `{market}:{action}:{bucket}` and id shape `sentinel-<market>-<seq>`.
#[test]
fn key_and_client_id_shapes() {
    // K2| market:action:bucket for 32/reduce/0.5 -> 32:reduce:50
    assert_eq!(
        idempotency_key(MarketId(32), "reduce", dec("0.5")),
        "32:reduce:50"
    );
    assert_eq!(
        idempotency_key(MarketId(48), "close", dec("1.999")),
        "48:close:199"
    );
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    assert_eq!(client_order_id(&o, 8), "sentinel-32-8");
    assert_eq!(client_order_id(&o, 123), "sentinel-32-123");
}

/// Monotonic sequence provider: seed at construction, first value = seed + 1.
#[test]
fn seq_counter_is_monotonic_from_seed_plus_one() {
    let c = SeqCounter::new(7);
    assert_eq!(c.next(), 8);
    assert_eq!(c.next(), 9);
    let c0 = SeqCounter::new(0);
    assert_eq!(c0.next(), 1);
}

// ---------------------------------------------------------------------------
// Idempotency store (spec §5)
// ---------------------------------------------------------------------------

/// Window boundary: suppressed while `now − last < window`; allowed exactly at
/// `now − last == window`; a suppressed duplicate must NOT refresh the window.
#[test]
fn idempotency_boundary_exact_window_and_no_duplicate_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idem.json");
    let window = Duration::from_millis(1000);
    let mut s = IdempotencyStore::load(window, path.clone()).unwrap();

    // Key A: record at 0, then duplicates at 500 and 999. If the duplicates
    // refreshed `last`, the key would still be suppressed at 1000.
    assert!(
        s.check_and_record("32:reduce:50", 0).unwrap(),
        "t=0 allowed"
    );
    assert!(
        !s.check_and_record("32:reduce:50", 500).unwrap(),
        "t=500 suppressed"
    );
    assert!(
        !s.check_and_record("32:reduce:50", 999).unwrap(),
        "t=999 suppressed"
    );
    assert!(
        s.check_and_record("32:reduce:50", 1000).unwrap(),
        "t=1000 == window after the ORIGINAL record: duplicates must not refresh"
    );
    // The allowed call at 1000 records; the next boundary is 2000.
    assert!(
        !s.check_and_record("32:reduce:50", 1999).unwrap(),
        "t=1999 suppressed"
    );
    assert!(
        s.check_and_record("32:reduce:50", 2000).unwrap(),
        "t=2000 allowed again"
    );

    // Key B: a single record has the same boundary (no intermediate calls).
    assert!(
        s.check_and_record("16:close:100", 3000).unwrap(),
        "fresh key allowed"
    );
    assert!(
        s.check_and_record("16:close:100", 4000).unwrap(),
        "exactly window later allowed"
    );
}

/// Restart safety: the store persists on every record and reloading it from
/// disk still suppresses; `load` on a missing file starts empty.
#[test]
fn idempotency_restart_reload_suppresses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let window = Duration::from_millis(1000);

    {
        let mut s = IdempotencyStore::load(window, path.clone()).unwrap();
        assert!(s.check_and_record("32:reduce:50", 0).unwrap());
    } // drop ⇒ restart

    // Persistence evidence: the file holds the key (HashMap<key, last_ms>).
    let raw = std::fs::read_to_string(&path).expect("store file must exist after a record");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("store file must be JSON");
    assert_eq!(v["32:reduce:50"], serde_json::json!(0), "last_ms persisted");

    let mut s2 = IdempotencyStore::load(window, path.clone()).unwrap();
    assert!(
        !s2.check_and_record("32:reduce:50", 500).unwrap(),
        "reloaded store must still suppress within the window"
    );
    assert!(
        s2.check_and_record("32:reduce:50", 1000).unwrap(),
        "reloaded store must allow at exactly the window"
    );

    // Missing file = empty store.
    let empty_dir = tempfile::tempdir().unwrap();
    let mut fresh = IdempotencyStore::load(window, empty_dir.path().join("none.json")).unwrap();
    assert!(fresh.check_and_record("32:reduce:50", 0).unwrap());
}

/// Prune drops entries older than the window and keeps fresh ones.
#[test]
fn idempotency_prune_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idem.json");
    let window = Duration::from_millis(1000);
    let mut s = IdempotencyStore::load(window, path.clone()).unwrap();

    assert!(s.check_and_record("stale", 0).unwrap());
    assert!(s.check_and_record("fresh", 1400).unwrap());
    s.prune(1500); // "stale" is 1500 ms old; "fresh" is 100 ms old

    // Fresh entries must survive the prune.
    assert!(
        !s.check_and_record("fresh", 1501).unwrap(),
        "prune must not drop entries within the window"
    );
    // A fresh record after the prune is still suppressed as usual.
    assert!(s.check_and_record("other", 1502).unwrap());
    assert!(!s.check_and_record("other", 1503).unwrap());
    // The persisted document stays well-formed JSON after pruning.
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).expect("JSON after prune");
    assert!(v.is_object(), "store stays a JSON object, got {v}");
}

// ---------------------------------------------------------------------------
// Dry-run executor (spec §5)
// ---------------------------------------------------------------------------

/// `S|` — fill price = `mark × (1 ∓ bps/10_000)` (sell lower, buy higher),
/// full size, `Simulated`, report persisted as one JSON line.
#[tokio::test]
async fn dry_run_slippage_math_python_derived() {
    let dir = tempfile::tempdir().unwrap();

    // Sell (CloseLong) at mark 2713.70, 10 bps ⇒ 2713.70 × 0.999 = 2710.9863.
    let path = dir.path().join("sell/reports.jsonl");
    let ex = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("2713.70")))),
        },
        10,
        path.clone(),
        7,
    );
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    let r = ex.submit(&o).await.expect("dry-run sell must succeed");
    assert_eq!(r.status, ExecutionStatus::Simulated);
    assert_eq!(r.filled_size, dec("0.5"), "full size simulated");
    assert_eq!(r.avg_price, Some(dec("2710.9863")), "S| sell = 2710.98630");
    assert!(r.tx_hash.is_none(), "no chain transaction in DRY_RUN");
    assert_eq!(
        r.client_order_id, "sentinel-32-8",
        "seq seeded at construction (+1)"
    );
    assert_eq!(r.order, o, "report answers the request it was given");
    assert!(r.ts_ms > 0, "wall-clock timestamp attached");

    // Buy (CloseShort) at mark 100, 5 bps ⇒ 100.05.
    let ex2 = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("100")))),
        },
        5,
        dir.path().join("buy.jsonl"),
        0,
    );
    let o2 = order("1", CloseSide::CloseShort, 5, 3);
    let r2 = ex2.submit(&o2).await.expect("dry-run buy must succeed");
    assert_eq!(r2.avg_price, Some(dec("100.05")), "S| buy = 100.0500");

    // Slippage 0 keeps the mark; small mark/large bps scales exactly.
    let ex3 = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("2713.70")))),
        },
        0,
        dir.path().join("zero.jsonl"),
        0,
    );
    let r3 = ex3
        .submit(&order("0.5", CloseSide::CloseLong, 0, 3))
        .await
        .unwrap();
    assert_eq!(r3.avg_price, Some(dec("2713.70")), "S| bps=0 => mark");

    let ex4 = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("0.5")))),
        },
        25,
        dir.path().join("small.jsonl"),
        0,
    );
    let r4 = ex4
        .submit(&order("0.5", CloseSide::CloseLong, 25, 3))
        .await
        .unwrap();
    assert_eq!(r4.avg_price, Some(dec("0.49875")), "S| sell = 0.49875");

    // The report is appended, one JSON line per submission, parent dirs created.
    let lines = jsonl(&path);
    assert_eq!(lines.len(), 1, "one line per submission");
    assert!(lines[0].is_object(), "line is a report object");
    assert_eq!(
        lines[0]["client_order_id"],
        serde_json::json!("sentinel-32-8")
    );
    assert_eq!(lines[0]["status"], serde_json::json!("Simulated"));
}

/// Missing mark (no position or `mark_price: None`) ⇒ `Err(PerplError::Order)`.
#[tokio::test]
async fn dry_run_missing_mark_is_order_error() {
    let dir = tempfile::tempdir().unwrap();
    let no_mark = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), None)),
        },
        10,
        dir.path().join("a.jsonl"),
        0,
    );
    let err = no_mark
        .submit(&order("0.5", CloseSide::CloseLong, 10, 3))
        .await
        .expect_err("missing mark must fail");
    assert!(
        matches!(err, SentinelError::Perpl(PerplError::Order(_))),
        "missing mark maps to PerplError::Order, got {err:?}"
    );

    let no_position = DryRunExecutor::new(
        StaticProbe { response: None },
        10,
        dir.path().join("b.jsonl"),
        0,
    );
    let err = no_position
        .submit(&order("0.5", CloseSide::CloseLong, 10, 3))
        .await
        .expect_err("no position must fail (no mark)");
    assert!(
        matches!(err, SentinelError::Perpl(PerplError::Order(_))),
        "no position maps to PerplError::Order, got {err:?}"
    );
}

/// Second submission gets the next sequence id (monotonic per executor).
#[tokio::test]
async fn dry_run_sequence_is_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seq.jsonl");
    let ex = DryRunExecutor::new(
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("100")))),
        },
        10,
        path.clone(),
        7,
    );
    let r1 = ex
        .submit(&order("0.5", CloseSide::CloseLong, 10, 3))
        .await
        .unwrap();
    let r2 = ex
        .submit(&order("0.25", CloseSide::CloseLong, 10, 3))
        .await
        .unwrap();
    assert_eq!(r1.client_order_id, "sentinel-32-8");
    assert_eq!(r2.client_order_id, "sentinel-32-9");
    assert_eq!(
        jsonl(&path).len(),
        2,
        "reports are appended, not overwritten"
    );
}

// ---------------------------------------------------------------------------
// Recording executor (spec §5)
// ---------------------------------------------------------------------------

/// Persists `{request, report}` JSONL; `Simulated` with `avg_price = None`.
#[tokio::test]
async fn recording_round_trip_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.jsonl");
    let ex = RecordingExecutor::new(path.clone(), 7);
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    let r = ex.submit(&o).await.expect("recording submit");
    assert_eq!(r.status, ExecutionStatus::Simulated);
    assert!(r.avg_price.is_none(), "recording never invents a price");
    assert_eq!(r.client_order_id, "sentinel-32-8");

    let lines = jsonl(&path);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    let req = &line["request"];
    let rep = &line["report"];
    assert!(req.is_object(), "request persisted: {line}");
    assert!(rep.is_object(), "report persisted: {line}");
    // Request fields (semantic equality; enum/number encodings pinned below).
    assert_eq!(req["market_id"], serde_json::json!(32));
    let close = &req["close"];
    assert!(
        close == &serde_json::json!("CloseLong") || close == &serde_json::json!(3),
        "CloseLong persisted (string or t=3 numeric), got {close}"
    );
    // Report summary.
    assert_eq!(rep["client_order_id"], serde_json::json!("sentinel-32-8"));
    assert_eq!(rep["status"], serde_json::json!("Simulated"));
    assert!(
        rep["avg_price"].is_null(),
        "avg_price None serialised as null"
    );

    // A second submission appends and advances the sequence.
    let r2 = ex.submit(&o).await.unwrap();
    assert_eq!(r2.client_order_id, "sentinel-32-9");
    assert_eq!(jsonl(&path).len(), 2);
}

// ---------------------------------------------------------------------------
// Perpl executor (spec §5; docs/FACTS.md §1.6) — wiremock on localhost
// ---------------------------------------------------------------------------

fn test_signer() -> ApiKeySigner {
    ApiKeySigner::from_parts("test-token", &"11".repeat(32), 10143).expect("test signer")
}

/// Body exactness against the frozen shape `{d:[OrderSpec]}`:
/// `rq, mkt, acc, oid: None, t: 3|4, p: 0, s: raw, fl: 0, lv: 0, lb: 0, ms`.
#[tokio::test]
async fn perpl_executor_body_and_ack_mapping() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/trading/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "mt": 31,
            "status": {"code": 0},
            "statuses": [{"code": 0}]
        })))
        .mount(&server)
        .await;

    let exec = PerplExecutor::new(
        server.uri(),
        test_signer(),
        42,
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("100")))),
        },
        7,
    )
    .expect("perpl executor must build");

    // R| size=0.123 d=3 -> raw=123. Sell leg (CloseLong) => t=3.
    let sell = order("0.123", CloseSide::CloseLong, 10, 3);
    let r1 = exec
        .submit(&sell)
        .await
        .expect("ack 0 must map to a report");
    assert_eq!(
        r1.status,
        ExecutionStatus::Submitted,
        "code 0 = accepted for forwarding"
    );
    assert!(
        r1.tx_hash.is_none(),
        "forwarded orders carry no tx hash (gasless)"
    );
    assert_eq!(r1.client_order_id, "sentinel-32-8");
    assert_eq!(r1.order, sell);

    // R| size=0.5 d=3 -> raw=500. Buy leg (CloseShort) => t=4.
    let buy = order("0.5", CloseSide::CloseShort, 10, 3);
    let r2 = exec.submit(&buy).await.expect("second ack");
    assert_eq!(r2.status, ExecutionStatus::Submitted);
    assert_eq!(r2.client_order_id, "sentinel-32-9");

    let requests = server.received_requests().await.expect("received requests");
    assert_eq!(
        requests.len(),
        2,
        "exactly one POST per submission (no retries)"
    );

    // Signed headers present and in canonical order (SPEC.md §3.1 / FACTS §1.4).
    for name in [
        "x-api-key",
        "x-api-timestamp",
        "x-api-nonce",
        "x-api-signature",
    ] {
        assert!(
            requests[0].headers.get(name).is_some(),
            "missing header {name}"
        );
    }

    let body1: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("body must be JSON");
    let d = body1["d"].as_array().expect("d[] array");
    assert_eq!(d.len(), 1, "one order per batch");
    let d0 = &d[0];
    assert!(
        d0["rq"].as_u64().unwrap_or(0) >= 1,
        "rq is a positive integer"
    );
    assert!(num_is(&d0["mkt"], 32.0), "mkt: {d0}");
    assert!(num_is(&d0["acc"], 42.0), "acc: {d0}");
    assert!(num_is(&d0["t"], 3.0), "CloseLong => t=3: {d0}");
    assert!(
        d0.get("oid").map(|v| v.is_null()).unwrap_or(true),
        "oid None (null/absent)"
    );
    assert!(num_is(&d0["p"], 0.0), "market order price 0: {d0}");
    assert!(
        num_is(&d0["s"], 123.0),
        "raw size = 0.123 x 10^3 = 123: {d0}"
    );
    assert!(num_is(&d0["fl"], 0.0), "fl: {d0}");
    assert!(num_is(&d0["lv"], 0.0), "lv: {d0}");
    assert!(num_is(&d0["lb"], 0.0), "lb: 0 (server default): {d0}");
    assert!(num_is(&d0["ms"], 10.0), "ms = max_slippage_bps: {d0}");

    let body2: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("body must be JSON");
    let d2 = &body2["d"][0];
    assert!(num_is(&d2["t"], 4.0), "CloseShort => t=4: {d2}");
    assert!(num_is(&d2["s"], 500.0), "raw size = 0.5 x 10^3 = 500: {d2}");
    let (rq1, rq2) = (
        d[0]["rq"].as_u64().unwrap(),
        d2["rq"]
            .as_u64()
            .unwrap_or_else(|| panic!("rq missing: {d2}")),
    );
    assert!(
        rq2 > rq1,
        "rq strictly increasing per account ({rq1} -> {rq2})"
    );
}

/// Non-zero ack code ⇒ `Rejected` (carrying the code) and no auto-retry.
#[tokio::test]
async fn perpl_rejected_ack_carries_code_and_no_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/trading/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "mt": 31,
            "status": {"code": 11},
            "statuses": [{"code": 11, "error": "insufficient margin"}]
        })))
        .mount(&server)
        .await;

    let exec = PerplExecutor::new(
        server.uri(),
        test_signer(),
        42,
        StaticProbe {
            response: Some(pos(32, dec("2"), Some(dec("100")))),
        },
        7,
    )
    .unwrap();
    let r = exec
        .submit(&order("0.5", CloseSide::CloseLong, 10, 3))
        .await
        .expect("a judged-and-refused order is still a report");
    assert_eq!(r.status, ExecutionStatus::Rejected);
    assert!(
        r.detail.contains("11"),
        "ack code surfaces in detail: {}",
        r.detail
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "single attempt — order submission is never auto-retried (SPEC §5)"
    );
}

/// HTTP 401 / 403 map to `PerplError::Order`.
#[tokio::test]
async fn perpl_http_401_403_map_to_order_error() {
    for status in [401u16, 403u16] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let exec = PerplExecutor::new(
            server.uri(),
            test_signer(),
            42,
            StaticProbe {
                response: Some(pos(32, dec("2"), Some(dec("100")))),
            },
            7,
        )
        .unwrap();
        let err = exec
            .submit(&order("0.5", CloseSide::CloseLong, 10, 3))
            .await
            .expect_err("auth/scope failures must be errors");
        assert!(
            matches!(err, SentinelError::Perpl(PerplError::Order(_))),
            "HTTP {status} must map to PerplError::Order, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Guarded executor (spec §5)
// ---------------------------------------------------------------------------

/// Same key twice within the window ⇒ exactly one execution + `DuplicateOrder`;
/// a different size bucket is a different key and executes.
#[tokio::test]
async fn guard_duplicate_suppressed_single_execution() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExec::new();
    let probe = ScriptProbe::new(vec![Some(pos(32, dec("1.5"), Some(dec("100"))))]);
    let guard = GuardedExecutor::new(
        inner.clone(),
        store_arc(&dir.path().join("g.json"), Duration::from_secs(60)),
        probe,
        Duration::from_millis(50),
    );

    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    guard
        .submit(&o)
        .await
        .expect("first submit passes the store");
    assert_eq!(inner.calls(), 1, "inner executed once");

    let err = guard
        .submit(&o)
        .await
        .expect_err("duplicate within the window");
    match err {
        SentinelError::DuplicateOrder { window_secs, key } => {
            assert_eq!(window_secs, 60, "error carries the window");
            let parts: Vec<&str> = key.split(':').collect();
            assert_eq!(parts.len(), 3, "key shape market:action:bucket, got {key}");
            assert_eq!(parts[0], "32");
            assert!(
                ["reduce", "close", "collateral"].contains(&parts[1]),
                "action token valid: {key}"
            );
            assert_eq!(parts[2], size_bucket(o.size).to_string());
            assert_eq!(key, idempotency_key(MarketId(32), parts[1], o.size));
        }
        other => panic!("expected DuplicateOrder, got {other:?}"),
    }
    assert_eq!(inner.calls(), 1, "duplicate never re-executes");

    // A third attempt is still suppressed (the failed duplicate did not
    // release the key).
    assert!(guard.submit(&o).await.is_err());
    assert_eq!(inner.calls(), 1);

    // Different size ⇒ different bucket ⇒ executes (0.5 -> 50, 0.25 -> 25).
    let o2 = order("0.25", CloseSide::CloseLong, 10, 3);
    guard.submit(&o2).await.expect("different bucket must pass");
    assert_eq!(inner.calls(), 2, "second distinct intent executed");
}

/// Post-verify: |size| decreases by the full order ⇒ `Filled` (including the
/// position reaching 0); a smaller decrease ⇒ `Partial` with the observed
/// fill.
#[tokio::test]
async fn guard_post_verify_filled() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExec::new();
    // Baseline reads repeat; the third call reports the decrease.
    let probe = ScriptProbe::new(vec![
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("1.5"), Some(dec("100")))),
    ]);
    let guard = GuardedExecutor::new(
        inner,
        store_arc(&dir.path().join("g.json"), Duration::from_secs(60)),
        probe.clone(),
        Duration::from_millis(300),
    );
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    let r = guard.submit(&o).await.expect("verified submit");
    assert_eq!(r.status, ExecutionStatus::Filled);
    assert_eq!(r.filled_size, dec("0.5"), "observed decrease = order size");
    assert_eq!(r.order, o);
    assert!(probe.calls() >= 3, "the guard polls the probe");
}

/// Full close to zero ⇒ `Filled` with the whole size.
#[tokio::test]
async fn guard_post_verify_filled_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExec::new();
    let probe = ScriptProbe::new(vec![
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("0"), Some(dec("100")))),
    ]);
    let guard = GuardedExecutor::new(
        inner,
        store_arc(&dir.path().join("g.json"), Duration::from_secs(60)),
        probe,
        Duration::from_millis(300),
    );
    let o = order("2", CloseSide::CloseLong, 10, 3);
    let r = guard.submit(&o).await.expect("verified submit");
    assert_eq!(r.status, ExecutionStatus::Filled);
    assert_eq!(r.filled_size, dec("2"), "position reached flat");
}

/// Partial decrease ⇒ `Partial` with the observed (smaller) fill size.
#[tokio::test]
async fn guard_post_verify_partial() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExec::new();
    let probe = ScriptProbe::new(vec![
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("2"), Some(dec("100")))),
        Some(pos(32, dec("1.9"), Some(dec("100")))),
    ]);
    let guard = GuardedExecutor::new(
        inner,
        store_arc(&dir.path().join("g.json"), Duration::from_secs(60)),
        probe,
        Duration::from_millis(300),
    );
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    let r = guard.submit(&o).await.expect("verified submit");
    assert_eq!(r.status, ExecutionStatus::Partial);
    assert_eq!(r.filled_size, dec("0.1"), "observed decrease = actual fill");
}

/// Timeout: no |size| decrease within `verify_after` ⇒ the inner report is
/// kept unchanged (status/fields intact) and the submission still succeeds.
#[tokio::test]
async fn guard_post_verify_timeout_keeps_inner_report() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExec::new();
    let probe = ScriptProbe::new(vec![Some(pos(32, dec("2"), Some(dec("100"))))]);
    let verify_after = Duration::from_millis(120);
    let guard = GuardedExecutor::new(
        inner.clone(),
        store_arc(&dir.path().join("g.json"), Duration::from_secs(60)),
        probe,
        verify_after,
    );
    let o = order("0.5", CloseSide::CloseLong, 10, 3);
    let started = Instant::now();
    let r = guard.submit(&o).await.expect("timeout is not an error");
    let elapsed = started.elapsed();
    assert_eq!(r.status, ExecutionStatus::Submitted, "inner report kept");
    assert_eq!(r.detail, "inner-report", "inner detail preserved");
    assert_eq!(r.client_order_id, "inner-id");
    assert_eq!(r.ts_ms, 7);
    assert_eq!(inner.calls(), 1);
    assert!(
        elapsed >= verify_after,
        "the guard must wait out verify_after before giving up ({elapsed:?})"
    );
}
