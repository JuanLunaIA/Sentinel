//! Do-nothing baseline (SPEC-P13 §5).
use crate::sim::scenario::Scenario;
use rust_decimal::Decimal;

/// Baseline outcome for one scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineOutcome {
    /// USD lost by doing nothing (collateral of liquidated positions, or
    /// end-of-path unrealized loss when nothing liquidates).
    pub loss_usd: Decimal,
    /// Number of positions that liquidate along the path.
    pub liquidations: u32,
}

/// Walk the scenario path with no actions (SPEC-P13 §5).
pub fn simulate(_scenario: &Scenario) -> BaselineOutcome {
    todo!("STUB: P13 wave implements the baseline walk")
}
