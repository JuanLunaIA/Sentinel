//! SimReport + rendering (SPEC-P13 §6).
use rust_decimal::Decimal;
use sentinel_core::types::RiskTier;
use serde::{Deserialize, Serialize};

/// One action (or recorded skip) along the simulated timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimAction {
    /// Tick timestamp, ms.
    pub ts_ms: i64,
    /// Market.
    pub market_id: u32,
    /// Tier at decision time.
    pub tier: RiskTier,
    /// `REFLEX` | `STRATEGY`.
    pub source: String,
    /// Human-readable outcome detail (fill, verdict, skip reason).
    pub detail: String,
    /// Position size after the action.
    pub size_after: Decimal,
}

/// Per-scenario report (SPEC-P13 §6; fields frozen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimReport {
    /// Scenario id.
    pub scenario_id: String,
    /// Honesty label.
    pub label: String,
    /// Number of ticks evaluated.
    pub ticks: u64,
    /// Actions timeline.
    pub actions: Vec<SimAction>,
    /// Baseline liquidation count.
    pub baseline_liquidations: u32,
    /// Sentinel liquidation count.
    pub sentinel_liquidations: u32,
    /// Avoided liquidations.
    pub liquidations_avoided: u32,
    /// Baseline USD loss.
    pub baseline_loss_usd: Decimal,
    /// Sentinel USD loss.
    pub sentinel_loss_usd: Decimal,
    /// Saved (may be negative; SPEC-P13 §5).
    pub capital_saved_usd: Decimal,
    /// Simulated taker fees, USD.
    pub sim_fees_usd: Decimal,
    /// Reduces executed in no-liquidation scenarios.
    pub false_positive_reduces: u32,
    /// Must be 0 (policy is the gate).
    pub policy_violations: u32,
    /// Notes (ignored events, reconstructions).
    pub notes: Vec<String>,
}

/// Aggregate across scenarios (SPEC-P13 §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    /// Scenario count.
    pub scenarios: u32,
    /// Total notional represented, USD.
    pub total_notional_usd: Decimal,
    /// Baseline liquidations.
    pub baseline_liquidations: u32,
    /// Sentinel liquidations.
    pub sentinel_liquidations: u32,
    /// Total saved, USD.
    pub total_saved_usd: Decimal,
    /// Saved percentage.
    pub pct_saved: Decimal,
}

/// Aggregate the per-scenario reports.
pub fn aggregate(_reports: &[SimReport]) -> Aggregate {
    todo!("STUB: P13 wave implements aggregation")
}

/// Render the markdown report (frozen literal line per SPEC-P13 §6).
pub fn render_md(_reports: &[SimReport], _aggregate: &Aggregate) -> String {
    todo!("STUB: P13 wave implements md rendering")
}
