//! Gateway executor — signed reduce-only order submission via
//! `POST /v1/trading/orders` (SPEC-P05 §5; gateway facts in `docs/FACTS.md` §1.6).
//!
//! One signed HTTP attempt per submission (never auto-retried). The batch body
//! `{"d":[OrderSpec]}` is serialized once, signed byte-exactly and POSTed to
//! the gateway; the `mt:31` answer maps a zero per-order `code` to
//! `ExecutionStatus::Submitted` ("accepted for forwarding") and a non-zero
//! per-order or batch code to `ExecutionStatus::Rejected`. HTTP 401/403 and
//! transport / parse failures surface as `PerplError::Order` with a bounded
//! excerpt of the response — never a retry (SPEC-P05 §5).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sentinel_core::order::{CloseSide, OrderRequest};
use serde_json::{Value, json};

use super::{
    ExecutionReport, ExecutionStatus, Executor, PositionProbe, SeqCounter, client_order_id,
};
use crate::error::{PerplError, Result, SentinelError};
use crate::perpl::auth::ApiKeySigner;

/// Gateway path of the batch order-submission endpoint.
const ORDERS_TARGET: &str = "/v1/trading/orders";

/// Per-request deadline of the executor's HTTP client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum characters of response text kept as an error excerpt.
const MAX_ERROR_EXCERPT_CHARS: usize = 200;

/// Gateway order type for a reduce-only close of a long (`t = 3`).
const T_CLOSE_LONG: u8 = 3;

/// Gateway order type for a reduce-only close of a short (`t = 4`).
const T_CLOSE_SHORT: u8 = 4;

/// Submits reduce-only orders through the gateway HTTP batch endpoint.
pub struct PerplExecutor<P> {
    base_url: String,
    signer: ApiKeySigner,
    account_id: u64,
    /// Retained for the guarded-executor wire-up (SPEC-P05 §5); submission
    /// itself never consults it.
    #[allow(dead_code)]
    probe: P,
    seq: SeqCounter,
    client: reqwest::Client,
}

impl<P> PerplExecutor<P> {
    /// Build the executor (constructs the HTTP client with a 10 s timeout).
    ///
    /// # Errors
    /// `PerplError::Order` when the HTTP client cannot be built.
    pub fn new(
        base_url: String,
        signer: ApiKeySigner,
        account_id: u64,
        probe: P,
        seq_seed: u64,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| order_error(format!("failed to build HTTP client: {err}")))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            signer,
            account_id,
            probe,
            seq: SeqCounter::new(seq_seed),
            client,
        })
    }

    /// Report for one judged submission (`seq` is the request id used).
    fn report(
        &self,
        order: &OrderRequest,
        seq: u64,
        status: ExecutionStatus,
        detail: String,
    ) -> ExecutionReport {
        ExecutionReport {
            order: order.clone(),
            status,
            filled_size: Decimal::ZERO,
            avg_price: None,
            tx_hash: None,
            client_order_id: client_order_id(order, seq),
            detail,
            ts_ms: now_ms(),
        }
    }

    /// Map an HTTP 200 `mt:31` batch ack onto a report (SPEC-P05 §5).
    ///
    /// A non-zero batch `status.code` refuses the request as a whole (nothing
    /// was acted on); a zero code defers to `statuses[0]`, answered by
    /// position.
    ///
    /// # Errors
    /// `PerplError::Order` when the ack lacks the batch or per-order status.
    fn map_ack(
        &self,
        order: &OrderRequest,
        seq: u64,
        ack: &Value,
        raw_body: &str,
    ) -> Result<ExecutionReport> {
        let batch_code = ack["status"]["code"].as_i64();
        let batch_error = ack["status"]["error"].as_str();
        match batch_code {
            None => Err(order_error(format!(
                "POST {ORDERS_TARGET} ack carries no batch status code: {}",
                error_excerpt(raw_body)
            ))),
            Some(0) => {
                let entry = ack["statuses"]
                    .as_array()
                    .and_then(|statuses| statuses.first());
                match entry.and_then(|entry| entry["code"].as_i64()) {
                    Some(0) => Ok(self.report(
                        order,
                        seq,
                        ExecutionStatus::Submitted,
                        "accepted for forwarding".to_string(),
                    )),
                    Some(code) => Ok(self.report(
                        order,
                        seq,
                        ExecutionStatus::Rejected,
                        rejection_detail(code, entry.and_then(|entry| entry["error"].as_str())),
                    )),
                    None => Err(order_error(format!(
                        "POST {ORDERS_TARGET} ack carries no per-order status: {}",
                        error_excerpt(raw_body)
                    ))),
                }
            }
            Some(code) => Ok(self.report(
                order,
                seq,
                ExecutionStatus::Rejected,
                rejection_detail(code, batch_error),
            )),
        }
    }
}

impl<P> Executor for PerplExecutor<P>
where
    P: PositionProbe + Sync,
{
    /// One signed POST of `{"d":[OrderSpec]}`; single attempt, no retry.
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let seq = self.seq.next();
        let raw = raw_size(order)?;
        let body = serde_json::to_vec(&json!({
            "d": [{
                "rq": seq,
                "mkt": order.market_id.0,
                "acc": self.account_id,
                "t": close_type(order.close),
                "p": 0,
                "s": raw,
                "fl": 0,
                "lv": 0,
                "lb": 0,
                "ms": order.max_slippage_bps,
            }]
        }))
        .map_err(|err| order_error(format!("failed to encode the order batch: {err}")))?;

        let mut headers = HeaderMap::new();
        for (name, value) in self
            .signer
            .signed_request_headers("POST", ORDERS_TARGET, &body)?
        {
            headers.insert(header_name(&name)?, header_value(&value, &name)?);
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let url = format!("{}{}", self.base_url, ORDERS_TARGET);
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|err| {
                order_error(format!(
                    "POST {ORDERS_TARGET} transport error: {}",
                    error_excerpt(&err.to_string())
                ))
            })?;

        let http_status = response.status();
        if http_status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(order_error("unauthorized — check API key"));
        }
        if http_status == reqwest::StatusCode::FORBIDDEN {
            return Err(order_error("order submission lacks trade scope"));
        }

        let text = response.text().await.map_err(|err| {
            order_error(format!(
                "POST {ORDERS_TARGET} response read error: {}",
                error_excerpt(&err.to_string())
            ))
        })?;

        if http_status != reqwest::StatusCode::OK {
            return Err(order_error(format!(
                "POST {ORDERS_TARGET} failed: HTTP {}: {}",
                http_status.as_u16(),
                error_excerpt(&text)
            )));
        }

        let ack: Value = serde_json::from_str(&text).map_err(|err| {
            order_error(format!(
                "POST {ORDERS_TARGET} returned invalid JSON: {err}: {}",
                error_excerpt(&text)
            ))
        })?;
        self.map_ack(order, seq, &ack, &text)
    }
}

/// Gateway order type for the reduce-only close of `close`.
fn close_type(close: CloseSide) -> u8 {
    match close {
        CloseSide::CloseLong => T_CLOSE_LONG,
        CloseSide::CloseShort => T_CLOSE_SHORT,
    }
}

/// Raw integer size for the gateway: `size × 10^size_decimals`, truncated
/// towards zero.
///
/// # Errors
/// `PerplError::Order` when the scale factor or the product leaves the
/// `Decimal` range, or the truncated value does not fit in a `u64` (such
/// orders can never be represented on the gateway).
fn raw_size(order: &OrderRequest) -> Result<u64> {
    let factor = pow10(order.size_decimals).ok_or_else(|| {
        order_error(format!(
            "size_decimals {} cannot be represented as a decimal scale",
            order.size_decimals
        ))
    })?;
    let scaled = order.size.checked_mul(factor).ok_or_else(|| {
        order_error(format!(
            "size {} x 10^{} overflows the decimal range",
            order.size, order.size_decimals
        ))
    })?;
    scaled.to_u64().ok_or_else(|| {
        order_error(format!(
            "raw size {scaled} does not fit the gateway's u64 size"
        ))
    })
}

/// `10^decimals` as a `Decimal`; `None` once the factor exceeds `Decimal::MAX`
/// (10^29 and up), which no representable gateway size can use.
fn pow10(decimals: u32) -> Option<Decimal> {
    let mut factor = Decimal::ONE;
    for _ in 0..decimals {
        factor = factor.checked_mul(Decimal::TEN)?;
    }
    Some(factor)
}

/// Human-readable `Rejected` detail: the gateway code plus its message.
fn rejection_detail(code: i64, error: Option<&str>) -> String {
    match error.filter(|message| !message.is_empty()) {
        Some(message) => format!("rejected: code {code}: {message}"),
        None => format!("rejected: code {code}"),
    }
}

/// Wrap an order failure message in `PerplError::Order`.
fn order_error(message: impl Into<String>) -> SentinelError {
    PerplError::Order(message.into()).into()
}

/// Header name from the signer; failures map to `PerplError::Order`.
fn header_name(name: &str) -> Result<HeaderName> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| order_error(format!("invalid signed header name ({name})")))
}

/// Header value from the signer; failures map to `PerplError::Order` without
/// echoing the value (P00 invariant #5).
fn header_value(value: &str, name: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value)
        .map_err(|_| order_error(format!("invalid signed header value for {name}")))
}

/// Truncate free text to [`MAX_ERROR_EXCERPT_CHARS`] characters (char-boundary
/// safe); response bodies reach errors only through this helper.
fn error_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_ERROR_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let mut excerpt: String = trimmed.chars().take(MAX_ERROR_EXCERPT_CHARS - 3).collect();
    excerpt.push_str("...");
    excerpt
}

/// Current UNIX time in milliseconds (0 if the clock is before the epoch).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use sentinel_core::order::OrderType;
    use sentinel_core::types::{MarketId, Position};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Signer over the frozen test seed (32 x `0x07`, testnet chain 10143).
    fn test_signer() -> ApiKeySigner {
        let seed = format!("0x{}", "07".repeat(32));
        ApiKeySigner::from_parts("test-token", &seed, 10143).expect("test signer builds")
    }

    /// Probe that never reports a position — submissions do not consult it.
    struct NoPositionsProbe;

    impl PositionProbe for NoPositionsProbe {
        async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
            Ok(None)
        }
    }

    /// Executor bound to the local wiremock server (never an external host).
    fn executor_for(server: &MockServer, seq_seed: u64) -> PerplExecutor<NoPositionsProbe> {
        PerplExecutor::new(server.uri(), test_signer(), 7, NoPositionsProbe, seq_seed)
            .expect("executor builds")
    }

    /// A reduce-only order on market 32 (ETH testnet).
    fn order(
        size: Decimal,
        close: CloseSide,
        size_decimals: u32,
        max_slippage_bps: u16,
    ) -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close,
            size,
            order_type: OrderType::Market,
            max_slippage_bps,
            size_decimals,
        }
    }

    /// HTTP 200 response carrying `body` as the `mt:31` ack.
    fn ack_body(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    /// Requests recorded by the mock server, in arrival order.
    async fn received(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .expect("wiremock records requests")
    }

    #[tokio::test]
    async fn success_posts_exact_signed_batch_and_maps_ack_zero_to_submitted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ack_body(json!({
                "mt": 31,
                "status": { "code": 0 },
                "statuses": [ { "code": 0 } ]
            })))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);
        let request = order(Decimal::new(325, 1), CloseSide::CloseLong, 3, 50);
        let report = executor.submit(&request).await.expect("ack 0 is answered");

        assert_eq!(report.status, ExecutionStatus::Submitted);
        assert_eq!(report.detail, "accepted for forwarding");
        assert_eq!(report.client_order_id, "sentinel-32-1");
        assert_eq!(report.order, request);
        assert_eq!(report.filled_size, Decimal::ZERO);
        assert!(report.avg_price.is_none());
        assert!(report.tx_hash.is_none());
        assert!(report.ts_ms > 0);

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1, "single attempt, no retry");
        let sent = &requests[0];
        assert_eq!(sent.method.as_str(), "POST");
        assert_eq!(sent.url.path(), "/v1/trading/orders");

        let api_key = sent
            .headers
            .get("x-api-key")
            .expect("X-API-Key header present")
            .to_str()
            .expect("X-API-Key is ASCII");
        assert_eq!(api_key, "test-token");
        for name in ["x-api-timestamp", "x-api-nonce", "x-api-signature"] {
            let value = sent
                .headers
                .get(name)
                .unwrap_or_else(|| panic!("{name} header present"))
                .to_str()
                .expect("signed headers are ASCII");
            assert!(!value.is_empty(), "{name} must not be empty");
        }
        assert_eq!(
            sent.headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );

        let body: Value = serde_json::from_slice(&sent.body).expect("request body is JSON");
        assert_eq!(
            body.as_object().map(|map| map.len()),
            Some(1),
            "top level carries only `d`"
        );
        assert_eq!(
            body["d"].as_array().map(|orders| orders.len()),
            Some(1),
            "exactly one order in the batch"
        );
        assert_eq!(
            body["d"][0],
            json!({
                "rq": 1,
                "mkt": 32,
                "acc": 7,
                "t": 3,
                "p": 0,
                "s": 32500,
                "fl": 0,
                "lv": 0,
                "lb": 0,
                "ms": 50
            }),
            "d[0] must be the exact frozen OrderSpec"
        );
    }

    #[tokio::test]
    async fn per_order_rejection_maps_to_rejected_with_code_and_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ack_body(json!({
                "mt": 31,
                "status": { "code": 0 },
                "statuses": [ { "code": 403, "error": "invalid account" } ]
            })))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);
        let report = executor
            .submit(&order(Decimal::new(325, 1), CloseSide::CloseLong, 3, 50))
            .await
            .expect("a per-order refusal is still a judged batch");

        assert_eq!(report.status, ExecutionStatus::Rejected);
        assert!(
            report.detail.contains("403"),
            "detail carries the code: {}",
            report.detail
        );
        assert!(
            report.detail.contains("invalid account"),
            "detail carries the error: {}",
            report.detail
        );
        assert_eq!(report.client_order_id, "sentinel-32-1");
    }

    #[tokio::test]
    async fn batch_level_rejection_maps_to_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ack_body(json!({
                "mt": 31,
                "status": { "code": 400, "error": "malformed body" }
            })))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);
        let report = executor
            .submit(&order(Decimal::new(325, 1), CloseSide::CloseLong, 3, 50))
            .await
            .expect("a batch-level refusal comes back as a report");

        assert_eq!(report.status, ExecutionStatus::Rejected);
        assert!(report.detail.contains("400"), "{}", report.detail);
        assert!(
            report.detail.contains("malformed body"),
            "{}",
            report.detail
        );
    }

    #[tokio::test]
    async fn http_401_maps_to_unauthorized_error_without_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);
        let err = executor
            .submit(&order(Decimal::new(1, 0), CloseSide::CloseLong, 0, 10))
            .await
            .expect_err("401 must fail the submission");
        match err {
            SentinelError::Perpl(PerplError::Order(message)) => {
                assert!(
                    message.contains("unauthorized"),
                    "mentions unauthorized: {message}"
                );
            }
            other => panic!("expected PerplError::Order, got {other:?}"),
        }
        assert_eq!(received(&server).await.len(), 1, "401 is never retried");
    }

    #[tokio::test]
    async fn http_403_maps_to_missing_trade_scope_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);
        let err = executor
            .submit(&order(Decimal::new(1, 0), CloseSide::CloseLong, 0, 10))
            .await
            .expect_err("403 must fail the submission");
        match err {
            SentinelError::Perpl(PerplError::Order(message)) => {
                assert!(
                    message.contains("trade scope"),
                    "mentions trade scope: {message}"
                );
            }
            other => panic!("expected PerplError::Order, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_short_posts_t4_with_the_same_exact_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ack_body(json!({
                "mt": 31,
                "status": { "code": 0 },
                "statuses": [ { "code": 0 } ]
            })))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 6);
        let report = executor
            .submit(&order(Decimal::new(325, 1), CloseSide::CloseShort, 3, 50))
            .await
            .expect("submission is answered");

        assert_eq!(report.status, ExecutionStatus::Submitted);
        assert_eq!(report.client_order_id, "sentinel-32-7");

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
        assert_eq!(
            body["d"][0],
            json!({
                "rq": 7,
                "mkt": 32,
                "acc": 7,
                "t": 4,
                "p": 0,
                "s": 32500,
                "fl": 0,
                "lv": 0,
                "lb": 0,
                "ms": 50
            })
        );
    }

    #[tokio::test]
    async fn raw_size_out_of_range_is_refused_before_posting() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/trading/orders"))
            .respond_with(ack_body(json!({
                "mt": 31,
                "status": { "code": 0 },
                "statuses": [ { "code": 0 } ]
            })))
            .mount(&server)
            .await;

        let executor = executor_for(&server, 0);

        // (2^64 - 1) scaled by 10 exceeds u64::MAX: must be refused.
        let too_big = order(Decimal::from(u64::MAX), CloseSide::CloseLong, 1, 10);
        let err = executor
            .submit(&too_big)
            .await
            .expect_err("oversized raw size must be refused");
        assert!(
            matches!(err, SentinelError::Perpl(PerplError::Order(_))),
            "expected PerplError::Order, got {err:?}"
        );

        // A negative size can never be a raw gateway size either.
        let negative = order(Decimal::new(-5, 0), CloseSide::CloseShort, 0, 10);
        let err = executor
            .submit(&negative)
            .await
            .expect_err("negative size must be refused");
        assert!(
            matches!(err, SentinelError::Perpl(PerplError::Order(_))),
            "expected PerplError::Order, got {err:?}"
        );

        assert!(
            received(&server).await.is_empty(),
            "refused orders never reach the network"
        );
    }
}
