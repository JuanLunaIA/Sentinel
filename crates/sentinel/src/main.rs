//! Sentinel daemon entry point (P02 skeleton).
//!
//! Loads configuration, installs telemetry, announces startup and exits 0.
//! Later prompts wire the supervised pipeline (feed → reflex → policy →
//! executor, strategy brain, bot, API) into this binary.

use anyhow::Context;
use sentinel_core::types::ExecutionMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = sentinel::config::Config::load().context("load configuration")?;
    let _guard = sentinel::telemetry::init(&cfg).context("init telemetry")?;

    tracing::info!(
        mode = %cfg.execution.mode,
        dry_run = cfg.execution.mode == ExecutionMode::DryRun,
        perpl_env = %cfg.perpl.env_name,
        chain_id = cfg.perpl.chain_id,
        reflex = cfg.features.enable_reflex,
        strategy = cfg.features.enable_strategy,
        "sentinel initialized"
    );

    Ok(())
}
