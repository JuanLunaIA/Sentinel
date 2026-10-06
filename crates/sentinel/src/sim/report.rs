//! SimReport + rendering (SPEC-P13 §6).
//!
//! Report shapes are frozen by SPEC-P13 §6. Everything except `metrics` is
//! deterministic for a given scenario: no wall clock, no map iteration order,
//! no timestamps. [`SimReport::canonical_json`] serializes with alphabetically
//! sorted keys so reruns compare byte-for-byte (metrics excluded, see
//! [`SimReport::determinism_view`]).
//!
//! One extension beyond the §6 field list is required by §6 itself:
//! `notional_usd` (the scenario's initial position notional) is the only way
//! the frozen aggregate `total_notional_usd` can be computed from
//! [`SimReport`] values alone.

use rust_decimal::Decimal;
use sentinel_core::order::{CloseSide, OrderRequest};
use sentinel_core::types::{PolicyVerdict, RiskTier};
use serde::{Deserialize, Serialize};

/// `source` value of actions produced by the deterministic reflex engine.
pub const SOURCE_REFLEX: &str = "REFLEX";
/// `source` value of actions produced by a replayed strategy decision.
pub const SOURCE_STRATEGY: &str = "STRATEGY";

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
    /// The reduce-only order that was submitted, when one was.
    pub order: Option<OrderRequest>,
    /// Policy gate verdict, when the policy engine was consulted.
    pub verdict: Option<PolicyVerdict>,
    /// Human-readable outcome detail (fill, verdict, skip reason).
    pub detail: String,
    /// Position size after the action.
    pub size_after: Decimal,
}

/// Latency percentiles of reflex evaluations, microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyPcts {
    /// 50th percentile, µs.
    pub p50_us: u64,
    /// 90th percentile, µs.
    pub p90_us: u64,
    /// 99th percentile, µs.
    pub p99_us: u64,
}

/// Report metrics, deliberately excluded from the determinism byte-compare
/// (SPEC-P13 §6). The `reflex_eval_eval` key name is kept verbatim from the
/// frozen spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimMetrics {
    /// `ReflexState::advance` evaluation latency percentiles, µs.
    pub reflex_eval_eval: LatencyPcts,
}

impl SimMetrics {
    /// Zeroed metrics — the determinism view of a report.
    pub fn zeroed() -> Self {
        Self {
            reflex_eval_eval: LatencyPcts {
                p50_us: 0,
                p90_us: 0,
                p99_us: 0,
            },
        }
    }
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
    /// Notional represented by the scenario's initial positions
    /// (`Σ |size| × entry`), USD. Needed by the frozen aggregate
    /// `total_notional_usd` (see the module docs).
    pub notional_usd: Decimal,
    /// Latency metrics (excluded from the determinism byte-compare).
    pub metrics: SimMetrics,
}

impl SimReport {
    /// The determinism view: everything verbatim except `metrics`, which is
    /// zeroed. Reruns of the same scenario must be byte-identical on this view
    /// (SPEC-P13 §6).
    pub fn determinism_view(&self) -> Self {
        let mut view = self.clone();
        view.metrics = SimMetrics::zeroed();
        view
    }

    /// Canonical JSON with alphabetically sorted keys (deterministic bytes).
    ///
    /// # Errors
    /// Propagates `serde_json` serialization errors (cannot occur for this
    /// report shape, but no `unwrap` is allowed in library code).
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&serde_json::to_value(self)?)
    }
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
    /// Saved percentage of the represented notional.
    pub pct_saved: Decimal,
}

impl Aggregate {
    /// Canonical JSON with alphabetically sorted keys (deterministic bytes).
    ///
    /// # Errors
    /// Propagates `serde_json` serialization errors.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&serde_json::to_value(self)?)
    }
}

/// Aggregate the per-scenario reports (SPEC-P13 §6).
pub fn aggregate(reports: &[SimReport]) -> Aggregate {
    let scenarios = u32::try_from(reports.len()).unwrap_or(u32::MAX);
    let total_notional_usd = reports
        .iter()
        .map(|report| report.notional_usd)
        .sum::<Decimal>();
    let baseline_liquidations = reports
        .iter()
        .map(|report| report.baseline_liquidations)
        .sum::<u32>();
    let sentinel_liquidations = reports
        .iter()
        .map(|report| report.sentinel_liquidations)
        .sum::<u32>();
    let total_saved_usd = reports
        .iter()
        .map(|report| report.capital_saved_usd)
        .sum::<Decimal>();
    let pct_saved = if total_notional_usd > Decimal::ZERO {
        (total_saved_usd / total_notional_usd * Decimal::ONE_HUNDRED).round_dp(2)
    } else {
        Decimal::ZERO
    };
    Aggregate {
        scenarios,
        total_notional_usd,
        baseline_liquidations,
        sentinel_liquidations,
        total_saved_usd,
        pct_saved,
    }
}

/// Render the markdown report (SPEC-P13 §6).
///
/// The first paragraph is the frozen literal aggregate line:
/// `"Across N scenarios representing $X notional, Sentinel preserved $Y (Z%);`
/// `baseline liquidations: A -> with Sentinel: B"`. Scenarios are rendered in
/// input order; nothing in the output depends on wall-clock time.
pub fn render_md(reports: &[SimReport], aggregate: &Aggregate) -> String {
    let mut md = String::new();
    md.push_str("# Sentinel backtest report\n\n");
    md.push_str(&format!(
        "Across {} scenarios representing ${} notional, Sentinel preserved ${} ({}%); \
         baseline liquidations: {} -> with Sentinel: {}\n\n",
        aggregate.scenarios,
        money(aggregate.total_notional_usd),
        money(aggregate.total_saved_usd),
        aggregate.pct_saved,
        aggregate.baseline_liquidations,
        aggregate.sentinel_liquidations,
    ));
    md.push_str("## Aggregate\n\n");
    md.push_str("| metric | value |\n|---|---|\n");
    md.push_str(&format!("| scenarios | {} |\n", aggregate.scenarios));
    md.push_str(&format!(
        "| total notional (USD) | {} |\n",
        money(aggregate.total_notional_usd)
    ));
    md.push_str(&format!(
        "| total saved (USD) | {} |\n",
        money(aggregate.total_saved_usd)
    ));
    md.push_str(&format!(
        "| saved (% of notional) | {} |\n",
        aggregate.pct_saved
    ));
    md.push_str(&format!(
        "| baseline liquidations | {} |\n",
        aggregate.baseline_liquidations
    ));
    md.push_str(&format!(
        "| sentinel liquidations | {} |\n",
        aggregate.sentinel_liquidations
    ));
    md.push_str("\n## Scenarios\n\n");
    for report in reports {
        render_scenario(&mut md, report);
    }
    md
}

/// Render one scenario section into `out`.
fn render_scenario(out: &mut String, report: &SimReport) {
    out.push_str(&format!(
        "### {} [{}]\n\n",
        report.scenario_id, report.label
    ));
    out.push_str(&format!("- ticks: {}\n", report.ticks));
    out.push_str(&format!("- actions: {}\n", report.actions.len()));
    out.push_str(&format!(
        "- baseline loss: ${} USD; sentinel loss: ${} USD; saved: ${} USD; \
         sim fees: ${} USD; scenario notional: ${} USD\n",
        money(report.baseline_loss_usd),
        money(report.sentinel_loss_usd),
        money(report.capital_saved_usd),
        money(report.sim_fees_usd),
        money(report.notional_usd),
    ));
    out.push_str(&format!(
        "- liquidations: baseline {}; sentinel {}; avoided {}\n",
        report.baseline_liquidations, report.sentinel_liquidations, report.liquidations_avoided
    ));
    out.push_str(&format!(
        "- false positive reduces: {}; policy violations: {}\n",
        report.false_positive_reduces, report.policy_violations
    ));
    if report.notes.is_empty() {
        out.push_str("- notes: none\n");
    } else {
        for note in &report.notes {
            out.push_str(&format!("- note: {note}\n"));
        }
    }
    if !report.actions.is_empty() {
        out.push('\n');
        out.push_str("| ts_ms | market | tier | source | detail |\n|---|---|---|---|---|\n");
        for action in &report.actions {
            out.push_str(&format!(
                "| {} | {} | {:?} | {} | {} |\n",
                action.ts_ms,
                action.market_id,
                action.tier,
                action.source,
                action_detail(action)
            ));
        }
    }
    out.push('\n');
}

/// Human-readable detail of one action, from its order/verdict/fill fields.
fn action_detail(action: &SimAction) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(verdict) = &action.verdict {
        parts.push(match verdict {
            PolicyVerdict::Allow => "allow".to_string(),
            PolicyVerdict::Deny { reason } => format!("deny: {reason}"),
            PolicyVerdict::NeedsApproval { reason } => format!("needs approval: {reason}"),
        });
    }
    if let Some(order) = &action.order {
        let side = match order.close {
            CloseSide::CloseLong => "close-long",
            CloseSide::CloseShort => "close-short",
        };
        parts.push(format!(
            "order {side} {} (slippage cap {} bps)",
            money(order.size),
            order.max_slippage_bps
        ));
    }
    parts.push(action.detail.clone());
    parts.join(" | ")
}

/// `Decimal` display trimmed of trailing zeros (report readability).
fn money(value: Decimal) -> String {
    value.normalize().to_string()
}

#[cfg(test)]
mod tests {
    use sentinel_core::types::PolicyVerdict;

    use super::*;

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn report(id: &str, label: &str, notional: Decimal, saved: Decimal) -> SimReport {
        SimReport {
            scenario_id: id.to_string(),
            label: label.to_string(),
            ticks: 3,
            actions: Vec::new(),
            baseline_liquidations: 1,
            sentinel_liquidations: 0,
            liquidations_avoided: 1,
            baseline_loss_usd: d("10"),
            sentinel_loss_usd: d("4"),
            capital_saved_usd: saved,
            sim_fees_usd: d("0.5"),
            false_positive_reduces: 0,
            policy_violations: 0,
            notes: Vec::new(),
            notional_usd: notional,
            metrics: SimMetrics::zeroed(),
        }
    }

    #[test]
    fn aggregate_sums_and_rounds_the_saved_percentage() {
        let reports = vec![
            report("a", "synthetic", d("100"), d("10")),
            report("b", "recorded", d("300"), d("-5")),
        ];
        let aggregate = aggregate(&reports);
        assert_eq!(aggregate.scenarios, 2);
        assert_eq!(aggregate.total_notional_usd, d("400"));
        assert_eq!(aggregate.baseline_liquidations, 2);
        assert_eq!(aggregate.sentinel_liquidations, 0);
        assert_eq!(aggregate.total_saved_usd, d("5"));
        // 5 / 400 × 100 = 1.25.
        assert_eq!(aggregate.pct_saved, d("1.25"));
    }

    #[test]
    fn render_md_contains_the_frozen_literal_aggregate_line_and_labels() {
        let reports = vec![SimReport {
            notes: vec!["ignored event 'funding' at 0 ms (unmodeled in v1.0)".to_string()],
            ..report("recon-1", "reconstructed", d("100"), d("10"))
        }];
        let aggregate = aggregate(&reports);
        let md = render_md(&reports, &aggregate);

        let frozen = "Across 1 scenarios representing $100 notional, Sentinel preserved \
                      $10 (10.00%); baseline liquidations: 1 -> with Sentinel: 0";
        assert!(md.contains(frozen), "frozen line missing from:\n{md}");
        assert!(
            md.contains("### recon-1 [reconstructed]"),
            "label not rendered"
        );
        assert!(md.contains("- note: ignored event 'funding' at 0 ms"));
        assert!(md.contains("| total notional (USD) | 100 |"));
    }

    #[test]
    fn render_md_renders_actions_deterministically() {
        let mut report = report("act-1", "synthetic", d("100"), d("10"));
        report.actions.push(SimAction {
            ts_ms: 1_000,
            market_id: 32,
            tier: RiskTier::Red,
            source: SOURCE_REFLEX.to_string(),
            order: Some(OrderRequest {
                market_id: sentinel_core::types::MarketId(32),
                close: CloseSide::CloseLong,
                size: d("0.5"),
                order_type: sentinel_core::order::OrderType::Market,
                max_slippage_bps: 10,
                size_decimals: 3,
            }),
            verdict: Some(PolicyVerdict::Allow),
            detail: "red tier: first-breach reduce: filled 0.5 @ 98.901".to_string(),
            size_after: d("0.5"),
        });
        let md = render_md(
            std::slice::from_ref(&report),
            &aggregate(std::slice::from_ref(&report)),
        );
        assert!(
            md.contains(
                "| 1000 | 32 | Red | REFLEX | allow | order close-long 0.5 \
                 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.5 @ 98.901 |"
            ),
            "action row mismatch:\n{md}"
        );
        // Same input → byte-identical md.
        assert_eq!(
            md,
            render_md(
                std::slice::from_ref(&report),
                &aggregate(std::slice::from_ref(&report))
            )
        );
    }

    #[test]
    fn canonical_json_sorts_keys_and_determinism_view_zeroes_metrics() {
        let mut report = report("json-1", "synthetic", d("100"), d("10"));
        report.metrics = SimMetrics {
            reflex_eval_eval: LatencyPcts {
                p50_us: 7,
                p90_us: 9,
                p99_us: 11,
            },
        };
        let json = report.canonical_json().expect("canonical json");
        assert!(
            json.starts_with("{\"actions\":"),
            "keys must be sorted: {json}"
        );
        assert!(json.contains("\"reflex_eval_eval\":{\"p50_us\":7"));

        let view = report.determinism_view();
        assert_eq!(view.metrics, SimMetrics::zeroed());
        assert_eq!(view.actions, report.actions);
        assert_eq!(
            view.canonical_json().expect("view json"),
            report
                .determinism_view()
                .canonical_json()
                .expect("view json")
        );

        let aggregate = aggregate(std::slice::from_ref(&report));
        let aggregate_json = aggregate.canonical_json().expect("aggregate json");
        assert!(
            aggregate_json.starts_with("{\"baseline_liquidations\":"),
            "{aggregate_json}"
        );
    }

    #[test]
    fn empty_reports_aggregate_to_zero() {
        let aggregate = aggregate(&[]);
        assert_eq!(aggregate.scenarios, 0);
        assert_eq!(aggregate.total_notional_usd, Decimal::ZERO);
        assert_eq!(aggregate.total_saved_usd, Decimal::ZERO);
        assert_eq!(aggregate.pct_saved, Decimal::ZERO, "no divide by zero");
    }
}
