//! Policy overlay admin — whitelisted mutations of `data/policy.json`.
//!
//! Frozen by `SPEC-P11.md` §5: bounded keys, validated ranges, atomic write,
//! version-bumped [`SharedPolicy`] the pipeline reloads from.
//!
//! **Status (P11):** implemented by agent `bot-admin`.
//!
//! The cross-field threshold check runs on the **resulting** overlay: a
//! threshold the overlay does not pin falls back to the base overlay's
//! value, then to the config defaults (`soft 25 / warn 15 / hard 8`), so a
//! single `/policy set` can never leave an unsatisfiable `hard < warn <
//! soft` combination behind.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Load the overlay (missing file ⇒ default; unreadable or malformed JSON ⇒
/// warned default — the daemon must boot even with a corrupt overlay).
pub fn load(path: &Path) -> PolicyOverlay {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<PolicyOverlay>(&raw) {
            Ok(overlay) => overlay,
            Err(err) => {
                eprintln!(
                    "policy overlay {} is malformed ({err}); keeping defaults",
                    path.display()
                );
                PolicyOverlay::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PolicyOverlay::default(),
        Err(err) => {
            eprintln!(
                "policy overlay {} is unreadable ({err}); keeping defaults",
                path.display()
            );
            PolicyOverlay::default()
        }
    }
}

/// Validate `key`/`value` with the compiled-in config defaults
/// (soft 25 / warn 15 / hard 8). See [`validate_key_value_with_defaults`].
///
/// # Errors
/// Static description of the first violation.
pub fn validate_key_value(
    key: &str,
    value: &str,
    base: &PolicyOverlay,
) -> Result<PolicyOverlay, String> {
    validate_key_value_with_defaults(
        key,
        value,
        base,
        (default_soft_pct(), default_warn_pct(), default_hard_pct()),
    )
}

/// Validate `key`/`value` against the whitelist and return the UPDATED
/// overlay (ranges + resulting cross-field `hard < warn < soft`).
///
/// Keys are matched exactly against [`WHITELISTED_KEYS`] (case-sensitive);
/// the value is trimmed, and `kill_switch` accepts `true`/`false`
/// case-insensitively. Missing thresholds are completed from `defaults`
/// (the operator's effective base via [`SharedPolicy::load_with_defaults`],
/// so a non-default `RISK_*_PCT` environment stays consistent).
///
/// # Errors
/// Static description of the first violation.
pub fn validate_key_value_with_defaults(
    key: &str,
    value: &str,
    base: &PolicyOverlay,
    defaults: (Decimal, Decimal, Decimal),
) -> Result<PolicyOverlay, String> {
    if !WHITELISTED_KEYS.contains(&key) {
        return Err(format!("unknown policy key: {key}"));
    }
    let raw = value.trim();
    let mut updated = base.clone();
    match key {
        "risk_soft_pct" => updated.risk_soft_pct = Some(parse_positive_decimal(key, raw)?),
        "risk_warn_pct" => updated.risk_warn_pct = Some(parse_positive_decimal(key, raw)?),
        "risk_hard_pct" => updated.risk_hard_pct = Some(parse_positive_decimal(key, raw)?),
        "reflex_reduce_fraction" => {
            updated.reflex_reduce_fraction = Some(parse_fraction(key, raw)?);
        }
        "reflex_orange_fraction" => {
            updated.reflex_orange_fraction = Some(parse_fraction(key, raw)?);
        }
        "reflex_cooldown_secs" => {
            updated.reflex_cooldown_secs = Some(parse_positive_u64(key, raw)?)
        }
        "max_order_size_usd" => {
            updated.max_order_size_usd = Some(parse_positive_decimal(key, raw)?);
        }
        "require_approval_above_usd" => {
            updated.require_approval_above_usd = Some(parse_positive_decimal(key, raw)?);
        }
        "max_daily_actions" => updated.max_daily_actions = Some(parse_positive_u32(key, raw)?),
        "kill_switch" => updated.kill_switch = Some(parse_bool_value(raw)?),
        // `WHITELISTED_KEYS` is exhaustive above; fail closed, never panic.
        _ => return Err(format!("unknown policy key: {key}")),
    }

    let effective_soft = updated
        .risk_soft_pct
        .or(base.risk_soft_pct)
        .unwrap_or(defaults.0);
    let effective_warn = updated
        .risk_warn_pct
        .or(base.risk_warn_pct)
        .unwrap_or(defaults.1);
    let effective_hard = updated
        .risk_hard_pct
        .or(base.risk_hard_pct)
        .unwrap_or(defaults.2);
    if !(effective_hard < effective_warn && effective_warn < effective_soft) {
        return Err(format!(
            "risk thresholds must satisfy hard < warn < soft \
             (effective: soft {effective_soft}, warn {effective_warn}, hard {effective_hard})"
        ));
    }
    Ok(updated)
}

/// Atomic save (tmp file + rename; creates parent dirs).
///
/// # Errors
/// I/O description.
pub fn save(path: &Path, overlay: &PolicyOverlay) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create policy dir {}: {err}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(overlay)
        .map_err(|err| format!("encode policy overlay: {err}"))?;
    let tmp = tmp_path(path);
    std::fs::write(&tmp, format!("{body}\n"))
        .map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {err}", tmp.display(), path.display())
    })?;
    Ok(())
}

/// Shared, version-counted overlay the pipeline reloads from.
#[derive(Debug)]
pub struct SharedPolicy {
    /// Current overlay.
    pub inner: RwLock<PolicyOverlay>,
    /// Bumped on every successful mutation (starts at 0 per load).
    pub version: AtomicU64,
    /// Backing file.
    pub path: PathBuf,
    /// Effective base thresholds used to complete missing pct keys.
    pub defaults: (Decimal, Decimal, Decimal),
}

impl SharedPolicy {
    /// Load from `path` (missing/malformed ⇒ defaults, as [`load`]) with the
    /// compiled-in threshold defaults (25/15/8).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        Self::load_with_defaults(
            path,
            (default_soft_pct(), default_warn_pct(), default_hard_pct()),
        )
    }

    /// Load with the operator's effective base thresholds (from `Config`), so
    /// cross-field validation matches the values the pipeline will run.
    pub fn load_with_defaults(
        path: impl Into<PathBuf>,
        defaults: (Decimal, Decimal, Decimal),
    ) -> Self {
        let path = path.into();
        let overlay = load(&path);
        Self {
            inner: RwLock::new(overlay),
            version: AtomicU64::new(0),
            path,
            defaults,
        }
    }

    /// Current overlay snapshot.
    pub fn snapshot(&self) -> PolicyOverlay {
        match self.inner.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Monotonic version (bumped by successful `set_key`).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Kill switch as currently configured.
    pub fn kill_switch(&self) -> bool {
        self.snapshot().kill_switch.unwrap_or(false)
    }

    /// Validate + persist + swap + bump.
    ///
    /// # Errors
    /// Validation or I/O description (state, file and version untouched on
    /// error).
    pub fn set_key(&self, key: &str, value: &str) -> Result<PolicyOverlay, String> {
        let mut guard = self.lock_write();
        let updated = validate_key_value_with_defaults(key, value, &guard, self.defaults)?;
        save(&self.path, &updated)?;
        *guard = updated;
        let _ = self.version.fetch_add(1, Ordering::SeqCst);
        Ok(guard.clone())
    }

    /// Write lock helper: a poisoned lock still yields the overlay (the
    /// overlay has no cross-entry invariant to defend).
    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, PolicyOverlay> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Clone `base` and override exactly the whitelisted fields.
///
/// `kill_switch` is not part of [`Config`] — consumers read it through
/// [`SharedPolicy::kill_switch`].
pub fn apply_to_config(base: &Config, overlay: &PolicyOverlay) -> Config {
    let mut cfg = base.clone();
    if let Some(value) = overlay.risk_soft_pct {
        cfg.risk.soft_pct = value;
    }
    if let Some(value) = overlay.risk_warn_pct {
        cfg.risk.warn_pct = value;
    }
    if let Some(value) = overlay.risk_hard_pct {
        cfg.risk.hard_pct = value;
    }
    if let Some(value) = overlay.reflex_reduce_fraction {
        cfg.risk.reflex_reduce_fraction = value;
    }
    if let Some(value) = overlay.reflex_orange_fraction {
        cfg.risk.reflex_orange_fraction = value;
    }
    if let Some(value) = overlay.reflex_cooldown_secs {
        cfg.risk.reflex_cooldown_secs = value;
    }
    if let Some(value) = overlay.max_order_size_usd {
        cfg.risk.max_order_size_usd = value;
    }
    if let Some(value) = overlay.require_approval_above_usd {
        cfg.risk.require_approval_above_usd = value;
    }
    if let Some(value) = overlay.max_daily_actions {
        cfg.risk.max_daily_actions = value;
    }
    cfg
}

/// `<path>.tmp` sibling used by [`save`] until the renaming swap.
fn tmp_path(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(".tmp");
    PathBuf::from(raw)
}

/// Fallback soft threshold (percent) when neither overlay pins one.
fn default_soft_pct() -> Decimal {
    Decimal::new(25, 0)
}

/// Fallback warn threshold (percent) when neither overlay pins one.
fn default_warn_pct() -> Decimal {
    Decimal::new(15, 0)
}

/// Fallback hard threshold (percent) when neither overlay pins one.
fn default_hard_pct() -> Decimal {
    Decimal::new(8, 0)
}

/// Parse a decimal, naming `key` in the error.
fn parse_decimal(key: &str, raw: &str) -> Result<Decimal, String> {
    raw.parse::<Decimal>()
        .map_err(|_| format!("{key}: \"{raw}\" is not a number"))
}

/// Decimal strictly above zero (percent thresholds and USD caps).
fn parse_positive_decimal(key: &str, raw: &str) -> Result<Decimal, String> {
    let parsed = parse_decimal(key, raw)?;
    if parsed <= Decimal::ZERO {
        return Err(format!("{key}: must be > 0 (got {parsed})"));
    }
    Ok(parsed)
}

/// Decimal within `(0, 1]` (reflex reduce fractions).
fn parse_fraction(key: &str, raw: &str) -> Result<Decimal, String> {
    let parsed = parse_decimal(key, raw)?;
    if parsed <= Decimal::ZERO || parsed > Decimal::ONE {
        return Err(format!("{key}: must be within (0, 1] (got {parsed})"));
    }
    Ok(parsed)
}

/// Integer strictly above zero.
fn parse_positive_u64(key: &str, raw: &str) -> Result<u64, String> {
    let parsed: u64 = raw
        .parse()
        .map_err(|_| format!("{key}: \"{raw}\" is not a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{key}: must be > 0"));
    }
    Ok(parsed)
}

/// 32-bit integer strictly above zero (daily action cap).
fn parse_positive_u32(key: &str, raw: &str) -> Result<u32, String> {
    let parsed: u32 = raw
        .parse()
        .map_err(|_| format!("{key}: \"{raw}\" is not a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{key}: must be > 0"));
    }
    Ok(parsed)
}

/// Boolean accepting `true`/`false` case-insensitively.
fn parse_bool_value(raw: &str) -> Result<bool, String> {
    if raw.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if raw.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(format!(
            "kill_switch: expected \"true\" or \"false\" (got \"{raw}\")"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;

    /// Minimal valid fixture config (mirrors the workspace smoke fixture).
    fn test_config() -> Config {
        let mut vars = HashMap::new();
        for (key, value) in [
            ("PERPL_ENV", "testnet"),
            ("PERPL_API_KEY", "test-perpl-key"),
            (
                "PERPL_API_KEY_SECRET",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            ("QWEN_API_KEY", "test-qwen-key"),
            ("KIMI_API_KEY", "test-kimi-key"),
            ("NANSEN_PAYER_KEY", "0x00"),
            ("TELOXIDE_TOKEN", "123456:test-token"),
            ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
            ("EXECUTION_MODE", "DRY_RUN"),
        ] {
            vars.insert(key.to_string(), value.to_string());
        }
        Config::from_vars(vars).expect("fixture config must load")
    }

    /// Field names that differ between `before` and `after`.
    fn changed_fields(before: &PolicyOverlay, after: &PolicyOverlay) -> Vec<&'static str> {
        [
            ("risk_soft_pct", before.risk_soft_pct != after.risk_soft_pct),
            ("risk_warn_pct", before.risk_warn_pct != after.risk_warn_pct),
            ("risk_hard_pct", before.risk_hard_pct != after.risk_hard_pct),
            (
                "reflex_reduce_fraction",
                before.reflex_reduce_fraction != after.reflex_reduce_fraction,
            ),
            (
                "reflex_orange_fraction",
                before.reflex_orange_fraction != after.reflex_orange_fraction,
            ),
            (
                "reflex_cooldown_secs",
                before.reflex_cooldown_secs != after.reflex_cooldown_secs,
            ),
            (
                "max_order_size_usd",
                before.max_order_size_usd != after.max_order_size_usd,
            ),
            (
                "require_approval_above_usd",
                before.require_approval_above_usd != after.require_approval_above_usd,
            ),
            (
                "max_daily_actions",
                before.max_daily_actions != after.max_daily_actions,
            ),
            ("kill_switch", before.kill_switch != after.kill_switch),
        ]
        .into_iter()
        .filter_map(|(name, different)| different.then_some(name))
        .collect()
    }

    /// Validation case: `(key, value, probe on the resulting overlay)`.
    type KeyCase = (&'static str, &'static str, fn(&PolicyOverlay) -> bool);

    #[test]
    fn validate_accepts_every_whitelisted_key_and_sets_only_that_field() {
        let base = PolicyOverlay::default();
        let cases: [KeyCase; 10] = [
            ("risk_soft_pct", "40", |o| {
                o.risk_soft_pct == Some(Decimal::new(40, 0))
            }),
            ("risk_warn_pct", "20", |o| {
                o.risk_warn_pct == Some(Decimal::new(20, 0))
            }),
            ("risk_hard_pct", "10", |o| {
                o.risk_hard_pct == Some(Decimal::new(10, 0))
            }),
            ("reflex_reduce_fraction", "0.5", |o| {
                o.reflex_reduce_fraction == Some(Decimal::new(5, 1))
            }),
            ("reflex_orange_fraction", "0.25", |o| {
                o.reflex_orange_fraction == Some(Decimal::new(25, 2))
            }),
            ("reflex_cooldown_secs", "120", |o| {
                o.reflex_cooldown_secs == Some(120)
            }),
            ("max_order_size_usd", "1000", |o| {
                o.max_order_size_usd == Some(Decimal::new(1000, 0))
            }),
            ("require_approval_above_usd", "500", |o| {
                o.require_approval_above_usd == Some(Decimal::new(500, 0))
            }),
            ("max_daily_actions", "5", |o| o.max_daily_actions == Some(5)),
            ("kill_switch", "true", |o| o.kill_switch == Some(true)),
        ];
        for (key, value, probe) in cases {
            let updated = validate_key_value(key, value, &base)
                .unwrap_or_else(|err| panic!("{key} must validate: {err}"));
            assert!(probe(&updated), "{key} must set its field");
            assert_eq!(
                changed_fields(&base, &updated),
                vec![key],
                "{key} must be the only change"
            );
        }
    }

    #[test]
    fn validate_rejects_unknown_key_naming_it() {
        let base = PolicyOverlay::default();
        for key in ["risk_soft", "MAX_DAILY_ACTIONS", "kill", "", "../policy"] {
            let err =
                validate_key_value(key, "1", &base).expect_err("unknown key must be rejected");
            assert!(err.contains(key), "error must name the key: {err}");
        }
    }

    #[test]
    fn validate_rejects_bad_types_and_strict_bools() {
        let base = PolicyOverlay::default();
        for (key, value) in [
            ("risk_soft_pct", "abc"),
            ("risk_soft_pct", ""),
            ("reflex_reduce_fraction", "half"),
            ("reflex_cooldown_secs", "later"),
            ("reflex_cooldown_secs", "1.5"),
            ("reflex_cooldown_secs", "-1"),
            ("max_order_size_usd", "12,5"),
            ("max_daily_actions", "3.5"),
            ("max_daily_actions", "-3"),
            ("max_daily_actions", "99999999999999999999"),
            ("kill_switch", "maybe"),
            ("kill_switch", "1"),
            ("kill_switch", "yes"),
        ] {
            assert!(
                validate_key_value(key, value, &base).is_err(),
                "{key} = {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn validate_enforces_ranges() {
        let base = PolicyOverlay::default();
        // Percent keys: > 0.
        assert!(validate_key_value("risk_soft_pct", "0", &base).is_err());
        assert!(validate_key_value("risk_warn_pct", "0", &base).is_err());
        assert!(validate_key_value("risk_hard_pct", "0", &base).is_err());
        assert!(validate_key_value("risk_hard_pct", "-1", &base).is_err());
        // Fractions: (0, 1] — the 1.0/1.01 boundary.
        assert!(validate_key_value("reflex_reduce_fraction", "0", &base).is_err());
        assert!(validate_key_value("reflex_reduce_fraction", "-0.5", &base).is_err());
        assert!(validate_key_value("reflex_reduce_fraction", "1.01", &base).is_err());
        assert!(validate_key_value("reflex_reduce_fraction", "1.0", &base).is_ok());
        assert!(validate_key_value("reflex_reduce_fraction", "1", &base).is_ok());
        assert!(validate_key_value("reflex_orange_fraction", "1.0001", &base).is_err());
        assert!(validate_key_value("reflex_orange_fraction", "0.0001", &base).is_ok());
        // Cooldown: > 0.
        assert!(validate_key_value("reflex_cooldown_secs", "0", &base).is_err());
        assert!(validate_key_value("reflex_cooldown_secs", "1", &base).is_ok());
        // Caps: > 0.
        assert!(validate_key_value("max_order_size_usd", "0", &base).is_err());
        assert!(validate_key_value("max_order_size_usd", "-5", &base).is_err());
        assert!(validate_key_value("max_order_size_usd", "0.01", &base).is_ok());
        assert!(validate_key_value("require_approval_above_usd", "0", &base).is_err());
        assert!(validate_key_value("require_approval_above_usd", "-1", &base).is_err());
        // Actions: > 0.
        assert!(validate_key_value("max_daily_actions", "0", &base).is_err());
        assert!(validate_key_value("max_daily_actions", "1", &base).is_ok());
        // Huge / adversarial magnitudes fail closed, without panicking.
        assert!(validate_key_value("max_order_size_usd", "1e999999", &base).is_err());
        assert!(validate_key_value("risk_soft_pct", &"9".repeat(500), &base).is_err());
    }

    #[test]
    fn validate_rejects_cross_field_violations() {
        // Spec example: soft 25 set, then `hard 30` must be rejected
        // (effective warn falls back to the default 15).
        let base = PolicyOverlay {
            risk_soft_pct: Some(Decimal::new(25, 0)),
            ..Default::default()
        };
        let err = validate_key_value("risk_hard_pct", "30", &base)
            .expect_err("hard 30 with soft 25 must be rejected");
        assert!(err.contains("hard < warn < soft"), "{err}");

        // Warn must stay strictly between hard (default 8) and soft (25);
        // equality on either side is a violation.
        assert!(validate_key_value("risk_warn_pct", "25", &base).is_err());
        assert!(validate_key_value("risk_warn_pct", "8", &base).is_err());
        assert!(validate_key_value("risk_warn_pct", "30", &base).is_err());
        assert!(validate_key_value("risk_warn_pct", "24.999", &base).is_ok());

        let strict = PolicyOverlay {
            risk_soft_pct: Some(Decimal::new(25, 0)),
            risk_warn_pct: Some(Decimal::new(15, 0)),
            ..Default::default()
        };
        assert!(validate_key_value("risk_hard_pct", "15", &strict).is_err());
        assert!(validate_key_value("risk_hard_pct", "14.999", &strict).is_ok());

        // Failures never mutate the base overlay.
        assert_eq!(base.risk_hard_pct, None);
        assert_eq!(strict.risk_hard_pct, None);
    }

    #[test]
    fn load_missing_or_malformed_falls_back_to_default() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("policy.json");
        assert_eq!(load(&path), PolicyOverlay::default());
        assert!(!path.exists(), "load never creates the file");

        std::fs::write(&path, "{ not json").expect("write garbage");
        assert_eq!(load(&path), PolicyOverlay::default());

        std::fs::write(&path, "\"a string, not an overlay\"").expect("write wrong shape");
        assert_eq!(load(&path), PolicyOverlay::default());
    }

    #[test]
    fn load_round_trips_a_saved_overlay() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("policy.json");
        let overlay = PolicyOverlay {
            risk_soft_pct: Some(Decimal::new(30, 0)),
            risk_warn_pct: Some(Decimal::new(20, 0)),
            risk_hard_pct: Some(Decimal::new(10, 0)),
            reflex_reduce_fraction: Some(Decimal::new(75, 2)),
            reflex_orange_fraction: Some(Decimal::new(5, 1)),
            reflex_cooldown_secs: Some(90),
            max_order_size_usd: Some(Decimal::new(1234, 0)),
            require_approval_above_usd: Some(Decimal::new(500, 0)),
            max_daily_actions: Some(7),
            kill_switch: Some(true),
        };
        save(&path, &overlay).expect("save must succeed");
        assert_eq!(load(&path), overlay, "reload returns the same overlay");
    }

    #[test]
    fn save_is_atomic_and_creates_dirs() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested/deep/policy.json");
        let overlay = PolicyOverlay {
            max_daily_actions: Some(7),
            kill_switch: Some(true),
            ..Default::default()
        };
        save(&path, &overlay).expect("save must create dirs and write");
        assert!(path.exists());
        assert!(
            !dir.path().join("nested/deep/policy.json.tmp").exists(),
            "tmp file is renamed away"
        );

        // Overwriting is atomic too and leaves no tmp behind.
        let updated = PolicyOverlay {
            max_daily_actions: Some(9),
            ..Default::default()
        };
        save(&path, &updated).expect("overwrite must succeed");
        assert_eq!(load(&path), updated);
        assert!(!dir.path().join("nested/deep/policy.json.tmp").exists());
    }

    #[test]
    fn shared_policy_persists_bumps_on_success_only_and_leaves_file_untouched_on_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("policy.json");
        let policy = SharedPolicy::load(&path);
        assert_eq!(policy.version(), 0, "fresh load starts at version 0");
        assert_eq!(policy.snapshot(), PolicyOverlay::default());
        assert!(!policy.kill_switch());

        // Validation failure: version unchanged, no file created.
        let err = policy
            .set_key("risk_soft", "40")
            .expect_err("unknown key must fail");
        assert!(err.contains("risk_soft"), "{err}");
        assert_eq!(policy.version(), 0);
        assert!(!path.exists(), "no file on validation failure");

        // First success: version bumps, snapshot swaps, file persists.
        let first = policy.set_key("risk_soft_pct", "25").expect("soft 25");
        assert_eq!(first.risk_soft_pct, Some(Decimal::new(25, 0)));
        assert_eq!(policy.version(), 1);
        assert_eq!(policy.snapshot(), first);
        assert_eq!(load(&path), first);
        assert!(!dir.path().join("policy.json.tmp").exists());

        // Cross-field failure: version and file untouched (SPEC-P11 §10).
        let raw_before = std::fs::read_to_string(&path).expect("file must exist");
        let err = policy
            .set_key("risk_hard_pct", "30")
            .expect_err("hard 30 with soft 25 must fail");
        assert!(err.contains("hard < warn < soft"), "{err}");
        assert_eq!(policy.version(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).expect("file must exist"),
            raw_before,
            "file untouched on cross-field violation"
        );

        // Range failure: untouched as well.
        assert!(policy.set_key("max_daily_actions", "0").is_err());
        assert_eq!(policy.version(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).expect("file must exist"),
            raw_before
        );

        // Second success accumulates over the persisted overlay.
        let second = policy
            .set_key("max_daily_actions", "7")
            .expect("valid set must succeed");
        assert_eq!(second.risk_soft_pct, Some(Decimal::new(25, 0)));
        assert_eq!(second.max_daily_actions, Some(7));
        assert_eq!(policy.version(), 2);
        assert_eq!(policy.snapshot(), second);
        assert_eq!(load(&path), second, "file holds the full overlay");

        // A fresh load of the same file returns the same overlay.
        let reloaded = SharedPolicy::load(&path);
        assert_eq!(reloaded.snapshot(), second);
        assert_eq!(reloaded.version(), 0, "each load restarts its own version");
    }

    #[test]
    fn shared_policy_kill_switch_accessor_reads_overlay() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("policy.json");
        let policy = SharedPolicy::load(&path);
        assert!(!policy.kill_switch(), "default off");

        policy
            .set_key("kill_switch", "TRUE")
            .expect("case-insensitive true");
        assert!(policy.kill_switch());

        let reloaded = SharedPolicy::load(&path);
        assert!(reloaded.kill_switch(), "persisted kill switch");

        policy
            .set_key("kill_switch", "false")
            .expect("case-insensitive false");
        assert!(!policy.kill_switch());
        assert!(validate_key_value("kill_switch", " True ", &PolicyOverlay::default()).is_ok());
    }

    #[test]
    fn apply_to_config_overrides_exactly_the_whitelisted_fields() {
        let base = test_config();
        let overlay = PolicyOverlay {
            risk_soft_pct: Some(Decimal::new(40, 0)),
            risk_warn_pct: Some(Decimal::new(20, 0)),
            risk_hard_pct: Some(Decimal::new(10, 0)),
            reflex_reduce_fraction: Some(Decimal::new(75, 2)),
            reflex_orange_fraction: Some(Decimal::new(35, 2)),
            reflex_cooldown_secs: Some(120),
            max_order_size_usd: Some(Decimal::new(999, 0)),
            require_approval_above_usd: Some(Decimal::new(100, 0)),
            max_daily_actions: Some(3),
            kill_switch: Some(true),
        };
        let effective = apply_to_config(&base, &overlay);

        assert_eq!(effective.risk.soft_pct, Decimal::new(40, 0));
        assert_eq!(effective.risk.warn_pct, Decimal::new(20, 0));
        assert_eq!(effective.risk.hard_pct, Decimal::new(10, 0));
        assert_eq!(effective.risk.reflex_reduce_fraction, Decimal::new(75, 2));
        assert_eq!(effective.risk.reflex_orange_fraction, Decimal::new(35, 2));
        assert_eq!(effective.risk.reflex_cooldown_secs, 120);
        assert_eq!(effective.risk.max_order_size_usd, Decimal::new(999, 0));
        assert_eq!(
            effective.risk.require_approval_above_usd,
            Decimal::new(100, 0)
        );
        assert_eq!(effective.risk.max_daily_actions, 3);

        // Everything outside the overlay mapping is identical to the base.
        assert_eq!(effective.risk.market_allowlist, base.risk.market_allowlist);
        assert_eq!(
            effective.risk.idempotency_window_secs,
            base.risk.idempotency_window_secs
        );
        assert_eq!(
            effective.risk.stale_data_alert_secs,
            base.risk.stale_data_alert_secs
        );
        assert_eq!(effective.execution.mode, base.execution.mode);
        assert_eq!(
            effective.execution.heartbeat_interval_secs,
            base.execution.heartbeat_interval_secs
        );
        assert_eq!(
            effective.telegram.allowed_user_ids,
            base.telegram.allowed_user_ids
        );
        assert_eq!(
            effective.telegram.approval_chat_id,
            base.telegram.approval_chat_id
        );
        assert_eq!(
            effective.strategy.min_interval_secs,
            base.strategy.min_interval_secs
        );
        assert_eq!(
            effective.strategy.confidence_floor,
            base.strategy.confidence_floor
        );
        assert_eq!(
            effective.features.enable_reflex,
            base.features.enable_reflex
        );

        // The base config is untouched (clone semantics).
        assert_eq!(base.risk.soft_pct, Decimal::new(25, 0));
        assert_eq!(base.risk.max_daily_actions, 20);
    }

    #[test]
    fn apply_to_config_partial_overlay_keeps_base_values() {
        let base = test_config();
        let overlay = PolicyOverlay {
            max_daily_actions: Some(3),
            ..Default::default()
        };
        let effective = apply_to_config(&base, &overlay);
        assert_eq!(effective.risk.max_daily_actions, 3);
        assert_eq!(effective.risk.soft_pct, base.risk.soft_pct);
        assert_eq!(effective.risk.warn_pct, base.risk.warn_pct);
        assert_eq!(effective.risk.hard_pct, base.risk.hard_pct);
        assert_eq!(
            effective.risk.reflex_reduce_fraction,
            base.risk.reflex_reduce_fraction
        );
        assert_eq!(
            effective.risk.reflex_orange_fraction,
            base.risk.reflex_orange_fraction
        );
        assert_eq!(
            effective.risk.reflex_cooldown_secs,
            base.risk.reflex_cooldown_secs
        );
        assert_eq!(
            effective.risk.max_order_size_usd,
            base.risk.max_order_size_usd
        );
        assert_eq!(
            effective.risk.require_approval_above_usd,
            base.risk.require_approval_above_usd
        );
    }
}
