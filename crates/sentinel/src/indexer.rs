//! Envio HyperIndex GraphQL client (P12).
//!
//! Reads the anchor trail (audit-verify cross-check + dashboard) and
//! liquidation rows (backtester P13) from the Envio indexer. Endpoint from
//! `ENVIO_GRAPHQL_ENDPOINT` / `cfg.indexer`; a missing endpoint degrades the
//! feature (`from_env() -> None`), never the reflex path.
//!
//! Envio serializes `BigInt` scalars as JSON strings; every integer-typed
//! entity field is decoded tolerantly from a number or from a numeric string.
//!
//! **P12 status:** implemented (`indexer-rs` agent); interfaces frozen.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Result, SentinelError};

/// Request timeout for every indexer call (SPEC-P12: 15 s).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum characters of free text kept in an error message (bounded excerpt).
const MAX_ERROR_EXCERPT_CHARS: usize = 200;

/// `SentinelAnchor` rows with `seq >= $from`, ascending (frozen by SPEC-P12).
const ANCHORS_QUERY: &str = "query($from: BigInt!){ SentinelAnchor(where:{seq:{_gte:$from}}, order_by:{seq:asc}){ seq entry_hash root account ts tx_hash } }";

/// Latest `SentinelHeartbeat` rows, newest first.
const HEARTBEATS_QUERY: &str = "query($limit: Int!){ SentinelHeartbeat(order_by:{ts:desc}, limit:$limit){ guardian risk_state_hash max_tier ts tx_hash } }";

/// Recent `Liquidation` rows, newest first.
const LIQUIDATIONS_QUERY: &str = "query($limit: Int!){ Liquidation(order_by:{ts:desc}, limit:$limit){ market_id account size price ts tx_hash collateral_lost } }";

/// Recent `Liquidation` rows for one market, newest first.
const LIQUIDATIONS_BY_MARKET_QUERY: &str = "query($market: BigInt!, $limit: Int!){ Liquidation(where:{market_id:{_eq:$market}}, order_by:{ts:desc}, limit:$limit){ market_id account size price ts tx_hash collateral_lost } }";

/// `SentinelAnchor` entity (`SPEC` P12 §2 field names verbatim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentinelAnchor {
    /// Journal sequence (on-chain = journal seq + 1, per SPEC-P10 §13b).
    pub seq: u64,
    /// Anchored entry hash.
    pub entry_hash: String,
    /// Batch running root.
    pub root: String,
    /// Anchoring account.
    pub account: String,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// `SentinelHeartbeat` entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentinelHeartbeat {
    /// Guardian account.
    pub guardian: String,
    /// Risk-state hash from the beat.
    pub risk_state_hash: String,
    /// Max tier at beat time.
    pub max_tier: u8,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// `Liquidation` entity row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquidationRow {
    /// Market id.
    pub market_id: u32,
    /// Liquidated account.
    pub account: String,
    /// Position size liquidated (decimal string).
    pub size: String,
    /// Liquidation price (decimal string).
    pub price: String,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Collateral lost (decimal string), when indexed.
    #[serde(default)]
    pub collateral_lost: Option<String>,
}

/// GraphQL client for the Envio indexer.
pub struct IndexerClient {
    endpoint: String,
    http: reqwest::Client,
}

impl IndexerClient {
    /// Build from `ENVIO_GRAPHQL_ENDPOINT`; `None` when unset or empty
    /// (feature off).
    pub fn from_env() -> Option<Self> {
        std::env::var("ENVIO_GRAPHQL_ENDPOINT")
            .ok()
            .map(|endpoint| endpoint.trim().to_string())
            .filter(|endpoint| !endpoint.is_empty())
            .map(Self::new)
    }

    /// Build against an explicit endpoint (surrounding whitespace and
    /// trailing `/` characters trimmed).
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim().trim_end_matches('/').to_string(),
            http: build_client(),
        }
    }

    /// Anchors with `seq >= from_seq`, ascending.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn anchors_after(&self, from_seq: u64) -> Result<Vec<SentinelAnchor>> {
        let data = self
            .post_query(ANCHORS_QUERY, json!({ "from": from_seq.to_string() }))
            .await?;
        parse_entity(&data, "SentinelAnchor", anchor_from_row)
    }

    /// Latest heartbeats, newest first.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn heartbeats(&self, limit: u32) -> Result<Vec<SentinelHeartbeat>> {
        let data = self
            .post_query(HEARTBEATS_QUERY, json!({ "limit": limit }))
            .await?;
        parse_entity(&data, "SentinelHeartbeat", heartbeat_from_row)
    }

    /// Recent liquidations, optionally filtered by market.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn recent_liquidations(
        &self,
        market_id: Option<u32>,
        limit: u32,
    ) -> Result<Vec<LiquidationRow>> {
        let (query, variables) = match market_id {
            Some(market) => (
                LIQUIDATIONS_BY_MARKET_QUERY,
                json!({ "market": market.to_string(), "limit": limit }),
            ),
            None => (LIQUIDATIONS_QUERY, json!({ "limit": limit })),
        };
        let data = self.post_query(query, variables).await?;
        parse_entity(&data, "Liquidation", liquidation_from_row)
    }

    /// One `POST {endpoint}` GraphQL call; returns the `data` object of the
    /// response.
    ///
    /// A non-empty top-level `errors` array fails the call with a bounded
    /// excerpt of the errors; an HTTP error status, a transport failure or a
    /// non-JSON body fails with a bounded excerpt of the body.
    async fn post_query(&self, query: &str, variables: Value) -> Result<Value> {
        let payload = json!({ "query": query, "variables": variables });
        let response = self
            .http
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                gql_error(format!(
                    "transport error: {}",
                    error_excerpt(&err.to_string())
                ))
            })?;

        let status = response.status();
        let body = response.text().await.map_err(|err| {
            gql_error(format!(
                "body read error: {}",
                error_excerpt(&err.to_string())
            ))
        })?;

        if !status.is_success() {
            return Err(gql_error(format!(
                "HTTP {}: {}",
                status.as_u16(),
                error_excerpt(&body)
            )));
        }

        let envelope: GraphQlEnvelope = serde_json::from_str(&body).map_err(|err| {
            gql_error(format!(
                "invalid GraphQL response: {err}: {}",
                error_excerpt(&body)
            ))
        })?;

        if let Some(errors) = &envelope.errors {
            match errors {
                Value::Null => {}
                Value::Array(items) if items.is_empty() => {}
                other => {
                    return Err(gql_error(format!(
                        "GraphQL errors: {}",
                        error_excerpt(&other.to_string())
                    )));
                }
            }
        }

        envelope.data.ok_or_else(|| {
            gql_error(format!(
                "missing `data` in response: {}",
                error_excerpt(&body)
            ))
        })
    }
}

/// Minimal GraphQL response envelope: the `data` object plus the optional
/// `errors` array.
#[derive(Debug, Deserialize)]
struct GraphQlEnvelope {
    /// Payload object of a successful query.
    #[serde(default)]
    data: Option<Value>,
    /// Top-level GraphQL errors, when the server reports any.
    #[serde(default)]
    errors: Option<Value>,
}

/// Build the shared HTTP client (15 s timeout, SPEC-P12).
///
/// The frozen `new` signature returns `Self`, not a `Result`, so a client
/// build failure (possible only when the TLS backend cannot initialise) falls
/// back to the default client with a warning; any resulting per-call
/// transport failure is reported by [`IndexerClient::post_query`].
fn build_client() -> reqwest::Client {
    match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to build the indexer HTTP client with the 15 s timeout; \
                 using the default client"
            );
            reqwest::Client::new()
        }
    }
}

/// Extract the entity array from a `data` object and decode every row.
fn parse_entity<T>(
    data: &Value,
    entity: &str,
    decode: impl Fn(&Value, usize) -> Result<T>,
) -> Result<Vec<T>> {
    let rows = data
        .get(entity)
        .and_then(Value::as_array)
        .ok_or_else(|| gql_error(format!("missing or malformed `data.{entity}` array")))?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| decode(row, index))
        .collect()
}

/// Decode one `SentinelAnchor` row.
fn anchor_from_row(row: &Value, index: usize) -> Result<SentinelAnchor> {
    let at = format!("SentinelAnchor[{index}]");
    let field = |name: &str| format!("{at}.{name}");
    Ok(SentinelAnchor {
        seq: json_integer(required_field(row, "seq", &at)?, &field("seq"))?,
        entry_hash: string_field(
            required_field(row, "entry_hash", &at)?,
            &field("entry_hash"),
        )?,
        root: string_field(required_field(row, "root", &at)?, &field("root"))?,
        account: string_field(required_field(row, "account", &at)?, &field("account"))?,
        ts: json_integer(required_field(row, "ts", &at)?, &field("ts"))?,
        tx_hash: optional_string_field(row, "tx_hash", &field("tx_hash"))?,
    })
}

/// Decode one `SentinelHeartbeat` row.
fn heartbeat_from_row(row: &Value, index: usize) -> Result<SentinelHeartbeat> {
    let at = format!("SentinelHeartbeat[{index}]");
    let field = |name: &str| format!("{at}.{name}");
    Ok(SentinelHeartbeat {
        guardian: string_field(required_field(row, "guardian", &at)?, &field("guardian"))?,
        risk_state_hash: string_field(
            required_field(row, "risk_state_hash", &at)?,
            &field("risk_state_hash"),
        )?,
        max_tier: json_integer(required_field(row, "max_tier", &at)?, &field("max_tier"))?,
        ts: json_integer(required_field(row, "ts", &at)?, &field("ts"))?,
        tx_hash: optional_string_field(row, "tx_hash", &field("tx_hash"))?,
    })
}

/// Decode one `Liquidation` row.
fn liquidation_from_row(row: &Value, index: usize) -> Result<LiquidationRow> {
    let at = format!("Liquidation[{index}]");
    let field = |name: &str| format!("{at}.{name}");
    Ok(LiquidationRow {
        market_id: json_integer(required_field(row, "market_id", &at)?, &field("market_id"))?,
        account: string_field(required_field(row, "account", &at)?, &field("account"))?,
        size: string_field(required_field(row, "size", &at)?, &field("size"))?,
        price: string_field(required_field(row, "price", &at)?, &field("price"))?,
        ts: json_integer(required_field(row, "ts", &at)?, &field("ts"))?,
        tx_hash: optional_string_field(row, "tx_hash", &field("tx_hash"))?,
        collateral_lost: optional_string_field(row, "collateral_lost", &field("collateral_lost"))?,
    })
}

/// Read a required field from an entity row; absent or `null` is a clear
/// error naming the field.
fn required_field<'a>(row: &'a Value, field: &str, at: &str) -> Result<&'a Value> {
    row.get(field)
        .filter(|value| !value.is_null())
        .ok_or_else(|| gql_error(format!("{at}: missing field `{field}`")))
}

/// Decode a string field.
fn string_field(value: &Value, at: &str) -> Result<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| gql_error(format!("{at}: expected a string, got `{value}`")))
}

/// Decode an optional string field (absent or `null` -> `None`).
fn optional_string_field(row: &Value, field: &str, at: &str) -> Result<Option<String>> {
    match row.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => string_field(value, at).map(Some),
    }
}

/// Decode an integer field that Envio may serialize as a JSON number or as a
/// numeric string (`BigInt` scalars are string-serialized).
fn json_integer<T>(value: &Value, at: &str) -> Result<T>
where
    T: TryFrom<u64>,
    T::Error: std::fmt::Display,
{
    let raw: u64 = match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| gql_error(format!("{at}: not a non-negative integer: `{number}`")))?,
        Value::String(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| gql_error(format!("{at}: invalid integer string: {text:?}")))?,
        other => {
            return Err(gql_error(format!(
                "{at}: expected an integer or integer string, got `{other}`"
            )));
        }
    };
    T::try_from(raw).map_err(|err| gql_error(format!("{at}: value out of range: {err}")))
}

/// Every indexer failure is `SentinelError::Internal` with the frozen
/// `indexer graphql: …` prefix.
fn gql_error(detail: impl std::fmt::Display) -> SentinelError {
    SentinelError::Internal(format!("indexer graphql: {detail}"))
}

/// Truncate free text to [`MAX_ERROR_EXCERPT_CHARS`] characters (char-boundary
/// safe). Error messages are the only place response bodies appear.
fn error_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_ERROR_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let mut excerpt: String = trimmed.chars().take(MAX_ERROR_EXCERPT_CHARS - 3).collect();
    excerpt.push_str("...");
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Frozen anchors query text (must match SPEC-P12 verbatim).
    const EXPECTED_ANCHORS_QUERY: &str = "query($from: BigInt!){ SentinelAnchor(where:{seq:{_gte:$from}}, order_by:{seq:asc}){ seq entry_hash root account ts tx_hash } }";

    /// Mount a `200 OK` GraphQL response carrying `data`.
    async fn mount_ok(server: &MockServer, data: Value) {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
            .mount(server)
            .await;
    }

    /// Requests recorded by the mock server, in arrival order.
    async fn received(server: &MockServer) -> Vec<Request> {
        server
            .received_requests()
            .await
            .expect("wiremock records requests")
    }

    /// Parsed JSON body of one recorded request.
    fn body_of(request: &Request) -> Value {
        serde_json::from_slice(&request.body).expect("request body is JSON")
    }

    /// A valid `SentinelAnchor` row (fields overridden per test).
    fn anchor_row() -> Value {
        json!({
            "seq": "7",
            "entry_hash": "0xentry",
            "root": "0xroot",
            "account": "0xaccount",
            "ts": "1700000000",
            "tx_hash": "0xtx",
        })
    }

    #[tokio::test]
    async fn anchors_after_posts_frozen_query_with_from_variable() {
        let server = MockServer::start().await;
        mount_ok(&server, json!({ "SentinelAnchor": [anchor_row()] })).await;

        let client = IndexerClient::new(server.uri());
        let anchors = client.anchors_after(7).await.expect("anchors parse");
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].seq, 7);

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1, "one call, one request");
        assert_eq!(requests[0].method.as_str(), "POST");
        let body = body_of(&requests[0]);
        let query = body["query"].as_str().expect("query text is a string");
        assert!(
            query.contains("SentinelAnchor"),
            "query carries the entity name: {query}"
        );
        assert_eq!(query, EXPECTED_ANCHORS_QUERY, "frozen query text verbatim");
        assert_eq!(
            body["variables"],
            json!({ "from": "7" }),
            "from_seq rides in the variables (BigInt as a lossless string)"
        );
    }

    #[tokio::test]
    async fn anchors_parse_seq_as_string_and_as_number() {
        let server = MockServer::start().await;
        let mut as_string = anchor_row();
        as_string["seq"] = json!("7");
        let mut as_number = anchor_row();
        as_number["seq"] = json!(7);
        mount_ok(&server, json!({ "SentinelAnchor": [as_string, as_number] })).await;

        let anchors = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect("anchors parse");
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].seq, 7, "string seq parses");
        assert_eq!(anchors[1].seq, 7, "number seq parses");
    }

    #[tokio::test]
    async fn anchor_parses_ts_string_and_optional_tx_hash() {
        let server = MockServer::start().await;
        let without_tx = json!({
            "seq": "1",
            "entry_hash": "0xe1",
            "root": "0xr1",
            "account": "0xa1",
            "ts": "1700000000",
        });
        let with_tx = json!({
            "seq": 2,
            "entry_hash": "0xe2",
            "root": "0xr2",
            "account": "0xa2",
            "ts": 1700000001,
            "tx_hash": "0xtx2",
        });
        mount_ok(&server, json!({ "SentinelAnchor": [without_tx, with_tx] })).await;

        let anchors = IndexerClient::new(server.uri())
            .anchors_after(1)
            .await
            .expect("anchors parse");
        assert_eq!(anchors[0].ts, 1_700_000_000, "string ts parses");
        assert_eq!(anchors[0].tx_hash, None, "missing tx_hash is None");
        assert_eq!(anchors[1].ts, 1_700_000_001, "number ts parses");
        assert_eq!(anchors[1].tx_hash.as_deref(), Some("0xtx2"));
    }

    #[tokio::test]
    async fn heartbeats_parse_string_fields_and_send_limit() {
        let server = MockServer::start().await;
        mount_ok(
            &server,
            json!({ "SentinelHeartbeat": [{
                "guardian": "0xguardian",
                "risk_state_hash": "0xrisk",
                "max_tier": "3",
                "ts": "1700000000",
            }] }),
        )
        .await;

        let beats = IndexerClient::new(server.uri())
            .heartbeats(5)
            .await
            .expect("heartbeats parse");
        assert_eq!(beats.len(), 1);
        assert_eq!(beats[0].guardian, "0xguardian");
        assert_eq!(beats[0].risk_state_hash, "0xrisk");
        assert_eq!(beats[0].max_tier, 3, "string max_tier parses");
        assert_eq!(beats[0].ts, 1_700_000_000);
        assert_eq!(beats[0].tx_hash, None);

        let requests = received(&server).await;
        let body = body_of(&requests[0]);
        let query = body["query"].as_str().expect("query text is a string");
        assert!(
            query.contains("SentinelHeartbeat"),
            "query carries the entity name: {query}"
        );
        assert!(
            query.contains("order_by:{ts:desc}, limit:$limit"),
            "query carries the frozen order/limit fragment: {query}"
        );
        assert_eq!(body["variables"], json!({ "limit": 5 }));
    }

    #[tokio::test]
    async fn recent_liquidations_with_market_filter() {
        let server = MockServer::start().await;
        mount_ok(
            &server,
            json!({ "Liquidation": [{
                "market_id": "5",
                "account": "0xvictim",
                "size": "12.5",
                "price": "1.23",
                "ts": "1700000000",
                "collateral_lost": "3.5",
            }] }),
        )
        .await;

        let rows = IndexerClient::new(server.uri())
            .recent_liquidations(Some(5), 10)
            .await
            .expect("liquidations parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].market_id, 5, "string market_id parses");
        assert_eq!(rows[0].size, "12.5");
        assert_eq!(rows[0].price, "1.23");
        assert_eq!(rows[0].collateral_lost.as_deref(), Some("3.5"));
        assert_eq!(rows[0].tx_hash, None);

        let requests = received(&server).await;
        let body = body_of(&requests[0]);
        let query = body["query"].as_str().expect("query text is a string");
        assert!(
            query.contains("Liquidation"),
            "query carries the entity name: {query}"
        );
        assert!(
            query.contains("where:{market_id:{_eq:$market}}"),
            "market filter present in query: {query}"
        );
        assert_eq!(body["variables"], json!({ "market": "5", "limit": 10 }));
    }

    #[tokio::test]
    async fn recent_liquidations_without_market_filter() {
        let server = MockServer::start().await;
        mount_ok(&server, json!({ "Liquidation": [] })).await;

        let rows = IndexerClient::new(server.uri())
            .recent_liquidations(None, 25)
            .await
            .expect("liquidations parse");
        assert!(rows.is_empty());

        let requests = received(&server).await;
        let body = body_of(&requests[0]);
        let query = body["query"].as_str().expect("query text is a string");
        assert!(
            !query.contains("where:"),
            "no filter clause when market is None: {query}"
        );
        assert_eq!(body["variables"], json!({ "limit": 25 }));
        assert!(
            body["variables"].get("market").is_none(),
            "no market variable when the filter is off"
        );
    }

    #[tokio::test]
    async fn graphql_errors_array_yields_bounded_err() {
        let server = MockServer::start().await;
        let long_message = format!("synthetic boom {}", "x".repeat(600));
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": long_message }],
            })))
            .mount(&server)
            .await;

        let err = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect_err("non-empty errors array must fail the call");
        match err {
            SentinelError::Internal(message) => {
                assert!(message.starts_with("indexer graphql:"), "prefix: {message}");
                assert!(
                    message.contains("synthetic boom"),
                    "excerpt keeps the error detail: {message}"
                );
                let len = message.chars().count();
                assert!(len <= 300, "excerpt is bounded ({len} chars): {message}");
            }
            other => panic!("expected SentinelError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_yields_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json {{{"))
            .mount(&server)
            .await;

        let err = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect_err("malformed JSON must fail the call");
        match err {
            SentinelError::Internal(message) => {
                assert!(message.starts_with("indexer graphql:"), "prefix: {message}");
                assert!(
                    message.contains("invalid GraphQL response"),
                    "message names the parse failure: {message}"
                );
            }
            other => panic!("expected SentinelError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_error_status_yields_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
            .mount(&server)
            .await;

        let err = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect_err("HTTP 500 must fail the call");
        match err {
            SentinelError::Internal(message) => {
                assert!(message.contains("HTTP 500"), "status in message: {message}");
                assert!(
                    message.contains("upstream exploded"),
                    "body excerpt in message: {message}"
                );
            }
            other => panic!("expected SentinelError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_entity_array_yields_clear_err() {
        let server = MockServer::start().await;
        mount_ok(&server, json!({ "SentinelHeartbeat": [] })).await;

        let err = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect_err("missing entity array must fail the call");
        match err {
            SentinelError::Internal(message) => {
                assert!(
                    message.contains("SentinelAnchor"),
                    "message names the entity: {message}"
                );
            }
            other => panic!("expected SentinelError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_required_field_yields_clear_err() {
        let server = MockServer::start().await;
        let row = json!({
            "seq": "7",
            "root": "0xroot",
            "account": "0xaccount",
            "ts": "1700000000",
        });
        mount_ok(&server, json!({ "SentinelAnchor": [row] })).await;

        let err = IndexerClient::new(server.uri())
            .anchors_after(0)
            .await
            .expect_err("row missing entry_hash must fail the call");
        match err {
            SentinelError::Internal(message) => {
                assert!(
                    message.contains("entry_hash"),
                    "message names the missing field: {message}"
                );
            }
            other => panic!("expected SentinelError::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn new_trims_trailing_slashes_before_posting() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": { "SentinelAnchor": [] } })),
            )
            .mount(&server)
            .await;

        let client = IndexerClient::new(format!("{}///", server.uri()));
        assert_eq!(client.endpoint, server.uri(), "trailing slashes trimmed");

        let anchors = client
            .anchors_after(0)
            .await
            .expect("call against the trimmed endpoint");
        assert!(anchors.is_empty());

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url.path(),
            "/",
            "recorded request path carries no trailing slash"
        );
    }

    #[test]
    fn from_env_degrades_until_endpoint_set() {
        // Serialize access to the process environment (edition 2024 marks env
        // mutation `unsafe`; the lock keeps this test binary's env access
        // ordered).
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().expect("env lock");

        unsafe { std::env::remove_var("ENVIO_GRAPHQL_ENDPOINT") };
        assert!(
            IndexerClient::from_env().is_none(),
            "unset endpoint degrades the feature"
        );

        unsafe { std::env::set_var("ENVIO_GRAPHQL_ENDPOINT", "") };
        assert!(
            IndexerClient::from_env().is_none(),
            "empty endpoint degrades the feature"
        );

        unsafe { std::env::set_var("ENVIO_GRAPHQL_ENDPOINT", "   ") };
        assert!(
            IndexerClient::from_env().is_none(),
            "blank endpoint degrades the feature"
        );

        unsafe { std::env::set_var("ENVIO_GRAPHQL_ENDPOINT", "http://127.0.0.1:9/graphql/") };
        let client = IndexerClient::from_env().expect("non-empty endpoint yields Some");
        assert_eq!(client.endpoint, "http://127.0.0.1:9/graphql");

        unsafe { std::env::remove_var("ENVIO_GRAPHQL_ENDPOINT") };
    }

    #[test]
    fn json_integer_tolerates_number_or_string_and_rejects_junk() {
        assert_eq!(
            json_integer::<u64>(&json!("7"), "seq").expect("string integer"),
            7
        );
        assert_eq!(json_integer::<u64>(&json!(7), "seq").expect("integer"), 7);
        assert_eq!(
            json_integer::<u8>(&json!("3"), "max_tier").expect("u8 string"),
            3
        );
        assert_eq!(
            json_integer::<u32>(&json!(5), "market_id").expect("u32 number"),
            5
        );

        for bad in [
            json!(null),
            json!("abc"),
            json!(-1),
            json!(1.5),
            json!(true),
        ] {
            assert!(json_integer::<u64>(&bad, "seq").is_err(), "rejects {bad}");
        }
        assert!(
            json_integer::<u8>(&json!(256), "max_tier").is_err(),
            "checks the target range"
        );
    }
}
