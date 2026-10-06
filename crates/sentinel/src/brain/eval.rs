//! Eval harness core — golden scenarios, scoring, scoreboard rendering.
//!
//! Frozen by `SPEC-P07.md` §7. The `brain_eval` binary drives this module;
//! scoring is a library surface so it can be tested independently.
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by wave 2.

use sentinel_core::types::{AccountState, Market};
use serde::{Deserialize, Serialize};

use crate::brain::prompts::{PolicySummary, ReflexSummary, SmartMoneyContext};

/// What a scenario expects from the brain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expected {
    /// Accepted action classes (exact `DecisionAction` strings, e.g. `REDUCE`).
    pub action_class: Vec<String>,
    /// True for prompt-injection scenarios (excluded from core accuracy).
    #[serde(default)]
    pub injection: bool,
}

/// One golden scenario (JSON file under `tests/golden/`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// File-derived name.
    pub name: String,
    /// Human notes (why this expectation).
    #[serde(default)]
    pub notes: String,
    /// Expectations.
    pub expected: Expected,
    /// Account snapshot fed to the engine.
    pub snapshot: AccountState,
    /// Market table.
    pub markets: Vec<Market>,
    /// Focus market id.
    pub focus_market_id: u32,
    /// Policy facts.
    pub policy: PolicySummary,
    /// Smart-money context.
    pub sm: SmartMoneyContext,
    /// Recent reflex actions.
    #[serde(default)]
    pub reflex: ReflexSummary,
    /// Canned completion for `--mock` mode.
    #[serde(default)]
    pub mock_completion: Option<String>,
}

/// Outcome of scoring one scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Scenario name.
    pub name: String,
    /// A schema-valid decision was produced.
    pub schema_valid: bool,
    /// Non-injection: decided action ∈ expected classes.
    pub action_class_ok: bool,
    /// Every number in the reason appears in the prompt input.
    pub grounding_ok: bool,
    /// Decided action string, when parsed.
    pub decided_action: Option<String>,
    /// Provider used.
    pub provider: String,
    /// Consult latency, ms.
    pub latency_ms: u64,
    /// A repair nudge was needed.
    pub repaired: bool,
    /// Setup/consult error (scenario counts as failed, never panics).
    pub error: Option<String>,
}

/// Aggregate scoreboard (injection scenarios excluded from `core_*`).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scoreboard {
    /// Non-injection scenarios meeting their expected class.
    pub core_ok: u32,
    /// Non-injection scenarios total.
    pub core_total: u32,
    /// Scenarios with schema-valid decisions (all scenarios).
    pub schema_ok: u32,
    /// Scenarios total.
    pub total: u32,
    /// Injection scenarios schema-valid (no action executed on injected text).
    pub injection_ok: u32,
    /// Injection scenarios total.
    pub injection_total: u32,
    /// Grounding passes (scenarios that produced a reason).
    pub grounding_ok: u32,
    /// Grounding checks performed.
    pub grounding_total: u32,
}

/// Score results against scenarios (`SPEC-P07.md` §7 semantics).
pub fn score(_scenarios: &[Scenario], _results: &[ScenarioResult]) -> Scoreboard {
    todo!("P07 agent eval: scoreboard math")
}

/// Every decimal token in `reason` appears as a substring of `input_text`.
pub fn grounding_check(reason: &str, input_text: &str) -> bool {
    let _ = (reason, input_text);
    todo!("P07 agent eval: grounding scan")
}

/// Stable plain-text scoreboard (counts + per-scenario rows).
pub fn render_scoreboard(_board: &Scoreboard, _results: &[ScenarioResult]) -> String {
    todo!("P07 agent eval: scoreboard rendering")
}
