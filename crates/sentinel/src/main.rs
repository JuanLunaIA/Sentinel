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
use sentinel::bot::approvals::ApprovalQueue;
use sentinel::bot::execution::HumanExecutor;
use sentinel::bot::handlers::BotContext;
use sentinel::bot::policy_admin::{POLICY_OVERLAY_PATH, SharedPolicy};
use sentinel::brain::engine::StrategyEngine;
use sentinel::brain::providers::{KimiProvider, QwenProvider};
use sentinel::config::Config;
use sentinel::execution::GuardedExecutor;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::idempotency::IdempotencyStore;
use sentinel::health::HealthState;
use sentinel::nansen::SPEND_LEDGER_PATH;
use sentinel::nansen::spend::SpendLedger;
use sentinel::notify::{AlertSink, DedupeSink, TelegramSink, TracingSink};
use sentinel::perpl::{LivePerpl, MockPerpl};
use sentinel::pipeline::{
    LiveState, Pipeline, PipelineEvent, PipelineOutcome, RunMode, StateProbe,
};
use sentinel_core::audit::AuditJournal;
use sentinel_core::types::ExecutionMode;
use std::sync::atomic::AtomicBool;
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
    spawn_signal_forwarder(shutdown_tx.clone());

    let state = Arc::new(Mutex::new(LiveState::new()));
    let health = Arc::new(HealthState::new(cfg.execution.mode));

    // Audit journal (SPEC-P10): open/create data/audit; degrade loudly.
    let journal = match AuditJournal::open("data/audit") {
        Ok(journal) => {
            tracing::info!(
                next_seq = journal.seq(),
                dir = "data/audit",
                "audit journal open"
            );
            Some(Arc::new(Mutex::new(journal)))
        }
        Err(err) => {
            tracing::error!(error = %err, "audit journal unavailable; continuing without journaling");
            None
        }
    };

    // Policy overlay + kill switch (SPEC-P11 §6).
    let policy = Arc::new(SharedPolicy::load_with_defaults(
        POLICY_OVERLAY_PATH,
        (cfg.risk.soft_pct, cfg.risk.warn_pct, cfg.risk.hard_pct),
    ));
    let kill = Arc::new(AtomicBool::new(false));

    let token = cfg.telegram.token.expose().trim().to_string();
    let bot_enabled = !token.is_empty()
        && !token.contains("replace")
        && !cfg.telegram.allowed_user_ids.is_empty();
    if bot_enabled {
        let engine = if cfg.features.enable_strategy {
            Some(
                StrategyEngine::new(
                    QwenProvider::new(&cfg.qwen),
                    cfg.strategy.min_interval_secs,
                    cfg.strategy.confidence_floor,
                )
                .with_fallback(KimiProvider::new(&cfg.kimi))
                .with_budget(
                    cfg.strategy.max_consults_per_hour,
                    cfg.strategy.max_tokens_per_day,
                ),
            )
        } else {
            None
        };
        let human_executor = match build_human_executor(&cfg, &state).await {
            Ok(executor) => Some(executor),
            Err(err) => {
                tracing::warn!(error = %err, "human executor unavailable (bot orders disabled)");
                None
            }
        };
        let bot_ctx = BotContext {
            cfg: cfg.clone(),
            state: Arc::clone(&state),
            health: Arc::clone(&health),
            journal: journal.clone(),
            policy: Arc::clone(&policy),
            approvals: Arc::new(ApprovalQueue::new()),
            engine,
            executor: human_executor,
            spend_ledger: Some(SpendLedger::new(SPEND_LEDGER_PATH)),
            kill: Arc::clone(&kill),
            pause_pending: AtomicBool::new(false),
        };
        let bot_run = sentinel::bot::BotRunConfig {
            token: token.clone(),
            allowed_user_ids: cfg.telegram.allowed_user_ids.clone(),
            approval_chat_id: cfg.telegram.approval_chat_id,
            cfg: cfg.clone(),
        };
        let bot_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(err) = sentinel::bot::run(bot_run, bot_ctx, bot_shutdown).await {
                tracing::warn!(error = %err, "telegram bot stopped");
            }
        });
        tracing::info!("telegram bot spawned");
    } else {
        tracing::info!("telegram bot disabled (placeholder token or empty allowlist)");
    }

    match (cfg.telegram.approval_chat_id, token.is_empty()) {
        (Some(chat_id), false) => {
            tracing::info!(chat_id, "alerts: telegram + tracing");
            let sink = DedupeSink::new(TelegramSink::new(&token, chat_id));
            dispatch(
                cfg,
                cli,
                state,
                health,
                shutdown_rx,
                sink,
                journal,
                policy,
                kill,
            )
            .await
        }
        _ => {
            tracing::info!("alerts: tracing only (no telegram chat configured)");
            let sink = DedupeSink::new(TracingSink);
            dispatch(
                cfg,
                cli,
                state,
                health,
                shutdown_rx,
                sink,
                journal,
                policy,
                kill,
            )
            .await
        }
    }
}

/// Route to the concrete pipeline for (replay | dry-run | testnet).
#[allow(clippy::too_many_arguments)] // daemon entry points thread the shared handles
async fn dispatch<S>(
    cfg: Config,
    cli: Cli,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    policy: Arc<SharedPolicy>,
    kill: Arc<AtomicBool>,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    match cli.replay {
        Some(path) => {
            run_replay(
                cfg, &path, state, health, shutdown, sink, journal, policy, kill,
            )
            .await
        }
        None => match cfg.execution.mode {
            ExecutionMode::DryRun => {
                run_live_dry(cfg, state, health, shutdown, sink, journal, policy, kill).await
            }
            ExecutionMode::Testnet => {
                run_live_testnet(cfg, state, health, shutdown, sink, journal, policy, kill).await
            }
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
#[allow(clippy::too_many_arguments)]
async fn run_replay<S>(
    cfg: Config,
    path: &Path,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    policy: Arc<SharedPolicy>,
    kill: Arc<AtomicBool>,
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
    let pipeline = match journal {
        Some(journal) => pipeline.with_journal(journal),
        None => pipeline,
    };
    let pipeline = pipeline.with_policy(policy).with_kill_switch(kill);
    finish(pipeline.run(shutdown).await.context("pipeline run")?)
}

/// Live DRY_RUN: live feed, simulated guarded fills, health surface on.
#[allow(clippy::too_many_arguments)]
async fn run_live_dry<S>(
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    policy: Arc<SharedPolicy>,
    kill: Arc<AtomicBool>,
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
    spawn_health(Arc::clone(&health), journal.clone(), shutdown.clone());
    spawn_anchor(&cfg, journal.clone(), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    let pipeline = match journal {
        Some(journal) => pipeline.with_journal(journal),
        None => pipeline,
    };
    let pipeline = pipeline.with_policy(policy).with_kill_switch(kill);
    finish(pipeline.run(shutdown).await.context("pipeline run")?)
}

/// Live TESTNET: real reduce-only orders through the guarded gateway executor.
///
/// Requires the API key + exchange account (SETUP-MANUAL; STUB-09).
#[allow(clippy::too_many_arguments)]
async fn run_live_testnet<S>(
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    health: Arc<HealthState>,
    shutdown: watch::Receiver<bool>,
    sink: S,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    policy: Arc<SharedPolicy>,
    kill: Arc<AtomicBool>,
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
    spawn_health(Arc::clone(&health), journal.clone(), shutdown.clone());
    spawn_anchor(&cfg, journal.clone(), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    let pipeline = match journal {
        Some(journal) => pipeline.with_journal(journal),
        None => pipeline,
    };
    let pipeline = pipeline.with_policy(policy).with_kill_switch(kill);
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

/// Build the human-path executor for the active mode (best-effort: bot
/// orders need it, the daemon itself does not; failures disable bot orders).
async fn build_human_executor(
    cfg: &Config,
    state: &Arc<Mutex<LiveState>>,
) -> anyhow::Result<HumanExecutor> {
    let store = Arc::new(Mutex::new(
        IdempotencyStore::load(
            Duration::from_secs(cfg.risk.idempotency_window_secs),
            PathBuf::from("data/idempotency.json"),
        )
        .context("load idempotency store")?,
    ));
    match cfg.execution.mode {
        ExecutionMode::DryRun => Ok(HumanExecutor::Dry(GuardedExecutor::new(
            DryRunExecutor::new(
                StateProbe::new(Arc::clone(state)),
                10,
                PathBuf::from("data/human-dryrun-reports.jsonl"),
                0,
            ),
            Arc::clone(&store),
            StateProbe::new(Arc::clone(state)),
            Duration::from_secs(3),
        ))),
        ExecutionMode::Testnet => {
            use sentinel::execution::perpl::PerplExecutor;
            use sentinel::perpl::auth::ApiKeySigner;

            let feed = LivePerpl::new(cfg).context("build live feed")?;
            let account_id = feed
                .primary_account_id()
                .await
                .context("wallet account id")?;
            let signer = ApiKeySigner::from_config(&cfg.perpl).context("build signer")?;
            Ok(HumanExecutor::Perpl(GuardedExecutor::new(
                PerplExecutor::new(
                    cfg.perpl.api_url.clone(),
                    signer,
                    account_id,
                    StateProbe::new(Arc::clone(state)),
                    0,
                )
                .context("build perpl executor")?,
                Arc::clone(&store),
                StateProbe::new(Arc::clone(state)),
                Duration::from_secs(5),
            )))
        }
        ExecutionMode::Mainnet => anyhow::bail!("mainnet human executor not wired"),
    }
}

/// Spawn the health + audit API surface (live modes only).
fn spawn_health(
    health: Arc<HealthState>,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    shutdown: watch::Receiver<bool>,
) {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let app = match journal {
        Some(journal) => sentinel::health::router(Arc::clone(&health))
            .merge(sentinel::api::audit_router(journal)),
        None => sentinel::health::router(health),
    };
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!(error = %err, "health server bind failed");
                return;
            }
        };
        tracing::info!(port, "health + audit API on /healthz, /api/audit");
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut shutdown = shutdown;
                let _ = shutdown.changed().await;
            })
            .await
        {
            tracing::warn!(error = %err, "health server stopped");
        }
    });
}

/// Spawn the on-chain anchor service when configured (`ENABLE_ANCHOR`,
/// contract address, signer key; otherwise PENDING-WALLET note).
fn spawn_anchor(
    cfg: &Config,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    shutdown: watch::Receiver<bool>,
) {
    if !cfg.features.enable_anchor {
        tracing::info!("anchor disabled (ENABLE_ANCHOR=false)");
        return;
    }
    let Some(journal) = journal else {
        tracing::warn!("anchor skipped: no audit journal");
        return;
    };
    if cfg.anchor.contract_address.is_none() || cfg.anchor.rpc_signer_key.is_none() {
        tracing::warn!(
            "anchor skipped: ANCHOR_CONTRACT_ADDRESS / RPC_SIGNER_KEY not configured \
             (PENDING-WALLET, see STUBS.md)"
        );
        return;
    }
    match sentinel::anchor::AlloyAnchorSink::new(cfg) {
        Ok(sink) => {
            let cfg = cfg.clone();
            tokio::spawn(async move {
                match sentinel::anchor::run(&cfg, journal, sink, shutdown).await {
                    Ok(report) => tracing::info!(?report, "anchor service finished"),
                    Err(err) => tracing::warn!(error = %err, "anchor service stopped"),
                }
            });
        }
        Err(err) => tracing::warn!(error = %err, "anchor service unavailable"),
    }
}
