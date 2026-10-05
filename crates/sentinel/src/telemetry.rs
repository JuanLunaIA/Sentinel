//! Telemetry bootstrap.
//!
//! Installs two `tracing` layers: structured stdout (pretty or JSON,
//! controlled by `LOG_FORMAT`) and a **daily-rolling file layer** under
//! `LOG_DIR`. The returned [`WorkerGuard`] must be held for the process
//! lifetime — dropping it flushes and stops the writer. The guard is
//! returned (never `mem::forget`-ed) so shutdown is deterministic.

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::{Config, LogFormat};

/// Initialize global tracing subscribers and return the file-writer guard.
///
/// # Errors
/// Fails when `RUST_LOG` is invalid, the log directory cannot be created, the
/// rolling appender cannot be built, or a global subscriber is already set.
pub fn init(cfg: &Config) -> anyhow::Result<WorkerGuard> {
    let obs = &cfg.observability;

    let filter = EnvFilter::try_new(&obs.rust_log)
        .map_err(|e| anyhow::anyhow!("invalid RUST_LOG ({}): {e}", obs.rust_log))?;

    std::fs::create_dir_all(&obs.log_dir)
        .with_context(|| format!("create log dir {}", obs.log_dir.display()))?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("sentinel")
        .filename_suffix("log")
        .build(&obs.log_dir)
        .context("build rolling file appender")?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let registry = tracing_subscriber::registry().with(filter);

    let installed = match obs.log_format {
        LogFormat::Pretty => {
            let stdout_layer = fmt::layer().with_target(true);
            let file_layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(file_writer);
            registry.with(stdout_layer).with(file_layer).try_init()
        }
        LogFormat::Json => {
            let stdout_layer = fmt::layer().json();
            let file_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(file_writer);
            registry.with(stdout_layer).with(file_layer).try_init()
        }
    };

    installed.map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;
    Ok(guard)
}
