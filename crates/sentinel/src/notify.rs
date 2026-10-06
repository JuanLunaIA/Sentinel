//! Alert delivery v1 — tier changes and reflex actions to tracing/Telegram.
//!
//! Frozen by `SPEC-P06.md` §6. Dedupe rule: one alert per `(market, tier)`
//! until the tier changes; a persistent `Red` re-alerts every 15 minutes.
//! All timestamps come from `Alert::at_ms` (logical in replay → deterministic).
//!
//! **Skeleton status (P06):** interfaces frozen; implemented by the P06 wave.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sentinel_core::types::{MarketId, RiskTier};
use serde::Serialize;

use crate::error::Result;

/// Persistent-Red re-alert interval, ms (SPEC-P06 §6).
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
    async fn send(&self, _alert: &Alert) -> Result<()> {
        todo!("P06 agent notify: tracing::info!(target: \"sentinel::alert\", ...)")
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
    async fn send(&self, _alert: &Alert) -> Result<()> {
        todo!("P06 agent notify: push to the buffer")
    }
}

/// Telegram delivery via `teloxide::Bot::sendMessage` (plain text v1).
#[derive(Debug)]
pub struct TelegramSink {
    /// Bot client.
    pub bot: teloxide::Bot,
    /// Target chat id.
    pub chat_id: teloxide::types::ChatId,
}

impl TelegramSink {
    /// Build from a token and a chat id.
    pub fn new(token: &str, chat_id: i64) -> Self {
        let _ = (token, chat_id);
        todo!("P06 agent notify")
    }
}

impl AlertSink for TelegramSink {
    async fn send(&self, _alert: &Alert) -> Result<()> {
        todo!("P06 agent notify: bot.send_message")
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
}

impl<S: AlertSink + Sync> AlertSink for DedupeSink<S> {
    async fn send(&self, _alert: &Alert) -> Result<()> {
        todo!("P06 agent notify: dedupe rule per SPEC-P06 §6")
    }
}
