//! P08 adversarial verification — independent black-box corpus (SPEC-P08.md
//! §2–§5).
//!
//! Written from `SPEC-P08.md` + the frozen public API only: the sibling
//! implementations (chain/engine/providers/harness) are treated as black
//! boxes. Log output is deliberately not asserted — the corpus pins
//! observable fields (per SPEC-P08 §8). Coverage:
//!
//! - **breaker (`chain.rs`)**: strike table re-derived (fresh `Closed`;
//!   strikes 1–2 stay `Closed`; strike 3 opens at exactly `now + open_ms`),
//!   the exact `until` boundary (`Open` at `until − 1`, `HalfOpen` and
//!   available at `until`), a half-open failure re-opening a **fresh** full
//!   window (not an extension of the old deadline), half-open success
//!   closing and resetting, `record_success` resetting partial progress, a
//!   1-strike geometry, and `NoProvider` naming itself `none`.
//! - **engine failover**: failover on provider error with the field shapes
//!   (`provider_used`, `failover_from`), the breaker opening after 3
//!   consecutive failures with the 4th consult skipping the primary, exactly
//!   one half-open probe (no retry within the consult after a half-open
//!   failure) and recovery through the probe; all-failed `last` naming both
//!   slots with their classes; forced-fail exact-name matching only, skip
//!   without a call, breaker accounting, failover fields.
//! - **P07 error-shape regression** (no fallback): repair-still-bad ⇒
//!   `InvalidJson{provider: "mock"}`, validation ⇒ `InvalidDecision`,
//!   provider errors propagate unchanged, primary success keeps
//!   `failover_from == None`.
//! - **budget**: N consults/hour allowed, the (N+1)th a synthetic ESCALATE
//!   (`provider_used: "budget"`, exact reason, providers not called); the
//!   hour window slides with the exact `< 3_600_000` boundary and synthetic
//!   attempts still counted; the daily token cap blocks at `>=` and slides.
//! - **harness**: `brain_eval --mock` with `FORCE_PROVIDER_FAIL=qwen` exits 0
//!   with every row `provider=kimi` and 12/12 core; without it every row is
//!   `provider=mock`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel::brain::chain::{BreakerState, CircuitBreaker, NoProvider};
use sentinel::brain::engine::{ConsultInput, ConsultOutcome, StrategyEngine};
use sentinel::brain::prompts::{PolicySummary, ReflexSummary, SmartMoneyContext};
use sentinel::brain::providers::{MockProvider, Provider, RawCompletion};
use sentinel::error::{BrainError, SentinelError};
use sentinel_core::types::{AccountState, DecisionAction, Market, MarketId, Position, Urgency};

/// The focus market every fixture below consults.
const FOCUS: u32 = 32;
/// A second market, so fixtures mirror the golden account shape.
const OTHER: u32 = 20;

/// Decimal literal shorthand (tests only).
fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal literal")
}

/// A compliant HOLD decision on `market_id`.
fn hold_json(market_id: u32, confidence: &str) -> String {
    format!(
        r#"{{"action":"HOLD","market_id":{market_id},"amount":null,"confidence":{confidence},"urgency":"ROUTINE","reason":"grounded in the snapshot"}}"#
    )
}

/// Unparseable provider output (no JSON object at all).
fn bad() -> String {
    "I cannot produce JSON right now.".to_string()
}

/// Schema-invalid decision: REDUCE requires a positive amount.
fn invalid_decision_json() -> String {
    format!(
        r#"{{"action":"REDUCE","market_id":{FOCUS},"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"trim"}}"#
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

/// Policy facts used across engine fixtures.
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

/// `MockProvider` behind a shared handle so the verifier keeps `calls()`
/// access after the engine (which owns its provider) takes it.
struct SharedMock(Arc<MockProvider>);

impl SharedMock {
    fn new(mock: &Arc<MockProvider>) -> Self {
        Self(Arc::clone(mock))
    }
}

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

/// A provider that always fails at the transport level: exactly one call per
/// consult (no repair is possible on a `complete()` error), which makes the
/// breaker/accounting arithmetic unambiguous.
struct RejectingProvider {
    name: &'static str,
    status: u16,
    calls: AtomicUsize,
}

impl RejectingProvider {
    fn new(name: &'static str, status: u16) -> Self {
        Self {
            name,
            status,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for RejectingProvider {
    async fn complete(
        &self,
        _system: &str,
        _user: &str,
    ) -> std::result::Result<RawCompletion, SentinelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(SentinelError::Brain(BrainError::ProviderHttp {
            provider: self.name.to_string(),
            status: self.status,
        }))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// Shared handle over a [`RejectingProvider`].
struct SharedRejecting(Arc<RejectingProvider>);

impl SharedRejecting {
    fn new(provider: &Arc<RejectingProvider>) -> Self {
        Self(Arc::clone(provider))
    }
}

impl Provider for SharedRejecting {
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

/// A primary + fallback engine whose mocks stay observable.
fn chain_engine(
    primary: &Arc<MockProvider>,
    fallback: &Arc<MockProvider>,
    min_interval_secs: u64,
    floor: &str,
) -> StrategyEngine<SharedMock, SharedMock> {
    StrategyEngine::new(SharedMock::new(primary), min_interval_secs, dec(floor))
        .with_fallback(SharedMock::new(fallback))
}

/// A single-provider (no fallback) engine whose mock stays observable.
fn solo_engine(
    mock: &Arc<MockProvider>,
    min_interval_secs: u64,
    floor: &str,
) -> StrategyEngine<SharedMock> {
    StrategyEngine::new(SharedMock::new(mock), min_interval_secs, dec(floor))
}

/// The full frozen synthetic budget outcome body (SPEC-P08 §3 step 2).
fn assert_budget_synthetic(outcome: &ConsultOutcome) {
    assert_eq!(
        outcome.decision.action,
        DecisionAction::Escalate,
        "budget exhaustion must escalate"
    );
    assert_eq!(outcome.decision.market_id, FOCUS);
    assert_eq!(outcome.decision.amount, None);
    assert_eq!(outcome.decision.confidence, dec("0"));
    assert_eq!(outcome.decision.urgency, Urgency::Critical);
    assert_eq!(
        outcome.decision.reason, "strategy budget exceeded — human review required",
        "reason string is frozen by SPEC-P08 §3 step 2"
    );
    assert_eq!(outcome.provider_used, "budget");
    assert_eq!(outcome.failover_from, None);
    assert!(!outcome.repaired);
    assert!(!outcome.downgraded);
}

// ===========================================================================
// Breaker (SPEC-P08 §2) — state machine table re-derived from the spec.
// ===========================================================================

#[test]
fn breaker_strike_table_opens_on_exactly_the_third_consecutive_failure() {
    let mut breaker = CircuitBreaker::new(3, 300_000);
    assert_eq!(breaker.threshold, 3, "constructor stores the threshold");
    assert_eq!(breaker.open_ms, 300_000, "constructor stores the window");

    assert!(matches!(breaker.state(0), BreakerState::Closed));
    assert!(breaker.is_available(0), "fresh breaker lets calls through");

    breaker.record_failure(10_000);
    assert!(
        matches!(breaker.state(10_000), BreakerState::Closed),
        "strike 1 must stay closed"
    );
    assert!(breaker.is_available(10_000));

    breaker.record_failure(20_000);
    assert!(
        matches!(breaker.state(20_000), BreakerState::Closed),
        "strike 2 must stay closed"
    );
    assert!(breaker.is_available(20_000));

    breaker.record_failure(30_000);
    assert!(
        matches!(
            breaker.state(30_000),
            BreakerState::Open { until_ms: 330_000 }
        ),
        "strike 3 opens with until = failure time + open_ms"
    );
    assert!(
        !breaker.is_available(30_000),
        "an open breaker refuses calls"
    );
}

#[test]
fn breaker_until_boundary_is_exact_and_half_open_success_closes_and_resets() {
    let mut breaker = CircuitBreaker::new(2, 1_000);
    breaker.record_failure(5_000);
    breaker.record_failure(5_500); // until = 5_500 + 1_000 = 6_500

    assert!(
        matches!(breaker.state(6_499), BreakerState::Open { until_ms: 6_500 }),
        "one ms before the deadline is still open"
    );
    assert!(
        !breaker.is_available(6_499),
        "one ms before the deadline refuses calls"
    );
    assert!(
        matches!(breaker.state(6_500), BreakerState::HalfOpen),
        "at exactly until the window elapsed"
    );
    assert!(
        breaker.is_available(6_500),
        "a probe is allowed at exactly until"
    );

    breaker.record_success();
    assert!(
        matches!(breaker.state(6_500), BreakerState::Closed),
        "half-open success closes the breaker"
    );
    assert!(breaker.is_available(6_500));
    // Reset proof: with threshold 2, one fresh failure must not re-open.
    breaker.record_failure(7_000);
    assert!(
        matches!(breaker.state(7_000), BreakerState::Closed),
        "the failure counter was reset to zero"
    );
}

#[test]
fn breaker_half_open_failure_reopens_a_fresh_full_window() {
    let mut breaker = CircuitBreaker::new(3, 300_000);
    breaker.record_failure(1_000);
    breaker.record_failure(2_000);
    breaker.record_failure(3_000); // until = 303_000

    let probe_at = 453_000; // 150 s after the first window elapsed
    assert!(matches!(breaker.state(probe_at), BreakerState::HalfOpen));

    breaker.record_failure(probe_at);
    let fresh_until = probe_at + 300_000; // 753_000
    assert!(
        matches!(breaker.state(probe_at), BreakerState::Open { until_ms } if until_ms == fresh_until),
        "a half-open failure must re-open for a full fresh window from the probe time"
    );
    assert_ne!(
        fresh_until,
        303_000 + 300_000,
        "not an extension of the old deadline"
    );
    assert!(!breaker.is_available(fresh_until - 1));
    assert!(matches!(breaker.state(fresh_until), BreakerState::HalfOpen));
    assert!(breaker.is_available(fresh_until));
}

#[test]
fn breaker_success_resets_partial_progress_and_one_strike_geometry_holds() {
    let mut breaker = CircuitBreaker::new(3, 10_000);
    breaker.record_failure(100);
    breaker.record_failure(200);
    breaker.record_success();
    assert!(
        matches!(breaker.state(300), BreakerState::Closed),
        "success clears accrued strikes"
    );

    // A full new series of three is required afterwards.
    breaker.record_failure(300);
    breaker.record_failure(400);
    assert!(matches!(breaker.state(400), BreakerState::Closed));
    breaker.record_failure(500);
    assert!(matches!(
        breaker.state(500),
        BreakerState::Open { until_ms: 10_500 }
    ));

    // Distinct non-default geometry: one strike, 50 ms window.
    let mut one_strike = CircuitBreaker::new(1, 50);
    assert!(one_strike.is_available(10));
    one_strike.record_failure(10);
    assert!(matches!(
        one_strike.state(10),
        BreakerState::Open { until_ms: 60 }
    ));
    assert!(!one_strike.is_available(59));
    assert!(matches!(one_strike.state(60), BreakerState::HalfOpen));
    assert!(one_strike.is_available(60));
}

#[tokio::test]
async fn no_provider_names_itself_none_and_always_fails() {
    let none = NoProvider;
    assert_eq!(none.name(), "none");
    let error = none
        .complete("system", "user")
        .await
        .expect_err("NoProvider cannot complete");
    match error {
        SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
            assert!(!last.is_empty(), "failure carries a descriptive tail");
        }
        other => panic!("expected AllProvidersFailed, got {other:?}"),
    }
}

// ===========================================================================
// Engine failover (SPEC-P08 §3) — fields, breaker skip, half-open probes.
// ===========================================================================

#[tokio::test]
async fn provider_error_fails_over_and_the_breaker_open_skips_the_fourth_consult() {
    let primary = Arc::new(RejectingProvider::new("qwen", 503));
    let fallback = Arc::new(MockProvider::canned_named(
        "kimi",
        vec![hold_json(FOCUS, "0.72"); 8],
    ));
    let engine = StrategyEngine::new(SharedRejecting::new(&primary), 1, dec("0.5"))
        .with_fallback(SharedMock::new(&fallback));
    let input = consult_input(FOCUS);

    let first = engine
        .consult(&input, 1_000_000)
        .await
        .expect("fallback succeeds");
    assert_eq!(first.provider_used, "kimi");
    assert_eq!(first.failover_from, Some("qwen".to_string()));
    assert_eq!(first.decision.action, DecisionAction::Hold);
    assert!(!first.repaired);
    assert_eq!(primary.calls(), 1, "one provider call per failed attempt");

    // Two more consecutive failures: 3 strikes ⇒ Open until 1_002_000 + 300_000.
    engine
        .consult(&input, 1_001_000)
        .await
        .expect("second failover");
    engine
        .consult(&input, 1_002_000)
        .await
        .expect("third failover");
    assert_eq!(primary.calls(), 3);

    // The 4th consult, still inside the window: the breaker refuses the
    // primary before any call (calls() count frozen).
    let fourth = engine
        .consult(&input, 1_003_000)
        .await
        .expect("fallback again");
    assert_eq!(fourth.provider_used, "kimi");
    assert_eq!(fourth.failover_from, Some("qwen".to_string()));
    assert_eq!(primary.calls(), 3, "open breaker: the primary is skipped");

    // A probe lands 150 s after the first deadline elapsed (well past any
    // naive extension of the old deadline). Exactly one call goes out.
    let probe = engine
        .consult(&input, 1_452_000)
        .await
        .expect("probe fails over");
    assert_eq!(probe.provider_used, "kimi");
    assert_eq!(primary.calls(), 4, "one half-open probe is allowed");

    // The failed probe re-opened a FRESH window (1_452_000 + 300_000 =
    // 1_752_000); a naive old-deadline extension (1_302_000 + 300_000 =
    // 1_602_000) would have probed again at 1_602_001.
    let between = engine
        .consult(&input, 1_602_001)
        .await
        .expect("fallback runs");
    assert_eq!(between.provider_used, "kimi");
    assert_eq!(
        primary.calls(),
        4,
        "fresh window, not an old-deadline extension"
    );

    let second_probe = engine
        .consult(&input, 1_752_000)
        .await
        .expect("second probe fails over");
    assert_eq!(second_probe.provider_used, "kimi");
    assert_eq!(
        primary.calls(),
        5,
        "the fresh deadline permits one new probe"
    );
}

#[tokio::test]
async fn all_providers_failed_names_both_slots_with_their_classes() {
    let primary = Arc::new(MockProvider::canned_named("qwen", vec![bad(), bad()]));
    let fallback = Arc::new(MockProvider::canned_named("kimi", vec![bad(), bad()]));
    let engine = chain_engine(&primary, &fallback, 1, "0.5");

    let error = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect_err("both providers return garbage");
    match error {
        SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
            assert_eq!(
                last, "qwen: invalid_output; kimi: invalid_output",
                "last must name both slots and both classes"
            );
        }
        other => panic!("expected AllProvidersFailed, got {other:?}"),
    }
    assert_eq!(primary.calls().len(), 2, "primary: original + one repair");
    assert_eq!(fallback.calls().len(), 2, "fallback: original + one repair");
}

#[tokio::test]
async fn forced_fail_matches_the_provider_name_exactly_only() {
    let input = consult_input(FOCUS);

    // Partial name: no match, so the primary is used normally.
    let primary = Arc::new(MockProvider::canned_named(
        "qwen",
        vec![hold_json(FOCUS, "0.72")],
    ));
    let fallback = Arc::new(MockProvider::canned_named(
        "kimi",
        vec![hold_json(FOCUS, "0.72")],
    ));
    let engine = chain_engine(&primary, &fallback, 1, "0.5").with_forced_failure("qw");
    let outcome = engine.consult(&input, 1_000_000).await.expect("consult ok");
    assert_eq!(
        outcome.provider_used, "qwen",
        "\"qw\" must not match \"qwen\""
    );
    assert_eq!(outcome.failover_from, None);
    assert_eq!(primary.calls().len(), 1, "primary was called");
    assert_eq!(fallback.calls().len(), 0, "fallback was not needed");

    // Case matters too: "Qwen" is not "qwen".
    let primary = Arc::new(MockProvider::canned_named(
        "qwen",
        vec![hold_json(FOCUS, "0.72")],
    ));
    let fallback = Arc::new(MockProvider::canned_named(
        "kimi",
        vec![hold_json(FOCUS, "0.72")],
    ));
    let engine = chain_engine(&primary, &fallback, 1, "0.5").with_forced_failure("Qwen");
    let outcome = engine.consult(&input, 1_000_000).await.expect("consult ok");
    assert_eq!(
        outcome.provider_used, "qwen",
        "\"Qwen\" must not match \"qwen\""
    );
    assert_eq!(primary.calls().len(), 1);
    assert_eq!(fallback.calls().len(), 0);
}

#[tokio::test]
async fn forced_failure_skips_without_calling_and_feeds_the_breaker() {
    let primary = Arc::new(MockProvider::canned_named(
        "qwen",
        vec![hold_json(FOCUS, "0.72"); 4],
    ));
    let fallback = Arc::new(MockProvider::canned_named("kimi", vec![bad(); 12]));
    let engine = chain_engine(&primary, &fallback, 1, "0.5").with_forced_failure("qwen");
    let input = consult_input(FOCUS);

    // Three forced consults: the primary is skipped WITHOUT a call and each
    // skip records one breaker failure.
    for at in [1_000_000u64, 1_001_000, 1_002_000] {
        let error = engine
            .consult(&input, at)
            .await
            .expect_err("forced primary, fallback fails");
        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(
                    last, "qwen: forced; kimi: invalid_output",
                    "a forced failure is recorded with the class `forced`"
                );
            }
            other => panic!("expected AllProvidersFailed, got {other:?}"),
        }
    }
    assert_eq!(
        primary.calls().len(),
        0,
        "forced-fail must not touch the provider"
    );
    assert_eq!(
        fallback.calls().len(),
        6,
        "fallback: original + repair per consult"
    );

    // After 3 failed attempts BOTH breakers are open (one per slot): the
    // fallback's shows up directly as class `circuit_open` on the 4th
    // consult, which still skips the primary with no call at all. Whether
    // the reported primary class stays `forced` or flips to `circuit_open`
    // depends on the (spec-unspecified) guard order, so both are accepted.
    let error = engine
        .consult(&input, 1_003_000)
        .await
        .expect_err("still no fallback success");
    match error {
        SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
            assert!(
                last == "qwen: forced; kimi: circuit_open"
                    || last == "qwen: circuit_open; kimi: circuit_open",
                "unexpected 4th-consult summary: {last:?}"
            );
        }
        other => panic!("expected AllProvidersFailed, got {other:?}"),
    }
    assert_eq!(
        primary.calls().len(),
        0,
        "the forced-open primary is never called"
    );
    assert_eq!(
        fallback.calls().len(),
        6,
        "the open fallback breaker skips without a call too"
    );

    // At the fallback's fresh deadline its half-open probe fires again
    // (unparseable output ⇒ class `invalid_output`); the primary is still
    // skipped for free.
    let error = engine
        .consult(&input, 1_302_001)
        .await
        .expect_err("fallback probe fails again");
    match error {
        SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
            assert!(
                last == "qwen: forced; kimi: invalid_output"
                    || last == "qwen: circuit_open; kimi: invalid_output",
                "unexpected probe summary: {last:?}"
            );
        }
        other => panic!("expected AllProvidersFailed, got {other:?}"),
    }
    assert_eq!(primary.calls().len(), 0);
    assert_eq!(
        fallback.calls().len(),
        8,
        "fallback probe attempt: original + repair"
    );
}

#[tokio::test]
async fn half_open_probe_is_single_and_no_retry_happens_within_the_consult() {
    // Primary: 8 unparseable texts (opening + one failed probe), then valid
    // output for the recovery probe and the direct consult after it.
    let mut primary_texts: Vec<String> = vec![bad(); 8];
    primary_texts.push(hold_json(FOCUS, "0.72"));
    primary_texts.push(hold_json(FOCUS, "0.72"));
    let primary = Arc::new(MockProvider::canned_named("qwen", primary_texts));
    let fallback = Arc::new(MockProvider::canned_named(
        "kimi",
        vec![hold_json(FOCUS, "0.72"); 8],
    ));
    let engine = chain_engine(&primary, &fallback, 1, "0.5");
    let input = consult_input(FOCUS);

    // Consults 1–3: unparseable primary output (original + repair, one
    // breaker failure each) ⇒ Open until 102_000 + 300_000 = 402_000.
    for at in [100_000u64, 101_000, 102_000] {
        let outcome = engine.consult(&input, at).await.expect("fallback succeeds");
        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from, Some("qwen".to_string()));
    }
    assert_eq!(primary.calls().len(), 6, "two calls per failed attempt");

    // The 4th consult, still inside the window: the primary is skipped.
    let fourth = engine
        .consult(&input, 302_000)
        .await
        .expect("fallback succeeds");
    assert_eq!(fourth.provider_used, "kimi");
    assert_eq!(primary.calls().len(), 6, "open breaker: no primary call");

    // At until + 1 the half-open probe runs ONCE. Its output is unparseable:
    // the attempt follows the frozen flow (original + one repair nudge per
    // SPEC-P08 §3.3 — the spec's `retry` vocabulary covers provider-level
    // re-attempts, and the nudge is a `repair call`) and the primary is NOT
    // re-probed afterwards: exactly one probe attempt, no further calls.
    let probe = engine
        .consult(&input, 402_001)
        .await
        .expect("fallback runs");
    assert_eq!(probe.provider_used, "kimi");
    assert_eq!(probe.failover_from, Some("qwen".to_string()));
    assert_eq!(
        primary.calls().len(),
        8,
        "single probe attempt (original + repair): no re-probe within the consult"
    );

    // The failed probe re-opened a fresh full window (402_001 + 300_000 =
    // 702_001): the next consult is skipped, not probed.
    let between = engine
        .consult(&input, 602_000)
        .await
        .expect("fallback runs");
    assert_eq!(between.provider_used, "kimi");
    assert_eq!(primary.calls().len(), 8, "fresh window: no probe yet");

    // At the fresh deadline the probe runs once and SUCCEEDS → Closed.
    let recovered = engine
        .consult(&input, 702_001)
        .await
        .expect("probe succeeds");
    assert_eq!(recovered.provider_used, "qwen");
    assert_eq!(recovered.failover_from, None);
    assert!(!recovered.repaired);

    // The primary is used directly afterwards (breaker stayed closed).
    let direct = engine
        .consult(&input, 703_500)
        .await
        .expect("primary stays closed");
    assert_eq!(direct.provider_used, "qwen");
    assert_eq!(direct.failover_from, None);

    assert_eq!(
        primary.calls().len(),
        10,
        "6 opening + probe attempt (original + repair) + recovery + direct"
    );
    assert_eq!(
        fallback.calls().len(),
        6,
        "fallback served consults 1–6 only"
    );
}

// ===========================================================================
// No-fallback P07 error-shape regression (SPEC-P08 §3 step 5).
// ===========================================================================

#[tokio::test]
async fn no_fallback_repair_still_bad_keeps_p07_invalid_json_shape() {
    let mock = Arc::new(MockProvider::canned(vec![bad(), bad()]));
    let engine = solo_engine(&mock, 1, "0.5");
    let error = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect_err("both completions unparseable");
    match error {
        SentinelError::Brain(BrainError::InvalidJson { provider }) => assert_eq!(provider, "mock"),
        other => panic!("expected InvalidJson, got {other:?}"),
    }
    assert_eq!(mock.calls().len(), 2, "exactly one repair attempt");
}

#[tokio::test]
async fn no_fallback_validation_failure_is_invalid_decision() {
    let mock = Arc::new(MockProvider::canned(vec![
        invalid_decision_json(),
        invalid_decision_json(),
    ]));
    let engine = solo_engine(&mock, 1, "0.5");
    let error = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect_err("schema-invalid decision");
    match error {
        SentinelError::Brain(BrainError::InvalidDecision { detail }) => {
            assert!(detail.contains("amount"), "detail: {detail}");
        }
        other => panic!("expected InvalidDecision, got {other:?}"),
    }
    // PROBED behavior: a validation failure burns the repair nudge like any
    // parse error (repair, then re-validate) — exactly 2 calls. SPEC-P08
    // does not freeze the count; pinned here so drift either way fails loudly.
    assert_eq!(
        mock.calls().len(),
        2,
        "validation errors also burn one repair call"
    );
}

#[tokio::test]
async fn no_fallback_provider_error_propagates_unchanged() {
    let primary = Arc::new(RejectingProvider::new("flaky", 503));
    let engine = StrategyEngine::new(SharedRejecting::new(&primary), 1, dec("0.5"));
    let error = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect_err("provider errors");
    match error {
        SentinelError::Brain(BrainError::ProviderHttp { provider, status }) => {
            assert_eq!(provider, "flaky");
            assert_eq!(status, 503);
        }
        other => panic!("expected ProviderHttp, got {other:?}"),
    }
    assert_eq!(
        primary.calls(),
        1,
        "no engine-level retry on provider errors"
    );
}

#[tokio::test]
async fn primary_success_keeps_failover_from_none() {
    let mock = Arc::new(MockProvider::canned(vec![hold_json(FOCUS, "0.72")]));
    let engine = solo_engine(&mock, 1, "0.5");
    let outcome = engine
        .consult(&consult_input(FOCUS), 1_000_000)
        .await
        .expect("consult ok");
    assert_eq!(outcome.provider_used, "mock");
    assert_eq!(outcome.failover_from, None);
    assert!(!outcome.repaired);
    assert_eq!(mock.calls().len(), 1);
}

// ===========================================================================
// Budget (SPEC-P08 §3 step 2) — consult/hour and token/day boundaries.
// ===========================================================================

#[tokio::test]
async fn consult_budget_allows_exactly_n_then_escalates_and_slides_forward() {
    let primary = Arc::new(MockProvider::canned_named(
        "qwen",
        vec![hold_json(FOCUS, "0.72"); 4],
    ));
    let fallback = Arc::new(MockProvider::canned_named(
        "kimi",
        vec![hold_json(FOCUS, "0.72"); 4],
    ));
    let engine = chain_engine(&primary, &fallback, 1, "0.5").with_budget(2, 200_000);
    let input = consult_input(FOCUS);

    let first = engine
        .consult(&input, 1_000_000)
        .await
        .expect("consult 1 inside the budget");
    assert_eq!(first.provider_used, "qwen");
    let second = engine
        .consult(&input, 1_001_000)
        .await
        .expect("consult 2 inside the budget");
    assert_eq!(second.provider_used, "qwen");

    let third = engine
        .consult(&input, 1_002_000)
        .await
        .expect("third consult returns the synthetic outcome");
    assert_budget_synthetic(&third);
    assert_eq!(
        primary.calls().len(),
        2,
        "providers must not be called past the budget"
    );
    assert_eq!(
        fallback.calls().len(),
        0,
        "the fallback was never consulted"
    );

    // Past one hour the window has slid: consults reach the provider again.
    let after = engine
        .consult(&input, 4_602_001)
        .await
        .expect("budget window slid");
    assert_eq!(after.provider_used, "qwen");
    assert_eq!(after.failover_from, None);
    assert_eq!(primary.calls().len(), 3);
}

#[tokio::test]
async fn consult_budget_hour_window_boundary_is_exact_and_counts_synthetics() {
    let mock = Arc::new(MockProvider::canned_named(
        "qwen",
        vec![hold_json(FOCUS, "0.72"); 3],
    ));
    let engine = solo_engine(&mock, 0, "0.5").with_budget(1, 200_000);
    let input = consult_input(FOCUS);
    let t = 10_000_000u64;

    let first = engine.consult(&input, t).await.expect("first consult");
    assert_eq!(first.provider_used, "qwen");

    // 1 ms shy of a full hour: the first entry still counts ⇒ synthetic.
    let blocked = engine
        .consult(&input, t + 3_599_999)
        .await
        .expect("synthetic outcome");
    assert_budget_synthetic(&blocked);

    // The first timestamp has expired now, but the synthetic attempt was
    // itself counted ("the attempt is still counted (hour)") ⇒ still blocked.
    let blocked_again = engine
        .consult(&input, t + 3_600_001)
        .await
        .expect("synthetic outcome");
    assert_budget_synthetic(&blocked_again);
    assert_eq!(
        mock.calls().len(),
        1,
        "providers stay uncalled while blocked"
    );

    // Exactly one hour past the LAST counted attempt (`now − ts < 3_600_000`
    // is false at equality) ⇒ allowed again.
    let allowed = engine
        .consult(&input, t + 7_200_001)
        .await
        .expect("window slid past the synthetic");
    assert_eq!(allowed.provider_used, "qwen");
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn consult_budget_token_cap_boundary_blocks_at_the_cap_and_slides_daily() {
    // Scenario 1: one consult uses EXACTLY the daily cap ⇒ the next blocks.
    let mock = Arc::new(MockProvider::named("qwen"));
    for _ in 0..3 {
        mock.responses
            .lock()
            .expect("mock responses lock")
            .push_back(Ok(RawCompletion {
                text: hold_json(FOCUS, "0.72"),
                provider: "qwen".to_string(),
                model: "mock-model".to_string(),
                prompt_tokens: Some(600_000),
                completion_tokens: Some(400_000),
                latency_ms: 0,
            }));
    }
    let engine = solo_engine(&mock, 0, "0.5").with_budget(100, 1_000_000);
    let input = consult_input(FOCUS);
    let t = 20_000_000u64;

    let first = engine.consult(&input, t).await.expect("first consult fits");
    assert_eq!(first.prompt_tokens, Some(600_000));
    assert_eq!(first.completion_tokens, Some(400_000));

    // sum == cap ⇒ `>=` blocks immediately.
    let blocked = engine
        .consult(&input, t + 1)
        .await
        .expect("synthetic outcome");
    assert_budget_synthetic(&blocked);
    assert_eq!(
        mock.calls().len(),
        1,
        "no provider call once the cap is reached"
    );

    // Day window: 1 ms shy of a full day the entry still counts...
    let still_blocked = engine
        .consult(&input, t + 86_399_999)
        .await
        .expect("synthetic outcome");
    assert_budget_synthetic(&still_blocked);
    // ... at exactly one day it drops out (`now − ts < 86_400_000` is false
    // at equality) ⇒ allowed again.
    let allowed = engine
        .consult(&input, t + 86_400_000)
        .await
        .expect("day window slid");
    assert_eq!(allowed.provider_used, "qwen");
    assert_eq!(mock.calls().len(), 2);

    // Scenario 2: 999_999 tokens (just below the cap) must NOT block.
    let under = Arc::new(MockProvider::named("qwen"));
    for _ in 0..3 {
        under
            .responses
            .lock()
            .expect("mock responses lock")
            .push_back(Ok(RawCompletion {
                text: hold_json(FOCUS, "0.72"),
                provider: "qwen".to_string(),
                model: "mock-model".to_string(),
                prompt_tokens: Some(600_000),
                completion_tokens: Some(399_999),
                latency_ms: 0,
            }));
    }
    let engine = solo_engine(&under, 0, "0.5").with_budget(100, 1_000_000);
    engine.consult(&input, t).await.expect("first consult");
    let second = engine
        .consult(&input, t + 1)
        .await
        .expect("just-below cap is allowed");
    assert_eq!(second.provider_used, "qwen");
    assert_eq!(under.calls().len(), 2);
}

// ===========================================================================
// Harness (SPEC-P08 §5) — the FORCE_PROVIDER_FAIL offline demonstration.
// ===========================================================================

/// Golden scenarios live at the repo root (`crates/sentinel/../../tests/golden`).
fn golden_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden"))
}

/// Run `brain_eval --mock` over the goldens with an optional forced provider.
fn run_mock_eval(force: Option<&str>, out: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brain_eval"));
    command
        .args(["--mock", "--scenarios"])
        .arg(golden_dir())
        .arg("--out")
        .arg(out);
    match force {
        Some(name) => command.env("FORCE_PROVIDER_FAIL", name),
        None => command.env_remove("FORCE_PROVIDER_FAIL"),
    };
    command.output().expect("brain_eval runs")
}

#[test]
fn brain_eval_mock_with_force_provider_fail_reports_kimi_on_every_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let Output {
        status,
        stdout,
        stderr,
    } = run_mock_eval(Some("qwen"), &tmp.path().join("forced.txt"));
    let stdout = String::from_utf8(stdout).expect("stdout is utf-8");
    assert!(
        status.success(),
        "brain_eval must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.trim_end() == "action-class accuracy (core): 12/12"),
        "scoreboard lost core accuracy\n--- stdout ---\n{stdout}"
    );

    let rows: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("PASS ") || line.starts_with("FAIL "))
        .collect();
    assert_eq!(rows.len(), 14, "one row per scenario\n{rows:#?}");
    for row in &rows {
        assert!(
            row.starts_with("PASS "),
            "forced chain must still pass: {row}"
        );
        assert!(
            row.contains("provider=kimi"),
            "every row must report provider=kimi: {row}"
        );
    }
    assert!(
        !stdout.contains("DEGRADED"),
        "the forced run must not degrade"
    );
}

#[test]
fn brain_eval_mock_without_force_reports_mock_on_every_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let Output {
        status,
        stdout,
        stderr,
    } = run_mock_eval(None, &tmp.path().join("plain.txt"));
    let stdout = String::from_utf8(stdout).expect("stdout is utf-8");
    assert!(
        status.success(),
        "brain_eval must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.trim_end() == "action-class accuracy (core): 12/12"),
        "scoreboard lost core accuracy\n--- stdout ---\n{stdout}"
    );

    let rows: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("PASS ") || line.starts_with("FAIL "))
        .collect();
    assert_eq!(rows.len(), 14, "one row per scenario\n{rows:#?}");
    for row in &rows {
        assert!(
            row.contains("provider=mock"),
            "the plain mock run must report provider=mock: {row}"
        );
    }
}
