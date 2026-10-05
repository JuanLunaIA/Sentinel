//! Hash-chained audit journal — tamper-evident decision log.
//!
//! Every decision (intent, policy verdict, execution outcome) is journaled
//! **before** the action it authorizes and appended with its outcome after
//! (P00 invariant #2: audit-before-action). Entries form a SHA-256 hash
//! chain whose head is periodically anchored on Monad by the
//! `SentinelAuditAnchor` contract (P10).
//!
//! **STUB in P02** — the chain construction, canonical pre-image and
//! tamper-detection tests land in P10.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// SHA-256 digest helper (also used to hash decision payloads).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// One entry of the tamper-evident decision journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonically increasing sequence number, starting at 0.
    pub seq: u64,
    /// Wall-clock timestamp assigned when the entry was created.
    pub ts: DateTime<Utc>,
    /// SHA-256 of the canonical payload (decision intent, verdict, outcome).
    pub payload_hash: [u8; 32],
    /// Hash of the previous entry; all-zero for the genesis entry.
    pub prev_hash: [u8; 32],
    /// This entry's hash: binds `seq`, `ts` and `prev_hash` to the payload.
    pub entry_hash: [u8; 32],
}

/// Compute the canonical entry hash.
///
/// **STUB — canonical pre-image frozen in P10** (must match on-chain anchor
/// verification exactly; changing it is a breaking format change).
pub fn entry_hash(
    prev_hash: &[u8; 32],
    seq: u64,
    ts_millis: i64,
    payload_hash: &[u8; 32],
) -> [u8; 32] {
    let _ = (prev_hash, seq, ts_millis, payload_hash);
    todo!("P10: freeze canonical pre-image")
}

/// Verify a full chain, detecting any tampering (altered payload, reordered,
/// inserted or removed entries).
///
/// **STUB — implemented in P10.**
pub fn verify_chain(entries: &[AuditEntry]) -> bool {
    let _ = entries;
    todo!("P10: walk the chain")
}
