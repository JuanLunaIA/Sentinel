//! Policy overlay admin — whitelisted mutations of `data/policy.json`.
//!
//! Frozen by `SPEC-P11.md` §5: bounded keys, validated ranges, atomic write,
//! version-bumped [`SharedPolicy`] the pipeline reloads from.
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Default overlay path.
pub const POLICY_OVERLAY_PATH: &str = "data/policy.json";

/// The ONLY keys `/policy set` accepts.
pub const WHITELISTED_KEYS: &[&str] = &[
    "risk_soft_pct",
    "risk_warn_pct",
    "risk_hard_pct",
    "reflex_reduce_fraction",
    "reflex_orange_fraction",
    "reflex_cooldown_secs",
    "max_order_size_usd",
    "require_approval_above_usd",
    "max_daily_actions",
    "kill_switch",
];

/// Partial overrides; `None` = leave the config value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyOverlay {
    /// Soft distance threshold, percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_soft_pct: Option<Decimal>,
    /// Warn distance threshold, percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_warn_pct: Option<Decimal>,
    /// Hard distance threshold, percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_hard_pct: Option<Decimal>,
    /// Red reduce fraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reflex_reduce_fraction: Option<Decimal>,
    /// Orange reduce fraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reflex_orange_fraction: Option<Decimal>,
    /// Reflex cooldown, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reflex_cooldown_secs: Option<u64>,
    /// Per-action notional cap, USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_order_size_usd: Option<Decimal>,
    /// Approval threshold, USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_approval_above_usd: Option<Decimal>,
    /// Daily action cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_daily_actions: Option<u32>,
    /// Kill switch flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_switch: Option<bool>,
}

/// Load the overlay (missing file ⇒ default).
pub fn load(_path: &Path) -> PolicyOverlay {
    todo!("P11 agent bot-admin")
}

/// Validate `key`/`value` against the whitelist and return the UPDATED
/// overlay (ranges + resulting cross-field `hard < warn < soft`).
///
/// # Errors
/// Static description of the first violation.
pub fn validate_key_value(
    _key: &str,
    _value: &str,
    _base: &PolicyOverlay,
) -> Result<PolicyOverlay, String> {
    todo!("P11 agent bot-admin")
}

/// Atomic save (tmp file + rename; creates parent dirs).
///
/// # Errors
/// I/O description.
pub fn save(_path: &Path, _overlay: &PolicyOverlay) -> Result<(), String> {
    todo!("P11 agent bot-admin")
}

/// Shared, version-counted overlay the pipeline reloads from.
#[derive(Debug)]
pub struct SharedPolicy {
    /// Current overlay.
    pub inner: RwLock<PolicyOverlay>,
    /// Bumped on every successful mutation.
    pub version: AtomicU64,
    /// Backing file.
    pub path: PathBuf,
}

impl SharedPolicy {
    /// Load from `path`.
    pub fn load(_path: impl Into<PathBuf>) -> Self {
        todo!("P11 agent bot-admin")
    }

    /// Current overlay snapshot.
    pub fn snapshot(&self) -> PolicyOverlay {
        todo!("P11 agent bot-admin")
    }

    /// Monotonic version (bumped by successful `set_key`).
    pub fn version(&self) -> u64 {
        todo!("P11 agent bot-admin")
    }

    /// Kill switch as currently configured.
    pub fn kill_switch(&self) -> bool {
        todo!("P11 agent bot-admin")
    }

    /// Validate + persist + swap + bump.
    ///
    /// # Errors
    /// Validation or I/O description (file untouched on error).
    pub fn set_key(&self, _key: &str, _value: &str) -> Result<PolicyOverlay, String> {
        todo!("P11 agent bot-admin")
    }
}

/// Clone `base` and override exactly the whitelisted fields.
pub fn apply_to_config(_base: &Config, _overlay: &PolicyOverlay) -> Config {
    todo!("P11 agent bot-admin")
}
