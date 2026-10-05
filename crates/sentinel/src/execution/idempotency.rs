//! Idempotency store — duplicate suppression with restart safety.
//!
//! In-memory map keyed by the SPEC-P05 §5 idempotency key, JSON-persisted to
//! disk on every record (`data/idempotency.json`) and reloaded on
//! construction. The window is the callers' TTL (IDEMPOTENCY_WINDOW_SECS).
//!
//! **Skeleton status (P05):** implemented by the P05 wave.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::Result;

/// In-memory duplicate-suppression window with JSON persistence.
#[derive(Debug)]
#[allow(dead_code)] // stub fields; consumed by the P05 wave
pub struct IdempotencyStore {
    /// Entries older than this are pruned.
    window: Duration,
    /// JSON file the map is persisted to.
    path: PathBuf,
    /// key -> last-recorded unix ms.
    seen: HashMap<String, u64>,
}

impl IdempotencyStore {
    /// Load the store from `path` (missing file = empty), using `window` as TTL.
    ///
    /// # Errors
    /// `PerplError::Order` when the file exists but cannot be read/parsed.
    pub fn load(_window: Duration, _path: PathBuf) -> Result<Self> {
        todo!("P05 agent exec")
    }

    /// `true` when `key` has not been seen within the window.
    ///
    /// Records the key (and persists) on every call, allowed or not.
    ///
    /// # Errors
    /// `PerplError::Order` when persisting fails.
    pub fn check_and_record(&mut self, _key: &str, _now_ms: u64) -> Result<bool> {
        todo!("P05 agent exec")
    }

    /// Drop entries older than the window.
    pub fn prune(&mut self, _now_ms: u64) {
        todo!("P05 agent exec")
    }
}
