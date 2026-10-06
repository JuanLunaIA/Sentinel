//! Breaker configuration — exact env keys, defaults and fail-fast validation
//! (`SPEC-P14.md` §2).
//!
//! | Key | Default | Notes |
//! |---|---|---|
//! | `BREAKER_RPC_URL` | `http://127.0.0.1:8545` | alloy HTTP provider for the anchor contract |
//! | `BREAKER_ANCHOR_ADDRESS` | — (required) | deployed `SentinelAuditAnchor` address |
//! | `BREAKER_GUARDIANS` | — (required) | csv of guardian addresses to watch |
//! | `BREAKER_HEARTBEAT_INTERVAL_SECS` | `60` | expected daemon heartbeat cadence |
//! | `BREAKER_STALE_MULT` | `3` | stale iff `age > stale_mult × interval` (strict) |
//! | `BREAKER_MODE` | `dry_run` | `dry_run` or `testnet` |
//! | `BREAKER_FRACTION` | `0.5` | fraction of the riskiest position to reduce |
//! | `BREAKER_MAX_REDUCE_USD` | `1000` | notional ceiling per reduce (clamp-down) |
//! | `BREAKER_PORT` | `9090` | armed HTTP surface port |
//! | `BREAKER_ARM_SECRET` | — (required) | HMAC-SHA256 secret for `POST /breaker/trigger` |
//! | `BREAKER_SNAPSHOT_FILE` | — (optional) | JSON `Position[]` fallback snapshot |
//! | `BREAKER_STATE_FILE` | `data/breaker-state.json` | idempotency state (tmp+rename) |
//! | `BREAKER_JOURNAL` | `data/breaker-journal.jsonl` | action journal (JSONL) |
//! | `PERPL_API_KEY` / `PERPL_API_KEY_SECRET` / `PERPL_API_URL` | — | required in `testnet` mode |
//! | `PERPL_CHAIN_ID` | `10143` | signing chain id (Monad testnet) |
//! | `TELEGRAM_BOT_TOKEN` (alias: `TELOXIDE_TOKEN`) / `TELEGRAM_APPROVAL_CHAT_ID` | — (optional pair) | alert delivery |
//!
//! `from_env` reads the process environment; `from_vars` is the deterministic
//! test seam. Required values that are missing or malformed produce a typed
//! [`ConfigError`] — nothing falls back silently to a placeholder.
//!
//! Mode-dependent requirements:
//! - always required: `BREAKER_ANCHOR_ADDRESS`, `BREAKER_GUARDIANS` and
//!   `BREAKER_ARM_SECRET` (the armed POST surface cannot authenticate without
//!   the secret);
//! - `BREAKER_MODE=testnet` additionally requires `PERPL_API_KEY`,
//!   `PERPL_API_KEY_SECRET` and `PERPL_API_URL` (live reduce submission signs
//!   every request and cannot work without them).

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use alloy::primitives::Address;
use rust_decimal::Decimal;

/// Default RPC endpoint (local anvil).
pub const DEFAULT_RPC_URL: &str = "http://127.0.0.1:8545";
/// Default expected heartbeat cadence, seconds.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 60;
/// Default staleness multiplier (`age > mult × interval`).
pub const DEFAULT_STALE_MULT: u64 = 3;
/// Default reduce fraction (string on the wire, decimal in the config).
pub const DEFAULT_FRACTION: &str = "0.5";
/// Default reduce notional ceiling in USD.
pub const DEFAULT_MAX_REDUCE_USD: &str = "1000";
/// Default armed HTTP port.
pub const DEFAULT_PORT: u16 = 9090;
/// Default idempotency state path.
pub const DEFAULT_STATE_FILE: &str = "data/breaker-state.json";
/// Default journal path.
pub const DEFAULT_JOURNAL: &str = "data/breaker-journal.jsonl";
/// Default Perpl signing chain id (Monad testnet).
pub const DEFAULT_PERPL_CHAIN_ID: u64 = 10143;

/// Execution mode of the breaker (`SPEC-P14` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerMode {
    /// Reuse the sentinel DRY_RUN executor; nothing leaves the process.
    DryRun,
    /// Submit reduce orders through the live Perpl gateway (last-resort path).
    Testnet,
}

impl BreakerMode {
    /// Lowercase wire name (`dry_run` / `testnet`), used by the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerMode::DryRun => "dry_run",
            BreakerMode::Testnet => "testnet",
        }
    }
}

/// Telegram alert delivery settings (both values required together).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    /// Bot token (`TELEGRAM_BOT_TOKEN`).
    pub bot_token: String,
    /// Target chat id (`TELEGRAM_APPROVAL_CHAT_ID`).
    pub chat_id: i64,
}

/// Typed configuration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required key is absent (or blank).
    Missing(&'static str),
    /// A present key has an unusable value.
    Invalid {
        /// The offending environment key.
        key: &'static str,
        /// Why the value was rejected (never contains secrets).
        detail: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(key) => write!(f, "missing required environment variable {key}"),
            ConfigError::Invalid { key, detail } => write!(f, "{key}: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Fully validated breaker configuration.
///
/// `Debug` is hand-written: the arm secret and every credential are redacted
/// (the workspace invariant that secrets never leak through formatting).
#[derive(Clone, PartialEq, Eq)]
pub struct BreakerConfig {
    /// alloy HTTP provider endpoint for the anchor contract.
    pub rpc_url: String,
    /// Deployed `SentinelAuditAnchor` address.
    pub anchor_address: Address,
    /// Guardian addresses watched for heartbeats (config order preserved).
    pub guardians: Vec<Address>,
    /// Expected heartbeat interval, seconds.
    pub heartbeat_interval_secs: u64,
    /// Staleness multiplier (`age > stale_mult × interval`, strict).
    pub stale_mult: u64,
    /// Execution mode.
    pub mode: BreakerMode,
    /// Fraction of the riskiest position to reduce, in `(0, 1]`.
    pub fraction: Decimal,
    /// Notional ceiling per reduce in USD (`0` degrades every plan to alert-only).
    pub max_reduce_usd: Decimal,
    /// Armed HTTP surface port (`0` = OS-assigned).
    pub port: u16,
    /// HMAC-SHA256 secret for `POST /breaker/trigger`.
    pub arm_secret: String,
    /// Optional snapshot fallback file (JSON `Position[]`).
    pub snapshot_file: Option<PathBuf>,
    /// Idempotency state file (atomic tmp+rename writes).
    pub state_file: PathBuf,
    /// Action journal (JSONL).
    pub journal: PathBuf,
    /// Perpl API key (`PERPL_API_KEY`), when configured.
    pub perpl_api_key: Option<String>,
    /// Perpl Ed25519 secret seed hex (`PERPL_API_KEY_SECRET`), when configured.
    pub perpl_api_key_secret: Option<String>,
    /// Perpl gateway base URL (`PERPL_API_URL`), when configured.
    pub perpl_api_url: Option<String>,
    /// Signing chain id (default Monad testnet `10143`).
    pub perpl_chain_id: u64,
    /// Telegram alert target, when both keys are set.
    pub telegram: Option<TelegramConfig>,
}

impl fmt::Debug for BreakerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BreakerConfig")
            .field("rpc_url", &self.rpc_url)
            .field("anchor_address", &self.anchor_address)
            .field("guardians", &self.guardians)
            .field("heartbeat_interval_secs", &self.heartbeat_interval_secs)
            .field("stale_mult", &self.stale_mult)
            .field("mode", &self.mode)
            .field("fraction", &self.fraction)
            .field("max_reduce_usd", &self.max_reduce_usd)
            .field("port", &self.port)
            .field("arm_secret", &"REDACTED")
            .field("snapshot_file", &self.snapshot_file)
            .field("state_file", &self.state_file)
            .field("journal", &self.journal)
            .field(
                "perpl_api_key",
                &self.perpl_api_key.as_ref().map(|_| "REDACTED"),
            )
            .field(
                "perpl_api_key_secret",
                &self.perpl_api_key_secret.as_ref().map(|_| "REDACTED"),
            )
            .field("perpl_api_url", &self.perpl_api_url)
            .field("perpl_chain_id", &self.perpl_chain_id)
            .field("telegram", &self.telegram.as_ref().map(|_| "REDACTED"))
            .finish()
    }
}

impl BreakerConfig {
    /// Load and validate from the process environment (`SPEC-P14` §2).
    ///
    /// # Errors
    /// [`ConfigError`] for any missing required or malformed value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(std::env::vars().collect())
    }

    /// Load and validate from an explicit variable map (the test seam).
    ///
    /// # Errors
    /// [`ConfigError`] for any missing required or malformed value.
    pub fn from_vars(vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let rpc_url =
            get_trimmed(&vars, "BREAKER_RPC_URL").unwrap_or_else(|| DEFAULT_RPC_URL.to_string());
        if rpc_url.is_empty() {
            return Err(invalid("BREAKER_RPC_URL", "must not be empty"));
        }

        let anchor_raw = require(&vars, "BREAKER_ANCHOR_ADDRESS")?;
        let anchor_address = anchor_raw.parse::<Address>().map_err(|err| {
            invalid(
                "BREAKER_ANCHOR_ADDRESS",
                &format!("not a 20-byte hex address: {err}"),
            )
        })?;

        let guardians_raw = require(&vars, "BREAKER_GUARDIANS")?;
        let guardians = parse_guardians(&guardians_raw)?;

        let heartbeat_interval_secs = parse_u64(
            &vars,
            "BREAKER_HEARTBEAT_INTERVAL_SECS",
            DEFAULT_HEARTBEAT_INTERVAL_SECS,
        )?;
        if heartbeat_interval_secs == 0 {
            return Err(invalid(
                "BREAKER_HEARTBEAT_INTERVAL_SECS",
                "must be greater than 0",
            ));
        }

        let stale_mult = parse_u64(&vars, "BREAKER_STALE_MULT", DEFAULT_STALE_MULT)?;
        if stale_mult == 0 {
            return Err(invalid("BREAKER_STALE_MULT", "must be at least 1"));
        }

        let mode = match get_trimmed(&vars, "BREAKER_MODE") {
            None => BreakerMode::DryRun,
            Some(raw) => match raw.as_str() {
                "dry_run" => BreakerMode::DryRun,
                "testnet" => BreakerMode::Testnet,
                other => {
                    return Err(invalid(
                        "BREAKER_MODE",
                        &format!("must be `dry_run` or `testnet`, got `{other}`"),
                    ));
                }
            },
        };

        let fraction = parse_decimal(
            &vars,
            "BREAKER_FRACTION",
            Decimal::from_str_exact(DEFAULT_FRACTION).expect("static decimal literal"),
        )?;
        if fraction <= Decimal::ZERO || fraction > Decimal::ONE {
            return Err(invalid(
                "BREAKER_FRACTION",
                &format!("must be in (0, 1], got {fraction}"),
            ));
        }

        let max_reduce_usd = parse_decimal(
            &vars,
            "BREAKER_MAX_REDUCE_USD",
            Decimal::from_str_exact(DEFAULT_MAX_REDUCE_USD).expect("static decimal literal"),
        )?;
        if max_reduce_usd < Decimal::ZERO {
            return Err(invalid(
                "BREAKER_MAX_REDUCE_USD",
                &format!("must not be negative, got {max_reduce_usd}"),
            ));
        }

        let port = parse_u16(&vars, "BREAKER_PORT", DEFAULT_PORT)?;

        let arm_secret = require(&vars, "BREAKER_ARM_SECRET")?;

        let snapshot_file = get_trimmed(&vars, "BREAKER_SNAPSHOT_FILE").map(PathBuf::from);
        let state_file = PathBuf::from(
            get_trimmed(&vars, "BREAKER_STATE_FILE")
                .unwrap_or_else(|| DEFAULT_STATE_FILE.to_string()),
        );
        let journal = PathBuf::from(
            get_trimmed(&vars, "BREAKER_JOURNAL").unwrap_or_else(|| DEFAULT_JOURNAL.to_string()),
        );

        let perpl_api_key = get_trimmed(&vars, "PERPL_API_KEY");
        let perpl_api_key_secret = get_trimmed(&vars, "PERPL_API_KEY_SECRET");
        let perpl_api_url = get_trimmed(&vars, "PERPL_API_URL");
        let perpl_chain_id = parse_u64(&vars, "PERPL_CHAIN_ID", DEFAULT_PERPL_CHAIN_ID)?;

        if mode == BreakerMode::Testnet {
            for (key, value) in [
                ("PERPL_API_KEY", &perpl_api_key),
                ("PERPL_API_KEY_SECRET", &perpl_api_key_secret),
                ("PERPL_API_URL", &perpl_api_url),
            ] {
                if value.is_none() {
                    return Err(ConfigError::Missing(key));
                }
            }
        }

        // Accept the daemon's token name as an alias so one `.env` value
        // configures both processes (key-readiness audit, 2026-10-06).
        let telegram = match (
            get_trimmed(&vars, "TELEGRAM_BOT_TOKEN")
                .or_else(|| get_trimmed(&vars, "TELOXIDE_TOKEN")),
            get_trimmed(&vars, "TELEGRAM_APPROVAL_CHAT_ID"),
        ) {
            (None, _) => None,
            (Some(_), None) => {
                return Err(invalid(
                    "TELEGRAM_APPROVAL_CHAT_ID",
                    "required when TELEGRAM_BOT_TOKEN is set",
                ));
            }
            (Some(bot_token), Some(chat_raw)) => {
                let chat_id = chat_raw.parse::<i64>().map_err(|_| {
                    invalid("TELEGRAM_APPROVAL_CHAT_ID", "must be an integer chat id")
                })?;
                Some(TelegramConfig { bot_token, chat_id })
            }
        };

        Ok(Self {
            rpc_url,
            anchor_address,
            guardians,
            heartbeat_interval_secs,
            stale_mult,
            mode,
            fraction,
            max_reduce_usd,
            port,
            arm_secret,
            snapshot_file,
            state_file,
            journal,
            perpl_api_key,
            perpl_api_key_secret,
            perpl_api_url,
            perpl_chain_id,
            telegram,
        })
    }

    /// Staleness threshold in seconds: `stale_mult × heartbeat_interval`
    /// (staleness itself is the strict `age > threshold` comparison).
    pub fn stale_threshold_secs(&self) -> u64 {
        self.stale_mult.saturating_mul(self.heartbeat_interval_secs)
    }

    /// Heartbeat interval in milliseconds (epoch denominator).
    pub fn heartbeat_interval_ms(&self) -> u64 {
        self.heartbeat_interval_secs.saturating_mul(1000).max(1)
    }

    /// Whether the live Perpl REST leg is configured (key + URL present).
    pub fn has_perpl_live(&self) -> bool {
        self.perpl_api_key.is_some() && self.perpl_api_url.is_some()
    }
}

/// Trimmed non-empty value for `key`, if present.
fn get_trimmed(vars: &HashMap<String, String>, key: &str) -> Option<String> {
    vars.get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Required trimmed value; [`ConfigError::Missing`] when absent or blank.
fn require(vars: &HashMap<String, String>, key: &'static str) -> Result<String, ConfigError> {
    get_trimmed(vars, key).ok_or(ConfigError::Missing(key))
}

/// Parse a `uint64` key with a default; only present values can fail.
fn parse_u64(
    vars: &HashMap<String, String>,
    key: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    match get_trimmed(vars, key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| invalid(key, &format!("must be an integer, got `{raw}`"))),
    }
}

/// Parse a `u16` key with a default; only present values can fail.
fn parse_u16(
    vars: &HashMap<String, String>,
    key: &'static str,
    default: u16,
) -> Result<u16, ConfigError> {
    match get_trimmed(vars, key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<u16>()
            .map_err(|_| invalid(key, &format!("must be a port number, got `{raw}`"))),
    }
}

/// Parse a decimal key with a default; only present values can fail.
fn parse_decimal(
    vars: &HashMap<String, String>,
    key: &'static str,
    default: Decimal,
) -> Result<Decimal, ConfigError> {
    match get_trimmed(vars, key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<Decimal>()
            .map_err(|_| invalid(key, &format!("must be a decimal number, got `{raw}`"))),
    }
}

/// Split the guardian csv, parse every address and deduplicate (order kept).
fn parse_guardians(raw: &str) -> Result<Vec<Address>, ConfigError> {
    let mut guardians: Vec<Address> = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let address = trimmed.parse::<Address>().map_err(|err| {
            invalid(
                "BREAKER_GUARDIANS",
                &format!("`{trimmed}` is not a 20-byte hex address: {err}"),
            )
        })?;
        if !guardians.contains(&address) {
            guardians.push(address);
        }
    }
    if guardians.is_empty() {
        return Err(invalid(
            "BREAKER_GUARDIANS",
            "at least one guardian address is required",
        ));
    }
    Ok(guardians)
}

/// Build an [`ConfigError::Invalid`] with a static key.
fn invalid(key: &'static str, detail: &str) -> ConfigError {
    ConfigError::Invalid {
        key,
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same guardian in mixed case must parse to the same bytes.
    const GUARDIAN: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const ANCHOR: &str = "0x5FbDB2315678afecb367f032d93F642f64180aa3";

    fn base() -> HashMap<String, String> {
        HashMap::from([
            ("BREAKER_ANCHOR_ADDRESS".to_string(), ANCHOR.to_string()),
            ("BREAKER_GUARDIANS".to_string(), GUARDIAN.to_string()),
            ("BREAKER_ARM_SECRET".to_string(), "secret".to_string()),
        ])
    }

    fn with(vars: &mut HashMap<String, String>, key: &str, value: &str) {
        vars.insert(key.to_string(), value.to_string());
    }

    #[test]
    fn minimal_env_applies_the_frozen_defaults() {
        let cfg = BreakerConfig::from_vars(base()).expect("minimal config loads");
        assert_eq!(cfg.rpc_url, DEFAULT_RPC_URL);
        assert_eq!(cfg.heartbeat_interval_secs, 60);
        assert_eq!(cfg.stale_mult, 3);
        assert_eq!(cfg.mode, BreakerMode::DryRun);
        assert_eq!(cfg.fraction, Decimal::from_str_exact("0.5").unwrap());
        assert_eq!(cfg.max_reduce_usd, Decimal::from_str_exact("1000").unwrap());
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.state_file, PathBuf::from("data/breaker-state.json"));
        assert_eq!(cfg.journal, PathBuf::from("data/breaker-journal.jsonl"));
        assert_eq!(cfg.snapshot_file, None);
        assert_eq!(cfg.perpl_api_key, None);
        assert_eq!(cfg.perpl_api_url, None);
        assert_eq!(cfg.perpl_chain_id, 10143);
        assert_eq!(cfg.telegram, None);
        assert_eq!(cfg.guardians, vec![GUARDIAN.parse::<Address>().unwrap()]);
        assert_eq!(cfg.stale_threshold_secs(), 180);
    }

    #[test]
    fn missing_required_keys_are_typed_errors() {
        for key in [
            "BREAKER_ANCHOR_ADDRESS",
            "BREAKER_GUARDIANS",
            "BREAKER_ARM_SECRET",
        ] {
            let mut vars = base();
            vars.remove(key);
            let err = BreakerConfig::from_vars(vars).expect_err("must be required");
            assert_eq!(err, ConfigError::Missing(key));
        }

        // Blank values count as missing.
        let mut vars = base();
        with(&mut vars, "BREAKER_ARM_SECRET", "   ");
        assert_eq!(
            BreakerConfig::from_vars(vars).expect_err("blank secret"),
            ConfigError::Missing("BREAKER_ARM_SECRET")
        );
    }

    #[test]
    fn invalid_scalars_are_rejected() {
        let cases: [(&str, &str); 7] = [
            ("BREAKER_ANCHOR_ADDRESS", "0xnot-an-address"),
            ("BREAKER_GUARDIANS", "0x1234,nope"),
            ("BREAKER_MODE", "mainnet"),
            ("BREAKER_FRACTION", "1.5"),
            ("BREAKER_HEARTBEAT_INTERVAL_SECS", "0"),
            ("BREAKER_STALE_MULT", "0"),
            ("BREAKER_PORT", "70000"),
        ];
        for (key, value) in cases {
            let mut vars = base();
            with(&mut vars, key, value);
            let err = BreakerConfig::from_vars(vars)
                .unwrap_err_or_panic(&format!("{key}={value} must be rejected"));
            assert_eq!(err.key(), key, "wrong key in error for {key}={value}");
        }

        // Zero fraction is out of range too.
        let mut vars = base();
        with(&mut vars, "BREAKER_FRACTION", "0");
        assert!(BreakerConfig::from_vars(vars).is_err());
        // Negative max reduce is rejected; exact 1.0 fraction is accepted.
        let mut vars = base();
        with(&mut vars, "BREAKER_MAX_REDUCE_USD", "-1");
        assert!(BreakerConfig::from_vars(vars).is_err());
        let mut vars = base();
        with(&mut vars, "BREAKER_FRACTION", "1");
        assert!(BreakerConfig::from_vars(vars).is_ok());
    }

    #[test]
    fn testnet_requires_perpl_credentials_but_dry_run_does_not() {
        let mut vars = base();
        with(&mut vars, "BREAKER_MODE", "testnet");
        assert_eq!(
            BreakerConfig::from_vars(vars).unwrap_err_or_panic("testnet needs perpl"),
            ConfigError::Missing("PERPL_API_KEY")
        );

        let mut vars = base();
        with(&mut vars, "BREAKER_MODE", "testnet");
        with(&mut vars, "PERPL_API_KEY", "key");
        with(&mut vars, "PERPL_API_URL", "https://testnet.perpl.xyz/api");
        assert_eq!(
            BreakerConfig::from_vars(vars).unwrap_err_or_panic("testnet needs secret"),
            ConfigError::Missing("PERPL_API_KEY_SECRET")
        );

        let mut vars = base();
        with(&mut vars, "BREAKER_MODE", "testnet");
        with(&mut vars, "PERPL_API_KEY", "key");
        with(&mut vars, "PERPL_API_KEY_SECRET", "00");
        with(&mut vars, "PERPL_API_URL", "https://testnet.perpl.xyz/api");
        let cfg = BreakerConfig::from_vars(vars).expect("testnet config loads");
        assert_eq!(cfg.mode, BreakerMode::Testnet);
        assert!(cfg.has_perpl_live());
    }

    #[test]
    fn telegram_accepts_teloxide_token_alias() {
        let mut vars = base();
        with(&mut vars, "TELOXIDE_TOKEN", "999:alias");
        with(&mut vars, "TELEGRAM_APPROVAL_CHAT_ID", "7");
        let cfg = BreakerConfig::from_vars(vars).expect("alias loads");
        assert_eq!(
            cfg.telegram,
            Some(TelegramConfig {
                bot_token: "999:alias".to_string(),
                chat_id: 7,
            })
        );
    }

    #[test]
    fn telegram_pair_must_be_complete() {
        let mut vars = base();
        with(&mut vars, "TELEGRAM_BOT_TOKEN", "123:abc");
        assert!(BreakerConfig::from_vars(vars).is_err(), "chat id required");

        let mut vars = base();
        with(&mut vars, "TELEGRAM_BOT_TOKEN", "123:abc");
        with(&mut vars, "TELEGRAM_APPROVAL_CHAT_ID", "42");
        let cfg = BreakerConfig::from_vars(vars).expect("pair loads");
        assert_eq!(
            cfg.telegram,
            Some(TelegramConfig {
                bot_token: "123:abc".to_string(),
                chat_id: 42,
            })
        );

        // A chat id without a token is inert (alerts simply fall back to tracing).
        let mut vars = base();
        with(&mut vars, "TELEGRAM_APPROVAL_CHAT_ID", "42");
        let cfg = BreakerConfig::from_vars(vars).expect("chat id alone is inert");
        assert_eq!(cfg.telegram, None);
    }

    #[test]
    fn guardians_are_deduplicated_case_insensitively_in_order() {
        let mut vars = base();
        with(
            &mut vars,
            "BREAKER_GUARDIANS",
            &format!(
                "{GUARDIAN}, 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 , 0x0000000000000000000000000000000000000001"
            ),
        );
        let cfg = BreakerConfig::from_vars(vars).expect("loads");
        assert_eq!(cfg.guardians.len(), 2);
        assert_eq!(
            cfg.guardians[0],
            GUARDIAN.parse::<Address>().unwrap(),
            "first occurrence (checksummed) keeps its position"
        );
    }

    #[test]
    fn optional_paths_and_bounds_round_trip() {
        let mut vars = base();
        with(
            &mut vars,
            "BREAKER_SNAPSHOT_FILE",
            " tests/fixtures/p14/breaker-snapshot.json ",
        );
        with(&mut vars, "BREAKER_STATE_FILE", "tmp/state.json");
        with(&mut vars, "BREAKER_JOURNAL", "tmp/journal.jsonl");
        with(&mut vars, "BREAKER_PORT", "0");
        with(&mut vars, "BREAKER_RPC_URL", "http://127.0.0.1:8547");
        let cfg = BreakerConfig::from_vars(vars).expect("loads");
        assert_eq!(
            cfg.snapshot_file,
            Some(PathBuf::from("tests/fixtures/p14/breaker-snapshot.json"))
        );
        assert_eq!(cfg.state_file, PathBuf::from("tmp/state.json"));
        assert_eq!(cfg.journal, PathBuf::from("tmp/journal.jsonl"));
        assert_eq!(cfg.port, 0);
        assert_eq!(cfg.rpc_url, "http://127.0.0.1:8547");

        // An empty snapshot file value is the same as unset.
        let mut vars = base();
        with(&mut vars, "BREAKER_SNAPSHOT_FILE", "");
        assert_eq!(BreakerConfig::from_vars(vars).unwrap().snapshot_file, None);
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut vars = base();
        with(&mut vars, "BREAKER_ARM_SECRET", "super-secret-value");
        with(&mut vars, "PERPL_API_KEY", "perpl-key-value");
        with(&mut vars, "PERPL_API_KEY_SECRET", "perpl-secret-value");
        with(&mut vars, "PERPL_API_URL", "https://example.invalid/api");
        with(&mut vars, "TELEGRAM_BOT_TOKEN", "bot-token-value");
        with(&mut vars, "TELEGRAM_APPROVAL_CHAT_ID", "7");
        let cfg = BreakerConfig::from_vars(vars).expect("loads");
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("perpl-key-value"));
        assert!(!rendered.contains("perpl-secret-value"));
        assert!(!rendered.contains("bot-token-value"));
        assert!(rendered.contains("REDACTED"));
    }

    /// Small helper: unwrap the error side of a [`ConfigError`] result.
    trait UnwrapErrOrPanic {
        fn unwrap_err_or_panic(self, context: &str) -> ConfigError;
    }
    impl UnwrapErrOrPanic for Result<BreakerConfig, ConfigError> {
        fn unwrap_err_or_panic(self, context: &str) -> ConfigError {
            match self {
                Ok(_) => panic!("{context}"),
                Err(err) => err,
            }
        }
    }

    impl ConfigError {
        fn key(&self) -> &'static str {
            match self {
                ConfigError::Missing(key) => key,
                ConfigError::Invalid { key, .. } => key,
            }
        }
    }
}
