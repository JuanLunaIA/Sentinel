//! Reflex decision loop — the deterministic pipeline
//! `feed → risk → policy → sizing → executor` with per-stage latency logging.
//!
//! P05 wires the path behind `ENABLE_REFLEX`; P06 turns this into the fully
//! supervised daemon. The LLM is not involved anywhere here (P00 invariant #1).
//!
//! `sweep` is a single deterministic pass over the current snapshot — it is
//! the unit of work the daemon repeats on a fixed interval, and the unit the
//! integration tests drive directly.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;
use sentinel_core::policy::{DayState, PolicyConfig, PolicyContext, PolicyEngine, PolicySource};
use sentinel_core::risk::{self, ReflexConfig, ReflexState, RiskThresholds};
use sentinel_core::types::{Intent, MarketId, PolicyVerdict, Position, RiskTier};

use crate::config::Config;
use crate::error::{Result, SentinelError};
use crate::execution::Executor;
use crate::perpl::PerplFeed;

/// Fixed sweep interval for the P05 daemon (P06 generalizes supervision).
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Default slippage cap for reflex reduce orders, bps.
const REFLEX_SLIPPAGE_BPS: u16 = 50;

/// One decision's structured record (also emitted via `tracing`).
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    /// Correlates every log line of this decision.
    pub decision_id: String,
    /// Market the decision concerns.
    pub market_id: MarketId,
    /// Tier after classification.
    pub tier: RiskTier,
    /// What the policy gate said.
    pub verdict: PolicyVerdict,
    /// True when an order was actually submitted.
    pub submitted: bool,
    /// Report status or error summary of the submission.
    pub outcome: String,
    /// Per-stage durations in microseconds: (classify, policy, submit).
    pub stage_us: (u64, u64, u64),
}

/// Build the core risk thresholds from the app configuration.
fn thresholds(cfg: &Config) -> RiskThresholds {
    RiskThresholds {
        soft: cfg.risk.soft_pct,
        warn: cfg.risk.warn_pct,
        hard: cfg.risk.hard_pct,
    }
}

/// Build the reflex parameters from the app configuration.
fn reflex_params(cfg: &Config) -> ReflexConfig {
    ReflexConfig {
        reduce_fraction: cfg.risk.reflex_reduce_fraction,
        orange_fraction: Decimal::new(25, 2),
        cooldown_ms: 600_000,
        stale_reduce: false,
    }
}

/// Build the policy limits from the app configuration.
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
fn intent_fraction(intent: &Intent) -> Option<Decimal> {
    match intent {
        Intent::Reduce { fraction, .. } => Some(*fraction),
        Intent::Close { .. } => Some(Decimal::ONE),
        Intent::AddCollateral { .. } | Intent::Alert { .. } => None,
    }
}

/// Wall-clock milliseconds (execution-layer bookkeeping only; the core stays
/// clock-free with caller-supplied `now_ms`).
pub(crate) fn unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

/// One decision sweep across every position in the account snapshot.
///
/// Returns one [`DecisionRecord`] per position considered. Errors from the
/// feed propagate; individual submission errors are captured in the record
/// (a duplicate order is an expected, non-fatal outcome).
pub async fn sweep<F, E>(
    cfg: &Config,
    feed: &F,
    executor: &E,
    reflex: &mut ReflexState,
    day: &mut DayState,
    decision_seq: &mut u64,
) -> Result<Vec<DecisionRecord>>
where
    F: PerplFeed + Sync,
    E: Executor + Sync,
{
    let markets = feed.context().await?;
    let account = feed.snapshot().await?;
    let thresholds = thresholds(cfg);
    let reflex_cfg = reflex_params(cfg);
    let policy_cfg = policy_params(cfg);
    let now_ms = unix_ms();
    let mut records = Vec::new();

    for pos in &account.positions {
        *decision_seq = decision_seq.saturating_add(1);
        let decision_id = format!("d-{}", *decision_seq);
        let Some(market) = markets.iter().find(|m| m.id == pos.market_id) else {
            tracing::warn!(decision_id = %decision_id, market_id = pos.market_id.0, "no market in context; skipped");
            continue;
        };

        let t0 = Instant::now();
        let Some(distance) = risk::distance_to_liq_pct(pos, market) else {
            tracing::warn!(decision_id = %decision_id, market_id = pos.market_id.0, "distance unavailable; skipped");
            continue;
        };
        let tier = risk::tier(distance, &thresholds);
        let intent = reflex.advance(
            pos,
            tier,
            sentinel_core::types::DataQuality::Fresh,
            &reflex_cfg,
            now_ms,
        );
        let classify_us = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);

        let Some(intent) = intent else {
            records.push(DecisionRecord {
                decision_id: decision_id.clone(),
                market_id: pos.market_id,
                tier,
                verdict: PolicyVerdict::Allow,
                submitted: false,
                outcome: "no action".to_string(),
                stage_us: (classify_us, 0, 0),
            });
            tracing::debug!(decision_id = %decision_id, market_id = pos.market_id.0, tier = ?tier, "no reflex action");
            continue;
        };

        let t1 = Instant::now();
        let ctx = PolicyContext {
            source: PolicySource::Reflex,
            tier,
            market_id: pos.market_id,
        };
        let verdict = PolicyEngine::evaluate(&intent, &account, &policy_cfg, day, &ctx);
        let policy_us = u64::try_from(t1.elapsed().as_micros()).unwrap_or(u64::MAX);

        let (submitted, outcome, submit_us) = match &verdict {
            PolicyVerdict::Allow => {
                let Some(fraction) = intent_fraction(&intent) else {
                    records.push(DecisionRecord {
                        decision_id: decision_id.clone(),
                        market_id: pos.market_id,
                        tier,
                        verdict: verdict.clone(),
                        submitted: false,
                        outcome: "non-order intent".to_string(),
                        stage_us: (classify_us, policy_us, 0),
                    });
                    continue;
                };
                let t2 = Instant::now();
                match sentinel_core::order::reduce_by_fraction(
                    pos,
                    fraction,
                    market,
                    REFLEX_SLIPPAGE_BPS,
                ) {
                    None => {
                        let us = u64::try_from(t2.elapsed().as_micros()).unwrap_or(u64::MAX);
                        (false, "skipped: size below lot/min".to_string(), us)
                    }
                    Some(order) => {
                        let res = executor.submit(&order).await;
                        let us = u64::try_from(t2.elapsed().as_micros()).unwrap_or(u64::MAX);
                        match res {
                            Ok(report) => {
                                day.actions_today = day.actions_today.saturating_add(1);
                                (
                                    true,
                                    format!("{:?} ({})", report.status, report.client_order_id),
                                    us,
                                )
                            }
                            Err(SentinelError::DuplicateOrder { key, .. }) => {
                                (false, format!("duplicate suppressed ({key})"), us)
                            }
                            Err(err) => (false, format!("submit error: {err}"), us),
                        }
                    }
                }
            }
            PolicyVerdict::Deny { reason } => (false, format!("denied: {reason}"), 0),
            PolicyVerdict::NeedsApproval { reason } => {
                (false, format!("needs approval: {reason}"), 0)
            }
        };

        tracing::info!(
            decision_id = %decision_id,
            market_id = pos.market_id.0,
            tier = ?tier,
            verdict = ?verdict,
            submitted,
            outcome = %outcome,
            classify_us,
            policy_us,
            submit_us,
            "reflex decision"
        );
        records.push(DecisionRecord {
            decision_id,
            market_id: pos.market_id,
            tier,
            verdict,
            submitted,
            outcome,
            stage_us: (classify_us, policy_us, submit_us),
        });
    }
    Ok(records)
}

/// The P05 daemon body: build the live feed + guarded DRY_RUN executor and
/// sweep on a fixed interval, tolerating transient failures.
///
/// TESTNET mode is wired but stays behind the mode flag until a real API key
/// exists (P05 FALLBACK; STUB-09). P06 replaces this with the supervised
/// pipeline.
pub async fn daemon(cfg: Config) -> anyhow::Result<()> {
    use crate::execution::GuardedExecutor;
    use crate::execution::dry_run::DryRunExecutor;
    use crate::execution::idempotency::IdempotencyStore;
    use crate::perpl::LivePerpl;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let feed = LivePerpl::new(&cfg)?;
    let probe = LivePerpl::new(&cfg)?;
    let store = Arc::new(Mutex::new(IdempotencyStore::load(
        Duration::from_secs(cfg.risk.idempotency_window_secs),
        PathBuf::from("data/idempotency.json"),
    )?));
    let executor = GuardedExecutor::new(
        DryRunExecutor::new(probe, 10, PathBuf::from("data/dryrun-reports.jsonl"), 0),
        Arc::clone(&store),
        feed_probe(&cfg)?,
        Duration::from_secs(3),
    );

    let mut reflex = ReflexState::new();
    let mut day = DayState::default();
    let mut seq: u64 = 0;

    tracing::info!(
        mode = %cfg.execution.mode,
        interval_secs = SWEEP_INTERVAL.as_secs(),
        "reflex daemon started (DRY_RUN executor module P05)"
    );
    loop {
        match sweep(&cfg, &feed, &executor, &mut reflex, &mut day, &mut seq).await {
            Ok(records) => {
                tracing::debug!(decisions = records.len(), "sweep complete");
            }
            Err(err) => {
                tracing::warn!(error = %err, "sweep failed; retrying next interval");
            }
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

/// Second live client used purely as the guard's position probe.
fn feed_probe(cfg: &Config) -> anyhow::Result<crate::perpl::LivePerpl> {
    Ok(crate::perpl::LivePerpl::new(cfg)?)
}

/// `PositionProbe` adapter for fixture replay (bins/tests); the live impl is
/// in `execution::mod`.
impl crate::execution::PositionProbe for crate::perpl::MockPerpl {
    async fn position(&self, market_id: MarketId) -> Result<Option<Position>> {
        let state = self.snapshot().await?;
        Ok(state
            .positions
            .into_iter()
            .find(|position| position.market_id == market_id))
    }
}
