//! Pure command handlers — no teloxide types (`SPEC-P11.md` §2).
//!
//! Every handler is deterministic given `now_ms`; the teloxide layer only
//! converts [`Reply`] into bot API calls.
//!
//! **Status (P11):** implemented by agent `bot-handlers`.
//!
//! Behaviour highlights (`SPEC-P11.md` §§6–7):
//!
//! - `/close` runs the same [`PolicyEngine`] gate as every other intent
//!   (`Manual` source); an admitted order is sized with
//!   [`reduce_by_fraction`], queued for approval and journaled as a `pending`
//!   human execution before the confirm keyboard goes out (audit-before-action,
//!   SPEC-P10 invariant #2).
//! - `/pause` plus a typed `PAUSE` flips both the shared kill-switch flag and
//!   the policy overlay (`kill_switch`), so the pipeline denies everything.
//! - Approve/Deny callbacks and the 5-minute expiry pass journal their
//!   outcomes through the same hash-chained journal the pipeline writes to.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::audit::{AuditJournal, OutcomeRecord, Trigger, hash_input};
use sentinel_core::order::reduce_by_fraction;
use sentinel_core::policy::{DayState, PolicyConfig, PolicyContext, PolicyEngine, PolicySource};
use sentinel_core::risk::{self, RiskThresholds};
use sentinel_core::types::{
    AccountState, Decision, DecisionAction, Intent, Market, MarketId, PolicyVerdict, RiskTier,
    Urgency,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::bot::approvals::{APPROVAL_TTL_MS, ApprovalQueue, PendingApproval};
use crate::bot::commands::Command;
use crate::bot::execution::HumanExecutor;
use crate::bot::format::{self, PositionRow};
use crate::bot::policy_admin::{SharedPolicy, apply_to_config};
use crate::brain::engine::{ConsultInput, StrategyEngine};
use crate::brain::prompts::{PolicySummary, ReflexSummary, SmartMoneyContext};
use crate::brain::providers::Provider;
use crate::config::Config;
use crate::execution::Executor as _;
use crate::health::HealthState;
use crate::nansen::spend::SpendLedger;
use crate::pipeline::LiveState;

/// One bot reply (already MarkdownV2-escaped by the builder).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// Message body.
    pub text: String,
    /// Optional inline keyboard: rows of `(label, callback_data)`.
    pub keyboard: Option<Vec<Vec<(String, String)>>>,
}

impl Reply {
    /// Plain text reply (no keyboard).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            keyboard: None,
        }
    }
}

/// Shared handler state (built by `main.rs`).
pub struct BotContext<P: Provider, F: Provider> {
    /// Application configuration (effective at bot start).
    pub cfg: Config,
    /// Live account state (shared with the pipeline).
    pub state: Arc<Mutex<LiveState>>,
    /// Health handle (feed freshness).
    pub health: Arc<HealthState>,
    /// Audit journal (when open).
    pub journal: Option<Arc<Mutex<AuditJournal>>>,
    /// Policy overlay (shared with the pipeline).
    pub policy: Arc<SharedPolicy>,
    /// Approval queue.
    pub approvals: Arc<ApprovalQueue>,
    /// Strategy engine (None when keys/features are unavailable).
    pub engine: Option<StrategyEngine<P, F>>,
    /// Executor for human-approved orders (None ⇒ orders cannot execute).
    pub executor: Option<HumanExecutor>,
    /// Spend ledger handle.
    pub spend_ledger: Option<SpendLedger>,
    /// Kill switch (shared with the pipeline).
    pub kill: Arc<AtomicBool>,
    /// Set when `/pause` awaits the typed `PAUSE` confirmation.
    pub pause_pending: AtomicBool,
}

/// Sliding-hour window for the Nansen call budget, ms.
const HOUR_MS: u64 = 3_600_000;
/// Sliding-day window for the Nansen cost total, ms.
const DAY_MS: u64 = 86_400_000;
/// Slippage cap carried by human close orders, bps.
const CLOSE_SLIPPAGE_BPS: u16 = 50;

/// Route one command to its reply.
pub async fn handle<P: Provider + Sync, F: Provider + Sync>(
    cmd: Command,
    ctx: &BotContext<P, F>,
    now_ms: u64,
) -> Reply {
    match cmd {
        Command::Start | Command::Help => onboarding(ctx),
        Command::Status => status(ctx, now_ms).await,
        Command::Risk { market } => risk(ctx, market, now_ms).await,
        Command::Close { market, fraction } => close(ctx, market, fraction, now_ms).await,
        Command::Policy => policy_view(ctx),
        Command::PolicySet { key, value } => policy_set(ctx, &key, &value),
        Command::Approve { id } => handle_approval_callback(ctx, &id, true, now_ms).await,
        Command::Deny { id } => handle_approval_callback(ctx, &id, false, now_ms).await,
        Command::Pause => pause(ctx),
        Command::Resume => resume(ctx, now_ms).await,
        Command::Audit { n } => audit(ctx, n).await,
        Command::Spend => spend(ctx, now_ms),
        Command::Mode => mode(ctx),
    }
}

/// Non-command text (the typed `PAUSE` confirmation).
pub async fn handle_text<P: Provider + Sync, F: Provider + Sync>(
    text: &str,
    ctx: &BotContext<P, F>,
    now_ms: u64,
) -> Option<Reply> {
    if !(text.trim().eq_ignore_ascii_case("PAUSE")
        && ctx.pause_pending.swap(false, Ordering::SeqCst))
    {
        return None;
    }
    ctx.kill.store(true, Ordering::SeqCst);
    // The overlay write is best-effort: the in-memory kill flag already
    // denies every intent even if the file cannot be persisted.
    let _ = ctx.policy.set_key("kill_switch", "true");
    let payload = json!({ "action": "kill_switch", "state": "paused" });
    let record = journal_record(
        Trigger::System,
        &account_label(&ctx.cfg),
        None,
        hash_input(&[&payload]),
        payload.clone(),
        json!({ "verdict": "kill_switch" }),
        payload,
    );
    write_record(&ctx.journal, &record, now_ms).await;
    Some(escaped_reply(
        "Kill switch engaged — every intent is denied until /resume.",
    ))
}

/// Approve/Deny inline-button callback.
pub async fn handle_approval_callback<P: Provider + Sync, F: Provider + Sync>(
    ctx: &BotContext<P, F>,
    id: &str,
    approve: bool,
    now_ms: u64,
) -> Reply {
    let Some(pending) = ctx.approvals.remove(id) else {
        return escaped_reply(format!("approval {id} expired or unknown"));
    };
    let account = account_label(&ctx.cfg);
    let input_hash = approval_input_hash(&pending);
    let decision = approval_decision(&pending);

    if !approve {
        let record = journal_record(
            Trigger::Human,
            &account,
            Some(pending.market_id),
            input_hash,
            decision,
            json!({ "verdict": "denied", "note": "human denial" }),
            json!({ "status": "denied", "executor": "human" }),
        );
        write_record(&ctx.journal, &record, now_ms).await;
        return escaped_reply(format!("approval {} denied", pending.id));
    }

    let Some(executor) = &ctx.executor else {
        let record = journal_record(
            Trigger::Human,
            &account,
            Some(pending.market_id),
            input_hash,
            decision,
            json!({ "verdict": "unavailable", "note": "no executor configured" }),
            json!({
                "status": "failed",
                "error": "no executor configured",
                "executor": "human",
            }),
        );
        write_record(&ctx.journal, &record, now_ms).await;
        return escaped_reply(format!(
            "no executor configured — approval {} cannot be executed",
            pending.id
        ));
    };

    match executor.submit(&pending.order).await {
        Ok(report) => {
            let report_status = format!("{:?}", report.status).to_lowercase();
            let record = journal_record(
                Trigger::Human,
                &account,
                Some(pending.market_id),
                input_hash,
                decision,
                json!({ "verdict": "human_approved" }),
                json!({
                    "status": "human_approved",
                    "report_status": report_status.clone(),
                    "order_id": report.client_order_id,
                    "tx_hash": report.tx_hash,
                    "fill": {
                        "filled_size": report.filled_size.to_string(),
                        "avg_price": report.avg_price.map(|price| price.to_string()),
                        "status": report_status,
                    },
                    "executor": "human",
                }),
            );
            write_record(&ctx.journal, &record, now_ms).await;
            let price = report
                .avg_price
                .map(|price| price.to_string())
                .unwrap_or_else(|| "n/a".to_string());
            escaped_reply(format!(
                "approval {} executed — {}, filled {} at {price}, order {}",
                pending.id, report_status, report.filled_size, report.client_order_id
            ))
        }
        Err(err) => {
            let record = journal_record(
                Trigger::Human,
                &account,
                Some(pending.market_id),
                input_hash,
                decision,
                json!({ "verdict": "failed" }),
                json!({
                    "status": "failed",
                    "error": err.to_string(),
                    "executor": "human",
                }),
            );
            write_record(&ctx.journal, &record, now_ms).await;
            escaped_reply(format!("approval {} execution failed: {err}", pending.id))
        }
    }
}

/// Periodic expiry pass; returns notifications to deliver.
pub async fn housekeeping<P: Provider + Sync, F: Provider + Sync>(
    ctx: &BotContext<P, F>,
    now_ms: u64,
) -> Vec<Reply> {
    let expired = ctx.approvals.expire_due(now_ms);
    let mut replies = Vec::with_capacity(expired.len());
    for pending in expired {
        let record = journal_record(
            Trigger::Human,
            &account_label(&ctx.cfg),
            Some(pending.market_id),
            approval_input_hash(&pending),
            approval_decision(&pending),
            json!({ "verdict": "expired", "note": "approval ttl elapsed" }),
            json!({ "status": "expired", "executor": "human" }),
        );
        write_record(&ctx.journal, &record, now_ms).await;
        replies.push(escaped_reply(format!("approval {} expired", pending.id)));
    }
    replies
}

// ---- commands ----------------------------------------------------------------

/// `/status` — positions, distance-to-liq tiers, feed freshness and mode.
async fn status<P: Provider, F: Provider>(ctx: &BotContext<P, F>, now_ms: u64) -> Reply {
    let thresholds = risk_thresholds(&ctx.cfg);
    let (rows, free_balance) = {
        let state = ctx.state.lock().await;
        let free_balance = state
            .account
            .as_ref()
            .map(|account| account.free_balance.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        let mut rows = Vec::new();
        if let Some(account) = &state.account {
            for position in &account.positions {
                let market = state
                    .markets
                    .iter()
                    .find(|market| market.id == position.market_id);
                let (distance_pct, tier_emoji) =
                    match market.and_then(|market| risk::distance_to_liq_pct(position, market)) {
                        Some(distance) => (
                            format!("{:.2}%", distance.round_dp(2)),
                            format::tier_emoji(&risk::tier(distance, &thresholds)).to_string(),
                        ),
                        None => ("n/a".to_string(), "⚪".to_string()),
                    };
                rows.push(PositionRow {
                    symbol: position.symbol.clone(),
                    market_id: position.market_id.0,
                    size: position.size.to_string(),
                    entry: position.entry_price.to_string(),
                    mark: position
                        .mark_price
                        .map(|mark| mark.to_string())
                        .unwrap_or_else(|| "n/a".to_string()),
                    distance_pct,
                    tier_emoji,
                    collateral: position.collateral.to_string(),
                    upnl: position.unrealized_pnl.to_string(),
                });
            }
        }
        (rows, free_balance)
    };
    let feed_age_s = feed_age_s(ctx, now_ms);
    let mode = ctx.cfg.execution.mode.to_string();
    Reply::text(format::status_card(&rows, &free_balance, feed_age_s, &mode))
}

/// `/risk [market]` — force a strategy consult and render its decision.
async fn risk<P: Provider, F: Provider>(
    ctx: &BotContext<P, F>,
    market: Option<u32>,
    now_ms: u64,
) -> Reply {
    let Some(engine) = &ctx.engine else {
        return escaped_reply("DEGRADED: strategy engine unavailable (check provider keys)");
    };
    let (account, markets) = {
        let state = ctx.state.lock().await;
        (state.account.clone(), state.markets.clone())
    };
    let Some(account) = account else {
        return escaped_reply("DEGRADED: no account snapshot yet");
    };
    let focus = match market {
        Some(id) => MarketId(id),
        None => match account.positions.first() {
            Some(position) => position.market_id,
            None => return escaped_reply("DEGRADED: no positions available to consult"),
        },
    };
    let asset = markets
        .iter()
        .find(|market| market.id == focus)
        .map(|market| market.symbol.clone())
        .unwrap_or_else(|| focus.0.to_string());
    let input = ConsultInput {
        account: account.clone(),
        markets,
        focus_market: focus,
        policy: policy_summary(&ctx.cfg),
        sm: SmartMoneyContext::unavailable(asset),
        reflex: ReflexSummary::default(),
    };
    match engine.consult(&input, now_ms).await {
        Ok(outcome) => {
            let decision = &outcome.decision;
            let action = action_label(decision.action);
            let amount = decision.amount.map(|amount| amount.to_string());
            let confidence = decision.confidence.to_string();
            let verdict = consulted_verdict(ctx, &account, &input.markets, decision);
            Reply::text(format::risk_card(
                decision.market_id,
                &action,
                amount.as_deref(),
                &confidence,
                urgency_label(decision.urgency),
                &decision.reason,
                &outcome.provider_used,
                &verdict,
            ))
        }
        Err(err) => escaped_reply(format!("DEGRADED: {err}")),
    }
}

/// `/close <market> [fraction]` — policy-gated human reduce with a confirm
/// keyboard; the order is queued for approval and journaled as pending.
async fn close<P: Provider, F: Provider>(
    ctx: &BotContext<P, F>,
    market: u32,
    fraction: Option<Decimal>,
    now_ms: u64,
) -> Reply {
    let fraction = fraction.unwrap_or(Decimal::ONE);
    if fraction <= Decimal::ZERO || fraction > Decimal::ONE {
        return escaped_reply(format!("invalid fraction {fraction} — must be in (0, 1]"));
    }

    let (account, position, market_info) = {
        let state = ctx.state.lock().await;
        let Some(account) = state.account.clone() else {
            return escaped_reply("no account snapshot yet — try /status in a moment");
        };
        let Some(position) = account
            .positions
            .iter()
            .find(|position| position.market_id == MarketId(market))
            .cloned()
        else {
            return escaped_reply(format!("no position for market {market}"));
        };
        let Some(market_info) = state
            .markets
            .iter()
            .find(|info| info.id == MarketId(market))
            .cloned()
        else {
            return escaped_reply(format!("market {market} not loaded"));
        };
        (account, position, market_info)
    };

    let thresholds = risk_thresholds(&ctx.cfg);
    let tier = risk::distance_to_liq_pct(&position, &market_info)
        .map(|distance| risk::tier(distance, &thresholds))
        .unwrap_or(RiskTier::Green);

    let intent = Intent::Reduce {
        fraction,
        reason: "human close request".to_string(),
    };
    let verdict = PolicyEngine::evaluate(
        &intent,
        &account,
        &policy_config(&ctx.cfg, ctx.policy.kill_switch()),
        &DayState { actions_today: 0 },
        &PolicyContext {
            source: PolicySource::Manual,
            tier,
            market_id: MarketId(market),
        },
    );
    if let PolicyVerdict::Deny { reason } = &verdict {
        return escaped_reply(format!("close denied: {reason}"));
    }

    let Some(order) = reduce_by_fraction(&position, fraction, &market_info, CLOSE_SLIPPAGE_BPS)
    else {
        return escaped_reply(format!("size below lot/min for market {market}"));
    };
    let mark = position.mark_price.unwrap_or(Decimal::ZERO);
    let notional = order.size * mark;

    let symbol = market_info.symbol.clone();
    let id = ApprovalQueue::id_for(&format!("close-{market}-{now_ms}"), now_ms);
    let approval = PendingApproval {
        id: id.clone(),
        market_id: market,
        summary: format!("close {fraction} of {symbol} market {market}"),
        order: order.clone(),
        decision_ref: None,
        created_ms: now_ms,
        expires_ms: now_ms.saturating_add(APPROVAL_TTL_MS),
    };
    let id = ctx.approvals.enqueue(approval.clone());

    let record = journal_record(
        Trigger::Human,
        &account_label(&ctx.cfg),
        Some(market),
        approval_input_hash(&approval),
        approval_decision(&approval),
        json!({ "verdict": verdict_text(&verdict), "note": "human close request" }),
        json!({ "status": "pending", "executor": "human" }),
    );
    write_record(&ctx.journal, &record, now_ms).await;

    Reply {
        text: format::closeconfirm_card(
            market,
            &symbol,
            &order.size.to_string(),
            &notional.to_string(),
        ),
        keyboard: Some(vec![vec![
            ("✅ Execute".to_string(), format!("approve:{id}")),
            ("❌ Cancel".to_string(), format!("deny:{id}")),
        ]]),
    }
}

/// `/pause` — arm the typed confirmation; the kill switch flips in
/// [`handle_text`] when the operator sends the literal `PAUSE`.
fn pause<P: Provider, F: Provider>(ctx: &BotContext<P, F>) -> Reply {
    ctx.pause_pending.store(true, Ordering::SeqCst);
    escaped_reply(
        "Type PAUSE (uppercase) as your next message to confirm the kill switch. \
         All intents will be denied until /resume.",
    )
}

/// `/resume` — release the kill switch (flag + overlay).
async fn resume<P: Provider, F: Provider>(ctx: &BotContext<P, F>, now_ms: u64) -> Reply {
    ctx.kill.store(false, Ordering::SeqCst);
    let _ = ctx.policy.set_key("kill_switch", "false");
    let payload = json!({ "action": "kill_switch", "state": "resumed" });
    let record = journal_record(
        Trigger::System,
        &account_label(&ctx.cfg),
        None,
        hash_input(&[&payload]),
        payload.clone(),
        json!({ "verdict": "kill_switch" }),
        payload,
    );
    write_record(&ctx.journal, &record, now_ms).await;
    escaped_reply("Kill switch released — intents flow through the policy gate again.")
}

/// `/policy` — the effective overlay + caps, one `key = value` line each.
fn policy_view<P: Provider, F: Provider>(ctx: &BotContext<P, F>) -> Reply {
    Reply::text(format::policy_card(&policy_lines(ctx)))
}

/// `/policy set <key> <value>` — validated overlay mutation + refreshed card.
fn policy_set<P: Provider, F: Provider>(ctx: &BotContext<P, F>, key: &str, value: &str) -> Reply {
    match ctx.policy.set_key(key, value) {
        Ok(_) => {
            let mut text = format::escape_md2(&format!("policy updated: {key} = {value}"));
            text.push_str("\n\n");
            text.push_str(&format::policy_card(&policy_lines(ctx)));
            Reply::text(text)
        }
        Err(err) => escaped_reply(format!("policy update rejected: {err}")),
    }
}

/// `/audit [n]` — the trailing `n` journal entries plus the on-chain
/// cross-check pointer.
async fn audit<P: Provider, F: Provider>(ctx: &BotContext<P, F>, n: usize) -> Reply {
    let Some(journal) = &ctx.journal else {
        return escaped_reply("audit journal unavailable");
    };
    let limit = n.min(50);
    let guard = journal.lock().await;
    let from_seq = guard
        .seq()
        .saturating_sub(u64::try_from(limit).unwrap_or(u64::MAX));
    let entries = match guard.read_entries(from_seq, limit) {
        Ok(entries) => entries,
        Err(err) => return escaped_reply(format!("audit read failed: {err}")),
    };
    drop(guard);
    let mut text = format::audit_list(&entries, None);
    text.push_str("\n\n");
    text.push_str(&format::escape_md2(
        "On-chain cross-check: cargo run --bin audit-verify",
    ));
    Reply::text(text)
}

/// `/spend` — Nansen x402 totals (sliding hour/day) + last five purchases.
fn spend<P: Provider, F: Provider>(ctx: &BotContext<P, F>, now_ms: u64) -> Reply {
    let Some(ledger) = &ctx.spend_ledger else {
        return escaped_reply("spend ledger unavailable");
    };
    let calls_hour = ledger.calls_since(now_ms, HOUR_MS);
    let cost_day = ledger.cost_since(now_ms, DAY_MS).to_string();
    let entries = ledger.load();
    let start = entries.len().saturating_sub(5);
    let recent: Vec<String> = entries[start..]
        .iter()
        .map(|entry| format!("{} · ${} · {}", entry.endpoint, entry.cost_usd, entry.ts_ms))
        .collect();
    Reply::text(format::spend_card(
        calls_hour,
        &cost_day,
        ctx.cfg.nansen.max_calls_per_hour,
        &recent,
    ))
}

/// `/mode` — honest about the restart requirement.
fn mode<P: Provider, F: Provider>(ctx: &BotContext<P, F>) -> Reply {
    escaped_reply(format!(
        "mode: {} — switching requires a config-level restart",
        ctx.cfg.execution.mode
    ))
}

/// `/start` / `/help` — onboarding card.
fn onboarding<P: Provider, F: Provider>(ctx: &BotContext<P, F>) -> Reply {
    escaped_reply(format!(
        "Sentinel — verifiable AI risk guardian for isolated-margin perpetuals on Perpl (Monad).\n\
         mode: {mode}\n\n\
         I watch positions, compute risk tiers, run reflex de-risking and LLM strategy consults, \
         and gate every intent through the policy engine before anything reaches an executor.\n\n\
         Commands:\n\
         • /status — positions, distance-to-liquidation tiers and feed freshness\n\
         • /risk [market] — force a strategy consult now\n\
         • /close <market> [fraction] — policy-gated human reduce with approve/deny buttons\n\
         • /policy [set <key> <value>] — view or edit the policy overlay\n\
         • /audit [n] — recent hash-chained journal entries\n\
         • /spend — Nansen x402 budget totals\n\
         • /pause then type PAUSE · /resume — the kill switch\n\
         • /mode — current execution mode\n\n\
         Safety: the kill switch denies every intent; orders only run through the guarded \
         executor; every decision is journaled in a hash-chained audit log.",
        mode = ctx.cfg.execution.mode
    ))
}

// ---- helpers -----------------------------------------------------------------

/// A reply whose text is plain input and gets MarkdownV2-escaped here.
fn escaped_reply(text: impl AsRef<str>) -> Reply {
    Reply::text(format::escape_md2(text.as_ref()))
}

/// Feed age in seconds (`None` until the first applied feed event).
fn feed_age_s<P: Provider, F: Provider>(ctx: &BotContext<P, F>, now_ms: u64) -> Option<u64> {
    let last_feed_ms = ctx.health.last_feed_ms.load(Ordering::Relaxed);
    if last_feed_ms == 0 {
        None
    } else {
        Some(now_ms.saturating_sub(last_feed_ms) / 1_000)
    }
}

/// Risk thresholds from the (current) effective configuration.
fn risk_thresholds(cfg: &Config) -> RiskThresholds {
    RiskThresholds {
        soft: cfg.risk.soft_pct,
        warn: cfg.risk.warn_pct,
        hard: cfg.risk.hard_pct,
    }
}

/// Policy limits mapped from the effective configuration.
fn policy_config(cfg: &Config, kill_switch: bool) -> PolicyConfig {
    PolicyConfig {
        market_allowlist: cfg
            .risk
            .market_allowlist
            .iter()
            .map(|id| MarketId(*id))
            .collect(),
        max_order_size_usd: cfg.risk.max_order_size_usd,
        max_daily_actions: cfg.risk.max_daily_actions,
        require_approval_above_usd: cfg.risk.require_approval_above_usd,
        kill_switch,
    }
}

/// Prompt-side policy summary for a forced consult.
fn policy_summary(cfg: &Config) -> PolicySummary {
    PolicySummary {
        market_allowlist: cfg
            .risk
            .market_allowlist
            .iter()
            .map(|id| MarketId(*id))
            .collect(),
        max_order_size_usd: cfg.risk.max_order_size_usd,
        require_approval_above_usd: cfg.risk.require_approval_above_usd,
        // Documented approximation (SPEC-P11 §2): the configured daily cap
        // itself, not the remaining count — the bot holds no day-state handle.
        daily_actions_left: cfg.risk.max_daily_actions,
    }
}

/// Effective `key = value` lines for the policy card (overlay over config).
fn policy_lines<P: Provider, F: Provider>(ctx: &BotContext<P, F>) -> Vec<String> {
    let overlay = ctx.policy.snapshot();
    let effective = apply_to_config(&ctx.cfg, &overlay);
    let risk = &effective.risk;
    vec![
        format!("risk_soft_pct = {}", risk.soft_pct),
        format!("risk_warn_pct = {}", risk.warn_pct),
        format!("risk_hard_pct = {}", risk.hard_pct),
        format!("reflex_reduce_fraction = {}", risk.reflex_reduce_fraction),
        format!("reflex_orange_fraction = {}", risk.reflex_orange_fraction),
        format!("reflex_cooldown_secs = {}", risk.reflex_cooldown_secs),
        format!("max_order_size_usd = {}", risk.max_order_size_usd),
        format!(
            "require_approval_above_usd = {}",
            risk.require_approval_above_usd
        ),
        format!("max_daily_actions = {}", risk.max_daily_actions),
        format!("kill_switch = {}", ctx.policy.kill_switch()),
    ]
}

/// The policy verdict of a consulted decision (display only): the implied
/// intent is evaluated through the same gate a real action would face.
fn consulted_verdict<P: Provider, F: Provider>(
    ctx: &BotContext<P, F>,
    account: &AccountState,
    markets: &[Market],
    decision: &Decision,
) -> String {
    let market_id = MarketId(decision.market_id);
    let intent = match decision.action {
        DecisionAction::Reduce => {
            let Some(position) = account
                .positions
                .iter()
                .find(|position| position.market_id == market_id)
            else {
                return "n/a".to_string();
            };
            let Some(amount) = decision.amount else {
                return "n/a".to_string();
            };
            if position.size.is_zero() {
                return "n/a".to_string();
            }
            Intent::Reduce {
                fraction: amount / position.size.abs(),
                reason: decision.reason.clone(),
            }
        }
        DecisionAction::Close => Intent::Close {
            reason: decision.reason.clone(),
        },
        DecisionAction::AddCollateral => {
            let Some(amount) = decision.amount else {
                return "n/a".to_string();
            };
            Intent::AddCollateral {
                amount,
                reason: decision.reason.clone(),
            }
        }
        DecisionAction::Hold | DecisionAction::Escalate => return "n/a".to_string(),
    };
    let thresholds = risk_thresholds(&ctx.cfg);
    let tier = account
        .positions
        .iter()
        .find(|position| position.market_id == market_id)
        .and_then(|position| {
            markets
                .iter()
                .find(|market| market.id == market_id)
                .and_then(|market| {
                    risk::distance_to_liq_pct(position, market)
                        .map(|distance| risk::tier(distance, &thresholds))
                })
        })
        .unwrap_or(RiskTier::Green);
    let verdict = PolicyEngine::evaluate(
        &intent,
        account,
        &policy_config(&ctx.cfg, ctx.policy.kill_switch()),
        &DayState { actions_today: 0 },
        &PolicyContext {
            source: PolicySource::Strategy,
            tier,
            market_id,
        },
    );
    verdict_text(&verdict)
}

/// Rendered verdict (`allow` / `deny: …` / `needs_approval: …`).
fn verdict_text(verdict: &PolicyVerdict) -> String {
    match verdict {
        PolicyVerdict::Allow => "allow".to_string(),
        PolicyVerdict::Deny { reason } => format!("deny: {reason}"),
        PolicyVerdict::NeedsApproval { reason } => format!("needs_approval: {reason}"),
    }
}

/// Wire-style action label (`HOLD` … `ADD_COLLATERAL`).
fn action_label(action: DecisionAction) -> String {
    match action {
        DecisionAction::Hold => "HOLD",
        DecisionAction::Reduce => "REDUCE",
        DecisionAction::Close => "CLOSE",
        DecisionAction::AddCollateral => "ADD_COLLATERAL",
        DecisionAction::Escalate => "ESCALATE",
    }
    .to_string()
}

/// Wire-style urgency label (`ROUTINE` / `ELEVATED` / `CRITICAL`).
fn urgency_label(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Routine => "ROUTINE",
        Urgency::Elevated => "ELEVATED",
        Urgency::Critical => "CRITICAL",
    }
}

/// Account label for journal entries (same policy as the pipeline).
fn account_label(cfg: &Config) -> String {
    cfg.perpl
        .account
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Journal timestamp from the injected clock (wall clock as a fallback).
fn journal_ts(now_ms: u64) -> DateTime<Utc> {
    i64::try_from(now_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
}

/// Build one human/system journal record.
#[allow(clippy::too_many_arguments)] // journal fields are explicit by design
fn journal_record(
    trigger: Trigger,
    account: &str,
    market_id: Option<u32>,
    input_hash: String,
    decision: Value,
    policy_verdict: Value,
    execution: Value,
) -> OutcomeRecord {
    OutcomeRecord {
        trigger,
        account: account.to_string(),
        market_id,
        input_hash,
        decision,
        policy_verdict,
        execution,
    }
}

/// Append a record (best-effort; a failed write logs loudly but never
/// blocks the reply — a human is waiting for one).
async fn write_record(
    journal: &Option<Arc<Mutex<AuditJournal>>>,
    record: &OutcomeRecord,
    now_ms: u64,
) {
    let Some(journal) = journal else {
        return;
    };
    let mut guard = journal.lock().await;
    if let Err(err) = guard.record_outcome(record, journal_ts(now_ms)) {
        tracing::warn!(error = %err, "audit journal write failed (human path)");
    }
}

/// Stable inputs hash shared by an approval's pending/outcome entries.
fn approval_input_hash(approval: &PendingApproval) -> String {
    let order = serde_json::to_value(&approval.order).unwrap_or(Value::Null);
    let payload = json!({
        "approval_id": approval.id.clone(),
        "market_id": approval.market_id,
        "created_ms": approval.created_ms,
        "order": order,
    });
    hash_input(&[&payload])
}

/// Decision document reconstructed for an approval's journal entries.
fn approval_decision(approval: &PendingApproval) -> Value {
    json!({
        "action": "close",
        "market_id": approval.market_id,
        "approval_id": approval.id.clone(),
        "summary": approval.summary.clone(),
        "order": serde_json::to_value(&approval.order).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sentinel_core::audit::AuditEntry;
    use sentinel_core::types::Position;

    use super::*;
    use crate::brain::chain::NoProvider;
    use crate::brain::providers::MockProvider;
    use crate::execution::GuardedExecutor;
    use crate::execution::dry_run::DryRunExecutor;
    use crate::execution::idempotency::IdempotencyStore;
    use crate::nansen::spend::SpendEntry;
    use crate::perpl::{AccountEvent, FeedEvent};
    use crate::pipeline::StateProbe;

    const NOW_MS: u64 = 1_700_000_000_000;
    const SECRET_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn utc(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).expect("valid test timestamp")
    }

    fn test_config() -> Config {
        let vars: &[(&str, &str)] = &[
            ("PERPL_ENV", "testnet"),
            ("PERPL_API_KEY_SECRET", SECRET_HEX),
            ("PERPL_API_KEY", "test-api-key"),
            (
                "PERPL_ACCOUNT",
                "0x0000000000000000000000000000000000000007",
            ),
            ("QWEN_API_KEY", "test"),
            ("KIMI_API_KEY", "test"),
            ("NANSEN_PAYER_KEY", "0x11"),
            ("TELEGRAM_ALLOWED_USER_IDS", "1"),
            ("TELOXIDE_TOKEN", "test"),
            ("EXECUTION_MODE", "DRY_RUN"),
            ("MARKET_ALLOWLIST", "32"),
            ("MAX_ORDER_SIZE_USD", "50000"),
            ("REQUIRE_APPROVAL_ABOVE_USD", "2500"),
            ("REFLEX_COOLDOWN_SECS", "60"),
        ];
        Config::from_vars(
            vars.iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        )
        .expect("test config")
    }

    /// ETH-like market on market 32 (mmr 0.05, size grid 3 decimals).
    fn eth_market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH Perp".to_string(),
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

    /// BTC-like market on market 20.
    fn btc_market() -> Market {
        Market {
            id: MarketId(20),
            symbol: "BTC".to_string(),
            base: "BTC Perp".to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: d("0.05"),
            maintenance_margin_fraction: d("0.05"),
            max_leverage: d("20"),
            min_size: Decimal::ZERO,
            tick_size: d("0.01"),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    /// ETH long: entry 2700.00, collateral 13560 ⇒ liq 1479.00, mark 2500.00
    /// ⇒ distance 40.84 % (Green).
    fn eth_position() -> Position {
        let mark = d("2500.00");
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size: d("10"),
            entry_price: d("2700.00"),
            mark_price: Some(mark),
            liq_price: None,
            collateral: d("13560"),
            unrealized_pnl: (mark - d("2700.00")) * d("10"),
            margin_ratio: None,
            leverage: d("2"),
            opened_at: None,
        }
    }

    /// BTC long: entry 30000, collateral 3000 ⇒ liq 28500, mark 35000 ⇒
    /// distance 18.57 % (Yellow).
    fn btc_position() -> Position {
        let mark = d("35000.00");
        Position {
            market_id: MarketId(20),
            symbol: "BTC".to_string(),
            size: d("1"),
            entry_price: d("30000.00"),
            mark_price: Some(mark),
            liq_price: None,
            collateral: d("3000"),
            unrealized_pnl: (mark - d("30000.00")) * d("1"),
            margin_ratio: None,
            leverage: d("10"),
            opened_at: None,
        }
    }

    fn fixture_account() -> AccountState {
        AccountState {
            positions: vec![eth_position(), btc_position()],
            free_balance: d("1000"),
            equity: d("1000"),
            fee_tier: 0,
            snapshot_ts: utc(NOW_MS as i64),
        }
    }

    fn fixture_state() -> Arc<Mutex<LiveState>> {
        let mut live = LiveState::new();
        live.set_markets(vec![eth_market(), btc_market()]);
        let _ = live.apply(&FeedEvent::Account(AccountEvent::Snapshot {
            state: fixture_account(),
        }));
        Arc::new(Mutex::new(live))
    }

    /// A bot context over the fixture state (tempdir journal + overlay).
    struct World {
        ctx: BotContext<MockProvider, NoProvider>,
        dir: tempfile::TempDir,
        state: Arc<Mutex<LiveState>>,
    }

    fn world(
        engine: Option<StrategyEngine<MockProvider, NoProvider>>,
        spend: Option<SpendLedger>,
    ) -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = test_config();
        let mode = cfg.execution.mode;
        let state = fixture_state();
        let health = Arc::new(HealthState::new(mode));
        health.touch_feed(NOW_MS - 5_000);
        let journal = AuditJournal::open(dir.path().join("audit")).expect("open journal");
        let ctx = BotContext {
            cfg,
            state: Arc::clone(&state),
            health,
            journal: Some(Arc::new(Mutex::new(journal))),
            policy: Arc::new(SharedPolicy::load(dir.path().join("policy.json"))),
            approvals: Arc::new(ApprovalQueue::new()),
            engine,
            executor: None,
            spend_ledger: spend,
            kill: Arc::new(AtomicBool::new(false)),
            pause_pending: AtomicBool::new(false),
        };
        World { ctx, dir, state }
    }

    /// Valid REDUCE decision JSON for the mock provider.
    fn reduce_json(market: u32, amount: &str, confidence: &str) -> String {
        format!(
            r#"{{"action":"REDUCE","market_id":{market},"amount":"{amount}","confidence":{confidence},"urgency":"ROUTINE","reason":"trim into strength near the tier boundary"}}"#
        )
    }

    /// Guarded DryRun human executor over the shared fixture probe.
    fn dry_human_executor(dir: &tempfile::TempDir, state: &Arc<Mutex<LiveState>>) -> HumanExecutor {
        let store = IdempotencyStore::load(Duration::from_secs(60), dir.path().join("idem.json"))
            .expect("idempotency store");
        HumanExecutor::Dry(GuardedExecutor::new(
            DryRunExecutor::new(
                StateProbe::new(Arc::clone(state)),
                10,
                dir.path().join("reports.jsonl"),
                0,
            ),
            Arc::new(Mutex::new(store)),
            StateProbe::new(Arc::clone(state)),
            Duration::from_millis(30),
        ))
    }

    async fn journal_entries(ctx: &BotContext<MockProvider, NoProvider>) -> Vec<AuditEntry> {
        let journal = ctx.journal.as_ref().expect("journal attached");
        let guard = journal.lock().await;
        guard.read_entries(0, 100).expect("read journal")
    }

    /// Run `/close`, returning the approval id from the confirm keyboard.
    async fn enqueue_close(world: &World, fraction: &str, now_ms: u64) -> String {
        let reply = handle(
            Command::Close {
                market: 32,
                fraction: Some(d(fraction)),
            },
            &world.ctx,
            now_ms,
        )
        .await;
        let keyboard = reply.keyboard.as_ref().expect("confirm keyboard");
        keyboard[0][0]
            .1
            .strip_prefix("approve:")
            .expect("approve callback data")
            .to_string()
    }

    // ---- /status -------------------------------------------------------------

    #[tokio::test]
    async fn status_card_renders_positions_and_freshness() {
        let world = world(None, None);
        let reply = handle(Command::Status, &world.ctx, NOW_MS).await;
        assert!(reply.keyboard.is_none());
        let text = &reply.text;
        assert!(text.contains("ETH"), "rows: {text}");
        assert!(text.contains("BTC"), "rows: {text}");
        assert!(text.contains('🟢'), "green tier emoji: {text}");
        assert!(text.contains('🟡'), "yellow tier emoji: {text}");
        assert!(text.contains("2500"), "ETH mark: {text}");
        assert!(text.contains("35000"), "BTC mark: {text}");
        assert!(text.contains("1000"), "free balance: {text}");
        assert!(text.contains("DRY"), "mode: {text}");
        assert!(text.contains('5'), "feed age: {text}");
    }

    // ---- /risk ---------------------------------------------------------------

    #[tokio::test]
    async fn risk_with_engine_returns_a_consult_card() {
        let engine = StrategyEngine::new(
            MockProvider::canned(vec![
                reduce_json(32, "1.0", "0.72"),
                reduce_json(32, "1.0", "0.72"),
            ]),
            0,
            d("0.6"),
        );
        let world = world(Some(engine), None);
        let reply = handle(Command::Risk { market: None }, &world.ctx, NOW_MS).await;
        let text = &reply.text;
        assert!(!text.contains("DEGRADED"), "card expected: {text}");
        assert!(text.contains("REDUCE"), "action: {text}");
        assert!(text.contains("ROUTINE"), "urgency: {text}");
        assert!(text.contains("trim into strength"), "reason: {text}");
        assert!(text.contains("mock"), "provider: {text}");
        assert!(text.contains("allow"), "policy verdict: {text}");

        // An explicit market takes the same consult path.
        let reply = handle(Command::Risk { market: Some(32) }, &world.ctx, NOW_MS + 1).await;
        assert!(
            !reply.text.contains("DEGRADED"),
            "card expected: {}",
            reply.text
        );
    }

    #[tokio::test]
    async fn risk_without_engine_degrades_honestly() {
        let world = world(None, None);
        let reply = handle(Command::Risk { market: Some(32) }, &world.ctx, NOW_MS).await;
        assert_eq!(
            reply.text,
            format::escape_md2("DEGRADED: strategy engine unavailable (check provider keys)")
        );
    }

    #[tokio::test]
    async fn risk_consult_failure_degrades_with_the_error() {
        let engine = StrategyEngine::new(MockProvider::canned(Vec::new()), 0, d("0.6"));
        let world = world(Some(engine), None);
        let reply = handle(Command::Risk { market: None }, &world.ctx, NOW_MS).await;
        assert!(reply.text.contains("DEGRADED"), "reply: {}", reply.text);
        assert!(
            reply.text.contains("mock queue exhausted"),
            "reply: {}",
            reply.text
        );
    }

    // ---- /close --------------------------------------------------------------

    #[tokio::test]
    async fn close_builds_confirm_keyboard_and_journals_pending() {
        let world = world(None, None);
        let reply = handle(
            Command::Close {
                market: 32,
                fraction: Some(d("0.3")),
            },
            &world.ctx,
            NOW_MS,
        )
        .await;

        let keyboard = reply.keyboard.as_ref().expect("confirm keyboard");
        assert_eq!(keyboard.len(), 1, "one row");
        assert_eq!(keyboard[0].len(), 2, "execute + cancel");
        assert!(
            keyboard[0][0].0.contains("Execute"),
            "label: {}",
            keyboard[0][0].0
        );
        assert!(
            keyboard[0][1].0.contains("Cancel"),
            "label: {}",
            keyboard[0][1].0
        );
        let approve_data = keyboard[0][0].1.clone();
        assert!(
            approve_data.starts_with("approve:ap-"),
            "data: {approve_data}"
        );
        let id = approve_data
            .strip_prefix("approve:")
            .expect("approve callback data")
            .to_string();
        assert_eq!(keyboard[0][1].1, format!("deny:{id}"));
        assert!(
            reply.text.contains("7500"),
            "notional in card: {}",
            reply.text
        );

        let queued = world.ctx.approvals.get(&id).expect("queued approval");
        assert_eq!(queued.market_id, 32);
        assert_eq!(queued.order.size, d("3"), "0.3 × 10 quantized");
        assert_eq!(queued.expires_ms, NOW_MS + APPROVAL_TTL_MS);
        assert_eq!(queued.decision_ref, None);

        let entries = journal_entries(&world.ctx).await;
        let last = entries.last().expect("pending entry");
        assert_eq!(entries.len(), 1);
        assert_eq!(last.trigger, Trigger::Human);
        assert_eq!(last.execution["status"], "pending");
        assert_eq!(last.execution["executor"], "human");
        assert_eq!(last.decision["approval_id"], id.as_str());
        assert_eq!(last.market_id, Some(32));
    }

    #[tokio::test]
    async fn close_defaults_to_full_position_and_validates_fraction() {
        let world = world(None, None);
        let reply = handle(
            Command::Close {
                market: 32,
                fraction: None,
            },
            &world.ctx,
            NOW_MS,
        )
        .await;
        let keyboard = reply.keyboard.as_ref().expect("full close confirms");
        let id = keyboard[0][0]
            .1
            .strip_prefix("approve:")
            .expect("approve callback")
            .to_string();
        let queued = world.ctx.approvals.get(&id).expect("queued");
        assert_eq!(queued.order.size, d("10"), "default fraction is 1.0");

        for bad in ["0", "-0.5", "1.5"] {
            let reply = handle(
                Command::Close {
                    market: 32,
                    fraction: Some(d(bad)),
                },
                &world.ctx,
                NOW_MS,
            )
            .await;
            assert!(reply.keyboard.is_none(), "fraction {bad} must not confirm");
            assert!(
                reply.text.contains(&format::escape_md2("invalid fraction")),
                "fraction {bad}: {}",
                reply.text
            );
        }

        let reply = handle(
            Command::Close {
                market: 99,
                fraction: None,
            },
            &world.ctx,
            NOW_MS,
        )
        .await;
        assert!(
            reply
                .text
                .contains(&format::escape_md2("no position for market 99"))
        );
    }

    #[tokio::test]
    async fn close_is_denied_when_the_kill_switch_is_engaged() {
        let world = world(None, None);
        let _ = world.ctx.policy.set_key("kill_switch", "true");
        let reply = handle(
            Command::Close {
                market: 32,
                fraction: Some(d("0.3")),
            },
            &world.ctx,
            NOW_MS,
        )
        .await;
        assert!(reply.keyboard.is_none());
        assert!(
            reply.text.contains(&format::escape_md2("close denied")),
            "reply: {}",
            reply.text
        );
        assert_eq!(world.ctx.approvals.len(), 0);
    }

    // ---- approve / deny / expire ----------------------------------------------

    #[tokio::test]
    async fn approve_callback_executes_and_journals_human_approved() {
        let mut world = world(None, None);
        world.ctx.executor = Some(dry_human_executor(&world.dir, &world.state));
        let id = enqueue_close(&world, "0.3", NOW_MS).await;

        let reply = handle(
            Command::Approve { id: id.clone() },
            &world.ctx,
            NOW_MS + 1_000,
        )
        .await;
        assert!(reply.keyboard.is_none());
        assert!(
            reply.text.contains(&format::escape_md2("executed")),
            "reply: {}",
            reply.text
        );
        assert!(
            reply.text.contains(&format::escape_md2("simulated")),
            "reply: {}",
            reply.text
        );
        assert!(world.ctx.approvals.get(&id).is_none(), "consumed");

        let entries = journal_entries(&world.ctx).await;
        let statuses: Vec<&str> = entries
            .iter()
            .map(|entry| entry.execution["status"].as_str().unwrap_or("?"))
            .collect();
        assert_eq!(statuses, vec!["pending", "human_approved"]);
        let outcome = entries.last().expect("approval outcome");
        assert_eq!(outcome.trigger, Trigger::Human);
        assert_eq!(outcome.execution["executor"], "human");
        assert_eq!(outcome.execution["report_status"], "simulated");
        assert!(
            outcome.execution["order_id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("sentinel-32-"),
            "order id: {}",
            outcome.execution["order_id"]
        );
        assert_eq!(outcome.execution["fill"]["filled_size"], "3.000");
        assert!(
            entries[0].input_hash == outcome.input_hash,
            "pair shares the hash"
        );

        let report_lines = std::fs::read_to_string(world.dir.path().join("reports.jsonl"))
            .expect("dry-run report file");
        assert_eq!(report_lines.lines().count(), 1);
    }

    #[tokio::test]
    async fn approve_without_executor_fails_closed() {
        let world = world(None, None);
        let id = enqueue_close(&world, "0.3", NOW_MS).await;
        let reply = handle_approval_callback(&world.ctx, &id, true, NOW_MS + 10).await;
        assert!(
            reply
                .text
                .contains(&format::escape_md2("no executor configured")),
            "reply: {}",
            reply.text
        );
        let entries = journal_entries(&world.ctx).await;
        let outcome = entries.last().expect("failed entry");
        assert_eq!(outcome.execution["status"], "failed");
        assert_eq!(outcome.execution["executor"], "human");
    }

    #[tokio::test]
    async fn deny_callback_journals_denied_and_replies() {
        let world = world(None, None);
        let id = enqueue_close(&world, "0.5", NOW_MS).await;
        let reply = handle_approval_callback(&world.ctx, &id, false, NOW_MS + 5_000).await;
        assert!(reply.keyboard.is_none());
        assert!(
            reply.text.contains(&format::escape_md2("denied")),
            "reply: {}",
            reply.text
        );
        assert!(world.ctx.approvals.get(&id).is_none());

        let entries = journal_entries(&world.ctx).await;
        let outcome = entries.last().expect("denied entry");
        assert_eq!(outcome.trigger, Trigger::Human);
        assert_eq!(outcome.execution["status"], "denied");
        assert_eq!(outcome.execution["executor"], "human");
    }

    #[tokio::test]
    async fn unknown_approval_callback_is_honest() {
        let world = world(None, None);
        let reply = handle_approval_callback(&world.ctx, "ap-00000000", true, NOW_MS).await;
        assert!(
            reply
                .text
                .contains(&format::escape_md2("expired or unknown")),
            "reply: {}",
            reply.text
        );
    }

    #[tokio::test]
    async fn housekeeping_expires_and_journals() {
        let world = world(None, None);
        let id = enqueue_close(&world, "0.5", NOW_MS).await;
        assert_eq!(world.ctx.approvals.len(), 1);

        // At exactly the TTL the entry counts as expired (queue semantics).
        let replies = housekeeping(&world.ctx, NOW_MS + APPROVAL_TTL_MS).await;
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].text.contains(&format::escape_md2("expired")),
            "notification: {}",
            replies[0].text
        );
        assert!(
            replies[0].text.contains(&format::escape_md2(&id)),
            "notification names the id: {}",
            replies[0].text
        );
        assert_eq!(world.ctx.approvals.len(), 0);

        let entries = journal_entries(&world.ctx).await;
        let outcome = entries.last().expect("expired entry");
        assert_eq!(outcome.trigger, Trigger::Human);
        assert_eq!(outcome.execution["status"], "expired");
        assert_eq!(outcome.execution["executor"], "human");
    }

    // ---- kill switch ----------------------------------------------------------

    #[tokio::test]
    async fn pause_flow_flips_kill_and_overlay() {
        let world = world(None, None);

        let ask = handle(Command::Pause, &world.ctx, NOW_MS).await;
        assert!(ask.text.contains("PAUSE"), "reply: {}", ask.text);
        assert!(world.ctx.pause_pending.load(Ordering::SeqCst));

        // Any other text is ignored and leaves the confirmation armed.
        assert!(handle_text("hello", &world.ctx, NOW_MS).await.is_none());
        assert!(world.ctx.pause_pending.load(Ordering::SeqCst));

        // The typed confirmation is case-insensitive and whitespace-trimmed.
        let confirmed = handle_text(" pause \n", &world.ctx, NOW_MS).await;
        assert!(confirmed.is_some());
        assert!(world.ctx.kill.load(Ordering::SeqCst), "in-memory flag");
        assert!(world.ctx.policy.kill_switch(), "overlay flag");
        assert!(!world.ctx.pause_pending.load(Ordering::SeqCst));

        // A second PAUSE without a fresh /pause is ignored.
        assert!(handle_text("PAUSE", &world.ctx, NOW_MS).await.is_none());

        let entries = journal_entries(&world.ctx).await;
        let system = entries.last().expect("system entry");
        assert_eq!(system.trigger, Trigger::System);
        assert_eq!(system.execution["action"], "kill_switch");
        assert_eq!(system.execution["state"], "paused");

        // Regression (SPEC §10): with the switch engaged the core policy gate
        // denies every intent.
        let verdict = PolicyEngine::evaluate(
            &Intent::Reduce {
                fraction: d("0.1"),
                reason: "regression".to_string(),
            },
            &fixture_account(),
            &policy_config(
                &world.ctx.cfg,
                world.ctx.kill.load(Ordering::SeqCst) || world.ctx.policy.kill_switch(),
            ),
            &DayState { actions_today: 0 },
            &PolicyContext {
                source: PolicySource::Manual,
                tier: RiskTier::Green,
                market_id: MarketId(32),
            },
        );
        assert_eq!(
            verdict,
            PolicyVerdict::Deny {
                reason: "kill switch engaged".to_string()
            }
        );

        // /resume releases both.
        let resumed = handle(Command::Resume, &world.ctx, NOW_MS + 1).await;
        assert!(!resumed.text.is_empty());
        assert!(!world.ctx.kill.load(Ordering::SeqCst));
        assert!(!world.ctx.policy.kill_switch());
        let entries = journal_entries(&world.ctx).await;
        let system = entries.last().expect("resumed entry");
        assert_eq!(system.execution["state"], "resumed");
    }

    // ---- /policy ---------------------------------------------------------------

    #[tokio::test]
    async fn policy_view_and_set() {
        let world = world(None, None);
        let view = handle(Command::Policy, &world.ctx, NOW_MS).await;
        assert!(view.text.contains("50000"), "effective cap: {}", view.text);
        assert!(
            view.text.contains("2500"),
            "approval threshold: {}",
            view.text
        );

        let ok = handle(
            Command::PolicySet {
                key: "max_order_size_usd".to_string(),
                value: "60000".to_string(),
            },
            &world.ctx,
            NOW_MS,
        )
        .await;
        assert!(
            ok.text.contains(&format::escape_md2("policy updated")),
            "reply: {}",
            ok.text
        );
        assert!(ok.text.contains("60000"), "refreshed card: {}", ok.text);
        assert_eq!(
            world.ctx.policy.snapshot().max_order_size_usd,
            Some(d("60000"))
        );

        let rejected = handle(
            Command::PolicySet {
                key: "risk_hard_pct".to_string(),
                value: "99".to_string(),
            },
            &world.ctx,
            NOW_MS,
        )
        .await;
        assert!(
            rejected
                .text
                .contains(&format::escape_md2("policy update rejected")),
            "reply: {}",
            rejected.text
        );
        assert_eq!(
            world.ctx.policy.snapshot().risk_hard_pct,
            None,
            "validation failure leaves the overlay untouched"
        );
    }

    // ---- /audit ----------------------------------------------------------------

    #[tokio::test]
    async fn audit_lists_entries_and_points_at_the_cli() {
        let world = world(None, None);
        // Seed two journal entries through the kill-switch flow.
        let _ = handle(Command::Pause, &world.ctx, NOW_MS).await;
        let _ = handle_text("PAUSE", &world.ctx, NOW_MS).await;
        let _ = handle(Command::Resume, &world.ctx, NOW_MS + 1).await;

        let reply = handle(Command::Audit { n: 10 }, &world.ctx, NOW_MS + 2).await;
        assert!(reply.keyboard.is_none());
        assert!(
            reply.text.contains(&format::escape_md2("audit-verify")),
            "cross-check note: {}",
            reply.text
        );
        assert!(!reply.text.is_empty());
    }

    #[tokio::test]
    async fn audit_without_journal_is_honest() {
        let mut world = world(None, None);
        world.ctx.journal = None;
        let reply = handle(Command::Audit { n: 5 }, &world.ctx, NOW_MS).await;
        assert!(
            reply
                .text
                .contains(&format::escape_md2("audit journal unavailable"))
        );
    }

    // ---- /spend ----------------------------------------------------------------

    #[tokio::test]
    async fn spend_reports_ledger_totals() {
        let mut world = world(None, None);
        let ledger = SpendLedger::new(world.dir.path().join("nansen-spend.jsonl"));
        for (ts_ms, cost, endpoint) in [
            (NOW_MS - 2_000, "0.05", "/api/v1/smart-money/netflow"),
            (NOW_MS - 1_000, "0.02", "/api/v1/smart-money/holdings"),
        ] {
            ledger
                .append(&SpendEntry {
                    ts_ms,
                    endpoint: endpoint.to_string(),
                    cost_usd: cost.to_string(),
                    tx_hash: None,
                    payer: None,
                    network: None,
                })
                .expect("append spend entry");
        }
        world.ctx.spend_ledger = Some(ledger);

        let reply = handle(Command::Spend, &world.ctx, NOW_MS).await;
        assert!(
            reply.text.contains(&format::escape_md2("0.07")),
            "day cost: {}",
            reply.text
        );
        assert!(reply.text.contains("netflow"), "recent: {}", reply.text);
        assert!(reply.text.contains("40"), "hourly cap: {}", reply.text);
    }

    #[tokio::test]
    async fn spend_without_ledger_is_honest() {
        let world = world(None, None);
        let reply = handle(Command::Spend, &world.ctx, NOW_MS).await;
        assert!(
            reply
                .text
                .contains(&format::escape_md2("spend ledger unavailable"))
        );
    }

    // ---- /mode, /start, /help ---------------------------------------------------

    #[tokio::test]
    async fn mode_is_honest_about_restarts() {
        let world = world(None, None);
        let reply = handle(Command::Mode, &world.ctx, NOW_MS).await;
        assert!(reply.text.contains("DRY"), "mode badge: {}", reply.text);
        assert!(
            reply
                .text
                .contains(&format::escape_md2("config-level restart")),
            "honesty note: {}",
            reply.text
        );
    }

    #[tokio::test]
    async fn start_and_help_render_the_onboarding() {
        let world = world(None, None);
        for cmd in [Command::Start, Command::Help] {
            let reply = handle(cmd, &world.ctx, NOW_MS).await;
            assert!(reply.text.contains("Sentinel"), "reply: {}", reply.text);
            assert!(reply.text.contains("kill switch"), "safety: {}", reply.text);
            assert!(
                reply
                    .text
                    .contains(&format::escape_md2("denies every intent")),
                "safety: {}",
                reply.text
            );
        }
    }
}
