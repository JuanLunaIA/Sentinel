//! Tick-by-tick engine (SPEC-P13 §4).
//!
//! One scenario is replayed as a synchronous tick loop that calls the exact
//! production decision path:
//!
//! 1. each price-path entry updates the market's last-known mark, and every
//!    position's `mark_price` / `unrealized_pnl` are recomputed;
//! 2. a sentinel-side crossing check records an honest warning note when the
//!    defended position's mark crosses its liq price (the frozen §5 sentinel
//!    accounting is formula-only: realized + final unrealized + fees);
//! 3. every position is evaluated: `risk::tier` → `ReflexState::advance`
//!    (the same core function the pipeline calls) → `PolicyEngine::evaluate`
//!    → [`SimExecutor`](crate::sim::venue::SimExecutor) fills;
//! 4. recorded strategy decisions replay at consult points — (a) a Green→Yellow
//!    entry transition per market and (b) after each executed reflex action —
//!    gated with `PolicySource::Strategy`.
//!
//! No wall clock is read for any state transition (only the `metrics`
//! percentiles use `Instant`, and those are excluded from the determinism
//! byte-compare). Executor futures complete on their first poll, so they are
//! driven deterministically without a runtime (important: callers may already
//! be inside one).

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::order::{OrderRequest, reduce_by_fraction};
use sentinel_core::policy::{DayState, PolicyConfig, PolicyContext, PolicyEngine, PolicySource};
use sentinel_core::risk::{self, ReflexConfig, ReflexState, RiskThresholds};
use sentinel_core::types::{
    AccountState, DataQuality, Decision, DecisionAction, Intent, Market, MarketId, PolicyVerdict,
    Position, RiskTier,
};

use crate::execution::Executor;
use crate::sim::baseline::{self, BaselineOutcome, crossed, liq_price_of};
use crate::sim::report::{
    LatencyPcts, SOURCE_REFLEX, SOURCE_STRATEGY, SimAction, SimMetrics, SimReport,
};
use crate::sim::scenario::{EventKind, Scenario, ScenarioError, ScenarioEvent, ScenarioLabel};
use crate::sim::venue::SimExecutor;

/// Run one scenario through the real decision path (reflex + policy +
/// optional recorded decisions) and produce the report.
///
/// # Errors
/// `ScenarioError` only for malformed inputs; policy/executor outcomes are
/// recorded inside the report, never fatal.
pub fn run(scenario: &Scenario) -> Result<SimReport, ScenarioError> {
    scenario.validate()?;
    let reflex_cfg = scenario.reflex_config()?;
    let policy_cfg = scenario.policy_config()?;
    let baseline = baseline::simulate(scenario);

    let markets: HashMap<MarketId, &Market> = scenario
        .markets
        .iter()
        .map(|market| (market.id, market))
        .collect();

    let executor = SimExecutor::default();
    for market in &scenario.markets {
        executor.register_market(market);
    }

    let mut notes = Vec::new();
    for event in &scenario.events {
        if event.kind != EventKind::FeedStale {
            notes.push(format!(
                "ignored event '{}' at {} ms (unmodeled in v1.0)",
                event.kind.as_str(),
                event.ts_ms
            ));
        }
        if let Some(note) = &event.note {
            notes.push(format!("event note: {note}"));
        }
    }
    match scenario.label {
        ScenarioLabel::Recorded => notes.push("label: recorded".to_string()),
        ScenarioLabel::Reconstructed => notes.push("label: reconstructed".to_string()),
        ScenarioLabel::Synthetic => {}
    }

    let mut sim = Sim {
        scenario,
        reflex_cfg,
        policy_cfg,
        thresholds: RiskThresholds {
            soft: Decimal::new(25, 0),
            warn: Decimal::new(15, 0),
            hard: Decimal::new(8, 0),
        },
        markets,
        executor,
        states: scenario
            .positions
            .iter()
            .cloned()
            .map(LivePosition::new)
            .collect(),
        reflex: ReflexState::new(),
        day: DayState::default(),
        actions: Vec::new(),
        notes,
        samples: Vec::new(),
        policy_violations: 0,
        executed_reduce_orders: 0,
        ticks: 0,
        free_balance: scenario.start_free_balance,
        marks: HashMap::new(),
    };
    sim.walk();
    Ok(sim.finish(baseline))
}

/// Live bookkeeping for one scenario position.
struct LivePosition {
    /// Mutable working copy of the position.
    position: Position,
    /// Realized PnL accumulated from executed fills, USD.
    realized_pnl: Decimal,
    /// Taker fees paid by this position, USD.
    fees_usd: Decimal,
    /// Active (not closed at size 0).
    active: bool,
    /// Tier observed at the previous evaluation (Yellow-entry detection).
    prev_tier: Option<RiskTier>,
}

impl LivePosition {
    /// Wrap a position with zeroed bookkeeping.
    fn new(position: Position) -> Self {
        Self {
            position,
            realized_pnl: Decimal::ZERO,
            fees_usd: Decimal::ZERO,
            active: true,
            prev_tier: None,
        }
    }
}

/// Engine state for one scenario run.
struct Sim<'a> {
    scenario: &'a Scenario,
    reflex_cfg: ReflexConfig,
    policy_cfg: PolicyConfig,
    thresholds: RiskThresholds,
    markets: HashMap<MarketId, &'a Market>,
    executor: SimExecutor,
    states: Vec<LivePosition>,
    reflex: ReflexState,
    day: DayState,
    actions: Vec<SimAction>,
    notes: Vec<String>,
    samples: Vec<u64>,
    policy_violations: u32,
    executed_reduce_orders: u32,
    ticks: u64,
    free_balance: Decimal,
    marks: HashMap<MarketId, Decimal>,
}

impl Sim<'_> {
    /// Apply every price-path entry in order (the tick loop, SPEC-P13 §4).
    fn walk(&mut self) {
        for index in 0..self.scenario.price_path.len() {
            let (ts_ms, market_id, mark) = {
                let tick = &self.scenario.price_path[index];
                (tick.ts_ms, tick.market_id, tick.mark_price)
            };
            self.step(ts_ms, market_id, mark);
        }
    }

    /// One tick: update marks, check liquidations, evaluate all positions.
    fn step(&mut self, ts_ms: i64, market_id: u32, mark: Decimal) {
        let now_ms = u64::try_from(ts_ms).unwrap_or(0);
        self.executor.set_tick(now_ms);
        self.executor.set_mark(MarketId(market_id), mark);
        self.marks.insert(MarketId(market_id), mark);

        for state in &mut self.states {
            if state.active && state.position.market_id.0 == market_id {
                state.position.mark_price = Some(mark);
                state.position.unrealized_pnl =
                    state.position.size * (mark - state.position.entry_price);
            }
        }

        // Sentinel-side crossing check: the frozen §5 sentinel accounting is
        // formula-only (realized + final unrealized + fees), so a crossing is
        // recorded as an honest warning note instead of ending the walk.
        for state in &self.states {
            let position = &state.position;
            if position.size.is_zero() {
                continue;
            }
            let Some(mark) = self.marks.get(&position.market_id).copied() else {
                continue;
            };
            let Some(liq) = self
                .markets
                .get(&position.market_id)
                .and_then(|market| liq_price_of(position, market))
            else {
                continue;
            };
            if crossed(position, mark, liq) {
                self.notes.push(format!(
                    "sentinel position crossed its liq price at {ts_ms} ms \
                     (formula-only accounting, SPEC-P13 §5)"
                ));
            }
        }

        let quality = quality_at(&self.scenario.events, ts_ms);
        for index in 0..self.states.len() {
            self.evaluate(index, ts_ms, now_ms, quality);
        }
        self.ticks = self.ticks.saturating_add(1);
    }

    /// Evaluate one position at one tick (reflex + consult points, §4).
    fn evaluate(&mut self, index: usize, ts_ms: i64, now_ms: u64, quality: DataQuality) {
        if !self.states[index].active {
            return;
        }
        let market_id = self.states[index].position.market_id;
        let Some(market) = self.markets.get(&market_id) else {
            return;
        };
        let Some(distance) = risk::distance_to_liq_pct(&self.states[index].position, market) else {
            // Cannot classify safely; the production pipeline skips the same way.
            return;
        };
        let tier = risk::tier(distance, &self.thresholds);

        // (a) Yellow-entry transition per market (`Green -> Yellow`, SPEC-P13
        // §4). The consult runs after the reflex pass, like the production
        // pipeline's post-risk strategy review.
        let entered_yellow =
            tier == RiskTier::Yellow && self.states[index].prev_tier == Some(RiskTier::Green);

        // Reflex: the exact production call, timed for the metrics section.
        let started = Instant::now();
        let intent = self.reflex.advance(
            &self.states[index].position,
            tier,
            quality,
            &self.reflex_cfg,
            now_ms,
        );
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.samples.push(elapsed);

        if let Some(intent) = intent {
            let executed_order =
                self.gate_and_execute(index, intent, PolicySource::Reflex, tier, ts_ms);
            // (b) post-review consult after each executed reflex action.
            if executed_order {
                self.consult(index, ts_ms, tier);
            }
        }
        // (a) Yellow-entry consult.
        if entered_yellow {
            self.consult(index, ts_ms, tier);
        }
        self.states[index].prev_tier = Some(tier);
    }

    /// Replay a recorded strategy decision at a consult point (SPEC-P13 §4).
    ///
    /// A trace entry applies only when `at_ms` == the consult tick's `ts_ms`
    /// and its market matches; entries at other ticks are not consulted.
    fn consult(&mut self, index: usize, ts_ms: i64, tier: RiskTier) {
        if !self.states[index].active {
            return;
        }
        let market_id = self.states[index].position.market_id;
        let decision = {
            let trace = self.scenario.decision_trace.as_deref().unwrap_or(&[]);
            trace
                .iter()
                .find(|entry| entry.at_ms == ts_ms && entry.market_id == market_id.0)
                .map(|entry| entry.decision.clone())
        };
        let Some(decision) = decision else {
            return;
        };
        self.apply_decision(index, &decision, tier, ts_ms);
    }

    /// Convert a Decision v3 document to an [`Intent`] and gate it
    /// (`PolicySource::Strategy`); HOLD/ESCALATE and non-convertible
    /// decisions are recorded as no-action rows.
    fn apply_decision(&mut self, index: usize, decision: &Decision, tier: RiskTier, ts_ms: i64) {
        match decision.action {
            DecisionAction::Hold => {
                let detail = format!("HOLD: {}", decision.reason);
                self.record_no_action(index, ts_ms, tier, SOURCE_STRATEGY, detail);
            }
            DecisionAction::Escalate => {
                let detail = format!("ESCALATE: {}", decision.reason);
                self.record_no_action(index, ts_ms, tier, SOURCE_STRATEGY, detail);
            }
            DecisionAction::Reduce => {
                let Some(amount) = decision.amount.filter(|amount| *amount > Decimal::ZERO) else {
                    let detail = "REDUCE trace skipped: no positive amount".to_string();
                    self.record_no_action(index, ts_ms, tier, SOURCE_STRATEGY, detail);
                    return;
                };
                let size_abs = self.states[index].position.size.abs();
                if size_abs.is_zero() {
                    let detail = "REDUCE trace skipped: position flat".to_string();
                    self.record_no_action(index, ts_ms, tier, SOURCE_STRATEGY, detail);
                    return;
                }
                // fraction = amount/|size| clamped to (0, 1].
                let fraction = (amount / size_abs).min(Decimal::ONE);
                let intent = Intent::Reduce {
                    fraction,
                    reason: decision.reason.clone(),
                };
                self.gate_and_execute(index, intent, PolicySource::Strategy, tier, ts_ms);
            }
            DecisionAction::Close => {
                let intent = Intent::Close {
                    reason: decision.reason.clone(),
                };
                self.gate_and_execute(index, intent, PolicySource::Strategy, tier, ts_ms);
            }
            DecisionAction::AddCollateral => {
                let Some(amount) = decision.amount.filter(|amount| *amount > Decimal::ZERO) else {
                    let detail = "ADD_COLLATERAL skipped: no positive amount".to_string();
                    self.record_no_action(index, ts_ms, tier, SOURCE_STRATEGY, detail);
                    return;
                };
                let intent = Intent::AddCollateral {
                    amount,
                    reason: decision.reason.clone(),
                };
                self.gate_and_execute(index, intent, PolicySource::Strategy, tier, ts_ms);
            }
        }
    }

    /// Gate `intent` through [`PolicyEngine`] and execute when allowed.
    /// Returns `true` when an order actually filled.
    fn gate_and_execute(
        &mut self,
        index: usize,
        intent: Intent,
        source: PolicySource,
        tier: RiskTier,
        ts_ms: i64,
    ) -> bool {
        let trigger = intent_reason(&intent).to_string();
        let market_id = self.states[index].position.market_id;
        let snapshot = self.account_snapshot();
        let context = PolicyContext {
            source,
            tier,
            market_id,
        };
        let verdict =
            PolicyEngine::evaluate(&intent, &snapshot, &self.policy_cfg, &self.day, &context);
        match &verdict {
            PolicyVerdict::Allow => match &intent {
                Intent::Reduce { fraction, .. } => {
                    self.order_and_fill(index, *fraction, &trigger, source, tier, ts_ms, true)
                }
                Intent::Close { .. } => {
                    self.order_and_fill(index, Decimal::ONE, &trigger, source, tier, ts_ms, false)
                }
                Intent::AddCollateral { amount, .. } => {
                    self.free_balance -= *amount;
                    self.states[index].position.collateral += *amount;
                    self.day.actions_today = self.day.actions_today.saturating_add(1);
                    let detail = format!(
                        "{trigger}: collateral added +{amount} (free balance {})",
                        self.free_balance
                    );
                    self.record_action(
                        index,
                        ts_ms,
                        tier,
                        source_str(source),
                        None,
                        Some(&PolicyVerdict::Allow),
                        detail,
                    );
                    false
                }
                Intent::Alert { message } => {
                    self.record_no_action(
                        index,
                        ts_ms,
                        tier,
                        source_str(source),
                        format!("alert: {message}"),
                    );
                    false
                }
            },
            PolicyVerdict::Deny { reason } => {
                let detail = format!("{trigger}: denied: {reason}");
                self.record_action(
                    index,
                    ts_ms,
                    tier,
                    source_str(source),
                    None,
                    Some(&verdict),
                    detail,
                );
                false
            }
            PolicyVerdict::NeedsApproval { reason } => {
                let detail = format!("{trigger}: needs approval: {reason}");
                self.record_action(
                    index,
                    ts_ms,
                    tier,
                    source_str(source),
                    None,
                    Some(&verdict),
                    detail,
                );
                false
            }
        }
    }

    /// Size a reduce-only order and (when sized) fill it through the
    /// [`SimExecutor`](crate::sim::venue::SimExecutor), updating the position
    /// and the daily action counter. `counts_as_reduce` marks the fill as a
    /// reduce-family execution for the `false_positive_reduces` accounting
    /// (Close orders are not counted, mirroring the frozen oracle).
    #[allow(clippy::too_many_arguments)] // explicit execution context by design (SPEC-P13 §4)
    fn order_and_fill(
        &mut self,
        index: usize,
        fraction: Decimal,
        reason: &str,
        source: PolicySource,
        tier: RiskTier,
        ts_ms: i64,
        counts_as_reduce: bool,
    ) -> bool {
        let order = {
            let position = &self.states[index].position;
            let Some(market) = self.markets.get(&position.market_id) else {
                return false;
            };
            reduce_by_fraction(position, fraction, market, self.executor.slippage_bps())
        };
        let Some(order) = order else {
            let detail = format!("{reason}: sizing skipped (size below lot/min)");
            self.record_action(
                index,
                ts_ms,
                tier,
                source_str(source),
                None,
                Some(&PolicyVerdict::Allow),
                detail,
            );
            return false;
        };

        match complete_now(self.executor.submit(&order)) {
            Ok(report) => {
                let Some(fill) = self.executor.last_fill() else {
                    let detail = format!("{reason}: executor produced no fill record");
                    self.record_action(
                        index,
                        ts_ms,
                        tier,
                        source_str(source),
                        None,
                        Some(&PolicyVerdict::Allow),
                        detail,
                    );
                    return false;
                };
                let state = &mut self.states[index];
                let signed_fill = if state.position.is_long() {
                    fill.size
                } else {
                    -fill.size
                };
                state.realized_pnl += (fill.price - state.position.entry_price) * signed_fill;
                state.position.size -= signed_fill;
                state.fees_usd += fill.fee_usd;
                if state.position.size.is_zero() {
                    // Position removed at size 0 (SPEC-P13 §4).
                    state.active = false;
                }
                if counts_as_reduce {
                    self.executed_reduce_orders = self.executed_reduce_orders.saturating_add(1);
                }
                self.day.actions_today = self.day.actions_today.saturating_add(1);

                let detail = format!(
                    "{reason}: filled {} @ {} ({}, fee ${})",
                    fill.size, fill.price, fill.client_order_id, fill.fee_usd
                );
                let allow = PolicyVerdict::Allow;
                self.record_action(
                    index,
                    ts_ms,
                    tier,
                    source_str(source),
                    Some(&report.order),
                    Some(&allow),
                    detail,
                );
                true
            }
            Err(err) => {
                let detail = format!("{reason}: executor error: {err}");
                self.record_action(
                    index,
                    ts_ms,
                    tier,
                    source_str(source),
                    None,
                    Some(&PolicyVerdict::Allow),
                    detail,
                );
                false
            }
        }
    }

    /// Account snapshot for the policy gate (deterministic; no wall clock).
    fn account_snapshot(&self) -> AccountState {
        let mut positions = Vec::new();
        let mut equity = self.free_balance;
        for state in &self.states {
            if !state.active {
                continue;
            }
            equity += state.position.collateral + state.position.unrealized_pnl;
            positions.push(state.position.clone());
        }
        AccountState {
            positions,
            free_balance: self.free_balance,
            equity,
            fee_tier: 0,
            snapshot_ts: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// Record one action row with no order and no verdict (HOLD, ESCALATE,
    /// alerts, trace conversion skips).
    fn record_no_action(
        &mut self,
        index: usize,
        ts_ms: i64,
        tier: RiskTier,
        source: &str,
        detail: String,
    ) {
        self.record_action(index, ts_ms, tier, source, None, None, detail);
    }

    /// Append one timeline row for the position at `index`.
    #[allow(clippy::too_many_arguments)] // one row shape, explicit fields by design
    fn record_action(
        &mut self,
        index: usize,
        ts_ms: i64,
        tier: RiskTier,
        source: &str,
        order: Option<&OrderRequest>,
        verdict: Option<&PolicyVerdict>,
        detail: String,
    ) {
        if let Some(verdict) = verdict
            && !matches!(verdict, PolicyVerdict::Allow)
            && order.is_some()
        {
            // Defensive: an executed action must never carry a non-Allow
            // verdict (policy is the gate, SPEC-P13 §6).
            self.policy_violations = self.policy_violations.saturating_add(1);
        }
        let state = &self.states[index];
        self.actions.push(SimAction {
            ts_ms,
            market_id: state.position.market_id.0,
            tier,
            source: source.to_string(),
            order: order.cloned(),
            verdict: verdict.cloned(),
            detail,
            size_after: state.position.size,
        });
    }

    /// Assemble the report (SPEC-P13 §5–§6).
    ///
    /// Sentinel-side liquidations are not part of the frozen §5 sentinel
    /// accounting (realized + final unrealized + fees only); crossings are
    /// recorded in `notes` and this field reports 0 for the current scenario
    /// set. `liquidations_avoided` follows the SPEC-P13 §5 definition.
    fn finish(mut self, baseline: BaselineOutcome) -> SimReport {
        let mut sentinel_loss = Decimal::ZERO;
        for state in &self.states {
            let term = if state.active {
                let final_unrealized = match state.position.mark_price {
                    Some(mark) => state.position.size * (mark - state.position.entry_price),
                    None => Decimal::ZERO,
                };
                (-(state.realized_pnl + final_unrealized)).max(Decimal::ZERO)
            } else {
                // Closed at size 0: realized PnL only (SPEC-P13 §5).
                (-state.realized_pnl).max(Decimal::ZERO)
            };
            sentinel_loss += term + state.fees_usd;
        }

        let baseline_loss = baseline.loss_usd;
        let capital_saved = baseline_loss - sentinel_loss;
        let false_positive_reduces = if baseline.liquidations == 0 {
            self.executed_reduce_orders
        } else {
            0
        };
        let notional_usd = self
            .scenario
            .positions
            .iter()
            .map(|position| (position.size * position.entry_price).abs())
            .sum::<Decimal>();

        SimReport {
            scenario_id: self.scenario.id.clone(),
            label: self.scenario.label.as_str().to_string(),
            ticks: self.ticks,
            actions: self.actions,
            baseline_liquidations: baseline.liquidations,
            sentinel_liquidations: 0,
            liquidations_avoided: baseline.liquidations,
            baseline_loss_usd: baseline_loss,
            sentinel_loss_usd: sentinel_loss,
            capital_saved_usd: capital_saved,
            sim_fees_usd: self.executor.total_fees_usd(),
            false_positive_reduces,
            policy_violations: self.policy_violations,
            notes: self.notes,
            notional_usd,
            metrics: SimMetrics {
                reflex_eval_eval: percentiles(&mut self.samples),
            },
        }
    }
}

/// Data quality at `ts_ms`: `Stale{secs}` inside a `feed_stale` window, else
/// `Fresh` (SPEC-P13 §4). The window is closed on both ends and `secs` counts
/// whole elapsed seconds since the active window's start.
fn quality_at(events: &[ScenarioEvent], ts_ms: i64) -> DataQuality {
    let mut window_start: Option<i64> = None;
    for event in events {
        if event.kind == EventKind::FeedStale
            && let Some(until) = event.until_ms
            && event.ts_ms <= ts_ms
            && ts_ms <= until
        {
            window_start = Some(window_start.map_or(event.ts_ms, |start| start.max(event.ts_ms)));
        }
    }
    match window_start {
        Some(start) => DataQuality::Stale {
            secs: u64::try_from(ts_ms.saturating_sub(start)).unwrap_or(0) / 1_000,
        },
        None => DataQuality::Fresh,
    }
}

/// Human-readable trigger of an intent.
fn intent_reason(intent: &Intent) -> &str {
    match intent {
        Intent::Reduce { reason, .. }
        | Intent::Close { reason, .. }
        | Intent::AddCollateral { reason, .. } => reason,
        Intent::Alert { message } => message,
    }
}

/// Report `source` string for a policy source.
fn source_str(source: PolicySource) -> &'static str {
    match source {
        PolicySource::Reflex => SOURCE_REFLEX,
        PolicySource::Strategy => SOURCE_STRATEGY,
        PolicySource::Manual => "MANUAL",
    }
}

/// Drive a future that completes on its first poll (the sim executor never
/// awaits). Keeps the engine runtime-free, so it is safe to call from inside
/// an existing async context.
fn complete_now<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("SimExecutor::submit must complete on its first poll"),
    }
}

/// Nearest-rank percentiles over the collected samples (µs).
fn percentiles(samples: &mut [u64]) -> LatencyPcts {
    samples.sort_unstable();
    LatencyPcts {
        p50_us: percentile(samples, 50),
        p90_us: percentile(samples, 90),
        p99_us: percentile(samples, 99),
    }
}

/// Nearest-rank percentile of a sorted slice (`ceil(p×n/100)`-th value).
fn percentile(sorted: &[u64], p: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as u64;
    let rank = (u64::from(p) * n).div_ceil(100);
    let index = usize::try_from(rank.saturating_sub(1).min(n - 1)).unwrap_or(0);
    sorted[index]
}

#[cfg(test)]
mod tests {
    use sentinel_core::types::{Decision, Urgency};
    use serde_json::json;

    use super::*;
    use crate::sim::scenario::{DecisionTraceEntry, PriceTick, ScenarioLabel};

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH".to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: d("0.083333"),
            maintenance_margin_fraction: d("0.05"),
            max_leverage: d("12"),
            min_size: Decimal::ZERO,
            tick_size: d("0.01"),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    fn long_position(
        size: Decimal,
        entry: Decimal,
        mark: Decimal,
        collateral: Decimal,
    ) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: entry,
            mark_price: Some(mark),
            liq_price: None,
            collateral,
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: d("10"),
            opened_at: None,
        }
    }

    fn scenario(positions: Vec<Position>, marks: &[(i64, &str)]) -> Scenario {
        Scenario {
            id: "engine-test".to_string(),
            label: ScenarioLabel::Synthetic,
            description: "engine unit test".to_string(),
            start_free_balance: d("10000"),
            markets: vec![market()],
            positions,
            price_path: marks
                .iter()
                .map(|(ts_ms, mark)| PriceTick {
                    ts_ms: *ts_ms,
                    market_id: 32,
                    mark_price: d(mark),
                })
                .collect(),
            events: Vec::new(),
            reflex: None,
            policy: None,
            decision_trace: None,
        }
    }

    fn trace_entry(
        at_ms: i64,
        action: DecisionAction,
        amount: Option<Decimal>,
        reason: &str,
    ) -> DecisionTraceEntry {
        DecisionTraceEntry {
            at_ms,
            market_id: 32,
            decision: Decision {
                action,
                market_id: 32,
                amount,
                confidence: d("0.9"),
                urgency: Urgency::Elevated,
                reason: reason.to_string(),
            },
        }
    }

    /// Long 1 @ 100 with 10 collateral: liq = 95; marks 99 → 96 → 94 make the
    /// baseline liquidate (94 ≤ 95) while the sentinel reduces at 99 (Red,
    /// distance 4.04 %) and never crosses (liq improves to 85 > 94).
    fn crash_scenario() -> Scenario {
        scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("10"))],
            &[(1_000, "99"), (2_000, "96"), (3_000, "94")],
        )
    }

    #[test]
    fn conservation_is_exact_with_hand_computed_values() {
        let report = run(&crash_scenario()).expect("run");
        assert_eq!(report.ticks, 3);
        assert_eq!(report.baseline_liquidations, 1);
        assert_eq!(report.sentinel_liquidations, 0);
        assert_eq!(report.liquidations_avoided, 1);
        assert_eq!(report.baseline_loss_usd, d("10"), "all collateral lost");
        // Fill 0.5 @ 99×0.999 = 98.901; realized −0.5495; final unrealized
        // 0.5×(94−100) = −3; fee 345 µs × 49.4505 / 1e6 = 0.0170604225.
        assert_eq!(report.sentinel_loss_usd, d("3.5665604225"));
        assert_eq!(report.capital_saved_usd, d("6.4334395775"));
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd,
            "conservation identity must hold exactly (SPEC-P13 §5)"
        );
        assert_eq!(report.sim_fees_usd, d("0.0170604225"));
        assert_eq!(report.false_positive_reduces, 0, "baseline liquidates");
        assert_eq!(report.policy_violations, 0);
        assert_eq!(report.notional_usd, d("100"));

        assert_eq!(report.actions.len(), 1);
        let action = &report.actions[0];
        assert_eq!(action.ts_ms, 1_000);
        assert_eq!(action.tier, RiskTier::Red);
        assert_eq!(action.source, SOURCE_REFLEX);
        assert_eq!(action.size_after, d("0.5"));
        assert_eq!(action.verdict, Some(PolicyVerdict::Allow));
        let order = action.order.as_ref().expect("sized order");
        assert_eq!(order.size, d("0.5"));
        assert_eq!(order.max_slippage_bps, 10);
        assert!(
            action.detail.contains("98.901"),
            "fill detail: {:?}",
            action.detail
        );
    }

    #[test]
    fn cooldown_respected_between_reflex_actions() {
        let scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("10"))],
            &[
                (0, "96"),
                (100_000, "96"),
                (200_000, "96"),
                (300_000, "96"),
                (400_000, "96"),
                (500_000, "96"),
                (600_000, "96"),
            ],
        );
        let report = run(&scenario).expect("run");

        let reflex_actions: Vec<&SimAction> = report
            .actions
            .iter()
            .filter(|action| action.source == SOURCE_REFLEX)
            .collect();
        assert_eq!(
            reflex_actions.len(),
            2,
            "one reduce, then one after cooldown"
        );
        assert_eq!(reflex_actions[0].ts_ms, 0);
        assert_eq!(reflex_actions[0].tier, RiskTier::Red);
        assert_eq!(
            reflex_actions[0].order.as_ref().expect("order").size,
            d("0.5")
        );
        assert_eq!(reflex_actions[1].ts_ms, 600_000, "cooldown equality fires");
        assert_eq!(reflex_actions[1].tier, RiskTier::Orange);
        assert_eq!(
            reflex_actions[1].order.as_ref().expect("order").size,
            d("0.125"),
            "0.25 × remaining 0.5"
        );
        assert!(
            !report
                .actions
                .iter()
                .any(|action| (100_000..600_000).contains(&action.ts_ms)),
            "no actions inside the cooldown window: {:?}",
            report.actions
        );
        assert_eq!(reflex_actions[0].size_after, d("0.5"));
        assert_eq!(reflex_actions[1].size_after, d("0.375"));

        // Defense cost honesty: the baseline never liquidates here and the
        // saved total is negative (fees + realized loss), reported as-is.
        assert_eq!(report.baseline_liquidations, 0);
        assert_eq!(report.false_positive_reduces, 2);
        assert_eq!(report.capital_saved_usd, d("-0.0806793"));
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd
        );
    }

    #[test]
    fn determinism_is_byte_identical_across_reruns() {
        let first = run(&crash_scenario()).expect("first run");
        let second = run(&crash_scenario()).expect("second run");
        assert_eq!(
            first.determinism_view(),
            second.determinism_view(),
            "everything except metrics must compare equal"
        );
        assert_eq!(
            first.determinism_view().canonical_json().expect("json"),
            second.determinism_view().canonical_json().expect("json"),
            "byte-compare of the deterministic section"
        );
    }

    #[test]
    fn yellow_entry_consult_applies_trace_decisions() {
        // Green at 100 (25 %), Yellow at 95 (21.05 %), Green again at 100
        // (reduced size ⇒ liq 45), Yellow again at 55 (18.18 %).
        let mut scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("30"))],
            &[(1_000, "100"), (2_000, "95"), (3_000, "100"), (4_000, "55")],
        );
        scenario.decision_trace = Some(vec![
            trace_entry(2_000, DecisionAction::Reduce, Some(d("0.5")), "de-risk"),
            trace_entry(4_000, DecisionAction::Hold, None, "watch"),
        ]);

        let report = run(&scenario).expect("run");
        assert_eq!(report.actions.len(), 2, "trace rows: {:?}", report.actions);

        let reduce = &report.actions[0];
        assert_eq!(reduce.ts_ms, 2_000, "Green → Yellow consult tick");
        assert_eq!(reduce.source, SOURCE_STRATEGY);
        assert_eq!(reduce.tier, RiskTier::Yellow);
        assert_eq!(reduce.verdict, Some(PolicyVerdict::Allow));
        assert_eq!(reduce.size_after, d("0.5"));
        let order = reduce.order.as_ref().expect("strategy order");
        assert_eq!(order.size, d("0.5"), "amount 0.5 / |size| 1 = fraction 0.5");
        // 95 × 0.999 = 94.905.
        assert!(
            reduce.detail.contains("94.905"),
            "detail: {:?}",
            reduce.detail
        );

        let hold = &report.actions[1];
        assert_eq!(hold.ts_ms, 4_000, "second Yellow entry after recovery");
        assert_eq!(hold.source, SOURCE_STRATEGY);
        assert_eq!(hold.order, None);
        assert_eq!(hold.verdict, None);
        assert_eq!(hold.size_after, d("0.5"));
        assert_eq!(hold.detail, "HOLD: watch");

        // Baseline liquidates at 55 ≤ 75 (loss = collateral 30); the sentinel
        // reduced to 0.5 and survives: sentinel loss = 25.0475 + 0.0163711125
        // fee (Python `decimal` verified), saved = 4.9361288875.
        assert_eq!(report.baseline_liquidations, 1);
        assert_eq!(report.sentinel_liquidations, 0);
        assert_eq!(report.baseline_loss_usd, d("30"));
        assert_eq!(report.sentinel_loss_usd, d("25.0638711125"));
        assert_eq!(report.capital_saved_usd, d("4.9361288875"));
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd
        );
    }

    #[test]
    fn strategy_decisions_above_the_approval_threshold_are_recorded_skips() {
        let mut scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("30"))],
            &[(1_000, "100"), (2_000, "95")],
        );
        scenario.policy = Some(json!({"require_approval_above_usd": "10"}));
        scenario.decision_trace = Some(vec![trace_entry(
            2_000,
            DecisionAction::Reduce,
            Some(d("0.5")),
            "de-risk",
        )]);

        let report = run(&scenario).expect("run");
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].ts_ms, 2_000, "Green → Yellow consult");
        assert_eq!(
            report.actions[0].verdict,
            Some(PolicyVerdict::NeedsApproval {
                reason: "notional above approval threshold".to_string()
            })
        );
        assert_eq!(report.actions[0].order, None);
        assert_eq!(report.actions[0].size_after, d("1"), "nothing executed");
        assert_eq!(report.sim_fees_usd, Decimal::ZERO);
        assert_eq!(report.policy_violations, 0);
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd
        );
    }

    #[test]
    fn kill_switch_denies_reflex_and_is_recorded() {
        let mut scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("10"))],
            &[(1_000, "99")],
        );
        scenario.policy = Some(json!({"kill_switch": true}));

        let report = run(&scenario).expect("run");
        assert_eq!(report.actions.len(), 1);
        assert_eq!(
            report.actions[0].verdict,
            Some(PolicyVerdict::Deny {
                reason: "kill switch engaged".to_string()
            })
        );
        assert_eq!(report.actions[0].size_after, d("1"));
        assert_eq!(report.sim_fees_usd, Decimal::ZERO);
        assert_eq!(report.sentinel_liquidations, 0);
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd
        );
    }

    #[test]
    fn stale_window_feeds_the_reflex_quality_gate() {
        let mut scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("10"))],
            &[(60_000, "96")],
        );
        scenario.events.push(crate::sim::scenario::ScenarioEvent {
            ts_ms: 0,
            kind: EventKind::FeedStale,
            until_ms: Some(120_000),
            note: None,
        });
        scenario.reflex = Some(json!({"stale_reduce": true, "cooldown_ms": 0}));

        let report = run(&scenario).expect("run");
        assert_eq!(report.actions.len(), 1, "gated stale reduce fires");
        assert!(
            report.actions[0]
                .detail
                .contains("stale data: gated reduce"),
            "detail: {:?}",
            report.actions[0].detail
        );
        assert_eq!(
            report.actions[0].order.as_ref().expect("order").size,
            d("0.25"),
            "gated reduce uses orange_fraction"
        );
        // 96 × 0.999 = 95.904.
        assert!(report.actions[0].detail.contains("95.904"));
    }

    #[test]
    fn sentinel_crossing_is_formula_only_with_a_honest_note() {
        // After the reduce (size 0.5, collateral 10 ⇒ liq 85) the mark falls to
        // 80: the frozen §5 sentinel accounting is formula-only, so no sentinel
        // liquidation is counted and the crossing is surfaced in the notes.
        let scenario = scenario(
            vec![long_position(d("1"), d("100"), d("100"), d("10"))],
            &[(1_000, "99"), (2_000, "96"), (3_000, "94"), (4_000, "80")],
        );
        let report = run(&scenario).expect("run");
        assert_eq!(report.sentinel_liquidations, 0);
        assert_eq!(report.liquidations_avoided, report.baseline_liquidations);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("crossed its liq")),
            "notes: {:?}",
            report.notes
        );
        // realized −0.5495, final unrealized 0.5×(80−100) = −10,
        // fee 0.0170604225 ⇒ loss 10.5665604225; saved = 10 − loss.
        assert_eq!(report.sentinel_loss_usd, d("10.5665604225"));
        assert_eq!(report.capital_saved_usd, d("-0.5665604225"));
        assert_eq!(
            report.capital_saved_usd,
            report.baseline_loss_usd - report.sentinel_loss_usd
        );
    }
}
