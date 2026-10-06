//! Supervised pipeline — the golden path: feed → state → risk → policy →
//! executor → alerts, with a deterministic replay mode.
//!
//! Frozen by `SPEC-P06.md`: [`PipelineEvent`] is the replay-determinism
//! contract (same fixture ⇒ identical JSONL of events); the logical clock in
//! replay is the latest applied event timestamp.
//!
//! **Skeleton status (P06):** interfaces frozen; implemented by the P06 wave.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use sentinel_core::policy::DayState;
use sentinel_core::risk::ReflexState;
use sentinel_core::types::{
    AccountState, DataQuality, ExecutionMode, Market, MarketId, Position, RiskTier,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use crate::config::Config;
use crate::error::Result;
use crate::execution::{Executor, PositionProbe};
use crate::health::HealthState;
use crate::notify::AlertSink;
use crate::perpl::{FeedEvent, PerplFeed};

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
    pub fn apply(&mut self, _event: &FeedEvent) -> StateDelta {
        todo!("P06 agent pipeline: apply per SPEC-P06 §4")
    }

    /// Data quality for risk decisions.
    pub fn quality(&self) -> DataQuality {
        todo!("P06 agent pipeline: Fresh or Stale{{secs}}")
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
    async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
        todo!("P06 agent pipeline: read from the shared state")
    }
}

/// The supervised pipeline (single owner of the event loop).
#[allow(dead_code)] // private stub fields; consumed by the P06 wave
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
        let _ = (&cfg, &feed, &executor, &sink, &state, &health, run_mode);
        todo!("P06 agent pipeline: constructor")
    }
}

impl<F, E, S> Pipeline<F, E, S>
where
    F: PerplFeed + Sync,
    E: Executor + Sync,
    S: AlertSink + Sync,
{
    /// Run until the shutdown watch flips; drain and return every event.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<PipelineOutcome> {
        let _ = shutdown;
        todo!("P06 agent pipeline: supervised event loop per SPEC-P06 §4")
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
