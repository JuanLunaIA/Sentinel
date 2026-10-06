//! Scenario format and loader (SPEC-P13 §3, normative).
use rust_decimal::Decimal;
use sentinel_core::types::{Decision, Market, Position};
use serde::{Deserialize, Serialize};

/// Honesty marker rendered verbatim in the report (SPEC-P13 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioLabel {
    /// Hand-authored stress scenario.
    Synthetic,
    /// Built from real recorded fixtures (P03).
    Recorded,
    /// Rebuilt from recorded testnet data + a consistent synthesized path.
    Reconstructed,
}

/// One price observation of the scenario path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    /// Logical timestamp, milliseconds (strictly increasing in the file).
    pub ts_ms: i64,
    /// Market the mark applies to.
    pub market_id: u32,
    /// Mark price at this tick.
    pub mark_price: Decimal,
}

/// Kinds accepted in `events` (v1.0 honors `feed_stale`; others are ignored
/// and counted in report notes — SPEC-P13 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Feed outage window (`until_ms` required).
    FeedStale,
    /// Accepted, ignored in v1.0.
    Funding,
    /// Accepted, ignored in v1.0.
    BigFill,
}

/// One scripted event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioEvent {
    /// Start timestamp, ms.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// End of the `feed_stale` window, ms.
    #[serde(default)]
    pub until_ms: Option<i64>,
    /// Free-form note surfaced in the report.
    #[serde(default)]
    pub note: Option<String>,
}

/// One recorded strategy decision replayed at a consult tick (SPEC-P13 §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionTraceEntry {
    /// Tick timestamp the decision applies to.
    pub at_ms: i64,
    /// Market the decision concerns.
    pub market_id: u32,
    /// Full Decision v3 document.
    pub decision: Decision,
}

/// A complete scenario (SPEC-P13 §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable id (file stem expected to match).
    pub id: String,
    /// Honesty label.
    pub label: ScenarioLabel,
    /// Human description.
    pub description: String,
    /// Free (unlocked) balance at scenario start, collateral units.
    pub start_free_balance: Decimal,
    /// Market definitions (full core `Market` documents).
    pub markets: Vec<Market>,
    /// Initial positions (full core `Position` documents).
    pub positions: Vec<Position>,
    /// Price path.
    pub price_path: Vec<PriceTick>,
    /// Scripted events.
    #[serde(default)]
    pub events: Vec<ScenarioEvent>,
    /// Reflex overrides (mapped onto `ReflexConfig`; defaults when absent).
    #[serde(default)]
    pub reflex: Option<serde_json::Value>,
    /// Policy overrides (mapped onto `PolicyConfig`; defaults when absent).
    #[serde(default)]
    pub policy: Option<serde_json::Value>,
    /// Optional recorded-decision trace.
    #[serde(default)]
    pub decision_trace: Option<Vec<DecisionTraceEntry>>,
}

/// Scenario loading/validation error (typed; SPEC-P13 §3).
#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    /// Filesystem error.
    #[error("scenario io: {0}")]
    Io(String),
    /// JSON/schema error.
    #[error("scenario json: {0}")]
    Json(String),
    /// Semantic validation error.
    #[error("scenario invalid: {0}")]
    Invalid(String),
}

/// Load + validate a scenario from disk (SPEC-P13 §3 validation rules).
///
/// # Errors
/// `ScenarioError` on io/json/validation failure.
pub fn load(_path: &std::path::Path) -> Result<Scenario, ScenarioError> {
    todo!("STUB: P13 wave implements scenario loading")
}
