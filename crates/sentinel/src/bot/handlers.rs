//! Pure command handlers — no teloxide types (`SPEC-P11.md` §2).
//!
//! Every handler is deterministic given `now_ms`; the teloxide layer only
//! converts [`Reply`] into bot API calls.
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use sentinel_core::audit::AuditJournal;

use crate::bot::approvals::ApprovalQueue;
use crate::bot::commands::Command;
use crate::bot::execution::HumanExecutor;
use crate::bot::policy_admin::SharedPolicy;
use crate::brain::engine::StrategyEngine;
use crate::brain::providers::Provider;
use crate::config::Config;
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

/// Route one command to its reply.
pub async fn handle<P: Provider + Sync, F: Provider + Sync>(
    _cmd: Command,
    _ctx: &BotContext<P, F>,
    _now_ms: u64,
) -> Reply {
    todo!("P11 agent bot-handlers")
}

/// Non-command text (the typed `PAUSE` confirmation).
pub async fn handle_text<P: Provider + Sync, F: Provider + Sync>(
    _text: &str,
    _ctx: &BotContext<P, F>,
    _now_ms: u64,
) -> Option<Reply> {
    todo!("P11 agent bot-handlers")
}

/// Approve/Deny inline-button callback.
pub async fn handle_approval_callback<P: Provider + Sync, F: Provider + Sync>(
    _ctx: &BotContext<P, F>,
    _id: &str,
    _approve: bool,
    _now_ms: u64,
) -> Reply {
    todo!("P11 agent bot-handlers")
}

/// Periodic expiry pass; returns notifications to deliver.
pub async fn housekeeping<P: Provider + Sync, F: Provider + Sync>(
    _ctx: &BotContext<P, F>,
    _now_ms: u64,
) -> Vec<Reply> {
    todo!("P11 agent bot-handlers")
}
