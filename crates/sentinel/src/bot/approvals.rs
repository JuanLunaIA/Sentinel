//! Approval queue — TTL'd pending approvals (`SPEC-P11.md` §7).
//!
//! Pure (injected `now_ms`); journaling of EXPIRED outcomes happens in the
//! handler layer that consumes [`ApprovalQueue::expire_due`].
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

use std::collections::HashMap;
use std::sync::Mutex;

use sentinel_core::order::OrderRequest;

/// Pending approval lifetime (5 min).
pub const APPROVAL_TTL_MS: u64 = 300_000;

/// One queued approval request.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    /// Deterministic short id (`ap-<8 hex>`).
    pub id: String,
    /// Market the order targets.
    pub market_id: u32,
    /// Human summary (pre-escaping).
    pub summary: String,
    /// The sized order to execute on approval.
    pub order: OrderRequest,
    /// Journal seq of the intent, when known.
    pub decision_ref: Option<u64>,
    /// Enqueue time (epoch ms).
    pub created_ms: u64,
    /// Expiry time (epoch ms).
    pub expires_ms: u64,
}

/// In-memory queue.
#[derive(Debug, Default)]
pub struct ApprovalQueue {
    pending: Mutex<HashMap<String, PendingApproval>>,
}

impl ApprovalQueue {
    /// Empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Deterministic id: `ap-` + first 8 hex of `sha256(decision_id|now_ms)`.
    pub fn id_for(_decision_id: &str, _now_ms: u64) -> String {
        todo!("P11 agent bot-admin")
    }

    /// Insert (id from [`ApprovalQueue::id_for`]); returns the id.
    pub fn enqueue(&self, _approval: PendingApproval) -> String {
        todo!("P11 agent bot-admin")
    }

    /// Fetch a clone.
    pub fn get(&self, _id: &str) -> Option<PendingApproval> {
        todo!("P11 agent bot-admin")
    }

    /// Remove and return (approve/deny path).
    pub fn remove(&self, _id: &str) -> Option<PendingApproval> {
        todo!("P11 agent bot-admin")
    }

    /// Remove and return everything expired at `now_ms` (expiry at exactly
    /// `expires_ms` counts as expired).
    pub fn expire_due(&self, _now_ms: u64) -> Vec<PendingApproval> {
        todo!("P11 agent bot-admin")
    }

    /// Live count.
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .map(|guard| guard.len())
            .unwrap_or_default()
    }
}
