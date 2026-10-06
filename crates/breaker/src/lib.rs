//! Sentinel breaker — the independent dead-man's-switch guardian (`SPEC-P14`).
//!
//! The breaker is a **process-independent** guardian that watches the
//! Sentinel daemon's on-chain heartbeats and, when they go stale while risk is
//! critical, performs a bounded, pre-authorized defensive reduce. It never
//! increases exposure. Module map:
//!
//! - [`config`] — exact env keys + fail-fast validation (`SPEC-P14` §2);
//! - [`watcher`] — alloy `eth_getLogs` polling of `Heartbeat` events on the
//!   anchor contract, latest event tracked per guardian (§3);
//! - [`trigger`] — staleness boundaries, criticality and the persisted
//!   one-fire-per-epoch gate (§3); fires arrive from the breaker's own 5 s
//!   automatic ticker (`run_auto`, the §7 demo behaviour) and from the armed
//!   POST surface (the CRE workflow, §6) — both share the same gate;
//! - [`snapshot`] — position sourcing: live Perpl REST, then snapshot file,
//!   then none (alert-only) (§4);
//! - [`executor`] — riskiest-position pick, clamp/quantize sizing, dry-run and
//!   last-resort testnet execution, journal + alert (§4);
//! - [`armed`] — HMAC-authenticated HTTP surface: `GET /api/heartbeat-status`
//!   and `POST /breaker/trigger` (§5).
//!
//! The binary entry point lives in `main.rs` and only wires these modules
//! together (watcher + auto trigger + armed server + graceful shutdown).

pub mod armed;
pub mod config;
pub mod executor;
pub mod snapshot;
pub mod trigger;
pub mod watcher;

#[cfg(test)]
pub(crate) mod test_support {
    //! Deterministic filesystem scaffolding for unit tests (no external
    //! dependencies; each directory is unique per process and sequence).

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// A unique temporary directory removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        /// Create a fresh directory tagged with `tag`.
        pub(crate) fn new(tag: &str) -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "breaker-test-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        /// A path inside the directory.
        pub(crate) fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
