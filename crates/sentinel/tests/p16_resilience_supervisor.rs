//! P16 resilience checkpoint (SPEC-P16 §2–§3).
//!
//! Covers:
//! - `supervisor::spawn` restart-on-panic / stop-on-shutdown semantics with
//!   the restart counter observable through tracing events;
//! - `TelegramSink` queueing: callers always get `Ok`, failed deliveries are
//!   retried (3 retries, 500 ms base, exponential) and a success after
//!   failures is delivered;
//! - queue overflow drops the oldest alerts without failing callers;
//! - the Debug redaction audit for `Config` / `NansenConfig` / `PerplConfig` /
//!   `ApiKeySigner` (plus `TelegramSink`'s bot token).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sentinel::notify::{Alert, AlertKind, AlertSink, TelegramSink};
use sentinel::supervisor;
use tokio::sync::watch;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `io::Write` sink collecting formatted log lines for assertions.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything captured so far, as lossy UTF-8.
fn log_text(buffer: &LogBuffer) -> String {
    String::from_utf8_lossy(
        &buffer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
    .into_owned()
}

/// Install a thread-local TRACE+ capture subscriber (the guard must stay
/// alive for the assertions; the current-thread test runtime polls the
/// spawned worker tasks on this same thread).
fn capture_logs() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buffer, guard)
}

/// Poll `cond` every 20 ms until it holds or `timeout` elapses.
async fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Feed-stale alert with a distinct text, for delivery/queue tests.
fn test_alert(text: &str) -> Alert {
    Alert {
        kind: AlertKind::FeedStale { secs: 5 },
        market_id: None,
        text: text.to_string(),
        at_ms: 1_700_000_000_000,
    }
}

/// A bot pointed at the wiremock server (`/bot<token>/sendMessage`).
fn mock_bot(server: &MockServer) -> teloxide::Bot {
    let url = reqwest::Url::parse(&server.uri()).expect("mock server uri parses");
    teloxide::Bot::new("123456:P16TESTTOKEN").set_api_url(url)
}

/// Minimal valid Telegram `sendMessage` success body.
fn ok_message_body() -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "message_id": 1,
            "date": 1_700_000_000,
            "chat": { "id": 42, "type": "private" }
        }
    })
}

/// Poll the recorded-request count until it reaches `wanted` or `timeout`.
async fn wait_for_requests(server: &MockServer, wanted: usize, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = server
            .received_requests()
            .await
            .map_or(0, |requests| requests.len());
        if count >= wanted || tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Supervisor (SPEC-P16 §2)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn supervisor_restarts_panicking_task_once_then_stops_on_shutdown() {
    let (logs, _guard) = capture_logs();
    let (tx, rx) = watch::channel(false);
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_task = Arc::clone(&runs);
    let task_watch = rx.clone();

    let handle = supervisor::spawn("flaky-test-task", rx.clone(), move || {
        let runs = Arc::clone(&runs_task);
        let task_watch = task_watch.clone();
        async move {
            let mut task_watch = task_watch;
            let attempt = runs.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                panic!("deliberate P16 supervisor test panic");
            }
            // A long-lived task would serve until shutdown drains it.
            if !*task_watch.borrow_and_update() {
                let _ = task_watch.changed().await;
            }
        }
    });

    assert!(
        wait_for(Duration::from_secs(5), || runs.load(Ordering::SeqCst) >= 2).await,
        "panicking task was never restarted"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the panic causes exactly one restart"
    );
    assert!(!handle.is_finished(), "supervisor must stay alive");

    // The restart log line may be written after the replacement task is
    // spawned, so wait for it instead of reading the buffer immediately.
    let logged = wait_for(Duration::from_secs(2), || {
        let text = log_text(&logs);
        text.contains("flaky-test-task")
            && text.contains("restarts=1")
            && text.contains("restarting")
    })
    .await;
    assert!(logged, "restart log line missing: {}", log_text(&logs));

    // Shutdown drains the healthy attempt and stops the supervision loop.
    tx.send(true).expect("shutdown receiver alive");
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("supervisor ends after shutdown")
        .expect("supervisor task joins cleanly");
    assert_eq!(runs.load(Ordering::SeqCst), 2, "no restarts after shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn supervisor_stops_restarting_once_shutdown_is_set() {
    let (tx, rx) = watch::channel(false);
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_task = Arc::clone(&runs);

    let handle = supervisor::spawn("stop-on-shutdown", rx.clone(), move || {
        let runs = Arc::clone(&runs_task);
        async move {
            let _attempt = runs.fetch_add(1, Ordering::SeqCst);
            panic!("every attempt panics until shutdown stops the loop");
        }
    });

    // Attempt #1 panics immediately; the supervisor enters its 1 s backoff.
    assert!(
        wait_for(Duration::from_secs(3), || runs.load(Ordering::SeqCst) >= 1).await,
        "first attempt never ran"
    );
    // Flip shutdown while the first restart is still pending.
    tx.send(true).expect("shutdown receiver alive");

    // The supervisor must end without launching another attempt.
    tokio::time::timeout(Duration::from_secs(4), handle)
        .await
        .expect("supervisor ends after shutdown")
        .expect("supervisor task joins cleanly");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no restart after shutdown");

    // Guard against a late wake-up starting a stray attempt.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "still no restart after shutdown"
    );
}

// ---------------------------------------------------------------------------
// Telegram delivery queue (SPEC-P16 §2)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn telegram_send_is_ok_and_retries_until_delivered() {
    let server = MockServer::start().await;
    // The first two attempts fail; the third is accepted. Failures are
    // injected as a Telegram-shaped 400 (teloxide adds a hard 10 s sleep to
    // every 5xx before surfacing the error, which would starve this window;
    // the queue retry path is identical for any delivery Err — the 5xx
    // wall-clock is measured and documented in docs/evidence/p16-verify.txt).
    Mock::given(method("POST"))
        .and(path_regex("(?i).*/sendmessage"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: injected delivery failure"
        })))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("(?i).*/sendmessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_message_body()))
        .with_priority(2)
        .mount(&server)
        .await;

    let sink = TelegramSink::with_bot(mock_bot(&server), 42);
    let alert = test_alert("p16 telegram retry delivery probe");

    // The caller gets Ok immediately, regardless of the coming 500s.
    sink.send(&alert)
        .await
        .expect("send never propagates delivery failures");

    // Retries observed: initial attempt + 2 retries before the 200 lands.
    let count = wait_for_requests(&server, 3, Duration::from_secs(8)).await;
    assert!(
        count >= 3,
        "expected at least 3 requests (failures + retry), got {count}"
    );
    // Settle: the delivery succeeded, so no further attempts.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 3, "two 500s then one success, then silence");
    let bodies: Vec<String> = requests
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("p16 telegram retry delivery probe")),
        "the delivered body carries the alert text: {bodies:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn telegram_delivery_abandons_after_three_retries() {
    let (logs, _guard) = capture_logs();
    let server = MockServer::start().await;
    // Everything fails (Telegram-shaped 400; see the note in the delivery
    // test above): the retry budget must cap the attempts at 4.
    Mock::given(method("POST"))
        .and(path_regex("(?i).*/sendmessage"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 400,
            "description": "Bad Request: injected delivery failure"
        })))
        .mount(&server)
        .await;

    let sink = TelegramSink::with_bot(mock_bot(&server), 42);
    let alert = test_alert("p16 telegram abandon probe");
    sink.send(&alert).await.expect("caller still gets Ok");

    // Attempts land at ~0 s, +0.5 s, +1.5 s, +3.5 s (500 ms doubling).
    let count = wait_for_requests(&server, 4, Duration::from_secs(10)).await;
    assert_eq!(count, 4, "initial attempt + exactly 3 retries");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 4, "no fifth attempt after the retry budget");

    let captured = log_text(&logs);
    assert!(
        captured.contains("alert dropped after retries"),
        "the abandonment is logged: {captured}"
    );
    assert!(
        captured.contains("retries=3"),
        "the log pins the retry count: {captured}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn telegram_queue_overflow_drops_oldest_and_never_fails_callers() {
    let (logs, _guard) = capture_logs();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("(?i).*/sendmessage"))
        .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_millis(200)))
        .mount(&server)
        .await;

    let sink = TelegramSink::with_bot(mock_bot(&server), 42);
    let alert = test_alert("p16 overflow probe");
    // 40 enqueues into a 32-slot queue: every caller stays `Ok`. `send` has
    // no internal await point, so the burst lands before the worker is
    // scheduled; the oldest entries (7 or 8 of them, depending on whether
    // the worker managed to pop one item first) are dropped.
    for _ in 0..40 {
        sink.send(&alert).await.expect("queueing never fails");
    }

    let captured = log_text(&logs);
    let drops = captured.matches("dropping oldest alert").count();
    assert!(
        (7..=8).contains(&drops),
        "40 enqueues into a 32-slot queue drop the oldest entries, got {drops}: {captured}"
    );
    assert!(
        captured.contains("capacity=32"),
        "the overflow warn carries the capacity: {captured}"
    );
    assert!(
        captured.contains("dropped_total="),
        "the overflow warn carries a running count: {captured}"
    );
}

// ---------------------------------------------------------------------------
// Debug redaction audit (SPEC-P16 §3)
// ---------------------------------------------------------------------------

mod redaction {
    use std::collections::HashMap;

    use sentinel::config::Config;
    use sentinel::perpl::auth::ApiKeySigner;

    /// Placeholder secrets: none of these may ever appear in `Debug` output.
    const PERPL_API_KEY: &str = "sentinel-placeholder-perpl-api-key";
    const QWEN_API_KEY: &str = "sentinel-placeholder-qwen-api-key";
    const KIMI_API_KEY: &str = "sentinel-placeholder-kimi-api-key";
    const NANSEN_PAYER_KEY: &str = "sentinel-placeholder-nansen-payer-key";
    const TELEGRAM_TOKEN: &str = "sentinel-placeholder-telegram-token";
    const RPC_SIGNER_KEY: &str = "sentinel-placeholder-rpc-signer-key";

    /// Valid 64-hex placeholder for `PERPL_API_KEY_SECRET` (32 bytes).
    fn perpl_secret_hex() -> String {
        "c5ec".repeat(16)
    }

    fn vars() -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("PERPL_ENV".to_string(), "testnet".to_string());
        vars.insert("PERPL_API_KEY".to_string(), PERPL_API_KEY.to_string());
        vars.insert("PERPL_API_KEY_SECRET".to_string(), perpl_secret_hex());
        vars.insert("QWEN_API_KEY".to_string(), QWEN_API_KEY.to_string());
        vars.insert("KIMI_API_KEY".to_string(), KIMI_API_KEY.to_string());
        vars.insert("NANSEN_PAYER_KEY".to_string(), NANSEN_PAYER_KEY.to_string());
        vars.insert("TELOXIDE_TOKEN".to_string(), TELEGRAM_TOKEN.to_string());
        vars.insert("TELEGRAM_ALLOWED_USER_IDS".to_string(), "42".to_string());
        vars.insert("RPC_SIGNER_KEY".to_string(), RPC_SIGNER_KEY.to_string());
        vars
    }

    #[test]
    fn config_and_nested_debugs_never_contain_secret_values() {
        let cfg = Config::from_vars(vars()).expect("placeholder config builds");
        let debug = format!("{cfg:?}");
        for secret in [
            PERPL_API_KEY,
            QWEN_API_KEY,
            KIMI_API_KEY,
            NANSEN_PAYER_KEY,
            TELEGRAM_TOKEN,
            RPC_SIGNER_KEY,
        ] {
            assert!(
                !debug.contains(secret),
                "config Debug leaked a secret value"
            );
        }
        let hex = perpl_secret_hex();
        assert!(
            !debug.contains(&hex),
            "config Debug leaked the Perpl API secret hex"
        );
        assert!(
            !debug.contains("sentinel-placeholder"),
            "a placeholder secret prefix leaked: {debug}"
        );
        // The redaction marker and structural context are still rendered.
        assert!(debug.contains("REDACTED"), "no redaction marker: {debug}");
        assert!(
            debug.contains("PerplConfig"),
            "nested config type missing: {debug}"
        );

        let perpl = format!("{:?}", cfg.perpl);
        assert!(
            !perpl.contains(PERPL_API_KEY) && !perpl.contains(&hex),
            "PerplConfig Debug leaked: {perpl}"
        );
        let nansen = format!("{:?}", cfg.nansen);
        assert!(
            !nansen.contains(NANSEN_PAYER_KEY),
            "NansenConfig Debug leaked: {nansen}"
        );
    }

    #[test]
    fn api_key_signer_debug_never_contains_token_or_seed() {
        let hex = perpl_secret_hex();
        let signer = ApiKeySigner::from_parts(PERPL_API_KEY, &hex, 10143).expect("signer builds");
        let debug = format!("{signer:?}");
        assert!(
            !debug.contains(PERPL_API_KEY),
            "signer Debug leaked the token: {debug}"
        );
        assert!(
            !debug.contains(&hex),
            "signer Debug leaked the seed hex: {debug}"
        );
        assert!(debug.contains("REDACTED"), "no redaction marker: {debug}");
        assert!(debug.contains("10143"), "chain id missing: {debug}");
    }

    #[test]
    fn telegram_sink_debug_never_contains_the_bot_token() {
        let sink = sentinel::notify::TelegramSink::new("123456:P16-OUT-OF-LINE-TOKEN", 42);
        let debug = format!("{sink:?}");
        assert!(
            !debug.contains("P16-OUT-OF-LINE-TOKEN"),
            "telegram sink Debug leaked the token: {debug}"
        );
        assert!(debug.contains("REDACTED"), "no redaction marker: {debug}");
    }
}
