//! Policy gate — pure evaluation of intents against configured limits.
//!
//! The gate sits between intent producers (reflex engine, strategy brain,
//! manual commands) and executors: every intent passes through
//! [`PolicyEngine::evaluate`] before execution (P00 invariant #1).
//!
//! Check precedence, exact reasons, and the deliberate REFLEX+Red approval
//! asymmetry are frozen in `SPEC-P05.md` §3.
//!
//! **Skeleton status (P05):** interfaces frozen; implemented by the P05 wave.

use rust_decimal::Decimal;

use crate::types::{AccountState, Intent, MarketId, PolicyVerdict, RiskTier};

/// Where an intent came from (affects only the approval asymmetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// Deterministic reflex engine.
    Reflex,
    /// LLM strategy brain (P07+).
    Strategy,
    /// Human command via bot/API (P11+).
    Manual,
}

/// Per-call context: which position an intent targets and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyContext {
    /// Producer of the intent.
    pub source: PolicySource,
    /// Current risk tier of the targeted position.
    pub tier: RiskTier,
    /// Market the intent acts on.
    pub market_id: MarketId,
}

/// Executed-action counter for the current day (caller-maintained).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DayState {
    /// Actions already executed today.
    pub actions_today: u32,
}

/// Static policy limits (mapped from the application configuration).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyConfig {
    /// Markets the engine may act on.
    pub market_allowlist: Vec<MarketId>,
    /// Per-action notional ceiling, USD.
    pub max_order_size_usd: Decimal,
    /// Daily action cap.
    pub max_daily_actions: u32,
    /// Notional above which human approval is required, USD.
    pub require_approval_above_usd: Decimal,
    /// Kill switch: when `true`, every intent is denied.
    pub kill_switch: bool,
}

/// The policy gate (pure associated function; no state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyEngine;

impl PolicyEngine {
    /// Evaluate an intent against the configured limits.
    ///
    /// Returns `Allow`, `Deny { reason }` or `NeedsApproval { reason }`;
    /// precedence and messages are normative in `SPEC-P05.md` §3.
    ///
    /// **STUB — implemented by the P05 wave.**
    pub fn evaluate(
        intent: &Intent,
        account: &AccountState,
        cfg: &PolicyConfig,
        day: &DayState,
        ctx: &PolicyContext,
    ) -> PolicyVerdict {
        let _ = (intent, account, cfg, day, ctx);
        todo!("P05 agent policy: precedence per SPEC-P05 §3")
    }
}
