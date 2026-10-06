//! Human-approved order execution (`SPEC-P11.md` §7).
//!
//! A concrete, guarded executor for the human path (the pipeline owns the
//! reflex path). Both variants share the same probe as the pipeline.
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

use sentinel_core::order::OrderRequest;

use crate::error::Result;
use crate::execution::dry_run::DryRunExecutor;
use crate::execution::perpl::PerplExecutor;
use crate::execution::{ExecutionReport, Executor, GuardedExecutor};
use crate::pipeline::StateProbe;

/// The human path's executor (mode-picked by `main.rs`).
pub enum HumanExecutor {
    /// DRY_RUN mode (guarded simulatio).
    Dry(GuardedExecutor<DryRunExecutor<StateProbe>, StateProbe>),
    /// TESTNET mode (guarded gateway orders).
    Perpl(GuardedExecutor<PerplExecutor<StateProbe>, StateProbe>),
}

impl Executor for HumanExecutor {
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let _ = order;
        todo!("P11 agent bot-handlers")
    }
}
