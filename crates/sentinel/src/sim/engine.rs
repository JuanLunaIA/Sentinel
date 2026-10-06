//! Tick-by-tick engine (SPEC-P13 §4).
use crate::sim::report::SimReport;
use crate::sim::scenario::{Scenario, ScenarioError};

/// Run one scenario through the real decision path (reflex + policy +
/// optional recorded decisions) and produce the report.
///
/// # Errors
/// `ScenarioError` only for malformed inputs; policy/executor outcomes are
/// recorded inside the report, never fatal.
pub fn run(_scenario: &Scenario) -> Result<SimReport, ScenarioError> {
    todo!("STUB: P13 wave implements the sim engine")
}
