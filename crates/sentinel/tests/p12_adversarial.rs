//! P12 adversarial verification — independent black-box corpus (P12 prompt:
//! "ENVIO HYPERINDEX (PERPL EVENTS + OWN ANCHOR CONTRACT)" + the frozen
//! `sentinel::indexer` interface).
//!
//! Written against the frozen public API and the P12 prompt pack only; the
//! implementation (`sentinel::indexer`) is treated as a black box. Coverage:
//!
//! - **Wire protocol (wiremock, localhost only)**: each method's query text
//!   must name the exact entity (`SentinelAnchor` / `SentinelHeartbeat` /
//!   `Liquidation`) and select the frozen field names; the anchors call sends
//!   the seq lower bound (probed as `from`, accepted alias `from_seq`, no
//!   `limit` — the signature carries none) filtering `seq _gte` ascending;
//!   heartbeats send `limit`; liquidations send `market` + `limit` (and omit
//!   `market` when unfiltered); no call may mention another entity.
//! - **BigInt tolerance**: `seq`/`ts` (Envio `BigInt`) accept both the string
//!   form (`"41"`) and the JSON-number form (`41`) with identical results.
//! - **Null optionals**: `tx_hash: null` (and an absent `tx_hash` key) parse
//!   to `None`; `collateral_lost: null` likewise.
//! - **GraphQL errors**: a non-empty `errors` array must yield `Err` whose
//!   message mentions `graphql` — with empty data and with partial data.
//! - **Degenerate payloads**: `{"data":{}}` is locked to the behaviour probed
//!   against the implementation (documented at the assertion); a `u64::MAX`
//!   string parses, `u64::MAX + 1` returns `Err` (never a panic).
//! - **Degenerate endpoints**: `new("")` / `new("   ")` must not panic at
//!   construction and must fail queries gracefully (probed behaviour locked).
//! - **`from_env`**: unset `ENVIO_GRAPHQL_ENDPOINT` ⇒ `None`; set ⇒ a working
//!   client against the mock (unsafe env ops serialized via a mutex).
//! - **Artifact cross-check (no execution)**: `indexer/schema.graphql` must
//!   declare exactly the five frozen entities with the frozen field names
//!   (snake_case, no camelCase renames); `indexer/config.yaml` must reference
//!   chain `10143` (or document anvil `31337`); the Perpl exchange address
//!   `0x1964c32f0be608e7d29302aff5e61268e72080cc` must appear in `indexer/`
//!   **unless** the Perpl ABI fallback is applied, in which case
//!   `indexer/README.md` must carry the roadmap note instead;
//!   `docs/queries.graphql` must document the four example queries; the
//!   local-run evidence (`docs/evidence/p12-envio-anvil.txt`) must show a
//!   `SentinelAnchor` result block or a documented blocker, and every cited
//!   `docs/evidence/p12-*.{txt,md}` reference must exist.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use sentinel::indexer::{IndexerClient, LiquidationRow, SentinelAnchor, SentinelHeartbeat};

/// The Perpl exchange on Monad testnet (from the prompt pack / FACTS).
const EXCHANGE_ADDRESS: &str = "0x1964c32f0be608e7d29302aff5e61268e72080cc";

/// Serializes all `std::env` mutation in this test binary (Rust 2024 marks
/// `set_var`/`remove_var` unsafe because the environment is process-global).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Repository root (`crates/sentinel` -> `crates` -> repo).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/sentinel has a repo root")
        .to_path_buf()
}

// ===========================================================================
// GraphQL mock helpers
// ===========================================================================

/// The JSON key a conforming GraphQL response must use for `entity`'s rows:
/// the field alias when the query aliases the selection, else the field name.
/// (GraphQL spec: the response key is the alias, falling back to the field.)
fn selection_key(query: &str, entity: &str) -> String {
    let Some(idx) = query.find(entity) else {
        return entity.to_string();
    };
    let before = query[..idx].trim_end();
    if let Some(boundary) = before.rfind(['{', '}', ',']) {
        let segment = before[boundary + 1..].trim();
        if let Some(colon) = segment.rfind(':') {
            let name = segment[..colon].trim();
            let is_ident =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if is_ident && segment[colon + 1..].trim().is_empty() {
                return name.to_string();
            }
        }
    }
    entity.to_string()
}

/// Mount a GraphQL mock for `entity` answering every POST with `rows` under
/// the alias-aware selection key (plus the bare entity name as a safety net).
async fn mount_graphql_rows(server: &MockServer, entity: &'static str, rows: Value) {
    let entity_name = entity.to_string();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().expect("GraphQL request body is JSON");
            let query = body["query"].as_str().unwrap_or_default();
            let key = selection_key(query, &entity_name);
            let mut data = serde_json::Map::new();
            data.insert(key, rows.clone());
            if !data.contains_key(&entity_name) {
                data.insert(entity_name.clone(), rows.clone());
            }
            ResponseTemplate::new(200).set_body_json(json!({ "data": Value::Object(data) }))
        })
        .mount(server)
        .await;
}

/// The parsed `{"query", "variables"}` body of exactly one recorded POST.
async fn single_request_json(server: &MockServer) -> Value {
    let requests = server.received_requests().await.expect("recording enabled");
    assert_eq!(requests.len(), 1, "expected exactly one GraphQL POST");
    requests[0].body_json().expect("request body is JSON")
}

/// Numeric variable view: accepts a JSON number or a decimal string.
fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

fn anchor_row(seq: Value, ts: Value, tx_hash: Value) -> Value {
    json!({
        "seq": seq,
        "entry_hash": "0x11aa",
        "root": "0x22bb",
        "account": "0x33cc",
        "ts": ts,
        "tx_hash": tx_hash,
    })
}

fn heartbeat_row(ts: Value) -> Value {
    json!({
        "guardian": "0xhb",
        "risk_state_hash": "0xhh",
        "max_tier": 2,
        "ts": ts,
        "tx_hash": Value::Null,
    })
}

fn liquidation_row(ts: Value) -> Value {
    json!({
        "market_id": 32,
        "account": "0xll",
        "size": "1.5",
        "price": "2000.25",
        "ts": ts,
        "tx_hash": "0xlt",
        "collateral_lost": "0.5",
    })
}

// ===========================================================================
// Black-box client corpus (wiremock; localhost only, no live network)
// ===========================================================================

#[tokio::test]
async fn anchor_query_uses_exact_entity_variables_and_field_names() {
    let server = MockServer::start().await;
    mount_graphql_rows(
        &server,
        "SentinelAnchor",
        json!([{
            "seq": 41,
            "entry_hash": "0x11aa",
            "root": "0x22bb",
            "account": "0x33cc",
            "ts": 1_700_000_000,
            "tx_hash": "0x44dd",
        }]),
    )
    .await;

    let client = IndexerClient::new(server.uri());
    let rows = client.anchors_after(41).await.expect("200 response parses");
    assert_eq!(
        rows,
        vec![SentinelAnchor {
            seq: 41,
            entry_hash: "0x11aa".to_string(),
            root: "0x22bb".to_string(),
            account: "0x33cc".to_string(),
            ts: 1_700_000_000,
            tx_hash: Some("0x44dd".to_string()),
        }]
    );

    let body = single_request_json(&server).await;
    let query = body["query"].as_str().expect("query text is a string");
    assert!(
        query.contains("SentinelAnchor"),
        "anchors query must name the exact entity: {query}"
    );
    for absent in [
        "SentinelHeartbeat",
        "Liquidation",
        "PerpFill",
        "AccountSnapshotDay",
    ] {
        assert!(
            !query.contains(absent),
            "anchors query must not touch `{absent}`: {query}"
        );
    }
    for field in ["seq", "entry_hash", "root", "account", "ts", "tx_hash"] {
        assert!(
            query.contains(field),
            "anchors query must select frozen field `{field}`: {query}"
        );
    }
    let vars = &body["variables"];
    // Probed wire shape (locked here): the anchors call declares
    // `query($from: BigInt!)` and sends `{"from":"41"}` — the seq lower bound
    // travels as an Envio `BigInt` *string*. The corpus name `from_seq` is
    // accepted as an alias so the check survives a rename; either way the
    // bound must be present and equal 41.
    let seq_bound = ["from_seq", "from"]
        .iter()
        .find_map(|key| vars.get(*key).and_then(num));
    assert_eq!(
        seq_bound,
        Some(41),
        "anchors call must send the seq lower bound (as `from_seq`/`from`): {vars}"
    );
    // No `limit` is sent for anchors: the frozen signature carries none and
    // the documented example is "all anchors ordered by seq" — the server's
    // default page size applies (observation recorded in the P12 report).
    let query_lower = query.to_lowercase();
    assert!(
        query_lower.contains("_gte"),
        "anchors query must filter `seq >= from`: {query}"
    );
    assert!(
        query_lower.contains("order_by"),
        "anchors query must order by seq: {query}"
    );
    assert!(
        query_lower.contains("asc"),
        "anchors order must be ascending (frozen contract): {query}"
    );
}

#[tokio::test]
async fn heartbeat_query_uses_exact_entity_variables_and_field_names() {
    let server = MockServer::start().await;
    mount_graphql_rows(
        &server,
        "SentinelHeartbeat",
        json!([heartbeat_row(json!(1_700_000_100))]),
    )
    .await;

    let client = IndexerClient::new(server.uri());
    let rows = client.heartbeats(7).await.expect("200 response parses");
    assert_eq!(
        rows,
        vec![SentinelHeartbeat {
            guardian: "0xhb".to_string(),
            risk_state_hash: "0xhh".to_string(),
            max_tier: 2,
            ts: 1_700_000_100,
            tx_hash: None,
        }]
    );

    let body = single_request_json(&server).await;
    let query = body["query"].as_str().expect("query text is a string");
    assert!(
        query.contains("SentinelHeartbeat"),
        "heartbeats query must name the exact entity: {query}"
    );
    for absent in [
        "SentinelAnchor",
        "Liquidation",
        "PerpFill",
        "AccountSnapshotDay",
    ] {
        assert!(
            !query.contains(absent),
            "heartbeats query must not touch `{absent}`: {query}"
        );
    }
    for field in ["guardian", "risk_state_hash", "max_tier", "ts"] {
        assert!(
            query.contains(field),
            "heartbeats query must select frozen field `{field}`: {query}"
        );
    }
    let vars = &body["variables"];
    assert_eq!(num(&vars["limit"]), Some(7), "limit variable: {vars}");
}

#[tokio::test]
async fn liquidation_query_with_market_filter_uses_exact_entity_and_variables() {
    let server = MockServer::start().await;
    mount_graphql_rows(
        &server,
        "Liquidation",
        json!([liquidation_row(json!(1_700_000_200))]),
    )
    .await;

    let client = IndexerClient::new(server.uri());
    let rows = client
        .recent_liquidations(Some(32), 9)
        .await
        .expect("200 response parses");
    assert_eq!(
        rows,
        vec![LiquidationRow {
            market_id: 32,
            account: "0xll".to_string(),
            size: "1.5".to_string(),
            price: "2000.25".to_string(),
            ts: 1_700_000_200,
            tx_hash: Some("0xlt".to_string()),
            collateral_lost: Some("0.5".to_string()),
        }]
    );

    let body = single_request_json(&server).await;
    let query = body["query"].as_str().expect("query text is a string");
    assert!(
        query.contains("Liquidation"),
        "liquidations query must name the exact entity: {query}"
    );
    for absent in [
        "SentinelAnchor",
        "SentinelHeartbeat",
        "PerpFill",
        "AccountSnapshotDay",
    ] {
        assert!(
            !query.contains(absent),
            "liquidations query must not touch `{absent}`: {query}"
        );
    }
    for field in ["market_id", "account", "size", "price", "ts", "tx_hash"] {
        assert!(
            query.contains(field),
            "liquidations query must select frozen field `{field}`: {query}"
        );
    }
    let vars = &body["variables"];
    assert_eq!(num(&vars["limit"]), Some(9), "limit variable: {vars}");
    assert_eq!(num(&vars["market"]), Some(32), "market variable: {vars}");
}

#[tokio::test]
async fn liquidation_query_without_market_filter_does_not_constrain_market() {
    let server = MockServer::start().await;
    mount_graphql_rows(
        &server,
        "Liquidation",
        json!([liquidation_row(json!(1_700_000_200))]),
    )
    .await;

    let client = IndexerClient::new(server.uri());
    let rows = client
        .recent_liquidations(None, 9)
        .await
        .expect("200 response parses");
    assert_eq!(rows.len(), 1);

    let body = single_request_json(&server).await;
    let vars = &body["variables"];
    assert_eq!(num(&vars["limit"]), Some(9), "limit variable: {vars}");
    match vars.get("market") {
        None => {}
        Some(Value::Null) => {}
        Some(other) => panic!(
            "unfiltered liquidations call must omit `market` (or send null), got {other}: {vars}"
        ),
    }
}

#[tokio::test]
async fn bigint_fields_accept_string_and_number_forms_equivalently() {
    // Anchors: one row with seq/ts as strings (Envio BigInt wire form), one
    // with the same values as JSON numbers.
    let anchors_server = MockServer::start().await;
    mount_graphql_rows(
        &anchors_server,
        "SentinelAnchor",
        json!([
            anchor_row(json!("41"), json!("1700000000"), json!("0x44dd")),
            anchor_row(json!(41), json!(1_700_000_000), json!("0x44dd")),
        ]),
    )
    .await;
    let anchors = IndexerClient::new(anchors_server.uri())
        .anchors_after(41)
        .await
        .expect("both BigInt forms parse");
    assert_eq!(anchors.len(), 2);
    assert_eq!(
        anchors[0], anchors[1],
        "string and number BigInt forms must be equivalent"
    );
    assert_eq!(anchors[0].seq, 41);
    assert_eq!(anchors[0].ts, 1_700_000_000);

    // Heartbeats: ts both ways.
    let heartbeats_server = MockServer::start().await;
    mount_graphql_rows(
        &heartbeats_server,
        "SentinelHeartbeat",
        json!([
            heartbeat_row(json!("1700000100")),
            heartbeat_row(json!(1_700_000_100))
        ]),
    )
    .await;
    let heartbeats = IndexerClient::new(heartbeats_server.uri())
        .heartbeats(5)
        .await
        .expect("both BigInt forms parse");
    assert_eq!(heartbeats.len(), 2);
    assert_eq!(
        heartbeats[0], heartbeats[1],
        "string and number BigInt forms must be equivalent"
    );
    assert_eq!(heartbeats[0].ts, 1_700_000_100);
    assert_eq!(heartbeats[0].max_tier, 2);

    // Liquidations: ts both ways.
    let liquidations_server = MockServer::start().await;
    mount_graphql_rows(
        &liquidations_server,
        "Liquidation",
        json!([
            liquidation_row(json!("1700000200")),
            liquidation_row(json!(1_700_000_200))
        ]),
    )
    .await;
    let liquidations = IndexerClient::new(liquidations_server.uri())
        .recent_liquidations(None, 5)
        .await
        .expect("both BigInt forms parse");
    assert_eq!(liquidations.len(), 2);
    assert_eq!(
        liquidations[0], liquidations[1],
        "string and number BigInt forms must be equivalent"
    );
    assert_eq!(liquidations[0].ts, 1_700_000_200);
    assert_eq!(liquidations[0].size, "1.5");
}

#[tokio::test]
async fn null_and_absent_tx_hash_deserialize_to_none() {
    let anchors_server = MockServer::start().await;
    mount_graphql_rows(
        &anchors_server,
        "SentinelAnchor",
        json!([
            anchor_row(json!(1), json!(1_700_000_001), Value::Null),
            {
                "seq": 2,
                "entry_hash": "0x11aa",
                "root": "0x22bb",
                "account": "0x33cc",
                "ts": 1_700_000_002,
            },
        ]),
    )
    .await;
    let anchors = IndexerClient::new(anchors_server.uri())
        .anchors_after(0)
        .await
        .expect("null/absent tx_hash parses");
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[0].seq, 1);
    assert_eq!(anchors[0].tx_hash, None, "`tx_hash: null` must be `None`");
    assert_eq!(anchors[1].seq, 2);
    assert_eq!(
        anchors[1].tx_hash, None,
        "absent `tx_hash` must default to `None`"
    );

    let liquidations_server = MockServer::start().await;
    mount_graphql_rows(
        &liquidations_server,
        "Liquidation",
        json!([
            {
                "market_id": 32,
                "account": "0xll",
                "size": "1.5",
                "price": "2000.25",
                "ts": 1_700_000_200,
                "tx_hash": Value::Null,
                "collateral_lost": Value::Null,
            },
            {
                "market_id": 32,
                "account": "0xll",
                "size": "1.5",
                "price": "2000.25",
                "ts": 1_700_000_201,
                "tx_hash": "0xlt",
            },
        ]),
    )
    .await;
    let liquidations = IndexerClient::new(liquidations_server.uri())
        .recent_liquidations(None, 5)
        .await
        .expect("null/absent optionals parse");
    assert_eq!(liquidations.len(), 2);
    assert_eq!(liquidations[0].tx_hash, None);
    assert_eq!(
        liquidations[0].collateral_lost, None,
        "`collateral_lost: null` must be `None`"
    );
    assert_eq!(
        liquidations[1].collateral_lost, None,
        "absent `collateral_lost` must default to `None`"
    );
}

#[tokio::test]
async fn graphql_errors_nonempty_is_err_mentioning_graphql() {
    // Errors with no data: the canonical GraphQL failure envelope.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "boom from the indexer" }],
        })))
        .mount(&server)
        .await;
    let err = tokio::time::timeout(
        Duration::from_secs(60),
        IndexerClient::new(server.uri()).anchors_after(0),
    )
    .await
    .expect("errors path must not hang")
    .expect_err("non-empty `errors` must be an Err");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("graphql"),
        "error must mention `graphql`, got: {err}"
    );

    // Errors alongside partial data: still Err (the array is non-empty).
    let partial_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "SentinelAnchor": [] },
            "errors": [{ "message": "partial failure" }],
        })))
        .mount(&partial_server)
        .await;
    let err = tokio::time::timeout(
        Duration::from_secs(60),
        IndexerClient::new(partial_server.uri()).anchors_after(0),
    )
    .await
    .expect("errors path must not hang")
    .expect_err("non-empty `errors` must be an Err even with partial data");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("graphql"),
        "error must mention `graphql`, got: {err}"
    );
}

#[tokio::test]
async fn empty_data_object_behaviour_is_deterministic() {
    // Probed against the implementation and locked: an empty `data` object
    // with no entity key is an ERROR, not a silent empty page —
    // `Err(internal: indexer graphql: missing or malformed `data.<Entity>`
    // array)`. All three methods share this behaviour; each call runs twice.
    for (name, first, second) in [
        (
            "anchors_after",
            run_empty_data_call(EmptyCall::Anchors).await,
            run_empty_data_call(EmptyCall::Anchors).await,
        ),
        (
            "heartbeats",
            run_empty_data_call(EmptyCall::Heartbeats).await,
            run_empty_data_call(EmptyCall::Heartbeats).await,
        ),
        (
            "recent_liquidations",
            run_empty_data_call(EmptyCall::Liquidations).await,
            run_empty_data_call(EmptyCall::Liquidations).await,
        ),
    ] {
        let first_err = match first {
            Ok(rows) => {
                panic!("{name}: locked behaviour is Err for empty `data`, got Ok({rows} rows)")
            }
            Err(err) => err,
        };
        let second_err = second.expect_err(&format!(
            "{name}: outcome must be deterministic (first was Err)"
        ));
        assert_eq!(
            first_err.to_string(),
            second_err.to_string(),
            "{name}: empty-data error must be deterministic"
        );
        assert!(
            first_err.to_string().contains("graphql"),
            "{name}: empty-data error must carry the graphql vocabulary, got: {first_err}"
        );
    }
}

#[derive(Clone, Copy)]
enum EmptyCall {
    Anchors,
    Heartbeats,
    Liquidations,
}

/// `Ok(len)` / `Err(discriminant)` of one call against `{"data":{}}`.
async fn run_empty_data_call(call: EmptyCall) -> Result<usize, sentinel::error::SentinelError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;
    let client = IndexerClient::new(server.uri());
    match call {
        EmptyCall::Anchors => client.anchors_after(0).await.map(|rows| rows.len()),
        EmptyCall::Heartbeats => client.heartbeats(5).await.map(|rows| rows.len()),
        EmptyCall::Liquidations => client
            .recent_liquidations(None, 5)
            .await
            .map(|rows| rows.len()),
    }
}

#[tokio::test]
async fn huge_u64_string_parses_at_max_and_errors_past_max_without_panic() {
    // u64::MAX itself is a valid `BigInt` for a u64 field.
    let max_server = MockServer::start().await;
    mount_graphql_rows(
        &max_server,
        "SentinelAnchor",
        json!([anchor_row(
            json!("18446744073709551615"),
            json!(1),
            Value::Null
        )]),
    )
    .await;
    let rows = IndexerClient::new(max_server.uri())
        .anchors_after(0)
        .await
        .expect("u64::MAX string must parse");
    assert_eq!(rows[0].seq, u64::MAX);

    // One past u64::MAX: an Err, never a panic.
    let overflow_server = MockServer::start().await;
    mount_graphql_rows(
        &overflow_server,
        "SentinelAnchor",
        json!([anchor_row(
            json!("18446744073709551616"),
            json!(1),
            Value::Null
        )]),
    )
    .await;
    let result = IndexerClient::new(overflow_server.uri())
        .anchors_after(0)
        .await;
    assert!(
        result.is_err(),
        "seq one past u64::MAX must be rejected, got {result:?}"
    );
}

#[tokio::test]
async fn empty_and_whitespace_endpoints_never_panic() {
    // Probed against the implementation and locked: construction is
    // infallible by signature (it must not panic), and every degenerate
    // endpoint fails at request time as
    // `Err(internal: indexer graphql: transport error: builder error)` —
    // never a hang, never a fabricated empty page.
    for endpoint in ["", "   ", "\t"] {
        let client = IndexerClient::new(endpoint);
        let outcome = tokio::time::timeout(Duration::from_secs(30), client.anchors_after(0))
            .await
            .unwrap_or_else(|_| panic!("endpoint {endpoint:?} must not hang"));
        match outcome {
            Ok(rows) => panic!(
                "endpoint {endpoint:?}: locked behaviour is Err, got Ok({} rows)",
                rows.len()
            ),
            Err(err) => assert!(
                !err.to_string().is_empty(),
                "endpoint {endpoint:?}: error must carry a message"
            ),
        }
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // std mutex intentionally serializes this test's env section end-to-end
async fn from_env_unset_is_none_set_is_some_serial() {
    const KEY: &str = "ENVIO_GRAPHQL_ENDPOINT";

    // SAFETY: `set_var`/`remove_var` are unsafe in Rust 2024 because the
    // environment is process-global. ENV_LOCK serializes every env mutation in
    // this binary, and this is the only test that reads or writes this key.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let saved = std::env::var(KEY).ok();

    let server = MockServer::start().await;
    mount_graphql_rows(
        &server,
        "SentinelAnchor",
        json!([anchor_row(json!(1), json!(1_700_000_000), Value::Null)]),
    )
    .await;

    unsafe { std::env::remove_var(KEY) };
    assert!(
        IndexerClient::from_env().is_none(),
        "unset `{KEY}` must degrade to None"
    );

    unsafe { std::env::set_var(KEY, server.uri()) };
    let client = IndexerClient::from_env().expect("set endpoint yields Some");
    let rows = client
        .anchors_after(0)
        .await
        .expect("client built from env must query the endpoint");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 1);
    let body = single_request_json(&server).await;
    assert!(
        body["query"]
            .as_str()
            .unwrap_or_default()
            .contains("SentinelAnchor"),
        "env-built client must issue the anchors query"
    );

    match saved {
        Some(value) => unsafe { std::env::set_var(KEY, value) },
        None => unsafe { std::env::remove_var(KEY) },
    }
}

// ===========================================================================
// Artifact cross-check (text level only; no execution of the indexer)
// ===========================================================================

/// The entity block `type <name> { ... }` of a GraphQL SDL document.
fn type_block<'a>(sdl: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("type {name} {{");
    let start = sdl.find(&marker)?;
    let rest = &sdl[start..];
    let end = rest.find("\n}")?;
    Some(&rest[..end])
}

/// True when the block declares field `name` (`name:` at a line start), with
/// `@index` annotations and inline comments tolerated, comment lines ignored.
fn declares_field(block: &str, name: &str) -> bool {
    block.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            return false;
        }
        match trimmed.strip_prefix(name) {
            Some(rest) => rest.trim_start().starts_with(':'),
            None => false,
        }
    })
}

/// `entry_hash` -> `entryHash` (the rename this check guards against).
fn camel_variant(name: &str) -> Option<String> {
    if !name.contains('_') {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    let mut upper_next = false;
    for ch in name.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

#[test]
fn schema_graphql_declares_five_entities_with_frozen_field_names() {
    let path = repo_root().join("indexer/schema.graphql");
    let sdl = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} unreadable: {err}", path.display()));

    // Frozen entity/field lists, verbatim from the P12 prompt pack §2.
    let frozen: &[(&str, &[&str])] = &[
        (
            "PerpFill",
            &[
                "id",
                "market_id",
                "account",
                "side",
                "price",
                "size",
                "ts",
                "tx_hash",
                "block_number",
            ],
        ),
        (
            "Liquidation",
            &[
                "id",
                "market_id",
                "account",
                "size",
                "price",
                "ts",
                "tx_hash",
                "collateral_lost",
            ],
        ),
        (
            "AccountSnapshotDay",
            &["id", "account", "day", "max_tier", "min_distance_pct"],
        ),
        (
            "SentinelAnchor",
            &["seq", "entry_hash", "root", "account", "ts", "tx_hash"],
        ),
        (
            "SentinelHeartbeat",
            &["guardian", "risk_state_hash", "max_tier", "ts"],
        ),
    ];

    for (entity, fields) in frozen {
        let block = type_block(&sdl, entity)
            .unwrap_or_else(|| panic!("`type {entity} {{` missing in {}", path.display()));
        for field in *fields {
            assert!(
                declares_field(block, field),
                "`{entity}` must declare frozen field `{field}`:\n{block}"
            );
            if let Some(camel) = camel_variant(field) {
                assert!(
                    !declares_field(block, &camel),
                    "`{entity}.{field}` must keep its exact frozen name (found camelCase `{camel}`)"
                );
            }
        }
    }

    let type_count = sdl
        .lines()
        .filter(|line| line.trim_start().starts_with("type "))
        .count();
    assert_eq!(
        type_count,
        frozen.len(),
        "schema.graphql must declare exactly the five frozen entities"
    );
}

/// Case-insensitive text scan of every readable (non-hidden, non-vendor) file
/// under `dir`.
fn tree_contains_case_insensitive(dir: &Path, needle_lower: &str) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if name == "node_modules" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if let Ok(text) = fs::read_to_string(&path)
                && text.to_lowercase().contains(needle_lower)
            {
                return true;
            }
        }
    }
    false
}

#[test]
fn config_and_fallback_evidence_are_consistent() {
    let root = repo_root();
    let config_path = root.join("indexer/config.yaml");
    let config = fs::read_to_string(&config_path)
        .unwrap_or_else(|err| panic!("{} unreadable: {err}", config_path.display()));

    assert!(
        config.contains("10143")
            || (config.contains("31337") && config.to_lowercase().contains("anvil")),
        "config.yaml must reference Monad testnet chain 10143 (or document anvil 31337 \
         for the local proof)"
    );

    // The Perpl exchange contract is "active" only when it appears outside
    // comments; otherwise the ABI fallback is in force.
    let active_config: String = config
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    if active_config.contains("PerplExchange") {
        assert!(
            tree_contains_case_insensitive(&root.join("indexer"), EXCHANGE_ADDRESS),
            "with the Perpl exchange active, {EXCHANGE_ADDRESS} must appear somewhere in indexer/"
        );
    } else {
        let readme_path = root.join("indexer/README.md");
        let readme = fs::read_to_string(&readme_path)
            .unwrap_or_else(|err| panic!("{} unreadable: {err}", readme_path.display()));
        let readme_lower = readme.to_lowercase();
        assert!(
            readme_lower.contains("roadmap") && readme_lower.contains("perpl"),
            "Perpl ABI fallback applied: indexer/README.md must mark Perpl-event indexing \
             as roadmap"
        );
    }
}

#[test]
fn docs_queries_graphql_documents_the_four_example_queries() {
    let root = repo_root();
    let candidates = [
        root.join("indexer/docs/queries.graphql"),
        root.join("docs/queries.graphql"),
    ];
    let path = candidates
        .iter()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| panic!("queries.graphql missing; looked at {candidates:?}"));
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{} unreadable: {err}", path.display()));
    let lower = text.to_lowercase();

    // Four documented example queries (P12 prompt §4): non-comment lines that
    // open a query operation.
    let operations = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| line.contains("query"))
        .count();
    assert!(
        operations >= 4,
        "expected >= 4 documented queries in {}: found {operations}",
        path.display()
    );

    for entity in [
        "Liquidation",
        "PerpFill",
        "SentinelAnchor",
        "SentinelHeartbeat",
    ] {
        assert!(
            text.contains(entity),
            "{} must document a query over `{entity}`",
            path.display()
        );
    }
    assert!(
        text.contains("50"),
        "the last-50-liquidations example is part of the frozen corpus"
    );
    assert!(
        lower.contains("order_by") || lower.contains("orderby"),
        "the anchors-by-seq example must order by `seq`"
    );
    assert!(
        text.contains("seq"),
        "the anchors example must mention `seq`"
    );
    assert!(
        lower.contains("gap") || lower.contains("interval"),
        "the heartbeat-gap example (`> 2x interval`) must be present"
    );
}

#[test]
fn p12_local_run_evidence_and_cited_references_are_consistent() {
    let root = repo_root();

    // (4) Local anvil run evidence, when present: a SentinelAnchor result
    // block or a documented blocker (content consistency only).
    let evidence_candidates = [
        root.join("docs/evidence/p12-envio-anvil.txt"),
        root.join("indexer/docs/evidence/p12-envio-anvil.txt"),
    ];
    if let Some(path) = evidence_candidates
        .iter()
        .find(|candidate| candidate.exists())
    {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("{} unreadable: {err}", path.display()));
        let lower = text.to_lowercase();
        let has_result_block = lower.contains("sentinelanchor")
            && (lower.contains("\"data\"") || lower.contains("seq"));
        let has_blocker = lower.contains("blocker");
        assert!(
            has_result_block || has_blocker,
            "{} must contain a SentinelAnchor query result block or a documented blocker",
            path.display()
        );
    }

    // Every cited `docs/evidence/p12-*.{txt,md}` reference must resolve.
    let mut references: Vec<String> = Vec::new();
    for doc in [
        root.join("indexer/config.yaml"),
        root.join("indexer/README.md"),
    ] {
        let Ok(text) = fs::read_to_string(&doc) else {
            continue;
        };
        let mut rest = text.as_str();
        while let Some(start) = rest.find("docs/evidence/p12-") {
            let tail = &rest[start..];
            let end = tail
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | '(' | '`' | ','))
                .unwrap_or(tail.len());
            let token = tail[..end].trim_end_matches('.').to_string();
            if token.ends_with(".txt") || token.ends_with(".md") {
                references.push(token);
            }
            rest = &tail[end.max(1)..];
        }
    }
    for reference in references {
        let resolved =
            root.join(&reference).exists() || root.join("indexer").join(&reference).exists();
        assert!(
            resolved,
            "cited evidence `{reference}` does not exist under the repo root or indexer/"
        );
    }
}
