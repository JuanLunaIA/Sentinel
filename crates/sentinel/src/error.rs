//! Error hierarchy for the Sentinel application.
//!
//! Library-level errors are typed with `thiserror` and carry per-subsystem
//! detail; binaries wrap them with `anyhow` + `.context(...)`. No error
//! variant ever embeds a secret value (see `config::SecretString`).

use thiserror::Error;

/// Convenience result alias used across the crate.
pub type Result<T, E = SentinelError> = std::result::Result<T, E>;

/// Top-level error type for Sentinel.
#[derive(Debug, Error)]
pub enum SentinelError {
    /// Failure originating in the Perpl integration layer.
    #[error("perpl: {0}")]
    Perpl(#[from] PerplError),

    /// Failure originating in the reasoning engine or a provider.
    #[error("brain: {0}")]
    Brain(#[from] BrainError),

    /// Failure originating in the Nansen x402 client.
    #[error("nansen: {0}")]
    Nansen(#[from] NansenError),

    /// An intent was refused by the policy gate.
    #[error("policy: {0}")]
    Policy(#[from] PolicyError),

    /// Failure in the audit journal (hash chain, persistence, anchoring).
    #[error("audit: {0}")]
    Audit(#[from] AuditError),

    /// Configuration failure (missing variable, invalid value, failed invariant).
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    /// An unexpected internal error (a bug — should never happen).
    #[error("internal: {0}")]
    Internal(String),
}

/// Perpl subsystem errors (`auth` / `ws` / `rest` / `order`).
#[derive(Debug, Error)]
pub enum PerplError {
    /// API-key authentication or request-signing failure.
    #[error("auth: {0}")]
    Auth(String),

    /// WebSocket connect / protocol / reconnect failure.
    #[error("ws: {0}")]
    Ws(String),

    /// REST request failure.
    #[error("rest: {0}")]
    Rest(String),

    /// Order submission or validation failure.
    #[error("order: {0}")]
    Order(String),
}

/// Reasoning-engine errors (provider chain).
#[derive(Debug, Error)]
pub enum BrainError {
    /// A provider returned an HTTP error.
    #[error("provider {provider} returned HTTP {status}")]
    ProviderHttp {
        /// Provider name (e.g. `qwen`, `kimi`).
        provider: String,
        /// HTTP status code.
        status: u16,
    },

    /// A provider response could not be parsed as the expected JSON.
    #[error("provider {provider} returned invalid JSON")]
    InvalidJson {
        /// Provider name.
        provider: String,
    },

    /// The decision JSON was structurally invalid.
    #[error("invalid decision: {detail}")]
    InvalidDecision {
        /// What was wrong with the decision document.
        detail: String,
    },

    /// Every provider in the chain failed.
    #[error("all providers failed; last error: {last}")]
    AllProvidersFailed {
        /// The last error encountered.
        last: String,
    },
}

/// Nansen x402 client errors.
#[derive(Debug, Error)]
pub enum NansenError {
    /// The 402 challenge could not be parsed.
    #[error("challenge parse: {0}")]
    Challenge(String),

    /// The payment payload could not be signed.
    #[error("sign: {0}")]
    Sign(String),

    /// The post-payment retry still failed.
    #[error("retry after payment failed (HTTP {status})")]
    Retry {
        /// HTTP status of the retry.
        status: u16,
    },

    /// The configured call budget was exhausted.
    #[error("budget exhausted: {spent} of {limit} calls used")]
    Budget {
        /// Calls spent in the current budget window.
        spent: u32,
        /// Configured limit for the window.
        limit: u32,
    },
}

/// Policy gate refusals.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// The intent was denied by a policy rule.
    #[error("rejected: {reason}")]
    Rejected {
        /// Why the intent was denied.
        reason: String,
    },

    /// The intent requires human approval before execution.
    #[error("needs approval: {reason}")]
    NeedsApproval {
        /// Why approval is required.
        reason: String,
    },
}

/// Audit journal errors.
#[derive(Debug, Error)]
pub enum AuditError {
    /// Free-form audit failure (persistence, chain verification, anchoring).
    #[error("{0}")]
    Other(String),
}

/// Configuration errors. Never echoes values (values may be secrets).
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A variable is missing or invalid.
    #[error("var {name}: {problem}")]
    Var {
        /// Environment variable name.
        name: String,
        /// What is wrong with it (no value echoed).
        problem: String,
    },
}
