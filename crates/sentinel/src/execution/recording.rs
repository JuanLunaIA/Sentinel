//! Recording executor — pass-through that persists request/report pairs.
//!
//! Used by tests and demos to capture exactly what the pipeline decided.
//!
//! **Skeleton status (P05):** implemented by the P05 wave.

use std::path::PathBuf;

use sentinel_core::order::OrderRequest;

use super::{ExecutionReport, Executor, SeqCounter};
use crate::error::Result;

/// Persists `{request, report}` JSONL lines; never touches the network.
#[allow(dead_code)] // stub fields; consumed by the P05 wave
pub struct RecordingExecutor {
    path: PathBuf,
    seq: SeqCounter,
}

impl RecordingExecutor {
    /// Build the recorder writing to `path`.
    pub fn new(_path: PathBuf, _seq_seed: u64) -> Self {
        todo!("P05 agent exec")
    }
}

impl Executor for RecordingExecutor {
    async fn submit(&self, _order: &OrderRequest) -> Result<ExecutionReport> {
        todo!("P05 agent exec: record + simulated report")
    }
}
