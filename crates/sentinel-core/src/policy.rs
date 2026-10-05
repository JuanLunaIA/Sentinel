//! Policy gate — pure evaluation of intents against configured limits.
//!
//! The gate sits between intent producers (reflex engine, strategy brain) and
//! executors. Every intent, regardless of origin, passes through
//! [`evaluate`] before execution (P00 invariant #1: the LLM never bypasses
//! the policy gate).
//!
//! **STUB in P02** — P05 implements the rule set: market allowlist,
//! per-action notional caps, daily action counts, approval queue above a USD
//! threshold (with the documented REFLEX+Red asymmetry), reduce-only bias as a
//! hard invariant, and the kill switch.

use rust_decimal::Decimal;

use crate::types::{AccountState, Intent, PolicyVerdict};

/// Static policy limits, mapped 1:1 from the application configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolicyLimits {
    /// Per-action notional ceiling in USD (0 = no cap configured).
    pub max_order_size_usd: Decimal,
    /// Maximum actions per rolling day.
    pub max_daily_actions: u32,
    /// Notional above which human approval is required, in USD.
    pub require_approval_above_usd: Decimal,
    /// Kill switch: when `true`, every intent is denied.
    pub kill_switch: bool,
}

/// Evaluate an intent against policy limits.
///
/// `actions_today` is the count of already-executed actions for the current
/// day (day boundary tracked by the caller).
///
/// **STUB — implemented in P05.**
pub fn evaluate(
    intent: &Intent,
    account: &AccountState,
    limits: &PolicyLimits,
    actions_today: u32,
) -> PolicyVerdict {
    let _ = (intent, account, limits, actions_today);
    todo!("P05: full rule set")
}
