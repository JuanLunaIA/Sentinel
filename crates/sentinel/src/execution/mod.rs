//! Execution layer — the [`Executor`] trait, reports, and the idempotency +
//! post-fill-verification guard.
//!
//! Interfaces are frozen in `SPEC-P05.md` §5. Generics only: the async traits
//! here are not `dyn`-compatible on purpose (no `dyn` dispatch needed).
//!
//! **Skeleton status (P05):** implemented by the P05 wave.

pub mod dry_run;
pub mod idempotency;
pub mod perpl;
pub mod recording;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rust_decimal::Decimal;
use sentinel_core::order::OrderRequest;
use sentinel_core::types::{MarketId, Position};
use tokio::sync::Mutex;

use crate::error::Result;

/// Terminal status of one submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[allow(dead_code)] // stub fields; consumed by the P05 wave
pub struct GuardedExecutor<E, P> {
    inner: E,
    store: Arc<Mutex<idempotency::IdempotencyStore>>,
    probe: P,
    verify_after: Duration,
}

impl<E, P> GuardedExecutor<E, P> {
    /// Wrap `inner` with the given idempotency store and position probe.
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
        let _ = (
            order,
            &self.inner,
            &self.store,
            &self.probe,
            self.verify_after,
        );
        todo!("P05 agent exec: idempotency pre-check + submit + post-verify")
    }
}
