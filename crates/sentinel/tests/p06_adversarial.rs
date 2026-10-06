//! P06 adversarial verification — independent, black-box corpus (SPEC-P06 §9).
//!
//! Runs against the verifier's own fixture
//! (`tests/fixtures/perpl/p06-verifier-scenario.jsonl`): an ETH-like book
//! (entry 2000.00, size +5.000, collateral 4000, mmr 0.05 ⇒ derived liq
//! 1300.00) crossing Green → Yellow → Orange → Red, plus a well-collateralized
//! BTC contrast that stays Green. Every asserted number was re-derived
//! independently with Python `decimal`:
//!
//! - ETH distances: mark 1900.00 ⇒ 31.5789 % Green, 1700.00 ⇒ 23.5294 %
//!   Yellow, 1500.00 ⇒ 13.3333 % Orange, 1380.00 ⇒ 5.7971 % Red,
//!   1400.00 ⇒ 7.1429 % Red.
//! - Reduce sizes: Orange 5.000 × 0.25 = 1.250; Red 5.000 × 0.5 = 2.500.
//!
//! Suites: (1) two-run determinism (byte-equal `to_jsonl`) + expected event
//! sequence; (2) `DedupeSink` boundary matrix; (3) health surface over HTTP;
//! (4) bounded shutdown drain.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use rust_decimal::Decimal;
use sentinel::config::Config;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::health::{HealthState, router};
use sentinel::notify::{Alert, AlertKind, AlertSink, DedupeSink, RED_REALERT_MS, RecordingSink};
use sentinel::perpl::MockPerpl;
use sentinel::pipeline::{
    LiveState, Pipeline, PipelineEvent, PipelineOutcome, RunMode, StateProbe,
};
use sentinel_core::types::{ExecutionMode, MarketId, RiskTier};
use tokio::sync::{Mutex, watch};

/// Logical session start baked into both verifier fixtures (ms).
const BASE_MS: u64 = 1_792_000_000_000;
/// ETH market id in the fixtures.
const ETH_MARKET: u32 = 32;
/// BTC market id in the fixtures.
const BTC_MARKET: u32 = 16;
/// When the 1700.00 mark (Green → Yellow) is applied: `t_ms = 26 000`.
const YELLOW_AT_MS: u64 = BASE_MS + 26_000;
/// Logical clock after the last fixture frame (`t_ms = 101 000`).
const LAST_EVENT_MS: u64 = BASE_MS + 101_000;
/// When the 1500.00 mark (Yellow → Orange) is applied: `t_ms = 51 000`.
const ORANGE_AT_MS: u64 = BASE_MS + 51_000;
/// When the 1380.00 mark (Orange → Red) is applied: `t_ms = 76 000`.
const RED_AT_MS: u64 = BASE_MS + 76_000;
/// First late frame in the shutdown fixture (`t_ms = 1 000 000 000`); a
/// correct flip must cut the stream before this frame is emitted.
const SHUTDOWN_CUT_MS: u64 = BASE_MS + 1_000_000_000;

/// Pin the MockPerpl replay pacing for the whole test binary (SPEC-P06 §6):
/// a divisor of 100 000 makes the 25 s scenario gaps scale to zero sleeps,
/// while the raised per-line cap keeps the shutdown fixture's late frames
/// (~2 s wall each, > 3 s total) slow enough that a flip which is *not*
/// honored could never satisfy the 3 s join in the shutdown test.
fn ensure_fast_replay() {
    static FAST_REPLAY: Once = Once::new();
    FAST_REPLAY.call_once(|| {
        // SAFETY: `std::env::set_var` is unsafe in Rust 2024. All tests call
        // this before building any feed; `Once` blocks concurrent callers
        // until the single write completes, so no MockPerpl reader races it.
        unsafe {
            std::env::set_var("SENTINEL_MOCK_PACE", "100000");
            std::env::set_var("SENTINEL_MOCK_CAP_MS", "2000");
        }
    });
}

const VALID_SECRET: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Config with the verifier's risk/replay switches (SPEC-P06 §9 corpus).
fn verifier_config() -> Config {
    let base: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", VALID_SECRET),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("MARKET_ALLOWLIST", "32,16"),
        ("RISK_SOFT_PCT", "25"),
        ("RISK_WARN_PCT", "15"),
        ("RISK_HARD_PCT", "8"),
        ("REFLEX_REDUCE_FRACTION", "0.5"),
        ("REFLEX_ORANGE_FRACTION", "0.25"),
        ("REFLEX_COOLDOWN_SECS", "10"),
        ("MAX_ORDER_SIZE_USD", "100000"),
        ("REQUIRE_APPROVAL_ABOVE_USD", "100000"),
    ];
    let vars: HashMap<String, String> = base
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    Config::from_vars(vars).expect("verifier config must load")
}

/// Absolute path of a fixture under `tests/fixtures/perpl/`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/perpl")
        .join(name)
}

/// Per-run scratch path for the dry-run executor's report (kept out of the
/// fixture tree; the second determinism run gets its own file).
fn report_path(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/p06-verifier");
    std::fs::create_dir_all(&dir).expect("scratch dir for dry-run reports");
    dir.join(format!("dryrun-{tag}.jsonl"))
}

/// Result of one full replay run.
struct RunResult {
    outcome: PipelineOutcome,
    alerts: Vec<Alert>,
}

/// Replay one fixture with a `RecordingSink` + `DryRunExecutor` over a
/// `StateProbe` (SPEC-P06 §5 replay row); optionally flip the shutdown watch
/// after `flip_after`.
async fn run_pipeline(path: &Path, tag: &str, flip_after: Option<Duration>) -> RunResult {
    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let sink = RecordingSink::new();
    let alerts = Arc::clone(&sink.alerts);
    let executor =
        DryRunExecutor::new(StateProbe::new(Arc::clone(&state)), 10, report_path(tag), 0);
    let feed = MockPerpl::from_fixture(path).expect("fixture loads");
    let pipeline = Pipeline::new(
        verifier_config(),
        feed,
        executor,
        sink,
        state,
        health,
        RunMode::Replay,
    );

    let (tx, rx) = watch::channel(false);
    let keepalive = tx.clone();
    let flipper = flip_after.map(move |delay| {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(true);
        })
    });

    let outcome = pipeline.run(rx).await.expect("pipeline returns Ok");
    drop(keepalive);
    if let Some(handle) = flipper {
        handle.abort();
    }

    let alerts = alerts.lock().expect("alerts lock").clone();
    RunResult { outcome, alerts }
}

/// Event timestamp (`at_ms`) for every variant — used for the monotonicity
/// check below.
fn event_at(event: &PipelineEvent) -> u64 {
    match event {
        PipelineEvent::Started { at_ms, .. }
        | PipelineEvent::TierChanged { at_ms, .. }
        | PipelineEvent::ConsultScheduled { at_ms, .. }
        | PipelineEvent::Decision { at_ms, .. }
        | PipelineEvent::Executed { at_ms, .. }
        | PipelineEvent::SubmitFailed { at_ms, .. }
        | PipelineEvent::DuplicateSuppressed { at_ms, .. }
        | PipelineEvent::Alert { at_ms, .. }
        | PipelineEvent::FeedStale { at_ms, .. }
        | PipelineEvent::Reconnected { at_ms, .. }
        | PipelineEvent::Shutdown { at_ms } => *at_ms,
    }
}

/// Normalize a kind discriminator for cross-source comparison.
fn norm_kind(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

#[tokio::test]
async fn determinism_two_runs_byte_equal_and_expected_sequence() {
    ensure_fast_replay();
    let path = fixture("p06-verifier-scenario.jsonl");

    let a = tokio::time::timeout(
        Duration::from_secs(10),
        run_pipeline(&path, "det-a", Some(Duration::from_millis(500))),
    )
    .await
    .expect("run A completes within 10 s");
    let b = tokio::time::timeout(
        Duration::from_secs(10),
        run_pipeline(&path, "det-b", Some(Duration::from_millis(500))),
    )
    .await
    .expect("run B completes within 10 s");

    assert_eq!(
        a.outcome.to_jsonl(),
        b.outcome.to_jsonl(),
        "same fixture + config must produce byte-identical JSONL"
    );
    assert_eq!(
        a.alerts, b.alerts,
        "sink payloads (alert texts included) must be identical across runs"
    );

    let events = &a.outcome.events;

    // --- Started: replay flag, markets, positions, logical start clock ---
    match events.first().expect("events must not be empty") {
        PipelineEvent::Started {
            mode,
            replay,
            markets,
            positions,
            at_ms,
        } => {
            assert!(replay, "replay run must set replay=true");
            assert!(
                mode.to_lowercase().contains("dry"),
                "mode should name the dry-run execution mode, got {mode:?}"
            );
            assert!(
                markets.contains(&ETH_MARKET) && markets.contains(&BTC_MARKET),
                "both fixture markets present, got {markets:?}"
            );
            assert_eq!(
                *positions, 2,
                "both fixture positions in the initial snapshot"
            );
            assert_eq!(
                *at_ms, BASE_MS,
                "replay clock starts at the initial snapshot_ts"
            );
        }
        other => panic!("first event must be Started, got {other:?}"),
    }

    // --- Shutdown: exactly one, final, on the logical clock ---
    let shutdowns: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::Shutdown { at_ms } => Some(*at_ms),
            _ => None,
        })
        .collect();
    assert_eq!(shutdowns.len(), 1, "exactly one Shutdown event");
    assert!(
        matches!(events.last(), Some(PipelineEvent::Shutdown { .. })),
        "Shutdown must be the final event"
    );
    assert_eq!(
        shutdowns[0], LAST_EVENT_MS,
        "replay shutdown uses the latest applied event timestamp"
    );

    // --- no wall-clock leak: at_ms must be non-decreasing ---
    for pair in events.windows(2) {
        assert!(
            event_at(&pair[0]) <= event_at(&pair[1]),
            "at_ms must be non-decreasing: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }

    // --- ETH tier crossings: Green -> Yellow -> Orange -> Red, in order ---
    let eth_tiers: Vec<(Option<RiskTier>, RiskTier, u64)> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::TierChanged {
                market_id,
                from,
                to,
                at_ms,
                ..
            } if *market_id == ETH_MARKET => Some((*from, *to, *at_ms)),
            _ => None,
        })
        .collect();
    assert!(!eth_tiers.is_empty(), "ETH must emit tier transitions");
    let changes: Vec<RiskTier> = eth_tiers
        .iter()
        .filter(|(from, _, _)| from.is_some())
        .map(|(_, to, _)| *to)
        .collect();
    assert_eq!(
        changes,
        vec![RiskTier::Yellow, RiskTier::Orange, RiskTier::Red],
        "ETH crossings (ignoring first sight), got {eth_tiers:?}"
    );
    let yellow_at = eth_tiers
        .iter()
        .find(|(from, to, _)| *from == Some(RiskTier::Green) && *to == RiskTier::Yellow)
        .map(|(_, _, at)| *at)
        .expect("Green→Yellow crossing");
    assert_eq!(
        yellow_at, YELLOW_AT_MS,
        "Yellow entered when the 1700.00 mark applied"
    );
    let orange_at = eth_tiers
        .iter()
        .find(|(from, to, _)| *from == Some(RiskTier::Yellow) && *to == RiskTier::Orange)
        .map(|(_, _, at)| *at)
        .expect("Yellow→Orange crossing");
    assert_eq!(
        orange_at, ORANGE_AT_MS,
        "Orange entered when the 1500.00 mark applied"
    );
    let red_at = eth_tiers
        .iter()
        .find(|(from, to, _)| *from == Some(RiskTier::Orange) && *to == RiskTier::Red)
        .map(|(_, _, at)| *at)
        .expect("Orange→Red crossing");
    assert_eq!(
        red_at, RED_AT_MS,
        "Red entered when the 1380.00 mark applied"
    );

    // --- ConsultScheduled exactly once, on the Yellow entry ---
    let consults: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::ConsultScheduled {
                market_id,
                tier,
                at_ms,
            } if *market_id == ETH_MARKET => {
                assert_eq!(*tier, RiskTier::Yellow, "consult is scheduled on Yellow");
                Some(*at_ms)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        consults,
        vec![YELLOW_AT_MS],
        "exactly one consult, at the Yellow entry time"
    );

    // --- Executed sizes: orange 1.250, red 2.500 (Python-derived) ---
    let executed: Vec<Decimal> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::Executed { filled_size, .. } => Some(*filled_size),
            _ => None,
        })
        .collect();
    assert!(
        executed.contains(&Decimal::new(1250, 3)),
        "orange reduce 1.250 must execute; got {executed:?}"
    );
    assert!(
        executed.contains(&Decimal::new(2500, 3)),
        "red reduce 2.500 must execute; got {executed:?}"
    );
    // Sizes depend on same-frame cross-market evaluation order (an ETH-first
    // frame yields a Red 2.500 at the crossing and a 5.000 Close later), so
    // only the corpus-mandated existence is asserted above; this guards
    // against unsized/alien submissions.
    for size in &executed {
        assert!(
            *size == Decimal::new(1250, 3)
                || *size == Decimal::new(2500, 3)
                || *size == Decimal::new(5000, 3),
            "only sized reduces/closes may be submitted; got {size}"
        );
    }

    // --- sink alerts mirror the Alert events 1:1 (order, text, market, ts) ---
    let alert_events: Vec<(&str, Option<u32>, &str, u64)> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::Alert {
                kind,
                market_id,
                text,
                at_ms,
            } => Some((kind.as_str(), *market_id, text.as_str(), *at_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(
        alert_events.len(),
        a.alerts.len(),
        "every sink alert must have a mirrored Alert event (and vice versa)"
    );
    assert!(a.alerts.len() >= 3, "expected several alerts");
    for (event, alert) in alert_events.iter().zip(a.alerts.iter()) {
        assert_eq!(event.2, alert.text, "alert text mirrored in order");
        assert_eq!(
            event.1,
            alert.market_id.map(|market| market.0),
            "alert market mirrored"
        );
        assert_eq!(event.3, alert.at_ms, "alert timestamp mirrored");
        let kind = serde_json::to_value(&alert.kind).expect("alert kind serializes");
        let tag = kind
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        assert_eq!(
            norm_kind(event.0),
            norm_kind(tag),
            "alert kind discriminator mirrored"
        );
    }

    // --- alert text contract (SPEC-P06 §3) ---
    let yellow_alert = a
        .alerts
        .iter()
        .find(|alert| {
            matches!(
                &alert.kind,
                AlertKind::TierChange {
                    to: RiskTier::Yellow,
                    ..
                }
            )
        })
        .expect("Yellow tier alert delivered");
    let upper = yellow_alert.text.to_uppercase();
    assert!(
        yellow_alert.text.contains("ETH"),
        "symbol in tier-change text: {}",
        yellow_alert.text
    );
    assert!(
        yellow_alert.text.contains("32"),
        "market id in tier-change text: {}",
        yellow_alert.text
    );
    assert!(
        upper.contains("GREEN") && upper.contains("YELLOW"),
        "from→to in tier-change text: {}",
        yellow_alert.text
    );
    assert!(
        yellow_alert.text.contains('%'),
        "rendered distance in tier-change text: {}",
        yellow_alert.text
    );
    let announces_consult = a
        .alerts
        .iter()
        .any(|alert| alert.text.to_lowercase().contains("consult"));
    assert!(
        announces_consult,
        "the consult is announced (SPEC-P06 §3 example text)"
    );

    for alert in &a.alerts {
        if let AlertKind::ReflexAction {
            size,
            client_order_id,
            status,
            ..
        } = &alert.kind
        {
            assert!(
                alert.text.contains(size.as_str()),
                "size in reflex text: {} vs {size}",
                alert.text
            );
            assert!(
                alert.text.contains(client_order_id.as_str()),
                "client order id in reflex text: {} vs {client_order_id}",
                alert.text
            );
            assert!(
                alert.text.to_lowercase().contains(&status.to_lowercase()),
                "status in reflex text: {} vs {status}",
                alert.text
            );
        }
    }

    // --- every Executed has a matching ReflexAction alert, in order ---
    let executed_ids: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::Executed {
                client_order_id, ..
            } => Some(client_order_id.as_str()),
            _ => None,
        })
        .collect();
    let reflex_ids: Vec<String> = a
        .alerts
        .iter()
        .filter_map(|alert| match &alert.kind {
            AlertKind::ReflexAction {
                client_order_id, ..
            } => Some(client_order_id.clone()),
            _ => None,
        })
        .collect();
    let reflex_ids: Vec<&str> = reflex_ids.iter().map(String::as_str).collect();
    assert_eq!(
        executed_ids, reflex_ids,
        "each Executed mirrors exactly one ReflexAction alert"
    );

    // --- BTC contrast stays Green and never acts ---
    let btc_tiers: Vec<RiskTier> = events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::TierChanged { market_id, to, .. } if *market_id == BTC_MARKET => {
                Some(*to)
            }
            _ => None,
        })
        .collect();
    assert!(
        btc_tiers.iter().all(|tier| *tier == RiskTier::Green),
        "well-collateralized BTC contrast stays Green, got {btc_tiers:?}"
    );
    let mut btc_decisions = 0usize;
    for event in events {
        if let PipelineEvent::Decision {
            market_id, tier, ..
        } = event
            && *market_id == BTC_MARKET
        {
            btc_decisions += 1;
            assert_eq!(*tier, RiskTier::Green, "BTC contrast decisions stay Green");
        }
    }
    assert!(btc_decisions > 0, "BTC is evaluated on every pass");
    let btc_actions = a
        .alerts
        .iter()
        .filter(|alert| alert.market_id == Some(MarketId(BTC_MARKET)))
        .filter(|alert| matches!(&alert.kind, AlertKind::ReflexAction { .. }))
        .count();
    assert_eq!(btc_actions, 0, "BTC contrast never acts");
}

/// Builders for the dedupe matrix below.
fn tier_name(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Green => "Green",
        RiskTier::Yellow => "Yellow",
        RiskTier::Orange => "Orange",
        RiskTier::Red => "Red",
    }
}

fn tier_change(market: u32, from: Option<RiskTier>, to: RiskTier, at_ms: u64) -> Alert {
    Alert {
        kind: AlertKind::TierChange {
            from,
            to,
            distance_pct: "10.00%".to_string(),
        },
        market_id: Some(MarketId(market)),
        text: format!(
            "tier-change {market} {}→{}",
            from.map_or("first", tier_name),
            tier_name(to)
        ),
        at_ms,
    }
}

fn reflex_action(market: u32, at_ms: u64, tag: &str) -> Alert {
    Alert {
        kind: AlertKind::ReflexAction {
            decision_id: "d-1".to_string(),
            status: "simulated".to_string(),
            size: "1.250".to_string(),
            client_order_id: format!("co-{tag}"),
        },
        market_id: Some(MarketId(market)),
        text: format!("reflex {tag}"),
        at_ms,
    }
}

fn consult_scheduled(at_ms: u64) -> Alert {
    Alert {
        kind: AlertKind::ConsultScheduled {
            tier: RiskTier::Yellow,
        },
        market_id: Some(MarketId(ETH_MARKET)),
        text: "consult scheduled".to_string(),
        at_ms,
    }
}

fn feed_stale(at_ms: u64) -> Alert {
    Alert {
        kind: AlertKind::FeedStale { secs: 70 },
        market_id: None,
        text: "feed stale".to_string(),
        at_ms,
    }
}

#[tokio::test]
async fn notify_dedupe_boundary_matrix_black_box() {
    let inner = RecordingSink::new();
    let captured = Arc::clone(&inner.alerts);
    let sink = DedupeSink::new(inner);

    let t0: u64 = 1_700_000_000_000;
    let red_base = t0 + 4_000;

    // (alert, expected to pass) — the full §9 dedupe matrix.
    let script: Vec<(Alert, bool)> = vec![
        // first alert for (32, Green) passes
        (tier_change(32, None, RiskTier::Green, t0), true),
        // same (market, tier) suppressed, regardless of `from`
        (
            tier_change(32, Some(RiskTier::Green), RiskTier::Green, t0 + 1_000),
            false,
        ),
        // a real change passes and updates the store
        (
            tier_change(32, Some(RiskTier::Green), RiskTier::Yellow, t0 + 2_000),
            true,
        ),
        (
            tier_change(32, Some(RiskTier::Green), RiskTier::Yellow, t0 + 3_000),
            false,
        ),
        // non-Red never re-alerts, however old the stored timestamp is
        (
            tier_change(
                32,
                Some(RiskTier::Green),
                RiskTier::Yellow,
                t0 + 3_000 + RED_REALERT_MS + 60_000,
            ),
            false,
        ),
        // a different market starts its own state; Red first breach passes
        (tier_change(16, None, RiskTier::Red, red_base), true),
        // not before `stored + RED_REALERT_MS` ...
        (
            tier_change(16, None, RiskTier::Red, red_base + RED_REALERT_MS - 1),
            false,
        ),
        // ... exactly at it passes
        (
            tier_change(16, None, RiskTier::Red, red_base + RED_REALERT_MS),
            true,
        ),
        // a passing non-tier alert does not touch the Red cadence
        (
            reflex_action(16, red_base + RED_REALERT_MS + 500, "probe"),
            true,
        ),
        (
            tier_change(16, None, RiskTier::Red, red_base + 2 * RED_REALERT_MS - 1),
            false,
        ),
        (
            tier_change(16, None, RiskTier::Red, red_base + 2 * RED_REALERT_MS),
            true,
        ),
        // leaving Red passes; Orange (non-Red) never re-alerts
        (
            tier_change(
                16,
                Some(RiskTier::Red),
                RiskTier::Orange,
                t0 + 5_000 + 2 * RED_REALERT_MS,
            ),
            true,
        ),
        (
            tier_change(
                16,
                Some(RiskTier::Red),
                RiskTier::Orange,
                t0 + 5_000 + 2 * RED_REALERT_MS + RED_REALERT_MS + 90_000,
            ),
            false,
        ),
        // non-tier kinds always pass, repeatedly, and do not touch the store
        (reflex_action(32, t0 + 6_000, "a"), true),
        (reflex_action(32, t0 + 6_001, "b"), true),
        (consult_scheduled(t0 + 6_002), true),
        (feed_stale(t0 + 6_003), true),
        // ... the stored (32, Yellow) is still enforced afterwards
        (
            tier_change(32, Some(RiskTier::Green), RiskTier::Yellow, t0 + 7_000),
            false,
        ),
    ];

    let mut expected: Vec<Alert> = Vec::new();
    for (alert, should_pass) in &script {
        sink.send(alert).await.expect("dedupe sink never errors");
        if *should_pass {
            expected.push(alert.clone());
        }
    }

    let recorded = captured.lock().expect("captured lock").clone();
    assert_eq!(
        recorded, expected,
        "only boundary-permitted alerts pass, in order"
    );
    assert_eq!(
        expected.len(),
        11,
        "matrix exercises both pass and suppress paths"
    );
}

#[tokio::test]
async fn health_endpoint_shape_and_feed_age() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let state = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let app = router(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/healthz");

    let resp = client.get(&url).send().await.expect("GET /healthz");
    assert_eq!(resp.status(), 200, "health endpoint answers 200");
    let body: serde_json::Value = resp.json().await.expect("health JSON");

    let mut keys: Vec<&str> = body
        .as_object()
        .expect("health body is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["feed_age_s", "mode", "uptime_s", "version"],
        "exact health shape"
    );
    assert!(
        body["uptime_s"].as_u64().expect("uptime_s is u64") < 3600,
        "uptime_s is a small number"
    );
    let first_age = &body["feed_age_s"];
    assert!(
        first_age.is_null() || first_age.is_u64(),
        "feed_age_s is null or u64 before any touch, got {first_age}"
    );
    assert_eq!(body["mode"], "DRY_RUN");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    // Touch the feed at wall-now; the next render must show a young age.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as u64;
    state.touch_feed(now_ms);

    let resp = client
        .get(&url)
        .send()
        .await
        .expect("GET /healthz after touch");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("health JSON after touch");
    let age = body["feed_age_s"]
        .as_u64()
        .expect("feed_age_s becomes a number after touch");
    assert!(age <= 10, "feed_age_s small after touch, got {age}");

    server.abort();
}

#[tokio::test]
async fn shutdown_mid_stream_returns_ok_with_single_shutdown() {
    ensure_fast_replay();
    let path = fixture("p06-verifier-shutdown.jsonl");

    // The shutdown fixture's late frames are paced ~2 s each (PACE=100000,
    // CAP=2000 in `ensure_fast_replay`), so the flip below lands mid-stream
    // and a flip that is never honored would exceed the 3 s join timeout.
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        run_pipeline(&path, "shutdown", Some(Duration::from_millis(30))),
    )
    .await
    .expect("pipeline joins within 3 s of the flip");

    let events = &result.outcome.events;
    assert!(
        matches!(events.first(), Some(PipelineEvent::Started { .. })),
        "Started is emitted before the drain"
    );
    let shutdowns = events
        .iter()
        .filter(|event| matches!(event, PipelineEvent::Shutdown { .. }))
        .count();
    assert_eq!(shutdowns, 1, "exactly one Shutdown after the flip");
    assert!(
        matches!(events.last(), Some(PipelineEvent::Shutdown { .. })),
        "nothing is emitted after Shutdown"
    );
    assert!(
        events.iter().all(|event| event_at(event) < SHUTDOWN_CUT_MS),
        "the flip must cut the stream before the late (2 s-paced) frames"
    );
}
