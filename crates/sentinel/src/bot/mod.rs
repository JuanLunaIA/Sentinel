//! Telegram bot — dispatcher assembly (thin teloxide layer).
//!
//! Frozen by `SPEC-P11.md` §2. All logic lives in `handlers.rs` (pure);
//! this module only wires teloxide: auth filter, routing, callbacks,
//! long polling and graceful shutdown.
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

pub mod approvals;
pub mod commands;
pub mod execution;
pub mod format;
pub mod handlers;
pub mod policy_admin;

use tokio::sync::watch;

use crate::config::Config;
use crate::error::Result;

/// Run context passed to the dispatcher (built by main).
pub struct BotRunConfig {
    /// Bot token (placeholder ⇒ caller must NOT call [`run`]).
    pub token: String,
    /// Allowed user ids (empty ⇒ caller must NOT call [`run`]).
    pub allowed_user_ids: Vec<u64>,
    /// Approval chat (falls back to the allowed users when absent).
    pub approval_chat_id: Option<i64>,
    /// Full application configuration.
    pub cfg: Config,
}

/// Long-poll until `shutdown` flips.
///
/// # Errors
/// `SentinelError::Internal` for startup-level failures only (bad token
/// shape, dispatcher build).
pub async fn run(
    _run: BotRunConfig,
    _ctx: handlers::BotContext<
        crate::brain::providers::QwenProvider,
        crate::brain::providers::KimiProvider,
    >,
    _shutdown: watch::Receiver<bool>,
) -> Result<()> {
    todo!("P11 agent bot-frame: teloxide dispatcher")
}
