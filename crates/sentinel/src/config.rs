//! Strongly-typed configuration, loaded from the environment with
//! **fail-fast validation**: a misconfigured Sentinel refuses to start.
//!
//! Values are read from `.env` (via `dotenvy`) and the process environment;
//! explicit `*_URL` / address overrides win over the per-network defaults.
//! Defaults for Perpl follow the live values verified in `docs/FACTS.md`
//! (including the corrected testnet collateral token).
//!
//! Secrets are wrapped in [`SecretString`] so they can never leak through
//! `Debug` formatting (P00 invariant #5).

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use rust_decimal::Decimal;

use sentinel_core::types::ExecutionMode;

use crate::error::{ConfigError, Result};

/// A string that never leaks through `Debug` formatting.
///
/// There is deliberately **no** `Display` implementation; read the value only
/// through [`SecretString::expose`], and keep those call sites few.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret value.
    pub fn new(inner: impl Into<String>) -> Self {
        Self(inner.into())
    }

    /// Explicitly expose the secret. Call sites should be few and auditable.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

/// Perpl network the daemon talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerplEnv {
    /// Monad testnet (chain id 10143).
    Testnet,
    /// Monad mainnet (chain id 143).
    Mainnet,
}

impl fmt::Display for PerplEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PerplEnv::Testnet => "testnet",
            PerplEnv::Mainnet => "mainnet",
        })
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable `tracing` output.
    Pretty,
    /// JSON lines (machine-parseable; used by deployment platforms).
    Json,
}

/// Perpl connection and API-key settings.
#[derive(Debug, Clone)]
pub struct PerplConfig {
    /// Selected network.
    pub env_name: PerplEnv,
    /// Chain id (`10143` testnet, `143` mainnet).
    pub chain_id: u64,
    /// REST base URL, e.g. `https://testnet.perpl.xyz/api`.
    pub api_url: String,
    /// WebSocket base URL, e.g. `wss://testnet.perpl.xyz`.
    pub ws_url: String,
    /// Monad RPC endpoint.
    pub rpc_url: String,
    /// Exchange contract address.
    pub exchange_address: String,
    /// Collateral token address (AUSD).
    pub collateral_token: String,
    /// Opaque `X-API-Key` token.
    pub api_key: SecretString,
    /// Hex of the 32-byte Ed25519 seed used to sign requests.
    pub api_key_secret: SecretString,
    /// Wallet address owning the exchange account (optional; verified later).
    pub account: Option<String>,
}

/// Qwen (Alibaba Model Studio / DashScope) settings.
#[derive(Debug, Clone)]
pub struct QwenConfig {
    /// API key.
    pub api_key: SecretString,
    /// OpenAI-compatible base URL (intl or CN; confirm with the real key).
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Output token budget (must cover the thinking trace).
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: Decimal,
}

/// Kimi (Moonshot) fallback-provider settings.
#[derive(Debug, Clone)]
pub struct KimiConfig {
    /// API key.
    pub api_key: SecretString,
    /// OpenAI-compatible base URL.
    pub base_url: String,
    /// Model id (confirm the exact `kimi-k3`-family string).
    pub model: String,
}

/// Nansen x402 client settings.
#[derive(Debug, Clone)]
pub struct NansenConfig {
    /// API base URL.
    pub base_url: String,
    /// Private key of the **separate, low-balance** x402 payer wallet.
    pub payer_key: SecretString,
    /// Response cache TTL in seconds (cost control).
    pub cache_ttl_secs: u64,
    /// Hourly call budget.
    pub max_calls_per_hour: u32,
    /// Payment rail network id (from the live 402 challenge).
    pub payment_network: String,
}

/// Telegram bot settings.
#[derive(Debug, Clone)]
pub struct TelegramConfig {
    /// Bot token from @BotFather.
    pub token: SecretString,
    /// Numeric user ids allowed to command the bot (non-empty allowlist).
    pub allowed_user_ids: Vec<u64>,
    /// Chat where approval requests are posted/tracked (optional until P11).
    pub approval_chat_id: Option<i64>,
}

/// Audit anchoring settings.
#[derive(Debug, Clone)]
pub struct AnchorConfig {
    /// Deployed `SentinelAuditAnchor` address (optional until P10).
    pub contract_address: Option<String>,
    /// Signer used to post anchors/heartbeats (separate from trading key).
    pub rpc_signer_key: Option<SecretString>,
}

/// Risk policy values (percent units where named `_pct`).
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Soft distance-to-liquidation threshold, percent.
    pub soft_pct: Decimal,
    /// Warn distance-to-liquidation threshold, percent.
    pub warn_pct: Decimal,
    /// Hard distance-to-liquidation threshold, percent.
    pub hard_pct: Decimal,
    /// Fraction of size a reflex reduce removes, `(0, 1]`.
    pub reflex_reduce_fraction: Decimal,
    /// Fraction of size an Orange (or gated-stale) reduce removes, `(0, 1]`.
    pub reflex_orange_fraction: Decimal,
    /// Per-market cooldown between reflexive actions, seconds.
    pub reflex_cooldown_secs: u64,
    /// Per-action notional cap, USD.
    pub max_order_size_usd: Decimal,
    /// Daily action cap.
    pub max_daily_actions: u32,
    /// Market allowlist (Perpl market ids).
    pub market_allowlist: Vec<u32>,
    /// Notional above which approval is required, USD.
    pub require_approval_above_usd: Decimal,
    /// Idempotency window for duplicate suppression, seconds.
    pub idempotency_window_secs: u64,
    /// Feed staleness alert threshold, seconds.
    pub stale_data_alert_secs: u64,
}

/// Strategy brain pacing/quality gates.
#[derive(Debug, Clone)]
pub struct StrategyConfig {
    /// Minimum interval between strategy consults, seconds.
    pub min_interval_secs: u64,
    /// Minimum model confidence accepted, `[0, 1]`.
    pub confidence_floor: Decimal,
}

/// Execution mode and heartbeat pacing.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// `DRY_RUN` / `TESTNET` / `MAINNET`.
    pub mode: ExecutionMode,
    /// On-chain heartbeat interval, seconds.
    pub heartbeat_interval_secs: u64,
}

/// Observability settings.
#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    /// `EnvFilter` directive string.
    pub rust_log: String,
    /// Stdout format.
    pub log_format: LogFormat,
    /// Directory for the daily-rolling log file.
    pub log_dir: PathBuf,
}

/// Feature flags.
#[derive(Debug, Clone)]
pub struct FeatureFlags {
    /// Enable the deterministic reflex loop.
    pub enable_reflex: bool,
    /// Enable LLM strategy consults.
    pub enable_strategy: bool,
    /// Enable Nansen data enrichment.
    pub enable_nansen: bool,
    /// Enable on-chain audit anchoring.
    pub enable_anchor: bool,
    /// Enable the CRE hook endpoint.
    pub enable_cre_hook: bool,
}

/// Root configuration object.
#[derive(Debug, Clone)]
pub struct Config {
    /// Perpl connection + API key.
    pub perpl: PerplConfig,
    /// Qwen provider.
    pub qwen: QwenConfig,
    /// Kimi provider.
    pub kimi: KimiConfig,
    /// Nansen x402 client.
    pub nansen: NansenConfig,
    /// Telegram bot.
    pub telegram: TelegramConfig,
    /// Audit anchor.
    pub anchor: AnchorConfig,
    /// Risk policy.
    pub risk: RiskConfig,
    /// Strategy pacing.
    pub strategy: StrategyConfig,
    /// Execution mode.
    pub execution: ExecutionConfig,
    /// Observability.
    pub observability: ObservabilityConfig,
    /// Feature flags.
    pub features: FeatureFlags,
}

impl Config {
    /// Load configuration from `.env` (best-effort) plus the process
    /// environment, then validate.
    ///
    /// # Errors
    /// Fails fast on missing required variables, unparseable values, or
    /// violated invariants (e.g. `hard < warn < soft`, mainnet acknowledgement).
    pub fn load() -> Result<Config> {
        // Best-effort: a missing .env is fine (CI, containers with real env).
        let _ = dotenvy::dotenv();
        Self::from_vars(std::env::vars().collect())
    }

    /// Build a configuration from an explicit variable map (tests, embeds).
    ///
    /// # Errors
    /// Same failure modes as [`Config::load`].
    pub fn from_vars(vars: HashMap<String, String>) -> Result<Config> {
        let env_name = match req(&vars, "PERPL_ENV")?.to_ascii_lowercase().as_str() {
            "testnet" => PerplEnv::Testnet,
            "mainnet" => PerplEnv::Mainnet,
            _ => return Err(cfg_err("PERPL_ENV", "expected \"testnet\" or \"mainnet\"")),
        };

        let secret_hex = req(&vars, "PERPL_API_KEY_SECRET")?;
        let hex_part = secret_hex.strip_prefix("0x").unwrap_or(&secret_hex);
        match hex::decode(hex_part) {
            Ok(bytes) if bytes.len() == 32 => {}
            Ok(_) => {
                return Err(cfg_err(
                    "PERPL_API_KEY_SECRET",
                    "expected 32 bytes of hex (64 hex chars)",
                ));
            }
            Err(_) => {
                return Err(cfg_err("PERPL_API_KEY_SECRET", "expected hex encoding"));
            }
        }

        let (default_api, default_ws, default_rpc, default_exchange, default_collateral) =
            match env_name {
                PerplEnv::Testnet => (
                    "https://testnet.perpl.xyz/api",
                    "wss://testnet.perpl.xyz",
                    "https://testnet-rpc.monad.xyz",
                    "0x1964c32f0be608e7d29302aff5e61268e72080cc",
                    "0xa9012a055bd4e0edff8ce09f960291c09d5322dc",
                ),
                PerplEnv::Mainnet => (
                    "https://app.perpl.xyz/api",
                    "wss://app.perpl.xyz",
                    "https://rpc.monad.xyz",
                    "0x34B6552d57a35a1D042CcAe1951BD1C370112a6F",
                    "0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a",
                ),
            };

        let perpl = PerplConfig {
            env_name,
            chain_id: match env_name {
                PerplEnv::Testnet => 10143,
                PerplEnv::Mainnet => 143,
            },
            api_url: opt(&vars, "PERPL_API_URL").unwrap_or_else(|| default_api.to_string()),
            ws_url: opt(&vars, "PERPL_WS_URL").unwrap_or_else(|| default_ws.to_string()),
            rpc_url: opt(&vars, "PERPL_RPC_URL").unwrap_or_else(|| default_rpc.to_string()),
            exchange_address: opt(&vars, "PERPL_EXCHANGE_ADDRESS")
                .unwrap_or_else(|| default_exchange.to_string()),
            collateral_token: opt(&vars, "PERPL_COLLATERAL_TOKEN")
                .unwrap_or_else(|| default_collateral.to_string()),
            api_key: SecretString::new(req(&vars, "PERPL_API_KEY")?),
            api_key_secret: SecretString::new(secret_hex),
            account: opt(&vars, "PERPL_ACCOUNT"),
        };
        require_address("PERPL_EXCHANGE_ADDRESS", &perpl.exchange_address)?;
        require_address("PERPL_COLLATERAL_TOKEN", &perpl.collateral_token)?;

        let qwen = QwenConfig {
            api_key: SecretString::new(req(&vars, "QWEN_API_KEY")?),
            base_url: opt(&vars, "QWEN_BASE_URL")
                .unwrap_or_else(|| "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into()),
            model: opt(&vars, "QWEN_MODEL").unwrap_or_else(|| "qwen3.8-max".into()),
            max_tokens: parse_or(&vars, "QWEN_MAX_TOKENS", 4000u32)?,
            temperature: parse_or(&vars, "QWEN_TEMPERATURE", Decimal::new(1, 1))?,
        };

        let kimi = KimiConfig {
            api_key: SecretString::new(req(&vars, "KIMI_API_KEY")?),
            base_url: opt(&vars, "KIMI_BASE_URL")
                .unwrap_or_else(|| "https://api.moonshot.ai/v1".into()),
            model: opt(&vars, "KIMI_MODEL").unwrap_or_else(|| "kimi-k3".into()),
        };

        let nansen = NansenConfig {
            base_url: opt(&vars, "NANSEN_BASE_URL")
                .unwrap_or_else(|| "https://api.nansen.ai".into()),
            payer_key: SecretString::new(req(&vars, "NANSEN_PAYER_KEY")?),
            cache_ttl_secs: parse_or(&vars, "NANSEN_CACHE_TTL_SECS", 300u64)?,
            max_calls_per_hour: parse_or(&vars, "NANSEN_MAX_CALLS_PER_HOUR", 40u32)?,
            payment_network: opt(&vars, "NANSEN_PAYMENT_NETWORK")
                .unwrap_or_else(|| "eip155:143".into()),
        };

        let allowed_user_ids = parse_csv_u64(&vars, "TELEGRAM_ALLOWED_USER_IDS")?;
        if allowed_user_ids.is_empty() {
            return Err(cfg_err(
                "TELEGRAM_ALLOWED_USER_IDS",
                "must contain at least one numeric user id",
            ));
        }
        let telegram = TelegramConfig {
            token: SecretString::new(req(&vars, "TELOXIDE_TOKEN")?),
            allowed_user_ids,
            approval_chat_id: parse_opt(&vars, "TELEGRAM_APPROVAL_CHAT_ID")?,
        };

        let anchor = AnchorConfig {
            contract_address: opt(&vars, "ANCHOR_CONTRACT_ADDRESS"),
            rpc_signer_key: opt(&vars, "RPC_SIGNER_KEY").map(SecretString::new),
        };
        if let Some(addr) = &anchor.contract_address {
            require_address("ANCHOR_CONTRACT_ADDRESS", addr)?;
        }

        let risk = RiskConfig {
            soft_pct: parse_or(&vars, "RISK_SOFT_PCT", Decimal::new(25, 0))?,
            warn_pct: parse_or(&vars, "RISK_WARN_PCT", Decimal::new(15, 0))?,
            hard_pct: parse_or(&vars, "RISK_HARD_PCT", Decimal::new(8, 0))?,
            reflex_reduce_fraction: parse_or(&vars, "REFLEX_REDUCE_FRACTION", Decimal::new(5, 1))?,
            reflex_orange_fraction: parse_or(&vars, "REFLEX_ORANGE_FRACTION", Decimal::new(25, 2))?,
            reflex_cooldown_secs: parse_or(&vars, "REFLEX_COOLDOWN_SECS", 600u64)?,
            max_order_size_usd: parse_or(&vars, "MAX_ORDER_SIZE_USD", Decimal::new(5000, 0))?,
            max_daily_actions: parse_or(&vars, "MAX_DAILY_ACTIONS", 20u32)?,
            market_allowlist: parse_csv_u32(&vars, "MARKET_ALLOWLIST", "20,1")?,
            require_approval_above_usd: parse_or(
                &vars,
                "REQUIRE_APPROVAL_ABOVE_USD",
                Decimal::new(2500, 0),
            )?,
            idempotency_window_secs: parse_or(&vars, "IDEMPOTENCY_WINDOW_SECS", 60u64)?,
            stale_data_alert_secs: parse_or(&vars, "STALE_DATA_ALERT_SECS", 30u64)?,
        };

        let strategy = StrategyConfig {
            min_interval_secs: parse_or(&vars, "STRATEGY_MIN_INTERVAL_SECS", 120u64)?,
            confidence_floor: parse_or(&vars, "STRATEGY_CONFIDENCE_FLOOR", Decimal::new(6, 1))?,
        };

        let mode = match opt(&vars, "EXECUTION_MODE")
            .unwrap_or_else(|| "DRY_RUN".to_string())
            .to_ascii_uppercase()
            .as_str()
        {
            "DRY_RUN" => ExecutionMode::DryRun,
            "TESTNET" => ExecutionMode::Testnet,
            "MAINNET" => ExecutionMode::Mainnet,
            _ => {
                return Err(cfg_err(
                    "EXECUTION_MODE",
                    "expected DRY_RUN, TESTNET or MAINNET",
                ));
            }
        };
        if mode == ExecutionMode::Mainnet
            && !opt(&vars, "I_UNDERSTAND_MAINNET_RISK")
                .is_some_and(|v| v.eq_ignore_ascii_case("yes"))
        {
            return Err(cfg_err(
                "EXECUTION_MODE",
                "MAINNET requires I_UNDERSTAND_MAINNET_RISK=yes",
            ));
        }
        // The execution mode must be consistent with the connected network:
        // live orders go to whatever environment the client is pointed at.
        match (mode, env_name) {
            (ExecutionMode::Testnet, PerplEnv::Mainnet) => {
                return Err(cfg_err(
                    "EXECUTION_MODE",
                    "TESTNET requires PERPL_ENV=testnet",
                ));
            }
            (ExecutionMode::Mainnet, PerplEnv::Testnet) => {
                return Err(cfg_err(
                    "EXECUTION_MODE",
                    "MAINNET requires PERPL_ENV=mainnet",
                ));
            }
            _ => {}
        }
        let execution = ExecutionConfig {
            mode,
            heartbeat_interval_secs: parse_or(&vars, "HEARTBEAT_INTERVAL_SECS", 120u64)?,
        };

        let observability = ObservabilityConfig {
            rust_log: opt(&vars, "RUST_LOG")
                .unwrap_or_else(|| "sentinel=debug,tower_http=info".into()),
            log_format: match opt(&vars, "LOG_FORMAT")
                .unwrap_or_else(|| "pretty".to_string())
                .to_ascii_lowercase()
                .as_str()
            {
                "pretty" => LogFormat::Pretty,
                "json" => LogFormat::Json,
                _ => return Err(cfg_err("LOG_FORMAT", "expected \"pretty\" or \"json\"")),
            },
            log_dir: PathBuf::from(opt(&vars, "LOG_DIR").unwrap_or_else(|| "./logs".into())),
        };

        let features = FeatureFlags {
            enable_reflex: parse_bool(&vars, "ENABLE_REFLEX", true)?,
            enable_strategy: parse_bool(&vars, "ENABLE_STRATEGY", true)?,
            enable_nansen: parse_bool(&vars, "ENABLE_NANSEN", true)?,
            enable_anchor: parse_bool(&vars, "ENABLE_ANCHOR", true)?,
            enable_cre_hook: parse_bool(&vars, "ENABLE_CRE_HOOK", false)?,
        };

        let cfg = Config {
            perpl,
            qwen,
            kimi,
            nansen,
            telegram,
            anchor,
            risk,
            strategy,
            execution,
            observability,
            features,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate cross-field invariants.
    ///
    /// # Errors
    /// Returns the first violated invariant as a [`ConfigError::Var`].
    pub fn validate(&self) -> Result<()> {
        let r = &self.risk;
        if !(r.hard_pct < r.warn_pct && r.warn_pct < r.soft_pct) {
            return Err(cfg_err(
                "RISK_HARD_PCT/RISK_WARN_PCT/RISK_SOFT_PCT",
                "must satisfy hard < warn < soft",
            ));
        }
        if r.reflex_reduce_fraction <= Decimal::ZERO || r.reflex_reduce_fraction > Decimal::ONE {
            return Err(cfg_err("REFLEX_REDUCE_FRACTION", "must be in (0, 1]"));
        }
        if r.reflex_orange_fraction <= Decimal::ZERO || r.reflex_orange_fraction > Decimal::ONE {
            return Err(cfg_err("REFLEX_ORANGE_FRACTION", "must be in (0, 1]"));
        }
        if r.reflex_cooldown_secs == 0 {
            return Err(cfg_err("REFLEX_COOLDOWN_SECS", "must be > 0"));
        }
        if r.max_order_size_usd <= Decimal::ZERO {
            return Err(cfg_err("MAX_ORDER_SIZE_USD", "must be > 0"));
        }
        if r.require_approval_above_usd < Decimal::ZERO {
            return Err(cfg_err("REQUIRE_APPROVAL_ABOVE_USD", "must be >= 0"));
        }
        if r.market_allowlist.is_empty() {
            return Err(cfg_err(
                "MARKET_ALLOWLIST",
                "must contain at least one market id",
            ));
        }
        if r.idempotency_window_secs == 0 {
            return Err(cfg_err("IDEMPOTENCY_WINDOW_SECS", "must be > 0"));
        }
        if r.stale_data_alert_secs == 0 {
            return Err(cfg_err("STALE_DATA_ALERT_SECS", "must be > 0"));
        }
        if self.strategy.confidence_floor < Decimal::ZERO
            || self.strategy.confidence_floor > Decimal::ONE
        {
            return Err(cfg_err("STRATEGY_CONFIDENCE_FLOOR", "must be in [0, 1]"));
        }
        if self.strategy.min_interval_secs == 0 {
            return Err(cfg_err("STRATEGY_MIN_INTERVAL_SECS", "must be > 0"));
        }
        if self.execution.heartbeat_interval_secs == 0 {
            return Err(cfg_err("HEARTBEAT_INTERVAL_SECS", "must be > 0"));
        }
        if self.qwen.max_tokens == 0 {
            return Err(cfg_err("QWEN_MAX_TOKENS", "must be > 0"));
        }
        if self.qwen.temperature < Decimal::ZERO || self.qwen.temperature > Decimal::new(2, 0) {
            return Err(cfg_err("QWEN_TEMPERATURE", "must be in [0, 2]"));
        }
        if self.nansen.cache_ttl_secs == 0 {
            return Err(cfg_err("NANSEN_CACHE_TTL_SECS", "must be > 0"));
        }
        if self.nansen.max_calls_per_hour == 0 {
            return Err(cfg_err("NANSEN_MAX_CALLS_PER_HOUR", "must be > 0"));
        }
        Ok(())
    }
}

fn cfg_err(name: &str, problem: &str) -> crate::error::SentinelError {
    ConfigError::Var {
        name: name.to_string(),
        problem: problem.to_string(),
    }
    .into()
}

/// Trimmed non-empty value for `name`.
fn get<'a>(vars: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    vars.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Optional trimmed value.
fn opt(vars: &HashMap<String, String>, name: &str) -> Option<String> {
    get(vars, name).map(ToOwned::to_owned)
}

/// Required non-empty value.
fn req(vars: &HashMap<String, String>, name: &str) -> Result<String> {
    opt(vars, name).ok_or_else(|| cfg_err(name, "missing (required, non-empty)"))
}

/// Validate an `0x`-prefixed 20-byte address.
fn require_address(name: &str, value: &str) -> Result<()> {
    let ok = value.len() == 42
        && value.starts_with("0x")
        && value[2..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(cfg_err(name, "expected a 0x-prefixed 20-byte address"))
    }
}

/// Parse an optional value of type `T`.
fn parse_opt<T>(vars: &HashMap<String, String>, name: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match get(vars, name) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<T>()
            .map(Some)
            .map_err(|e| cfg_err(name, &format!("invalid value: {e}"))),
    }
}

/// Parse a value of type `T`, falling back to `default` when unset.
fn parse_or<T>(vars: &HashMap<String, String>, name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match get(vars, name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<T>()
            .map_err(|e| cfg_err(name, &format!("invalid value: {e}"))),
    }
}

/// Parse a boolean accepting `true/false/1/0/yes/no`.
fn parse_bool(vars: &HashMap<String, String>, name: &str, default: bool) -> Result<bool> {
    match get(vars, name) {
        None => Ok(default),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(cfg_err(name, "expected true/false/1/0/yes/no")),
        },
    }
}

/// Parse a comma-separated list of `u32` market ids.
fn parse_csv_u32(vars: &HashMap<String, String>, name: &str, default: &str) -> Result<Vec<u32>> {
    let raw = get(vars, name).unwrap_or(default);
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(
            part.parse::<u32>()
                .map_err(|e| cfg_err(name, &format!("invalid market id: {e}")))?,
        );
    }
    Ok(out)
}

/// Parse a required, non-empty comma-separated list of `u64` user ids.
fn parse_csv_u64(vars: &HashMap<String, String>, name: &str) -> Result<Vec<u64>> {
    let raw = opt(vars, name).unwrap_or_default();
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(
            part.parse::<u64>()
                .map_err(|e| cfg_err(name, &format!("invalid user id: {e}")))?,
        );
    }
    Ok(out)
}
