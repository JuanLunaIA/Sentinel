//! Alert delivery v1 — tier changes and reflex actions to tracing/Telegram.
//!
//! Frozen by `SPEC-P06.md` §3. Dedupe rule: one alert per `(market, tier)`
//! until the tier changes; a persistent `Red` re-alerts every 15 minutes.
//! All timestamps come from `Alert::at_ms` (logical in replay → deterministic).
//! Suppressed alerts return `Ok(())` without touching the inner sink; sink
//! errors propagate to the caller (the pipeline logs them, never fatal) —
//! except [`TelegramSink`], whose deliveries are queued and retried in the
//! background and never surface to the pipeline (SPEC-P16 §2).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use sentinel_core::types::{MarketId, RiskTier};
use serde::Serialize;
use teloxide::requests::Requester;

use crate::error::Result;

/// Persistent-Red re-alert interval, ms (SPEC-P06 §3).
pub const RED_REALERT_MS: u64 = 15 * 60 * 1000;

/// What an alert is about.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertKind {
    /// Risk tier crossed into `to` (from `from`, `None` on first sight).
    TierChange {
        /// Previous tier, `None` when the market was first seen.
        from: Option<RiskTier>,
        /// New tier.
        to: RiskTier,
        /// Rendered distance-to-liquidation percent at the transition.
        distance_pct: String,
    },
    /// A reflex engine action was submitted.
    ReflexAction {
        /// Decision correlation id.
        decision_id: String,
        /// Report status or skip reason.
        status: String,
        /// Submitted size in base units (rendered).
        size: String,
        /// Deterministic client order id.
        client_order_id: String,
    },
    /// A strategy consult is due (Yellow entry — brain lands in P07).
    ConsultScheduled {
        /// Tier that triggered the consult.
        tier: RiskTier,
    },
    /// Feed went stale.
    FeedStale {
        /// Seconds since the last event.
        secs: u64,
    },
}

/// One outbound alert.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alert {
    /// What this alert is about.
    pub kind: AlertKind,
    /// Market the alert concerns (when applicable).
    pub market_id: Option<MarketId>,
    /// Preformatted human-readable text.
    pub text: String,
    /// Event timestamp (logical ms in replay).
    pub at_ms: u64,
}

/// Destination for alerts. Errors are logged by the pipeline, never fatal.
#[allow(async_fn_in_trait)]
pub trait AlertSink {
    /// Deliver one alert.
    async fn send(&self, alert: &Alert) -> Result<()>;
}

/// Logs every alert via `tracing` (target `sentinel::alert`).
#[derive(Debug, Default)]
pub struct TracingSink;

impl AlertSink for TracingSink {
    async fn send(&self, alert: &Alert) -> Result<()> {
        tracing::info!(
            target: "sentinel::alert",
            kind = kind_label(&alert.kind),
            market_id = alert.market_id.map(|market| market.0),
            text = %alert.text,
            "alert"
        );
        Ok(())
    }
}

/// Captures alerts for tests and evidence.
#[derive(Debug, Default)]
pub struct RecordingSink {
    /// Shared capture buffer (clone it before the sink moves into a pipeline).
    pub alerts: Arc<Mutex<Vec<Alert>>>,
}

impl RecordingSink {
    /// New sink with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl AlertSink for RecordingSink {
    async fn send(&self, alert: &Alert) -> Result<()> {
        // Recover from a poisoned mutex: a panicked reader must not stop the
        // capture (and this call must never panic).
        self.alerts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(alert.clone());
        Ok(())
    }
}

/// Telegram delivery via `teloxide::Bot::sendMessage` (plain text v1).
///
/// P16: delivery is queued. [`TelegramSink::send`] enqueues into a bounded
/// queue (32) serviced by a background worker that retries up to 3 times with
/// exponential backoff (500 ms base); on overflow the **oldest** alert is
/// dropped and a warning is logged. `send` always returns `Ok(())` — Telegram
/// failures never propagate into the pipeline.
pub struct TelegramSink {
    /// Bot client.
    pub bot: teloxide::Bot,
    /// Target chat id.
    pub chat_id: teloxide::types::ChatId,
    /// Bounded outbound queue shared with the background worker.
    queue: Arc<AlertQueue>,
    /// Set exactly once, when the queue's worker task is spawned.
    worker_started: OnceLock<()>,
}

impl TelegramSink {
    /// Build from a token and a chat id.
    pub fn new(token: &str, chat_id: i64) -> Self {
        Self::with_bot(teloxide::Bot::new(token.to_string()), chat_id)
    }

    /// Build around an existing bot client (tests point it at a mock server).
    pub fn with_bot(bot: teloxide::Bot, chat_id: i64) -> Self {
        Self {
            bot,
            chat_id: teloxide::types::ChatId(chat_id),
            queue: AlertQueue::new(),
            worker_started: OnceLock::new(),
        }
    }

    /// Spawn the delivery worker on first use (single spawn per sink).
    fn ensure_worker(&self) {
        if self.worker_started.set(()).is_ok() {
            tokio::spawn(run_worker(
                Arc::clone(&self.queue),
                self.bot.clone(),
                self.chat_id,
            ));
        }
    }
}

// Hand-written (never derived): teloxide's `Bot` renders its token field in
// its derived `Debug`, so a derived `TelegramSink` `Debug` would leak the
// token into logs (P00 invariant #5).
impl fmt::Debug for TelegramSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramSink")
            .field("bot", &"REDACTED")
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

impl AlertSink for TelegramSink {
    async fn send(&self, alert: &Alert) -> Result<()> {
        self.ensure_worker();
        self.queue.push(alert.clone());
        Ok(())
    }
}

/// Outbound queue capacity (frozen: 32, SPEC-P16 §2).
const QUEUE_CAPACITY: usize = 32;
/// Retries after the initial delivery attempt (frozen: 3).
const MAX_RETRIES: u32 = 3;
/// Base delay between delivery retries (doubles per retry: 500ms/1s/2s).
const RETRY_BASE: Duration = Duration::from_millis(500);

/// Bounded FIFO alert queue with a worker wake signal.
struct AlertQueue {
    items: Mutex<VecDeque<Alert>>,
    wake: tokio::sync::Semaphore,
    dropped: AtomicU64,
}

impl AlertQueue {
    /// Empty queue.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            items: Mutex::new(VecDeque::new()),
            wake: tokio::sync::Semaphore::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    /// Lock the buffer, recovering from a poisoned mutex: a panicked consumer
    /// must not silence every later alert.
    fn lock_items(&self) -> MutexGuard<'_, VecDeque<Alert>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enqueue one alert; when full, drop the oldest and warn. Never fails.
    fn push(&self, alert: Alert) {
        {
            let mut items = self.lock_items();
            if items.len() >= QUEUE_CAPACITY
                && let Some(dropped) = items.pop_front()
            {
                let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    kind = kind_label(&dropped.kind),
                    capacity = QUEUE_CAPACITY,
                    dropped_total = total,
                    "telegram alert queue full; dropping oldest alert"
                );
            }
            items.push_back(alert);
        }
        self.wake.add_permits(1);
    }

    /// Take the oldest queued alert, if any.
    fn pop(&self) -> Option<Alert> {
        self.lock_items().pop_front()
    }

    /// Current queue depth (test observability).
    #[cfg(test)]
    fn queued_len(&self) -> usize {
        self.lock_items().len()
    }
}

/// Background worker: drains the queue forever, delivering with retries.
async fn run_worker(queue: Arc<AlertQueue>, bot: teloxide::Bot, chat_id: teloxide::types::ChatId) {
    loop {
        // A permit means "at least one alert was enqueued": drain everything
        // currently queued before waiting again.
        let Ok(_wake) = queue.wake.acquire().await else {
            return; // semaphore closed: no further wakeups can arrive
        };
        while let Some(alert) = queue.pop() {
            deliver(&bot, chat_id, &alert).await;
        }
    }
}

/// Deliver one alert: initial attempt plus up to [`MAX_RETRIES`] retries with
/// exponential backoff. Called only from the worker; failures are logged and
/// swallowed, never propagated.
async fn deliver(bot: &teloxide::Bot, chat_id: teloxide::types::ChatId, alert: &Alert) {
    let mut retries = 0u32;
    loop {
        match bot.send_message(chat_id, alert.text.clone()).await {
            Ok(_) => return,
            Err(err) => {
                // `RequestError` never carries the bot token (teloxide
                // redacts it from network errors), so this is safe to log.
                if retries >= MAX_RETRIES {
                    tracing::error!(
                        error = %err,
                        retries,
                        "telegram delivery failed; alert dropped after retries"
                    );
                    return;
                }
                retries += 1;
                let delay = RETRY_BASE * 2u32.pow(retries - 1);
                tracing::warn!(
                    error = %err,
                    retry = retries,
                    retry_in_ms = delay.as_millis() as u64,
                    "telegram delivery failed; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Dedupe wrapper: suppresses repeated `(market, tier)` tier-change alerts;
/// a persistent `Red` re-alerts every [`RED_REALERT_MS`]; non-tier alerts
/// always pass. The clock is `Alert::at_ms` (deterministic in replay).
#[derive(Debug)]
pub struct DedupeSink<S> {
    inner: S,
    seen: Mutex<HashMap<MarketId, (RiskTier, u64)>>,
}

impl<S> DedupeSink<S> {
    /// Wrap `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Lock the dedupe table, recovering from a poisoned mutex: a panicked
    /// alert consumer must not silence every later alert.
    fn lock_seen(&self) -> MutexGuard<'_, HashMap<MarketId, (RiskTier, u64)>> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<S: AlertSink + Sync> AlertSink for DedupeSink<S> {
    async fn send(&self, alert: &Alert) -> Result<()> {
        // Dedupe applies to tier changes only; every other kind always passes.
        let AlertKind::TierChange { to, .. } = &alert.kind else {
            return self.inner.send(alert).await;
        };
        let tier = *to;
        // A tier change without a market cannot be keyed — fail open.
        let Some(market_id) = alert.market_id else {
            return self.inner.send(alert).await;
        };

        // Decide under the lock; never hold it across the `await` below.
        let forward = {
            let seen = self.lock_seen();
            match seen.get(&market_id) {
                Some((stored_tier, stored_ms)) if *stored_tier == tier => {
                    // Repeat of the same (market, tier): suppressed, unless a
                    // persistent `Red` has reached the re-alert window. The
                    // clock is `at_ms` (logical), never wall time.
                    tier == RiskTier::Red
                        && alert.at_ms.saturating_sub(*stored_ms) >= RED_REALERT_MS
                }
                _ => true,
            }
        };
        if !forward {
            // Suppression sends nothing and is not an error.
            return Ok(());
        }

        self.inner.send(alert).await?;
        // Only a delivered alert advances the stored timestamp; a failed send
        // keeps the state so the next alert retries.
        self.lock_seen().insert(market_id, (tier, alert.at_ms));
        Ok(())
    }
}

/// Short stable discriminator for an alert kind (used as a tracing field).
fn kind_label(kind: &AlertKind) -> &'static str {
    match kind {
        AlertKind::TierChange { .. } => "tier_change",
        AlertKind::ReflexAction { .. } => "reflex_action",
        AlertKind::ConsultScheduled { .. } => "consult_scheduled",
        AlertKind::FeedStale { .. } => "feed_stale",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SentinelError;

    /// Market used by the dedupe tests.
    const MARKET: MarketId = MarketId(32);
    /// Base logical timestamp for the dedupe tests.
    const T0: u64 = 1_000_000;

    /// Tier-change alert for [`MARKET`] (or another market when patched).
    fn tier_alert(from: Option<RiskTier>, to: RiskTier, at_ms: u64) -> Alert {
        Alert {
            kind: AlertKind::TierChange {
                from,
                to,
                distance_pct: "24.31".to_string(),
            },
            market_id: Some(MARKET),
            text: format!("tier change {from:?}→{to:?} at {at_ms}ms"),
            at_ms,
        }
    }

    /// Feed-stale alert (non-tier kind, no market).
    fn stale_alert(secs: u64, at_ms: u64) -> Alert {
        Alert {
            kind: AlertKind::FeedStale { secs },
            market_id: None,
            text: format!("feed stale {secs}s"),
            at_ms,
        }
    }

    /// `DedupeSink` over a [`RecordingSink`], plus its capture buffer.
    fn dedupe_recorder() -> (DedupeSink<RecordingSink>, Arc<Mutex<Vec<Alert>>>) {
        let sink = RecordingSink::new();
        let capture = Arc::clone(&sink.alerts);
        (DedupeSink::new(sink), capture)
    }

    /// Snapshot of everything the sink captured so far.
    fn captured(capture: &Arc<Mutex<Vec<Alert>>>) -> Vec<Alert> {
        capture.lock().expect("capture buffer lock").clone()
    }

    #[tokio::test]
    async fn dedupe_first_tier_change_passes() {
        let (sink, capture) = dedupe_recorder();
        let first = tier_alert(None, RiskTier::Red, T0);
        sink.send(&first).await.expect("first alert forwards");
        assert_eq!(captured(&capture), vec![first]);
    }

    #[tokio::test]
    async fn dedupe_repeat_same_tier_is_suppressed() {
        let (sink, capture) = dedupe_recorder();
        let first = tier_alert(None, RiskTier::Yellow, T0);
        sink.send(&first).await.expect("first sighting forwards");
        // Same (market, tier) again: `from` differs, only `to` matters.
        sink.send(&tier_alert(
            Some(RiskTier::Green),
            RiskTier::Yellow,
            T0 + 1_000,
        ))
        .await
        .expect("suppression returns Ok without sending");
        assert_eq!(captured(&capture), vec![first]);
    }

    #[tokio::test]
    async fn dedupe_tier_change_passes_immediately() {
        let (sink, capture) = dedupe_recorder();
        sink.send(&tier_alert(None, RiskTier::Green, T0))
            .await
            .expect("first sighting forwards");
        // Escalation forwards immediately (tier changed)…
        sink.send(&tier_alert(Some(RiskTier::Green), RiskTier::Yellow, T0 + 1))
            .await
            .expect("escalation forwards");
        // …and so does the (recovered) de-escalation.
        sink.send(&tier_alert(Some(RiskTier::Yellow), RiskTier::Green, T0 + 2))
            .await
            .expect("de-escalation forwards");
        assert_eq!(captured(&capture).len(), 3);
    }

    #[tokio::test]
    async fn dedupe_red_realerts_only_at_the_window() {
        let (sink, capture) = dedupe_recorder();
        sink.send(&tier_alert(Some(RiskTier::Orange), RiskTier::Red, T0))
            .await
            .expect("entry to Red forwards");
        // One millisecond before the window: suppressed.
        sink.send(&tier_alert(
            Some(RiskTier::Orange),
            RiskTier::Red,
            T0 + RED_REALERT_MS - 1,
        ))
        .await
        .expect("early repeat is suppressed");
        assert_eq!(captured(&capture).len(), 1);

        // Exactly at the window: the persistent Red re-alerts and re-stores.
        sink.send(&tier_alert(
            Some(RiskTier::Orange),
            RiskTier::Red,
            T0 + RED_REALERT_MS,
        ))
        .await
        .expect("window boundary forwards");
        assert_eq!(captured(&capture).len(), 2);

        // The window restarts from the re-alert (`T0 + RED_REALERT_MS`).
        sink.send(&tier_alert(
            Some(RiskTier::Orange),
            RiskTier::Red,
            T0 + 2 * RED_REALERT_MS - 1,
        ))
        .await
        .expect("early repeat is suppressed");
        assert_eq!(captured(&capture).len(), 2);
        sink.send(&tier_alert(
            Some(RiskTier::Orange),
            RiskTier::Red,
            T0 + 2 * RED_REALERT_MS,
        ))
        .await
        .expect("second window boundary forwards");
        assert_eq!(captured(&capture).len(), 3);
    }

    #[tokio::test]
    async fn dedupe_non_red_never_realerts() {
        let (sink, capture) = dedupe_recorder();
        sink.send(&tier_alert(None, RiskTier::Yellow, T0))
            .await
            .expect("first Yellow forwards");
        // No re-alert window exists for non-Red tiers: suppressed even 1 h on.
        sink.send(&tier_alert(None, RiskTier::Yellow, T0 + 60 * 60 * 1000))
            .await
            .expect("Yellow repeat is suppressed");
        sink.send(&tier_alert(None, RiskTier::Green, T0 + 60 * 60 * 1000 + 1))
            .await
            .expect("first Green forwards");
        sink.send(&tier_alert(None, RiskTier::Green, T0 + 2 * 60 * 60 * 1000))
            .await
            .expect("Green repeat is suppressed");
        assert_eq!(captured(&capture).len(), 2);
    }

    #[tokio::test]
    async fn dedupe_non_tier_kinds_always_pass_even_identical() {
        let (sink, capture) = dedupe_recorder();
        let reflex = Alert {
            kind: AlertKind::ReflexAction {
                decision_id: "dec-1".to_string(),
                status: "simulated".to_string(),
                size: "2.500".to_string(),
                client_order_id: "sentinel-00001".to_string(),
            },
            market_id: Some(MARKET),
            text: "action reduce size 2.500 client sentinel-00001 status simulated".to_string(),
            at_ms: T0,
        };
        let consult = Alert {
            kind: AlertKind::ConsultScheduled {
                tier: RiskTier::Yellow,
            },
            market_id: Some(MARKET),
            text: "consult scheduled (yellow)".to_string(),
            at_ms: T0,
        };
        let stale = stale_alert(70, T0);
        let mut expected = Vec::new();
        for _ in 0..2 {
            sink.send(&reflex)
                .await
                .expect("reflex actions always pass");
            sink.send(&consult).await.expect("consults always pass");
            sink.send(&stale).await.expect("stale alerts always pass");
            expected.push(reflex.clone());
            expected.push(consult.clone());
            expected.push(stale.clone());
        }
        assert_eq!(captured(&capture), expected);
    }

    #[tokio::test]
    async fn dedupe_state_is_not_touched_by_non_tier_alerts() {
        let (sink, capture) = dedupe_recorder();
        sink.send(&tier_alert(None, RiskTier::Yellow, T0))
            .await
            .expect("first Yellow forwards");
        sink.send(&stale_alert(70, T0 + 1))
            .await
            .expect("stale alert forwards");
        sink.send(&tier_alert(None, RiskTier::Yellow, T0 + 2))
            .await
            .expect("Yellow repeat is still suppressed");
        assert_eq!(captured(&capture).len(), 2);
    }

    #[tokio::test]
    async fn dedupe_is_per_market() {
        let (sink, capture) = dedupe_recorder();
        let mut other_market = tier_alert(None, RiskTier::Yellow, T0);
        other_market.market_id = Some(MarketId(16));
        sink.send(&tier_alert(None, RiskTier::Yellow, T0))
            .await
            .expect("market 32 forwards");
        sink.send(&other_market).await.expect("market 16 forwards");
        sink.send(&tier_alert(None, RiskTier::Yellow, T0 + 1))
            .await
            .expect("market 32 repeat is suppressed");
        assert_eq!(captured(&capture).len(), 2);
    }

    #[tokio::test]
    async fn dedupe_unkeyed_tier_change_fails_open() {
        // A `TierChange` without a market id cannot be keyed — always forward.
        let (sink, capture) = dedupe_recorder();
        let mut alert = tier_alert(None, RiskTier::Red, T0);
        alert.market_id = None;
        sink.send(&alert).await.expect("forwarded");
        sink.send(&alert).await.expect("forwarded again");
        assert_eq!(captured(&capture).len(), 2);
    }

    #[tokio::test]
    async fn recording_sink_captures_every_alert_in_order() {
        let sink = RecordingSink::new();
        let capture = Arc::clone(&sink.alerts);
        let first = tier_alert(None, RiskTier::Green, T0);
        let second = stale_alert(5, T0 + 1);
        let third = tier_alert(Some(RiskTier::Green), RiskTier::Yellow, T0 + 2);
        sink.send(&first).await.expect("captured");
        sink.send(&second).await.expect("captured");
        sink.send(&third).await.expect("captured");
        assert_eq!(captured(&capture), vec![first, second, third]);
    }

    #[tokio::test]
    async fn tracing_sink_send_returns_ok() {
        let alert = tier_alert(None, RiskTier::Yellow, T0);
        TracingSink
            .send(&alert)
            .await
            .expect("tracing delivery never fails");
    }

    #[tokio::test]
    async fn telegram_sink_constructs_and_debug_does_not_panic() {
        // No network: construction and Debug only (never `send` in tests).
        let sink = TelegramSink::new("123456:TESTTOKEN", 42);
        assert_eq!(sink.chat_id, teloxide::types::ChatId(42));
        let rendered = format!("{sink:?}");
        assert!(rendered.contains("TelegramSink"));
    }

    /// Inner sink that always fails — proves `DedupeSink` forwards errors.
    struct FailingSink;

    impl AlertSink for FailingSink {
        async fn send(&self, _alert: &Alert) -> Result<()> {
            Err(SentinelError::Internal("inner boom".to_string()))
        }
    }

    #[tokio::test]
    async fn dedupe_forwards_inner_errors() {
        let sink = DedupeSink::new(FailingSink);
        // Non-tier alerts go straight through to the inner sink…
        let stale = stale_alert(5, T0);
        assert!(matches!(
            sink.send(&stale).await,
            Err(SentinelError::Internal(message)) if message == "inner boom"
        ));
        // …and so does a forwarded tier change.
        assert!(
            sink.send(&tier_alert(None, RiskTier::Red, T0))
                .await
                .is_err()
        );
    }

    /// `io::Write` sink collecting formatted log lines for assertions.
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Thread-local TRACE+ capture (the guard must stay alive while asserting).
    fn capture_logs() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (buffer, guard)
    }

    /// Everything captured so far, as lossy UTF-8.
    fn captured_text(buffer: &LogBuffer) -> String {
        String::from_utf8_lossy(
            &buffer
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_owned()
    }

    /// Feed-stale alert with a distinct marker text for the queue tests.
    fn queue_alert(marker: u64) -> Alert {
        Alert {
            kind: AlertKind::FeedStale { secs: 1 },
            market_id: None,
            text: format!("queue-alert-{marker}"),
            at_ms: marker,
        }
    }

    #[test]
    fn alert_queue_drops_oldest_and_warns_on_overflow() {
        let (logs, _guard) = capture_logs();
        let queue = AlertQueue::new();
        for marker in 0..QUEUE_CAPACITY as u64 {
            queue.push(queue_alert(marker));
        }
        assert_eq!(queue.queued_len(), QUEUE_CAPACITY);

        queue.push(queue_alert(99));
        assert_eq!(
            queue.queued_len(),
            QUEUE_CAPACITY,
            "capacity holds under overflow"
        );
        // The oldest alert (marker 0) was dropped; the head is now marker 1.
        assert_eq!(
            queue.pop().expect("queue holds alerts").text,
            "queue-alert-1"
        );
        // The overflow is loud (SPEC-P16 §2: drop oldest + warn).
        let text = captured_text(&logs);
        assert!(
            text.contains("dropping oldest alert"),
            "missing overflow warn: {text}"
        );
        assert!(
            text.contains("capacity=32"),
            "missing capacity field: {text}"
        );
    }

    #[test]
    fn alert_queue_is_fifo_until_capacity() {
        let queue = AlertQueue::new();
        for marker in 0..5 {
            queue.push(queue_alert(marker));
        }
        for marker in 0..5 {
            assert_eq!(
                queue.pop().expect("queued alert").text,
                format!("queue-alert-{marker}")
            );
        }
        assert!(queue.pop().is_none(), "queue drains to empty");
    }

    #[tokio::test]
    async fn telegram_sink_debug_redacts_bot_token() {
        let sink = TelegramSink::new("123456:P16-INLINE-TOKEN", 42);
        let rendered = format!("{sink:?}");
        assert!(
            !rendered.contains("P16-INLINE-TOKEN"),
            "token leaked: {rendered}"
        );
        assert!(
            rendered.contains("REDACTED"),
            "redaction marker: {rendered}"
        );
        assert!(
            rendered.contains("TelegramSink"),
            "type name kept: {rendered}"
        );
    }
}
