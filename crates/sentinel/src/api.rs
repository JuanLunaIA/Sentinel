//! Audit-facing HTTP API (merged into the daemon router by main; P15 extiende).
//!
//! Frozen by `SPEC-P10.md` §7.
//!
//! **Skeleton status (P10):** interfaces frozen; implemented by the P10 wave.

use std::sync::Arc;

use tokio::sync::Mutex;

use sentinel_core::audit::AuditJournal;

/// Router exposing `GET /api/audit?from_seq&limit` and
/// `GET /api/audit/verify`.
pub fn audit_router(_journal: Arc<Mutex<AuditJournal>>) -> axum::Router {
    todo!("P10 agent anchor-side")
}
