//! Sentinel breaker — binary entry point (`SPEC-P14`).
//!
//! Wires the parts together and owns shutdown:
//!
//! 1. [`HeartbeatWatcher`] polls the anchor contract's `Heartbeat` events;
//! 2. the trigger layer fires `stale AND critical` guardians on two paths —
//!    the breaker's own 5 s automatic ticker ([`run_auto`], the §7 demo
//!    behaviour) and the armed `POST /breaker/trigger` surface (the CRE
//!    workflow, §6). Both share one persisted one-fire-per-epoch gate;
//! 3. the executor plans and performs the defensive reduce (dry-run or the
//!    documented testnet last-resort path), journaling and alerting.
//!
//! The watcher's **first poll completes before the HTTP surface starts
//! serving**, so a status read never races the initial `eth_getLogs` pass.
//! Shutdown: SIGINT or SIGTERM flips the watch channel; the watcher and
//! trigger loops exit at the next tick, the HTTP server drains in-flight
//! requests, and every task is joined before exit.

use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::watch;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use breaker::armed::{ArmedState, serve};
use breaker::config::BreakerConfig;
use breaker::executor::BreakerExecutor;
use breaker::trigger::{DEFAULT_AUTO_CHECK_INTERVAL, FireStore, run_auto};
use breaker::watcher::{DEFAULT_LOOKBACK_BLOCKS, HeartbeatWatcher, WatcherState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = Arc::new(BreakerConfig::from_env().context("breaker configuration was rejected")?);
    info!(
        mode = cfg.mode.as_str(),
        anchor = %cfg.anchor_address,
        guardians = cfg.guardians.len(),
        interval_secs = cfg.heartbeat_interval_secs,
        stale_mult = cfg.stale_mult,
        port = cfg.port,
        "breaker: starting"
    );

    let watched = Arc::new(tokio::sync::RwLock::new(WatcherState::new()));
    let fires = Arc::new(tokio::sync::Mutex::new(
        FireStore::load(cfg.state_file.clone()).with_context(|| {
            format!(
                "cannot load BREAKER_STATE_FILE {}",
                cfg.state_file.display()
            )
        })?,
    ));
    let executor = Arc::new(BreakerExecutor::new(Arc::clone(&cfg)));

    let watcher = HeartbeatWatcher::new(&cfg.rpc_url, cfg.anchor_address, DEFAULT_LOOKBACK_BLOCKS)
        .map_err(|err| anyhow::anyhow!("cannot build the heartbeat watcher: {err}"))?;

    // First poll before serving: the status endpoint must never report a
    // pre-observation state (e.g. a configured guardian as "never seen")
    // while the initial `eth_getLogs` pass is still in flight.
    match watcher.poll_once(&watched).await {
        Ok(applied) => info!(applied, "breaker: first heartbeat poll completed"),
        Err(err) => warn!(
            error = %err,
            "breaker: first heartbeat poll failed; serving with the empty state"
        ),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The watch loop keeps the per-guardian heartbeat table current.
    let watcher_task = {
        let watched = Arc::clone(&watched);
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move { watcher.run(watched, shutdown).await })
    };

    // The automatic trigger path (SPEC-P14 §3/§7): one local pass every 5 s,
    // sharing the persisted epoch gate with the armed POST path.
    let trigger_task = tokio::spawn(run_auto(
        Arc::clone(&cfg),
        Arc::clone(&watched),
        Arc::clone(&fires),
        Arc::clone(&executor),
        DEFAULT_AUTO_CHECK_INTERVAL,
        shutdown_rx.clone(),
    ));

    // Armed HTTP surface (HMAC-protected; POST is the frozen trigger path
    // and dispatches into executor).
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port))
        .await
        .with_context(|| format!("cannot bind BREAKER_PORT {}", cfg.port))?;
    let bound_port = listener
        .local_addr()
        .context("cannot read the bound port")?
        .port();
    info!(port = bound_port, "breaker: armed HTTP surface listening");
    let state = ArmedState::new(
        Arc::clone(&cfg),
        Arc::clone(&watched),
        Arc::clone(&fires),
        Arc::clone(&executor),
    );
    let server_task = {
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move { serve(listener, state, shutdown).await })
    };

    // Wait for SIGINT / SIGTERM, then stop everything.
    wait_for_shutdown_signal().await;
    info!("breaker: shutdown signal received");
    let _ = shutdown_tx.send(true);

    if let Err(err) = watcher_task.await {
        warn!(error = %err, "breaker: watcher task failed");
    }
    if let Err(err) = trigger_task.await {
        warn!(error = %err, "breaker: trigger task failed");
    }
    match server_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(error = %err, "breaker: HTTP server stopped with an error"),
        Err(err) => warn!(error = %err, "breaker: HTTP server task failed"),
    }
    info!("breaker: stopped");
    Ok(())
}

/// Initialize `tracing_subscriber` (JSON-capable; `RUST_LOG` overrides the
/// default `info` filter).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Resolve on SIGINT (Ctrl-C) or SIGTERM.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("install the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
