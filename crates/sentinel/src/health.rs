//! Minimal health surface — `GET /healthz` on `PORT` (default 8080).
//!
//! Frozen by `SPEC-P06.md` §3: JSON `{uptime_s, feed_age_s, mode, version}`;
//! `feed_age_s` is `null` until the first feed event. The pipeline touches
//! [`HealthState::touch_feed`] on every applied event; the dashboard (P15)
//! extends this server.
//!
//! **Skeleton status (P06):** interfaces frozen; implemented by the P06 wave.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use sentinel_core::types::ExecutionMode;

use crate::error::Result;

/// Shared health snapshot, updated lock-free by the pipeline.
#[derive(Debug)]
pub struct HealthState {
    /// Process start.
    pub started: Instant,
    /// Epoch-ms of the last applied feed event (0 = never).
    pub last_feed_ms: AtomicU64,
    /// Current execution mode.
    pub mode: ExecutionMode,
    /// Crate version (`env!("CARGO_PKG_VERSION")`).
    pub version: &'static str,
}

impl HealthState {
    /// New state for `mode`.
    pub fn new(mode: ExecutionMode) -> Self {
        Self {
            started: Instant::now(),
            last_feed_ms: AtomicU64::new(0),
            mode,
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Record a feed event at `epoch_ms`.
    pub fn touch_feed(&self, epoch_ms: u64) {
        self.last_feed_ms.store(epoch_ms, Ordering::Relaxed);
    }

    /// Render the `/healthz` JSON body (`now_ms` injected for tests).
    pub fn render(&self, now_ms: u64) -> serde_json::Value {
        let _ = now_ms;
        todo!("P06 agent health: render the /healthz JSON body")
    }
}

/// Build the axum app exposing `GET /healthz`.
pub fn router(state: Arc<HealthState>) -> axum::Router {
    let _ = state;
    todo!("P06 agent health: Router::new().route(\"/healthz\", get(...))")
}

/// Serve `router` on `0.0.0.0:port` until `shutdown` flips.
///
/// # Errors
/// `SentinelError::Internal` when the listener cannot bind or the server
/// fails while running.
pub async fn serve(
    state: Arc<HealthState>,
    port: u16,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let _ = (state, port, shutdown);
    todo!("P06 agent health: axum::serve + graceful shutdown")
}
