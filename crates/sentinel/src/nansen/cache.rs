//! TTL cache for paid Nansen responses (cost control).
//!
//! Frozen by `SPEC-P09.md` §3.3 — hand-rolled (no moka), fully deterministic
//! with caller-supplied `now_ms`.
//!
//! **Skeleton status (P09):** interfaces frozen; implemented by the P09 wave.

use std::collections::HashMap;

/// Deterministic cache key: `"{endpoint}|{canonical-json(params)}"`.
///
/// `serde_json::Value` objects are `BTreeMap`s, so the JSON is
/// key-sorted and stable across runs.
pub fn key(_endpoint: &str, _params: &serde_json::Value) -> String {
    todo!("P09 agent endpoints")
}

/// Entry-expiry TTL map (expiry inclusive: `now >= expiry` ⇒ miss).
pub struct TtlCache {
    /// Entry lifetime, ms.
    pub ttl_ms: u64,
    entries: HashMap<String, (u64, serde_json::Value)>,
}

impl TtlCache {
    /// Empty cache with `ttl_ms` lifetime per entry.
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            entries: HashMap::new(),
        }
    }

    /// Lookup at `now` (misses and drops expired entries).
    pub fn get(&mut self, _key: &str, _now: u64) -> Option<serde_json::Value> {
        todo!("P09 agent endpoints")
    }

    /// Insert/replace an entry valid until `now + ttl_ms`.
    pub fn put(&mut self, _key: &str, _value: serde_json::Value, _now: u64) {
        todo!("P09 agent endpoints")
    }

    /// Drop every expired entry.
    pub fn prune(&mut self, _now: u64) {
        todo!("P09 agent endpoints")
    }

    /// Live entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
