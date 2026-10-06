//! Minimal health surface — `GET /healthz` on `PORT` (default 8080).
//!
//! Frozen by `SPEC-P06.md` §3: JSON `{uptime_s, feed_age_s, mode, version}`;
//! `feed_age_s` is `null` until the first feed event. The pipeline touches
//! [`HealthState::touch_feed`] on every applied event; the dashboard (P15)
//! extends this server.
//!
//! **P06 status:** implemented; interfaces frozen per `SPEC-P06.md` §3.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sentinel_core::types::ExecutionMode;

use crate::error::{Result, SentinelError};

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
    ///
    /// `feed_age_s` is `null` while no feed event has been recorded, else
    /// `(now_ms − last_feed_ms) / 1000`, saturating at zero on a backwards
    /// clock. `mode` renders through [`ExecutionMode`]'s `Display`
    /// (`DRY_RUN` / `TESTNET` / `MAINNET`).
    pub fn render(&self, now_ms: u64) -> serde_json::Value {
        let last_feed_ms = self.last_feed_ms.load(Ordering::Relaxed);
        let feed_age_s: Option<u64> = if last_feed_ms == 0 {
            None
        } else {
            Some(now_ms.saturating_sub(last_feed_ms) / 1_000)
        };
        serde_json::json!({
            "uptime_s": self.started.elapsed().as_secs(),
            "feed_age_s": feed_age_s,
            "mode": format!("{}", self.mode),
            "version": self.version,
        })
    }
}

/// Build the axum app exposing `GET /healthz`.
pub fn router(state: Arc<HealthState>) -> axum::Router {
    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .with_state(state)
}

/// `GET /healthz` — render the snapshot at the current wall time.
async fn healthz(
    axum::extract::State(state): axum::extract::State<Arc<HealthState>>,
) -> axum::Json<serde_json::Value> {
    axum::Json(state.render(wall_now_ms()))
}

/// Current wall-clock time in milliseconds since the Unix epoch (0 before it).
fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_millis() as u64)
}

/// Serve `router` on `0.0.0.0:port` until `shutdown` flips.
///
/// Returns `Ok(())` once the watch flips and in-flight requests drain.
///
/// # Errors
/// `SentinelError::Internal` when the listener cannot bind or the server
/// fails while running.
pub async fn serve(
    state: Arc<HealthState>,
    port: u16,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|err| SentinelError::Internal(format!("health: bind 0.0.0.0:{port}: {err}")))?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let mut shutdown = shutdown;
            if !*shutdown.borrow_and_update() {
                let _ = shutdown.changed().await;
            }
        })
        .await
        .map_err(|err| SentinelError::Internal(format!("health: serve: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A fixed injected "now" for deterministic math.
    const NOW_MS: u64 = 1_700_000_000_000;

    #[test]
    fn render_returns_frozen_shape_with_null_feed_age_until_touched() {
        let state = HealthState::new(ExecutionMode::DryRun);
        let body = state.render(NOW_MS);

        let object = body.as_object().expect("render returns an object");
        for key in ["uptime_s", "feed_age_s", "mode", "version"] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(object.len(), 4, "no extra keys in the frozen shape");
        assert_eq!(body["feed_age_s"], serde_json::Value::Null);
        assert_eq!(body["mode"], "DRY_RUN");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn render_computes_feed_age_seconds_after_touch() {
        let state = HealthState::new(ExecutionMode::Testnet);
        state.touch_feed(NOW_MS - 5_000);

        let body = state.render(NOW_MS);
        assert_eq!(body["feed_age_s"].as_u64(), Some(5));
        assert_eq!(body["mode"], "TESTNET");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn render_feed_age_saturates_on_backwards_clock() {
        let state = HealthState::new(ExecutionMode::Mainnet);
        state.touch_feed(NOW_MS + 10_000);

        let body = state.render(NOW_MS);
        assert_eq!(body["feed_age_s"].as_u64(), Some(0));
        assert_eq!(body["mode"], "MAINNET", "Display renders uppercase");
    }

    #[tokio::test]
    async fn router_serves_healthz_json_over_http() {
        let state = Arc::new(HealthState::new(ExecutionMode::DryRun));
        state.touch_feed(wall_now_ms() - 5_000);

        let app = router(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = reqwest::get(format!("http://{addr}/healthz"))
            .await
            .expect("GET /healthz");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.expect("JSON body");

        assert_eq!(body["mode"], "DRY_RUN");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body["uptime_s"].as_u64().is_some());
        let feed_age_s = body["feed_age_s"].as_u64().expect("touched feed");
        assert!(
            (5..=6).contains(&feed_age_s),
            "feed_age_s ≈ 5 s, got {feed_age_s}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn serve_answers_healthz_and_returns_ok_on_shutdown() {
        let state = Arc::new(HealthState::new(ExecutionMode::Testnet));

        // Pick a free port: bind an ephemeral listener, read it, release it.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(Arc::clone(&state), port, shutdown_rx));

        let url = format!("http://127.0.0.1:{port}/healthz");
        let mut ready = false;
        let mut last_error = String::new();
        for _ in 0..40 {
            match reqwest::get(&url).await {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    ready = true;
                    break;
                }
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(err) => last_error = err.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "server never became ready: {last_error}");

        shutdown_tx.send(true).expect("shutdown receiver alive");
        let joined = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("serve future finishes within 2 s of the flip")
            .expect("serve task joined");
        joined.expect("graceful shutdown returns Ok(())");
    }
}
