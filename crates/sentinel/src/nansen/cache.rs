//! TTL cache for paid Nansen responses (cost control).
//!
//! Frozen by `SPEC-P09.md` §3.3 — hand-rolled (no moka), fully deterministic
//! with caller-supplied `now_ms`.
//!
//! **P09 status:** implemented (`endpoints` agent); interfaces frozen.

use std::collections::HashMap;

/// Deterministic cache key: `"{endpoint}|{canonical-json(params)}"`.
///
/// `serde_json::Value` objects are `BTreeMap`s, so the JSON is
/// key-sorted and stable across runs.
pub fn key(endpoint: &str, params: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(params).unwrap_or_else(|_| params.to_string());
    format!("{endpoint}|{canonical}")
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
    pub fn get(&mut self, key: &str, now: u64) -> Option<serde_json::Value> {
        let expired = matches!(
            self.entries.get(key),
            Some((expiry, _)) if now >= *expiry
        );
        if expired {
            self.entries.remove(key);
            return None;
        }
        self.entries.get(key).map(|(_, value)| value.clone())
    }

    /// Insert/replace an entry valid until `now + ttl_ms`.
    pub fn put(&mut self, key: &str, value: serde_json::Value, now: u64) {
        let expiry = now.saturating_add(self.ttl_ms);
        self.entries.insert(key.to_string(), (expiry, value));
    }

    /// Drop every expired entry.
    pub fn prune(&mut self, now: u64) {
        self.entries.retain(|_, (expiry, _)| now < *expiry);
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn key_is_deterministic_and_canonical() {
        let first = json!({"b": 1, "a": {"z": true, "y": [2, 1]}});
        let second = json!({"a": {"y": [2, 1], "z": true}, "b": 1});
        assert_eq!(key("/e", &first), key("/e", &second));
        assert_eq!(key("/e", &first), key("/e", &first.clone()));
        assert_eq!(key("/e", &json!({"a": 1})), "/e|{\"a\":1}");
        assert_ne!(key("/e", &json!({"a": 1})), key("/f", &json!({"a": 1})));
        assert_ne!(
            key("/e", &json!({"a": 1})),
            key("/e", &json!({"a": 2})),
            "different params ⇒ different keys"
        );
    }

    #[test]
    fn fresh_cache_is_empty() {
        let cache = TtlCache::new(1_000);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn put_then_get_is_a_hit_within_ttl() {
        let mut cache = TtlCache::new(1_000);
        cache.put("k", json!({"v": 1}), 100);
        assert_eq!(cache.get("k", 1_099), Some(json!({"v": 1})));
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
    }

    #[test]
    fn expiry_is_inclusive_and_get_drops_the_entry() {
        let mut cache = TtlCache::new(1_000);
        cache.put("k", json!({"v": 1}), 100); // expires at 1100
        assert_eq!(cache.get("k", 1_100), None);
        assert_eq!(cache.len(), 0, "expired entry dropped on access");
        assert!(cache.is_empty());
    }

    #[test]
    fn put_replaces_existing_entry() {
        let mut cache = TtlCache::new(1_000);
        cache.put("k", json!(1), 0);
        cache.put("k", json!(2), 10);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("k", 999), Some(json!(2)));
        assert_eq!(cache.get("k", 1_010), None, "refreshed expiry is 1010");
    }

    #[test]
    fn prune_drops_only_expired_entries() {
        let mut cache = TtlCache::new(1_000);
        cache.put("old", json!(1), 0); // expiry 1000
        cache.put("new", json!(2), 500); // expiry 1500
        cache.put("future", json!(3), 2_000); // expiry 3000
        cache.prune(1_000);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("old", 1_000), None);
        assert_eq!(cache.get("new", 1_000), Some(json!(2)));
        assert_eq!(cache.get("future", 1_000), Some(json!(3)));
        cache.prune(3_000);
        assert!(cache.is_empty(), "expiry is inclusive for prune too");
    }
}
