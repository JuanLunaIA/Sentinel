//! LLM providers — Qwen (primary), Kimi (fallback), and the test mock.
//!
//! Frozen by `SPEC-P07.md` §3. No `dyn` dispatch (async-fn-in-trait);
//! everything is generic over [`Provider`].
//!
//! Both HTTP providers share one retry policy: the initial request plus
//! **exactly one** retry, granted only for HTTP 5xx responses or transport
//! failures (timeout, connect, DNS, reset — anything that yields no HTTP
//! response). 4xx responses and malformed 2xx bodies are never retried. A
//! final transport failure is reported as [`BrainError::ProviderHttp`] with
//! `status: 0` because no HTTP status was ever received; a 2xx body whose
//! `choices[0].message.content` is missing or not a string is reported as
//! [`BrainError::InvalidJson`].
//!
//! Implemented by the P07 wave-1 agent; offline-testable via `wiremock`
//! (localhost only) and [`MockProvider`].

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};

use crate::config::{KimiConfig, QwenConfig};
use crate::error::{BrainError, Result};

/// Timeout for provider HTTP requests (`SPEC-P07.md` §3).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempts per call: the initial request plus exactly one retry.
const MAX_ATTEMPTS: u32 = 2;

/// Fallback sampling temperature when a configured [`Decimal`] has no `f64`
/// representation (`SPEC-P07.md` §3: default `0.1`).
///
/// [`Decimal`]: rust_decimal::Decimal
const DEFAULT_TEMPERATURE: f64 = 0.1;

/// One raw completion plus the provider-side metadata worth auditing.
#[derive(Debug, Clone, PartialEq)]
pub struct RawCompletion {
    /// Assistant message content.
    pub text: String,
    /// Provider name (`qwen`, `kimi`, `mock`).
    pub provider: String,
    /// Model id as sent in the request.
    pub model: String,
    /// Prompt tokens from `usage`, when reported.
    pub prompt_tokens: Option<u32>,
    /// Completion tokens from `usage`, when reported.
    pub completion_tokens: Option<u32>,
    /// Round-trip latency of the successful attempt, in milliseconds.
    pub latency_ms: u64,
}

/// A chat-completion backend.
#[allow(async_fn_in_trait)]
pub trait Provider {
    /// Complete a `(system, user)` pair.
    async fn complete(&self, system: &str, user: &str) -> Result<RawCompletion>;

    /// Stable provider name for logs and audit fields.
    fn name(&self) -> &'static str;
}

/// Qwen via Alibaba Cloud Model Studio (OpenAI-compatible chat completions).
#[derive(Debug)]
pub struct QwenProvider {
    /// API key (redacted `Debug` via `SecretString`).
    pub api_key: crate::config::SecretString,
    /// Base URL (intl or CN; confirmed with the real key).
    pub base_url: String,
    /// Model id (`qwen3.8-max`).
    pub model: String,
    /// Output budget (must cover the thinking trace).
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: rust_decimal::Decimal,
    /// Whether to send `response_format: {"type":"json_object"}`.
    pub json_mode: bool,
    /// HTTP client (60 s timeout by default).
    pub client: reqwest::Client,
}

impl QwenProvider {
    /// Build from configuration (60 s timeout client).
    ///
    /// `json_mode` defaults to `true`; the first live keyed run disables it
    /// with [`Self::with_json_mode`] if the endpoint rejects
    /// `response_format` (`SPEC-P07.md` §3).
    pub fn new(cfg: &QwenConfig) -> Self {
        Self {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            json_mode: true,
            client: build_client(),
        }
    }

    /// Replace the HTTP client (tests: short timeouts).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Toggle `response_format` (`json_mode`).
    pub fn with_json_mode(mut self, enabled: bool) -> Self {
        self.json_mode = enabled;
        self
    }
}

impl Provider for QwenProvider {
    async fn complete(&self, system: &str, user: &str) -> Result<RawCompletion> {
        let temperature = match self.temperature.to_f64() {
            Some(value) => value,
            None => DEFAULT_TEMPERATURE,
        };
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": self.max_tokens,
            "temperature": temperature,
        });
        if self.json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        chat_completion(
            self.name(),
            &self.client,
            &self.base_url,
            self.api_key.expose(),
            &self.model,
            &body,
        )
        .await
    }

    fn name(&self) -> &'static str {
        "qwen"
    }
}

/// Kimi (Moonshot) — same OpenAI-compatible shape; pulled forward for the
/// P07 FALLBACK (chain logic stays P08).
#[derive(Debug)]
pub struct KimiProvider {
    /// API key.
    pub api_key: crate::config::SecretString,
    /// Base URL (intl or CN).
    pub base_url: String,
    /// Model id (`kimi-k3`-family; confirmed at first live call).
    pub model: String,
    /// HTTP client (60 s timeout by default).
    pub client: reqwest::Client,
}

impl KimiProvider {
    /// Output budget; `KimiConfig` carries no `max_tokens` (`SPEC-P07.md` §3).
    const MAX_TOKENS: u32 = 4000;

    /// Sampling temperature; `KimiConfig` carries no temperature.
    const TEMPERATURE: f64 = 0.1;

    /// Build from configuration (60 s timeout client).
    ///
    /// Kimi never sends `response_format` (`json_mode` is effectively `false`
    /// and is not part of this profile).
    pub fn new(cfg: &KimiConfig) -> Self {
        Self {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            client: build_client(),
        }
    }

    /// Replace the HTTP client (tests).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }
}

impl Provider for KimiProvider {
    async fn complete(&self, system: &str, user: &str) -> Result<RawCompletion> {
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": Self::MAX_TOKENS,
            "temperature": Self::TEMPERATURE,
        });
        chat_completion(
            self.name(),
            &self.client,
            &self.base_url,
            self.api_key.expose(),
            &self.model,
            &body,
        )
        .await
    }

    fn name(&self) -> &'static str {
        "kimi"
    }
}

/// Deterministic provider for tests and the offline eval harness.
#[derive(Debug, Default)]
pub struct MockProvider {
    /// Queued responses (popped front-to-back).
    pub responses: Mutex<VecDeque<std::result::Result<RawCompletion, String>>>,
    /// Every `(system, user)` pair seen, in call order.
    pub calls: Mutex<Vec<(String, String)>>,
}

impl MockProvider {
    /// All-`Ok` canned texts.
    ///
    /// Queued completions carry `provider`/`model` `"mock"`, no usage metadata
    /// and zero latency (nothing was measured).
    pub fn canned(texts: Vec<String>) -> Self {
        let responses = texts
            .into_iter()
            .map(|text| {
                Ok(RawCompletion {
                    text,
                    provider: "mock".to_string(),
                    model: "mock".to_string(),
                    prompt_tokens: None,
                    completion_tokens: None,
                    latency_ms: 0,
                })
            })
            .collect();
        Self {
            responses: Mutex::new(responses),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Call log (`(system, user)` pairs).
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Provider for MockProvider {
    async fn complete(&self, system: &str, user: &str) -> Result<RawCompletion> {
        lock(&self.calls).push((system.to_string(), user.to_string()));
        match lock(&self.responses).pop_front() {
            Some(Ok(completion)) => Ok(completion),
            Some(Err(last)) => Err(BrainError::AllProvidersFailed { last }.into()),
            None => Err(mock_exhausted().into()),
        }
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Convenience: the error an exhausted mock queue produces.
fn mock_exhausted() -> BrainError {
    BrainError::AllProvidersFailed {
        last: "mock queue exhausted".to_string(),
    }
}

/// Build the default HTTP client (60 s timeout).
///
/// The frozen `new` signatures return `Self`, not a `Result`, so a client
/// build failure (possible only when the TLS backend cannot initialise) falls
/// back to the default client with a warning instead of panicking here; any
/// resulting per-call transport failure is reported by [`ProviderHttp`].
///
/// [`ProviderHttp`]: BrainError::ProviderHttp
fn build_client() -> reqwest::Client {
    match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to build the provider HTTP client with the 60 s timeout; \
                 using the default client"
            );
            reqwest::Client::new()
        }
    }
}

/// One `POST {base}/chat/completions` call under the frozen retry policy.
///
/// Exactly two attempts total: the initial request plus one retry, granted
/// only for HTTP 5xx responses or transport failures (timeout, connect, DNS,
/// reset). 4xx responses and malformed 2xx bodies fail immediately. When both
/// attempts fail at the transport layer there is no HTTP status to report, so
/// the final error is [`BrainError::ProviderHttp`] with `status: 0`.
async fn chat_completion(
    provider: &'static str,
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    body: &Value,
) -> Result<RawCompletion> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut last_status = 0u16;
    for attempt in 1..=MAX_ATTEMPTS {
        let started = Instant::now();
        let outcome = attempt_chat(provider, client, &url, api_key, body).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let retried = attempt > 1;
        match outcome {
            Attempt::Completion {
                status,
                text,
                prompt_tokens,
                completion_tokens,
            } => {
                tracing::info!(
                    provider = provider,
                    model = %model,
                    latency_ms = latency_ms,
                    attempt = attempt,
                    status = status,
                    retried = retried,
                    prompt_tokens = ?prompt_tokens,
                    completion_tokens = ?completion_tokens,
                    "provider call succeeded"
                );
                return Ok(RawCompletion {
                    text,
                    provider: provider.to_string(),
                    model: model.to_string(),
                    prompt_tokens,
                    completion_tokens,
                    latency_ms,
                });
            }
            Attempt::Retryable { status } => {
                last_status = status;
                tracing::info!(
                    provider = provider,
                    model = %model,
                    latency_ms = latency_ms,
                    attempt = attempt,
                    status = status,
                    retried = retried,
                    "provider attempt failed; retrying once"
                );
            }
            Attempt::Terminal { status, error } => {
                tracing::info!(
                    provider = provider,
                    model = %model,
                    latency_ms = latency_ms,
                    attempt = attempt,
                    status = status,
                    retried = retried,
                    "provider attempt failed; not retryable"
                );
                return Err(error.into());
            }
        }
    }
    Err(BrainError::ProviderHttp {
        provider: provider.to_string(),
        status: last_status,
    }
    .into())
}

/// The outcome of a single HTTP attempt, before the retry policy is applied.
enum Attempt {
    /// HTTP 2xx with a parseable assistant message.
    Completion {
        /// HTTP status (2xx).
        status: u16,
        /// Assistant content.
        text: String,
        /// Prompt tokens when reported.
        prompt_tokens: Option<u32>,
        /// Completion tokens when reported.
        completion_tokens: Option<u32>,
    },
    /// Retryable failure: HTTP 5xx (`status` = the code) or transport
    /// failure (`status` = 0).
    Retryable {
        /// HTTP status, or `0` when no response was received.
        status: u16,
    },
    /// Terminal failure: HTTP 4xx or a malformed 2xx body.
    Terminal {
        /// HTTP status, or `0` when no response was received.
        status: u16,
        /// Error to surface to the caller.
        error: BrainError,
    },
}

/// Perform one HTTP request/response round-trip.
async fn attempt_chat(
    provider: &'static str,
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
) -> Attempt {
    let response = match client
        .post(url)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
    {
        Ok(response) => response,
        // No HTTP response at all: transport failure (timeout, connect, ...).
        Err(_transport) => return Attempt::Retryable { status: 0 },
    };

    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        return if status.is_server_error() {
            Attempt::Retryable { status: code }
        } else {
            Attempt::Terminal {
                status: code,
                error: BrainError::ProviderHttp {
                    provider: provider.to_string(),
                    status: code,
                },
            }
        };
    }

    let payload: Value = match response.json().await {
        Ok(payload) => payload,
        Err(_malformed) => {
            return Attempt::Terminal {
                status: status.as_u16(),
                error: BrainError::InvalidJson {
                    provider: provider.to_string(),
                },
            };
        }
    };

    match parse_completion(&payload) {
        Some((text, prompt_tokens, completion_tokens)) => Attempt::Completion {
            status: status.as_u16(),
            text,
            prompt_tokens,
            completion_tokens,
        },
        None => Attempt::Terminal {
            status: status.as_u16(),
            error: BrainError::InvalidJson {
                provider: provider.to_string(),
            },
        },
    }
}

/// Extract `(content, prompt_tokens, completion_tokens)` from a chat body.
///
/// Returns `None` when `choices[0].message.content` is missing or not a
/// string; usage token counts are optional and saturate at [`u32::MAX`].
fn parse_completion(payload: &Value) -> Option<(String, Option<u32>, Option<u32>)> {
    let content = payload
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()?;
    Some((
        content.to_string(),
        usage_tokens(payload, "prompt_tokens"),
        usage_tokens(payload, "completion_tokens"),
    ))
}

/// Optional `usage.<field>` token count, saturating into `u32`.
fn usage_tokens(payload: &Value, field: &str) -> Option<u32> {
    payload
        .get("usage")?
        .get(field)?
        .as_u64()
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
}

/// Lock a mock mutex, recovering the guard when a previous holder panicked.
///
/// A poisoned test double should still answer deterministically instead of
/// masking the original panic behind a lock error.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rust_decimal::Decimal;
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::SecretString;
    use crate::error::SentinelError;

    /// The key every test provider is configured with; the mock server asserts
    /// the matching `Authorization: Bearer <key>` header on the success paths.
    const TEST_KEY: &str = "test-key";

    fn qwen_cfg(base_url: String) -> QwenConfig {
        QwenConfig {
            api_key: SecretString::new(TEST_KEY),
            base_url,
            model: "qwen3.8-max".to_string(),
            max_tokens: 4000,
            temperature: Decimal::new(1, 1),
        }
    }

    fn kimi_cfg(base_url: String) -> KimiConfig {
        KimiConfig {
            api_key: SecretString::new(TEST_KEY),
            base_url,
            model: "kimi-k3".to_string(),
        }
    }

    /// An OpenAI-compatible completion body with usage metadata.
    fn completion_body(content: &str, prompt_tokens: u64, completion_tokens: u64) -> Value {
        json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            },
        })
    }

    /// Requests recorded by the mock server, in arrival order.
    async fn received(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .expect("wiremock records requests")
    }

    /// Parse the body of a recorded request.
    fn request_body(request: &wiremock::Request) -> Value {
        serde_json::from_slice(&request.body).expect("recorded request body is JSON")
    }

    #[tokio::test]
    async fn qwen_success_sends_frozen_request_and_parses_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion_body(
                r#"{"action":"HOLD","market_id":32}"#,
                12,
                34,
            )))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let completion = provider
            .complete("SYS PROMPT", "USER PROMPT")
            .await
            .expect("qwen call succeeds");

        assert_eq!(completion.text, r#"{"action":"HOLD","market_id":32}"#);
        assert_eq!(completion.provider, "qwen");
        assert_eq!(completion.model, "qwen3.8-max");
        assert_eq!(completion.prompt_tokens, Some(12));
        assert_eq!(completion.completion_tokens, Some(34));

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1, "a 2xx call is not retried");
        let request = &requests[0];
        assert_eq!(request.url.path(), "/chat/completions");
        let authorization = request
            .headers
            .get("authorization")
            .expect("authorization header is present")
            .to_str()
            .expect("authorization header is ASCII");
        assert_eq!(authorization, "Bearer test-key");

        let body = request_body(request);
        assert_eq!(body["model"], "qwen3.8-max");
        assert_eq!(body["max_tokens"], 4000);
        assert!(
            body["temperature"].is_number(),
            "temperature must serialize as a JSON number: {}",
            body["temperature"]
        );
        assert_eq!(body["temperature"].as_f64(), Some(0.1));
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "SYS PROMPT");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "USER PROMPT");
    }

    #[tokio::test]
    async fn qwen_json_mode_can_be_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion_body(
                r#"{"action":"HOLD"}"#,
                1,
                1,
            )))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri())).with_json_mode(false);
        provider
            .complete("s", "u")
            .await
            .expect("json_mode=false call succeeds");

        let requests = received(&server).await;
        let body = request_body(&requests[0]);
        assert!(
            body.get("response_format").is_none(),
            "json_mode=false must omit response_format"
        );
    }

    #[tokio::test]
    async fn kimi_success_omits_response_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion_body("HOLD", 5, 7)))
            .mount(&server)
            .await;

        let provider = KimiProvider::new(&kimi_cfg(server.uri()));
        let completion = provider
            .complete("SYS", "USER")
            .await
            .expect("kimi call succeeds");

        assert_eq!(completion.text, "HOLD");
        assert_eq!(completion.provider, "kimi");
        assert_eq!(completion.model, "kimi-k3");
        assert_eq!(completion.prompt_tokens, Some(5));
        assert_eq!(completion.completion_tokens, Some(7));

        let requests = received(&server).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/chat/completions");
        let body = request_body(&requests[0]);
        assert_eq!(body["model"], "kimi-k3");
        assert_eq!(body["max_tokens"], 4000);
        assert!(
            body["temperature"].is_number(),
            "temperature must be a JSON number"
        );
        assert_eq!(body["temperature"].as_f64(), Some(0.1));
        assert!(
            body.get("response_format").is_none(),
            "kimi must not send response_format"
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[tokio::test]
    async fn retries_once_on_5xx_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion_body("ok", 3, 4)))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let completion = provider
            .complete("s", "u")
            .await
            .expect("the retry succeeds");

        assert_eq!(completion.text, "ok");
        assert_eq!(completion.prompt_tokens, Some(3));
        assert_eq!(completion.completion_tokens, Some(4));

        let requests = received(&server).await;
        assert_eq!(requests.len(), 2, "exactly one retry (two attempts)");
    }

    #[tokio::test]
    async fn two_5xx_responses_fail_after_exactly_two_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;

        let provider = KimiProvider::new(&kimi_cfg(server.uri()));
        let error = provider
            .complete("s", "u")
            .await
            .expect_err("both attempts fail");

        match error {
            SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
                assert_eq!(provider, "kimi");
                assert_eq!(status, 503);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let requests = received(&server).await;
        assert_eq!(requests.len(), 2, "exactly one retry (two attempts)");
    }

    #[tokio::test]
    async fn client_error_is_never_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"bad request"}"#))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let error = provider
            .complete("s", "u")
            .await
            .expect_err("a 400 fails immediately");

        match error {
            SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
                assert_eq!(provider, "qwen");
                assert_eq!(status, 400);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        assert_eq!(received(&server).await.len(), 1, "4xx is never retried");
    }

    #[tokio::test]
    async fn malformed_choices_are_invalid_json_and_not_retried() {
        let cases = vec![
            json!({"usage": {"prompt_tokens": 1}}),
            json!({"choices": []}),
            json!({"choices": [{"message": {"content": 42}}]}),
        ];

        for payload in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(payload.clone()))
                .mount(&server)
                .await;

            let provider = QwenProvider::new(&qwen_cfg(server.uri()));
            let error = provider
                .complete("s", "u")
                .await
                .expect_err("a malformed body fails");

            match error {
                SentinelError::Brain(BrainError::InvalidJson { provider }) => {
                    assert_eq!(provider, "qwen");
                }
                other => panic!("unexpected error for {payload}: {other:?}"),
            }
            assert_eq!(
                received(&server).await.len(),
                1,
                "a malformed body is not retried: {payload}"
            );
        }
    }

    #[tokio::test]
    async fn non_json_body_is_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let error = provider
            .complete("s", "u")
            .await
            .expect_err("body fails to parse");

        match error {
            SentinelError::Brain(BrainError::InvalidJson { provider }) => {
                assert_eq!(provider, "qwen")
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(received(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn usage_metadata_is_optional() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"choices": [{"message": {"content": "x"}}]})),
            )
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let completion = provider.complete("s", "u").await.expect("call succeeds");

        assert_eq!(completion.text, "x");
        assert_eq!(completion.prompt_tokens, None);
        assert_eq!(completion.completion_tokens, None);
    }

    #[tokio::test]
    async fn usage_tokens_saturate_to_u32_max() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "x"}}],
                "usage": {
                    "prompt_tokens": 5_000_000_000_u64,
                    "completion_tokens": 4_294_967_296_u64,
                },
            })))
            .mount(&server)
            .await;

        let provider = QwenProvider::new(&qwen_cfg(server.uri()));
        let completion = provider.complete("s", "u").await.expect("call succeeds");

        assert_eq!(completion.prompt_tokens, Some(u32::MAX));
        assert_eq!(completion.completion_tokens, Some(u32::MAX));
    }

    #[tokio::test]
    async fn short_client_timeout_is_retried_once_then_reported_as_status_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(completion_body("late", 1, 1)),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("test client builds");
        let provider = QwenProvider::new(&qwen_cfg(server.uri())).with_client(client);

        let error = provider
            .complete("s", "u")
            .await
            .expect_err("both attempts time out");

        match error {
            SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
                assert_eq!(provider, "qwen");
                assert_eq!(status, 0, "transport failures carry HTTP status 0");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let requests = received(&server).await;
        assert_eq!(requests.len(), 2, "the timeout is retried exactly once");
    }

    #[tokio::test]
    async fn mock_provider_preserves_canned_order_and_logs_calls() {
        let mock = MockProvider::canned(vec!["first".to_string(), "second".to_string()]);

        let first = mock.complete("sys-1", "user-1").await.expect("first pops");
        let second = mock.complete("sys-2", "user-2").await.expect("second pops");

        assert_eq!(first.text, "first");
        assert_eq!(second.text, "second");
        assert_eq!(first.provider, "mock");
        assert_eq!(first.model, "mock");
        assert_eq!(
            mock.calls(),
            vec![
                ("sys-1".to_string(), "user-1".to_string()),
                ("sys-2".to_string(), "user-2".to_string()),
            ],
            "calls are logged in order"
        );
    }

    #[tokio::test]
    async fn mock_provider_exhausted_queue_reports_last_error() {
        let mock = MockProvider::default();

        let error = mock
            .complete("s", "u")
            .await
            .expect_err("empty queue fails");

        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(last, "mock queue exhausted");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(mock.calls().len(), 1, "the failed call is still logged");
    }

    #[tokio::test]
    async fn mock_provider_queued_error_preserves_text_and_order() {
        let mut responses = VecDeque::new();
        responses.push_back(Err("upstream exploded".to_string()));
        responses.push_back(Ok(RawCompletion {
            text: "after".to_string(),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
        }));
        let mock = MockProvider {
            responses: Mutex::new(responses),
            calls: Mutex::new(Vec::new()),
        };

        let error = mock
            .complete("sys-a", "user-a")
            .await
            .expect_err("queued error surfaces");
        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(last, "upstream exploded");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let completion = mock
            .complete("sys-b", "user-b")
            .await
            .expect("the queue continues after the error");
        assert_eq!(completion.text, "after");
        assert_eq!(mock.calls().len(), 2);
    }
}
