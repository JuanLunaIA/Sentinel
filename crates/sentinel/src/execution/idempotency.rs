//! Idempotency store — duplicate suppression with restart safety.
//!
//! In-memory map keyed by the SPEC-P05 §5 idempotency key, JSON-persisted to
//! disk on every record (`data/idempotency.json`) and reloaded on
//! construction. The window is the callers' TTL (IDEMPOTENCY_WINDOW_SECS).
//!
//! **P05 status:** implemented; interfaces frozen per `SPEC-P05.md` §5.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{PerplError, Result};

/// In-memory duplicate-suppression window with JSON persistence.
///
/// The window is fixed from the **first** record of a key: a duplicate seen
/// inside the window is not re-stamped, so the key expires exactly `window`
/// after it was first recorded (`now − last >= window` allows again). A
/// missing file loads as an empty store; a file that exists but cannot be
/// read or parsed is an error.
#[derive(Debug)]
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
    pub fn load(window: Duration, path: PathBuf) -> Result<Self> {
        let seen = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|err| {
                PerplError::Order(format!(
                    "idempotency store {}: invalid JSON: {err}",
                    path.display()
                ))
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                return Err(PerplError::Order(format!(
                    "idempotency store {}: {err}",
                    path.display()
                ))
                .into());
            }
        };
        Ok(Self { window, path, seen })
    }

    /// The duplicate-suppression window this store was constructed with.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// `true` when `key` has not been seen within the window.
    ///
    /// A missing or expired key is recorded at `now_ms` and the store is
    /// persisted (parent directories are created on demand). A duplicate
    /// inside the window returns `false` **without refreshing** the key's
    /// timestamp — the window stays fixed from the first record — and without
    /// touching the file.
    ///
    /// # Errors
    /// `PerplError::Order` when persisting fails.
    pub fn check_and_record(&mut self, key: &str, now_ms: u64) -> Result<bool> {
        let window_ms = self.window_ms();
        if let Some(&last_ms) = self.seen.get(key)
            && now_ms.saturating_sub(last_ms) < window_ms
        {
            return Ok(false);
        }
        self.seen.insert(key.to_string(), now_ms);
        self.persist()?;
        Ok(true)
    }

    /// Drop entries whose window has fully elapsed at `now_ms`.
    pub fn prune(&mut self, now_ms: u64) {
        let window_ms = self.window_ms();
        self.seen
            .retain(|_, &mut last_ms| now_ms.saturating_sub(last_ms) < window_ms);
    }

    /// Window in milliseconds (saturating for absurdly large durations).
    fn window_ms(&self) -> u64 {
        u64::try_from(self.window.as_millis()).unwrap_or(u64::MAX)
    }

    /// Write the whole map to `path` as JSON (creating parent directories).
    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                PerplError::Order(format!("idempotency store dir {}: {err}", parent.display()))
            })?;
        }
        let json = serde_json::to_string_pretty(&self.seen).map_err(|err| {
            PerplError::Order(format!("idempotency store {}: {err}", self.path.display()))
        })?;
        std::fs::write(&self.path, json).map_err(|err| {
            PerplError::Order(format!("idempotency store {}: {err}", self.path.display()))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap as Map;

    use super::*;
    use crate::error::SentinelError;

    fn parse_file(path: &PathBuf) -> Map<String, u64> {
        let text = std::fs::read_to_string(path).expect("store file exists");
        serde_json::from_str(&text).expect("store file is valid JSON")
    }

    #[test]
    fn fresh_key_allowed_recorded_and_persisted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idem.json");
        let mut store =
            IdempotencyStore::load(Duration::from_secs(1), path.clone()).expect("load empty store");

        assert!(
            store
                .check_and_record("32:reduce:50", 1_000)
                .expect("record")
        );
        assert_eq!(parse_file(&path).get("32:reduce:50"), Some(&1_000));

        // Still suppressed at one millisecond before the window; not refreshed.
        assert!(
            !store
                .check_and_record("32:reduce:50", 1_999)
                .expect("dup check")
        );
        assert!(
            !store
                .check_and_record("32:reduce:50", 1_999)
                .expect("dup check again")
        );

        // Exactly at the window it is allowed again (fixed window from first
        // record: had either duplicate refreshed the timestamp, this would
        // still be inside the window).
        assert!(
            store
                .check_and_record("32:reduce:50", 2_000)
                .expect("expired at exactly the window")
        );
        assert_eq!(parse_file(&path).get("32:reduce:50"), Some(&2_000));
    }

    #[test]
    fn missing_file_loads_empty_and_is_created_on_first_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/deep/idem.json");
        assert!(!path.exists());

        let mut store = IdempotencyStore::load(Duration::from_millis(500), path.clone())
            .expect("missing file = empty store");
        assert!(store.check_and_record("k", 10).expect("record"));
        // Parent directories are created on demand.
        assert_eq!(parse_file(&path).get("k"), Some(&10));
    }

    #[test]
    fn restart_reload_still_suppresses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idem.json");

        let mut store =
            IdempotencyStore::load(Duration::from_secs(1), path.clone()).expect("store");
        assert!(store.check_and_record("k", 5_000).expect("record"));
        drop(store);

        let mut reloaded =
            IdempotencyStore::load(Duration::from_secs(1), path.clone()).expect("reload");
        assert!(
            !reloaded
                .check_and_record("k", 5_500)
                .expect("dup after restart"),
            "a restart must not forget a live suppression window"
        );
        assert!(
            reloaded.check_and_record("k", 6_000).expect("expired"),
            "at exactly the window it is allowed again"
        );
    }

    #[test]
    fn prune_drops_only_expired_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idem.json");
        let mut store =
            IdempotencyStore::load(Duration::from_secs(1), path.clone()).expect("store");

        assert!(store.check_and_record("a", 10_000).expect("record a"));
        assert!(store.check_and_record("b", 10_500).expect("record b"));

        store.prune(11_000); // a: 1000ms elapsed → dropped; b: 500ms → kept

        assert!(store.check_and_record("a", 11_000).expect("a re-allowed"));
        assert!(!store.check_and_record("b", 11_400).expect("b suppressed"));
    }

    #[test]
    fn corrupt_file_is_an_order_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idem.json");
        std::fs::write(&path, "not json at all").expect("write corrupt file");

        let err = IdempotencyStore::load(Duration::from_secs(1), path)
            .expect_err("corrupt store must fail loudly");
        assert!(matches!(err, SentinelError::Perpl(PerplError::Order(_))));
    }
}
