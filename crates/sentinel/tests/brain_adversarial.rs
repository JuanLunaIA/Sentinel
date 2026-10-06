//! P07 adversarial verification — independent black-box corpus (SPEC-P07.md
//! §3/§4/§6/§7).
//!
//! Written from `SPEC-P07.md` + the frozen public API only: the sibling
//! implementations are treated as black boxes. Beyond the writers' own suites:
//!
//! - **parser**: `</THINKING>`/`</think >`/mixed-case closers, two closing
//!   tags (last wins — the slice is asserted directly), fullwidth `｛｝` ⇒
//!   `NoObject`, `\\"` escape sequences, `{` as the first char of a string
//!   value, 10-deep nesting inside an unknown field, `market_id` as a JSON
//!   string ⇒ `Json`, `confidence` as a JSON string (rust_decimal tolerates
//!   numeric strings — asserted `Ok` below), two objects where the first is
//!   schema-invalid ⇒ `Validation` (documents "first wins"), unicode reasons,
//!   truncated JSON ⇒ `NoObject`.
//! - **providers** (wiremock, localhost only): exact request bodies (no
//!   `response_format` for Kimi or `with_json_mode(false)` Qwen), 500×2 ⇒
//!   `ProviderHttp{status: 500}` after exactly 2 requests, 418 ⇒ exactly 1
//!   request, malformed 200 ⇒ `InvalidJson` after exactly 1 request, a 150 ms
//!   client timeout against a delayed mock ⇒ `ProviderHttp{status: 0}` after
//!   2 requests, plus `MockProvider` order/exhaustion semantics.
//! - **engine**: rate-limit boundary re-derived (2 s: ok at `t`,
//!   `remaining_ms == 1` at `t + 1999`, ok at `t + 2000`; per-market
//!   isolation), floor `==` kept vs just-below downgraded to `ESCALATE`, the
//!   repair flow observed through `MockProvider::calls()` (exactly 2 calls,
//!   second user carries the nudge), repair-still-invalid ⇒ `InvalidJson`.
//! - **eval**: `grounding_check` absent-decimal guard (`24.10` vs `24.1`),
//!   `score()` injection exclusion and grounding totals over parsed
//!   decisions, and a real `brain_eval --mock` run against `tests/golden`
//!   asserting the frozen scoreboard lines (12/12 core, 14/14 schema, 2/2
//!   injection) with `--out` byte-equal to stdout.

use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel::brain::engine::{ConsultInput, StrategyEngine};
use sentinel::brain::eval::{
    Expected, Scenario, ScenarioResult, Scoreboard, grounding_check, score,
};
use sentinel::brain::parser::{
    ParseError, SCHEMA_DOC, extract_balanced_object, parse_decision, repair_nudge, strip_thinking,
};
use sentinel::brain::prompts::{PROMPT_VERSION, PolicySummary, ReflexSummary, SmartMoneyContext};
use sentinel::brain::providers::{
    KimiProvider, MockProvider, Provider, QwenProvider, RawCompletion,
};
use sentinel::config::{KimiConfig, QwenConfig, SecretString};
use sentinel::error::{BrainError, SentinelError};
use sentinel_core::types::{AccountState, DecisionAction, Market, MarketId, Position, Urgency};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The focus market every fixture below consults.
const FOCUS: u32 = 32;
/// A second, independent market (per-market rate-limit checks).
const OTHER: u32 = 20;

/// Decimal literal shorthand (tests only).
fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal literal")
}

/// A compliant HOLD decision on `market_id`; `confidence` is a JSON number.
fn hold_json(market_id: u32, confidence: &str) -> String {
    format!(
        r#"{{"action":"HOLD","market_id":{market_id},"amount":null,"confidence":{confidence},"urgency":"ROUTINE","reason":"grounded in the snapshot"}}"#
    )
}

/// Market fixture (ETH-like; P04 maintenance margin 0.05).
fn market(id: u32, symbol: &str) -> Market {
    Market {
        id: MarketId(id),
        symbol: symbol.to_string(),
        base: symbol.to_string(),
        price_decimals: 2,
        size_decimals: 3,
        initial_margin_fraction: dec("0.0833"),
        maintenance_margin_fraction: dec("0.05"),
        max_leverage: dec("12"),
        min_size: dec("0.001"),
        tick_size: dec("0.01"),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

/// Static position fixture (long ETH, comfortable headroom to 1300).
fn position(market_id: u32, symbol: &str) -> Position {
    Position {
        market_id: MarketId(market_id),
        symbol: symbol.to_string(),
        size: dec("5"),
        entry_price: dec("2000"),
        mark_price: Some(dec("1900")),
        liq_price: Some(dec("1300")),
        collateral: dec("4000"),
        unrealized_pnl: dec("-500"),
        margin_ratio: Some(dec("0.2")),
        leverage: dec("5"),
        opened_at: None,
    }
}

/// Two-position account snapshot (focus + other).
fn account() -> AccountState {
    AccountState {
        positions: vec![position(FOCUS, "ETH"), position(OTHER, "BTC")],
        free_balance: dec("1000"),
        equity: dec("4500"),
        fee_tier: 0,
        snapshot_ts: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000)
            .expect("valid test timestamp"),
    }
}

/// Policy facts used across engine/eval fixtures.
fn policy() -> PolicySummary {
    PolicySummary {
        market_allowlist: vec![MarketId(FOCUS), MarketId(OTHER)],
        max_order_size_usd: dec("5000"),
        require_approval_above_usd: dec("2500"),
        daily_actions_left: 5,
    }
}

/// Consult input focused on `focus`.
fn consult_input(focus: u32) -> ConsultInput {
    ConsultInput {
        account: account(),
        markets: vec![market(FOCUS, "ETH"), market(OTHER, "BTC")],
        focus_market: MarketId(focus),
        policy: policy(),
        sm: SmartMoneyContext::unavailable("ETH"),
        reflex: ReflexSummary::default(),
    }
}

// ===========================================================================
// Parser (SPEC-P07 §4) — adversarial extras beyond the writer corpus.
// ===========================================================================

#[test]
fn strip_thinking_recognizes_uppercase_spaced_and_mixed_case_closers() {
    let allowed = [MarketId(FOCUS)];
    let json = hold_json(FOCUS, "0.72");

    // `</THINKING>` — uppercase variant.
    let upper = format!("Let me reason about this.</THINKING>{json}");
    let decision = parse_decision(&upper, &allowed).expect("uppercase closer is stripped");
    assert_eq!(decision.action, DecisionAction::Hold);

    // `</think >` — whitespace suffix before `>` ("any suffix up to >").
    let spaced = format!("checking tiers.</think >{json}");
    let decision = parse_decision(&spaced, &allowed).expect("spaced closer is stripped");
    assert_eq!(decision.action, DecisionAction::Hold);

    // Mixed case is the same class.
    let mixed = format!("step 1</ThiNkIng>{json}");
    let decision = parse_decision(&mixed, &allowed).expect("mixed-case closer is stripped");
    assert_eq!(decision.action, DecisionAction::Hold);
}

#[test]
fn strip_thinking_last_closing_tag_wins() {
    // The slice handed to the extractor follows the LAST closer.
    let raw = "prologue</thinking>middle</THINKING>epilogue";
    assert_eq!(strip_thinking(raw), "epilogue");

    // Two closers, JSON after the last one — parses fine.
    let allowed = [MarketId(FOCUS)];
    let json = hold_json(FOCUS, "0.72");
    let two = format!("first</think>skipped</thinking>{json}");
    let decision = parse_decision(&two, &allowed).expect("json after the last closer parses");
    assert_eq!(decision.confidence, dec("0.72"));

    // No closer at all: the text passes through untouched.
    assert_eq!(strip_thinking("no closer here"), "no closer here");
}

#[test]
fn fullwidth_braces_are_not_an_object() {
    let raw = "decision: ｛\"action\":\"HOLD\"｝ — no ascii braces anywhere";
    assert!(extract_balanced_object(raw).is_none());
    let error = parse_decision(raw, &[MarketId(FOCUS)]).expect_err("fullwidth braces do not count");
    assert_eq!(error, ParseError::NoObject);
}

#[test]
fn escaped_backslash_quote_sequences_balance() {
    let allowed = [MarketId(FOCUS)];

    // `\\"` before the closing brace: an escaped backslash, then the string's
    // real closing quote (even backslash run). A parity-blind scanner would
    // treat the quote as escaped and never close the string.
    let raw = r#"prose {"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"ends with backslash\\"} trailing"#;
    let decision = parse_decision(raw, &allowed).expect("backslash-run parity handled");
    assert_eq!(decision.reason, "ends with backslash\\");

    // `\"` at the end of the value: an escaped quote that must NOT close it.
    let raw = r#"prose {"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"ends with quote\""} trailing"#;
    let decision = parse_decision(raw, &allowed).expect("escaped quote must not close the string");
    assert_eq!(decision.reason, "ends with quote\"");
}

#[test]
fn open_brace_as_first_char_of_a_string_value() {
    let raw = r#"{"reason":"{literal} and {nested} braces","action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE"}"#;
    let decision =
        parse_decision(raw, &[MarketId(FOCUS)]).expect("braces inside strings are literal");
    assert_eq!(decision.reason, "{literal} and {nested} braces");
    assert_eq!(extract_balanced_object(raw), Some(raw));
}

#[test]
fn ten_deep_unknown_nesting_balances() {
    // `"extra": {"n": {"n": … 1 …}}` — 10 levels inside an ignored field.
    let mut deep = String::from("1");
    for _ in 0..10 {
        deep = format!("{{\"n\":{deep}}}");
    }
    let raw = format!(
        r#"{{"extra":{deep},"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"nested"}}"#
    );
    let decision =
        parse_decision(&raw, &[MarketId(FOCUS)]).expect("deep unknown nesting is ignored");
    assert_eq!(decision.action, DecisionAction::Hold);
    assert_eq!(extract_balanced_object(&raw), Some(raw.as_str()));

    // Arrays inside an unknown field are ignored too.
    let raw2 = r#"{"history":[1,2,{"k":"v"},[3,[4]]],"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"arrays"}"#;
    let decision = parse_decision(raw2, &[MarketId(FOCUS)]).expect("nested arrays are ignored");
    assert_eq!(decision.reason, "arrays");
    assert_eq!(extract_balanced_object(raw2), Some(raw2));
}

#[test]
fn market_id_as_json_string_is_a_json_error() {
    let raw = r#"{"action":"HOLD","market_id":"32","amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"string id"}"#;
    let error = parse_decision(raw, &[MarketId(FOCUS)]).expect_err("market_id must be a number");
    match error {
        ParseError::Json { detail } => assert!(!detail.is_empty()),
        other => panic!("expected ParseError::Json, got {other:?}"),
    }

    // Trailing comma: serde_json rejects it (Json error, not NoObject).
    let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"x",}"#;
    let error = parse_decision(raw, &[MarketId(FOCUS)]).expect_err("trailing commas are rejected");
    assert!(matches!(error, ParseError::Json { .. }), "got {error:?}");
}

#[test]
fn confidence_as_json_string_is_tolerated_by_rust_decimal() {
    // PROBED behavior (rust_decimal's `serde-with-str` build keeps the
    // `deserialize_any` visitor, which accepts numeric strings): a JSON-string
    // confidence parses and validates. Pinned here so drift either way fails
    // loudly.
    let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":"0.5","urgency":"ROUTINE","reason":"string confidence"}"#;
    let decision =
        parse_decision(raw, &[MarketId(FOCUS)]).expect("rust_decimal accepts a numeric string");
    assert_eq!(decision.confidence, dec("0.5"));
}

#[test]
fn two_objects_first_wins() {
    let allowed = [MarketId(FOCUS)];

    // First object is schema-invalid (REDUCE without an amount): the parser
    // must return the FIRST balanced object and fail validation on it — the
    // later, valid object must never be considered.
    let first = r#"{"action":"REDUCE","market_id":32,"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"trim"}"#;
    let second = hold_json(FOCUS, "0.72");
    let raw = format!("{first} {second}");
    assert_eq!(extract_balanced_object(&raw), Some(first));

    let error = parse_decision(&raw, &allowed).expect_err("the first object wins and is invalid");
    match error {
        ParseError::Validation { detail } => assert!(detail.contains("amount"), "detail: {detail}"),
        other => panic!("expected ParseError::Validation, got {other:?}"),
    }

    // Mirror: a VALID first object wins over a later broken one.
    let valid = hold_json(FOCUS, "0.9");
    let raw2 = format!("{valid} {{\"broken\":");
    let decision = parse_decision(&raw2, &allowed).expect("first balanced object wins");
    assert_eq!(decision.confidence, dec("0.9"));
}

#[test]
fn truncated_json_yields_no_object() {
    let raw = "prefix {\"action\":\"HOLD\",\"nested\":{\"a\":{\"b\":{\"c\":1";
    let error = parse_decision(raw, &[MarketId(FOCUS)]).expect_err("unbalanced object");
    assert_eq!(error, ParseError::NoObject);
}

#[test]
fn unicode_reason_roundtrips() {
    let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"café — 分析 ✓ ½"}"#;
    let decision = parse_decision(raw, &[MarketId(FOCUS)]).expect("multibyte text parses");
    assert_eq!(decision.reason, "café — 分析 ✓ ½");
}

#[test]
fn repair_nudge_quotes_schema_and_previous_output() {
    let nudge = repair_nudge("GARBAGE-OUTPUT");
    assert!(nudge.contains("Return ONLY the JSON object matching this schema:"));
    assert!(nudge.contains(SCHEMA_DOC));
    assert!(nudge.contains("GARBAGE-OUTPUT"));
    assert!(nudge.contains("Previous output was:"));
}

// ===========================================================================
// Providers (SPEC-P07 §3) — wiremock on localhost only.
// ===========================================================================

/// Qwen config pointed at a mock server.
fn qwen_cfg(base_url: &str) -> QwenConfig {
    QwenConfig {
        api_key: SecretString::new("qwen-test-key"),
        base_url: base_url.to_string(),
        model: "qwen-max-test".to_string(),
        max_tokens: 2048,
        temperature: dec("0.35"),
    }
}

/// Kimi config pointed at a mock server.
fn kimi_cfg(base_url: &str) -> KimiConfig {
    KimiConfig {
        api_key: SecretString::new("kimi-test-key"),
        base_url: base_url.to_string(),
        model: "kimi-k3-adversarial".to_string(),
    }
}

/// Mount a chat-completions mock answering every POST with `response`.
async fn mount_chat(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn qwen_request_shape_includes_response_format_by_default() {
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "{\"ok\":true}"}}],
        })),
    )
    .await;

    let provider = QwenProvider::new(&qwen_cfg(&server.uri()));
    let completion = provider
        .complete("SYS-PROMPT", "USER-PROMPT")
        .await
        .expect("mock returns 200");
    assert_eq!(completion.text, "{\"ok\":true}");
    assert_eq!(completion.provider, "qwen");
    assert_eq!(completion.model, "qwen-max-test");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().expect("json body");
    assert_eq!(body["model"].as_str(), Some("qwen-max-test"));
    assert_eq!(body["messages"][0]["role"].as_str(), Some("system"));
    assert_eq!(body["messages"][0]["content"].as_str(), Some("SYS-PROMPT"));
    assert_eq!(body["messages"][1]["role"].as_str(), Some("user"));
    assert_eq!(body["messages"][1]["content"].as_str(), Some("USER-PROMPT"));
    assert_eq!(body["max_tokens"].as_u64(), Some(2048));
    assert!(
        body["temperature"].is_number(),
        "temperature must be a JSON number"
    );
    assert_eq!(body["temperature"].as_f64(), Some(0.35));
    assert_eq!(
        body["response_format"]["type"].as_str(),
        Some("json_object")
    );
    let auth = requests[0]
        .headers
        .get("authorization")
        .expect("bearer header");
    assert_eq!(auth.to_str().expect("ascii"), "Bearer qwen-test-key");
}

#[tokio::test]
async fn qwen_with_json_mode_false_omits_response_format() {
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(json!({"choices": [{"message": {"content": "ok"}}]})),
    )
    .await;

    let provider = QwenProvider::new(&qwen_cfg(&server.uri())).with_json_mode(false);
    provider.complete("s", "u").await.expect("mock returns 200");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().expect("json body");
    assert!(
        body.get("response_format").is_none(),
        "response_format must be omitted when json_mode is off: {body}"
    );
    assert_eq!(body["model"].as_str(), Some("qwen-max-test"));
}

#[tokio::test]
async fn kimi_request_shape_uses_config_model_and_no_response_format() {
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(json!({"choices": [{"message": {"content": "ok"}}]})),
    )
    .await;

    let provider = KimiProvider::new(&kimi_cfg(&server.uri()));
    let completion = provider
        .complete("SYS", "USER")
        .await
        .expect("mock returns 200");
    assert_eq!(completion.text, "ok");
    assert_eq!(completion.provider, "kimi");
    assert_eq!(completion.model, "kimi-k3-adversarial");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().expect("json body");
    assert_eq!(body["model"].as_str(), Some("kimi-k3-adversarial"));
    assert!(
        body.get("response_format").is_none(),
        "kimi json_mode defaults off: {body}"
    );
    assert_eq!(body["max_tokens"].as_u64(), Some(4000), "Kimi constant");
    assert!(
        body["temperature"].is_number(),
        "temperature must be a JSON number"
    );
    assert_eq!(
        body["temperature"].as_f64(),
        Some(0.1),
        "Kimi constant, numeric"
    );
    assert_eq!(body["messages"][1]["content"].as_str(), Some("USER"));
    let auth = requests[0]
        .headers
        .get("authorization")
        .expect("bearer header");
    assert_eq!(auth.to_str().expect("ascii"), "Bearer kimi-test-key");
}

#[tokio::test]
async fn provider_retries_once_on_500_then_fails() {
    let server = MockServer::start().await;
    mount_chat(&server, ResponseTemplate::new(500)).await;

    let provider = QwenProvider::new(&qwen_cfg(&server.uri()));
    let error = provider.complete("s", "u").await.expect_err("500 → error");
    match error {
        SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
            assert_eq!(provider, "qwen");
            assert_eq!(status, 500);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 2, "one retry on 5xx: exactly two requests");
}

#[tokio::test]
async fn provider_does_not_retry_on_4xx() {
    let server = MockServer::start().await;
    mount_chat(&server, ResponseTemplate::new(418)).await;

    let provider = QwenProvider::new(&qwen_cfg(&server.uri()));
    let error = provider.complete("s", "u").await.expect_err("418 → error");
    match error {
        SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
            assert_eq!(provider, "qwen");
            assert_eq!(status, 418);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1, "4xx is never retried");
}

#[tokio::test]
async fn malformed_200_body_is_invalid_json_after_one_request() {
    // (a) valid JSON, but no usable `choices[0].message.content`.
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})),
    )
    .await;
    let provider = QwenProvider::new(&qwen_cfg(&server.uri()));
    let error = provider
        .complete("s", "u")
        .await
        .expect_err("missing choices");
    match error {
        SentinelError::Brain(BrainError::InvalidJson { provider }) => assert_eq!(provider, "qwen"),
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        server.received_requests().await.expect("recorded").len(),
        1,
        "a malformed 200 is not retried"
    );

    // (b) the body is not JSON at all.
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200).set_body_string("totally not json"),
    )
    .await;
    let provider = QwenProvider::new(&qwen_cfg(&server.uri()));
    let error = provider
        .complete("s", "u")
        .await
        .expect_err("non-json body");
    match error {
        SentinelError::Brain(BrainError::InvalidJson { provider }) => assert_eq!(provider, "qwen"),
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        server.received_requests().await.expect("recorded").len(),
        1,
        "a non-json 200 is not retried"
    );
}

#[tokio::test]
async fn client_timeout_retries_once_and_reports_status_zero() {
    let server = MockServer::start().await;
    mount_chat(
        &server,
        ResponseTemplate::new(200).set_delay(Duration::from_secs(3)),
    )
    .await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(150))
        .build()
        .expect("client builds");
    let provider = QwenProvider::new(&qwen_cfg(&server.uri())).with_client(client);

    let error = provider
        .complete("s", "u")
        .await
        .expect_err("150 ms timeout");
    match error {
        SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
            assert_eq!(provider, "qwen");
            assert_eq!(status, 0, "a timeout maps to HTTP status 0");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // The retry fired; give the abandoned attempt a beat to land, then count.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        2,
        "one retry on timeout: exactly two requests"
    );
}

#[tokio::test]
async fn mock_provider_pops_in_order_records_calls_and_exhausts() {
    let mock = MockProvider::canned(vec!["first".to_string(), "second".to_string()]);
    assert_eq!(mock.name(), "mock");

    let first = mock
        .complete("sys-1", "user-1")
        .await
        .expect("first canned");
    assert_eq!(first.text, "first");
    assert_eq!(first.provider, "mock");
    assert_eq!(first.prompt_tokens, None);
    assert_eq!(first.latency_ms, 0);

    let second = mock
        .complete("sys-2", "user-2")
        .await
        .expect("second canned");
    assert_eq!(second.text, "second");

    let error = mock
        .complete("sys-3", "user-3")
        .await
        .expect_err("queue exhausted");
    match error {
        SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
            assert_eq!(last, "mock queue exhausted");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let calls = mock.calls();
    assert_eq!(
        calls.len(),
        3,
        "every attempt is recorded, even after exhaustion"
    );
    assert_eq!(calls[0], ("sys-1".to_string(), "user-1".to_string()));
    assert_eq!(calls[1], ("sys-2".to_string(), "user-2".to_string()));
    assert_eq!(calls[2], ("sys-3".to_string(), "user-3".to_string()));
}

// ===========================================================================
// Engine (SPEC-P07 §6) — boundaries re-derived, observed via calls().
// ===========================================================================

/// `MockProvider` behind a shared handle so the verifier keeps `calls()`
/// access after the engine (which owns its provider) takes it. Uses only the
/// public `Provider` trait + `MockProvider` API.
struct SharedMock(Arc<MockProvider>);

impl Provider for SharedMock {
    async fn complete(
        &self,
        system: &str,
        user: &str,
    ) -> std::result::Result<RawCompletion, SentinelError> {
        self.0.complete(system, user).await
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }
}

/// Build a shared mock + engine over it.
fn engine_with(
    texts: Vec<String>,
    min_interval_secs: u64,
    floor: &str,
) -> (StrategyEngine<SharedMock>, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::canned(texts));
    let engine = StrategyEngine::new(SharedMock(Arc::clone(&mock)), min_interval_secs, dec(floor));
    (engine, mock)
}

#[tokio::test]
async fn rate_limit_boundary_re_derived() {
    let (engine, mock) = engine_with(
        vec![hold_json(FOCUS, "0.72"), hold_json(FOCUS, "0.72")],
        2,
        "0.5",
    );
    let input = consult_input(FOCUS);
    let t = 1_000_000u64;

    engine
        .consult(&input, t)
        .await
        .expect("t admits the first consult");

    let error = engine
        .consult(&input, t + 1999)
        .await
        .expect_err("t + 1999 < interval");
    match error {
        SentinelError::Brain(BrainError::RateLimited {
            market_id,
            remaining_ms,
        }) => {
            assert_eq!(market_id, FOCUS);
            assert_eq!(remaining_ms, 1, "2000 − 1999 ms remain");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    engine
        .consult(&input, t + 2000)
        .await
        .expect("equality at the interval is allowed");

    assert_eq!(
        mock.calls().len(),
        2,
        "the rate-limited consult never reached the provider"
    );
}

#[tokio::test]
async fn rate_limit_is_per_market() {
    let (engine, mock) = engine_with(
        vec![hold_json(FOCUS, "0.72"), hold_json(OTHER, "0.72")],
        2,
        "0.5",
    );
    let t = 1_000_000u64;

    engine
        .consult(&consult_input(FOCUS), t)
        .await
        .expect("focus consult admits");
    // 1 ms later the other market is unaffected by the focus market's limit.
    engine
        .consult(&consult_input(OTHER), t + 1)
        .await
        .expect("the other market has its own clock");
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn floor_equal_is_kept() {
    let (engine, _mock) = engine_with(vec![hold_json(FOCUS, "0.6")], 2, "0.6");
    let outcome = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect("consult ok");

    assert!(!outcome.downgraded, "confidence == floor is kept");
    assert_eq!(outcome.decision.action, DecisionAction::Hold);
    assert_eq!(outcome.decision.confidence, dec("0.6"));
}

#[tokio::test]
async fn floor_just_below_downgrades_to_escalate() {
    let below = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.5999999,"urgency":"CRITICAL","reason":"borderline"}"#;
    let (engine, _mock) = engine_with(vec![below.to_string()], 2, "0.6");
    let outcome = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect("consult ok");

    assert!(outcome.downgraded, "just below the floor must downgrade");
    assert_eq!(outcome.decision.action, DecisionAction::Escalate);
    assert_eq!(outcome.decision.confidence, dec("0.5999999"));
    assert_eq!(
        outcome.decision.urgency,
        Urgency::Critical,
        "urgency is kept"
    );
    assert_eq!(outcome.decision.reason, "borderline", "reason is kept");
}

#[tokio::test]
async fn repair_flow_calls_provider_exactly_twice_with_the_nudge() {
    let bad = "I cannot produce JSON right now.";
    let (engine, mock) = engine_with(vec![bad.to_string(), hold_json(FOCUS, "0.72")], 2, "0.5");
    let outcome = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect("the repair call succeeds");

    assert!(outcome.repaired);
    assert_eq!(outcome.decision.action, DecisionAction::Hold);
    assert_eq!(outcome.decision.confidence, dec("0.72"));
    assert_eq!(outcome.provider_used, "mock");
    assert_eq!(outcome.prompt_version, PROMPT_VERSION);
    assert!(!outcome.downgraded);

    let calls = mock.calls();
    assert_eq!(calls.len(), 2, "exactly one repair call");
    assert_eq!(
        calls[0].0, calls[1].0,
        "the repair reuses the system prompt"
    );
    assert!(
        calls[1].1.contains("Return ONLY the JSON"),
        "second user carries the nudge: {}",
        calls[1].1
    );
    assert!(
        calls[1].1.contains(bad),
        "the nudge quotes the previous output"
    );
}

#[tokio::test]
async fn repair_still_invalid_maps_to_invalid_json() {
    let (engine, mock) = engine_with(
        vec!["no json at all".to_string(), "still not json".to_string()],
        2,
        "0.5",
    );
    let error = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect_err("both completions are unparseable");
    match error {
        SentinelError::Brain(BrainError::InvalidJson { provider }) => assert_eq!(provider, "mock"),
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(mock.calls().len(), 2, "exactly one repair attempt");
}

// ===========================================================================
// Eval (SPEC-P07 §7) — grounding, scoring, and the real harness binary.
// ===========================================================================

#[test]
fn grounding_check_rejects_absent_decimal_tokens() {
    // '24.10' as a whole token is NOT present in '24.1'.
    assert!(!grounding_check("exit at 24.10", "the 24.1 level held"));
    assert!(!grounding_check("used 24.10 and 99", "only 24.1 here"));
}

#[test]
fn grounding_check_accepts_present_tokens_and_empty_token_sets() {
    // '25' is present as a substring of the input.
    assert!(grounding_check(
        "level 25 held",
        "the 25 level is in the data"
    ));
    // No numbers ⇒ vacuously grounded.
    assert!(grounding_check(
        "no numbers in this reason",
        "anything at all"
    ));
    assert!(grounding_check("", ""));
    // Substring semantics: '24.1' IS a substring of '24.10'.
    assert!(grounding_check("24.1", "24.10"));
    assert!(grounding_check("mark 12.50", "mark 12.50 now"));
}

/// Minimal scenario fixture for `score()` math.
fn scenario(name: &str, injection: bool) -> Scenario {
    Scenario {
        name: name.to_string(),
        notes: String::new(),
        expected: Expected {
            action_class: vec!["HOLD".to_string()],
            schema_valid: true,
            injection,
        },
        snapshot: account(),
        markets: vec![market(FOCUS, "ETH"), market(OTHER, "BTC")],
        focus_market_id: FOCUS,
        policy: policy(),
        sm: SmartMoneyContext::unavailable("ETH"),
        reflex: ReflexSummary::default(),
        mock_completion: Some(hold_json(FOCUS, "0.72")),
    }
}

/// Minimal result fixture for `score()` math.
#[allow(clippy::too_many_arguments)]
fn scenario_result(
    name: &str,
    schema_valid: bool,
    action_class_ok: bool,
    grounding_ok: bool,
    decided_action: Option<&str>,
    error: Option<&str>,
) -> ScenarioResult {
    ScenarioResult {
        name: name.to_string(),
        schema_valid,
        action_class_ok,
        grounding_ok,
        decided_action: decided_action.map(str::to_string),
        expected_classes: vec!["HOLD".to_string()],
        provider: "mock".to_string(),
        latency_ms: 0,
        repaired: false,
        error: error.map(str::to_string),
    }
}

#[test]
fn score_excludes_injection_from_core_and_counts_parsed_grounding() {
    let scenarios = vec![
        scenario("core-ok", false),
        scenario("core-miss", false),
        scenario("injection-ok", true),
        scenario("core-error", false),
    ];
    let results = vec![
        scenario_result("core-ok", true, true, true, Some("HOLD"), None),
        scenario_result("core-miss", true, false, false, Some("CLOSE"), None),
        // Injection: schema-valid, parsed — counted in schema/injection/
        // grounding totals, never in core accuracy.
        scenario_result("injection-ok", true, false, true, Some("ESCALATE"), None),
        scenario_result(
            "core-error",
            false,
            false,
            false,
            None,
            Some("provider down"),
        ),
    ];

    let board = score(&scenarios, &results);
    let expected = Scoreboard {
        core_ok: 1,
        core_total: 3,
        schema_ok: 3,
        total: 4,
        injection_ok: 1,
        injection_total: 1,
        // Grounding totals count scenarios with a parsed decision (the errored
        // one is excluded), injection included.
        grounding_ok: 2,
        grounding_total: 3,
    };
    assert_eq!(board, expected);

    assert_eq!(
        score(&[], &[]),
        Scoreboard::default(),
        "empty run scores zero"
    );
}

#[test]
fn score_counts_scenarios_without_results_as_failed() {
    // Documented contract: results match scenarios by name; a scenario without
    // a matching result counts toward the totals as failed.
    let scenarios = vec![
        scenario("core-ok", false),
        scenario("core-no-result", false),
    ];
    let results = vec![scenario_result(
        "core-ok",
        true,
        true,
        true,
        Some("HOLD"),
        None,
    )];

    let board = score(&scenarios, &results);
    let expected = Scoreboard {
        core_ok: 1,
        core_total: 2,
        schema_ok: 1,
        total: 2,
        injection_ok: 0,
        injection_total: 0,
        grounding_ok: 1,
        grounding_total: 1,
    };
    assert_eq!(
        board, expected,
        "an unmatched scenario is counted, never dropped"
    );
}

/// Golden scenarios live at the repo root (`CRATES/sentinel/../../tests/golden`).
fn golden_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden"))
}

#[test]
fn brain_eval_mock_run_is_green_and_out_file_matches_stdout() {
    let dir = golden_dir();
    assert!(
        dir.is_dir(),
        "golden scenario dir missing: {}",
        dir.display()
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("brain-scoreboard.txt");

    let output = Command::new(env!("CARGO_BIN_EXE_brain_eval"))
        .args(["--mock", "--scenarios"])
        .arg(&dir)
        .arg("--out")
        .arg(&out_path)
        .output()
        .expect("brain_eval runs");

    let std::process::Output {
        status,
        stdout,
        stderr,
    } = output;
    let stdout = String::from_utf8(stdout).expect("stdout is utf-8");
    assert!(
        status.success(),
        "brain_eval --mock must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );

    // The frozen scoreboard lines, exactly (12/12 core, 14/14 schema,
    // 2/2 injection on the 14 golden files).
    for expected in [
        "SENTINEL BRAIN EVAL — 14 scenarios (12 core, 2 injection)",
        "action-class accuracy (core): 12/12",
        "schema validity: 14/14",
        "injection schema validity: 2/2",
    ] {
        assert!(
            stdout.lines().any(|line| line.trim_end() == expected),
            "missing frozen line {expected:?}\n--- stdout ---\n{stdout}"
        );
    }

    assert!(
        stdout
            .lines()
            .any(|line| line.trim_end().starts_with("grounding: ")),
        "grounding line missing\n--- stdout ---\n{stdout}"
    );
    assert!(
        !stdout.contains("DEGRADED"),
        "the mocked run must not degrade\n--- stdout ---\n{stdout}"
    );

    let rows: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("PASS ") || line.starts_with("FAIL "))
        .collect();
    assert_eq!(rows.len(), 14, "one row per scenario\n{rows:#?}");
    assert!(
        rows.iter().all(|row| row.starts_with("PASS ")),
        "every mocked scenario must pass\n{rows:#?}"
    );

    let written = std::fs::read_to_string(&out_path).expect("scoreboard file written");
    assert_eq!(written, stdout, "--out must be stdout-identical");
}
