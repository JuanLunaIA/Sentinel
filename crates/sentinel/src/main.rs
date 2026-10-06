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
use sentinel::consult::{ApprovedOrder, ConsultTask, ConsultTrigger, DEFAULT_REVIEW_INTERVAL_SECS};
use sentinel::execution::GuardedExecutor;
use sentinel::execution::dry_run::DryRunExecutor;
use sentinel::execution::idempotency::IdempotencyStore;
use sentinel::health::HealthState;
use sentinel::nansen::NansenClient;
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
use tokio::sync::{Mutex, mpsc, watch};

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

    // ---- Strategy engine (SPEC-P20 §2): built ONCE and shared by the bot
    // and the consult task; `None` (feature off / placeholder key) keeps
    // every pre-P20 path.
    let strategy_engine = build_strategy_engine(&cfg);

    // ---- Consult channels (SPEC-P20 §2–§3): triggers flow pipeline ->
    // consult task, approved orders flow back. Created only when an engine
    // exists; otherwise the pipeline is built with `None` channels and stays
    // byte-identical.
    let (consult_tx, consult_rx, orders_tx, strategy_rx) = match &strategy_engine {
        Some(_) => {
            let (consult_tx, consult_rx) = mpsc::channel(CONSULT_QUEUE_CAPACITY);
            let (orders_tx, strategy_rx) = mpsc::channel(CONSULT_QUEUE_CAPACITY);
            (
                Some(consult_tx),
                Some(consult_rx),
                Some(orders_tx),
                Some(strategy_rx),
            )
        }
        None => (None, None, None, None),
    };
    let review_interval_secs = review_interval_secs();
    let nansen = build_nansen_client(&cfg, strategy_engine.is_some());

    let token = cfg.telegram.token.expose().trim().to_string();
    let bot_enabled = !token.is_empty()
        && !token.contains("replace")
        && !cfg.telegram.allowed_user_ids.is_empty();
    if bot_enabled {
        // The approval queue survives bot restarts: created once, shared with
        // every attempt (P16 supervision rebuilds the rest per attempt).
        let bot_approvals = Arc::new(ApprovalQueue::new());
        let bot_watch = shutdown_tx.subscribe();
        let bot_cfg = cfg.clone();
        let bot_state = Arc::clone(&state);
        let bot_health = Arc::clone(&health);
        let bot_journal = journal.clone();
        let bot_policy = Arc::clone(&policy);
        let bot_kill = Arc::clone(&kill);
        let bot_token = token.clone();
        let bot_engine = strategy_engine.clone();
        // Supervised (SPEC-P16 §2): a bot panic/exit is caught, logged with the
        // task name + restart counter, and retried with backoff; the context is
        // rebuilt per attempt because the executors/engines are not `Clone`.
        sentinel::supervisor::spawn("telegram-bot", bot_watch.clone(), move || {
            let cfg = bot_cfg.clone();
            let state = Arc::clone(&bot_state);
            let health = Arc::clone(&bot_health);
            let journal = bot_journal.clone();
            let policy = Arc::clone(&bot_policy);
            let kill = Arc::clone(&bot_kill);
            let approvals = Arc::clone(&bot_approvals);
            let token = bot_token.clone();
            let bot_shutdown = bot_watch.clone();
            // The shared strategy engine (built once in `main`, P20 §2);
            // cloned per attempt outside the async block so the supervisor's
            // `FnMut` closure can rebuild the attempt future.
            let engine = bot_engine.clone();
            async move {
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
                    approvals,
                    engine,
                    executor: human_executor,
                    spend_ledger: Some(SpendLedger::new(SPEND_LEDGER_PATH)),
                    kill: Arc::clone(&kill),
                    pause_pending: AtomicBool::new(false),
                };
                let bot_run = sentinel::bot::BotRunConfig {
                    token,
                    allowed_user_ids: cfg.telegram.allowed_user_ids.clone(),
                    approval_chat_id: cfg.telegram.approval_chat_id,
                    cfg: cfg.clone(),
                };
                if let Err(err) = sentinel::bot::run(bot_run, bot_ctx, bot_shutdown).await {
                    tracing::warn!(error = %err, "telegram bot stopped");
                }
            }
        });
        tracing::info!("telegram bot spawned");
    } else {
        tracing::info!("telegram bot disabled (placeholder token or empty allowlist)");
    }

    match (cfg.telegram.approval_chat_id, token.is_empty()) {
        (Some(chat_id), false) => {
            tracing::info!(chat_id, "alerts: telegram + tracing");
            let alert_token = token.clone();
            spawn_consult(
                strategy_engine.clone(),
                cfg.clone(),
                Arc::clone(&state),
                journal.clone(),
                Arc::clone(&policy),
                consult_rx,
                orders_tx,
                nansen.clone(),
                review_interval_secs,
                shutdown_tx.subscribe(),
                move || DedupeSink::new(TelegramSink::new(&alert_token, chat_id)),
            );
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
                ConsultChannels {
                    consult_tx,
                    strategy_rx,
                },
            )
            .await
        }
        _ => {
            tracing::info!("alerts: tracing only (no telegram chat configured)");
            spawn_consult(
                strategy_engine.clone(),
                cfg.clone(),
                Arc::clone(&state),
                journal.clone(),
                Arc::clone(&policy),
                consult_rx,
                orders_tx,
                nansen.clone(),
                review_interval_secs,
                shutdown_tx.subscribe(),
                || DedupeSink::new(TracingSink),
            );
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
                ConsultChannels {
                    consult_tx,
                    strategy_rx,
                },
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
    consult: ConsultChannels,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    match cli.replay {
        Some(path) => {
            run_replay(
                cfg, &path, state, health, shutdown, sink, journal, policy, kill, consult,
            )
            .await
        }
        None => match cfg.execution.mode {
            ExecutionMode::DryRun => {
                run_live_dry(
                    cfg, state, health, shutdown, sink, journal, policy, kill, consult,
                )
                .await
            }
            ExecutionMode::Testnet => {
                run_live_testnet(
                    cfg, state, health, shutdown, sink, journal, policy, kill, consult,
                )
                .await
            }
            ExecutionMode::Mainnet => {
                anyhow::bail!(
                    "mainnet is not wired yet (testnet-first per P00); refusing to start in MAINNET mode"
                )
            }
        },
    }
}

/// P20 channels threaded from `main` into every run mode: the trigger sender
/// toward the consult task and the approved-order receiver for the pipeline
/// (both `None` without a strategy engine — the pre-P20 path).
struct ConsultChannels {
    /// Trigger sender toward the consult task.
    consult_tx: Option<mpsc::Sender<ConsultTrigger>>,
    /// Approved-order receiver for the pipeline.
    strategy_rx: Option<mpsc::Receiver<ApprovedOrder>>,
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
    consult: ConsultChannels,
) -> anyhow::Result<()>
where
    S: AlertSink + Sync,
{
    let feed = MockPerpl::from_fixture(path).context("load replay fixture")?;
    // P14: replay demos keep heartbeats live for the dead-man's switch
    // (self-gated: no-op unless ENABLE_ANCHOR + address + signer key exist).
    spawn_anchor(&cfg, journal.clone(), shutdown.clone());
    spawn_health(
        Arc::clone(&health),
        journal.clone(),
        Arc::clone(&kill),
        Arc::clone(&state),
        shutdown.clone(),
    );
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
    let pipeline = pipeline
        .with_policy(policy)
        .with_kill_switch(kill)
        .with_consult_tx(consult.consult_tx)
        .with_strategy_rx(consult.strategy_rx);
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
    consult: ConsultChannels,
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
    spawn_health(
        Arc::clone(&health),
        journal.clone(),
        Arc::clone(&kill),
        Arc::clone(&state),
        shutdown.clone(),
    );
    spawn_anchor(&cfg, journal.clone(), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    let pipeline = match journal {
        Some(journal) => pipeline.with_journal(journal),
        None => pipeline,
    };
    let pipeline = pipeline
        .with_policy(policy)
        .with_kill_switch(kill)
        .with_consult_tx(consult.consult_tx)
        .with_strategy_rx(consult.strategy_rx);
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
    consult: ConsultChannels,
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
    spawn_health(
        Arc::clone(&health),
        journal.clone(),
        Arc::clone(&kill),
        Arc::clone(&state),
        shutdown.clone(),
    );
    spawn_anchor(&cfg, journal.clone(), shutdown.clone());
    let pipeline = Pipeline::new(cfg, feed, executor, sink, state, health, RunMode::Live);
    let pipeline = match journal {
        Some(journal) => pipeline.with_journal(journal),
        None => pipeline,
    };
    let pipeline = pipeline
        .with_policy(policy)
        .with_kill_switch(kill)
        .with_consult_tx(consult.consult_tx)
        .with_strategy_rx(consult.strategy_rx);
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

/// SIGINT/SIGTERM → shutdown watch (supervised: signal-handler registration is
/// retried if it ever fails; once the flag is raised the forwarder stops).
fn spawn_signal_forwarder(shutdown_tx: watch::Sender<bool>) {
    let watch = shutdown_tx.subscribe();
    sentinel::supervisor::spawn("signal-forwarder", watch, move || {
        let shutdown_tx = shutdown_tx.clone();
        async move {
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
        }
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
    kill: Arc<AtomicBool>,
    state: Arc<Mutex<LiveState>>,
    shutdown: watch::Receiver<bool>,
) {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let dashboard = sentinel::api::DashboardState {
        health: Arc::clone(&health),
        journal: journal.clone(),
        kill,
        live_state: Some(state),
        paths: sentinel::api::DashboardPaths::default(),
    };
    let app = sentinel::health::router(health).merge(sentinel::api::dashboard_router(dashboard));
    // Supervised (SPEC-P16 §2): an early exit (e.g. a transient bind failure)
    // is retried with backoff; the graceful drain on shutdown stops the loop.
    sentinel::supervisor::spawn("health-api", shutdown.clone(), move || {
        let app = app.clone();
        let serve_shutdown = shutdown.clone();
        async move {
            let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::warn!(error = %err, "health server bind failed");
                    return;
                }
            };
            tracing::info!(port, "health + audit API on /healthz, /api/audit");
            if let Err(err) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let mut serve_shutdown = serve_shutdown;
                if !*serve_shutdown.borrow_and_update() {
                    let _ = serve_shutdown.changed().await;
                }
            })
            .await
            {
                tracing::warn!(error = %err, "health server stopped");
            }
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
    // Pre-flight construction: a bad configuration is skipped loudly (as
    // before P16); the sink itself is rebuilt per attempt below because it is
    // consumed by `anchor::run` and is not `Clone`.
    if let Err(err) = sentinel::anchor::AlloyAnchorSink::new(cfg) {
        tracing::warn!(error = %err, "anchor service unavailable");
        return;
    }
    let cfg = cfg.clone();
    let watch = shutdown.clone();
    // Supervised (SPEC-P16 §2): an anchor stop/error is caught, logged and
    // retried with backoff; the shutdown drain stops the loop.
    sentinel::supervisor::spawn("anchor", watch, move || {
        let cfg = cfg.clone();
        let journal = Arc::clone(&journal);
        let run_shutdown = shutdown.clone();
        async move {
            match sentinel::anchor::AlloyAnchorSink::new(&cfg) {
                Ok(sink) => match sentinel::anchor::run(&cfg, journal, sink, run_shutdown).await {
                    Ok(report) => tracing::info!(?report, "anchor service finished"),
                    Err(err) => {
                        tracing::warn!(error = %err, "anchor service stopped");
                    }
                },
                Err(err) => tracing::warn!(error = %err, "anchor service unavailable"),
            }
        }
    });
}

/// Consult channel capacity (triggers / approved orders each).
pub const CONSULT_QUEUE_CAPACITY: usize = 32;

/// Build the strategy engine ONCE (SPEC-P20 §2): active only when
/// `ENABLE_STRATEGY` is set AND the Qwen key looks real. `None` keeps every
/// pre-P20 path (bot `/risk` degrades honestly, no consult task is spawned,
/// the pipeline runs unchanged).
fn build_strategy_engine(cfg: &Config) -> Option<Arc<StrategyEngine<QwenProvider, KimiProvider>>> {
    if !cfg.features.enable_strategy {
        tracing::info!("strategy engine disabled (ENABLE_STRATEGY=false)");
        return None;
    }
    if !provider_key_present(cfg.qwen.api_key.expose()) {
        tracing::warn!(
            "strategy engine disabled: QWEN_API_KEY is missing or still a placeholder \
             (see SETUP-MANUAL)"
        );
        return None;
    }
    Some(Arc::new(
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
    ))
}

/// True when an API key looks real (non-empty, no placeholder markers).
fn provider_key_present(key: &str) -> bool {
    let value = key.trim();
    if value.is_empty() {
        return false;
    }
    let lowered = value.to_ascii_lowercase();
    !(lowered.contains("replace-me")
        || lowered.contains("replace_me")
        || lowered.contains("placeholder")
        || lowered.contains("your_")
        || value.starts_with('<'))
}

/// `STRATEGY_REVIEW_INTERVAL_SECS` (default
/// [`DEFAULT_REVIEW_INTERVAL_SECS`], `SPEC-P20` §2c).
fn review_interval_secs() -> u64 {
    std::env::var("STRATEGY_REVIEW_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_REVIEW_INTERVAL_SECS)
}

/// Build the cache-first Nansen client for the consult task (best-effort):
/// `None` when the feature is off, there is no engine to serve, or the payer
/// key is missing/shape-invalid — the consult task then degrades to
/// `SmartMoneyContext::unavailable`.
fn build_nansen_client(cfg: &Config, engine_active: bool) -> Option<Arc<NansenClient>> {
    if !engine_active {
        return None;
    }
    if !cfg.features.enable_nansen {
        tracing::info!("nansen disabled (ENABLE_NANSEN=false)");
        return None;
    }
    match NansenClient::new(&cfg.nansen) {
        Ok(client) => {
            tracing::info!(payer = %client.address(), "nansen x402 client ready");
            Some(Arc::new(client))
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "nansen client unavailable (check NANSEN_PAYER_KEY); smart-money context disabled"
            );
            None
        }
    }
}

/// Spawn the supervised consult task (SPEC-P20 §2) in every run mode, active
/// only when a strategy engine exists; the sink factory rebuilds the alert
/// destination per attempt (the daemon sinks are not `Clone`).
#[allow(clippy::too_many_arguments)] // daemon wiring is explicit by design (SPEC-P20 §2)
fn spawn_consult<S, M>(
    engine: Option<Arc<StrategyEngine<QwenProvider, KimiProvider>>>,
    cfg: Config,
    state: Arc<Mutex<LiveState>>,
    journal: Option<Arc<Mutex<AuditJournal>>>,
    policy: Arc<SharedPolicy>,
    triggers: Option<mpsc::Receiver<ConsultTrigger>>,
    orders: Option<mpsc::Sender<ApprovedOrder>>,
    nansen: Option<Arc<NansenClient>>,
    review_interval_secs: u64,
    shutdown: watch::Receiver<bool>,
    make_sink: M,
) where
    M: Fn() -> S + Send + 'static,
    S: AlertSink + Send + Sync + 'static,
{
    let (Some(_), Some(triggers), Some(orders)) = (engine.as_ref(), triggers, orders) else {
        tracing::info!("consult task disabled (no strategy engine)");
        return;
    };
    let triggers = Arc::new(Mutex::new(triggers));
    sentinel::supervisor::spawn("consult", shutdown.clone(), move || {
        let engine = engine.clone();
        let cfg = cfg.clone();
        let state = Arc::clone(&state);
        let journal = journal.clone();
        let policy = Arc::clone(&policy);
        let triggers = Arc::clone(&triggers);
        let orders = orders.clone();
        let nansen = nansen.clone();
        let sink = make_sink();
        let run_shutdown = shutdown.clone();
        async move {
            let task = ConsultTask::new(
                engine,
                cfg,
                state,
                journal,
                policy,
                triggers,
                orders,
                sink,
                nansen,
                review_interval_secs,
            );
            task.run(run_shutdown).await;
        }
    });
    tracing::info!(review_interval_secs, "strategy consult task spawned");
}
