//! Sentinel breaker — independent dead-man's-switch guardian (SPEC-P14).
//! Stub skeleton: the P14 wave fills the modules.
pub mod armed;
pub mod config;
pub mod executor;
pub mod snapshot;
pub mod trigger;
pub mod watcher;

fn main() -> anyhow::Result<()> {
    tracing::warn!("STUB: breaker pending (P14 wave)");
    Ok(())
}
