//! Perpl perception layer: authentication, REST snapshots and WebSocket
//! feeds — the sensory system of Sentinel.
//!
//! Submodules: [`auth`] (Ed25519 request signing), [`types`] (raw-JSON to
//! domain mapping), [`rest`] (HTTP snapshots), [`ws`] (streams, events,
//! staleness detection). This module wires them into the [`PerplFeed`] trait,
//! with two implementations:
//!
//! * [`LivePerpl`] — the real gateway (REST + both WebSockets), built from
//!   [`crate::config::Config`].
//! * [`MockPerpl`] — fixture replay (JSONL, format in `SPEC.md` §3.4) for
//!   offline development, tests and the deterministic demo.
//!
//! Implementations never expose raw venue JSON; everything is mapped into
//! `sentinel_core` types at this boundary (P00 invariant #3).

pub mod auth;
pub mod rest;
pub mod types;
pub mod ws;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Market};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{OnceCell, watch};

use crate::config::Config;
use crate::error::{PerplError, Result};
use crate::perpl::auth::ApiKeySigner;
use crate::perpl::rest::PerplRest;

pub use types::AccountUpdate;
pub use ws::{AccountEvent, FeedEvent, MarketEvent, StaleDetector};

/// Capacity of the event bus between the socket loops and consumers.
const EVENT_BUS_CAPACITY: usize = 256;

/// Fixture replay pacing: real inter-message deltas are divided by this
/// factor and capped, so an 18 s live session replays in a few seconds.
const MOCK_PACE_DIVISOR: u64 = 50;

/// Upper bound for a single replayed inter-message pause.
const MOCK_PACE_CAP_MS: u64 = 100;

/// Perception feed: streams live events and serves point-in-time snapshots.
///
/// Implementations never expose raw venue JSON; everything is mapped into
/// `sentinel_core` types at this boundary (P00 invariant #3).
///
/// `async_fn_in_trait` is allowed deliberately: this is an internal,
/// non-generic trait used behind concrete types (`LivePerpl`/`MockPerpl`); no
/// external implementor needs `Send` bounds on the returned futures.
#[allow(async_fn_in_trait)]
pub trait PerplFeed {
    /// Subscribe to the live event stream (market + account + feed health).
    ///
    /// The background producer stops when the returned receiver is dropped.
    async fn stream(&self) -> Receiver<FeedEvent>;

    /// Point-in-time account snapshot (positions + balances).
    async fn snapshot(&self) -> Result<AccountState>;

    /// Static market context (the markets the venue currently lists).
    async fn context(&self) -> Result<Vec<Market>>;
}

/// Live implementation over the Perpl gateway (REST + WebSocket).
pub struct LivePerpl {
    /// Signed REST client (context, ticker, wallet, positions).
    rest: PerplRest,
    /// Signer used by the trading socket (kept in an `Arc` so the streaming
    /// task can own it).
    ws_signer: Arc<ApiKeySigner>,
    /// WebSocket base URL (e.g. `wss://testnet.perpl.xyz`).
    ws_url: String,
    /// Chain id for stream names.
    chain_id: u64,
    /// Staleness threshold forwarded to the socket layer.
    stale_after: Duration,
    /// Cached market table from `/v1/pub/context`.
    markets: OnceCell<Vec<Market>>,
    /// Shutdown broadcast: dropping the `LivePerpl` (or calling
    /// [`LivePerpl::shutdown`]) stops every spawned stream.
    shutdown_tx: watch::Sender<bool>,
}

impl LivePerpl {
    /// Construct from configuration (builds the REST client and signers).
    ///
    /// # Errors
    /// Propagates `PerplError::Auth` for a malformed API key secret and
    /// `PerplError::Rest` if the HTTP client cannot be built.
    pub fn new(cfg: &Config) -> Result<Self> {
        let rest_signer = ApiKeySigner::from_config(&cfg.perpl)?;
        let ws_signer = Arc::new(ApiKeySigner::from_config(&cfg.perpl)?);
        let rest = PerplRest::new(cfg.perpl.api_url.clone(), rest_signer)?;
        let (shutdown_tx, _initial_rx) = watch::channel(false);
        Ok(Self {
            rest,
            ws_signer,
            ws_url: cfg.perpl.ws_url.clone(),
            chain_id: cfg.perpl.chain_id,
            stale_after: Duration::from_secs(cfg.risk.stale_data_alert_secs),
            markets: OnceCell::new(),
            shutdown_tx,
        })
    }

    /// Stop all spawned streams (idempotent).
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Full market table from `/v1/pub/context`, fetched once and cached.
    async fn markets(&self) -> Result<&Vec<Market>> {
        self.markets
            .get_or_try_init(|| async {
                let raw = self.rest.get_context().await?;
                types::parse_context(&raw)
            })
            .await
    }
}

impl PerplFeed for LivePerpl {
    async fn stream(&self) -> Receiver<FeedEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUS_CAPACITY);
        let shutdown_rx = self.shutdown_tx.subscribe();

        // Resolve the market table before spawning: the socket layer scales
        // prices and position sizes with the venue's own decimals. On failure
        // the stream degrades (mark events unavailable) rather than dying —
        // the error is logged loudly because it is actionable.
        let markets = match self.markets().await {
            Ok(markets) => markets.clone(),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "context fetch failed while starting the stream; running degraded (no market table)"
                );
                Vec::new()
            }
        };

        let ws_cfg = ws::WsConfig {
            ws_url: self.ws_url.clone(),
            chain_id: self.chain_id,
            markets,
            stale_after: self.stale_after,
        };
        let signer = Arc::clone(&self.ws_signer);
        tokio::spawn(run_live_ws(ws_cfg, signer, tx, shutdown_rx));
        rx
    }

    async fn snapshot(&self) -> Result<AccountState> {
        let markets = self.markets().await?.clone();
        let ticker = self.rest.get_ticker(None).await?;
        let marks = types::marks_from_ticker(&ticker, &markets)?;
        let wallet = self.rest.get_wallet().await?;
        let positions = self.rest.get_positions().await?;
        types::account_state(&wallet, &positions, &marks, &markets)
    }

    async fn context(&self) -> Result<Vec<Market>> {
        Ok(self.markets().await?.clone())
    }
}

/// Owns the signer inside the streaming task and logs an unexpected stop.
async fn run_live_ws(
    cfg: ws::WsConfig,
    signer: Arc<ApiKeySigner>,
    tx: Sender<FeedEvent>,
    shutdown: watch::Receiver<bool>,
) {
    if let Err(err) = ws::run(cfg, &signer, tx, shutdown).await {
        tracing::error!(error = %err, "perpl websocket run loop stopped");
    }
}

/// One line of a session fixture (`SPEC.md` §3.4).
#[derive(Debug, Clone)]
enum FixtureLine {
    /// A recorded REST response keyed by its exact request target.
    Rest {
        /// Request target, e.g. `/v1/trading/positions`.
        path: String,
        /// Raw recorded response payload.
        resp: serde_json::Value,
    },
    /// A recorded WebSocket frame with its offset from session start.
    Ws {
        /// Milliseconds since session start.
        t_ms: u64,
        /// Raw frame payload.
        msg: serde_json::Value,
    },
}

/// Fixture-backed implementation for offline dev, tests and demo replay.
///
/// `snapshot()`/`context()` are served from the fixture's `rest` lines
/// (recording signed endpoints requires a live key — see STUB-09);
/// `stream()` replays the `ws` lines with compressed pacing and emits the
/// same [`FeedEvent`]s as [`LivePerpl`].
pub struct MockPerpl {
    lines: Vec<FixtureLine>,
}

impl MockPerpl {
    /// Load a session fixture (JSONL; format frozen in `SPEC.md` §3.4).
    ///
    /// # Errors
    /// `PerplError::Rest` when the file cannot be read or a line is malformed.
    pub fn from_fixture(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| PerplError::Rest(format!("fixture {}: {err}", path.display())))?;
        let mut lines = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(raw).map_err(|err| {
                PerplError::Rest(format!("fixture {}:{}: {err}", path.display(), idx + 1))
            })?;
            match v.get("kind").and_then(serde_json::Value::as_str) {
                Some("rest") => {
                    let path_field = v
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            PerplError::Rest(format!(
                                "fixture {}:{}: rest line without a path",
                                path.display(),
                                idx + 1
                            ))
                        })?
                        .to_string();
                    let resp = v.get("resp").cloned().ok_or_else(|| {
                        PerplError::Rest(format!(
                            "fixture {}:{}: rest line without a response",
                            path.display(),
                            idx + 1
                        ))
                    })?;
                    lines.push(FixtureLine::Rest {
                        path: path_field,
                        resp,
                    });
                }
                Some("ws") => {
                    let t_ms = v
                        .get("t_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let msg = v.get("msg").cloned().ok_or_else(|| {
                        PerplError::Rest(format!(
                            "fixture {}:{}: ws line without a message",
                            path.display(),
                            idx + 1
                        ))
                    })?;
                    lines.push(FixtureLine::Ws { t_ms, msg });
                }
                other => {
                    return Err(PerplError::Rest(format!(
                        "fixture {}:{}: unknown fixture kind {other:?}",
                        path.display(),
                        idx + 1
                    ))
                    .into());
                }
            }
        }
        Ok(Self { lines })
    }

    /// First recorded REST response for `path`.
    fn rest_line(&self, path: &str) -> Option<&serde_json::Value> {
        self.lines.iter().find_map(|line| match line {
            FixtureLine::Rest { path: p, resp } if p == path => Some(resp),
            _ => None,
        })
    }

    /// Market table parsed from the fixture's recorded `/v1/pub/context`.
    fn markets(&self) -> Result<Vec<Market>> {
        let ctx = self.rest_line("/v1/pub/context").ok_or_else(|| {
            PerplError::Rest("fixture lacks a /v1/pub/context recording".to_string())
        })?;
        types::parse_context(ctx)
    }
}

impl PerplFeed for MockPerpl {
    async fn stream(&self) -> Receiver<FeedEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUS_CAPACITY);
        let lines = self.lines.clone();
        let markets = match self.markets() {
            Ok(markets) => markets,
            Err(err) => {
                tracing::error!(error = %err, "mock fixture has no usable market table; replay degraded");
                Vec::new()
            }
        };
        tokio::spawn(replay(lines, markets, tx));
        rx
    }

    async fn snapshot(&self) -> Result<AccountState> {
        let markets = self.markets()?;
        let wallet = self.rest_line("/v1/trading/wallet").ok_or_else(|| {
            PerplError::Rest("fixture lacks a /v1/trading/wallet recording".to_string())
        })?;
        let positions = self.rest_line("/v1/trading/positions").ok_or_else(|| {
            PerplError::Rest("fixture lacks a /v1/trading/positions recording".to_string())
        })?;
        let marks = match self.rest_line("/v1/market-data/ticker") {
            Some(ticker) => types::marks_from_ticker(ticker, &markets)?,
            None => HashMap::new(),
        };
        types::account_state(wallet, positions, &marks, &markets)
    }

    async fn context(&self) -> Result<Vec<Market>> {
        self.markets()
    }
}

/// Replay recorded WS frames as [`FeedEvent`]s with compressed pacing.
async fn replay(lines: Vec<FixtureLine>, markets: Vec<Market>, tx: Sender<FeedEvent>) {
    let mut marks: HashMap<u32, Decimal> = HashMap::new();
    let mut wallet: Option<serde_json::Value> = None;
    let mut positions: Option<serde_json::Value> = None;
    let mut prev_t: u64 = 0;

    for line in lines {
        if tx.is_closed() {
            return;
        }
        let FixtureLine::Ws { t_ms, msg } = line else {
            continue; // rest lines are served by snapshot()/context()
        };
        let delta = t_ms.saturating_sub(prev_t) / MOCK_PACE_DIVISOR;
        prev_t = t_ms;
        if delta > 0 {
            tokio::time::sleep(Duration::from_millis(delta.min(MOCK_PACE_CAP_MS))).await;
        }
        match msg.get("mt").and_then(serde_json::Value::as_u64) {
            Some(9) => {
                if replay_market_state(&msg, &markets, &mut marks, &tx)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(19) => {
                wallet = Some(msg);
                if replay_snapshot(&wallet, &positions, &marks, &markets, &tx)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(26) => {
                positions = Some(msg);
                if replay_snapshot(&wallet, &positions, &marks, &markets, &tx)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(21) => match types::parse_account_update(&msg) {
                Ok(update) => {
                    let event = FeedEvent::Account(AccountEvent::Update { update });
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "replay: unparseable account update skipped");
                }
            },
            _ => {}
        }
    }
}

/// Replay one `mt:9` market-state frame into mark updates.
async fn replay_market_state(
    msg: &serde_json::Value,
    markets: &[Market],
    marks: &mut HashMap<u32, Decimal>,
    tx: &Sender<FeedEvent>,
) -> std::result::Result<(), ()> {
    let Some(entries) = msg.get("d").and_then(serde_json::Value::as_object) else {
        return Ok(());
    };
    for (key, state) in entries {
        let Ok(market_id) = key.parse::<u32>() else {
            continue;
        };
        let Some(market) = markets.iter().find(|market| market.id.0 == market_id) else {
            continue;
        };
        let Some(raw) = state.get("mrk").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Ok(price) = Decimal::try_new(raw, market.price_decimals) else {
            continue;
        };
        let Some(ts) = state
            .get("at")
            .and_then(|at| at.get("t"))
            .and_then(serde_json::Value::as_i64)
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        else {
            continue;
        };
        marks.insert(market_id, price);
        let event = FeedEvent::Market(MarketEvent::MarkPrice {
            market_id: sentinel_core::types::MarketId(market_id),
            price,
            ts,
        });
        if tx.send(event).await.is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// Replay-compose an account snapshot once both wallet and positions landed.
async fn replay_snapshot(
    wallet: &Option<serde_json::Value>,
    positions: &Option<serde_json::Value>,
    marks: &HashMap<u32, Decimal>,
    markets: &[Market],
    tx: &Sender<FeedEvent>,
) -> std::result::Result<(), ()> {
    let (Some(wallet), Some(positions)) = (wallet, positions) else {
        return Ok(());
    };
    match types::account_state(wallet, positions, marks, markets) {
        Ok(state) => {
            let event = FeedEvent::Account(AccountEvent::Snapshot { state });
            if tx.send(event).await.is_err() {
                return Err(());
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "replay: snapshot composition skipped");
        }
    }
    Ok(())
}
