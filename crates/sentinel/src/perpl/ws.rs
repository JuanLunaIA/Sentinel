//! WebSocket perception: market-data + trading sockets, event bus, staleness.
//!
//! SKELETON STUB — frozen interface (see `SPEC.md` §4.4). Replaced by agent
//! `ws`; do not change public signatures.
//!
//! Transport facts (docs/FACTS.md §1.10): market-data `GET {ws}/ws/v1/market-data`
//! with `mt:5` subscriptions (`market-state@<chain>`, `heartbeat@<chain>`,
//! `order-book@<id>`); trading `{ws}/ws/v1/trading` with first-frame `mt:29`
//! sign-in, snapshots `mt:19/23/26`, updates `mt:21/24/25/27`, heartbeat
//! `mt:100` (sequence-checked, gap ⇒ reconnect). Reconnect: exponential
//! backoff base [`backoff_base`] plus jitter, cap 30 s.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, MarketId};
use tokio::sync::{mpsc::Sender, watch::Receiver as WatchReceiver};

use crate::error::Result;
use crate::perpl::auth::ApiKeySigner;
use crate::perpl::types::AccountUpdate;

/// Market-data events relevant to risk.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketEvent {
    /// Mark price update from the `market-state@<chain>` stream.
    MarkPrice {
        /// Market the price belongs to.
        market_id: MarketId,
        /// Mark price in price units.
        price: Decimal,
        /// Source timestamp.
        ts: DateTime<Utc>,
    },
}

/// Account-level events from the trading socket.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountEvent {
    /// Initial or re-acquired snapshot (composed with REST data when needed).
    Snapshot {
        /// Full account state.
        state: AccountState,
    },
    /// Incremental account update (`mt:21`).
    Update {
        /// Parsed update payload.
        update: AccountUpdate,
    },
}

/// Everything the perception layer emits on its event bus.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedEvent {
    /// A market-data event.
    Market(MarketEvent),
    /// An account event.
    Account(AccountEvent),
    /// No data for longer than the configured staleness threshold.
    FeedStale {
        /// Seconds since the last event.
        secs: u64,
    },
    /// The client reconnected after `attempt` failures.
    Reconnected {
        /// Consecutive failed attempts before the successful reconnect.
        attempt: u32,
    },
}

/// Reference to a market the socket layer subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketRef {
    /// Perpl market id (`order-book@<id>`).
    pub id: u32,
    /// Price scaling exponent, for converting raw `mrk` values.
    pub price_decimals: u32,
}

/// Run configuration for the live sockets.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// WebSocket base URL (e.g. `wss://testnet.perpl.xyz`).
    pub ws_url: String,
    /// Chain id used in stream names (`market-state@<chain_id>`).
    pub chain_id: u64,
    /// Markets to subscribe to (`order-book@<id>`) and to translate.
    pub markets: Vec<MarketRef>,
    /// Emit [`FeedEvent::FeedStale`] after this long without events.
    pub stale_after: Duration,
}

/// Edge-triggered staleness detector (pure, deterministic; `now` in ms).
#[derive(Debug, Clone)]
#[allow(dead_code)] // stub fields; consumed by agent `ws` implementation
pub struct StaleDetector {
    threshold: Duration,
    last_touch_ms: Option<u64>,
    fired: bool,
}

impl StaleDetector {
    /// New detector that fires once per stale episode.
    pub fn new(_threshold: Duration) -> Self {
        todo!("P03 agent ws")
    }

    /// Record that an event was received at `now_ms` (resets the episode).
    pub fn touch(&mut self, _now_ms: u64) {
        todo!("P03 agent ws")
    }

    /// Poll for staleness; returns `Some(secs)` exactly once per episode.
    pub fn observe(&mut self, _now_ms: u64) -> Option<u64> {
        todo!("P03 agent ws")
    }
}

/// Deterministic exponential backoff base: 500 ms · 2^attempt, cap 30 s.
pub fn backoff_base(_attempt: u32) -> Duration {
    todo!("P03 agent ws")
}

/// Run both sockets until `shutdown` flips to `true`.
///
/// Spawns market-data and trading tasks; events flow through `tx`.
///
/// # Errors
/// `PerplError::Ws` on unrecoverable setup failures (individual disconnects
/// are handled internally by reconnecting).
pub async fn run(
    _cfg: WsConfig,
    _signer: &ApiKeySigner,
    _tx: Sender<FeedEvent>,
    _shutdown: WatchReceiver<bool>,
) -> Result<()> {
    todo!("P03 agent ws")
}
