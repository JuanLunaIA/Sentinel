//! Approval queue — TTL'd pending approvals (`SPEC-P11.md` §7).
//!
//! Pure (injected `now_ms`); journaling of EXPIRED outcomes happens in the
//! handler layer that consumes [`ApprovalQueue::expire_due`].
//!
//! **Status (P11):** implemented by agent `bot-admin`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

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
    pub fn id_for(decision_id: &str, now_ms: u64) -> String {
        let preimage = format!("{decision_id}|{now_ms}");
        let digest = sentinel_core::audit::sha256(preimage.as_bytes());
        format!("ap-{}", hex::encode(&digest[..4]))
    }

    /// Insert under its `id` (produced by [`ApprovalQueue::id_for`]);
    /// returns the id.
    pub fn enqueue(&self, approval: PendingApproval) -> String {
        let id = approval.id.clone();
        self.lock().insert(id.clone(), approval);
        id
    }

    /// Fetch a clone.
    pub fn get(&self, id: &str) -> Option<PendingApproval> {
        self.lock().get(id).cloned()
    }

    /// Remove and return (approve/deny path).
    pub fn remove(&self, id: &str) -> Option<PendingApproval> {
        self.lock().remove(id)
    }

    /// Remove and return everything expired at `now_ms` (expiry at exactly
    /// `expires_ms` counts as expired), in deterministic id order.
    pub fn expire_due(&self, now_ms: u64) -> Vec<PendingApproval> {
        let mut guard = self.lock();
        let mut due: Vec<String> = guard
            .iter()
            .filter(|(_, approval)| now_ms >= approval.expires_ms)
            .map(|(id, _)| id.clone())
            .collect();
        due.sort();
        let mut expired = Vec::with_capacity(due.len());
        for id in due {
            if let Some(approval) = guard.remove(&id) {
                expired.push(approval);
            }
        }
        expired
    }

    /// Live count.
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .map(|guard| guard.len())
            .unwrap_or_default()
    }

    /// True when the queue holds no approvals.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lock helper: a poisoned lock (a panicking holder) still yields the
    /// map — the queue holds no cross-entry invariant to defend.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sentinel_core::order::{CloseSide, OrderType};
    use sentinel_core::types::MarketId;

    /// Minimal reduce-only order for queue entries.
    fn order(market_id: u32) -> OrderRequest {
        OrderRequest {
            market_id: MarketId(market_id),
            close: CloseSide::CloseLong,
            size: Decimal::new(1, 0),
            order_type: OrderType::Market,
            max_slippage_bps: 50,
            size_decimals: 2,
        }
    }

    /// Queue entry with a chosen id and expiry.
    fn pending(id: &str, created_ms: u64, expires_ms: u64) -> PendingApproval {
        PendingApproval {
            id: id.to_string(),
            market_id: 32,
            summary: "Close 1.0 ETH-PERP".to_string(),
            order: order(32),
            decision_ref: Some(7),
            created_ms,
            expires_ms,
        }
    }

    #[test]
    fn enqueue_get_remove_and_len() {
        let queue = ApprovalQueue::new();
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
        assert_eq!(queue.get("ap-00000000"), None);

        let created = 1_000;
        let id = ApprovalQueue::id_for("close:32", created);
        let entry = pending(&id, created, created + APPROVAL_TTL_MS);
        assert_eq!(queue.enqueue(entry.clone()), id, "enqueue returns the id");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.get(&id), Some(entry.clone()), "get returns a clone");

        let second = pending("ap-00000000", created, created + APPROVAL_TTL_MS);
        assert_eq!(queue.enqueue(second.clone()), "ap-00000000");
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.remove(&id), Some(entry), "remove returns the entry");
        assert_eq!(queue.remove(&id), None, "double remove is a no-op");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.get("ap-00000000"), Some(second));
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let queue = ApprovalQueue::new();
        let created = 10_000;
        let entry = pending("ap-00000001", created, created + APPROVAL_TTL_MS);
        let expires_ms = entry.expires_ms;
        queue.enqueue(entry.clone());

        assert!(
            queue.expire_due(expires_ms - 1).is_empty(),
            "one ms before expiry the entry stays queued"
        );
        assert_eq!(queue.len(), 1);

        let expired = queue.expire_due(expires_ms);
        assert_eq!(
            expired,
            vec![entry],
            "at exactly expires_ms the entry is expired"
        );
        assert_eq!(queue.len(), 0);
        assert!(queue.expire_due(expires_ms + 1).is_empty());
    }

    #[test]
    fn expire_due_is_selective_and_sorted_by_id() {
        let queue = ApprovalQueue::new();
        let created = 5_000;
        let soon = created + 1_000;
        let later = created + 60_000;
        let c = pending("ap-cccccccc", created, soon);
        let a = pending("ap-aaaaaaaa", created, soon);
        let b = pending("ap-bbbbbbbb", created, later);
        queue.enqueue(c.clone());
        queue.enqueue(a.clone());
        queue.enqueue(b.clone());

        let expired = queue.expire_due(soon);
        assert_eq!(
            expired,
            vec![a, c],
            "only due entries are removed and the order is by id"
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.get(&b.id), Some(b), "the not-yet-due entry remains");
        assert!(queue.expire_due(soon).is_empty());
    }

    #[test]
    fn expire_due_on_empty_queue_is_empty() {
        let queue = ApprovalQueue::new();
        assert!(queue.expire_due(u64::MAX).is_empty());
    }

    #[test]
    fn id_for_is_deterministic_and_well_formed() {
        let id = ApprovalQueue::id_for("close:32", 1_000);
        assert_eq!(
            id,
            ApprovalQueue::id_for("close:32", 1_000),
            "same input, same id"
        );
        // Independent vector: sha256("close:32|1000") first 8 hex.
        assert_eq!(id, "ap-6011dded");
        assert_eq!(
            ApprovalQueue::id_for("42", 1_700_000_000_000),
            "ap-93d1ade0"
        );
        assert_ne!(id, ApprovalQueue::id_for("close:32", 1_001), "ms matters");
        assert_ne!(id, ApprovalQueue::id_for("close:33", 1_000), "id matters");

        assert!(id.starts_with("ap-"));
        assert_eq!(id.len(), 11, "ap- + 8 hex chars");
        let hex_part = &id[3..];
        assert!(
            hex_part
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "8 lowercase hex chars: {hex_part}"
        );
    }
}
