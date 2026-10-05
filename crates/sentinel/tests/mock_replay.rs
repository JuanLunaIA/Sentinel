//! Fixture-replay equality (P03 validation): `MockPerpl` over a recorded
//! session must emit exactly the `MarkPrice` sequence that a raw, independent
//! re-derivation of the fixture implies — same order, same values.

use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use sentinel::perpl::{FeedEvent, MarketEvent, MockPerpl, PerplFeed};
use serde_json::Value;

/// Newest recorded session fixture under `tests/fixtures/perpl/`.
fn newest_fixture() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/perpl");
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir readable")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl"))
        })
        .collect();
    sessions.sort();
    sessions
        .pop()
        .expect("at least one session fixture recorded (scripts/record-fixtures.sh)")
}

/// Price decimals per market, derived directly from the fixture's context line.
fn decimals_from_fixture(raw: &str) -> std::collections::HashMap<u32, u32> {
    let mut map = std::collections::HashMap::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("path").and_then(Value::as_str) != Some("/v1/pub/context") {
            continue;
        }
        let Some(markets) = v["resp"]["markets"].as_array() else {
            continue;
        };
        for market in markets {
            let (Some(id), Some(decimals)) = (
                market["id"].as_u64(),
                market["config"]["price_decimals"].as_u64(),
            ) else {
                continue;
            };
            map.insert(id as u32, decimals as u32);
        }
    }
    map
}

/// Expected `(market_id, mark)` sequence, derived independently from raw lines.
fn expected_marks(
    raw: &str,
    decimals: &std::collections::HashMap<u32, u32>,
) -> Vec<(u32, Decimal)> {
    let mut expected = Vec::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("ws") {
            continue;
        }
        let msg = &v["msg"];
        if msg.get("mt").and_then(Value::as_u64) != Some(9) {
            continue;
        }
        let Some(entries) = msg["d"].as_object() else {
            continue;
        };
        for (key, state) in entries {
            let Ok(id) = key.parse::<u32>() else { continue };
            let (Some(pd), Some(raw_price)) = (decimals.get(&id), state["mrk"].as_i64()) else {
                continue;
            };
            expected.push((
                id,
                Decimal::try_new(raw_price, *pd).expect("scalable price"),
            ));
        }
    }
    expected
}

#[tokio::test]
async fn mock_replay_matches_raw_fixture_derivation() {
    let path = newest_fixture();
    let raw = std::fs::read_to_string(&path).expect("fixture readable");
    let decimals = decimals_from_fixture(&raw);
    assert!(!decimals.is_empty(), "fixture carries a context recording");

    let expected = expected_marks(&raw, &decimals);
    assert!(
        !expected.is_empty(),
        "fixture must contain at least one mt:9 market-state frame"
    );

    let feed = MockPerpl::from_fixture(&path).expect("fixture loads");
    let mut rx = feed.stream().await;

    let mut received: Vec<(u32, Decimal)> = Vec::new();
    let collect = async {
        while let Some(event) = rx.recv().await {
            match event {
                FeedEvent::Market(MarketEvent::MarkPrice {
                    market_id, price, ..
                }) => {
                    received.push((market_id.0, price));
                }
                FeedEvent::Account(_)
                | FeedEvent::FeedStale { .. }
                | FeedEvent::Reconnected { .. } => {
                    panic!(
                        "recorded market-data fixture must not emit account/stale/reconnect events"
                    );
                }
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), collect)
        .await
        .expect("replay finishes within the timeout");

    assert_eq!(
        received, expected,
        "replayed marks must match the raw derivation exactly"
    );
}

#[tokio::test]
async fn mock_context_parses_and_snapshot_is_documented_pending() {
    let path = newest_fixture();
    let feed = MockPerpl::from_fixture(&path).expect("fixture loads");

    let markets = feed
        .context()
        .await
        .expect("context parses from the fixture");
    assert!(!markets.is_empty(), "context has markets");

    // Public-only recordings (STUB-09) have no signed REST lines yet: the
    // snapshot must fail loudly with a message naming the missing recording.
    let err = feed
        .snapshot()
        .await
        .expect_err("snapshot requires signed REST lines");
    let text = format!("{err}");
    assert!(
        text.contains("wallet") || text.contains("positions"),
        "error names the missing recording: {text}"
    );
}
