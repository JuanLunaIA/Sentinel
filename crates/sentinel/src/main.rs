//! Sentinel daemon — supervised pipeline entry point (P06).
//!
//! `--mode dry-run|testnet|mainnet` overrides `EXECUTION_MODE`;
//! `--replay <fixture.jsonl>` runs the deterministic golden path offline
//! (DRY_RUN executor, `MockPerpl` feed, logical clock). Health surface:
//! `GET /healthz` on `PORT` (default 8080) in live modes; replay runs it out
//! ([`SPEC-P06.md`] §3 — determinism without ports). SIGINT/SIGTERM drain the
//! pipeline and flush telemetry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sentinel::args::Cli;
use sentinel::config::Config;
use sentinel::execution::GuardedExecutor;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::idempotency::IdempotencyStore;
use sentinel::health::HealthState;
use sentinel::notify::{AlertSink, DedupeSink, TelegramSink, TracingSink};
use sentinel::perpl::{LivePerpl, MockPerpl};
use sentinel::pipeline::{
    LiveState, Pipeline, PipelineEvent, PipelineOutcome, RunMode, StateProbe,
};
use sentinel_core::types::ExecutionMode;
use tokio::sync::{Mutex, watch};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse(std::env::args().skip(1)).map_err(anyhow::Error::msg)?;
    let mut cfg = Config::load().context("load configuration")?;
    if let Some(mode) = cli.mode {
        cfg.execution.mode = mode;
        cfg.validate().context("validate mode override")?;
    }
    let _guard = sentinel::telemetry::init(&cfg).context("init telemetry")?;

    tracing::info!(
        mode = %cfg.execution.mode,
        replay = cli.replay.is_some(),
        perpl_env = %cfg.perpl.env_name,
        chain_id = cfg.perpl.chain_id,
        version = env!("CARGO_PKG_VERSION"),
        "sentinel starting"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_signal_forwarder(shutdown_tx);

    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));

    let token = cfg.telegram.token.expose().trim().to_string();
    match (cfg.telegram.approval_chat_id, token.is_empty()) {
        (Some(chat_id), false) => {
            tracing::info!(chat_id, "alerts: telegram + tracing");
            let sink = DedupeSink::new(TelegramSink::new(&token, chat_id));
            dispatch(cfg, cli, state, health, shutdown_rx, sink).await
        }
        _ => {
            tracing::info!("alerts: tracing only (no telegram chat configured)");
            let sink = DedupeSink::new(TracingSink);
            dispatch(cfg, cli, state, health, shutdown_rx, sink).await
        }
    }
}

/// Route to the concrete pipeline for (replay | dry-run | testnet).
async fn dispatch<S>(
    cfg: Config,
    cli: Cli,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    match cli.replay {
        Some(path) => run_replay(cfg, &path, state, health, shutdown, sink).await,
        None => match cfg.execution.mode {
            ExecutionMode::DryRun => run_live_dry(cfg, state, health, shutdown, sink).await,
            ExecutionMode::Testnet => run_live_testnet(cfg, state, health, shutdown, sink).await,
            ExecutionMode::Mainnet => {
                anyhow::bail!(
                    "mainnet is not wired yet (testnet-first per P00); refusing to start in MAINNET mode"
                )
            }
        },
    }
}

/// Deterministic replay: `MockPerpl` fixture + unguarded DRY_RUN executor
/// (nothing hits a venue, and the guard's post-verify cannot confirm fills in
/// a fixture — `SPEC-P06.md` §4).
async fn run_replay<S>(
    cfg: Config,
    path: &Path,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    let feed = MockPerpl::from_fixture(path).context("load replay fixture")?;
    let executor = DryRunExecutor::new(
        StateProbe::new(Arc::clone(&state)),
        10,
        PathBuf::from("data/dryrun-reports.jsonl"),
        0,
    );
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Replay);
    finish(pipeline.run(shutdown).await.context("pipeline run")?)
}

/// Live DRY_RUN: live feed, simulated guarded fills, health surface on.
async fn run_live_dry<S>(
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    let feed = LivePerpl::new(&cfg).context("build live feed")?;
    let store = Arc::new(Mutex::new(
        IdempotencyStore::load(
            Duration::from_secs(cfg.risk.idempotency_window_secs),
            PathBuf::from("data/idempotency.json"),
        )
        .context("load idempotency store")?,
    ));
    let executor = GuardedExecutor::new(
        DryRunExecutor::new(
            StateProbe::new(Arc::clone(&state)),
            10,
            PathBuf::from("data/dryrun-reports.jsonl"),
            0,
        ),
        Arc::clone(&store),
        StateProbe::new(Arc::clone(&state)),
        Duration::from_secs(3),
    );
    spawn_health(Arc::clone(&health), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    finish(pipeline.run(shutdown).await.context("pipeline run")?)
}

/// Live TESTNET: real reduce-only orders through the guarded gateway executor.
///
/// Requires the API key + exchange account (SETUP-MANUAL; STUB-09).
async fn run_live_testnet<S>(
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    use sentinel::execution::perpl::PerplExecutor;
    use sentinel::perpl::auth::ApiKeySigner;

    let feed = LivePerpl::new(&cfg).context("build live feed")?;
    let account_id = feed
        .primary_account_id()
        .await
        .context("wallet account id (needs testnet key + exchange account)")?;
    let signer = ApiKeySigner::from_config(&cfg.perpl).context("build signer")?;
    let store = Arc::new(Mutex::new(
        IdempotencyStore::load(
            Duration::from_secs(cfg.risk.idempotency_window_secs),
            PathBuf::from("data/idempotency.json"),
        )
        .context("load idempotency store")?,
    ));
    let executor = GuardedExecutor::new(
        PerplExecutor::new(
            cfg.perpl.api_url.clone(),
            signer,
            account_id,
            StateProbe::new(Arc::clone(&state)),
            0,
        )
        .context("build perpl executor")?,
        Arc::clone(&store),
        StateProbe::new(Arc::clone(&state)),
        Duration::from_secs(5),
    );
    spawn_health(Arc::clone(&health), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    finish(pipeline.run(shutdown).await.context("pipeline run")?)
}

/// Summary log for a finished run.
fn finish(outcome: PipelineOutcome) -> anyhow::Result<()> {
    let mut executed = 0usize;
    let mut alerts = 0usize;
    let mut denied = 0usize;
    for event in &outcome.events {
        match event {
            PipelineEvent::Executed { .. } => executed += 1,
            PipelineEvent::Alert { .. } => alerts += 1,
            PipelineEvent::Decision { verdict, .. } if verdict.starts_with("deny") => denied += 1,
            _ => {}
        }
    }
    tracing::info!(
        events = outcome.events.len(),
        executed,
        alerts,
        denied,
        "pipeline finished"
    );
    Ok(())
}

/// SIGINT/SIGTERM → shutdown watch.
fn spawn_signal_forwarder(shutdown_tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
            return;
        };
        let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
            return;
        };
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
        tracing::info!("shutdown signal received; draining");
        let _ = shutdown_tx.send(true);
    });
}

/// Spawn the health surface (live modes only).
fn spawn_health(health: Arc<HealthState>, shutdown: watch::Receiver<bool>) {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    tokio::spawn(async move {
        tracing::info!(port, "health surface on /healthz");
        if let Err(err) = sentinel::health::serve(health, port, shutdown).await {
            tracing::warn!(error = %err, "health server stopped");
        }
    });
}
