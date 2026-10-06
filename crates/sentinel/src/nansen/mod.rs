//! Nansen x402 client — cache-first, budget-capped, ledger-audited.
//!
//! Frozen by `SPEC-P09.md` §3.5: the self-paying agent. Cache hits cost
//! nothing; the spend ledger is the budget source of truth; failures degrade
//! to `SmartMoneyContext::unavailable` and never stall the caller.
//!
//! **Skeleton status (P09):** interfaces frozen; implemented by the P09 wave.

pub mod cache;
pub mod client;
pub mod spend;
pub mod x402;

use std::path::PathBuf;
use std::sync::Mutex;

use rust_decimal::Decimal;

use crate::brain::prompts::SmartMoneyContext;
use crate::config::NansenConfig;
use crate::error::Result;

use cache::TtlCache;
use spend::SpendLedger;
use x402::PayerSigner;

/// Default ledger location (gitignored `data/`).
pub const SPEND_LEDGER_PATH: &str = "data/nansen-spend.jsonl";

/// Per-call metadata returned alongside the raw payload.
#[derive(Debug, Clone, PartialEq)]
pub struct CallMeta {
    /// Endpoint path called.
    pub endpoint: String,
    /// Cost of this call, USD (`0` when cached).
    pub cost_usd: Decimal,
    /// Settlement tx hash when the facilitator reported one.
    pub tx_hash: Option<String>,
    /// True when served from the cache.
    pub cached: bool,
}

/// The paying client.
#[allow(dead_code)] // stub fields; consumed by the P09 wave
pub struct NansenClient {
    http: reqwest::Client,
    signer: PayerSigner,
    base_url: String,
    network: String,
    cache: Mutex<TtlCache>,
    ledger: SpendLedger,
    max_calls_per_hour: u32,
}

impl NansenClient {
    /// Build from configuration (`NANSEN_PAYER_KEY` must be a real 0x key at
    /// call time; construction validates its shape only).
    ///
    /// # Errors
    /// `NansenError::Sign` on a malformed payer key; `SentinelError::Internal`
    /// when the HTTP client cannot be built.
    pub fn new(_cfg: &NansenConfig) -> Result<Self> {
        todo!("P09 agent orchestrator")
    }

    /// Override the ledger path (tests).
    pub fn with_ledger_path(self, _path: impl Into<PathBuf>) -> Self {
        todo!("P09 agent orchestrator")
    }

    /// Cache-first paid call to `endpoint` with `body`.
    ///
    /// # Errors
    /// `NansenError::Budget` before any HTTP when the sliding hour is full;
    /// `{Challenge,Sign,Retry}` from the x402 flow.
    pub async fn call_endpoint(
        &self,
        _endpoint: &str,
        _body: &serde_json::Value,
        _now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        todo!("P09 agent orchestrator")
    }

    /// Paid `sm_netflow` wrapper.
    ///
    /// # Errors
    /// Same as [`NansenClient::call_endpoint`].
    pub async fn sm_netflow(
        &self,
        _chain: &str,
        _now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        todo!("P09 agent orchestrator")
    }

    /// Paid `perp_positions` wrapper.
    ///
    /// # Errors
    /// Same as [`NansenClient::call_endpoint`].
    pub async fn perp_positions(
        &self,
        _addresses: &[String],
        _now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        todo!("P09 agent orchestrator")
    }

    /// Best-effort smart-money context for `asset` (**never fails**).
    ///
    /// Fills `netflow_24h`/`fetched_at_ms`/`total_cost_usd` and always sets
    /// the cross-venue proxy `note` (SPEC-P09 §3.5 honesty requirement).
    pub async fn smart_money_context(
        &self,
        _asset: &str,
        _chain: &str,
        _now_ms: u64,
    ) -> SmartMoneyContext {
        todo!("P09 agent orchestrator")
    }
}
