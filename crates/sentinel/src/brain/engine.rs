//! Strategy engine — rate-limited, floor-gated LLM consults.
//!
//! Frozen by `SPEC-P07.md` §6: a per-market rate limit, the grounded prompt
//! build, one provider call plus at most one repair call, the confidence
//! floor, and a structured audit-style log. All timestamps are caller-supplied
//! (`now_ms`) so the engine is fully testable offline. Provider chains and
//! circuit breakers wrap this in P08 (`ProviderChain` implements [`Provider`]).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Decision, DecisionAction, Market, MarketId};

use crate::brain::parser::{ParseError, parse_decision, repair_nudge};
use crate::brain::prompts::{
    PROMPT_VERSION, PolicySummary, PromptInput, ReflexSummary, SmartMoneyContext, system_prompt,
    user_prompt,
};
use crate::brain::providers::{Provider, RawCompletion};
use crate::error::{BrainError, Result, SentinelError};

/// Everything a consult needs (assembled by the caller/trigger).
#[derive(Debug, Clone)]
pub struct ConsultInput {
    /// Full account snapshot.
    pub account: AccountState,
    /// Market table.
    pub markets: Vec<Market>,
    /// Market under consultation.
    pub focus_market: MarketId,
    /// Policy facts for the prompt block.
    pub policy: PolicySummary,
    /// Smart-money context (or `unavailable`).
    pub sm: SmartMoneyContext,
    /// Recent reflex actions.
    pub reflex: ReflexSummary,
}

/// A completed, validated consult.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsultOutcome {
    /// The validated decision.
    pub decision: Decision,
    /// Provider that produced it.
    pub provider_used: String,
    /// Prompt version that produced it.
    pub prompt_version: &'static str,
    /// End-to-end latency of the (winning) call, ms.
    pub latency_ms: u64,
    /// Prompt tokens of the winning call.
    pub prompt_tokens: Option<u32>,
    /// Completion tokens of the winning call.
    pub completion_tokens: Option<u32>,
    /// True when a repair nudge was needed.
    pub repaired: bool,
    /// True when the confidence floor downgraded the decision to ESCALATE.
    pub downgraded: bool,
}

/// The consult engine.
pub struct StrategyEngine<P: Provider> {
    /// Backend (single provider in P07; a chain in P08).
    provider: P,
    /// Per-market minimum interval between consults, seconds.
    min_interval_secs: u64,
    /// Decisions below this confidence downgrade to ESCALATE.
    confidence_floor: Decimal,
    /// Last consult time per market (caller clock domain).
    last_consult_ms: Mutex<HashMap<MarketId, u64>>,
}

impl<P: Provider> StrategyEngine<P> {
    /// Build an engine.
    pub fn new(provider: P, min_interval_secs: u64, confidence_floor: Decimal) -> Self {
        Self {
            provider,
            min_interval_secs,
            confidence_floor,
            last_consult_ms: Mutex::new(HashMap::new()),
        }
    }
}

impl<P: Provider> StrategyEngine<P> {
    /// Run one consult (rate limit → prompt → complete → parse → repair →
    /// floor → audit log); see `SPEC-P07.md` §6 for the exact sequence.
    ///
    /// # Errors
    /// - [`BrainError::RateLimited`] when the focus market was consulted less
    ///   than `min_interval_secs` ago;
    /// - [`BrainError::InvalidDecision`] when the focus market has no position
    ///   in the snapshot, or when the repaired completion still fails schema
    ///   validation;
    /// - [`BrainError::InvalidJson`] when neither the completion nor its one
    ///   repair carried a parseable decision object;
    /// - any provider error, propagated unchanged (never repaired).
    pub async fn consult(&self, input: &ConsultInput, now_ms: u64) -> Result<ConsultOutcome> {
        self.check_rate_limit(input.focus_market, now_ms)?;

        let focus = input
            .account
            .positions
            .iter()
            .find(|position| position.market_id == input.focus_market)
            .ok_or_else(|| BrainError::InvalidDecision {
                detail: format!("focus market {} not in snapshot", input.focus_market.0),
            })?;

        let system = system_prompt();
        let user = user_prompt(&PromptInput {
            account: &input.account,
            markets: &input.markets,
            focus,
            policy: &input.policy,
            sm: &input.sm,
            reflex: &input.reflex,
            now_ms,
        });
        let allowed_markets: Vec<MarketId> = input.markets.iter().map(|market| market.id).collect();

        let first = self.provider.complete(system, &user).await?;
        let mut repaired = false;
        let (raw, mut decision) = match parse_decision(&first.text, &allowed_markets) {
            Ok(decision) => (first, decision),
            Err(_) => {
                repaired = true;
                let nudge = repair_nudge(&first.text);
                let second = self.provider.complete(system, &nudge).await?;
                match parse_decision(&second.text, &allowed_markets) {
                    Ok(decision) => (second, decision),
                    Err(error) => return Err(map_parse_error(self.provider.name(), error)),
                }
            }
        };

        let mut downgraded = false;
        if decision.confidence < self.confidence_floor {
            tracing::warn!(
                confidence = %decision.confidence,
                floor = %self.confidence_floor,
                "confidence below the floor; downgrading the decision to ESCALATE"
            );
            decision.action = DecisionAction::Escalate;
            downgraded = true;
        }

        lock(&self.last_consult_ms).insert(input.focus_market, now_ms);
        log_consult(&self.provider, &raw, &decision, repaired, downgraded);

        Ok(ConsultOutcome {
            decision,
            provider_used: self.provider.name().to_string(),
            prompt_version: PROMPT_VERSION,
            latency_ms: raw.latency_ms,
            prompt_tokens: raw.prompt_tokens,
            completion_tokens: raw.completion_tokens,
            repaired,
            downgraded,
        })
    }

    /// Per-market rate-limit gate (`SPEC-P07.md` §6 step 1).
    ///
    /// A consult is allowed when `now_ms - last >= min_interval_secs * 1000`
    /// (equality included). A caller clock that moved backwards counts as zero
    /// elapsed — the consult stays rate-limited with the full interval
    /// remaining instead of underflowing.
    fn check_rate_limit(&self, market: MarketId, now_ms: u64) -> Result<()> {
        let min_interval_ms = self.min_interval_secs.saturating_mul(1000);
        let last_consult = lock(&self.last_consult_ms);
        if let Some(&last_ms) = last_consult.get(&market) {
            let elapsed = now_ms.saturating_sub(last_ms);
            if elapsed < min_interval_ms {
                return Err(BrainError::RateLimited {
                    market_id: market.0,
                    remaining_ms: min_interval_ms - elapsed,
                }
                .into());
            }
        }
        Ok(())
    }
}

/// Map a final parse failure onto the engine error surface (`SPEC-P07.md` §6).
fn map_parse_error(provider: &str, error: ParseError) -> SentinelError {
    match error {
        ParseError::NoObject | ParseError::Json { .. } => BrainError::InvalidJson {
            provider: provider.to_string(),
        }
        .into(),
        ParseError::Validation { detail } => BrainError::InvalidDecision { detail }.into(),
    }
}

/// Audit-style record of one successful consult (`SPEC-P07.md` §6 step 4;
/// P10 persists journals from these).
fn log_consult(
    provider: &impl Provider,
    raw: &RawCompletion,
    decision: &Decision,
    repaired: bool,
    downgraded: bool,
) {
    let decision_json = match serde_json::to_string(decision) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!(error = %error, "decision serialization for the audit log failed");
            "(unserializable)".to_string()
        }
    };
    tracing::info!(
        provider = provider.name(),
        prompt_version = PROMPT_VERSION,
        latency_ms = raw.latency_ms,
        prompt_tokens = ?raw.prompt_tokens,
        completion_tokens = ?raw.completion_tokens,
        repaired = repaired,
        downgraded = downgraded,
        action = ?decision.action,
        market_id = decision.market_id,
        confidence = %decision.confidence,
        urgency = ?decision.urgency,
        reason = %decision.reason,
        decision = %decision_json,
        "strategy consult complete"
    );
}

/// Lock a mutex, recovering the guard when a previous holder panicked.
///
/// The engine's critical sections are panic-free, so a poisoned lock only
/// reflects an unrelated panic; recovering keeps consults deterministic
/// instead of masking the original failure behind a lock error.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::brain::providers::{MockProvider, RawCompletion};
    use sentinel_core::types::{Position, Urgency};

    /// Decimal from a literal (test shorthand).
    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).expect("test decimal literal")
    }

    /// ETH-like market with the P04 fixture's maintenance margin (0.05).
    fn market(id: u32, symbol: &str) -> Market {
        Market {
            id: MarketId(id),
            symbol: symbol.to_string(),
            base: symbol.to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: Decimal::new(83333, 6),
            maintenance_margin_fraction: Decimal::new(5, 2),
            max_leverage: Decimal::new(12, 0),
            min_size: Decimal::ZERO,
            tick_size: Decimal::new(1, 2),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    /// A healthy 10x long with 45 % of headroom to liquidation.
    fn position(market_id: u32, symbol: &str) -> Position {
        Position {
            market_id: MarketId(market_id),
            symbol: symbol.to_string(),
            size: Decimal::new(10, 0),
            entry_price: Decimal::new(2700, 0),
            mark_price: Some(Decimal::new(2700, 0)),
            liq_price: None,
            collateral: Decimal::new(13560, 0),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: Decimal::new(10, 0),
            opened_at: None,
        }
    }

    /// Two-position account (ETH #32 focus candidate, BTC #20).
    fn account() -> AccountState {
        AccountState {
            positions: vec![position(32, "ETH"), position(20, "BTC")],
            free_balance: Decimal::new(5000, 0),
            equity: Decimal::new(25000, 0),
            fee_tier: 0,
            snapshot_ts: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000)
                .expect("valid test timestamp"),
        }
    }

    /// Consult input focused on ETH #32.
    fn consult_input() -> ConsultInput {
        ConsultInput {
            account: account(),
            markets: vec![market(32, "ETH"), market(20, "BTC")],
            focus_market: MarketId(32),
            policy: PolicySummary {
                market_allowlist: vec![MarketId(32), MarketId(20)],
                max_order_size_usd: Decimal::new(5000, 0),
                require_approval_above_usd: Decimal::new(2500, 0),
                daily_actions_left: 5,
            },
            sm: SmartMoneyContext::unavailable("ETH"),
            reflex: ReflexSummary::default(),
        }
    }

    fn make_engine(
        mock: MockProvider,
        min_interval_secs: u64,
        floor: &str,
    ) -> StrategyEngine<MockProvider> {
        StrategyEngine::new(mock, min_interval_secs, dec(floor))
    }

    /// Valid HOLD decision JSON for `market_id` at `confidence`.
    fn hold_json(market_id: u32, confidence: &str) -> String {
        format!(
            r#"{{"action":"HOLD","market_id":{market_id},"amount":null,"confidence":{confidence},"urgency":"ROUTINE","reason":"grounded in the snapshot"}}"#
        )
    }

    /// A mock whose queue is exactly `responses` (front pops first).
    fn mock_with(responses: Vec<std::result::Result<RawCompletion, String>>) -> MockProvider {
        MockProvider {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn happy_path_parses_and_reports_the_outcome() {
        let mock = MockProvider::canned(vec![hold_json(32, "0.72")]);
        let engine = make_engine(mock, 2, "0.5");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("happy-path consult succeeds");

        assert_eq!(outcome.decision.action, DecisionAction::Hold);
        assert_eq!(outcome.decision.market_id, 32);
        assert_eq!(outcome.decision.amount, None);
        assert_eq!(outcome.decision.confidence, dec("0.72"));
        assert_eq!(outcome.decision.urgency, Urgency::Routine);
        assert_eq!(outcome.decision.reason, "grounded in the snapshot");
        assert_eq!(outcome.provider_used, "mock");
        assert_eq!(outcome.prompt_version, PROMPT_VERSION);
        assert_eq!(outcome.latency_ms, 0);
        assert_eq!(outcome.prompt_tokens, None);
        assert_eq!(outcome.completion_tokens, None);
        assert!(!outcome.repaired);
        assert!(!outcome.downgraded);

        let calls = engine.provider.calls();
        assert_eq!(calls.len(), 1, "a valid first completion needs no repair");
        assert_eq!(calls[0].0, system_prompt());
        assert!(calls[0].1.contains("- prompt_version: v3.0"));
        assert!(
            calls[0].1.contains("- now_ms: 1000000"),
            "now_ms is injected"
        );
        assert!(calls[0].1.contains("## ACCOUNT"));
        assert!(
            calls[0].1.contains(">> ETH | #32"),
            "focus position is marked: {}",
            calls[0].1
        );
    }

    #[tokio::test]
    async fn happy_path_carries_the_winning_latency_and_tokens() {
        let raw = RawCompletion {
            text: hold_json(32, "0.6"),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            prompt_tokens: Some(120),
            completion_tokens: Some(48),
            latency_ms: 321,
        };
        let engine = make_engine(mock_with(vec![Ok(raw)]), 2, "0.5");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("consult succeeds");

        assert_eq!(outcome.latency_ms, 321);
        assert_eq!(outcome.prompt_tokens, Some(120));
        assert_eq!(outcome.completion_tokens, Some(48));
        assert_eq!(engine.provider.calls().len(), 1);
    }

    #[tokio::test]
    async fn repair_flow_retries_once_with_the_nudge() {
        let bad = "I'm sorry, I can't produce JSON right now.".to_string();
        let mock = MockProvider::canned(vec![bad.clone(), hold_json(32, "0.72")]);
        let engine = make_engine(mock, 2, "0.5");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("the repair call succeeds");

        assert!(outcome.repaired);
        assert_eq!(outcome.decision.action, DecisionAction::Hold);
        assert_eq!(outcome.decision.confidence, dec("0.72"));

        let calls = engine.provider.calls();
        assert_eq!(calls.len(), 2, "provider called exactly twice");
        assert_eq!(
            calls[0].0, calls[1].0,
            "the repair reuses the system prompt"
        );
        assert!(calls[1].1.contains("Return ONLY the JSON"));
        assert_eq!(calls[1].1, repair_nudge(&bad), "the exact repair nudge");
    }

    #[tokio::test]
    async fn repair_flow_still_invalid_json_maps_to_invalid_json() {
        let mock = MockProvider::canned(vec![
            "refusal: no JSON here".to_string(),
            "still refusing".to_string(),
        ]);
        let engine = make_engine(mock, 2, "0.5");

        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("both completions are unparseable");

        match error {
            SentinelError::Brain(BrainError::InvalidJson { provider }) => {
                assert_eq!(provider, "mock");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            engine.provider.calls().len(),
            2,
            "exactly one repair attempt"
        );
    }

    #[tokio::test]
    async fn repair_flow_still_invalid_decision_maps_to_invalid_decision() {
        let schema_violation = r#"{"action":"REDUCE","market_id":32,"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"trim without size"}"#;
        let mock = MockProvider::canned(vec![
            "prose instead of JSON".to_string(),
            schema_violation.to_string(),
        ]);
        let engine = make_engine(mock, 2, "0.5");

        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("the repaired decision fails validation");

        match error {
            SentinelError::Brain(BrainError::InvalidDecision { detail }) => {
                assert!(detail.contains("amount"), "detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            engine.provider.calls().len(),
            2,
            "exactly one repair attempt"
        );
    }

    #[tokio::test]
    async fn provider_error_propagates_without_a_repair_call() {
        let mock = mock_with(vec![Err("upstream exploded".to_string())]);
        let engine = make_engine(mock, 2, "0.5");

        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("the provider fails");

        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(last, "upstream exploded");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            engine.provider.calls().len(),
            1,
            "provider errors are not repaired"
        );
    }

    #[tokio::test]
    async fn rate_limit_boundary_is_inclusive() {
        let mock = MockProvider::canned(vec![hold_json(32, "0.72"), hold_json(32, "0.72")]);
        let engine = make_engine(mock, 2, "0.5");
        let input = consult_input();
        let t0 = 1_000_000u64;

        engine.consult(&input, t0).await.expect("first consult ok");
        let stored = lock(&engine.last_consult_ms).get(&MarketId(32)).copied();
        assert_eq!(stored, Some(t0), "the successful consult records now_ms");

        let error = engine
            .consult(&input, t0)
            .await
            .expect_err("same ms blocked");
        match error {
            SentinelError::Brain(BrainError::RateLimited {
                market_id,
                remaining_ms,
            }) => {
                assert_eq!(market_id, 32);
                assert_eq!(remaining_ms, 2000);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let error = engine
            .consult(&input, t0 + 1999)
            .await
            .expect_err("one ms before the interval is blocked");
        match error {
            SentinelError::Brain(BrainError::RateLimited {
                market_id,
                remaining_ms,
            }) => {
                assert_eq!(market_id, 32);
                assert_eq!(remaining_ms, 1, "1999 of 2000 ms elapsed");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        engine
            .consult(&input, t0 + 2000)
            .await
            .expect("exactly at the interval is allowed");
        let stored = lock(&engine.last_consult_ms).get(&MarketId(32)).copied();
        assert_eq!(stored, Some(t0 + 2000));
        assert_eq!(
            engine.provider.calls().len(),
            2,
            "only the two admitted consults reached the provider"
        );
    }

    #[tokio::test]
    async fn rate_limit_is_per_market() {
        let mock = MockProvider::canned(vec![
            hold_json(32, "0.72"),
            hold_json(20, "0.72"),
            hold_json(20, "0.72"),
        ]);
        let engine = make_engine(mock, 2, "0.5");
        let eth = consult_input();
        let mut btc = consult_input();
        btc.focus_market = MarketId(20);
        let t0 = 1_000_000u64;

        engine.consult(&eth, t0).await.expect("ETH consult ok");
        // BTC is independent: allowed one ms later although ETH is limited.
        engine
            .consult(&btc, t0 + 1)
            .await
            .expect("BTC is not blocked by ETH's limit");

        let error = engine
            .consult(&eth, t0 + 2)
            .await
            .expect_err("ETH is still limited");
        match error {
            SentinelError::Brain(BrainError::RateLimited {
                market_id,
                remaining_ms,
            }) => {
                assert_eq!(market_id, 32);
                assert_eq!(remaining_ms, 1998);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        engine
            .consult(&btc, t0 + 2001)
            .await
            .expect("BTC at its own interval boundary is allowed");
        assert_eq!(engine.provider.calls().len(), 3);
    }

    #[tokio::test]
    async fn floor_equal_is_kept() {
        let mock = MockProvider::canned(vec![hold_json(32, "0.5")]);
        let engine = make_engine(mock, 2, "0.5");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("consult ok");

        assert!(!outcome.downgraded, "confidence == floor is kept");
        assert_eq!(outcome.decision.action, DecisionAction::Hold);
        assert_eq!(outcome.decision.confidence, dec("0.5"));
    }

    #[tokio::test]
    async fn floor_below_downgrades_to_escalate() {
        let mock = MockProvider::canned(vec![hold_json(32, "0.49")]);
        let engine = make_engine(mock, 2, "0.5");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("consult ok");

        assert!(outcome.downgraded);
        assert_eq!(outcome.decision.action, DecisionAction::Escalate);
        assert_eq!(
            outcome.decision.confidence,
            dec("0.49"),
            "confidence is kept"
        );
        assert_eq!(
            outcome.decision.urgency,
            Urgency::Routine,
            "urgency is kept"
        );
        assert_eq!(outcome.decision.reason, "grounded in the snapshot");
    }

    #[tokio::test]
    async fn missing_focus_market_is_invalid_decision() {
        let mock = MockProvider::canned(vec![hold_json(32, "0.72")]);
        let engine = make_engine(mock, 2, "0.5");
        let mut input = consult_input();
        input.focus_market = MarketId(99);

        let error = engine
            .consult(&input, 1_000_000)
            .await
            .expect_err("there is no focus position");

        match error {
            SentinelError::Brain(BrainError::InvalidDecision { detail }) => {
                assert_eq!(detail, "focus market 99 not in snapshot");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            engine.provider.calls().is_empty(),
            "the provider is never called without a focus position"
        );
    }
}
