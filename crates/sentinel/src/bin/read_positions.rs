//! `read-positions` — first live proof (P03): prints the market context, the
//! wallet account and open positions from the configured Perpl environment as
//! pretty JSON.
//!
//! Requires a real testnet API key (`PERPL_API_KEY` / `PERPL_API_KEY_SECRET`)
//! and an exchange account with order forwarding enabled — see
//! `docs/SETUP-MANUAL.md` (steps 2-3). Acceptance evidence for P03 compares
//! this output against the Perpl UI (liq price / collateral).

use anyhow::Context;
use sentinel::config::Config;
use sentinel::perpl::{LivePerpl, PerplFeed};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load().context("load configuration")?;
    let feed = LivePerpl::new(&cfg).context("build live Perpl feed")?;

    let markets = feed.context().await.context("fetch market context")?;
    let snapshot = feed.snapshot().await.context(
        "fetch account snapshot (needs a testnet exchange account — docs/SETUP-MANUAL.md)",
    )?;

    let output = serde_json::json!({
        "perpl_env": cfg.perpl.env_name.to_string(),
        "chain_id": cfg.perpl.chain_id,
        "markets": markets,
        "account": snapshot,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
