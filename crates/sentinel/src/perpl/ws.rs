//! WebSocket perception: market-data + trading sockets, event bus, staleness.
//!
//! Transport (docs/FACTS.md §1.10): the market-data socket connects
//! `{ws_url}/ws/v1/market-data` and subscribes with a first-frame `mt:5`
//! (`market-state@<chain>`, `heartbeat@<chain>`, one `order-book@<id>` per
//! configured market); `mt:9` frames become [`MarketEvent::MarkPrice`]. The
//! trading socket connects `{ws_url}/ws/v1/trading`, signs in with a
//! first-frame `mt:29` (`ApiKeySigner::ws_signin_frame`), translates `mt:19`
//! (wallet) / `mt:26` (positions) / `mt:21` (account update) frames and
//! sequence-checks the `mt:100` heartbeat (seeded from the wallet snapshot
//! `sn`; a gap forces a reconnect), keeping the socket alive with a
//! `{"mt":1,"t":<ms>}` ping every 30 s.
//!
//! Both sockets reconnect forever with exponential backoff ([`backoff_base`]
//! plus ≤ 25 % jitter, cap 30 s) and emit [`FeedEvent::Reconnected`] on
//! recovery. Every incoming frame resets the shared [`StaleDetector`]; a 1 s
//! checker emits [`FeedEvent::FeedStale`] once per stale episode. [`run`]
//! exits gracefully (`Ok`) when the shutdown watch flips and fails only for
//! startup-level problems (unparseable URL, unusable signer).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Market, MarketId};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{PerplError, Result};
use crate::perpl::auth::ApiKeySigner;
use crate::perpl::types::{self, AccountUpdate};

/// Exponential-backoff base: 500 ms · 2^attempt (before jitter).
const BACKOFF_BASE_MS: u64 = 500;

/// Backoff cap (SPEC §4: 30 s).
const BACKOFF_CAP_MS: u64 = 30_000;

/// Safety cap on the backoff exponent (2^8 · 500 ms already exceeds the cap).
const BACKOFF_MAX_SHIFT: u32 = 8;

/// Jitter is at most this fraction of the base delay (25 %).
const JITTER_DIVISOR: u64 = 4;

/// Trading-socket keep-alive ping period (protocol `mt:1`).
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How often the staleness checker polls the shared detector.
const STALE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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

/// Run configuration for the live sockets.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// WebSocket base URL (e.g. `wss://testnet.perpl.xyz`).
    pub ws_url: String,
    /// Chain id used in stream names (`market-state@<chain_id>`).
    pub chain_id: u64,
    /// Markets to subscribe to (`order-book@<id>`) and to translate. The full
    /// table (not just ids) lets the socket layer compose account snapshots
    /// (`size_decimals`, symbols) without inventing metadata (SPEC v1.0.1 §11c).
    pub markets: Vec<Market>,
    /// Emit [`FeedEvent::FeedStale`] after this long without events.
    pub stale_after: Duration,
}

/// Edge-triggered staleness detector (pure, deterministic; `now` in ms).
#[derive(Debug, Clone)]
pub struct StaleDetector {
    threshold: Duration,
    last_touch_ms: Option<u64>,
    fired: bool,
}

impl StaleDetector {
    /// New detector that fires once per stale episode.
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            last_touch_ms: None,
            fired: false,
        }
    }

    /// Record that an event was received at `now_ms` (resets the episode).
    pub fn touch(&mut self, now_ms: u64) {
        self.last_touch_ms = Some(now_ms);
        self.fired = false;
    }

    /// Poll for staleness; returns `Some(secs)` exactly once per episode.
    pub fn observe(&mut self, now_ms: u64) -> Option<u64> {
        let last_ms = self.last_touch_ms?;
        if self.fired {
            return None;
        }
        let elapsed_ms = u128::from(now_ms.saturating_sub(last_ms));
        if elapsed_ms < self.threshold.as_millis() {
            return None;
        }
        self.fired = true;
        u64::try_from(elapsed_ms / 1000).ok()
    }
}

/// Deterministic exponential backoff base: 500 ms · 2^attempt, cap 30 s.
pub fn backoff_base(attempt: u32) -> Duration {
    let factor = 1_u64 << attempt.min(BACKOFF_MAX_SHIFT);
    Duration::from_millis((BACKOFF_BASE_MS * factor).min(BACKOFF_CAP_MS))
}

/// Run both sockets until `shutdown` flips to `true`.
///
/// Runs the market-data and trading socket loops concurrently; events flow
/// through `tx`.
///
/// # Errors
/// `PerplError::Ws` on unrecoverable setup failures (individual disconnects
/// are handled internally by reconnecting).
pub async fn run(
    cfg: WsConfig,
    signer: &ApiKeySigner,
    tx: Sender<FeedEvent>,
    shutdown: WatchReceiver<bool>,
) -> Result<()> {
    // Startup-level validation: the endpoints must be parseable `ws`/`wss`
    // URLs. Everything that happens after this point is recovered by
    // reconnecting.
    let market_url = websocket_endpoint(&cfg.ws_url, "/ws/v1/market-data")?;
    let trading_url = websocket_endpoint(&cfg.ws_url, "/ws/v1/trading")?;
    // A signer that cannot produce a `mt:29` sign-in frame at all is a
    // startup-level failure (reconnecting could never recover it).
    let _ = signer.ws_signin_frame()?;

    let state = Mutex::new(SharedState::default());
    let stale = Mutex::new(StaleDetector::new(cfg.stale_after));
    let env = LoopEnv {
        cfg: &cfg,
        signer,
        market_url: &market_url,
        trading_url: &trading_url,
        tx: &tx,
        state: &state,
        stale: &stale,
    };

    tokio::join!(
        reconnect_loop(&env, shutdown.clone(), Side::Market),
        reconnect_loop(&env, shutdown.clone(), Side::Trading),
        stale_checker(&env, shutdown),
    );
    Ok(())
}

/// Shared, borrowed context for both socket loops and the staleness checker.
struct LoopEnv<'a> {
    /// Run configuration.
    cfg: &'a WsConfig,
    /// Signer used for the trading-socket `mt:29` frame.
    signer: &'a ApiKeySigner,
    /// Validated market-data endpoint URL.
    market_url: &'a str,
    /// Validated trading endpoint URL.
    trading_url: &'a str,
    /// Event bus sender shared by everything.
    tx: &'a Sender<FeedEvent>,
    /// State shared by both socket loops.
    state: &'a Mutex<SharedState>,
    /// Staleness detector shared by both socket loops.
    stale: &'a Mutex<StaleDetector>,
}

/// State shared by the two socket loops.
#[derive(Debug, Default)]
struct SharedState {
    /// Latest mark price per market id, in price units.
    marks: HashMap<u32, Decimal>,
    /// Latest `mt:19` wallet payload (raw venue JSON; never leaves this module).
    wallet: Option<Value>,
    /// Latest `mt:26` positions payload (raw venue JSON; never leaves this module).
    positions: Option<Value>,
}

/// Which socket a [`reconnect_loop`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// Public market-data socket.
    Market,
    /// Authenticated trading socket.
    Trading,
}

/// How one socket session ended; drives the reconnect loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    /// Stop the loop: shutdown was observed or the event bus was closed.
    Stop,
    /// The connection could not be established (reconnect with backoff).
    ConnectFailed,
    /// The connection was established and the session ended later.
    Ended,
}

/// What a message handler wants the session loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Keep reading.
    Continue,
    /// Stop the whole loop (shutdown or the event bus was closed).
    Stop,
    /// Force a reconnect (heartbeat sequence gap).
    Reconnect,
}

/// Reconnect driver shared by both sockets: run sessions forever, backing off
/// with [`backoff_base`] + ≤ 25 % jitter whenever a connection attempt fails.
async fn reconnect_loop(env: &LoopEnv<'_>, mut shutdown: WatchReceiver<bool>, side: Side) {
    let mut failures: u32 = 0;
    let mut ever_connected = false;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let outcome = match side {
            Side::Market => market_session(env, &mut shutdown, failures, ever_connected).await,
            Side::Trading => trading_session(env, &mut shutdown, failures, ever_connected).await,
        };
        match outcome {
            SessionOutcome::Stop => return,
            SessionOutcome::Ended => {
                // The connection was healthy: reconnect after a brief jittered
                // pause (avoids a hot loop when the server closes immediately).
                ever_connected = true;
                failures = 0;
                if sleep_or_shutdown(jittered_backoff(0), &mut shutdown).await {
                    return;
                }
            }
            SessionOutcome::ConnectFailed => {
                let delay = jittered_backoff(failures);
                failures = failures.saturating_add(1);
                if sleep_or_shutdown(delay, &mut shutdown).await {
                    return;
                }
            }
        }
    }
}

/// One market-data session: connect, subscribe (`mt:5`), then translate
/// `mt:9` frames until the socket or the process asks to stop.
async fn market_session(
    env: &LoopEnv<'_>,
    shutdown: &mut WatchReceiver<bool>,
    failures: u32,
    ever_connected: bool,
) -> SessionOutcome {
    let socket = tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => return SessionOutcome::Stop,
        result = connect_async(env.market_url) => match result {
            Ok((socket, _response)) => socket,
            Err(err) => {
                tracing::warn!(url = env.market_url, error = %err, "market-data connect failed");
                return SessionOutcome::ConnectFailed;
            }
        },
    };

    let mut socket = socket;
    if let Err(err) = socket
        .send(Message::text(subscription_frame(env.cfg)))
        .await
    {
        tracing::warn!(error = %err, "market-data subscribe frame failed");
        return SessionOutcome::ConnectFailed;
    }

    // Connection + subscription are in place: this counts as a recovery.
    if ever_connected || failures > 0 {
        if !emit(env.tx, FeedEvent::Reconnected { attempt: failures }).await {
            return SessionOutcome::Stop;
        }
        tracing::info!(attempt = failures, "market-data reconnected");
    }

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return SessionOutcome::Stop,
            () = env.tx.closed() => return SessionOutcome::Stop,
            incoming = socket.next() => match incoming {
                Some(Ok(message)) => {
                    touch(env.stale).await;
                    match message {
                        Message::Text(text) => {
                            if handle_market_message(text.as_str(), env).await == Flow::Stop {
                                return SessionOutcome::Stop;
                            }
                        }
                        Message::Close(_) => {
                            tracing::debug!("market-data socket closed by server");
                            return SessionOutcome::Ended;
                        }
                        Message::Binary(_)
                        | Message::Ping(_)
                        | Message::Pong(_)
                        | Message::Frame(_) => {}
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "market-data socket error");
                    return SessionOutcome::Ended;
                }
                None => {
                    tracing::debug!("market-data stream ended");
                    return SessionOutcome::Ended;
                }
            },
        }
    }
}

/// Translate one market-data text frame: `mt:9` market state (its `d` object
/// is keyed by market id); every other message type is ignored.
async fn handle_market_message(text: &str, env: &LoopEnv<'_>) -> Flow {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("market-data: ignoring non-JSON frame");
        return Flow::Continue;
    };
    if value.get("mt").and_then(Value::as_u64) != Some(9) {
        return Flow::Continue;
    }
    let Some(entries) = value.get("d").and_then(Value::as_object) else {
        return Flow::Continue;
    };

    for (key, market_state) in entries {
        let Ok(market_id) = key.parse::<u32>() else {
            continue;
        };
        // The scaling exponent comes from the configured market table; a
        // state entry for a market we did not subscribe to cannot be scaled.
        let Some(market) = env
            .cfg
            .markets
            .iter()
            .find(|market| market.id.0 == market_id)
        else {
            tracing::debug!(
                market_id = market_id,
                "market-data: state for unsubscribed market ignored"
            );
            continue;
        };
        let Some(raw) = market_state.get("mrk").and_then(Value::as_i64) else {
            continue;
        };
        let Some(price) = scale_raw(raw, market.price_decimals) else {
            tracing::warn!(
                market_id = market_id,
                raw = raw,
                "market-data: mark price outside Decimal's range, skipped"
            );
            continue;
        };
        let Some(ts) = market_state
            .get("at")
            .and_then(|at| at.get("t"))
            .and_then(Value::as_i64)
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            tracing::debug!(
                market_id = market_id,
                "market-data: mark price without a usable timestamp skipped"
            );
            continue;
        };

        env.state.lock().await.marks.insert(market_id, price);
        let event = FeedEvent::Market(MarketEvent::MarkPrice {
            market_id: MarketId(market_id),
            price,
            ts,
        });
        if !emit(env.tx, event).await {
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// One trading session: connect, sign in (`mt:29`, first frame), then
/// translate snapshots/updates and keep the connection alive with `mt:1`
/// pings every [`PING_INTERVAL`].
async fn trading_session(
    env: &LoopEnv<'_>,
    shutdown: &mut WatchReceiver<bool>,
    failures: u32,
    ever_connected: bool,
) -> SessionOutcome {
    let socket = tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => return SessionOutcome::Stop,
        result = connect_async(env.trading_url) => match result {
            Ok((socket, _response)) => socket,
            Err(err) => {
                tracing::warn!(url = env.trading_url, error = %err, "trading connect failed");
                return SessionOutcome::ConnectFailed;
            }
        },
    };

    let signin = match env.signer.ws_signin_frame() {
        Ok(frame) => frame,
        Err(err) => {
            tracing::warn!(error = %err, "trading sign-in frame could not be produced");
            return SessionOutcome::ConnectFailed;
        }
    };

    let mut socket = socket;
    if let Err(err) = socket.send(Message::text(signin)).await {
        tracing::warn!(error = %err, "trading sign-in frame failed to send");
        return SessionOutcome::ConnectFailed;
    }

    if ever_connected || failures > 0 {
        if !emit(env.tx, FeedEvent::Reconnected { attempt: failures }).await {
            return SessionOutcome::Stop;
        }
        tracing::info!(attempt = failures, "trading reconnected");
    }

    let (mut sink, mut stream) = socket.split();
    let mut pings = interval(PING_INTERVAL);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    pings.tick().await; // discard the immediate first tick: ping after 30 s

    let mut last_sn: Option<u64> = None;

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return SessionOutcome::Stop,
            () = env.tx.closed() => return SessionOutcome::Stop,
            _ = pings.tick() => {
                let payload = json!({ "mt": 1, "t": unix_ms() }).to_string();
                if let Err(err) = sink.send(Message::text(payload)).await {
                    tracing::warn!(error = %err, "trading ping failed");
                    return SessionOutcome::Ended;
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(message)) => {
                    touch(env.stale).await;
                    match message {
                        Message::Text(text) => {
                            match handle_trading_message(text.as_str(), env, &mut last_sn).await {
                                Flow::Continue => {}
                                Flow::Stop => return SessionOutcome::Stop,
                                Flow::Reconnect => return SessionOutcome::Ended,
                            }
                        }
                        Message::Close(_) => {
                            tracing::debug!("trading socket closed by server");
                            return SessionOutcome::Ended;
                        }
                        Message::Binary(_)
                        | Message::Ping(_)
                        | Message::Pong(_)
                        | Message::Frame(_) => {}
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "trading socket error");
                    return SessionOutcome::Ended;
                }
                None => {
                    tracing::debug!("trading stream ended");
                    return SessionOutcome::Ended;
                }
            },
        }
    }
}

/// Translate one trading-socket text frame. Handles `mt:19` (wallet
/// snapshot), `mt:26` (positions snapshot), `mt:21` (account update) and
/// `mt:100` (heartbeat sequence check); everything else is ignored.
async fn handle_trading_message(text: &str, env: &LoopEnv<'_>, last_sn: &mut Option<u64>) -> Flow {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("trading: ignoring non-JSON frame");
        return Flow::Continue;
    };
    match value.get("mt").and_then(Value::as_u64) {
        Some(19) => {
            // Wallet snapshot: seed the heartbeat sequence from its `sn`
            // (docs: heartbeat continuity is seeded from the wallet snapshot).
            if let Some(sn) = value.get("sn").and_then(Value::as_u64) {
                *last_sn = Some(sn);
            } else {
                tracing::debug!("trading: wallet snapshot without a sequence number");
            }
            env.state.lock().await.wallet = Some(value);
            compose_snapshot(env).await;
            Flow::Continue
        }
        Some(26) => {
            env.state.lock().await.positions = Some(value);
            compose_snapshot(env).await;
            Flow::Continue
        }
        Some(21) => match types::parse_account_update(&value) {
            Ok(update) => {
                let event = FeedEvent::Account(AccountEvent::Update { update });
                if emit(env.tx, event).await {
                    Flow::Continue
                } else {
                    Flow::Stop
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "trading: unparseable account update ignored");
                Flow::Continue
            }
        },
        Some(100) => {
            let Some(sn) = value.get("sn").and_then(Value::as_u64) else {
                tracing::debug!("trading: heartbeat without a sequence number");
                return Flow::Continue;
            };
            match *last_sn {
                Some(previous) if previous.checked_add(1) != Some(sn) => {
                    tracing::warn!(
                        previous = previous,
                        received = sn,
                        "trading: heartbeat sequence gap; reconnecting"
                    );
                    Flow::Reconnect
                }
                _ => {
                    *last_sn = Some(sn);
                    Flow::Continue
                }
            }
        }
        _ => Flow::Continue,
    }
}

/// Compose an [`AccountEvent::Snapshot`] from the shared `mt:19` wallet and
/// `mt:26` positions payloads once both are present.
///
/// The full market table travels in [`WsConfig::markets`], so position sizes
/// and prices are scaled with the venue's own `size_decimals`/`price_decimals`
/// (SPEC v1.0.1 §11c) instead of any invented metadata.
async fn compose_snapshot(env: &LoopEnv<'_>) {
    let (wallet, positions, marks) = {
        let state = env.state.lock().await;
        let (Some(wallet), Some(positions)) = (state.wallet.as_ref(), state.positions.as_ref())
        else {
            return;
        };
        (wallet.clone(), positions.clone(), state.marks.clone())
    };
    match types::account_state(&wallet, &positions, &marks, &env.cfg.markets) {
        Ok(account) => {
            let event = FeedEvent::Account(AccountEvent::Snapshot { state: account });
            let _ = emit(env.tx, event).await;
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "trading: account snapshot needs the full market table; skipped"
            );
        }
    }
}

/// 1 s staleness checker: emits [`FeedEvent::FeedStale`] once per episode as
/// soon as the shared detector crosses the configured threshold.
async fn stale_checker(env: &LoopEnv<'_>, mut shutdown: WatchReceiver<bool>) {
    let mut ticker = interval(STALE_CHECK_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            () = env.tx.closed() => return,
            _ = ticker.tick() => {
                let now_ms = unix_ms();
                if let Some(secs) = env.stale.lock().await.observe(now_ms) {
                    tracing::warn!(secs = secs, "perpl feed stale");
                    if !emit(env.tx, FeedEvent::FeedStale { secs }).await {
                        return;
                    }
                }
            }
        }
    }
}

/// Send one event; `false` when the receiver is gone (the loops then stop).
async fn emit(tx: &Sender<FeedEvent>, event: FeedEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// Record an incoming frame on the shared staleness detector.
async fn touch(stale: &Mutex<StaleDetector>) {
    stale.lock().await.touch(unix_ms());
}

/// Resolve when shutdown is signalled (`true`), including a dropped sender.
async fn wait_for_shutdown(shutdown: &mut WatchReceiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Sleep `delay`, returning `true` when shutdown is signalled while waiting.
async fn sleep_or_shutdown(delay: Duration, shutdown: &mut WatchReceiver<bool>) -> bool {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => true,
        () = sleep(delay) => *shutdown.borrow(),
    }
}

/// [`backoff_base`] plus up to 25 % jitter, sampled from the OS entropy pool
/// (`getrandom`); falls back to the un-jittered base if entropy is unavailable.
fn jittered_backoff(attempt: u32) -> Duration {
    let base_ms = u64::try_from(backoff_base(attempt).as_millis()).unwrap_or(BACKOFF_CAP_MS);
    let jitter_span = base_ms / JITTER_DIVISOR;
    let mut bytes = [0_u8; 8];
    let extra_ms = match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes) % (jitter_span + 1),
        Err(_) => 0,
    };
    Duration::from_millis(base_ms + extra_ms)
}

/// The market-data `mt:5` subscription frame: `market-state@<chain>`,
/// `heartbeat@<chain>` and one `order-book@<id>` per configured market.
fn subscription_frame(cfg: &WsConfig) -> String {
    let mut subs = vec![
        json!({ "stream": format!("market-state@{}", cfg.chain_id), "subscribe": true }),
        json!({ "stream": format!("heartbeat@{}", cfg.chain_id), "subscribe": true }),
    ];
    subs.extend(cfg.markets.iter().map(
        |market| json!({ "stream": format!("order-book@{}", market.id.0), "subscribe": true }),
    ));
    json!({ "mt": 5, "subs": subs }).to_string()
}

/// Scale a raw venue integer by `decimals` (`raw / 10^decimals`); `None` when
/// the scale is outside `Decimal`'s range (never for venue data).
fn scale_raw(raw: i64, decimals: u32) -> Option<Decimal> {
    Decimal::try_new(raw, decimals).ok()
}

/// Validate `{base}{path}` and return it when it parses as a `ws`/`wss` URL;
/// this is the startup-level failure channel of [`run`].
fn websocket_endpoint(base: &str, path: &str) -> Result<String> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let parsed = url::Url::parse(&url)
        .map_err(|err| PerplError::Ws(format!("invalid websocket URL {url:?}: {err}")))?;
    match parsed.scheme() {
        "ws" | "wss" => Ok(url),
        scheme => Err(PerplError::Ws(format!(
            "unsupported websocket URL scheme {scheme:?} (expected ws or wss): {url:?}"
        ))
        .into()),
    }
}

/// Milliseconds since the Unix epoch (0 before the epoch — never in practice).
fn unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal core `Market` for socket-layer tests (SPEC v1.0.1 §11c).
    fn test_market(id: u32, price_decimals: u32) -> Market {
        Market {
            id: MarketId(id),
            symbol: format!("T{id}"),
            base: format!("T{id}"),
            price_decimals,
            size_decimals: 3,
            initial_margin_fraction: Decimal::new(1, 1),
            maintenance_margin_fraction: Decimal::new(5, 2),
            max_leverage: Decimal::new(10, 0),
            min_size: Decimal::ZERO,
            tick_size: Decimal::new(1, price_decimals),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    #[test]
    fn backoff_base_matches_spec_table() {
        // SPEC §4/§9: attempts 0..=7 = 500ms, 1s, 2s, 4s, 8s, 16s, 30s, 30s.
        let expected_ms = [500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000];
        for (attempt, expected) in expected_ms.iter().enumerate() {
            assert_eq!(
                backoff_base(attempt as u32),
                Duration::from_millis(*expected),
                "attempt {attempt}"
            );
        }
    }

    #[test]
    fn backoff_base_is_capped_for_large_attempts() {
        assert_eq!(backoff_base(6), Duration::from_secs(30));
        assert_eq!(backoff_base(30), Duration::from_secs(30));
        assert_eq!(backoff_base(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn jittered_backoff_stays_within_twenty_five_percent() {
        for attempt in [0, 5, 9] {
            let base = backoff_base(attempt);
            let ceiling = base + base / 4;
            for _ in 0..16 {
                let jittered = jittered_backoff(attempt);
                assert!(jittered >= base, "jitter never shrinks the delay");
                assert!(
                    jittered <= ceiling,
                    "jitter must stay within 25 %: {jittered:?} > {ceiling:?}"
                );
            }
        }
    }

    #[test]
    fn stale_detector_never_fires_without_a_touch() {
        let mut detector = StaleDetector::new(Duration::ZERO);
        assert_eq!(detector.observe(1), None);
        assert_eq!(detector.observe(u64::MAX), None);
    }

    #[test]
    fn stale_detector_fires_once_at_the_threshold_and_resets_on_touch() {
        let mut detector = StaleDetector::new(Duration::from_secs(5));
        detector.touch(1_000);
        assert_eq!(detector.observe(5_999), None, "below threshold");
        assert_eq!(
            detector.observe(6_000),
            Some(5),
            "at threshold: fires with whole seconds since the last touch"
        );
        assert_eq!(detector.observe(7_000), None, "exactly once per episode");
        assert_eq!(detector.observe(60_000), None, "still the same episode");
        detector.touch(60_000);
        assert_eq!(detector.observe(64_999), None, "touch reset the episode");
        assert_eq!(
            detector.observe(65_500),
            Some(5),
            "the next stale episode fires again"
        );
    }

    #[test]
    fn stale_detector_reports_whole_seconds() {
        let mut detector = StaleDetector::new(Duration::from_millis(1_500));
        detector.touch(10_000);
        assert_eq!(detector.observe(11_600), Some(1));
    }

    #[test]
    fn subscription_frame_matches_the_frozen_mt5_shape() {
        let cfg = WsConfig {
            ws_url: "wss://testnet.perpl.xyz".to_string(),
            chain_id: 10_143,
            markets: vec![test_market(32, 2), test_market(16, 1)],
            stale_after: Duration::from_secs(30),
        };
        let frame: Value = serde_json::from_str(&subscription_frame(&cfg)).expect("frame is JSON");
        assert_eq!(frame["mt"], json!(5));
        assert_eq!(
            frame["subs"],
            json!([
                { "stream": "market-state@10143", "subscribe": true },
                { "stream": "heartbeat@10143", "subscribe": true },
                { "stream": "order-book@32", "subscribe": true },
                { "stream": "order-book@16", "subscribe": true }
            ])
        );
    }

    #[test]
    fn scale_raw_divides_by_the_decimal_scale() {
        assert_eq!(scale_raw(271_370, 2), Some(Decimal::new(271_370, 2)));
        assert_eq!(scale_raw(-5_000, 3), Some(Decimal::new(-5_000, 3)));
        assert_eq!(scale_raw(1, 29), None, "beyond Decimal's scale range");
    }

    #[test]
    fn websocket_endpoint_rejects_bad_urls() {
        assert!(websocket_endpoint("not a url", "/ws/v1/trading").is_err());
        assert!(websocket_endpoint("http://example.com", "/ws/v1/trading").is_err());
        assert_eq!(
            websocket_endpoint("wss://testnet.perpl.xyz/", "/ws/v1/trading").expect("valid ws URL"),
            "wss://testnet.perpl.xyz/ws/v1/trading"
        );
    }
}
