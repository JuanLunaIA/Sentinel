//! Gateway executor — signed reduce-only order submission via
//! `POST /v1/trading/orders` (SPEC-P05 §5; gateway facts in docs/FACTS.md §1.6).
//!
//! **Skeleton status (P05):** implemented by the P05 wave.

use sentinel_core::order::OrderRequest;

use super::{ExecutionReport, Executor, PositionProbe, SeqCounter};
use crate::error::Result;
use crate::perpl::auth::ApiKeySigner;

/// Submits reduce-only orders through the gateway HTTP batch endpoint.
#[allow(dead_code)] // stub fields; consumed by the P05 wave
pub struct PerplExecutor<P> {
    base_url: String,
    signer: ApiKeySigner,
    account_id: u64,
    probe: P,
    seq: SeqCounter,
    client: reqwest::Client,
}

impl<P> PerplExecutor<P> {
    /// Build the executor (constructs the HTTP client).
    ///
    /// # Errors
    /// `PerplError::Order` when the HTTP client cannot be built.
    pub fn new(
        _base_url: String,
        _signer: ApiKeySigner,
        _account_id: u64,
        _probe: P,
        _seq_seed: u64,
    ) -> Result<Self> {
        todo!("P05 agent perpl-exec")
    }
}

impl<P> Executor for PerplExecutor<P>
where
    P: PositionProbe + Sync,
{
    async fn submit(&self, _order: &OrderRequest) -> Result<ExecutionReport> {
        todo!("P05 agent perpl-exec: signed batch POST + mt:31 ack mapping")
    }
}
