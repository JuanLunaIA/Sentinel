//! `test-nansen` — x402 smoke driver.
//!
//! `--check`: FREE live leg — unpaid POST to the profiler endpoint prints the
//! rail table (no key, no payment). Default: ONE paid call via the funded
//! x402 wallet (`--address <0x…>`, default = payer), printing cost, tx hash
//! and a Monad explorer link (PENDING-WALLET until the key is real).
//!
//! **Skeleton status (P09):** interface frozen; implemented by the P09 wave.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    todo!("P09 agent orchestrator: --check rail table + paid smoke")
}
