//! Strategy engine — rate-limited, floor-gated LLM consults with a resilient
//! provider chain.
//!
//! Frozen by `SPEC-P07.md` §6 (single-provider consult) and `SPEC-P08.md` §3
//! (provider + optional fallback, per-slot circuit breakers, consult/token
//! budget guard). All timestamps are caller-supplied (`now_ms`) so the engine
//! is fully testable offline:
//!
//! 1. per-market rate limit;
//! 2. budget guard — a synthetic `ESCALATE` once a sliding window is
//!    exhausted, before any provider call;
//! 3. primary attempt — forced-fail check, breaker gate, `complete()`, one
//!    repair nudge on a parse failure;
//! 4. fallback attempt (the same flow) when configured and the primary failed;
//! 5. the confidence floor, then the structured audit-style log.
//!
//! With no fallback configured the engine is behaviourally identical to P07
//! whenever the budget guard admits the consult (error shapes and all).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Decision, DecisionAction, Market, MarketId, Urgency};

use crate::brain::chain::{CircuitBreaker, NoProvider, breaker_pair};
use crate::brain::parser::{ParseError, parse_decision, repair_nudge};
use crate::brain::prompts::{
    PROMPT_VERSION, PolicySummary, PromptInput, ReflexSummary, SmartMoneyContext, system_prompt,
    user_prompt,
};
use crate::brain::providers::{Provider, RawCompletion};
use crate::error::{BrainError, Result, SentinelError};

/// Sliding hour window of the consult budget, ms (`SPEC-P08.md` §3 step 2).
const HOUR_MS: u64 = 3_600_000;
/// Sliding day window of the token budget, ms (`SPEC-P08.md` §3 step 2).
const DAY_MS: u64 = 86_400_000;
/// Consecutive failures that open a slot's breaker (`SPEC-P08.md` §3).
const BREAKER_THRESHOLD: u32 = 3;
/// Breaker open window, ms (`SPEC-P08.md` §3).
const BREAKER_OPEN_MS: u64 = 300_000;
/// Failure class: the slot was forced to fail by exact name; no call made.
const CLASS_FORCED: &str = "forced";
/// Failure class: the slot's breaker is open; no call made.
const CLASS_CIRCUIT_OPEN: &str = "circuit_open";
/// Failure class: unparseable output after the one repair nudge.
const CLASS_INVALID_OUTPUT: &str = "invalid_output";
/// Failure class: the provider call failed with an HTTP response.
const CLASS_HTTP: &str = "http";
/// Failure class: the provider call failed at the transport layer.
const CLASS_TRANSPORT: &str = "transport";

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
    /// Provider that produced it (`budget` for synthetic outcomes).
    pub provider_used: String,
    /// Primary provider name when the winning call came from the fallback
    /// slot; `None` on single-provider (P07) and budget paths.
    pub failover_from: Option<String>,
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
pub struct StrategyEngine<P: Provider, F: Provider = NoProvider> {
    /// Primary backend.
    provider: P,
    /// Optional fallback attempted when the primary fails (`SPEC-P08.md` §3).
    fallback: Option<F>,
    /// One breaker per slot, `[primary, fallback]`.
    breakers: Mutex<[CircuitBreaker; 2]>,
    /// Provider name forced to fail (`with_forced_failure`; no call made).
    forced_fail: Option<String>,
    /// Per-market minimum interval between consults, seconds.
    min_interval_secs: u64,
    /// Decisions below this confidence downgrade to ESCALATE.
    confidence_floor: Decimal,
    /// Last consult time per market (caller clock domain).
    last_consult_ms: Mutex<HashMap<MarketId, u64>>,
    /// Sliding hour window: one mark per admitted consult, ms.
    hour_marks: Mutex<VecDeque<u64>>,
    /// Sliding day window: `(mark_ms, estimated_tokens)` per admitted consult.
    day_marks: Mutex<VecDeque<(u64, u64)>>,
    /// Consults admitted per sliding hour before the budget guard refuses.
    max_consults_per_hour: u32,
    /// Estimated tokens per sliding day before the budget guard refuses.
    max_tokens_per_day: u64,
}

impl<P: Provider> StrategyEngine<P> {
    /// Build an engine with no fallback, breakers `(3, 300_000)` and the
    /// default budget (`30` consults/hour, `200_000` tokens/day).
    pub fn new(provider: P, min_interval_secs: u64, confidence_floor: Decimal) -> Self {
        Self {
            provider,
            fallback: None,
            breakers: breaker_pair(BREAKER_THRESHOLD, BREAKER_OPEN_MS),
            forced_fail: None,
            min_interval_secs,
            confidence_floor,
            last_consult_ms: Mutex::new(HashMap::new()),
            hour_marks: Mutex::new(VecDeque::new()),
            day_marks: Mutex::new(VecDeque::new()),
            max_consults_per_hour: 30,
            max_tokens_per_day: 200_000,
        }
    }
}

impl<P: Provider, F: Provider> StrategyEngine<P, F> {
    /// Add a fallback provider, attempted when the primary fails
    /// (`SPEC-P08.md` §3 step 4); the existing breaker/budget state is kept.
    pub fn with_fallback<F2: Provider>(self, fallback: F2) -> StrategyEngine<P, F2> {
        let StrategyEngine {
            provider,
            breakers,
            forced_fail,
            min_interval_secs,
            confidence_floor,
            last_consult_ms,
            hour_marks,
            day_marks,
            max_consults_per_hour,
            max_tokens_per_day,
            ..
        } = self;
        StrategyEngine {
            provider,
            fallback: Some(fallback),
            breakers,
            forced_fail,
            min_interval_secs,
            confidence_floor,
            last_consult_ms,
            hour_marks,
            day_marks,
            max_consults_per_hour,
            max_tokens_per_day,
        }
    }

    /// Force-fail the provider whose [`Provider::name`] equals `name` — an
    /// exact match short-circuits before any call (`FORCE_PROVIDER_FAIL` in
    /// the eval harness).
    pub fn with_forced_failure(mut self, name: impl Into<String>) -> Self {
        self.forced_fail = Some(name.into());
        self
    }

    /// Override the budget guard limits (`SPEC-P08.md` §3 step 2).
    pub fn with_budget(mut self, max_consults_per_hour: u32, max_tokens_per_day: u64) -> Self {
        self.max_consults_per_hour = max_consults_per_hour;
        self.max_tokens_per_day = max_tokens_per_day;
        self
    }
}

impl<P: Provider, F: Provider> StrategyEngine<P, F> {
    /// Run one consult (rate limit → budget guard → primary attempt →
    /// fallback attempt → floor → audit log); see `SPEC-P08.md` §3 for the
    /// exact sequence.
    ///
    /// With no fallback configured this preserves `SPEC-P07.md` §6 semantics
    /// exactly: the provider's own errors are returned unchanged.
    ///
    /// # Errors
    /// - [`BrainError::RateLimited`] when the focus market was consulted less
    ///   than `min_interval_secs` ago;
    /// - [`BrainError::InvalidDecision`] when the focus market has no position
    ///   in the snapshot, or when the repaired completion still fails schema
    ///   validation;
    /// - [`BrainError::InvalidJson`] when neither the completion nor its one
    ///   repair carried a parseable decision object;
    /// - the slot's own error when a consult is refused without a call
    ///   (forced-fail / open breaker) and no fallback is configured;
    /// - the primary provider's error unchanged when it fails and no
    ///   fallback is configured;
    /// - [`BrainError::AllProvidersFailed`] when both slots fail.
    pub async fn consult(&self, input: &ConsultInput, now_ms: u64) -> Result<ConsultOutcome> {
        self.check_rate_limit(input.focus_market, now_ms)?;

        // Step 2: budget guard, before any provider call. The attempt is
        // counted (hour) and the rate slot recorded either way.
        let over_budget = self.budget_exhausted(now_ms);
        lock(&self.hour_marks).push_back(now_ms);
        if over_budget {
            lock(&self.day_marks).push_back((now_ms, 0));
            lock(&self.last_consult_ms).insert(input.focus_market, now_ms);
            tracing::warn!(
                market_id = input.focus_market.0,
                "strategy budget exceeded — human review required"
            );
            return Ok(self.budget_outcome(input.focus_market));
        }

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

        let primary_name = self.provider.name();
        let primary = attempt_provider(
            &self.provider,
            self.forced_fail.as_deref(),
            &self.breakers,
            0,
            system,
            &user,
            &allowed_markets,
            now_ms,
        )
        .await;

        let (provider_name, raw, mut decision, repaired, failover_from, est_tokens) = match primary
        {
            Attempt::Success {
                raw,
                decision,
                repaired,
                est_tokens,
            } => (primary_name, raw, decision, repaired, None, est_tokens),
            Attempt::Failure { class, error } => {
                let Some(fallback) = &self.fallback else {
                    // Step 5: the primary's own error, unchanged (P07 corpus
                    // compatibility).
                    lock(&self.day_marks).push_back((now_ms, 0));
                    return Err(error);
                };
                let fallback_name = fallback.name();
                tracing::warn!(
                    provider = primary_name,
                    failover_to = fallback_name,
                    error_class = class,
                    "provider failed; failing over"
                );
                match attempt_provider(
                    fallback,
                    self.forced_fail.as_deref(),
                    &self.breakers,
                    1,
                    system,
                    &user,
                    &allowed_markets,
                    now_ms,
                )
                .await
                {
                    Attempt::Success {
                        raw,
                        decision,
                        repaired,
                        est_tokens,
                    } => (
                        fallback_name,
                        raw,
                        decision,
                        repaired,
                        Some(primary_name.to_string()),
                        est_tokens,
                    ),
                    Attempt::Failure {
                        class: fallback_class,
                        error: _,
                    } => {
                        lock(&self.day_marks).push_back((now_ms, 0));
                        return Err(BrainError::AllProvidersFailed {
                            last: format!(
                                "{primary_name}: {class}; {fallback_name}: {fallback_class}"
                            ),
                        }
                        .into());
                    }
                }
            }
        };

        // Step 7: account the winning call's tokens against the day budget.
        lock(&self.day_marks).push_back((now_ms, est_tokens));

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
        log_consult(provider_name, &raw, &decision, repaired, downgraded);

        Ok(ConsultOutcome {
            decision,
            provider_used: provider_name.to_string(),
            failover_from,
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

    /// Budget guard (`SPEC-P08.md` §3 step 2): prunes both sliding windows at
    /// `now_ms` and reports whether either limit is reached.
    fn budget_exhausted(&self, now_ms: u64) -> bool {
        let consults = {
            let mut hour = lock(&self.hour_marks);
            hour.retain(|&mark| now_ms.saturating_sub(mark) < HOUR_MS);
            hour.len()
        };
        let tokens = {
            let mut day = lock(&self.day_marks);
            day.retain(|&(mark, _)| now_ms.saturating_sub(mark) < DAY_MS);
            day.iter().map(|&(_, est)| est).sum::<u64>()
        };
        consults >= self.max_consults_per_hour as usize || tokens >= self.max_tokens_per_day
    }

    /// Synthetic `ESCALATE` returned when the budget guard refuses a consult
    /// (`SPEC-P08.md` §3 step 2).
    fn budget_outcome(&self, focus: MarketId) -> ConsultOutcome {
        ConsultOutcome {
            decision: Decision {
                action: DecisionAction::Escalate,
                market_id: focus.0,
                amount: None,
                confidence: Decimal::ZERO,
                urgency: Urgency::Critical,
                reason: "strategy budget exceeded — human review required".to_string(),
            },
            provider_used: "budget".to_string(),
            failover_from: None,
            prompt_version: PROMPT_VERSION,
            latency_ms: 0,
            prompt_tokens: None,
            completion_tokens: None,
            repaired: false,
            downgraded: false,
        }
    }
}

/// Outcome of one provider attempt (`SPEC-P08.md` §3 steps 3–4).
enum Attempt {
    /// A validated decision; `est_tokens` accounts the winning call.
    Success {
        /// The winning completion.
        raw: RawCompletion,
        /// The validated decision it carried.
        decision: Decision,
        /// True when the one repair nudge produced the winning call.
        repaired: bool,
        /// Token estimate of the winning call (day budget).
        est_tokens: u64,
    },
    /// The attempt failed; `error` is the exact (P07-shaped) error returned
    /// when no fallback is configured.
    Failure {
        /// Failure class (see the `CLASS_*` constants).
        class: &'static str,
        /// Error to surface when this is the last slot.
        error: SentinelError,
    },
}

/// One provider attempt (`SPEC-P08.md` §3 steps 3–4): forced-fail check,
/// breaker gate, one completion, at most one repair, and the slot's breaker
/// bookkeeping. No lock is held across an `await`.
#[allow(clippy::too_many_arguments)] // attempt context is explicit by design (SPEC-P08 §3)
async fn attempt_provider<Pr: Provider>(
    provider: &Pr,
    forced_fail: Option<&str>,
    breakers: &Mutex<[CircuitBreaker; 2]>,
    slot: usize,
    system: &str,
    user: &str,
    allowed_markets: &[MarketId],
    now_ms: u64,
) -> Attempt {
    let name = provider.name();

    if forced_fail == Some(name) {
        // Forced failures skip the call but still feed the slot's breaker
        // (`SPEC-P08.md` §8).
        lock(breakers)[slot].record_failure(now_ms);
        return Attempt::Failure {
            class: CLASS_FORCED,
            error: slot_error(name, CLASS_FORCED),
        };
    }

    {
        let breaker = lock(breakers);
        if !breaker[slot].is_available(now_ms) {
            // A skipped call does not feed the breaker: the open window
            // already governs when the next probe is allowed.
            return Attempt::Failure {
                class: CLASS_CIRCUIT_OPEN,
                error: slot_error(name, CLASS_CIRCUIT_OPEN),
            };
        }
    }

    let first = match provider.complete(system, user).await {
        Ok(raw) => raw,
        Err(error) => {
            lock(breakers)[slot].record_failure(now_ms);
            let class = classify_error(&error);
            return Attempt::Failure { class, error };
        }
    };

    match parse_decision(&first.text, allowed_markets) {
        Ok(decision) => {
            lock(breakers)[slot].record_success();
            Attempt::Success {
                est_tokens: estimated_tokens(system, user, &first),
                raw: first,
                decision,
                repaired: false,
            }
        }
        Err(_) => {
            let nudge = repair_nudge(&first.text);
            let second = match provider.complete(system, &nudge).await {
                Ok(raw) => raw,
                Err(error) => {
                    lock(breakers)[slot].record_failure(now_ms);
                    let class = classify_error(&error);
                    return Attempt::Failure { class, error };
                }
            };
            match parse_decision(&second.text, allowed_markets) {
                Ok(decision) => {
                    lock(breakers)[slot].record_success();
                    Attempt::Success {
                        est_tokens: estimated_tokens(system, &nudge, &second),
                        raw: second,
                        decision,
                        repaired: true,
                    }
                }
                Err(error) => {
                    lock(breakers)[slot].record_failure(now_ms);
                    Attempt::Failure {
                        class: CLASS_INVALID_OUTPUT,
                        error: map_parse_error(name, error),
                    }
                }
            }
        }
    }
}

/// Synthetic error for a slot that could not run (forced-fail / open
/// breaker).
fn slot_error(name: &str, class: &str) -> SentinelError {
    BrainError::AllProvidersFailed {
        last: format!("{name}: {class}"),
    }
    .into()
}

/// Failure class of a `complete()` error: `transport` when no HTTP response
/// was received (`status: 0`), `http` otherwise (`SPEC-P08.md` §3 step 3).
fn classify_error(error: &SentinelError) -> &'static str {
    match error {
        SentinelError::Brain(BrainError::ProviderHttp { status: 0, .. }) => CLASS_TRANSPORT,
        _ => CLASS_HTTP,
    }
}

/// Token estimate of a call (`SPEC-P08.md` §3 step 7): reported usage when
/// present, else ~4 bytes per token.
fn estimated_tokens(system: &str, user: &str, raw: &RawCompletion) -> u64 {
    let prompt = match raw.prompt_tokens {
        Some(tokens) => u64::from(tokens),
        None => (system.len() + user.len()) as u64 / 4,
    };
    let completion = match raw.completion_tokens {
        Some(tokens) => u64::from(tokens),
        None => raw.text.len() as u64 / 4,
    };
    prompt + completion
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
    provider: &str,
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
        provider = provider,
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
    use crate::brain::chain::BreakerState;
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
        mock_named_with("mock", responses)
    }

    /// As [`mock_with`], reporting `name` from [`Provider::name`].
    fn mock_named_with(
        name: &'static str,
        responses: Vec<std::result::Result<RawCompletion, String>>,
    ) -> MockProvider {
        MockProvider {
            name,
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A bare completion (no usage metadata, zero latency).
    fn raw_completion(text: impl Into<String>) -> RawCompletion {
        RawCompletion {
            text: text.into(),
            provider: "mock".to_string(),
            model: "mock".to_string(),
            prompt_tokens: None,
            completion_tokens: None,
            latency_ms: 0,
        }
    }

    /// A completion carrying the given usage metadata.
    fn raw_with_usage(
        text: impl Into<String>,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> RawCompletion {
        RawCompletion {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            ..raw_completion(text)
        }
    }

    /// A two-slot chain engine (`qwen` primary, `kimi` fallback), 2 s
    /// interval, floor 0.5.
    fn chain_engine(
        primary: MockProvider,
        fallback: MockProvider,
    ) -> StrategyEngine<MockProvider, MockProvider> {
        StrategyEngine::new(primary, 2, dec("0.5")).with_fallback(fallback)
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
        assert_eq!(outcome.failover_from, None);
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

    #[tokio::test]
    async fn failover_on_http_failure_uses_the_fallback() {
        let engine = chain_engine(
            mock_named_with("qwen", vec![Err("qwen exploded".to_string())]),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72")]),
        );

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("the fallback succeeds");

        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from, Some("qwen".to_string()));
        assert_eq!(outcome.decision.action, DecisionAction::Hold);
        assert_eq!(outcome.decision.confidence, dec("0.72"));
        assert!(!outcome.repaired, "the fallback succeeded on its first try");

        assert_eq!(engine.provider.calls().len(), 1, "primary tried once");
        let fallback = engine.fallback.as_ref().expect("fallback is configured");
        assert_eq!(fallback.calls().len(), 1, "fallback tried once");
    }

    #[tokio::test]
    async fn failover_on_unparseable_output_after_repair() {
        let engine = chain_engine(
            MockProvider::canned_named(
                "qwen",
                vec![
                    "prose instead of JSON".to_string(),
                    "still refusing".to_string(),
                ],
            ),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72")]),
        );

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("the fallback succeeds");

        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from, Some("qwen".to_string()));
        assert!(
            !outcome.repaired,
            "the fallback gets a fresh attempt, not the nudge"
        );

        let primary_calls = engine.provider.calls();
        assert_eq!(primary_calls.len(), 2, "primary: initial + one repair");
        let fallback_calls = engine
            .fallback
            .as_ref()
            .expect("fallback is configured")
            .calls();
        assert_eq!(fallback_calls.len(), 1, "fallback: one fresh call");
        assert_eq!(
            fallback_calls[0], primary_calls[0],
            "the fallback restarts from the original (system, user) prompt"
        );
    }

    #[tokio::test]
    async fn failover_on_schema_invalid_output_after_repair() {
        let schema_violation = r#"{"action":"REDUCE","market_id":32,"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"trim without size"}"#;
        let engine = chain_engine(
            MockProvider::canned_named(
                "qwen",
                vec![schema_violation.to_string(), schema_violation.to_string()],
            ),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72")]),
        );

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("the fallback succeeds");

        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from, Some("qwen".to_string()));
        assert_eq!(engine.provider.calls().len(), 2);
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn no_fallback_keeps_p07_error_shapes_exactly() {
        // Unparseable output after the repair ⇒ InvalidJson{provider}.
        let mock = MockProvider::canned(vec!["no JSON".to_string(), "still none".to_string()]);
        let engine = make_engine(mock, 2, "0.5");
        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("unparseable twice");
        match error {
            SentinelError::Brain(BrainError::InvalidJson { provider }) => {
                assert_eq!(provider, "mock");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // Schema-invalid output after the repair ⇒ InvalidDecision.
        let schema_violation = r#"{"action":"REDUCE","market_id":32,"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"trim without size"}"#;
        let mock = MockProvider::canned(vec![
            "prose instead of JSON".to_string(),
            schema_violation.to_string(),
        ]);
        let engine = make_engine(mock, 2, "0.5");
        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("schema-invalid after repair");
        match error {
            SentinelError::Brain(BrainError::InvalidDecision { detail }) => {
                assert!(detail.contains("amount"), "detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // A provider error propagates with its exact value.
        let mock = mock_with(vec![Err("upstream exploded".to_string())]);
        let engine = make_engine(mock, 2, "0.5");
        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("provider error propagates");
        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(last, "upstream exploded");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn forced_failure_skips_the_primary_without_calling_it() {
        let engine = chain_engine(
            MockProvider::canned_named("qwen", vec![hold_json(32, "0.99")]),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72"); 3]),
        )
        .with_forced_failure("qwen");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("the fallback succeeds");
        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from, Some("qwen".to_string()));
        assert!(
            engine.provider.calls().is_empty(),
            "the forced primary is never called"
        );

        // The forced failure still feeds the primary's breaker: three forced
        // consults open it (observable without peeking at internals).
        for step in 1..=2u64 {
            engine
                .consult(&consult_input(), 1_000_000 + step * 2_000)
                .await
                .expect("the fallback still succeeds");
        }
        assert!(engine.provider.calls().is_empty(), "still never called");
        assert_eq!(
            lock(&engine.breakers)[0].state(1_000_000 + 4_000),
            BreakerState::Open {
                until_ms: 1_000_000 + 4_000 + BREAKER_OPEN_MS
            },
            "forced failures are recorded on the primary's breaker"
        );
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn forced_failure_matches_exact_names_only() {
        let engine = chain_engine(
            MockProvider::canned_named("qwen", vec![hold_json(32, "0.72")]),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72")]),
        )
        .with_forced_failure("qwe");

        let outcome = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect("a near-miss name does not force-fail");

        assert_eq!(outcome.provider_used, "qwen");
        assert_eq!(outcome.failover_from, None);
        assert_eq!(engine.provider.calls().len(), 1);
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            0,
            "the fallback is never consulted"
        );
    }

    #[tokio::test]
    async fn all_providers_failed_reports_both_names_and_classes() {
        let engine = chain_engine(
            mock_named_with("qwen", vec![Err("qwen exploded".to_string())]),
            MockProvider::canned_named(
                "kimi",
                vec![
                    "prose instead of JSON".to_string(),
                    "still refusing".to_string(),
                ],
            ),
        );

        let error = engine
            .consult(&consult_input(), 1_000_000)
            .await
            .expect_err("both slots fail");

        match error {
            SentinelError::Brain(BrainError::AllProvidersFailed { last }) => {
                assert_eq!(last, "qwen: http; kimi: invalid_output");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(engine.provider.calls().len(), 1);
    }

    #[tokio::test]
    async fn breaker_opens_after_three_failures_and_the_next_consult_skips_the_primary() {
        let engine = chain_engine(
            mock_named_with(
                "qwen",
                vec![
                    Err("down 1".to_string()),
                    Err("down 2".to_string()),
                    Err("down 3".to_string()),
                    Ok(raw_completion(hold_json(32, "0.72"))),
                ],
            ),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72"); 4]),
        );

        let t0 = 1_000_000u64;
        for step in 0..3u64 {
            let outcome = engine
                .consult(&consult_input(), t0 + step * 2_000)
                .await
                .expect("the fallback carries each consult");
            assert_eq!(outcome.provider_used, "kimi");
            assert_eq!(outcome.failover_from.as_deref(), Some("qwen"));
        }
        assert_eq!(engine.provider.calls().len(), 3, "three primary strikes");
        assert_eq!(
            lock(&engine.breakers)[0].state(t0 + 4_000),
            BreakerState::Open {
                until_ms: t0 + 4_000 + BREAKER_OPEN_MS
            }
        );

        // The fourth consult skips the primary (`circuit_open`) and goes
        // straight to the fallback: the primary's call count stays frozen.
        let outcome = engine
            .consult(&consult_input(), t0 + 6_000)
            .await
            .expect("the fallback still carries the consult");
        assert_eq!(outcome.provider_used, "kimi");
        assert_eq!(outcome.failover_from.as_deref(), Some("qwen"));
        assert_eq!(
            engine.provider.calls().len(),
            3,
            "the open breaker froze the primary's calls"
        );
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            4
        );
    }

    #[tokio::test]
    async fn half_open_probe_recovers_the_primary_through_the_engine() {
        let engine = chain_engine(
            mock_named_with(
                "qwen",
                vec![
                    Err("down 1".to_string()),
                    Err("down 2".to_string()),
                    Err("down 3".to_string()),
                    Ok(raw_completion(hold_json(32, "0.72"))),
                ],
            ),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72"); 3]),
        );

        let t0 = 1_000_000u64;
        for step in 0..3u64 {
            engine
                .consult(&consult_input(), t0 + step * 2_000)
                .await
                .expect("the fallback carries each consult");
        }

        // `open_ms + 1` later the window has elapsed: the next consult probes
        // the primary again and its success closes the breaker.
        let probe_at = t0 + 4_000 + BREAKER_OPEN_MS + 1;
        let outcome = engine
            .consult(&consult_input(), probe_at)
            .await
            .expect("the half-open probe succeeds");

        assert_eq!(outcome.provider_used, "qwen", "the primary won again");
        assert_eq!(outcome.failover_from, None);
        assert_eq!(engine.provider.calls().len(), 4, "the probe called it");
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            3,
            "the fallback was not needed for the probe"
        );
        assert_eq!(
            lock(&engine.breakers)[0].state(probe_at),
            BreakerState::Closed,
            "a successful probe closes the breaker"
        );
    }

    #[tokio::test]
    async fn budget_hour_window_degrades_to_synthetic_and_slides() {
        let engine = chain_engine(
            MockProvider::canned_named("qwen", vec![hold_json(32, "0.72"); 3]),
            MockProvider::canned_named("kimi", vec![hold_json(32, "0.72")]),
        )
        .with_budget(2, 200_000);

        let t0 = 1_000_000u64;
        engine
            .consult(&consult_input(), t0)
            .await
            .expect("first admitted");
        engine
            .consult(&consult_input(), t0 + 2_000)
            .await
            .expect("second admitted");
        assert_eq!(engine.provider.calls().len(), 2);

        let outcome = engine
            .consult(&consult_input(), t0 + 4_000)
            .await
            .expect("the budget guard refuses the third consult");
        assert_eq!(outcome.provider_used, "budget");
        assert_eq!(outcome.failover_from, None);
        assert_eq!(outcome.decision.action, DecisionAction::Escalate);
        assert_eq!(outcome.decision.market_id, 32);
        assert_eq!(outcome.decision.amount, None);
        assert_eq!(outcome.decision.confidence, Decimal::ZERO);
        assert_eq!(outcome.decision.urgency, Urgency::Critical);
        assert_eq!(
            outcome.decision.reason,
            "strategy budget exceeded — human review required"
        );
        assert_eq!(outcome.prompt_version, PROMPT_VERSION);
        assert_eq!(outcome.latency_ms, 0);
        assert_eq!(outcome.prompt_tokens, None);
        assert_eq!(outcome.completion_tokens, None);
        assert!(!outcome.repaired);
        assert!(!outcome.downgraded);

        assert_eq!(
            engine.provider.calls().len(),
            2,
            "the synthetic outcome never reached the primary"
        );
        assert_eq!(
            engine
                .fallback
                .as_ref()
                .expect("fallback is configured")
                .calls()
                .len(),
            0,
            "nor the fallback"
        );
        assert_eq!(
            lock(&engine.hour_marks).len(),
            3,
            "the refused attempt is still counted"
        );
        assert_eq!(
            lock(&engine.last_consult_ms).get(&MarketId(32)).copied(),
            Some(t0 + 4_000),
            "the refused attempt still records the rate slot"
        );

        // Past an hour the window slides: the consult is admitted again.
        let later = t0 + 4_000 + HOUR_MS + 1;
        let outcome = engine
            .consult(&consult_input(), later)
            .await
            .expect("the window has slid");
        assert_eq!(outcome.provider_used, "qwen");
        assert_eq!(engine.provider.calls().len(), 3);
    }

    #[tokio::test]
    async fn daily_token_cap_degrades_to_synthetic() {
        let engine = StrategyEngine::new(
            mock_named_with(
                "qwen",
                vec![
                    Ok(raw_with_usage(hold_json(32, "0.72"), 20_000, 10_000)),
                    Ok(raw_with_usage(hold_json(32, "0.72"), 20_000, 10_000)),
                    Ok(raw_with_usage(hold_json(32, "0.72"), 20_000, 10_000)),
                ],
            ),
            2,
            dec("0.5"),
        )
        .with_budget(30, 50_000);

        let t0 = 1_000_000u64;
        engine
            .consult(&consult_input(), t0)
            .await
            .expect("first admitted");
        engine
            .consult(&consult_input(), t0 + 2_000)
            .await
            .expect("second admitted");

        let outcome = engine
            .consult(&consult_input(), t0 + 4_000)
            .await
            .expect("the token cap refuses the third consult");
        assert_eq!(outcome.provider_used, "budget");
        assert_eq!(outcome.decision.action, DecisionAction::Escalate);
        assert_eq!(
            outcome.decision.reason,
            "strategy budget exceeded — human review required"
        );
        assert_eq!(
            engine.provider.calls().len(),
            2,
            "the token cap stopped the third consult before any call"
        );
        let day_tokens: u64 = lock(&engine.day_marks).iter().map(|&(_, est)| est).sum();
        assert_eq!(
            day_tokens, 60_000,
            "two 30k calls were accounted; the synthetic one added nothing"
        );
    }
}
