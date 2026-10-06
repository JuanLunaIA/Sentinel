//! Execution layer — the [`Executor`] trait, reports, and the idempotency +
//! post-fill-verification guard.
//!
//! Interfaces are frozen in `SPEC-P05.md` §5. Generics only: the async traits
//! here are not `dyn`-compatible on purpose (no `dyn` dispatch needed).
//!
//! **P05 status:** implemented. [`GuardedExecutor`] suppresses duplicates via
//! [`idempotency::IdempotencyStore`], submits through the inner executor, then
//! verifies the fill against a [`PositionProbe`]: a decreasing position size
//! sets the report's filled size to the observed decrease and its status to
//! `Filled` (the decrease covers the ordered size) or `Partial` (smaller
//! observed decrease); otherwise the inner report is returned as-is with a
//! `tracing::warn!` (alerting is P11).

pub mod dry_run;
pub mod idempotency;
pub mod perpl;
pub mod recording;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use sentinel_core::order::OrderRequest;
use sentinel_core::types::{MarketId, Position};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::error::{PerplError, Result, SentinelError};

/// How often the guard re-reads the position while verifying a fill.
const VERIFY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Terminal status of one submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExecutionStatus {
    /// Filled by the local DRY_RUN simulation.
    Simulated,
    /// Accepted for forwarding by the exchange (outcome arrives later).
    Submitted,
    /// Fully filled (post-verification).
    Filled,
    /// Partially filled (post-verification).
    Partial,
    /// Refused (validation, scope, or exchange rejection).
    Rejected,
}

/// Result of one submission.
///
/// Serializes to one JSON object — the report journal (`dry_run`) and the
/// recording executor persist exactly this shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionReport {
    /// The request this report answers.
    pub order: OrderRequest,
    /// Terminal status at report time.
    pub status: ExecutionStatus,
    /// Filled size in base units.
    pub filled_size: Decimal,
    /// Average fill price when known.
    pub avg_price: Option<Decimal>,
    /// Transaction hash when the venue produced one (gateway forwarding: none).
    pub tx_hash: Option<String>,
    /// Deterministic client order id (`sentinel-<market>-<seq>`).
    pub client_order_id: String,
    /// Human-readable detail (ack codes, simulation notes).
    pub detail: String,
    /// Wall-clock timestamp (ms) assigned by the executor.
    pub ts_ms: u64,
}

/// Deterministic client order id: `sentinel-<market_id>-<seq>`.
pub fn client_order_id(order: &OrderRequest, seq: u64) -> String {
    format!("sentinel-{}-{}", order.market_id.0, seq)
}

/// Monotonic sequence provider for client order ids (seeded from the
/// account's last forwarded request id when available).
#[derive(Debug)]
pub struct SeqCounter(AtomicU64);

impl SeqCounter {
    /// New counter starting at `seed + 1`.
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(seed.saturating_add(1)))
    }

    /// Next sequence value.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// Narrow account view used by executors and the guard.
#[allow(async_fn_in_trait)]
pub trait PositionProbe {
    /// Current position for `market_id`, if any.
    async fn position(&self, market_id: MarketId) -> Result<Option<Position>>;
}

impl PositionProbe for crate::perpl::LivePerpl {
    async fn position(&self, market_id: MarketId) -> Result<Option<Position>> {
        use crate::perpl::PerplFeed as _;
        let state = self.snapshot().await?;
        Ok(state
            .positions
            .into_iter()
            .find(|position| position.market_id == market_id))
    }
}

/// One order submission.
#[allow(async_fn_in_trait)]
pub trait Executor {
    /// Submit `order` and return its report.
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport>;
}

/// Hundredths-of-base-units bucket of a size (`truncate(size × 100)`).
///
/// Part of the idempotency key: near-identical sizes map to one bucket.
pub fn size_bucket(size: Decimal) -> i64 {
    let scaled = size.saturating_mul(Decimal::new(100, 0));
    i64::try_from(scaled.trunc()).unwrap_or(i64::MIN)
}

/// Idempotency key: `"{market_id}:{action}:{bucket}"`.
pub fn idempotency_key(market_id: MarketId, action: &str, size: Decimal) -> String {
    format!("{}:{}:{}", market_id.0, action, size_bucket(size))
}

/// Guarded wrapper: idempotency pre-check, then submit, then post-fill
/// verification against the probe (SPEC-P05 §5).
///
/// The key is the reduce-order key (`{market}:reduce:{bucket}`); a duplicate
/// inside the store's window is rejected with
/// [`SentinelError::DuplicateOrder`] before the inner executor is reached.
/// After a successful submit the position is polled every 200 ms (up to
/// `verify_after`): if its absolute size shrank against the pre-submit read,
/// the report's `filled_size` becomes the observed decrease and the status is
/// `Filled` when that decrease covers the ordered size, else `Partial` (the
/// inner report's price and detail are kept). A timeout or a probe failure
/// keeps the inner report and logs a warning.
pub struct GuardedExecutor<E, P> {
    inner: E,
    store: Arc<Mutex<idempotency::IdempotencyStore>>,
    probe: P,
    verify_after: Duration,
}

impl<E, P> GuardedExecutor<E, P> {
    /// Wrap `inner` with the given idempotency store and position probe.
    ///
    /// `verify_after` bounds the post-fill verification window (the store's
    /// window bounds duplicate suppression).
    pub fn new(
        inner: E,
        store: Arc<Mutex<idempotency::IdempotencyStore>>,
        probe: P,
        verify_after: Duration,
    ) -> Self {
        Self {
            inner,
            store,
            probe,
            verify_after,
        }
    }
}

impl<E, P> Executor for GuardedExecutor<E, P>
where
    E: Executor + Sync,
    P: PositionProbe + Sync,
{
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let key = idempotency_key(order.market_id, "reduce", order.size);
        let now_ms = unix_ms();
        let (allowed, window_secs) = {
            let mut store = self.store.lock().await;
            let window_secs = store.window().as_secs();
            let allowed = store.check_and_record(&key, now_ms)?;
            (allowed, window_secs)
        };
        if !allowed {
            return Err(SentinelError::DuplicateOrder { window_secs, key });
        }

        // Reference size before the order goes out: the verification baseline.
        let old_reference = match self.probe.position(order.market_id).await {
            Ok(position) => Some(position.map_or(Decimal::ZERO, |position| position.size.abs())),
            Err(err) => {
                tracing::warn!(
                    market_id = order.market_id.0,
                    error = %err,
                    "post-fill verification skipped: baseline position probe failed"
                );
                None
            }
        };

        let mut report = self.inner.submit(order).await?;

        let Some(old_reference) = old_reference else {
            return Ok(report);
        };

        let deadline = Instant::now() + self.verify_after;
        loop {
            match self.probe.position(order.market_id).await {
                Ok(position) => {
                    let new_size = position.map_or(Decimal::ZERO, |position| position.size.abs());
                    if new_size < old_reference {
                        let observed_fill = old_reference - new_size;
                        report.filled_size = observed_fill;
                        report.status = if observed_fill >= order.size {
                            ExecutionStatus::Filled
                        } else {
                            ExecutionStatus::Partial
                        };
                        return Ok(report);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        market_id = order.market_id.0,
                        error = %err,
                        "post-fill verification aborted: position probe failed; keeping inner report"
                    );
                    return Ok(report);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep((deadline - now).min(VERIFY_POLL_INTERVAL)).await;
        }

        tracing::warn!(
            market_id = order.market_id.0,
            key,
            verify_after = ?self.verify_after,
            "post-fill verification timed out; keeping inner report (alerting is P11)"
        );
        Ok(report)
    }
}

/// Append `value` as one JSON line to `path`, creating parent directories.
fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| PerplError::Order(format!("report dir {}: {err}", parent.display())))?;
    }
    let line = serde_json::to_string(value)
        .map_err(|err| PerplError::Order(format!("serialize report: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| PerplError::Order(format!("report file {}: {err}", path.display())))?;
    writeln!(file, "{line}")
        .map_err(|err| PerplError::Order(format!("report file {}: {err}", path.display())))?;
    Ok(())
}

/// Wall-clock unix milliseconds (report timestamps + idempotency bookkeeping).
fn unix_ms() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use sentinel_core::order::{CloseSide, OrderType};
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).expect("valid decimal literal")
    }

    fn position(size: Decimal) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: d("100"),
            mark_price: Some(d("100")),
            liq_price: None,
            collateral: d("50"),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: d("5"),
            opened_at: None,
        }
    }

    fn order() -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close: CloseSide::CloseLong,
            size: d("0.5"),
            order_type: OrderType::Market,
            max_slippage_bps: 10,
            size_decimals: 3,
        }
    }

    fn load_store(window: Duration, path: PathBuf) -> Arc<Mutex<idempotency::IdempotencyStore>> {
        Arc::new(Mutex::new(
            idempotency::IdempotencyStore::load(window, path).expect("load idempotency store"),
        ))
    }

    /// One scripted probe reply.
    #[derive(Clone)]
    enum Reply {
        /// Position (or absence) to return.
        Position(Option<Position>),
        /// Fail the read with a `PerplError::Rest`.
        Fail,
    }

    /// Scripted probe: pops one reply per call; the fallback repeats once the
    /// script is exhausted.
    struct ScriptProbe {
        replies: AsyncMutex<VecDeque<Reply>>,
        fallback: Reply,
    }

    impl ScriptProbe {
        fn new(replies: Vec<Reply>, fallback: Reply) -> Self {
            Self {
                replies: AsyncMutex::new(replies.into()),
                fallback,
            }
        }
    }

    impl PositionProbe for ScriptProbe {
        async fn position(&self, _market_id: MarketId) -> Result<Option<Position>> {
            let reply = {
                let mut replies = self.replies.lock().await;
                replies.pop_front().unwrap_or_else(|| self.fallback.clone())
            };
            match reply {
                Reply::Position(position) => Ok(position),
                Reply::Fail => Err(PerplError::Rest("scripted probe failure".to_string()).into()),
            }
        }
    }

    /// Inner executor counting submissions; its report is recognizable
    /// (`Filled`/`Partial` verdicts must preserve these fields).
    #[derive(Clone, Default)]
    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }

    impl Executor for CountingExecutor {
        async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionReport {
                order: order.clone(),
                status: ExecutionStatus::Submitted,
                filled_size: Decimal::ZERO,
                avg_price: None,
                tx_hash: None,
                client_order_id: "inner-id".to_string(),
                detail: "inner".to_string(),
                ts_ms: 42,
            })
        }
    }

    #[test]
    fn size_bucket_table_and_key_shape() {
        assert_eq!(size_bucket(d("0.5")), 50);
        assert_eq!(size_bucket(d("0.509")), 50, "truncation, not rounding");
        assert_eq!(size_bucket(d("-0.509")), -50, "truncates towards zero");
        assert_eq!(size_bucket(Decimal::ZERO), 0);
        assert_eq!(size_bucket(d("2")), 200);
        assert_eq!(
            idempotency_key(MarketId(32), "reduce", d("0.5")),
            "32:reduce:50"
        );
        assert_eq!(
            idempotency_key(MarketId(32), "close", d("0.5")),
            "32:close:50"
        );
    }

    #[test]
    fn seq_counter_starts_after_seed() {
        let seq = SeqCounter::new(41);
        assert_eq!(seq.next(), 42);
        assert_eq!(seq.next(), 43);
        assert_eq!(client_order_id(&order(), 7), "sentinel-32-7");
    }

    #[tokio::test]
    async fn guard_suppresses_duplicate_and_keeps_single_execution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = CountingExecutor::default();
        let calls = Arc::clone(&inner.calls);
        let guard = GuardedExecutor::new(
            inner,
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            // Constant probe: no decrease is ever observable → timeout path.
            ScriptProbe::new(
                vec![Reply::Position(Some(position(d("10"))))],
                Reply::Position(Some(position(d("10")))),
            ),
            Duration::from_millis(60),
        );

        let first = guard.submit(&order()).await.expect("first submission");
        assert_eq!(first.status, ExecutionStatus::Submitted);
        assert_eq!(first.detail, "inner");
        assert_eq!(first.client_order_id, "inner-id");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        match guard.submit(&order()).await {
            Err(SentinelError::DuplicateOrder { window_secs, key }) => {
                assert_eq!(window_secs, 60, "the error carries the store window");
                assert_eq!(key, "32:reduce:50");
            }
            other => panic!("expected DuplicateOrder, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a duplicate must never reach the inner executor"
        );
    }

    #[tokio::test]
    async fn guard_marks_partial_when_position_shrinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = GuardedExecutor::new(
            CountingExecutor::default(),
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            // Baseline 10 → verify read 9.9: decreased by 0.1 < ordered 0.5.
            ScriptProbe::new(
                vec![
                    Reply::Position(Some(position(d("10")))),
                    Reply::Position(Some(position(d("9.9")))),
                ],
                Reply::Position(Some(position(d("9.9")))),
            ),
            Duration::from_millis(500),
        );

        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(report.status, ExecutionStatus::Partial);
        // filled_size becomes the observed decrease; the rest of the inner
        // report is kept verbatim.
        assert_eq!(report.filled_size, d("0.1"));
        assert_eq!(report.avg_price, None);
        assert_eq!(report.detail, "inner");
        assert_eq!(report.client_order_id, "inner-id");
    }

    #[tokio::test]
    async fn guard_marks_filled_when_decrease_covers_order_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = GuardedExecutor::new(
            CountingExecutor::default(),
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            // Baseline 1 → verify read 0.5: decrease equals the ordered 0.5
            // while the position is still open.
            ScriptProbe::new(
                vec![
                    Reply::Position(Some(position(d("1")))),
                    Reply::Position(Some(position(d("0.5")))),
                ],
                Reply::Position(Some(position(d("0.5")))),
            ),
            Duration::from_millis(500),
        );

        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(
            report.status,
            ExecutionStatus::Filled,
            "a decrease covering the ordered size is a full fill"
        );
        assert_eq!(report.filled_size, d("0.5"));
    }

    #[tokio::test]
    async fn guard_marks_filled_when_position_goes_flat() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Size drops to zero.
        let guard = GuardedExecutor::new(
            CountingExecutor::default(),
            load_store(Duration::from_secs(60), dir.path().join("a.json")),
            ScriptProbe::new(
                vec![
                    Reply::Position(Some(position(d("10")))),
                    Reply::Position(Some(position(Decimal::ZERO))),
                ],
                Reply::Position(Some(position(Decimal::ZERO))),
            ),
            Duration::from_millis(500),
        );
        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(report.status, ExecutionStatus::Filled);
        assert_eq!(report.filled_size, d("10"), "observed decrease fills all");

        // Position disappears from the snapshot entirely.
        let guard = GuardedExecutor::new(
            CountingExecutor::default(),
            load_store(Duration::from_secs(60), dir.path().join("b.json")),
            ScriptProbe::new(
                vec![
                    Reply::Position(Some(position(d("10")))),
                    Reply::Position(None),
                ],
                Reply::Position(None),
            ),
            Duration::from_millis(500),
        );
        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(report.status, ExecutionStatus::Filled);
        assert_eq!(report.filled_size, d("10"));
    }

    #[tokio::test]
    async fn guard_keeps_inner_report_on_verification_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = CountingExecutor::default();
        let calls = Arc::clone(&inner.calls);
        let guard = GuardedExecutor::new(
            inner,
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            ScriptProbe::new(vec![], Reply::Position(Some(position(d("10"))))),
            Duration::from_millis(60),
        );

        let started = Instant::now();
        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(
            report.status,
            ExecutionStatus::Submitted,
            "no decrease observed within verify_after ⇒ inner report kept"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the guard must wait out the verification window"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn guard_keeps_inner_report_when_verify_probe_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = GuardedExecutor::new(
            CountingExecutor::default(),
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            ScriptProbe::new(
                vec![Reply::Position(Some(position(d("10")))), Reply::Fail],
                Reply::Fail,
            ),
            Duration::from_millis(500),
        );

        let report = guard
            .submit(&order())
            .await
            .expect("probe failure is not fatal");
        assert_eq!(report.status, ExecutionStatus::Submitted);
        assert_eq!(report.detail, "inner");
    }

    #[tokio::test]
    async fn guard_submits_unverified_when_baseline_probe_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = CountingExecutor::default();
        let calls = Arc::clone(&inner.calls);
        let guard = GuardedExecutor::new(
            inner,
            load_store(Duration::from_secs(60), dir.path().join("idem.json")),
            ScriptProbe::new(vec![Reply::Fail], Reply::Fail),
            Duration::from_millis(500),
        );

        let report = guard.submit(&order()).await.expect("submit");
        assert_eq!(
            report.status,
            ExecutionStatus::Submitted,
            "without a baseline the guard keeps the inner report"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the order still goes out");
    }

    #[tokio::test]
    async fn guard_allows_again_after_window_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = CountingExecutor::default();
        let calls = Arc::clone(&inner.calls);
        let guard = GuardedExecutor::new(
            inner,
            load_store(Duration::from_millis(300), dir.path().join("idem.json")),
            ScriptProbe::new(vec![], Reply::Position(Some(position(d("10"))))),
            Duration::from_millis(50),
        );

        guard.submit(&order()).await.expect("first submission");
        match guard.submit(&order()).await {
            Err(SentinelError::DuplicateOrder { .. }) => {}
            other => panic!("immediate repeat must be suppressed, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(350)).await;
        guard
            .submit(&order())
            .await
            .expect("after the window the key is allowed again");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
