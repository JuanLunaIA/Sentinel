//! Telegram bot — dispatcher assembly (thin teloxide layer).
//!
//! Frozen by `SPEC-P11.md` §2. All logic lives in `handlers.rs` (pure);
//! this module only wires teloxide: auth filter, routing, callbacks,
//! long polling and graceful shutdown.
//!
//! Auth is a strict allowlist on the sender id (messages and callbacks):
//! anyone else is logged and ignored silently. The bot token is never
//! logged, formatted into errors, or echoed anywhere.

pub mod approvals;
pub mod commands;
pub mod execution;
pub mod format;
pub mod handlers;
pub mod policy_admin;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tokio::sync::watch;

use crate::bot::handlers::{BotContext, Reply};
use crate::config::Config;
use crate::error::{Result, SentinelError};

/// Handler context the bot runs with (`main.rs` builds the concrete pair).
type Ctx = BotContext<crate::brain::providers::QwenProvider, crate::brain::providers::KimiProvider>;

/// Housekeeping cadence (approval expiry pass), seconds.
const HOUSEKEEPING_INTERVAL_SECS: u64 = 30;

/// Short help sent when no command matches (valid MarkdownV2).
const UNKNOWN_COMMAND_REPLY: &str = "*Sentinel*\nUnknown command — send /help for the command list";

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
    bot_cfg: BotRunConfig,
    ctx: handlers::BotContext<
        crate::brain::providers::QwenProvider,
        crate::brain::providers::KimiProvider,
    >,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let BotRunConfig {
        token,
        allowed_user_ids,
        approval_chat_id,
        ..
    } = bot_cfg;

    validate_token(&token)?;
    if allowed_user_ids.is_empty() {
        return Err(SentinelError::Internal(
            "telegram allowlist is empty; refusing to start the bot".to_string(),
        ));
    }
    if *shutdown.borrow() {
        tracing::info!("shutdown already requested; telegram bot not started");
        return Ok(());
    }

    let delivery_chat = approval_chat_id
        .map(ChatId)
        .or_else(|| allowed_user_ids.first().map(|id| ChatId(*id as i64)));
    let allowed = Arc::new(allowed_user_ids);
    let bot = Bot::new(token); // moved in; never logged
    let ctx = Arc::new(ctx);

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback_query));

    let mut dispatcher = catch_unwind(AssertUnwindSafe(|| {
        Dispatcher::builder(bot.clone(), handler)
            .dependencies(dptree::deps![ctx.clone(), allowed.clone()])
            .build()
    }))
    .map_err(|_| {
        SentinelError::Internal(
            "telegram dispatcher handler failed its dependency type-check".to_string(),
        )
    })?;

    let shutdown_token = dispatcher.shutdown_token();
    let mut dispatch_task = tokio::spawn(async move { dispatcher.dispatch().await });

    tracing::info!(
        allowed_users = allowed.len(),
        approval_chat = ?delivery_chat,
        "telegram bot long polling started"
    );

    let housekeeping_task =
        spawn_housekeeping(bot, Arc::clone(&ctx), delivery_chat, shutdown.clone());

    tokio::select! {
        joined = &mut dispatch_task => {
            housekeeping_task.abort();
            return dispatcher_outcome(joined);
        }
        _ = wait_for_stop(&mut shutdown) => {}
    }

    if let Err(err) = shutdown_token.shutdown() {
        tracing::warn!(error = %err, "telegram dispatcher was not running at shutdown");
    }
    match dispatch_task.await {
        Ok(()) => tracing::info!("telegram dispatcher stopped"),
        Err(err) => tracing::warn!(error = %err, "telegram dispatcher task failed during shutdown"),
    }
    housekeeping_task.abort();
    Ok(())
}

/// Map the dispatcher task's join result onto the run result.
fn dispatcher_outcome(joined: std::result::Result<(), tokio::task::JoinError>) -> Result<()> {
    match joined {
        Ok(()) => {
            tracing::info!("telegram dispatcher exited");
            Ok(())
        }
        Err(err) if err.is_panic() => Err(SentinelError::Internal(format!(
            "telegram dispatcher panicked at startup: {err}"
        ))),
        Err(err) => Err(SentinelError::Internal(format!(
            "telegram dispatcher task failed: {err}"
        ))),
    }
}

/// Handle one text message: commands go to the pure router, other text to
/// the typed-confirmation handler, with a short help fallback.
async fn on_message(
    bot: Bot,
    msg: Message,
    ctx: Arc<Ctx>,
    allowed: Arc<Vec<u64>>,
) -> ResponseResult<()> {
    // `msg.from` field access (the `from()` getter is deprecated in 0.17).
    let Some(user) = msg.from.as_ref() else {
        return Ok(()); // channel/anon posts carry no user id
    };
    if !allowed.contains(&user.id.0) {
        tracing::warn!(
            user_id = user.id.0,
            "ignoring message from unauthorized user"
        );
        return Ok(());
    }
    let Some(text) = msg.text() else {
        return Ok(()); // non-text updates are not part of the surface
    };

    let now = unix_ms();
    let reply = match commands::parse(text) {
        Some(cmd) => handlers::handle(cmd, ctx.as_ref(), now).await,
        None => handlers::handle_text(text, ctx.as_ref(), now)
            .await
            .unwrap_or_else(|| Reply::text(UNKNOWN_COMMAND_REPLY)),
    };
    send_reply(&bot, msg.chat.id, &reply).await
}

/// Handle one inline-button callback (`approve:<id>` / `deny:<id>`).
async fn on_callback_query(
    bot: Bot,
    q: CallbackQuery,
    ctx: Arc<Ctx>,
    allowed: Arc<Vec<u64>>,
) -> ResponseResult<()> {
    if !allowed.contains(&q.from.id.0) {
        tracing::warn!(
            user_id = q.from.id.0,
            "ignoring callback from unauthorized user"
        );
        return Ok(());
    }
    let Some((kind, id)) = q.data.as_deref().and_then(|data| data.split_once(':')) else {
        tracing::debug!("ignoring callback with unrecognized data");
        return Ok(());
    };
    let approve = match kind {
        "approve" => true,
        "deny" => false,
        _ => {
            tracing::debug!("ignoring callback with unknown action");
            return Ok(());
        }
    };

    // Acknowledge first so the client spinner clears even when the handler
    // (which may execute an approved order) takes a moment.
    bot.answer_callback_query(q.id.clone()).await?;
    let reply = handlers::handle_approval_callback(ctx.as_ref(), id, approve, unix_ms()).await;

    if let Some(message) = q.message.as_ref() {
        let chat = message.chat().id;
        let message_id = message.id();
        let markup = inline_keyboard(&reply.keyboard).unwrap_or_default();
        if let Err(err) = bot
            .edit_message_text(chat, message_id, reply.text.as_str())
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(markup)
            .await
        {
            tracing::warn!(error = %err, "failed to update the approval message");
        }
    }
    Ok(())
}

/// Send one handler reply as a MarkdownV2 message with its keyboard.
async fn send_reply(bot: &Bot, chat: ChatId, reply: &Reply) -> ResponseResult<()> {
    let mut request = bot
        .send_message(chat, reply.text.as_str())
        .parse_mode(ParseMode::MarkdownV2);
    if let Some(keyboard) = inline_keyboard(&reply.keyboard) {
        request = request.reply_markup(keyboard);
    }
    request.await?;
    Ok(())
}

/// Convert handler keyboard rows into a Telegram inline keyboard.
fn inline_keyboard(rows: &Option<Vec<Vec<(String, String)>>>) -> Option<InlineKeyboardMarkup> {
    rows.as_ref().map(|rows| {
        InlineKeyboardMarkup::new(
            rows.iter()
                .map(|row| {
                    row.iter()
                        .map(|(label, data)| {
                            InlineKeyboardButton::callback(label.clone(), data.clone())
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        )
    })
}

/// Spawn the 30 s housekeeping loop (approval expiry) that delivers any
/// replies to the approval chat (fallback: first allowed user).
fn spawn_housekeeping(
    bot: Bot,
    ctx: Arc<Ctx>,
    delivery_chat: Option<ChatId>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(HOUSEKEEPING_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let replies = handlers::housekeeping(ctx.as_ref(), unix_ms()).await;
                    match delivery_chat {
                        Some(chat) => {
                            for reply in &replies {
                                if let Err(err) = send_reply(&bot, chat, reply).await {
                                    tracing::warn!(error = %err, "housekeeping delivery failed");
                                }
                            }
                        }
                        None => tracing::warn!(
                            count = replies.len(),
                            "housekeeping replies have no delivery chat"
                        ),
                    }
                }
                _ = wait_for_stop(&mut shutdown) => break,
            }
        }
    })
}

/// Resolve when the shutdown watch flips to `true` (or the sender goes away).
async fn wait_for_stop(rx: &mut watch::Receiver<bool>) {
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        if *rx.borrow() {
            return;
        }
    }
}

/// Wall-clock milliseconds since the Unix epoch (0 before 1970).
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Validate the token shape before handing it to teloxide.
///
/// The token value is never logged, formatted into errors, or otherwise
/// echoed; failures carry only a generic reason.
fn validate_token(token: &str) -> Result<()> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(SentinelError::Internal(
            "telegram token is empty".to_string(),
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    let is_placeholder = lowered.contains("replace-me")
        || lowered.contains("replace_me")
        || lowered.contains("placeholder")
        || trimmed.starts_with('<');
    if is_placeholder {
        return Err(SentinelError::Internal(
            "telegram token is a placeholder; refusing to start the bot".to_string(),
        ));
    }
    match trimmed.split_once(':') {
        Some((id, secret))
            if !id.is_empty() && !secret.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(())
        }
        _ => Err(SentinelError::Internal(
            "telegram token has an invalid shape (expected <digits>:<secret>)".to_string(),
        )),
    }
}
