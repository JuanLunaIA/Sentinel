//! Typed REST client for the Perpl gateway (snapshots + history).
//!
//! SKELETON STUB — frozen interface (see `SPEC.md` §4.3). Replaced by agent
//! `rest`; do not change public signatures.
//!
//! Behavior contract: authenticated endpoints sign every request via
//! [`ApiKeySigner`]; public endpoints are unsigned. A private helper retries
//! **3 attempts total** on 5xx / timeout / connect errors (backoff 250 ms,
//! 1 s); 4xx are never retried. Logs are structured and never contain header
//! values (P00 invariant #5).

use serde_json::Value;

use crate::error::Result;
use crate::perpl::auth::ApiKeySigner;

/// REST client bound to one gateway base URL and one signer.
pub struct PerplRest;

impl PerplRest {
    /// Build the client.
    ///
    /// # Errors
    /// `PerplError::Rest` if the HTTP client cannot be built.
    pub fn new(_base_url: impl Into<String>, _signer: ApiKeySigner) -> Result<Self> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/pub/context` (public, unsigned).
    pub async fn get_context(&self) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/market-data/ticker` or `.../ticker/<market>` (public, unsigned).
    pub async fn get_ticker(&self, _market_id: Option<u32>) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/trading/wallet` (signed).
    pub async fn get_wallet(&self) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/trading/positions` (signed).
    pub async fn get_positions(&self) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/trading/orders` (signed).
    pub async fn get_orders(&self) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/trading/fills?count=<n>` (signed).
    pub async fn get_fills(&self, _count: u32) -> Result<Value> {
        todo!("P03 agent rest")
    }

    /// `GET /v1/trading/account-history?count=<n>` (signed).
    pub async fn get_account_history(&self, _count: u32) -> Result<Value> {
        todo!("P03 agent rest")
    }
}
