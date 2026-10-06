//! Reflex decision core — pure planning over a state snapshot.
//!
//! P06 refactor: the decision pass was extracted from the P05 sweep so the
//! supervised pipeline ([`crate::pipeline`]) drives it directly. This module
//! contains **no I/O**: it maps (account state, markets, clocks) to planned
//! actions; the pipeline executes them.
//!
//! Determinism: given the same state, config and `now_ms`, [`decide`] returns
//! the same plan — the replay contract in `SPEC-P06.md` §7 relies on this.

use rust_decimal::Decimal;
use sentinel_core::order::{OrderRequest, reduce_by_fraction};
use sentinel_core::policy::{DayState, PolicyConfig, PolicyContext, PolicyEngine, PolicySource};
use sentinel_core::risk::{self, ReflexConfig, ReflexState, RiskThresholds};
use sentinel_core::types::{
    AccountState, DataQuality, Intent, Market, MarketId, PolicyVerdict, RiskTier,
};

use crate::config::Config;

/// Default slippage cap for reflex reduce orders, bps.
pub const REFLEX_SLIPPAGE_BPS: u16 = 50;

/// A fully planned decision, ready for the executor (pure; no I/O).
#[derive(Debug, Clone)]
pub struct PlannedAction {
    /// Correlates every log line and event of this decision.
    pub decision_id: String,
    /// Market the decision concerns.
    pub market_id: MarketId,
    /// Tier after classification.
    pub tier: RiskTier,
    /// Distance to liquidation at decision time, percent.
    pub distance_pct: Decimal,
    /// Intent produced by the reflex engine, if any.
    pub intent: Option<Intent>,
    /// Sized reduce order when the intent survived policy and sizing.
    pub order: Option<OrderRequest>,
    /// Policy gate verdict.
    pub verdict: PolicyVerdict,
    /// Human-readable note (skip reasons, no-action markers).
    pub note: String,
}

/// Risk thresholds from the app configuration.
pub fn thresholds_from(cfg: &Config) -> RiskThresholds {
    RiskThresholds {
        soft: cfg.risk.soft_pct,
        warn: cfg.risk.warn_pct,
        hard: cfg.risk.hard_pct,
    }
}

/// Reflex parameters from the app configuration.
fn reflex_params(cfg: &Config) -> ReflexConfig {
    ReflexConfig {
        reduce_fraction: cfg.risk.reflex_reduce_fraction,
        orange_fraction: cfg.risk.reflex_orange_fraction,
        cooldown_ms: cfg.risk.reflex_cooldown_secs.saturating_mul(1000),
        stale_reduce: false,
    }
}

/// Policy limits from the app configuration.
///
/// The kill switch is wired in P11 (Telegram); until then it is always off.
fn policy_params(cfg: &Config) -> PolicyConfig {
    PolicyConfig {
        market_allowlist: cfg
            .risk
            .market_allowlist
            .iter()
            .copied()
            .map(MarketId)
            .collect(),
        max_order_size_usd: cfg.risk.max_order_size_usd,
        max_daily_actions: cfg.risk.max_daily_actions,
        require_approval_above_usd: cfg.risk.require_approval_above_usd,
        kill_switch: false,
    }
}

/// Fraction of the position an intent asks to remove.
pub fn intent_fraction(intent: &Intent) -> Option<Decimal> {
    match intent {
        Intent::Reduce { fraction, .. } => Some(*fraction),
        Intent::Close { .. } => Some(Decimal::ONE),
        Intent::AddCollateral { .. } | Intent::Alert { .. } => None,
    }
}

/// One deterministic evaluation pass over every position (no I/O).
///
/// `now_ms` is the caller's clock (logical in replay → deterministic);
/// `quality` is the feed freshness at evaluation time. Positions without a
/// market entry or without a derivable distance are skipped with a warning
/// (they cannot be classified safely).
#[allow(clippy::too_many_arguments)] // decision context is explicit by design (SPEC-P06 §4)
pub fn decide(
    state: &AccountState,
    markets: &[Market],
    cfg: &Config,
    reflex: &mut ReflexState,
    day: &DayState,
    seq: &mut u64,
    now_ms: u64,
    quality: DataQuality,
) -> Vec<PlannedAction> {
    let thresholds = thresholds_from(cfg);
    let reflex_cfg = reflex_params(cfg);
    let policy_cfg = policy_params(cfg);
    let mut planned = Vec::new();

    for pos in &state.positions {
        *seq = seq.saturating_add(1);
        let decision_id = format!("d-{}", *seq);
        let Some(market) = markets.iter().find(|market| market.id == pos.market_id) else {
            tracing::warn!(
                decision_id = %decision_id,
                market_id = pos.market_id.0,
                "no market in context; skipped"
            );
            continue;
        };
        let Some(distance) = risk::distance_to_liq_pct(pos, market) else {
            tracing::warn!(
                decision_id = %decision_id,
                market_id = pos.market_id.0,
                "distance unavailable; skipped"
            );
            continue;
        };
        let tier = risk::tier(distance, &thresholds);
        let intent = reflex.advance(pos, tier, quality, &reflex_cfg, now_ms);

        let (verdict, order, note) = match &intent {
            None => (PolicyVerdict::Allow, None, "no action".to_string()),
            Some(intent) => {
                let ctx = PolicyContext {
                    source: PolicySource::Reflex,
                    tier,
                    market_id: pos.market_id,
                };
                let verdict = PolicyEngine::evaluate(intent, state, &policy_cfg, day, &ctx);
                let (order, note) = match &verdict {
                    PolicyVerdict::Allow => match intent_fraction(intent) {
                        None => (None, "non-order intent".to_string()),
                        Some(fraction) => {
                            match reduce_by_fraction(pos, fraction, market, REFLEX_SLIPPAGE_BPS) {
                                Some(order) => (Some(order), "sized".to_string()),
                                None => (None, "skipped: size below lot/min".to_string()),
                            }
                        }
                    },
                    PolicyVerdict::Deny { reason } => (None, format!("denied: {reason}")),
                    PolicyVerdict::NeedsApproval { reason } => {
                        (None, format!("needs approval: {reason}"))
                    }
                };
                (verdict, order, note)
            }
        };

        planned.push(PlannedAction {
            decision_id,
            market_id: pos.market_id,
            tier,
            distance_pct: distance,
            intent,
            order,
            verdict,
            note,
        });
    }
    planned
}

/// `PositionProbe` adapter for fixture replay (bins/tests); the live impl is
/// in `execution::mod` and the pipeline's live-state probe in `pipeline`.
impl crate::execution::PositionProbe for crate::perpl::MockPerpl {
    async fn position(
        &self,
        market_id: MarketId,
    ) -> crate::error::Result<Option<sentinel_core::types::Position>> {
        use crate::perpl::PerplFeed as _;
        let state = self.snapshot().await?;
        Ok(state
            .positions
            .into_iter()
            .find(|position| position.market_id == market_id))
    }
}
