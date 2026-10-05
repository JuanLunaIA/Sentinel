//! Typed REST client for the Perpl gateway (snapshots + history).
//!
//! Authenticated endpoints (`/v1/trading/...`) sign every request via
//! [`ApiKeySigner`]; public endpoints (`/v1/pub/context`, `/v1/market-data/...`)
//! are unsigned. Every method returns the raw `serde_json::Value` payload —
//! mapping into domain types happens in [`crate::perpl::types`].
//!
//! Behavior contract (SPEC.md §4): a private helper retries **3 attempts
//! total** on 5xx / timeout / connect errors (backoff 250 ms, 1 s); 4xx are
//! never retried. Logs are structured (`method`, `target`, `status`,
//! `latency_ms`) and never contain header values or response bodies beyond a
//! truncated (≤200-char) error excerpt (P00 invariant #5).

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::error::{PerplError, Result, SentinelError};
use crate::perpl::auth::ApiKeySigner;

/// Total attempts per request (the initial try plus two retries).
const MAX_ATTEMPTS: u32 = 3;

/// Backoff before attempt 2 (250 ms) and before attempt 3 (1 s).
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

/// Per-request deadline, so a hung gateway surfaces as a retryable timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum characters of response text kept as an error excerpt.
const MAX_ERROR_EXCERPT_CHARS: usize = 200;

/// REST client bound to one gateway base URL and one signer.
pub struct PerplRest {
    /// Gateway base URL (trailing `/` characters trimmed in [`PerplRest::new`]).
    base_url: String,
    /// Signer used for the `/v1/trading/...` requests.
    signer: ApiKeySigner,
    /// Shared HTTP client (connection pooling).
    http: reqwest::Client,
}

impl PerplRest {
    /// Build the client.
    ///
    /// `base_url` is used verbatim after trimming trailing `/` characters.
    ///
    /// # Errors
    /// `PerplError::Rest` if the HTTP client cannot be built.
    pub fn new(base_url: impl Into<String>, signer: ApiKeySigner) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| rest_error(format!("failed to build HTTP client: {err}")))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            signer,
            http,
        })
    }

    /// `GET /v1/pub/context` (public, unsigned).
    ///
    /// # Errors
    /// `PerplError::Rest` on transport, HTTP or JSON failure.
    pub async fn get_context(&self) -> Result<Value> {
        self.get_json("/v1/pub/context", false).await
    }

    /// `GET /v1/market-data/ticker` or `.../ticker/<market>` (public, unsigned).
    ///
    /// # Errors
    /// `PerplError::Rest` on transport, HTTP or JSON failure.
    pub async fn get_ticker(&self, market_id: Option<u32>) -> Result<Value> {
        match market_id {
            Some(id) => {
                self.get_json(&format!("/v1/market-data/ticker/{id}"), false)
                    .await
            }
            None => self.get_json("/v1/market-data/ticker", false).await,
        }
    }

    /// `GET /v1/trading/wallet` (signed).
    ///
    /// # Errors
    /// `PerplError::Auth` if the request cannot be signed; `PerplError::Rest`
    /// on transport, HTTP or JSON failure.
    pub async fn get_wallet(&self) -> Result<Value> {
        self.get_json("/v1/trading/wallet", true).await
    }

    /// `GET /v1/trading/positions` (signed).
    ///
    /// # Errors
    /// `PerplError::Auth` if the request cannot be signed; `PerplError::Rest`
    /// on transport, HTTP or JSON failure.
    pub async fn get_positions(&self) -> Result<Value> {
        self.get_json("/v1/trading/positions", true).await
    }

    /// `GET /v1/trading/orders` (signed).
    ///
    /// # Errors
    /// `PerplError::Auth` if the request cannot be signed; `PerplError::Rest`
    /// on transport, HTTP or JSON failure.
    pub async fn get_orders(&self) -> Result<Value> {
        self.get_json("/v1/trading/orders", true).await
    }

    /// `GET /v1/trading/fills?count=<n>` (signed).
    ///
    /// # Errors
    /// `PerplError::Auth` if the request cannot be signed; `PerplError::Rest`
    /// on transport, HTTP or JSON failure.
    pub async fn get_fills(&self, count: u32) -> Result<Value> {
        self.get_json(&format!("/v1/trading/fills?count={count}"), true)
            .await
    }

    /// `GET /v1/trading/account-history?count=<n>` (signed).
    ///
    /// # Errors
    /// `PerplError::Auth` if the request cannot be signed; `PerplError::Rest`
    /// on transport, HTTP or JSON failure.
    pub async fn get_account_history(&self, count: u32) -> Result<Value> {
        self.get_json(&format!("/v1/trading/account-history?count={count}"), true)
            .await
    }

    /// `GET <target>` with the shared retry policy: 3 attempts total on
    /// 5xx / timeout / connect failures (250 ms, then 1 s backoff); 4xx and
    /// every other failure stop after the first attempt. `signed` attaches
    /// `X-API-Key` / `X-API-Timestamp` / `X-API-Nonce` / `X-API-Signature`.
    async fn get_json(&self, target: &str, signed: bool) -> Result<Value> {
        let url = format!("{}{}", self.base_url, target);
        let mut headers = HeaderMap::new();
        if signed {
            for (name, value) in self.signer.signed_request_headers("GET", target, &[])? {
                headers.insert(header_name(&name)?, header_value(&value, &name)?);
            }
        }

        let mut attempt: u32 = 1;
        loop {
            match self.attempt_get(&url, target, &headers, attempt).await {
                AttemptOutcome::Done(value) => return Ok(value),
                AttemptOutcome::Fatal(message) => return Err(rest_error(message)),
                AttemptOutcome::Retry(message) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(rest_error(message));
                    }
                    let delay = RETRY_BACKOFF[(attempt - 1) as usize];
                    tracing::debug!(
                        method = "GET",
                        target = target,
                        attempt = attempt,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "perpl REST transient failure, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One `GET` attempt: send the request and classify the outcome for the
    /// retry loop. Structured logs carry `method`, `target`, `status` (when a
    /// response arrived) and `latency_ms`; header values are never logged.
    async fn attempt_get(
        &self,
        url: &str,
        target: &str,
        headers: &HeaderMap,
        attempt: u32,
    ) -> AttemptOutcome {
        let started = Instant::now();
        let response = self.http.get(url).headers(headers.clone()).send().await;

        let response = match response {
            Ok(response) => response,
            Err(err) => {
                let retryable = is_retryable(&err);
                tracing::debug!(
                    method = "GET",
                    target = target,
                    latency_ms = elapsed_ms(started),
                    attempt = attempt,
                    retryable = retryable,
                    error = %err,
                    "perpl REST transport error"
                );
                let message = format!(
                    "GET {target} transport error: {}",
                    error_excerpt(&err.to_string())
                );
                return classify(retryable, message);
            }
        };

        let status = response.status();
        tracing::debug!(
            method = "GET",
            target = target,
            status = status.as_u16(),
            latency_ms = elapsed_ms(started),
            attempt = attempt,
            "perpl REST response"
        );

        if !status.is_success() {
            let body = match response.text().await {
                Ok(body) => body,
                Err(err) => format!("<unreadable body: {}>", err),
            };
            let message = format!(
                "GET {target} failed: HTTP {}: {}",
                status.as_u16(),
                error_excerpt(&body)
            );
            // 5xx is retryable; every other non-success status (4xx, ...) is not.
            return classify(status.is_server_error(), message);
        }

        match response.bytes().await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => AttemptOutcome::Done(value),
                Err(err) => AttemptOutcome::Fatal(format!(
                    "GET {target} returned invalid JSON: {err}: {}",
                    error_excerpt(&String::from_utf8_lossy(&bytes))
                )),
            },
            Err(err) => {
                let retryable = is_retryable(&err);
                tracing::debug!(
                    method = "GET",
                    target = target,
                    status = status.as_u16(),
                    latency_ms = elapsed_ms(started),
                    attempt = attempt,
                    retryable = retryable,
                    error = %err,
                    "perpl REST body read error"
                );
                classify(
                    retryable,
                    format!(
                        "GET {target} body read failed: {}",
                        error_excerpt(&err.to_string())
                    ),
                )
            }
        }
    }
}

/// Result of one HTTP attempt, classified for the retry loop.
enum AttemptOutcome {
    /// Success: the response body parsed as JSON.
    Done(Value),
    /// Transient failure (5xx / timeout / connect) — eligible for retry.
    Retry(String),
    /// Permanent failure (4xx, malformed JSON, other transport errors).
    Fatal(String),
}

/// Turn a retryable flag plus message into an [`AttemptOutcome`].
fn classify(retryable: bool, message: String) -> AttemptOutcome {
    if retryable {
        AttemptOutcome::Retry(message)
    } else {
        AttemptOutcome::Fatal(message)
    }
}

/// Wrap a REST failure message in `PerplError::Rest` (the frozen home for
/// HTTP / JSON problems, SPEC §8).
fn rest_error(message: String) -> SentinelError {
    PerplError::Rest(message).into()
}

/// Retryable transport failures per the frozen contract: timeout or connect.
fn is_retryable(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

/// Milliseconds elapsed since `started` (saturating).
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Truncate free text to [`MAX_ERROR_EXCERPT_CHARS`] characters (char-boundary
/// safe). Response bodies may only reach logs and errors through this helper.
fn error_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_ERROR_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let mut excerpt: String = trimmed.chars().take(MAX_ERROR_EXCERPT_CHARS - 3).collect();
    excerpt.push_str("...");
    excerpt
}

/// Header name from the signer; failures map to `PerplError::Rest`.
fn header_name(name: &str) -> Result<HeaderName> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|err| rest_error(format!("invalid signed header name ({name}): {err}")))
}

/// Header value from the signer; failures map to `PerplError::Rest` without
/// echoing the value (P00 invariant #5).
fn header_value(value: &str, name: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value)
        .map_err(|_| rest_error(format!("invalid signed header value for {name}")))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Signer over a fixed testnet seed (`0x07` × 32) and token `test-token`.
    fn test_signer() -> ApiKeySigner {
        let seed = format!("0x{}", "07".repeat(32));
        ApiKeySigner::from_parts("test-token", &seed, 10143).expect("test signer builds")
    }

    /// Client bound to a wiremock server on localhost (never an external host).
    fn client_for(server: &MockServer) -> PerplRest {
        PerplRest::new(server.uri(), test_signer()).expect("REST client builds")
    }

    /// Requests recorded by the mock server, in arrival order.
    async fn received(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .expect("wiremock records matched requests")
    }

    #[tokio::test]
    async fn signed_request_carries_exact_path_query_and_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/trading/fills"))
            .and(query_param("count", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "d": [] })))
            .mount(&server)
            .await;

        let rest = client_for(&server);
        let value = rest.get_fills(100).await.expect("fills request succeeds");
        assert_eq!(value, serde_json::json!({ "d": [] }));

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1, "exactly one attempt on success");
        let request = &requests[0];
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(request.url.path(), "/v1/trading/fills");
        assert_eq!(request.url.query(), Some("count=100"));
        let api_key = request
            .headers
            .get("x-api-key")
            .expect("X-API-Key header present")
            .to_str()
            .expect("X-API-Key is ASCII");
        assert_eq!(api_key, "test-token");
        // The other three signed headers ride along (fresh values each call).
        for name in ["x-api-timestamp", "x-api-nonce", "x-api-signature"] {
            assert!(request.headers.contains_key(name), "{name} header present");
        }
    }

    #[tokio::test]
    async fn persistent_500_retries_three_attempts_then_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/pub/context"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
            .mount(&server)
            .await;

        let rest = client_for(&server);
        let started = Instant::now();
        let result = rest.get_context().await;
        let elapsed = started.elapsed();

        let err = result.expect_err("persistent 500 must fail after retries");
        match err {
            SentinelError::Perpl(PerplError::Rest(message)) => {
                assert!(
                    message.contains("500"),
                    "message mentions status: {message}"
                );
                assert!(
                    message.contains("/v1/pub/context"),
                    "message mentions target: {message}"
                );
            }
            other => panic!("expected PerplError::Rest, got {other:?}"),
        }

        let requests = received(&server).await;
        assert_eq!(requests.len(), 3, "3 attempts total (initial + 2 retries)");
        assert!(
            elapsed >= Duration::from_millis(1200),
            "backoff of 250 ms + 1 s must have run; elapsed = {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn unauthorized_401_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/trading/wallet"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&server)
            .await;

        let rest = client_for(&server);
        let err = rest
            .get_wallet()
            .await
            .expect_err("401 must fail without retrying");
        match err {
            SentinelError::Perpl(PerplError::Rest(message)) => {
                assert!(
                    message.contains("401"),
                    "message mentions status: {message}"
                );
            }
            other => panic!("expected PerplError::Rest, got {other:?}"),
        }
        assert_eq!(
            received(&server).await.len(),
            1,
            "4xx must never be retried (exactly one attempt)"
        );
    }

    #[tokio::test]
    async fn success_path_returns_parsed_json() {
        let server = MockServer::start().await;
        let all_payload = serde_json::json!({
            "mt": 9,
            "sn": 1,
            "d": { "32": { "mrk": 271370 } }
        });
        let one_payload = serde_json::json!({
            "mt": 9,
            "sn": 2,
            "d": { "mrk": 271370 }
        });
        Mock::given(method("GET"))
            .and(path("/v1/market-data/ticker"))
            .respond_with(ResponseTemplate::new(200).set_body_json(all_payload.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/market-data/ticker/32"))
            .respond_with(ResponseTemplate::new(200).set_body_json(one_payload.clone()))
            .mount(&server)
            .await;

        let rest = client_for(&server);
        let all = rest.get_ticker(None).await.expect("all-markets ticker");
        assert_eq!(all, all_payload);
        let one = rest
            .get_ticker(Some(32))
            .await
            .expect("single-market ticker");
        assert_eq!(one, one_payload);

        let requests = received(&server).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url.path(), "/v1/market-data/ticker");
        assert_eq!(requests[1].url.path(), "/v1/market-data/ticker/32");
        // Market-data endpoints are unsigned: no X-API-* headers are attached.
        for request in &requests {
            assert!(!request.headers.contains_key("x-api-key"));
        }
    }
}
