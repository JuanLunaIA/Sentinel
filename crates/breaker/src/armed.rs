//! Armed HTTP surface (`SPEC-P14.md` §5).
//!
//! - `GET /api/heartbeat-status` → 200
//!   `{"generated_at_ms":n,"guardians":[{"address","last_ts_ms","age_secs",
//!   "max_tier","stale","critical","armed"}]}` (`armed` = would fire now or
//!   already fired this epoch);
//! - `POST /breaker/trigger` — body exactly
//!   `{"guardian":"0x..","reason":"..","requested_at_ms":n}`; header
//!   `X-Breaker-Signature: sha256=<hex HMAC_SHA256(secret, raw body bytes)>`;
//!   constant-time compare; responses: **202**
//!   `{"accepted":true,"epoch":n,"fired":bool}` (`fired=false` on a duplicate
//!   epoch), **401** bad signature, **400** bad body, **423** when the
//!   guardian is fresh (`stale=false`).
//!
//! The POST path enforces the full §3 fire predicate server-side: the
//! guardian must be **stale** (`age > stale_mult × interval`) **and**
//! **critical** (`max_tier >= 2`). A fresh guardian is refused with 423; a
//! stale-but-not-critical guardian is accepted (202) but `fired=false` — the
//! breaker performs no action. The breaker's own 5 s ticker fires the same
//! predicate; the shared one-fire-per-epoch gate makes the two paths
//! idempotent against each other.
//!
//! Frozen test vector (`SPEC-P14` §5):
//!
//! ```text
//! secret    = spec-test-secret
//! body      = {"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","reason":"spec-vector","requested_at_ms":1791200000000}
//! signature = sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04
//! ```

use std::sync::Arc;

use alloy::primitives::Address;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::{Mutex, RwLock, watch};
use tracing::{info, warn};

use crate::config::BreakerConfig;
use crate::executor::BreakerExecutor;
use crate::trigger::{self, FireOutcome, FireStore, GuardianStatus, unix_ms};
use crate::watcher::WatcherState;

/// HMAC-SHA256 header carrying `sha256=<hex>`.
pub const SIGNATURE_HEADER: &str = "X-Breaker-Signature";

/// Required prefix of the signature header value.
pub const SIGNATURE_PREFIX: &str = "sha256=";

/// Secret of the frozen spec test vector.
pub const SPEC_TEST_SECRET: &str = "spec-test-secret";

/// Body of the frozen spec test vector (byte-exact).
pub const SPEC_TEST_BODY: &str = "{\"guardian\":\"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\",\"reason\":\"spec-vector\",\"requested_at_ms\":1791200000000}";

/// Expected HMAC-SHA256 of [`SPEC_TEST_BODY`] under [`SPEC_TEST_SECRET`].
pub const SPEC_TEST_SIGNATURE: &str =
    "af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04";

/// Compute `HMAC_SHA256(secret, body)` as lowercase hex.
pub fn compute_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verification of `X-Breaker-Signature` for `body`.
///
/// Returns `false` for a missing/malformed value. The final comparison is
/// constant-time (`hmac`'s `verify_slice`, which uses a constant-time byte
/// equality internally).
pub fn verify_signature(secret: &[u8], body: &[u8], header_value: &str) -> bool {
    let Some(expected_hex) = header_value.trim().strip_prefix(SIGNATURE_PREFIX) else {
        return false;
    };
    let Ok(expected) = hex::decode(expected_hex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Exact POST body shape (`deny_unknown_fields` → "body exactly", §5).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRequest {
    /// Guardian address to fire for (must be in `BREAKER_GUARDIANS`).
    pub guardian: String,
    /// Operator/CRE-supplied reason, journaled verbatim.
    pub reason: String,
    /// Request timestamp from the caller (frozen body field; the server's own
    /// clock is authoritative for staleness/epoch).
    pub requested_at_ms: u64,
}

/// 200 body of `GET /api/heartbeat-status` (§5).
#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatStatus {
    /// Server wall clock, ms.
    pub generated_at_ms: u64,
    /// One entry per configured guardian, config order.
    pub guardians: Vec<GuardianStatus>,
}

/// 202 body of `POST /breaker/trigger` (§5).
#[derive(Debug, Clone, Serialize)]
pub struct TriggerAccepted {
    /// Always `true` on 202.
    pub accepted: bool,
    /// Staleness epoch the request landed in.
    pub epoch: u64,
    /// `true` when this call performed a new fire.
    pub fired: bool,
}

/// Shared state of the armed surface.
#[derive(Clone)]
pub struct ArmedState {
    /// Validated configuration.
    pub cfg: Arc<BreakerConfig>,
    /// Watcher-tracked heartbeats.
    pub watched: Arc<RwLock<WatcherState>>,
    /// Persisted fire gate.
    pub fires: Arc<Mutex<FireStore>>,
    /// Action executor (plan + journal + alert).
    pub executor: Arc<BreakerExecutor>,
}

impl ArmedState {
    /// Assemble the state.
    pub fn new(
        cfg: Arc<BreakerConfig>,
        watched: Arc<RwLock<WatcherState>>,
        fires: Arc<Mutex<FireStore>>,
        executor: Arc<BreakerExecutor>,
    ) -> Self {
        Self {
            cfg,
            watched,
            fires,
            executor,
        }
    }
}

/// Build the router (`GET /api/heartbeat-status`, `POST /breaker/trigger`).
pub fn router(state: ArmedState) -> Router {
    Router::new()
        .route("/api/heartbeat-status", get(heartbeat_status))
        .route("/breaker/trigger", post(trigger_handler))
        .with_state(state)
}

/// Serve until `shutdown` flips (graceful; in-flight requests finish).
///
/// # Errors
/// Propagates the IO error from `axum::serve`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: ArmedState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
}

/// `GET /api/heartbeat-status` handler.
pub async fn heartbeat_status(State(state): State<ArmedState>) -> Response {
    let now_ms = unix_ms();
    let heartbeats: Vec<(Address, Option<crate::watcher::GuardianHeartbeat>)> = {
        let watched = state.watched.read().await;
        state
            .cfg
            .guardians
            .iter()
            .map(|guardian| (*guardian, watched.latest(guardian).cloned()))
            .collect()
    };
    let guardians = {
        let fires = state.fires.lock().await;
        heartbeats
            .iter()
            .map(|(guardian, heartbeat)| {
                trigger::status_for(&state.cfg, guardian, heartbeat.as_ref(), &fires, now_ms)
            })
            .collect()
    };
    Json(HeartbeatStatus {
        generated_at_ms: now_ms,
        guardians,
    })
    .into_response()
}

/// `POST /breaker/trigger` handler (§5 status codes).
pub async fn trigger_handler(
    State(state): State<ArmedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Signature over the RAW body bytes, constant-time compare.
    let Some(signature) = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        warn!("breaker armed: POST /breaker/trigger without a signature header");
        return json_error(
            StatusCode::UNAUTHORIZED,
            "missing or malformed X-Breaker-Signature header",
        );
    };
    if !verify_signature(state.cfg.arm_secret.as_bytes(), &body, signature) {
        warn!("breaker armed: POST /breaker/trigger with an invalid signature");
        return json_error(StatusCode::UNAUTHORIZED, "bad signature");
    }

    // 2. Body shape (exactly the frozen fields).
    let request: TriggerRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return json_error(StatusCode::BAD_REQUEST, format!("bad body: {err}"));
        }
    };
    let Ok(guardian) = request.guardian.trim().parse::<Address>() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "guardian is not a 20-byte hex address",
        );
    };
    if !state.cfg.guardians.contains(&guardian) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "guardian is not in BREAKER_GUARDIANS",
        );
    }

    // 3. Fresh guardians are locked out (423); stale ones may fire.
    let now_ms = unix_ms();
    let heartbeat = state.watched.read().await.latest(&guardian).cloned();
    let last_ts_ms = heartbeat.as_ref().map(|hb| hb.last_ts_ms).unwrap_or(0);
    let age = trigger::age_secs(now_ms, last_ts_ms);
    if !trigger::is_stale(age, state.cfg.heartbeat_interval_secs, state.cfg.stale_mult) {
        info!(
            guardian = %request.guardian,
            age_secs = age,
            "breaker armed: trigger refused, guardian is fresh"
        );
        return (
            StatusCode::LOCKED,
            Json(serde_json::json!({
                "error": "guardian is fresh",
                "stale": false,
                "age_secs": age,
            })),
        )
            .into_response();
    }

    // 4. Fire (the §3 predicate is enforced inside: critical + unclaimed
    //    epoch).
    match trigger::fire_if_due(
        &state.cfg,
        &state.watched,
        &state.fires,
        &state.executor,
        guardian,
        &request.reason,
        now_ms,
    )
    .await
    {
        Ok(FireOutcome::Fired { epoch, .. }) => {
            info!(
                guardian = %request.guardian,
                epoch,
                reason = %request.reason,
                "breaker armed: trigger accepted (fired)"
            );
            (
                StatusCode::ACCEPTED,
                Json(TriggerAccepted {
                    accepted: true,
                    epoch,
                    fired: true,
                }),
            )
                .into_response()
        }
        Ok(FireOutcome::Duplicate { epoch }) => (
            StatusCode::ACCEPTED,
            Json(TriggerAccepted {
                accepted: true,
                epoch,
                fired: false,
            }),
        )
            .into_response(),
        Ok(FireOutcome::NotCritical { epoch }) => {
            info!(
                guardian = %request.guardian,
                epoch,
                "breaker armed: trigger accepted but the guardian is not critical; no fire"
            );
            (
                StatusCode::ACCEPTED,
                Json(TriggerAccepted {
                    accepted: true,
                    epoch,
                    fired: false,
                }),
            )
                .into_response()
        }
        Err(err) => {
            warn!(error = %err, "breaker armed: fire failed to persist");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("fire failed: {err}"),
            )
        }
    }
}

/// JSON error body `{"error": "..."}`.
fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::BreakerExecutor;
    use crate::test_support::TempDir;
    use crate::watcher::{GuardianHeartbeat, WatcherState};

    use alloy::primitives::B256;
    use std::collections::HashMap;

    const GUARDIAN: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const GUARDIAN_KEY: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";

    fn test_cfg(dir: &TempDir) -> Arc<BreakerConfig> {
        Arc::new(
            BreakerConfig::from_vars(HashMap::from([
                (
                    "BREAKER_ANCHOR_ADDRESS".to_string(),
                    "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
                ),
                ("BREAKER_GUARDIANS".to_string(), GUARDIAN.to_string()),
                (
                    "BREAKER_ARM_SECRET".to_string(),
                    "spec-test-secret".to_string(),
                ),
                (
                    "BREAKER_JOURNAL".to_string(),
                    dir.join("journal.jsonl").display().to_string(),
                ),
                (
                    "BREAKER_STATE_FILE".to_string(),
                    dir.join("state.json").display().to_string(),
                ),
            ]))
            .expect("test config"),
        )
    }

    fn test_state(dir: &TempDir) -> ArmedState {
        let cfg = test_cfg(dir);
        let watched = Arc::new(RwLock::new(WatcherState::new()));
        let fires = Arc::new(Mutex::new(
            FireStore::load(cfg.state_file.clone()).expect("fire store"),
        ));
        let executor = Arc::new(BreakerExecutor::new(Arc::clone(&cfg)));
        ArmedState::new(cfg, watched, fires, executor)
    }

    fn seed_heartbeat(state: &ArmedState, seq: u64, last_ts_ms: u64, max_tier: u8) {
        let mut watched = state.watched.try_write().expect("write lock");
        watched.record(GuardianHeartbeat {
            guardian: GUARDIAN.parse().expect("guardian"),
            last_ts_ms,
            max_tier,
            open_positions: 1,
            risk_state_hash: B256::ZERO,
            block_number: seq,
            log_index: 0,
        });
    }

    fn signed(body: &str) -> (String, Bytes) {
        let signature = compute_signature(SPEC_TEST_SECRET.as_bytes(), body.as_bytes());
        (
            format!("{SIGNATURE_PREFIX}{signature}"),
            Bytes::from(body.to_string()),
        )
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body bytes");
        serde_json::from_slice(&bytes).expect("JSON body")
    }

    fn stale_body() -> String {
        format!(
            r#"{{"guardian":"{GUARDIAN}","reason":"unit-test","requested_at_ms":1791200000000}}"#
        )
    }

    // ---- HMAC ---------------------------------------------------------

    #[test]
    fn spec_vector_signature_matches_exactly() {
        let computed = compute_signature(SPEC_TEST_SECRET.as_bytes(), SPEC_TEST_BODY.as_bytes());
        assert_eq!(computed, SPEC_TEST_SIGNATURE);
        assert!(verify_signature(
            SPEC_TEST_SECRET.as_bytes(),
            SPEC_TEST_BODY.as_bytes(),
            &format!("{SIGNATURE_PREFIX}{SPEC_TEST_SIGNATURE}")
        ));

        // The exact spec body parses into the frozen request shape.
        let request: TriggerRequest = serde_json::from_str(SPEC_TEST_BODY).expect("parses");
        assert_eq!(request.guardian, GUARDIAN);
        assert_eq!(request.reason, "spec-vector");
        assert_eq!(request.requested_at_ms, 1_791_200_000_000);
    }

    #[test]
    fn signature_rejects_wrong_secret_tampering_and_malformed_headers() {
        let good = compute_signature(SPEC_TEST_SECRET.as_bytes(), SPEC_TEST_BODY.as_bytes());
        let header = format!("{SIGNATURE_PREFIX}{good}");

        // Wrong secret.
        assert!(!verify_signature(
            b"other-secret",
            SPEC_TEST_BODY.as_bytes(),
            &header
        ));

        // Tampered body (a single appended byte flips the digest).
        let mut tampered = SPEC_TEST_BODY.to_string();
        tampered.push(' ');
        assert!(!verify_signature(
            SPEC_TEST_SECRET.as_bytes(),
            tampered.as_bytes(),
            &header
        ));

        // Tampered body, character edit in the middle.
        let edited = SPEC_TEST_BODY.replace("spec-vector", "spec-vect0r");
        assert!(!verify_signature(
            SPEC_TEST_SECRET.as_bytes(),
            edited.as_bytes(),
            &header
        ));

        // Malformed headers fail closed.
        for bad in [
            "",
            "deadbeef",
            "sha256=",
            "sha256=zz",
            "sha256=abcdef",
            "SHA256=abcdef",
            "sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da0",
        ] {
            assert!(
                !verify_signature(SPEC_TEST_SECRET.as_bytes(), SPEC_TEST_BODY.as_bytes(), bad),
                "header {bad:?} must fail"
            );
        }

        // A valid signature for a different body fails closed.
        let other = compute_signature(SPEC_TEST_SECRET.as_bytes(), b"another body");
        assert!(!verify_signature(
            SPEC_TEST_SECRET.as_bytes(),
            SPEC_TEST_BODY.as_bytes(),
            &format!("{SIGNATURE_PREFIX}{other}")
        ));
    }

    // ---- handlers -----------------------------------------------------

    #[tokio::test]
    async fn status_reports_stale_critical_and_armed_per_guardian() {
        let dir = TempDir::new("armed-status");
        let state = test_state(&dir);
        let now = unix_ms();
        seed_heartbeat(&state, 1, now - 181_000, 3); // stale + Red

        let response = heartbeat_status(State(state.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert!(json["generated_at_ms"].as_u64().expect("ts") > 0);
        let guardians = json["guardians"].as_array().expect("guardians");
        assert_eq!(guardians.len(), 1);
        let entry = &guardians[0];
        assert_eq!(entry["address"], GUARDIAN_KEY);
        assert_eq!(entry["max_tier"], 3);
        assert_eq!(entry["stale"], true);
        assert_eq!(entry["critical"], true);
        assert_eq!(entry["armed"], true);
        assert!(entry["age_secs"].as_u64().expect("age") >= 181);
    }

    #[tokio::test]
    async fn post_flow_covers_202_400_401_and_423() {
        let dir = TempDir::new("armed-post");
        let state = test_state(&dir);

        // Fresh guardian → 423.
        seed_heartbeat(&state, 1, unix_ms(), 3);
        let (header, body) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::LOCKED);
        let json = body_json(response).await;
        assert_eq!(json["stale"], false);

        // Make it stale: a NEWER block carries an older timestamp (rewind).
        seed_heartbeat(&state, 2, unix_ms() - 1_000_000, 3);

        // Bad signature → 401.
        let (_, body) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, "sha256=beef".parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Missing signature header → 401.
        let response = trigger_handler(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from(stale_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Valid signature, tampered body → 401.
        let (header, _) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(
            State(state.clone()),
            headers,
            Bytes::from(format!("{} ", stale_body())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Valid signature, bad body (missing field) → 400.
        let bad_body = r#"{"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"}"#;
        let (header, body) = signed(bad_body);
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Valid signature, unknown guardian → 400.
        let unknown = r#"{"guardian":"0x0000000000000000000000000000000000000001","reason":"x","requested_at_ms":1}"#;
        let (header, body) = signed(unknown);
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Valid signature, stale guardian → 202 fired=true.
        let (header, body) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let json = body_json(response).await;
        assert_eq!(json["accepted"], true);
        assert_eq!(json["fired"], true);
        let epoch = json["epoch"].as_u64().expect("epoch");

        // Same epoch again → 202 fired=false (duplicate).
        let (header, body) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let json = body_json(response).await;
        assert_eq!(json["fired"], false);
        assert_eq!(json["epoch"].as_u64(), Some(epoch));

        // The fire was journaled once (alert-only: no snapshot source).
        let journal = std::fs::read_to_string(state.cfg.journal.clone()).expect("journal");
        assert_eq!(journal.lines().count(), 1, "{journal}");
        let line: serde_json::Value = serde_json::from_str(journal.trim()).expect("line JSON");
        assert_eq!(line["status"], "alert_only");
        assert_eq!(line["guardian"], GUARDIAN_KEY);
        assert!(
            line["detail"].as_str().unwrap_or("").contains("unit-test"),
            "reason journaled: {line}"
        );
    }

    #[tokio::test]
    async fn post_on_stale_but_not_critical_guardian_does_not_fire() {
        let dir = TempDir::new("armed-not-critical");
        let state = test_state(&dir);
        // Stale (300 s) but Green — the §3 predicate is `stale AND critical`.
        seed_heartbeat(&state, 1, unix_ms() - 300_000, 0);

        let (header, body) = signed(&stale_body());
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_HEADER, header.parse().expect("header"));
        let response = trigger_handler(State(state.clone()), headers, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let json = body_json(response).await;
        assert_eq!(json["accepted"], true);
        assert_eq!(json["fired"], false, "not critical => no fire: {json}");
        assert!(json["epoch"].is_u64());
        assert!(
            !state.cfg.journal.exists(),
            "no journal line for a non-fire"
        );
        assert!(
            state.fires.lock().await.snapshot().guardians.is_empty(),
            "no epoch claimed for a non-fire"
        );
    }

    // ---- end-to-end over a loopback socket ----------------------------

    async fn http_roundtrip(addr: std::net::SocketAddr, request: String) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect loopback");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut buffer = Vec::new();
        stream
            .read_to_end(&mut buffer)
            .await
            .expect("read response");
        let text = String::from_utf8_lossy(&buffer).to_string();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    #[tokio::test]
    async fn armed_surface_works_over_a_loopback_socket() {
        let dir = TempDir::new("armed-http");
        let state = test_state(&dir);
        seed_heartbeat(&state, 1, unix_ms() - 1_000_000, 2); // stale + Orange

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(serve(listener, state.clone(), shutdown_rx));

        // GET status.
        let (status, body) = http_roundtrip(
            addr,
            format!(
                "GET /api/heartbeat-status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_str(&body).expect("status JSON");
        assert_eq!(json["guardians"][0]["address"], GUARDIAN_KEY);
        assert_eq!(json["guardians"][0]["armed"], true);

        // POST with a valid signature → 202 fired=true.
        let (header, body_bytes) = signed(&stale_body());
        let request = format!(
            "POST /breaker/trigger HTTP/1.1\r\nHost: {addr}\r\n\
             X-Breaker-Signature: {header}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_bytes.len(),
            String::from_utf8_lossy(&body_bytes)
        );
        let (status, body) = http_roundtrip(addr, request).await;
        assert_eq!(status, 202, "body: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("trigger JSON");
        assert_eq!(json["accepted"], true);
        assert_eq!(json["fired"], true);

        // POST with a bad signature → 401.
        let request = format!(
            "POST /breaker/trigger HTTP/1.1\r\nHost: {addr}\r\n\
             X-Breaker-Signature: sha256=00\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            stale_body().len(),
            stale_body()
        );
        let (status, _) = http_roundtrip(addr, request).await;
        assert_eq!(status, 401);

        // Unknown route → 404 (surface shape check).
        let (status, _) = http_roundtrip(
            addr,
            format!("GET /nope HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 404);

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server stops promptly")
            .expect("server joins")
            .expect("server exits cleanly");
    }
}
