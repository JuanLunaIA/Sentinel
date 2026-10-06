//! Envio HyperIndex GraphQL client (P12).
//!
//! Reads the anchor trail (audit-verify cross-check + dashboard) and
//! liquidation rows (backtester P13) from the Envio indexer. Endpoint from
//! `ENVIO_GRAPHQL_ENDPOINT` / `cfg.indexer`; a missing endpoint degrades the
//! feature (`from_env() -> None`), never the reflex path.
//!
//! **Skeleton status (P12):** interfaces frozen; implemented by the P12 wave.

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// `SentinelAnchor` entity (`SPEC` P12 §2 field names verbatim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentinelAnchor {
    /// Journal sequence (on-chain = journal seq + 1, per SPEC-P10 §13b).
    pub seq: u64,
    /// Anchored entry hash.
    pub entry_hash: String,
    /// Batch running root.
    pub root: String,
    /// Anchoring account.
    pub account: String,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// `SentinelHeartbeat` entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentinelHeartbeat {
    /// Guardian account.
    pub guardian: String,
    /// Risk-state hash from the beat.
    pub risk_state_hash: String,
    /// Max tier at beat time.
    pub max_tier: u8,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// `Liquidation` entity row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiquidationRow {
    /// Market id.
    pub market_id: u32,
    /// Liquidated account.
    pub account: String,
    /// Position size liquidated (decimal string).
    pub size: String,
    /// Liquidation price (decimal string).
    pub price: String,
    /// Event timestamp (unix seconds).
    pub ts: u64,
    /// Transaction hash.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Collateral lost (decimal string), when indexed.
    #[serde(default)]
    pub collateral_lost: Option<String>,
}

/// GraphQL client for the Envio indexer.
pub struct IndexerClient {
    endpoint: String,
    http: reqwest::Client,
}

impl IndexerClient {
    /// Build from `ENVIO_GRAPHQL_ENDPOINT`; `None` when unset (feature off).
    pub fn from_env() -> Option<Self> {
        todo!("P12 agent indexer-rs")
    }

    /// Build against an explicit endpoint.
    pub fn new(_endpoint: impl Into<String>) -> Self {
        todo!("P12 agent indexer-rs")
    }

    /// Anchors with `seq >= from_seq`, ascending.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn anchors_after(&self, _from_seq: u64) -> Result<Vec<SentinelAnchor>> {
        todo!("P12 agent indexer-rs")
    }

    /// Latest heartbeats, newest first.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn heartbeats(&self, _limit: u32) -> Result<Vec<SentinelHeartbeat>> {
        todo!("P12 agent indexer-rs")
    }

    /// Recent liquidations, optionally filtered by market.
    ///
    /// # Errors
    /// `SentinelError::Internal` on transport/GraphQL errors.
    pub async fn recent_liquidations(
        &self,
        _market_id: Option<u32>,
        _limit: u32,
    ) -> Result<Vec<LiquidationRow>> {
        todo!("P12 agent indexer-rs")
    }
}
