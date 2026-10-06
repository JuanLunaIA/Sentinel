//! Prompt construction — versioned, grounded, budget-bounded.
//!
//! Frozen by `SPEC-P07.md` §5. The system text is static; every number the
//! model may cite lives in the user block (grounding rule).
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by wave 1.

use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Market, MarketId, Position};
use serde::{Deserialize, Serialize};

/// Prompt version logged with every consult and decision.
pub const PROMPT_VERSION: &str = "v3.0";

/// The static system prompt.
pub fn system_prompt() -> &'static str {
    todo!("P07 agent prompts")
}

/// Policy facts the model must respect (summary of the live gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySummary {
    /// Markets the engine may act on.
    pub market_allowlist: Vec<MarketId>,
    /// Per-action notional ceiling, USD.
    pub max_order_size_usd: Decimal,
    /// Notional above which human approval is required, USD.
    pub require_approval_above_usd: Decimal,
    /// Actions left under the daily cap.
    pub daily_actions_left: u32,
}

/// Smart-money enrichment (filled by the Nansen x402 client in P09).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartMoneyContext {
    /// Asset the context is about.
    pub asset: String,
    /// 24 h net flow, USD (positive = inflow).
    pub netflow_24h: Option<Decimal>,
    /// Holdings delta, USD.
    pub holdings_delta: Option<Decimal>,
    /// Long/short ratio from the leaderboard.
    pub long_short_ratio: Option<Decimal>,
    /// Top-trader net bias text.
    pub top_traders_net_bias: Option<String>,
    /// When the data was fetched (epoch ms).
    pub fetched_at_ms: Option<u64>,
    /// Total x402 cost of the context, USD.
    pub total_cost_usd: Option<Decimal>,
    /// Honesty note (e.g. cross-venue proxy wording).
    pub note: Option<String>,
}

impl SmartMoneyContext {
    /// Placeholder when no smart-money data is available.
    pub fn unavailable(asset: impl Into<String>) -> Self {
        let _ = asset;
        todo!("P07 agent prompts")
    }
}

/// What the deterministic reflex layer recently did (context for judgment).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReflexSummary {
    /// Recent reflex actions, newest last.
    pub actions: Vec<String>,
}

/// Everything the user prompt renders.
pub struct PromptInput<'a> {
    /// Full account snapshot.
    pub account: &'a AccountState,
    /// Market table.
    pub markets: &'a [Market],
    /// The position under consultation.
    pub focus: &'a Position,
    /// Policy facts.
    pub policy: &'a PolicySummary,
    /// Smart-money context (or [`SmartMoneyContext::unavailable`]).
    pub sm: &'a SmartMoneyContext,
    /// Recent reflex actions.
    pub reflex: &'a ReflexSummary,
    /// Event/consult timestamp (epoch ms).
    pub now_ms: u64,
}

/// Render the user prompt (≤ ~6000 chars for a 3-position account).
pub fn user_prompt(input: &PromptInput<'_>) -> String {
    let _ = input;
    todo!("P07 agent prompts")
}
