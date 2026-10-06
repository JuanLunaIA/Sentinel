//! LLM providers — Qwen (primary), Kimi (fallback), and the test mock.
//!
//! Frozen by `SPEC-P07.md` §3. No `dyn` dispatch (async-fn-in-trait);
//! everything is generic over [`Provider`].
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by wave 1.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::config::{KimiConfig, QwenConfig};
use crate::error::{BrainError, Result};

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
    /// Round-trip latency in milliseconds.
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
    pub fn new(cfg: &QwenConfig) -> Self {
        let _ = cfg;
        todo!("P07 agent providers")
    }

    /// Replace the HTTP client (tests: short timeouts).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        let _ = &client;
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
    async fn complete(&self, _system: &str, _user: &str) -> Result<RawCompletion> {
        todo!("P07 agent providers: bearer POST /chat/completions, one 5xx/timeout retry")
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
    /// Build from configuration.
    pub fn new(cfg: &KimiConfig) -> Self {
        let _ = cfg;
        todo!("P07 agent providers")
    }

    /// Replace the HTTP client (tests).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        let _ = &client;
        self.client = client;
        self
    }
}

impl Provider for KimiProvider {
    async fn complete(&self, _system: &str, _user: &str) -> Result<RawCompletion> {
        todo!("P07 agent providers: no response_format by default")
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
    pub fn canned(texts: Vec<String>) -> Self {
        let _ = texts;
        todo!("P07 agent providers")
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
    async fn complete(&self, _system: &str, _user: &str) -> Result<RawCompletion> {
        todo!("P07 agent providers: pop the queue; empty ⇒ AllProvidersFailed")
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Convenience: the error an exhausted mock queue produces.
#[allow(dead_code)]
fn mock_exhausted() -> BrainError {
    BrainError::AllProvidersFailed {
        last: "mock queue exhausted".to_string(),
    }
}
