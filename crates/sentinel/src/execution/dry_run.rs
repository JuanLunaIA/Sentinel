//! DRY_RUN executor — local fills at mark ± slippage, reports persisted.
//!
//! **Skeleton status (P05):** implemented by the P05 wave.

use std::path::PathBuf;

use sentinel_core::order::OrderRequest;

use super::{ExecutionReport, Executor, PositionProbe, SeqCounter};
use crate::error::Result;

/// Simulates fills at the current mark with a bps slippage penalty.
#[allow(dead_code)] // stub fields; consumed by the P05 wave
pub struct DryRunExecutor<P> {
    probe: P,
    slippage_bps: u16,
    report_path: PathBuf,
    seq: SeqCounter,
}

impl<P> DryRunExecutor<P> {
    /// Build the executor.
    pub fn new(_probe: P, _slippage_bps: u16, _report_path: PathBuf, _seq_seed: u64) -> Self {
        todo!("P05 agent exec")
    }
}

impl<P> Executor for DryRunExecutor<P>
where
    P: PositionProbe + Sync,
{
    async fn submit(&self, _order: &OrderRequest) -> Result<ExecutionReport> {
        todo!("P05 agent exec: simulate fill at mark ∓ slippage, persist JSONL")
    }
}
