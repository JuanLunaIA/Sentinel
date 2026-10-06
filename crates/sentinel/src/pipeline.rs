//! Supervised pipeline — the golden path: feed → state → risk → policy →
//! executor → alerts, with a deterministic replay mode.
//!
//! Frozen by `SPEC-P06.md`: [`PipelineEvent`] is the replay-determinism
//! contract (same fixture ⇒ identical JSONL of events); the logical clock in
//! replay is the latest applied event timestamp.
//!
//! P20 adds two optional channels around the existing path (SPEC-P20 §2–§3):
//! a `ConsultTrigger` sender toward the consult task (Yellow-entry and
//! post-reflex-review triggers) and an `ApprovedOrder` receiver carrying
//! policy-allowed strategy orders into the **existing** executor/event/alert
//! path. Both default to `None` — tests and replay keep the pre-P20
//! behaviour byte-for-byte.
//!
//! **P06 status:** implemented — [`LiveState`] revaluation, the supervised
//! event loop ([`Pipeline::run`]) and the decision/alert emission rules live
//! here; alert transport lives in [`crate::notify`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::policy::DayState;
use sentinel_core::risk::ReflexState;
use sentinel_core::types::{
    AccountState, DataQuality, ExecutionMode, Intent, Market, MarketId, PolicyVerdict, Position,
    RiskTier,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc, watch};

use crate::config::Config;
use crate::consult::{ApprovedOrder, ConsultTrigger};
use crate::error::{Result, SentinelError};
use crate::execution::{Executor, PositionProbe};
use crate::health::HealthState;
use crate::notify::{Alert, AlertKind, AlertSink};
use crate::perpl::{AccountEvent, FeedEvent, MarketEvent, PerplFeed};
use crate::reflex::{self, PlannedAction};

/// How the pipeline sources time and input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Wall-clock pacing; live feed.
    Live,
    /// Fixture pacing; logical clock from event timestamps.
    Replay,
}

/// One externally observable pipeline event (the determinism contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// Pipeline armed.
    Started {
        /// Mode string (`dry-run` / `testnet`).
        mode: String,
        /// True in fixture replay.
        replay: bool,
        /// Market ids available.
        markets: Vec<u32>,
        /// Positions in the initial snapshot.
        positions: usize,
        /// Logical start time.
        at_ms: u64,
    },
    /// Risk tier transition for a market.
    TierChanged {
        /// Event time.
        at_ms: u64,
        /// Market.
        market_id: u32,
        /// Previous tier (`None` on first sight).
        from: Option<RiskTier>,
        /// New tier.
        to: RiskTier,
        /// Rendered distance percent.
        distance_pct: String,
    },
    /// A strategy consult is due (brain wired in P07).
    ConsultScheduled {
        /// Event time.
        at_ms: u64,
        /// Market.
        market_id: u32,
        /// Tier that scheduled the consult.
        tier: RiskTier,
    },
    /// A decision came out of the reflex engine.
    Decision {
        /// Event time.
        at_ms: u64,
        /// Decision id.
        decision_id: String,
        /// Market.
        market_id: u32,
        /// Tier at decision time.
        tier: RiskTier,
        /// Rendered policy verdict (`allow` / `deny: …` / `needs_approval: …`).
        verdict: String,
        /// Rendered action (`reduce 25 %`, `close`, `alert`, `none`).
        action: String,
    },
    /// An order was submitted and reported.
    Executed {
        /// Event time.
        at_ms: u64,
        /// Decision id.
        decision_id: String,
        /// Client order id.
        client_order_id: String,
        /// Report status (`simulated` / `submitted` / …).
        status: String,
        /// Filled size.
        filled_size: Decimal,
        /// Average price when known.
        avg_price: Option<Decimal>,
    },
    /// Submission failed.
    SubmitFailed {
        /// Event time.
        at_ms: u64,
        /// Decision id.
        decision_id: String,
        /// Error rendering.
        error: String,
    },
    /// The idempotency guard suppressed a duplicate.
    DuplicateSuppressed {
        /// Event time.
        at_ms: u64,
        /// Decision id.
        decision_id: String,
        /// Idempotency key.
        key: String,
    },
    /// An alert was emitted (mirror of the sink payload).
    Alert {
        /// Event time.
        at_ms: u64,
        /// Alert kind discriminator.
        kind: String,
        /// Market when applicable.
        market_id: Option<u32>,
        /// Alert text.
        text: String,
    },
    /// Feed went stale.
    FeedStale {
        /// Event time.
        at_ms: u64,
        /// Stale seconds.
        secs: u64,
    },
    /// Feed reconnected.
    Reconnected {
        /// Event time.
        at_ms: u64,
        /// Attempt counter.
        attempt: u32,
    },
    /// Clean shutdown completed.
    Shutdown {
        /// Event time.
        at_ms: u64,
    },
}

/// Per-run outcome (the determinism comparison payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineOutcome {
    /// All events in emission order.
    pub events: Vec<PipelineEvent>,
}

impl PipelineOutcome {
    /// Render events as JSON lines (stable field order per serde).
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            match serde_json::to_string(event) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(_) => out.push_str("{\"event\":\"serialize_error\"}\n"),
            }
        }
        out
    }
}

/// Live account state maintained from feed events (marks revalue positions).
#[derive(Debug, Default)]
pub struct LiveState {
    /// Market table (from `context()` at startup).
    pub markets: Vec<Market>,
    /// Latest account snapshot.
    pub account: Option<AccountState>,
    /// Latest mark per market.
    pub marks: std::collections::HashMap<MarketId, Decimal>,
    /// Base balance: `equity − Σ uPnL` at the last snapshot (for revaluation).
    pub base_balance: Decimal,
    /// Staleness in seconds (set by `FeedStale`, cleared by any event).
    pub stale_secs: Option<u64>,
    /// Logical/wall event time in epoch ms (0 = none yet).
    pub now_ms: u64,
}

/// What changed after applying a feed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDelta {
    /// Nothing risk-relevant.
    None,
    /// Marks or account changed → evaluate.
    Risk,
    /// Feed went stale.
    Stale,
    /// Feed reconnected.
    Reconnected,
}

impl LiveState {
    /// Empty state; `set_markets` must run before evaluation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the market table.
    pub fn set_markets(&mut self, markets: Vec<Market>) {
        self.markets = markets;
    }

    /// Apply one feed event; returns what changed.
    ///
    /// Frozen rules (`SPEC-P06.md` §4): mark updates revalue every position of
    /// that market (`unrealized_pnl = (mark − entry) × size`,
    /// `equity = base_balance + Σ uPnL`); snapshots replace the account and
    /// recompute `base_balance`; updates patch balances; any event clears
    /// staleness and advances `now_ms` to the event timestamp.
    pub fn apply(&mut self, event: &FeedEvent) -> StateDelta {
        // Any event ends a stale episode; `FeedStale` re-enters one below.
        self.stale_secs = None;
        match event {
            FeedEvent::Market(MarketEvent::MarkPrice {
                market_id,
                price,
                ts,
            }) => {
                self.marks.insert(*market_id, *price);
                if let Some(account) = &mut self.account {
                    for position in &mut account.positions {
                        if position.market_id == *market_id {
                            position.mark_price = Some(*price);
                            position.unrealized_pnl =
                                (*price - position.entry_price) * position.size;
                        }
                    }
                    let equity = self.base_balance + total_unrealized_pnl(account);
                    account.equity = equity;
                }
                self.now_ms = datetime_ms(*ts);
                StateDelta::Risk
            }
            FeedEvent::Account(AccountEvent::Snapshot { state }) => {
                // Merge the snapshot's position marks into the shared mark map
                // (the venue re-sends current marks with every snapshot).
                for position in &state.positions {
                    if let Some(mark) = position.mark_price {
                        self.marks.insert(position.market_id, mark);
                    }
                }
                self.base_balance = state.equity - total_unrealized_pnl(state);
                self.account = Some(state.clone());
                self.now_ms = datetime_ms(state.snapshot_ts);
                StateDelta::Risk
            }
            FeedEvent::Account(AccountEvent::Update { update }) => {
                if let Some(account) = &mut self.account {
                    account.free_balance = update.free_balance;
                    account.fee_tier = update.fee_tier;
                    let equity = self.base_balance + total_unrealized_pnl(account);
                    account.equity = equity;
                }
                // "Time unchanged" unless the update carries its own timestamp.
                if let Some(ts) = update.ts {
                    self.now_ms = datetime_ms(ts);
                }
                StateDelta::Risk
            }
            FeedEvent::FeedStale { secs } => {
                self.stale_secs = Some(*secs);
                StateDelta::Stale
            }
            FeedEvent::Reconnected { .. } => StateDelta::Reconnected,
        }
    }

    /// Data quality for risk decisions.
    pub fn quality(&self) -> DataQuality {
        match self.stale_secs {
            Some(secs) => DataQuality::Stale { secs },
            None => DataQuality::Fresh,
        }
    }

    /// Current position for a market, if any.
    pub fn position(&self, market_id: MarketId) -> Option<Position> {
        self.account
            .as_ref()?
            .positions
            .iter()
            .find(|position| position.market_id == market_id)
            .cloned()
    }
}

/// `PositionProbe` over the pipeline's shared live state (used by executors).
#[derive(Debug)]
pub struct StateProbe {
    /// Shared state.
    pub state: Arc<Mutex<LiveState>>,
}

impl StateProbe {
    /// Wrap the shared state.
    pub fn new(state: Arc<Mutex<LiveState>>) -> Self {
        Self { state }
    }
}

impl PositionProbe for StateProbe {
    async fn position(&self, market_id: MarketId) -> Result<Option<Position>> {
        let state = self.state.lock().await;
        Ok(state.position(market_id))
    }
}

/// The supervised pipeline (single owner of the event loop).
pub struct Pipeline<F, E, S> {
    /// Configuration.
    pub cfg: Config,
    /// Feed (live or replay).
    pub feed: F,
    /// Executor (dry-run or gateway).
    pub executor: E,
    /// Alert sink (dedupe-wrapped by the caller when needed).
    pub sink: S,
    /// Shared live state (also exposed through [`StateProbe`]).
    pub state: Arc<Mutex<LiveState>>,
    /// Health handle (touched on every event).
    pub health: Arc<HealthState>,
    /// Run mode.
    pub run_mode: RunMode,
    /// Reflex state.
    reflex: ReflexState,
    /// Daily action counter.
    day: DayState,
    /// Decision sequence.
    seq: u64,
    /// Last tier per market (for transition detection).
    last_tier: std::collections::HashMap<MarketId, RiskTier>,
    /// Optional audit journal (SPEC-P10 §8): intents before execution,
    /// outcomes after; `None` keeps the pipeline journal-free (P06 tests).
    journal: Option<Arc<Mutex<sentinel_core::audit::AuditJournal>>>,
    /// Base configuration (policy overlay applies on top).
    base_cfg: Config,
    /// Shared policy overlay (SPEC-P11 §6); `None` keeps the static config.
    policy: Option<Arc<crate::bot::policy_admin::SharedPolicy>>,
    /// Kill switch shared with the bot.
    kill: Arc<std::sync::atomic::AtomicBool>,
    /// Last applied overlay version.
    last_policy_version: u64,
    /// Trigger sender toward the consult task (SPEC-P20 §2); `None` keeps
    /// the pre-P20 path (no triggers, no events — determinism preserved).
    consult_tx: Option<mpsc::Sender<ConsultTrigger>>,
    /// Approved strategy orders inbound from the consult task (SPEC-P20 §2.4);
    /// `None` keeps the pre-P20 path.
    strategy_rx: Option<mpsc::Receiver<ApprovedOrder>>,
}

impl<F, E, S> Pipeline<F, E, S> {
    /// Assemble a pipeline (the caller shares `state` with its probes).
    pub fn new(
        cfg: Config,
        feed: F,
        executor: E,
        sink: S,
        state: Arc<Mutex<LiveState>>,
        health: Arc<HealthState>,
        run_mode: RunMode,
    ) -> Self {
        let base_cfg = cfg.clone();
        Self {
            cfg,
            feed,
            executor,
            sink,
            state,
            health,
            run_mode,
            reflex: ReflexState::new(),
            day: DayState::default(),
            seq: 0,
            last_tier: std::collections::HashMap::new(),
            journal: None,
            base_cfg,
            policy: None,
            kill: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_policy_version: 0,
            consult_tx: None,
            strategy_rx: None,
        }
    }

    /// Attach the trigger sender toward the consult task (SPEC-P20 §2):
    /// Yellow entries and executed Orange/Red reduces queue [`ConsultTrigger`]s
    /// for the consult task. `None` (tests, replay without an engine) keeps
    /// the pipeline's event stream byte-identical.
    pub fn with_consult_tx(mut self, tx: Option<mpsc::Sender<ConsultTrigger>>) -> Self {
        self.consult_tx = tx;
        self
    }

    /// Attach the approved-order receiver (SPEC-P20 §2.4): orders are polled
    /// in the event loop and submitted through the existing executor/event/
    /// alert path. `None` keeps the pre-P20 path.
    pub fn with_strategy_rx(mut self, rx: Option<mpsc::Receiver<ApprovedOrder>>) -> Self {
        self.strategy_rx = rx;
        self
    }

    /// Attach the audit journal (SPEC-P10 §8): every intent-bearing decision
    /// is journaled before execution and its outcome right after.
    pub fn with_journal(mut self, journal: Arc<Mutex<sentinel_core::audit::AuditJournal>>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Attach the shared policy overlay (SPEC-P11 §6): `/policy set` edits
    /// are picked up on the next evaluation via the version counter.
    pub fn with_policy(mut self, policy: Arc<crate::bot::policy_admin::SharedPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Attach the kill switch shared with the bot (SPEC-P11 §6).
    pub fn with_kill_switch(mut self, kill: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.kill = kill;
        self
    }

    /// Refresh the effective config from the overlay when its version moved.
    fn refresh_policy(&mut self) {
        let Some(policy) = &self.policy else {
            return;
        };
        let version = policy.version();
        if version != self.last_policy_version {
            self.cfg =
                crate::bot::policy_admin::apply_to_config(&self.base_cfg, &policy.snapshot());
            self.last_policy_version = version;
        }
    }

    /// Journal one intent (no-op when no journal is attached). Failures log
    /// loudly and PROCEED: a rescue action outranks audit availability
    /// (revisited in P16 with in-memory buffering).
    async fn journal_intent(
        &self,
        action: &crate::reflex::PlannedAction,
        account: &sentinel_core::types::AccountState,
        now_ms: u64,
    ) {
        let Some(journal) = &self.journal else {
            return;
        };
        let record = sentinel_core::audit::IntentRecord {
            trigger: sentinel_core::audit::Trigger::Reflex,
            account: self.account_label(),
            market_id: Some(action.market_id.0),
            input_hash: journal_input_hash(account, now_ms),
            decision: decision_json(action),
            policy_verdict: serde_json::json!({
                "verdict": verdict_text(&action.verdict),
                "note": action.note,
            }),
        };
        let mut guard = journal.lock().await;
        if let Err(err) = guard.record_intent(&record, chrono::Utc::now()) {
            tracing::error!(
                error = %err,
                decision_id = %action.decision_id,
                "audit journal intent write failed; proceeding"
            );
        }
    }

    /// Journal the outcome of a previously recorded intent.
    async fn journal_outcome(
        &self,
        action: &crate::reflex::PlannedAction,
        account: &sentinel_core::types::AccountState,
        now_ms: u64,
        execution: serde_json::Value,
    ) {
        let Some(journal) = &self.journal else {
            return;
        };
        let record = sentinel_core::audit::OutcomeRecord {
            trigger: sentinel_core::audit::Trigger::Reflex,
            account: self.account_label(),
            market_id: Some(action.market_id.0),
            input_hash: journal_input_hash(account, now_ms),
            decision: decision_json(action),
            policy_verdict: serde_json::json!({
                "verdict": verdict_text(&action.verdict),
                "note": action.note,
            }),
            execution,
        };
        let mut guard = journal.lock().await;
        if let Err(err) = guard.record_outcome(&record, chrono::Utc::now()) {
            tracing::error!(
                error = %err,
                decision_id = %action.decision_id,
                "audit journal outcome write failed"
            );
        }
    }

    /// Account label for journal entries.
    fn account_label(&self) -> String {
        self.cfg
            .perpl
            .account
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Inputs hash for a reflex decision (canonical account snapshot + clock).
fn journal_input_hash(account: &sentinel_core::types::AccountState, now_ms: u64) -> String {
    let account_json = serde_json::to_value(account).unwrap_or(serde_json::Value::Null);
    let meta = serde_json::json!({ "now_ms": now_ms });
    sentinel_core::audit::hash_input(&[&account_json, &meta])
}

/// Decision document for journal entries.
fn decision_json(action: &crate::reflex::PlannedAction) -> serde_json::Value {
    serde_json::json!({
        "decision_id": action.decision_id,
        "market_id": action.market_id.0,
        "tier": format!("{:?}", action.tier),
        "action": action_text(action),
        "note": action.note,
        "order": action
            .order
            .as_ref()
            .map(|order| serde_json::to_value(order).unwrap_or(serde_json::Value::Null)),
    })
}

/// Outcome status for decisions that never reached the executor.
fn no_order_status(action: &crate::reflex::PlannedAction) -> &'static str {
    match &action.verdict {
        PolicyVerdict::Deny { .. } => "denied",
        PolicyVerdict::NeedsApproval { .. } => "needs_approval",
        PolicyVerdict::Allow => "skipped",
    }
}

impl<F, E, S> Pipeline<F, E, S>
where
    F: PerplFeed + Sync,
    E: Executor + Sync,
    S: AlertSink + Sync,
{
    /// Run until the shutdown watch flips; drain and return every event.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<PipelineOutcome> {
        // --- Startup: market table + initial account (errors are fatal).
        let markets = self.feed.context().await?;
        let snapshot = self.feed.snapshot().await?;
        {
            let mut state = self.state.lock().await;
            state.set_markets(markets.clone());
            let _ = state.apply(&FeedEvent::Account(AccountEvent::Snapshot {
                state: snapshot.clone(),
            }));
        }

        let started_at = match self.run_mode {
            RunMode::Replay => datetime_ms(snapshot.snapshot_ts),
            RunMode::Live => unix_ms(),
        };
        tracing::info!(
            markets = markets.len(),
            positions = snapshot.positions.len(),
            replay = self.run_mode == RunMode::Replay,
            "pipeline started"
        );

        let mut outcome = PipelineOutcome::default();
        outcome.events.push(PipelineEvent::Started {
            mode: mode_label(self.cfg.execution.mode),
            replay: self.run_mode == RunMode::Replay,
            markets: markets.iter().map(|market| market.id.0).collect(),
            positions: snapshot.positions.len(),
            at_ms: started_at,
        });

        let mut rx = self.feed.stream().await;
        let mut last_eval_ms: Option<u64> = None;
        // P20: the approved-order receiver is polled alongside the feed. It
        // is owned by the loop (not `self`) so branch handlers can borrow
        // `self` mutably; `None` keeps the pre-P20 select shape.
        let mut strategy_rx = self.strategy_rx.take();

        // --- Event loop: apply, then evaluate; shutdown stops consumption.
        loop {
            if *shutdown.borrow() {
                break;
            }
            let mut strategy_closed = false;
            tokio::select! {
                biased;
                change = shutdown.changed() => {
                    if change.is_err() {
                        tracing::debug!("shutdown watch closed; treating as shutdown");
                        break;
                    }
                    // Loop around: the borrow check above re-reads the value,
                    // so only a flip to `true` stops the pipeline.
                }
                incoming = rx.recv() => {
                    let Some(event) = incoming else {
                        // The producer died (fixture exhausted / sockets gone):
                        // end gracefully, never spin.
                        tracing::warn!("feed event channel closed; ending pipeline");
                        break;
                    };
                    let (delta, state_now_ms) = {
                        let mut state = self.state.lock().await;
                        let delta = state.apply(&event);
                        (delta, state.now_ms)
                    };
                    let now_ms = match self.run_mode {
                        RunMode::Replay => state_now_ms,
                        RunMode::Live => unix_ms(),
                    };
                    self.health.touch_feed(now_ms);

                    match &event {
                        FeedEvent::FeedStale { secs } => {
                            outcome
                                .events
                                .push(PipelineEvent::FeedStale { at_ms: now_ms, secs: *secs });
                            let alert = Alert {
                                kind: AlertKind::FeedStale { secs: *secs },
                                market_id: None,
                                text: format!("⚠️ feed stale: no data for {secs}s"),
                                at_ms: now_ms,
                            };
                            self.send_alert(alert, &mut outcome.events).await;
                        }
                        FeedEvent::Reconnected { attempt } => {
                            outcome
                                .events
                                .push(PipelineEvent::Reconnected { at_ms: now_ms, attempt: *attempt });
                        }
                        FeedEvent::Market(_) | FeedEvent::Account(_) => {}
                    }

                    if delta == StateDelta::Risk && self.eval_due(now_ms, &mut last_eval_ms) {
                        let produced = self.evaluate(now_ms).await;
                        outcome.events.extend(produced);
                    }
                }
                approved = recv_optional(strategy_rx.as_mut()) => {
                    match approved {
                        Some(approved) => {
                            let now_ms = match self.run_mode {
                                RunMode::Replay => self.state.lock().await.now_ms,
                                RunMode::Live => unix_ms(),
                            };
                            let produced = self.submit_strategy_order(approved, now_ms).await;
                            outcome.events.extend(produced);
                        }
                        None => strategy_closed = true,
                    }
                }
            }
            if strategy_closed {
                strategy_rx = None;
                tracing::info!("strategy order channel closed; not polling it again");
            }
        }

        let ended_at = match self.run_mode {
            RunMode::Replay => self.state.lock().await.now_ms,
            RunMode::Live => unix_ms(),
        };
        outcome
            .events
            .push(PipelineEvent::Shutdown { at_ms: ended_at });
        tracing::info!(events = outcome.events.len(), "pipeline stopped");
        Ok(outcome)
    }

    /// One deterministic evaluation pass (SPEC-P06 §4.5–§4.6).
    ///
    /// Emits one [`PipelineEvent::Decision`] per planned action, tracks tier
    /// transitions (per market) into [`PipelineEvent::TierChanged`] /
    /// [`PipelineEvent::ConsultScheduled`] plus sink alerts, and submits every
    /// `Allow`-verdict order through the executor. Event order per action:
    /// Decision → tier events → execution outcome → alert mirrors.
    async fn evaluate(&mut self, now_ms: u64) -> Vec<PipelineEvent> {
        let mut emitted = Vec::new();

        // Snapshot the state under the lock; never hold it across awaits
        // (the executor probes the same mutex).
        let (account, markets, quality) = {
            let state = self.state.lock().await;
            (
                state.account.clone(),
                state.markets.clone(),
                state.quality(),
            )
        };
        let Some(account) = account else {
            return emitted;
        };

        self.refresh_policy();
        let kill = self.kill.load(std::sync::atomic::Ordering::Relaxed)
            || self
                .policy
                .as_ref()
                .is_some_and(|policy| policy.kill_switch());
        let planned = reflex::decide(
            &account,
            &markets,
            &self.cfg,
            &mut self.reflex,
            &self.day,
            &mut self.seq,
            now_ms,
            quality,
            kill,
        );

        for action in planned {
            emitted.push(PipelineEvent::Decision {
                at_ms: now_ms,
                decision_id: action.decision_id.clone(),
                market_id: action.market_id.0,
                tier: action.tier,
                verdict: verdict_text(&action.verdict),
                action: action_text(&action),
            });

            // Audit-before-action (P00 #2): intent-bearing decisions are
            // journaled before any submission.
            if action.intent.is_some() {
                self.journal_intent(&action, &account, now_ms).await;
            }

            let symbol = markets
                .iter()
                .find(|market| market.id == action.market_id)
                .map(|market| market.symbol.clone())
                .unwrap_or_else(|| action.market_id.0.to_string());

            // Tier transitions: the first evaluation of a market records the
            // tier (emitting only for non-Green); later changes report from/to.
            let mut alerts: Vec<Alert> = Vec::new();
            let previous = self.last_tier.insert(action.market_id, action.tier);
            let transition = match previous {
                None if action.tier == RiskTier::Green => None,
                None => Some(None),
                Some(previous) if previous != action.tier => Some(Some(previous)),
                Some(_) => None,
            };
            if let Some(from) = transition {
                let distance = render_distance(action.distance_pct);
                emitted.push(PipelineEvent::TierChanged {
                    at_ms: now_ms,
                    market_id: action.market_id.0,
                    from,
                    to: action.tier,
                    distance_pct: distance.clone(),
                });
                if action.tier == RiskTier::Yellow {
                    emitted.push(PipelineEvent::ConsultScheduled {
                        at_ms: now_ms,
                        market_id: action.market_id.0,
                        tier: action.tier,
                    });
                    // P20: per-market Yellow-ENTRY trigger for the consult
                    // task (SPEC-P07 §5a); a no-op without a trigger channel.
                    self.send_consult_trigger(ConsultTrigger::YellowEntry {
                        market_id: action.market_id,
                    });
                }
                if action.tier >= RiskTier::Yellow {
                    alerts.push(tier_change_alert(
                        action.market_id,
                        &symbol,
                        from,
                        action.tier,
                        &distance,
                        now_ms,
                    ));
                }
                if action.tier == RiskTier::Yellow {
                    alerts.push(consult_alert(
                        action.market_id,
                        &symbol,
                        action.tier,
                        &distance,
                        now_ms,
                    ));
                }
            }

            // Execution: only an `Allow` verdict with a sized order submits.
            let mut submitted = false;
            if let (Some(order), PolicyVerdict::Allow) = (&action.order, &action.verdict) {
                submitted = true;
                match self.executor.submit(order).await {
                    Ok(report) => {
                        let status = format!("{:?}", report.status).to_lowercase();
                        let size_text = format!("{}", report.filled_size);
                        emitted.push(PipelineEvent::Executed {
                            at_ms: now_ms,
                            decision_id: action.decision_id.clone(),
                            client_order_id: report.client_order_id.clone(),
                            status: status.clone(),
                            filled_size: report.filled_size,
                            avg_price: report.avg_price,
                        });
                        alerts.push(reflex_alert(
                            &action,
                            &symbol,
                            &status,
                            &size_text,
                            &report.client_order_id,
                            now_ms,
                        ));
                        self.day.actions_today = self.day.actions_today.saturating_add(1);
                        self.journal_outcome(
                            &action,
                            &account,
                            now_ms,
                            serde_json::json!({
                                "status": status,
                                "mode": mode_label(self.cfg.execution.mode),
                                "order_id": report.client_order_id,
                                "tx_hash": report.tx_hash,
                                "fill": {
                                    "filled_size": report.filled_size.to_string(),
                                    "avg_price": report.avg_price.map(|price| price.to_string()),
                                    "status": status,
                                },
                            }),
                        )
                        .await;
                        // P20: an executed Orange/Red reduce schedules a
                        // post-reflex strategy review (SPEC-P07 §5b); a
                        // no-op without a trigger channel.
                        if action.tier >= RiskTier::Orange {
                            self.send_consult_trigger(ConsultTrigger::PostReflexReduce {
                                market_id: action.market_id,
                                tier: action.tier,
                            });
                        }
                    }
                    Err(SentinelError::DuplicateOrder { key, .. }) => {
                        self.journal_outcome(
                            &action,
                            &account,
                            now_ms,
                            serde_json::json!({
                                "status": "duplicate",
                                "key": key,
                                "mode": mode_label(self.cfg.execution.mode),
                            }),
                        )
                        .await;
                        emitted.push(PipelineEvent::DuplicateSuppressed {
                            at_ms: now_ms,
                            decision_id: action.decision_id.clone(),
                            key,
                        });
                    }
                    Err(err) => {
                        self.journal_outcome(
                            &action,
                            &account,
                            now_ms,
                            serde_json::json!({
                                "status": "failed",
                                "error": err.to_string(),
                                "mode": mode_label(self.cfg.execution.mode),
                            }),
                        )
                        .await;
                        emitted.push(PipelineEvent::SubmitFailed {
                            at_ms: now_ms,
                            decision_id: action.decision_id.clone(),
                            error: err.to_string(),
                        });
                    }
                }
            }

            // Outcome for decisions that never reached the executor
            // (denied / needs-approval / skipped): journaled immediately so
            // every intent has a matching outcome entry.
            if !submitted && action.intent.is_some() {
                self.journal_outcome(
                    &action,
                    &account,
                    now_ms,
                    serde_json::json!({
                        "status": no_order_status(&action),
                        "mode": mode_label(self.cfg.execution.mode),
                        "note": action.note,
                    }),
                )
                .await;
            }

            // Alerts are delivered after this action's outcome events; every
            // delivery is mirrored as a `PipelineEvent::Alert` regardless of
            // sink failures (which are logged, never fatal).
            for alert in alerts {
                self.send_alert(alert, &mut emitted).await;
            }
        }

        emitted
    }

    /// Deliver one alert; failures are `warn!`-logged (never fatal) and the
    /// event stream always mirrors what was handed to the sink.
    async fn send_alert(&self, alert: Alert, emitted: &mut Vec<PipelineEvent>) {
        if let Err(err) = self.sink.send(&alert).await {
            tracing::warn!(
                error = %err,
                kind = alert_kind_text(&alert.kind),
                "alert delivery failed"
            );
        }
        emitted.push(PipelineEvent::Alert {
            at_ms: alert.at_ms,
            kind: alert_kind_text(&alert.kind).to_string(),
            market_id: alert.market_id.map(|market| market.0),
            text: alert.text,
        });
    }

    /// Whether this `Risk` delta triggers an evaluation pass: always in
    /// replay; at most once per [`LIVE_EVAL_INTERVAL`] (wall clock) in live.
    fn eval_due(&self, now_ms: u64, last_eval_ms: &mut Option<u64>) -> bool {
        match self.run_mode {
            RunMode::Replay => true,
            RunMode::Live => {
                let interval_ms = u64::try_from(LIVE_EVAL_INTERVAL.as_millis()).unwrap_or(u64::MAX);
                let due = match *last_eval_ms {
                    None => true,
                    Some(previous) => now_ms.saturating_sub(previous) >= interval_ms,
                };
                if due {
                    *last_eval_ms = Some(now_ms);
                }
                due
            }
        }
    }

    /// Queue one consult trigger (no-op without a channel, SPEC-P20 §2).
    /// Best-effort by design: a full or closed channel costs a warning, never
    /// a stalled evaluation pass.
    fn send_consult_trigger(&self, trigger: ConsultTrigger) {
        let Some(tx) = &self.consult_tx else {
            return;
        };
        if let Err(err) = tx.try_send(trigger) {
            tracing::warn!(error = %err, "consult trigger dropped");
        }
    }

    /// Submit one approved strategy order through the existing executor /
    /// event / alert path (SPEC-P20 §2.4), journaling the STRATEGY outcome
    /// with the execution status. The matching STRATEGY intent was journaled
    /// by the consult task (audit-before-action).
    async fn submit_strategy_order(
        &mut self,
        approved: ApprovedOrder,
        now_ms: u64,
    ) -> Vec<PipelineEvent> {
        let mut emitted = Vec::new();
        let market_id = approved.order.market_id;
        let (account, symbol) = {
            let state = self.state.lock().await;
            (
                state.account.clone(),
                state
                    .markets
                    .iter()
                    .find(|market| market.id == market_id)
                    .map(|market| market.symbol.clone())
                    .unwrap_or_else(|| market_id.0.to_string()),
            )
        };

        match self.executor.submit(&approved.order).await {
            Ok(report) => {
                let status = format!("{:?}", report.status).to_lowercase();
                let size_text = format!("{}", report.filled_size);
                emitted.push(PipelineEvent::Executed {
                    at_ms: now_ms,
                    decision_id: approved.decision_id.clone(),
                    client_order_id: report.client_order_id.clone(),
                    status: status.clone(),
                    filled_size: report.filled_size,
                    avg_price: report.avg_price,
                });
                let alert = strategy_execution_alert(
                    &approved,
                    &symbol,
                    &status,
                    &size_text,
                    &report.client_order_id,
                    now_ms,
                );
                self.day.actions_today = self.day.actions_today.saturating_add(1);
                self.journal_strategy_outcome(
                    &approved,
                    account.as_ref(),
                    now_ms,
                    serde_json::json!({
                        "status": status,
                        "mode": mode_label(self.cfg.execution.mode),
                        "order_id": report.client_order_id,
                        "tx_hash": report.tx_hash,
                        "fill": {
                            "filled_size": report.filled_size.to_string(),
                            "avg_price": report.avg_price.map(|price| price.to_string()),
                            "status": status,
                        },
                    }),
                )
                .await;
                self.send_alert(alert, &mut emitted).await;
            }
            Err(SentinelError::DuplicateOrder { key, .. }) => {
                self.journal_strategy_outcome(
                    &approved,
                    account.as_ref(),
                    now_ms,
                    serde_json::json!({
                        "status": "duplicate",
                        "key": key,
                        "mode": mode_label(self.cfg.execution.mode),
                    }),
                )
                .await;
                emitted.push(PipelineEvent::DuplicateSuppressed {
                    at_ms: now_ms,
                    decision_id: approved.decision_id.clone(),
                    key,
                });
            }
            Err(err) => {
                self.journal_strategy_outcome(
                    &approved,
                    account.as_ref(),
                    now_ms,
                    serde_json::json!({
                        "status": "failed",
                        "error": err.to_string(),
                        "mode": mode_label(self.cfg.execution.mode),
                    }),
                )
                .await;
                emitted.push(PipelineEvent::SubmitFailed {
                    at_ms: now_ms,
                    decision_id: approved.decision_id.clone(),
                    error: err.to_string(),
                });
            }
        }
        emitted
    }

    /// Journal the STRATEGY outcome of an approved order with its execution
    /// status (SPEC-P20 §2.4); no-op without a journal.
    async fn journal_strategy_outcome(
        &self,
        approved: &ApprovedOrder,
        account: Option<&sentinel_core::types::AccountState>,
        now_ms: u64,
        execution: serde_json::Value,
    ) {
        let Some(journal) = &self.journal else {
            return;
        };
        let input_hash = match account {
            Some(account) => journal_input_hash(account, now_ms),
            None => sentinel_core::audit::hash_input(&[&serde_json::json!({ "now_ms": now_ms })]),
        };
        let record = sentinel_core::audit::OutcomeRecord {
            trigger: sentinel_core::audit::Trigger::Strategy,
            account: self.account_label(),
            market_id: Some(approved.order.market_id.0),
            input_hash,
            decision: serde_json::json!({
                "decision_id": approved.decision_id,
                "market_id": approved.order.market_id.0,
                "action": "reduce",
                "source": "strategy_consult",
                "order": serde_json::to_value(&approved.order)
                    .unwrap_or(serde_json::Value::Null),
            }),
            policy_verdict: serde_json::json!({
                "verdict": "allow",
                "note": "gated by the consult task (PolicySource::Strategy)",
            }),
            execution,
        };
        let mut guard = journal.lock().await;
        if let Err(err) = guard.record_outcome(&record, chrono::Utc::now()) {
            tracing::error!(
                error = %err,
                decision_id = %approved.decision_id,
                "strategy outcome journal write failed"
            );
        }
    }
}

/// Default live evaluation throttle (`SPEC-P06` §4): at most one decision
/// pass per this interval in live mode; every event in replay.
pub const LIVE_EVAL_INTERVAL: Duration = Duration::from_secs(1);

/// Replay option bag pulled from the CLI (kept separate from `Config`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayOpts {
    /// Fixture path.
    pub fixture: Option<PathBuf>,
    /// Effective mode string for logs/health.
    pub effective_mode: Option<ExecutionMode>,
}

// ---- helpers -----------------------------------------------------------------

/// Σ unrealized PnL over every position of an account.
fn total_unrealized_pnl(account: &AccountState) -> Decimal {
    account
        .positions
        .iter()
        .fold(Decimal::ZERO, |total, position| {
            total + position.unrealized_pnl
        })
}

/// Epoch milliseconds of a timestamp (0 for pre-epoch values — never real).
fn datetime_ms(ts: DateTime<Utc>) -> u64 {
    u64::try_from(ts.timestamp_millis()).unwrap_or(0)
}

/// Wall-clock unix milliseconds (live mode only; replay never reads it).
fn unix_ms() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

/// Await the next value on an optional strategy receiver; pending forever
/// when no channel is attached (the pipeline then behaves exactly as before
/// P20 — the branch can never win the select).
async fn recv_optional<T>(rx: Option<&mut mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Pipeline mode label for [`PipelineEvent::Started`] (`dry-run`/`testnet`).
fn mode_label(mode: ExecutionMode) -> String {
    match mode {
        ExecutionMode::DryRun => "dry-run",
        ExecutionMode::Testnet => "testnet",
        ExecutionMode::Mainnet => "mainnet",
    }
    .to_string()
}

/// Rendered policy verdict (`allow` / `deny: …` / `needs_approval: …`).
fn verdict_text(verdict: &PolicyVerdict) -> String {
    match verdict {
        PolicyVerdict::Allow => "allow".to_string(),
        PolicyVerdict::Deny { reason } => format!("deny: {reason}"),
        PolicyVerdict::NeedsApproval { reason } => format!("needs_approval: {reason}"),
    }
}

/// Rendered action (`reduce 25%` / `close` / `add_collateral` / `alert` /
/// `none`).
fn action_text(action: &PlannedAction) -> String {
    match &action.intent {
        None => "none".to_string(),
        Some(Intent::Reduce { fraction, .. }) => {
            let pct = (*fraction * Decimal::ONE_HUNDRED).normalize();
            format!("reduce {pct}%")
        }
        Some(Intent::Close { .. }) => "close".to_string(),
        Some(Intent::AddCollateral { .. }) => "add_collateral".to_string(),
        Some(Intent::Alert { .. }) => "alert".to_string(),
    }
}

/// Distance-to-liquidation percent rendered to two decimals (`24.31%`).
fn render_distance(distance: Decimal) -> String {
    format!("{:.2}%", distance.round_dp(2))
}

/// Uppercase tier name for alert texts (`GREEN` … `RED`).
fn tier_name(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Green => "GREEN",
        RiskTier::Yellow => "YELLOW",
        RiskTier::Orange => "ORANGE",
        RiskTier::Red => "RED",
    }
}

/// Tier emoji for alert texts.
fn tier_emoji(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Green => "🟢",
        RiskTier::Yellow => "🟡",
        RiskTier::Orange => "🟠",
        RiskTier::Red => "🔴",
    }
}

/// Alert kind discriminator (matches the serde tag of [`AlertKind`]).
fn alert_kind_text(kind: &AlertKind) -> &'static str {
    match kind {
        AlertKind::TierChange { .. } => "tier_change",
        AlertKind::ReflexAction { .. } => "reflex_action",
        AlertKind::ConsultScheduled { .. } => "consult_scheduled",
        AlertKind::FeedStale { .. } => "feed_stale",
        AlertKind::AnchoringDegraded { .. } => "anchoring_degraded",
        AlertKind::StrategyDecision { .. } => "strategy_decision",
    }
}

/// Tier-change alert: symbol, `#market`, FROM→TO and the rendered distance
/// (`SPEC-P06.md` §3). Yellow entries announce the scheduled consult too.
fn tier_change_alert(
    market_id: MarketId,
    symbol: &str,
    from: Option<RiskTier>,
    to: RiskTier,
    distance: &str,
    at_ms: u64,
) -> Alert {
    let from_name = from.map(tier_name).unwrap_or("NEW");
    let mut text = format!(
        "{} {}#{} {}→{} · distance {}",
        tier_emoji(to),
        symbol,
        market_id.0,
        from_name,
        tier_name(to),
        distance
    );
    if to == RiskTier::Yellow {
        text.push_str(" · consult scheduled");
    }
    Alert {
        kind: AlertKind::TierChange {
            from,
            to,
            distance_pct: distance.to_string(),
        },
        market_id: Some(market_id),
        text,
        at_ms,
    }
}

/// Consult-scheduled alert (Yellow entries).
fn consult_alert(
    market_id: MarketId,
    symbol: &str,
    tier: RiskTier,
    distance: &str,
    at_ms: u64,
) -> Alert {
    Alert {
        kind: AlertKind::ConsultScheduled { tier },
        market_id: Some(market_id),
        text: format!(
            "🧠 {}#{} {} · consult scheduled · distance {}",
            symbol,
            market_id.0,
            tier_name(tier),
            distance
        ),
        at_ms,
    }
}

/// Reflex-action alert: action, size, client order id and status (`§4.6`).
fn reflex_alert(
    action: &PlannedAction,
    symbol: &str,
    status: &str,
    size: &str,
    client_order_id: &str,
    at_ms: u64,
) -> Alert {
    Alert {
        kind: AlertKind::ReflexAction {
            decision_id: action.decision_id.clone(),
            status: status.to_string(),
            size: size.to_string(),
            client_order_id: client_order_id.to_string(),
        },
        market_id: Some(action.market_id),
        text: format!(
            "⚡ {}#{} {} · size {} · {} · {}",
            symbol,
            action.market_id.0,
            action_text(action),
            size,
            client_order_id,
            status
        ),
        at_ms,
    }
}

/// Strategy-order execution alert (SPEC-P20 §2.4–§2.5): mirrors the reflex
/// alert shape for orders that entered through the consult task.
fn strategy_execution_alert(
    approved: &ApprovedOrder,
    symbol: &str,
    status: &str,
    size: &str,
    client_order_id: &str,
    at_ms: u64,
) -> Alert {
    Alert {
        kind: AlertKind::StrategyDecision {
            decision_id: approved.decision_id.clone(),
            action: "REDUCE".to_string(),
            status: status.to_string(),
        },
        market_id: Some(approved.order.market_id),
        text: format!(
            "🧠 {}#{} strategy reduce · size {} · {} · {}",
            symbol, approved.order.market_id.0, size, client_order_id, status
        ),
        at_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration as StdDuration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::execution::dry_run::DryRunExecutor;
    use crate::perpl::{AccountUpdate, MockPerpl};

    // ---- fixtures ------------------------------------------------------------

    fn dec(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    fn utc(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).expect("valid timestamp")
    }

    /// ETH testnet-like market: price 2dp, size 3dp, mmr 0.05, 12x max.
    fn eth_market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH Perp".to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: dec("0.083333"),
            maintenance_margin_fraction: dec("0.05"),
            max_leverage: dec("12"),
            min_size: Decimal::ZERO,
            tick_size: dec("0.01"),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    /// ETH long: entry 2700.00, isolated collateral 13560 (liq = 1479.00).
    fn eth_position(size: Decimal, mark: Option<Decimal>) -> Position {
        let entry_price = dec("2700.00");
        let unrealized_pnl = match mark {
            Some(mark) => (mark - entry_price) * size,
            None => Decimal::ZERO,
        };
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price,
            mark_price: mark,
            liq_price: None,
            collateral: dec("13560"),
            unrealized_pnl,
            margin_ratio: None,
            leverage: dec("2"),
            opened_at: None,
        }
    }

    /// A `Snapshot` feed event with one ETH position and the given equity.
    fn snapshot_event(mark: Option<Decimal>, equity: Decimal, ms: i64) -> FeedEvent {
        FeedEvent::Account(AccountEvent::Snapshot {
            state: AccountState {
                positions: vec![eth_position(dec("10"), mark)],
                free_balance: dec("1000"),
                equity,
                fee_tier: 0,
                snapshot_ts: utc(ms),
            },
        })
    }

    /// ETH mark update at `ms`.
    fn mark_event(price: Decimal, ms: i64) -> FeedEvent {
        FeedEvent::Market(MarketEvent::MarkPrice {
            market_id: MarketId(32),
            price,
            ts: utc(ms),
        })
    }

    /// Config mirroring the crash-demo environment (generous caps so the
    /// reflex reduce reaches the executor).
    fn test_config() -> Config {
        let mut vars = HashMap::new();
        vars.insert("PERPL_ENV".to_string(), "testnet".to_string());
        vars.insert(
            "PERPL_API_KEY_SECRET".to_string(),
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_string(),
        );
        vars.insert("PERPL_API_KEY".to_string(), "test-api-key".to_string());
        vars.insert("QWEN_API_KEY".to_string(), "test".to_string());
        vars.insert("KIMI_API_KEY".to_string(), "test".to_string());
        vars.insert("NANSEN_PAYER_KEY".to_string(), "0x11".to_string());
        vars.insert("TELEGRAM_ALLOWED_USER_IDS".to_string(), "1".to_string());
        vars.insert("TELOXIDE_TOKEN".to_string(), "test".to_string());
        vars.insert("EXECUTION_MODE".to_string(), "DRY_RUN".to_string());
        vars.insert("MARKET_ALLOWLIST".to_string(), "32".to_string());
        vars.insert("MAX_ORDER_SIZE_USD".to_string(), "100000".to_string());
        vars.insert(
            "REQUIRE_APPROVAL_ABOVE_USD".to_string(),
            "100000".to_string(),
        );
        vars.insert("REFLEX_COOLDOWN_SECS".to_string(), "60".to_string());
        Config::from_vars(vars).expect("test config")
    }

    /// Test-local sink (independent of the notify module's implementations).
    #[derive(Clone, Default)]
    struct TestSink {
        alerts: Arc<StdMutex<Vec<Alert>>>,
    }

    impl TestSink {
        fn new() -> Self {
            Self::default()
        }
    }

    impl AlertSink for TestSink {
        async fn send(&self, alert: &Alert) -> Result<()> {
            self.alerts
                .lock()
                .expect("alerts buffer")
                .push(alert.clone());
            Ok(())
        }
    }

    // ---- LiveState::apply rules table -----------------------------------------

    #[test]
    fn apply_marks_revalue_positions_and_equity() {
        let mut state = LiveState::new();
        state.set_markets(vec![eth_market()]);

        // Snapshot: equity = balance + Σ uPnL = 1000 + (2112.86 − 2700.00) × 10.
        let event = snapshot_event(Some(dec("2112.86")), dec("-4871.40"), 1_700_000_000_000);
        assert_eq!(state.apply(&event), StateDelta::Risk);
        assert_eq!(state.base_balance, dec("1000"), "base = equity − Σ uPnL");
        assert_eq!(
            state.marks.get(&MarketId(32)).copied(),
            Some(dec("2112.86"))
        );
        assert_eq!(state.now_ms, 1_700_000_000_000);

        // Mark update revalues the position and the account equity.
        assert_eq!(
            state.apply(&mark_event(dec("1573.40"), 1_700_000_025_000)),
            StateDelta::Risk
        );
        let account = state.account.as_ref().expect("account installed");
        let position = &account.positions[0];
        assert_eq!(position.mark_price, Some(dec("1573.40")));
        assert_eq!(position.unrealized_pnl, dec("-11266.00")); // (1573.40 − 2700) × 10
        assert_eq!(account.equity, dec("1000") + dec("-11266.00"));
        assert_eq!(
            state.marks.get(&MarketId(32)).copied(),
            Some(dec("1573.40"))
        );
        assert_eq!(state.now_ms, 1_700_000_025_000);
    }

    #[test]
    fn apply_snapshot_replaces_account_and_merges_marks() {
        let mut state = LiveState::new();
        state.set_markets(vec![eth_market()]);
        // An unrelated market's mark survives the merge.
        state.marks.insert(MarketId(99), dec("5"));

        let event = snapshot_event(Some(dec("2112.86")), dec("-4871.40"), 1_700_000_000_000);
        assert_eq!(state.apply(&event), StateDelta::Risk);
        assert_eq!(
            state.marks.get(&MarketId(32)).copied(),
            Some(dec("2112.86"))
        );
        assert_eq!(state.marks.get(&MarketId(99)).copied(), Some(dec("5")));
        assert_eq!(state.base_balance, dec("1000"));
        assert_eq!(state.now_ms, 1_700_000_000_000);
        assert_eq!(state.account.as_ref().expect("account").positions.len(), 1);

        // A mark-less snapshot updates the account but never clears marks.
        let event = snapshot_event(None, dec("1000"), 1_700_000_100_000);
        assert_eq!(state.apply(&event), StateDelta::Risk);
        assert_eq!(
            state.marks.get(&MarketId(32)).copied(),
            Some(dec("2112.86"))
        );
        assert_eq!(state.base_balance, dec("1000"));
        assert_eq!(state.now_ms, 1_700_000_100_000);
        let account = state.account.as_ref().expect("account");
        assert_eq!(account.positions[0].mark_price, None);
        assert_eq!(account.equity, dec("1000"));
    }

    #[test]
    fn apply_update_patches_balances_and_keeps_marks() {
        let mut state = LiveState::new();
        state.set_markets(vec![eth_market()]);
        let _ = state.apply(&snapshot_event(
            Some(dec("2112.86")),
            dec("-4871.40"),
            1_700_000_000_000,
        ));

        let update = AccountUpdate {
            account_id: 7,
            free_balance: dec("750.5"),
            fee_tier: 3,
            forward_enabled: true,
            last_forwarded_request_id: 42,
            ts: Some(utc(1_700_000_010_000)),
        };
        assert_eq!(
            state.apply(&FeedEvent::Account(AccountEvent::Update {
                update: update.clone()
            })),
            StateDelta::Risk
        );
        let account = state.account.as_ref().expect("account");
        assert_eq!(account.free_balance, dec("750.5"));
        assert_eq!(account.fee_tier, 3);
        // equity = base_balance + Σ uPnL = 1000 + (2112.86 − 2700) × 10.
        assert_eq!(account.equity, dec("-4871.40"));
        assert_eq!(
            state.marks.get(&MarketId(32)).copied(),
            Some(dec("2112.86"))
        );
        assert_eq!(state.now_ms, 1_700_000_010_000);

        // An update without a timestamp leaves the clock unchanged.
        let update = AccountUpdate { ts: None, ..update };
        assert_eq!(
            state.apply(&FeedEvent::Account(AccountEvent::Update { update })),
            StateDelta::Risk
        );
        assert_eq!(state.now_ms, 1_700_000_010_000);
    }

    #[test]
    fn apply_stale_sets_and_any_event_clears() {
        let mut state = LiveState::new();
        state.set_markets(vec![eth_market()]);
        assert_eq!(state.quality(), DataQuality::Fresh);

        assert_eq!(
            state.apply(&FeedEvent::FeedStale { secs: 7 }),
            StateDelta::Stale
        );
        assert_eq!(state.stale_secs, Some(7));
        assert_eq!(state.quality(), DataQuality::Stale { secs: 7 });

        // Reconnected clears the episode but never evaluates.
        assert_eq!(
            state.apply(&FeedEvent::Reconnected { attempt: 3 }),
            StateDelta::Reconnected
        );
        assert_eq!(state.stale_secs, None);
        assert_eq!(state.quality(), DataQuality::Fresh);

        // Any data event clears staleness again.
        assert_eq!(
            state.apply(&FeedEvent::FeedStale { secs: 12 }),
            StateDelta::Stale
        );
        assert_eq!(state.stale_secs, Some(12));
        assert_eq!(
            state.apply(&mark_event(dec("1573.40"), 1_700_000_025_000)),
            StateDelta::Risk
        );
        assert_eq!(state.stale_secs, None);
        assert_eq!(state.quality(), DataQuality::Fresh);
    }

    // ---- StateProbe ------------------------------------------------------------

    #[tokio::test]
    async fn state_probe_reads_positions_from_shared_state() {
        let state = Arc::new(Mutex::new(LiveState::new()));
        {
            let mut live = state.lock().await;
            live.set_markets(vec![eth_market()]);
            let _ = live.apply(&snapshot_event(
                Some(dec("2112.86")),
                dec("-4871.40"),
                1_700_000_000_000,
            ));
        }

        let probe = StateProbe::new(Arc::clone(&state));
        let found = probe.position(MarketId(32)).await.expect("probe read");
        assert_eq!(found, Some(eth_position(dec("10"), Some(dec("2112.86")))));
        assert_eq!(
            probe.position(MarketId(99)).await.expect("probe read"),
            None
        );
    }

    // ---- supervised loop -------------------------------------------------------

    /// Two-step declining-mark fixture: Green (30.0 %) → Red (6.0 %).
    ///
    /// REST: context (ETH id 32), ticker, wallet, positions (entry 2700.00,
    /// size +10.000, collateral 13560 ⇒ liq 1479.00). WS: initial `mt:19` +
    /// `mt:26` (composed with the marks known so far), then two `mt:9` steps
    /// 25 s apart in logical time.
    const FIXTURE_2STEP: &str = concat!(
        r#"{"kind":"rest","path":"/v1/pub/context","resp":{"chain":{"chain_id":10143,"name":"Monad Testnet"},"instances":[{"id":12,"address":"0x1964c32f0be608e7d29302aff5e61268e72080cc"}],"tokens":[{"id":1,"symbol":"AUSD","decimals":6}],"markets":[{"ver":1,"id":32,"instance_id":12,"perpetual_id":32,"symbol":"ETH","name":"ETH Perp","funding_interval_sec":2580,"order_ttl_blocks":20,"config":{"price_decimals":2,"size_decimals":3,"min_posting_amount":"0","initial_margin":1200,"maintenance_margin":2000,"maker_fee":45,"taker_fee":345}}]}}"#,
        "\n",
        r#"{"kind":"rest","path":"/v1/market-data/ticker","resp":{"mt":9,"sn":1,"d":{"32":{"at":{"b":1,"t":1700000000000},"mrk":211286}}}}"#,
        "\n",
        r#"{"kind":"rest","path":"/v1/trading/wallet","resp":{"mt":19,"sn":68507460,"at":{"b":68507460,"t":1700000000000},"addr":"0x0000000000000000000000000000000000000007","n":12,"fl":0,"as":[{"mt":19,"in":12,"id":7,"fr":false,"fw":true,"ft":0,"lfr":41,"b":"1000000000","lb":"0"}],"sts":[]}}"#,
        "\n",
        r#"{"kind":"rest","path":"/v1/trading/positions","resp":{"mt":26,"sn":68507460,"at":{"b":68507460,"t":1700000000000},"d":[{"at":{"b":1,"t":1700000000000},"mkt":32,"acc":7,"pid":1001,"rq":41,"oid":555,"st":1,"sr":21,"sd":1,"c":"13560000000","ep":270000,"s":10000,"fee":"0","cfee":"0","efs":0,"lv":200,"dpnl":"0","fnd":"0","ots":{"b":1,"t":1700000000000}}]}}"#,
        "\n",
        r#"{"kind":"ws","t_ms":0,"msg":{"mt":19,"sn":68507460,"at":{"b":68507460,"t":1700000000000},"as":[{"mt":19,"in":12,"id":7,"fr":false,"fw":true,"ft":0,"lfr":41,"b":"1000000000","lb":"0"}]}}"#,
        "\n",
        r#"{"kind":"ws","t_ms":0,"msg":{"mt":26,"sn":68507460,"at":{"b":68507460,"t":1700000000000},"d":[{"at":{"b":1,"t":1700000000000},"mkt":32,"acc":7,"pid":1001,"rq":41,"oid":555,"st":1,"sr":21,"sd":1,"c":"13560000000","ep":270000,"s":10000,"fee":"0","cfee":"0","efs":0,"lv":200,"dpnl":"0","fnd":"0","ots":{"b":1,"t":1700000000000}}]}}"#,
        "\n",
        r#"{"kind":"ws","t_ms":25000,"msg":{"mt":9,"sn":2,"d":{"32":{"at":{"b":1,"t":1700000025000},"mrk":211286}}}}"#,
        "\n",
        r#"{"kind":"ws","t_ms":50000,"msg":{"mt":9,"sn":3,"d":{"32":{"at":{"b":1,"t":1700000050000},"mrk":157340}}}}"#,
        "\n",
    );

    #[tokio::test]
    async fn fixture_replay_emits_ordered_events() {
        // Replay must be instantaneous: the pace divisor exceeds the fixture
        // span, so every inter-message delta rounds to zero (SPEC-P06 §7).
        // SAFETY: `set_var` is `unsafe` in edition 2024 (it races concurrent
        // env reads); this test writes the variable *before* any `MockPerpl`
        // replay starts and nothing else in this binary touches it.
        unsafe { std::env::set_var("SENTINEL_MOCK_PACE", "100000") };

        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("scenario.jsonl");
        std::fs::write(&fixture, FIXTURE_2STEP).expect("fixture written");
        let feed = MockPerpl::from_fixture(&fixture).expect("fixture loads");

        let state = Arc::new(Mutex::new(LiveState::new()));
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let executor = DryRunExecutor::new(
            StateProbe::new(Arc::clone(&state)),
            10,
            dir.path().join("reports.jsonl"),
            0,
        );
        let sink = TestSink::new();
        let recorded = Arc::clone(&sink.alerts);

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let pipeline = Pipeline::new(
            test_config(),
            feed,
            executor,
            sink,
            Arc::clone(&state),
            health,
            RunMode::Replay,
        );
        let outcome = tokio::spawn(pipeline.run(shutdown_rx))
            .await
            .expect("task joins")
            .expect("run succeeds");

        // Evaluation clock is the latest applied event timestamp; the
        // mark-less in-stream snapshot consumes `d-1`, so steps are d-2/d-3.
        let expected = vec![
            PipelineEvent::Started {
                mode: "dry-run".to_string(),
                replay: true,
                markets: vec![32],
                positions: 1,
                at_ms: 1_700_000_000_000,
            },
            PipelineEvent::Decision {
                at_ms: 1_700_000_025_000,
                decision_id: "d-2".to_string(),
                market_id: 32,
                tier: RiskTier::Green,
                verdict: "allow".to_string(),
                action: "none".to_string(),
            },
            PipelineEvent::Decision {
                at_ms: 1_700_000_050_000,
                decision_id: "d-3".to_string(),
                market_id: 32,
                tier: RiskTier::Red,
                verdict: "allow".to_string(),
                action: "reduce 50%".to_string(),
            },
            PipelineEvent::TierChanged {
                at_ms: 1_700_000_050_000,
                market_id: 32,
                from: Some(RiskTier::Green),
                to: RiskTier::Red,
                distance_pct: "6.00%".to_string(),
            },
            PipelineEvent::Executed {
                at_ms: 1_700_000_050_000,
                decision_id: "d-3".to_string(),
                client_order_id: "sentinel-32-1".to_string(),
                status: "simulated".to_string(),
                filled_size: dec("5"),
                avg_price: Some(dec("1571.82660")),
            },
            PipelineEvent::Alert {
                at_ms: 1_700_000_050_000,
                kind: "tier_change".to_string(),
                market_id: Some(32),
                text: "🔴 ETH#32 GREEN→RED · distance 6.00%".to_string(),
            },
            PipelineEvent::Alert {
                at_ms: 1_700_000_050_000,
                kind: "reflex_action".to_string(),
                market_id: Some(32),
                text: "⚡ ETH#32 reduce 50% · size 5.000 · sentinel-32-1 · simulated".to_string(),
            },
            PipelineEvent::Shutdown {
                at_ms: 1_700_000_050_000,
            },
        ];
        assert_eq!(outcome.events, expected);

        // Every mirrored alert matches what the sink actually received.
        let expected_alerts = vec![
            Alert {
                kind: AlertKind::TierChange {
                    from: Some(RiskTier::Green),
                    to: RiskTier::Red,
                    distance_pct: "6.00%".to_string(),
                },
                market_id: Some(MarketId(32)),
                text: "🔴 ETH#32 GREEN→RED · distance 6.00%".to_string(),
                at_ms: 1_700_000_050_000,
            },
            Alert {
                kind: AlertKind::ReflexAction {
                    decision_id: "d-3".to_string(),
                    status: "simulated".to_string(),
                    size: "5.000".to_string(),
                    client_order_id: "sentinel-32-1".to_string(),
                },
                market_id: Some(MarketId(32)),
                text: "⚡ ETH#32 reduce 50% · size 5.000 · sentinel-32-1 · simulated".to_string(),
                at_ms: 1_700_000_050_000,
            },
        ];
        assert_eq!(*recorded.lock().expect("alerts buffer"), expected_alerts);
    }

    /// Feed under test control: `stream()` hands out a channel whose sender
    /// the test keeps, so shutdown/close timing is deterministic (no pacing
    /// environment variables involved).
    struct ScriptedFeed {
        markets: Vec<Market>,
        account: AccountState,
        sender: Arc<Mutex<Option<mpsc::Sender<FeedEvent>>>>,
    }

    impl ScriptedFeed {
        fn new(
            markets: Vec<Market>,
            account: AccountState,
        ) -> (Self, Arc<Mutex<Option<mpsc::Sender<FeedEvent>>>>) {
            let slot = Arc::new(Mutex::new(None));
            let feed = Self {
                markets,
                account,
                sender: Arc::clone(&slot),
            };
            (feed, slot)
        }
    }

    impl PerplFeed for ScriptedFeed {
        async fn stream(&self) -> mpsc::Receiver<FeedEvent> {
            let (sender, receiver) = mpsc::channel(16);
            *self.sender.lock().await = Some(sender);
            receiver
        }

        async fn snapshot(&self) -> Result<AccountState> {
            Ok(self.account.clone())
        }

        async fn context(&self) -> Result<Vec<Market>> {
            Ok(self.markets.clone())
        }
    }

    /// An account with no positions (lifecycle tests need no decisions).
    fn quiet_account() -> AccountState {
        AccountState {
            positions: Vec::new(),
            free_balance: dec("1000"),
            equity: dec("1000"),
            fee_tier: 0,
            snapshot_ts: utc(1_699_000_000_000),
        }
    }

    async fn wait_for_sender(
        slot: &Arc<Mutex<Option<mpsc::Sender<FeedEvent>>>>,
    ) -> mpsc::Sender<FeedEvent> {
        for _ in 0..1_000 {
            if let Some(sender) = slot.lock().await.clone() {
                return sender;
            }
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
        panic!("feed stream() was never called");
    }

    async fn wait_for_now(state: &Arc<Mutex<LiveState>>, expected: u64) {
        for _ in 0..1_000 {
            if state.lock().await.now_ms == expected {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
        panic!("event at {expected} was never applied");
    }

    #[tokio::test]
    async fn shutdown_mid_stream_returns_ok_with_exactly_one_shutdown() {
        let (feed, slot) = ScriptedFeed::new(vec![eth_market()], quiet_account());
        let state = Arc::new(Mutex::new(LiveState::new()));
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = DryRunExecutor::new(
            StateProbe::new(Arc::clone(&state)),
            10,
            dir.path().join("reports.jsonl"),
            0,
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let pipeline = Pipeline::new(
            test_config(),
            feed,
            executor,
            TestSink::new(),
            Arc::clone(&state),
            health,
            RunMode::Replay,
        );
        let task = tokio::spawn(pipeline.run(shutdown_rx));

        // Drive one event through, then flip the watch while the stream is
        // still open: the pipeline must stop consuming and end cleanly.
        let sender = wait_for_sender(&slot).await;
        sender
            .send(mark_event(dec("1573.40"), 1_700_000_010_000))
            .await
            .expect("send mark");
        wait_for_now(&state, 1_700_000_010_000).await;

        shutdown_tx.send(true).expect("signal shutdown");
        let outcome = task.await.expect("task joins").expect("run succeeds");

        assert!(matches!(
            outcome.events.first(),
            Some(PipelineEvent::Started { .. })
        ));
        let shutdowns = outcome
            .events
            .iter()
            .filter(|event| matches!(event, PipelineEvent::Shutdown { .. }))
            .count();
        assert_eq!(shutdowns, 1, "exactly one Shutdown");
        assert_eq!(
            outcome.events.last(),
            Some(&PipelineEvent::Shutdown {
                at_ms: 1_700_000_010_000
            })
        );
        assert_eq!(state.lock().await.now_ms, 1_700_000_010_000);
    }

    #[tokio::test]
    async fn channel_close_ends_gracefully_with_shutdown() {
        let (feed, slot) = ScriptedFeed::new(vec![eth_market()], quiet_account());
        let state = Arc::new(Mutex::new(LiveState::new()));
        let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = DryRunExecutor::new(
            StateProbe::new(Arc::clone(&state)),
            10,
            dir.path().join("reports.jsonl"),
            0,
        );
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let pipeline = Pipeline::new(
            test_config(),
            feed,
            executor,
            TestSink::new(),
            Arc::clone(&state),
            health,
            RunMode::Replay,
        );
        let task = tokio::spawn(pipeline.run(shutdown_rx));

        let sender = wait_for_sender(&slot).await;
        sender
            .send(mark_event(dec("1573.40"), 1_700_000_020_000))
            .await
            .expect("send mark");
        wait_for_now(&state, 1_700_000_020_000).await;

        // Producer death: drop both the local clone and the registered one.
        slot.lock().await.take();
        drop(sender);

        let outcome = task.await.expect("task joins").expect("run succeeds");
        assert_eq!(
            outcome.events,
            vec![
                PipelineEvent::Started {
                    mode: "dry-run".to_string(),
                    replay: true,
                    markets: vec![32],
                    positions: 0,
                    at_ms: 1_699_000_000_000,
                },
                PipelineEvent::Shutdown {
                    at_ms: 1_700_000_020_000
                },
            ]
        );
    }
}
