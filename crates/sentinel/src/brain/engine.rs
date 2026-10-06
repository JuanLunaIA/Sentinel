//! Strategy engine — rate-limited, floor-gated LLM consults.
//!
//! Frozen by `SPEC-P07.md` §6. All timestamps are caller-supplied (`now_ms`)
//! so the engine is fully testable offline. Provider chains/circuit breakers
//! wrap this in P08 (`ProviderChain` implements [`Provider`]).
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by wave 2.

use std::collections::HashMap;
use std::sync::Mutex;

use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Decision, Market, MarketId};

use crate::brain::prompts::{PolicySummary, ReflexSummary, SmartMoneyContext};
use crate::brain::providers::Provider;
use crate::error::Result;

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
#[allow(dead_code)] // stub fields; consumed by the P07 wave 2
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
    /// **STUB — implemented by the P07 wave 2.**
    pub async fn consult(&self, input: &ConsultInput, now_ms: u64) -> Result<ConsultOutcome> {
        let _ = (input, now_ms);
        todo!("P07 agent engine: rate limit, repair, floor, audit log")
    }
}
