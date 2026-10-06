//! On-chain anchoring service — journal head → `SentinelAuditAnchor`.
//!
//! Frozen by `SPEC-P10.md` §5. The journal is the source of truth; RPC
//! failures back off and retry, never losing entries.
//!
//! **Skeleton status (P10):** interfaces frozen; implemented by the P10 wave.

use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use sentinel_core::audit::AuditJournal;

use crate::config::Config;
use crate::error::Result;

/// What the run loop reports when it stops.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnchorRunReport {
    /// Successful batch anchors.
    pub batches: u64,
    /// Successfully anchored entries.
    pub entries_anchored: u64,
    /// Successful heartbeats.
    pub heartbeats: u64,
    /// Sink failures seen (retried).
    pub failures: u64,
}

/// RPC-side abstraction (mockable in tests).
#[allow(async_fn_in_trait)]
pub trait AnchorSink {
    /// Anchor `entry_hashes` (seq range starting at `from_seq`) under `root`.
    async fn anchor_batch(
        &self,
        from_seq: u64,
        entry_hashes: &[String],
        root: &str,
    ) -> Result<String>;

    /// Post a heartbeat.
    async fn beat(
        &self,
        risk_state_hash: &str,
        open_positions: u32,
        max_tier: u8,
    ) -> Result<String>;
}

/// alloy-backed sink (real RPC).
#[allow(dead_code)] // stub fields; consumed by the P10 wave
pub struct AlloyAnchorSink {
    provider: alloy::providers::RootProvider,
    contract: alloy::primitives::Address,
    wallet: alloy::signers::local::PrivateKeySigner,
}

impl AlloyAnchorSink {
    /// Build from `AnchorConfig` + RPC url (`RPC_SIGNER_KEY`,
    /// `ANCHOR_CONTRACT_ADDRESS`); clear PENDING-WALLET error when the key
    /// or address is missing/placeholder.
    ///
    /// # Errors
    /// `SentinelError::Internal` with a PENDING-WALLET message.
    pub fn new(_cfg: &Config) -> Result<Self> {
        todo!("P10 agent anchor-side")
    }
}

impl AnchorSink for AlloyAnchorSink {
    async fn anchor_batch(
        &self,
        _from_seq: u64,
        _entry_hashes: &[String],
        _root: &str,
    ) -> Result<String> {
        todo!("P10 agent anchor-side")
    }

    async fn beat(
        &self,
        _risk_state_hash: &str,
        _open_positions: u32,
        _max_tier: u8,
    ) -> Result<String> {
        todo!("P10 agent anchor-side")
    }
}

/// Pairwise sha256 Merkle root over hex hashes (`SPEC-P10` §5).
pub fn merkle_root(_hashes: &[String]) -> String {
    todo!("P10 agent anchor-side")
}

/// `sha256` hex over canonical `{"max_tier":t,"summary":"…"}`.
pub fn risk_state_hash(_canonical_summary: &str, _max_tier: u8) -> String {
    todo!("P10 agent anchor-side")
}

/// Run heartbeats + batch anchoring until `shutdown` flips.
///
/// # Errors
/// `SentinelError::Internal` for startup-level problems only.
pub async fn run<S: AnchorSink>(
    _cfg: &Config,
    _journal: Arc<Mutex<AuditJournal>>,
    _sink: S,
    _shutdown: watch::Receiver<bool>,
) -> Result<AnchorRunReport> {
    todo!("P10 agent anchor-side")
}
