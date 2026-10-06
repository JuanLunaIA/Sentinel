//! Defensive reduce execution (`SPEC-P14.md` §4).
//!
//! # Plan
//!
//! The executor resolves the snapshot (source order in [`crate::snapshot`]),
//! picks the **riskiest** position (smallest `distance_to_liq_pct`; tie →
//! larger `|size| × mark` notional), sizes the reduce as
//! `fraction × |size|` **clamped down** so its notional never exceeds
//! `BREAKER_MAX_REDUCE_USD`, then **quantizes down** with the core sizing rule
//! ([`sentinel_core::order::quantize_size_down`]). Below the market's
//! `min_size`, or with no rankable position, the action degrades to
//! **alert-only** and submits NO order.
//!
//! # Execution
//!
//! - `dry_run` → [`DryRunExecutor`] (fill at mark ± [`DEFAULT_SLIPPAGE_BPS`],
//!   report persisted next to the journal);
//! - `testnet` → [`PerplExecutor`] directly — **no** `GuardedExecutor`; the
//!   breaker re-reads the position afterwards and journals the observed
//!   change. This is a **last-resort path**: it signs and submits live orders
//!   with the operator-provisioned API key.
//!
//! # Journal and alert
//!
//! One JSONL line per action (`{ts_ms, guardian, epoch, mode, market_id,
//! fraction, size, notional_usd, status, detail}`) appended to
//! `BREAKER_JOURNAL`; an alert through
//! [`sentinel::notify::TelegramSink`] when configured, else `tracing::warn`.
//! The alert text starts with an emoji followed by the frozen literal
//! [`ALERT_PREFIX`].

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use rust_decimal::Decimal;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::perpl::PerplExecutor;
use sentinel::execution::{ExecutionStatus, Executor, PositionProbe};
use sentinel::notify::{Alert, AlertKind, AlertSink, TelegramSink};
use sentinel::perpl::auth::ApiKeySigner;
use sentinel::perpl::rest::PerplRest;
use sentinel_core::order::{CloseSide, OrderRequest, OrderType, quantize_size_down};
use sentinel_core::types::{Market, MarketId, Position};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::{BreakerConfig, BreakerMode};
use crate::snapshot::{LivePerplSource, SourcedSnapshot};
use crate::trigger::{guardian_key, unix_ms};

/// Frozen alert literal; the message starts with an emoji prefix followed by
/// this text (`SPEC-P14` §4).
pub const ALERT_PREFIX: &str = "BREAKER: Sentinel unresponsive";

/// Emoji prefixed to every breaker alert message.
pub const ALERT_EMOJI: &str = "🚨";

/// Slippage applied to dry-run fills and carried as the live order cap
/// (matches the daemon's approval-path default of 50 bps).
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Documented fallback lot grid when a snapshot file carries positions but no
/// market metadata (Perpl ETH/BTC report `size_decimals = 3`; see
/// `docs/FACTS.md` §1.5 and `tests/fixtures/perpl/crash-scenario.jsonl`).
pub const FALLBACK_SIZE_DECIMALS: u32 = 3;

/// Documented fallback minimum size when no market metadata is available
/// (Perpl `min_posting_amount` is `0` for the markets in scope).
pub const FALLBACK_MIN_SIZE: Decimal = Decimal::ZERO;

/// Terminal status of one breaker action (journal `status` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// No order was submitted (safe degrade).
    AlertOnly,
    /// Dry-run fill simulated locally.
    Simulated,
    /// Live order accepted for forwarding.
    Submitted,
    /// Live order fully filled (post-verification).
    Filled,
    /// Live order partially filled (post-verification).
    Partial,
    /// Order refused by the venue or failed validation.
    Rejected,
    /// The submission itself failed (transport/auth/config) — loud, journaled.
    Failed,
}

impl ActionStatus {
    /// Wire string (`snake_case`).
    pub fn as_str(self) -> &'static str {
        match self {
            ActionStatus::AlertOnly => "alert_only",
            ActionStatus::Simulated => "simulated",
            ActionStatus::Submitted => "submitted",
            ActionStatus::Filled => "filled",
            ActionStatus::Partial => "partial",
            ActionStatus::Rejected => "rejected",
            ActionStatus::Failed => "failed",
        }
    }
}

/// One breaker-journal line (`SPEC-P14` §4). Decimals serialize as JSON
/// numbers (`fraction`, `size`, `notional_usd`) so plain readers can compare
/// them arithmetically; `null` only when genuinely unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalLine {
    /// Wall-clock timestamp of the action, ms.
    pub ts_ms: u64,
    /// Lowercase guardian address.
    pub guardian: String,
    /// Fired staleness epoch.
    pub epoch: u64,
    /// `dry_run` or `testnet`.
    pub mode: String,
    /// Perpl market id (absent when nothing was rankable).
    pub market_id: Option<u32>,
    /// Configured reduce fraction.
    #[serde(with = "decimal_number")]
    pub fraction: Option<Decimal>,
    /// Submitted/simulated size in base units (`0` when nothing was reduced).
    #[serde(with = "decimal_number")]
    pub size: Option<Decimal>,
    /// Notional of that size in USD (`0` when nothing was reduced).
    #[serde(with = "decimal_number")]
    pub notional_usd: Option<Decimal>,
    /// Terminal status.
    pub status: ActionStatus,
    /// Human-readable detail (reason, clamp/min-size notes, fill info).
    pub detail: String,
}

/// serde helper: `Option<Decimal>` as a JSON number (null when absent).
mod decimal_number {
    use rust_decimal::Decimal;
    use rust_decimal::prelude::ToPrimitive;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize a decimal as an f64 JSON number.
    pub fn serialize<S: Serializer>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(decimal) => serializer.serialize_f64(decimal.to_f64().unwrap_or_default()),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize from a JSON number or numeric string.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Decimal>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(other) => serde_json::from_value(other)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Outcome of planning one reduce action.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStatus {
    /// An order should be submitted.
    Execute,
    /// No order — alert only (safe degrade).
    AlertOnly,
}

/// A fully planned (not yet executed) action.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionPlan {
    /// Execute or degrade.
    pub status: PlanStatus,
    /// Market of the picked position, when one was picked.
    pub market_id: Option<MarketId>,
    /// Configured fraction.
    pub fraction: Decimal,
    /// Planned size (before/without execution), when computable.
    pub size: Option<Decimal>,
    /// Planned notional in USD, when computable.
    pub notional_usd: Option<Decimal>,
    /// The reduce order to submit (only on [`PlanStatus::Execute`]).
    pub order: Option<OrderRequest>,
    /// Why this plan looks the way it does.
    pub detail: String,
}

/// Distance from mark to liquidation, in percent of mark.
///
/// Uses the core formula ([`sentinel_core::risk::distance_to_liq_pct`]) when
/// the market metadata is known; without it, only an exchange-provided
/// `liq_price` can yield a distance (the first-principles derivation needs the
/// maintenance-margin fraction, which is per-market data).
pub fn distance_to_liq_pct(position: &Position, markets: &[Market]) -> Option<Decimal> {
    if let Some(market) = markets
        .iter()
        .find(|market| market.id == position.market_id)
    {
        return sentinel_core::risk::distance_to_liq_pct(position, market);
    }
    if position.size.is_zero() {
        return None;
    }
    let mark = position.mark_price?;
    if mark <= Decimal::ZERO {
        return None;
    }
    let liq = position.liq_price?;
    Some((mark - liq).abs() / mark * Decimal::ONE_HUNDRED)
}

/// A ranked position: the pick candidate with its ranking data.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedPosition<'a> {
    /// The position itself.
    pub position: &'a Position,
    /// Distance to liquidation, percent of mark.
    pub distance_pct: Decimal,
    /// `|size| × mark` (fallback entry) — larger wins ties.
    pub notional: Decimal,
}

/// Pick the riskiest position: smallest distance to liquidation; on an exact
/// distance tie, the larger notional. Positions with no computable distance
/// are skipped; `None` when none is rankable.
pub fn pick_riskiest<'a>(
    positions: &'a [Position],
    markets: &[Market],
) -> Option<RankedPosition<'a>> {
    let mut best: Option<RankedPosition<'a>> = None;
    for position in positions {
        let Some(distance) = distance_to_liq_pct(position, markets) else {
            continue;
        };
        let candidate = RankedPosition {
            position,
            distance_pct: distance,
            notional: position.notional(),
        };
        best = match best {
            None => Some(candidate),
            Some(current) => {
                let candidate_wins = candidate.distance_pct < current.distance_pct
                    || (candidate.distance_pct == current.distance_pct
                        && candidate.notional > current.notional);
                if candidate_wins {
                    Some(candidate)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

/// Plan the defensive reduce from a position set.
///
/// Clamp order is the frozen one: `fraction × |size|`, clamped DOWN so the
/// notional stays `<= max_reduce_usd`, then quantized DOWN to the market lot
/// grid; below `min_size` → alert-only.
pub fn plan_reduce(
    positions: &[Position],
    markets: &[Market],
    fraction: Decimal,
    max_reduce_usd: Decimal,
) -> ActionPlan {
    let Some(ranked) = pick_riskiest(positions, markets) else {
        return ActionPlan {
            status: PlanStatus::AlertOnly,
            market_id: None,
            fraction,
            size: Some(Decimal::ZERO),
            notional_usd: Some(Decimal::ZERO),
            order: None,
            detail: "no position with a computable distance-to-liquidation".to_string(),
        };
    };
    let position = ranked.position;

    let (size_decimals, min_size, fallback_note) = match markets
        .iter()
        .find(|market| market.id == position.market_id)
    {
        Some(market) => (market.size_decimals, market.min_size, ""),
        None => {
            warn!(
                market_id = position.market_id.0,
                "breaker executor: no market metadata; using the documented Perpl \
                 fallback (size_decimals=3, min_size=0)"
            );
            (
                FALLBACK_SIZE_DECIMALS,
                FALLBACK_MIN_SIZE,
                " [fallback market metadata: size_decimals=3, min_size=0]",
            )
        }
    };

    let Some(price) = position.mark_price else {
        // Defensive: a rankable position always carries a mark (distance
        // needs it), so this only fires on future shape changes.
        return ActionPlan {
            status: PlanStatus::AlertOnly,
            market_id: Some(position.market_id),
            fraction,
            size: None,
            notional_usd: None,
            order: None,
            detail: format!(
                "no mark price for market {}{fallback_note}",
                position.market_id.0
            ),
        };
    };

    let raw_size = fraction * position.size.abs();
    let max_size = if max_reduce_usd.is_zero() {
        Decimal::ZERO
    } else {
        max_reduce_usd / price
    };
    let clamped = raw_size.min(max_size);
    let size = quantize_size_down(clamped, size_decimals);
    let notional_usd = size * price;

    if size.is_zero() || size < min_size {
        return ActionPlan {
            status: PlanStatus::AlertOnly,
            market_id: Some(position.market_id),
            fraction,
            size: Some(size),
            notional_usd: Some(notional_usd),
            order: None,
            detail: format!(
                "computed size {size} is below the minimum {} for market {}{fallback_note}",
                min_size.max(Decimal::ZERO),
                position.market_id.0
            ),
        };
    }

    let order = OrderRequest {
        market_id: position.market_id,
        close: if position.is_long() {
            CloseSide::CloseLong
        } else {
            CloseSide::CloseShort
        },
        size,
        order_type: OrderType::Market,
        max_slippage_bps: DEFAULT_SLIPPAGE_BPS,
        size_decimals,
    };
    ActionPlan {
        status: PlanStatus::Execute,
        market_id: Some(position.market_id),
        fraction,
        size: Some(size),
        notional_usd: Some(notional_usd),
        order: Some(order),
        detail: format!(
            "reduce {fraction} × {} = {size} @ mark {price} (notional {notional_usd}, \
             distance {:.2}%{fallback_note})",
            position.size.abs(),
            ranked.distance_pct
        ),
    }
}

/// Position view fixed to one snapshot (the executors re-read it during a
/// submission; the breaker has no live probe of its own).
#[derive(Debug, Clone)]
pub struct SnapshotProbe {
    positions: Vec<Position>,
}

impl SnapshotProbe {
    /// New probe over `positions`.
    pub fn new(positions: Vec<Position>) -> Self {
        Self { positions }
    }
}

impl PositionProbe for SnapshotProbe {
    async fn position(&self, market_id: MarketId) -> sentinel::error::Result<Option<Position>> {
        Ok(self
            .positions
            .iter()
            .find(|position| position.market_id == market_id)
            .cloned())
    }
}

/// Where breaker alerts go.
pub enum AlertDelivery {
    /// Telegram via [`sentinel::notify::TelegramSink`].
    Telegram(TelegramSink),
    /// `tracing::warn` fallback (no Telegram configured).
    Tracing,
}

impl AlertDelivery {
    /// Build from the configuration (Telegram when both keys are set).
    pub fn new(cfg: &BreakerConfig) -> Self {
        match &cfg.telegram {
            Some(telegram) => {
                AlertDelivery::Telegram(TelegramSink::new(&telegram.bot_token, telegram.chat_id))
            }
            None => AlertDelivery::Tracing,
        }
    }

    /// Delivery channel label for logs.
    pub fn channel(&self) -> &'static str {
        match self {
            AlertDelivery::Telegram(_) => "telegram",
            AlertDelivery::Tracing => "tracing",
        }
    }

    /// Deliver one alert. Failures are logged, never fatal (the journal is
    /// the durable record).
    pub async fn send(&self, alert: &Alert) {
        match self {
            AlertDelivery::Telegram(sink) => {
                if let Err(err) = sink.send(alert).await {
                    warn!(error = %err, "breaker alert: telegram delivery failed");
                }
            }
            AlertDelivery::Tracing => {
                warn!(target: "sentinel::alert", text = %alert.text, at_ms = alert.at_ms, "breaker alert");
            }
        }
    }
}

/// The frozen breaker alert text: emoji prefix + [`ALERT_PREFIX`] + detail.
pub fn alert_message(guardian: &str, age_secs: u64, detail: &str) -> String {
    format!("{ALERT_EMOJI} {ALERT_PREFIX}: guardian {guardian} stale {age_secs}s — {detail}")
}

/// Build the [`Alert`] for one fire.
pub fn breaker_alert(guardian: &str, age_secs: u64, detail: &str, at_ms: u64) -> Alert {
    Alert {
        kind: AlertKind::FeedStale { secs: age_secs },
        market_id: None,
        text: alert_message(guardian, age_secs, detail),
        at_ms,
    }
}

/// Dry-run report path derived from the journal
/// (`breaker-journal.jsonl` → `breaker-journal.dry-run.jsonl`).
pub fn dry_run_report_path(journal: &Path) -> PathBuf {
    journal.with_extension("dry-run.jsonl")
}

/// Append one journal line (JSONL, parent directories created on demand).
///
/// # Errors
/// Human-readable string on any IO failure.
pub fn append_journal_line(path: &Path, line: &JournalLine) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "cannot create journal directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let mut text = serde_json::to_string(line)
        .map_err(|err| format!("cannot serialize journal line: {err}"))?;
    text.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("cannot open journal {}: {err}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|err| format!("cannot append journal {}: {err}", path.display()))
}

/// Executes breaker actions: plan → (dry-run | testnet) → journal → alert.
pub struct BreakerExecutor {
    cfg: Arc<BreakerConfig>,
    delivery: AlertDelivery,
}

impl BreakerExecutor {
    /// Build from configuration.
    pub fn new(cfg: Arc<BreakerConfig>) -> Self {
        let delivery = AlertDelivery::new(&cfg);
        Self { cfg, delivery }
    }

    /// Plan against an explicit position/market set (also the unit-test seam).
    pub fn plan(&self, positions: &[Position], markets: &[Market]) -> ActionPlan {
        plan_reduce(
            positions,
            markets,
            self.cfg.fraction,
            self.cfg.max_reduce_usd,
        )
    }

    /// Resolve the snapshot, plan, execute, journal and alert — one fire.
    ///
    /// Never fails: every degradation is journaled (status `alert_only` or
    /// `failed`) and alerted.
    pub async fn execute(
        &self,
        guardian: &Address,
        epoch: u64,
        age_secs: u64,
        max_tier: u8,
        reason: &str,
    ) -> JournalLine {
        let sourced = crate::snapshot::resolve(&self.cfg).await;
        let mut line = self
            .build_line(guardian, epoch, age_secs, max_tier, reason, &sourced)
            .await;
        line.ts_ms = unix_ms();

        if let Err(err) = append_journal_line(&self.cfg.journal, &line) {
            warn!(
                error = %err,
                journal = %self.cfg.journal.display(),
                "breaker executor: journal write failed"
            );
        }
        let alert = breaker_alert(&line.guardian, age_secs, &line.detail, line.ts_ms);
        self.delivery.send(&alert).await;
        info!(
            guardian = %line.guardian,
            epoch,
            status = line.status.as_str(),
            market_id = ?line.market_id,
            channel = self.delivery.channel(),
            "breaker executor: action recorded"
        );
        line
    }

    /// Compose the journal line (execution included); `ts_ms` is finalized by
    /// the caller.
    async fn build_line(
        &self,
        guardian: &Address,
        epoch: u64,
        age_secs: u64,
        max_tier: u8,
        reason: &str,
        sourced: &SourcedSnapshot,
    ) -> JournalLine {
        let plan = self.plan(&sourced.bundle.positions, &sourced.bundle.markets);
        let base_detail = format!(
            "{reason}; snapshot={}; age={age_secs}s; tier={} ({})",
            sourced.origin.as_str(),
            max_tier,
            crate::watcher::tier_name(max_tier)
        );
        let mut line = JournalLine {
            ts_ms: 0,
            guardian: guardian_key(guardian),
            epoch,
            mode: self.cfg.mode.as_str().to_string(),
            market_id: plan.market_id.map(|market| market.0),
            fraction: Some(plan.fraction),
            size: plan.size,
            notional_usd: plan.notional_usd,
            status: ActionStatus::AlertOnly,
            detail: String::new(),
        };

        let (status, execution_detail) = match (&plan.status, &plan.order) {
            (PlanStatus::AlertOnly, _) | (_, None) => {
                (ActionStatus::AlertOnly, plan.detail.clone())
            }
            (PlanStatus::Execute, Some(order)) => match self.cfg.mode {
                BreakerMode::DryRun => {
                    match self
                        .run_dry_run(order, sourced.bundle.positions.clone(), epoch)
                        .await
                    {
                        Ok(report) => (
                            map_execution_status(report.status),
                            format!(
                                "{}; {}; client_order_id {}",
                                plan.detail, report.detail, report.client_order_id
                            ),
                        ),
                        Err(err) => (
                            ActionStatus::Failed,
                            format!("dry-run submission failed: {err}"),
                        ),
                    }
                }
                BreakerMode::Testnet => match self.run_testnet(order, epoch).await {
                    Ok((status, detail)) => (status, format!("{}; {detail}", plan.detail)),
                    Err(err) => (
                        ActionStatus::Failed,
                        format!("testnet submission failed: {err}"),
                    ),
                },
            },
        };
        line.status = status;
        line.detail = format!("{execution_detail}; {base_detail}");
        line
    }

    /// DRY_RUN: reuse [`DryRunExecutor`] (fill at mark ± slippage, report
    /// persisted next to the journal).
    async fn run_dry_run(
        &self,
        order: &OrderRequest,
        positions: Vec<Position>,
        epoch: u64,
    ) -> Result<sentinel::execution::ExecutionReport, String> {
        let probe = SnapshotProbe::new(positions);
        let executor = DryRunExecutor::new(
            probe,
            DEFAULT_SLIPPAGE_BPS,
            dry_run_report_path(&self.cfg.journal),
            epoch,
        );
        executor.submit(order).await.map_err(|err| err.to_string())
    }

    /// TESTNET: submit through [`PerplExecutor`] directly (no
    /// `GuardedExecutor`), then re-read the position and journal the observed
    /// change. Documented last-resort path.
    async fn run_testnet(
        &self,
        order: &OrderRequest,
        epoch: u64,
    ) -> Result<(ActionStatus, String), String> {
        let api_key = self
            .cfg
            .perpl_api_key
            .clone()
            .ok_or_else(|| "PERPL_API_KEY is not configured".to_string())?;
        let api_key_secret = self
            .cfg
            .perpl_api_key_secret
            .clone()
            .ok_or_else(|| "PERPL_API_KEY_SECRET is not configured".to_string())?;
        let base_url = self
            .cfg
            .perpl_api_url
            .clone()
            .ok_or_else(|| "PERPL_API_URL is not configured".to_string())?;

        // Resolve the exchange account id from the wallet (PERPL_ACCOUNT is a
        // wallet address, not the numeric id).
        let resolve_signer =
            ApiKeySigner::from_parts(&api_key, &api_key_secret, self.cfg.perpl_chain_id)
                .map_err(|err| format!("signer: {err}"))?;
        let rest = PerplRest::new(base_url.clone(), resolve_signer)
            .map_err(|err| format!("REST client: {err}"))?;
        let wallet = rest
            .get_wallet()
            .await
            .map_err(|err| format!("wallet read failed (account id unknown): {err}"))?;
        let (account_id, _, _, _, _, _) = sentinel::perpl::types::parse_wallet(&wallet)
            .map_err(|err| format!("wallet parse failed: {err}"))?;

        let signer = ApiKeySigner::from_parts(&api_key, &api_key_secret, self.cfg.perpl_chain_id)
            .map_err(|err| format!("signer: {err}"))?;
        let executor = PerplExecutor::new(
            base_url,
            signer,
            account_id,
            SnapshotProbe::new(Vec::new()),
            epoch,
        )
        .map_err(|err| format!("executor build failed: {err}"))?;
        let report = executor
            .submit(order)
            .await
            .map_err(|err| format!("submission failed: {err}"))?;

        // Re-verify by re-reading positions (last-resort path: no guard).
        let note = match LivePerplSource::new(&self.cfg) {
            Ok(source) => match source.load().await {
                Ok(bundle) => match bundle
                    .positions
                    .iter()
                    .find(|position| position.market_id == order.market_id)
                {
                    Some(position) => {
                        let before = match order.close {
                            CloseSide::CloseLong => position.size + order.size,
                            CloseSide::CloseShort => position.size - order.size,
                        };
                        format!(
                            "re-read position size {} (was {before}), reduce ordered {}",
                            position.size, order.size
                        )
                    }
                    None => format!("re-read: market {} flat after submit", order.market_id.0),
                },
                Err(err) => format!("re-read unavailable: {err}"),
            },
            Err(err) => format!("re-read unavailable: {err}"),
        };
        Ok((
            map_execution_status(report.status),
            format!(
                "{}; {}; client_order_id {}",
                report.detail, note, report.client_order_id
            ),
        ))
    }
}

/// Map the sentinel execution status onto the breaker's journal status.
fn map_execution_status(status: ExecutionStatus) -> ActionStatus {
    match status {
        ExecutionStatus::Simulated => ActionStatus::Simulated,
        ExecutionStatus::Submitted => ActionStatus::Submitted,
        ExecutionStatus::Filled => ActionStatus::Filled,
        ExecutionStatus::Partial => ActionStatus::Partial,
        ExecutionStatus::Rejected => ActionStatus::Rejected,
    }
}

/// Parse a decimal from a JSON value (string or number) — test helper spelled
/// out here so journal assertions do not depend on formatting trivia.
pub fn decimal_from_json(value: &serde_json::Value) -> Option<Decimal> {
    serde_json::from_value(value.clone()).ok()
}

/// `Decimal::from_str` convenience (documented for evidence scripts).
pub fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).expect("valid decimal literal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BreakerConfig;
    use crate::test_support::TempDir;

    use std::collections::HashMap;

    const GUARDIAN: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    fn d(value: &str) -> Decimal {
        decimal(value)
    }

    /// Position with a known mark + exchange liq price (distance = 10 %).
    fn position(market_id: u32, size: &str, mark: &str, liq: &str) -> Position {
        Position {
            market_id: MarketId(market_id),
            symbol: "ETH".to_string(),
            size: d(size),
            entry_price: d(mark),
            mark_price: Some(d(mark)),
            liq_price: Some(d(liq)),
            collateral: d("600"),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: d("10"),
            opened_at: None,
        }
    }

    /// ETH-like market fixture (matches the crash-scenario venue facts).
    fn market(size_decimals: u32, min_size: &str) -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH".to_string(),
            price_decimals: 2,
            size_decimals,
            initial_margin_fraction: d("0.083333"),
            maintenance_margin_fraction: d("0.05"),
            max_leverage: d("12"),
            min_size: d(min_size),
            tick_size: d("0.01"),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    fn test_cfg(journal: &Path, extra: &[(&str, &str)]) -> BreakerConfig {
        let mut vars = HashMap::from([
            (
                "BREAKER_ANCHOR_ADDRESS".to_string(),
                "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
            ),
            ("BREAKER_GUARDIANS".to_string(), GUARDIAN.to_string()),
            ("BREAKER_ARM_SECRET".to_string(), "s".to_string()),
            ("BREAKER_JOURNAL".to_string(), journal.display().to_string()),
        ]);
        for (key, value) in extra {
            vars.insert((*key).to_string(), (*value).to_string());
        }
        BreakerConfig::from_vars(vars).expect("test config")
    }

    #[test]
    fn picks_smallest_distance_then_larger_notional() {
        // Distances: 40 %, 10 %, 10 %. The 10 % pair is broken by notional.
        let a = position(32, "2", "100", "60"); // 40 %, notional 200
        let b = position(32, "-3", "100", "90"); // 10 %, notional 300
        let c = position(32, "9", "100", "90"); // 10 %, notional 900
        let positions = vec![a, b, c];
        let picked = pick_riskiest(&positions, &[]).expect("rankable");
        assert_eq!(picked.distance_pct, d("10"));
        assert_eq!(picked.notional, d("900"));
        assert_eq!(picked.position.size, d("9"));

        // Adding a 5 % position wins over both.
        let mut positions2 = positions.clone();
        positions2.push(position(32, "1", "100", "95"));
        let picked = pick_riskiest(&positions2, &[]).expect("rankable");
        assert_eq!(picked.distance_pct, d("5"));
    }

    #[test]
    fn picks_skip_unrankable_and_empty() {
        assert!(pick_riskiest(&[], &[]).is_none());

        let mut no_liq = position(32, "2", "100", "60");
        no_liq.liq_price = None;
        let mut no_mark = position(32, "2", "100", "60");
        no_mark.mark_price = None;
        let flat = position(32, "0", "100", "60");
        assert!(
            pick_riskiest(&[no_liq, no_mark, flat], &[]).is_none(),
            "nothing rankable without market metadata"
        );

        // With market metadata, the implied liquidation price ranks no_liq.
        let market = market(3, "0");
        let mut implied = position(32, "2", "100", "60");
        implied.liq_price = None;
        implied.collateral = d("40");
        let implied_positions = [implied];
        let picked =
            pick_riskiest(&implied_positions, std::slice::from_ref(&market)).expect("derivable");
        assert!(picked.distance_pct > Decimal::ZERO);
    }

    #[test]
    fn plan_clamps_down_then_quantizes() {
        let positions = vec![position(32, "2", "3000", "2400")];
        // 0.5 × 2 = 1.0 → notional 3000 > 1000 ⇒ clamp to 1000/3000 ⇒ 0.333…
        // ⇒ quantize(3) ⇒ 0.333.
        let plan = plan_reduce(&positions, &[market(3, "0")], d("0.5"), d("1000"));
        assert_eq!(plan.status, PlanStatus::Execute);
        let order = plan.order.expect("order planned");
        assert_eq!(order.size, d("0.333"));
        assert_eq!(order.market_id, MarketId(32));
        assert_eq!(order.close, CloseSide::CloseLong);
        assert_eq!(order.order_type, OrderType::Market);
        assert_eq!(order.max_slippage_bps, DEFAULT_SLIPPAGE_BPS);
        assert_eq!(order.size_decimals, 3);
        let notional = plan.notional_usd.expect("notional");
        assert!(notional <= d("1000"), "clamped notional {notional} <= 1000");
        assert!(
            order.size <= positions[0].size.abs(),
            "never exceeds position"
        );

        // Fine lot grid: 0.5 × 1.23456 = 0.61728 ⇒ 0.6172 at 4 decimals.
        let fine = vec![position(32, "-1.23456", "100", "60")];
        let plan = plan_reduce(&fine, &[market(4, "0")], d("0.5"), d("1000"));
        let order = plan.order.expect("order planned");
        assert_eq!(order.size, d("0.6172"));
        assert_eq!(order.close, CloseSide::CloseShort);
    }

    #[test]
    fn plan_degrades_below_min_size_and_zero_cap() {
        let positions = vec![position(32, "2", "3000", "2400")];
        // Computed 0.333 < min_size 0.5 ⇒ alert-only, no order.
        let plan = plan_reduce(&positions, &[market(3, "0.5")], d("0.5"), d("1000"));
        assert_eq!(plan.status, PlanStatus::AlertOnly);
        assert!(plan.order.is_none());
        assert_eq!(plan.size, Some(d("0.333")));
        assert!(plan.detail.contains("minimum"), "detail: {}", plan.detail);

        // Max reduce 0 ⇒ clamped size 0 ⇒ alert-only.
        let plan = plan_reduce(&positions, &[market(3, "0")], d("0.5"), Decimal::ZERO);
        assert_eq!(plan.status, PlanStatus::AlertOnly);
        assert!(plan.order.is_none());

        // No positions ⇒ alert-only with a clear reason; size resolves to
        // zero (nothing to reduce), never null.
        let plan = plan_reduce(&[], &[], d("0.5"), d("1000"));
        assert_eq!(plan.status, PlanStatus::AlertOnly);
        assert!(plan.market_id.is_none());
        assert_eq!(plan.size, Some(Decimal::ZERO));
        assert!(plan.detail.contains("no position"));
    }

    #[test]
    fn plan_falls_back_to_documented_market_defaults() {
        // No markets in the bundle: quantize at 3 decimals, no min floor.
        let positions = vec![position(32, "2", "3000", "2400")];
        let plan = plan_reduce(&positions, &[], d("0.5"), d("1000"));
        assert_eq!(plan.status, PlanStatus::Execute);
        let order = plan.order.expect("order planned");
        assert_eq!(order.size, d("0.333"));
        assert!(
            plan.detail.contains("fallback market metadata"),
            "detail: {}",
            plan.detail
        );
    }

    #[test]
    fn alert_message_contract() {
        let detail = "reduce 0.5 × 2 = 1.0";
        let message = alert_message("0xabc", 300, detail);
        assert!(
            message.starts_with(&format!("{ALERT_EMOJI} {ALERT_PREFIX}")),
            "{message}"
        );
        assert!(message.contains(ALERT_PREFIX));
        assert!(message.contains("stale 300s"));

        let alert = breaker_alert("0xabc", 300, detail, 42);
        assert_eq!(alert.at_ms, 42);
        assert_eq!(alert.text, message);
        assert!(matches!(alert.kind, AlertKind::FeedStale { secs: 300 }));
    }

    #[test]
    fn dry_run_report_path_is_derived_from_the_journal() {
        assert_eq!(
            dry_run_report_path(Path::new("data/breaker-journal.jsonl")),
            PathBuf::from("data/breaker-journal.dry-run.jsonl")
        );
    }

    #[tokio::test]
    async fn dry_run_flow_writes_journal_and_report() {
        let dir = TempDir::new("dry-run");
        let journal = dir.join("breaker-journal.jsonl");
        // Snapshot file: full object form so the plan sees real market data.
        let snapshot_path = dir.join("snapshot.json");
        let position_json = r#"{
            "market_id": 32,
            "symbol": "ETH",
            "size": "2",
            "entry_price": "3000",
            "mark_price": "3000",
            "liq_price": "2400",
            "collateral": "600",
            "unrealized_pnl": "0",
            "margin_ratio": null,
            "leverage": "10",
            "opened_at": null
        }"#;
        let market_json = r#"{
            "id": 32,
            "symbol": "ETH",
            "base": "ETH",
            "price_decimals": 2,
            "size_decimals": 3,
            "initial_margin_fraction": "0.083333",
            "maintenance_margin_fraction": "0.05",
            "max_leverage": "12",
            "min_size": "0",
            "tick_size": "0.01",
            "maker_fee_micros": 45,
            "taker_fee_micros": 345,
            "order_ttl_blocks": 20
        }"#;
        std::fs::write(
            &snapshot_path,
            format!(r#"{{"positions":[{position_json}],"markets":[{market_json}]}}"#),
        )
        .expect("write snapshot");

        let cfg = Arc::new(test_cfg(
            &journal,
            &[
                ("BREAKER_FRACTION", "0.5"),
                ("BREAKER_MAX_REDUCE_USD", "1000"),
                (
                    "BREAKER_SNAPSHOT_FILE",
                    snapshot_path.to_str().expect("path"),
                ),
            ],
        ));
        let executor = BreakerExecutor::new(Arc::clone(&cfg));
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");

        let line = executor.execute(&guardian, 7, 300, 3, "unit-test").await;
        assert_eq!(line.status, ActionStatus::Simulated);
        assert_eq!(line.mode, "dry_run");
        assert_eq!(line.market_id, Some(32));
        assert_eq!(line.size, Some(d("0.333")));
        assert!(
            line.detail.contains("client_order_id sentinel-32-8"),
            "detail: {}",
            line.detail
        );

        // Journal: exactly one line with exactly the frozen §4 keys.
        let text = std::fs::read_to_string(&journal).expect("journal");
        assert_eq!(text.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("journal JSON");
        let mut keys: Vec<&str> = parsed
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "detail",
                "epoch",
                "fraction",
                "guardian",
                "market_id",
                "mode",
                "notional_usd",
                "size",
                "status",
                "ts_ms",
            ],
            "journal line shape: {parsed}"
        );
        assert_eq!(parsed["guardian"], GUARDIAN.to_lowercase());
        assert_eq!(parsed["epoch"], 7);
        assert_eq!(parsed["status"], "simulated");
        // Decimals are JSON numbers (the journal contract the executors and
        // the verifier harness rely on).
        assert!(parsed["fraction"].is_number(), "{parsed}");
        assert!(parsed["size"].is_number(), "{parsed}");
        assert!(parsed["notional_usd"].is_number(), "{parsed}");
        assert_eq!(parsed["fraction"].as_f64(), Some(0.5));
        assert_eq!(parsed["size"].as_f64(), Some(0.333));
        assert_eq!(parsed["notional_usd"].as_f64(), Some(999.0));
        assert_eq!(decimal_from_json(&parsed["fraction"]), Some(d("0.5")));
        assert_eq!(decimal_from_json(&parsed["size"]), Some(d("0.333")));
        assert_eq!(
            decimal_from_json(&parsed["notional_usd"]),
            Some(d("999.000"))
        );
        assert!(parsed["ts_ms"].as_u64().expect("ts") > 0);

        // The upstream DryRunExecutor report landed next to the journal.
        let report_path = dry_run_report_path(&journal);
        let report_text = std::fs::read_to_string(&report_path).expect("dry-run report");
        let report: serde_json::Value =
            serde_json::from_str(report_text.trim()).expect("report JSON");
        assert_eq!(report["status"], "Simulated");
        assert_eq!(report["client_order_id"], "sentinel-32-8");
        assert_eq!(report["filled_size"], "0.333");
    }

    #[tokio::test]
    async fn execute_degrades_to_alert_only_without_a_snapshot_source() {
        let dir = TempDir::new("no-source");
        let journal = dir.join("journal.jsonl");
        let cfg = Arc::new(test_cfg(&journal, &[]));
        let executor = BreakerExecutor::new(Arc::clone(&cfg));
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");

        let line = executor.execute(&guardian, 1, 200, 3, "unit-test").await;
        assert_eq!(line.status, ActionStatus::AlertOnly);
        assert!(line.market_id.is_none());
        assert_eq!(line.size, Some(Decimal::ZERO));
        assert!(
            line.detail.contains("snapshot=none"),
            "detail: {}",
            line.detail
        );

        let text = std::fs::read_to_string(&journal).expect("journal");
        assert_eq!(text.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("journal JSON");
        assert!(parsed["size"].is_number(), "{parsed}");
        assert_eq!(parsed["size"].as_f64(), Some(0.0));
    }

    #[test]
    fn append_journal_creates_parent_directories() {
        let dir = TempDir::new("append");
        let nested = dir.join("a/b/c.jsonl");
        let line = JournalLine {
            ts_ms: 1,
            guardian: "0x0".to_string(),
            epoch: 0,
            mode: "dry_run".to_string(),
            market_id: None,
            fraction: Some(d("0.5")),
            size: None,
            notional_usd: None,
            status: ActionStatus::AlertOnly,
            detail: "x".to_string(),
        };
        append_journal_line(&nested, &line).expect("append");
        append_journal_line(&nested, &line).expect("append again");
        let text = std::fs::read_to_string(&nested).expect("read");
        assert_eq!(text.lines().count(), 2);
    }
}
