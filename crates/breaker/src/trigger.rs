//! Staleness, criticality and the persisted one-fire-per-epoch gate
//! (`SPEC-P14.md` §3).
//!
//! Rules (frozen):
//! - `stale <=> age_secs > stale_mult × heartbeat_interval` — **strict** `>`;
//!   at exactly 3× the threshold (default) the guardian is NOT stale;
//! - `critical <=> max_tier >= 2` (Orange or Red);
//! - `fire = stale AND critical`, enforced by [`fire_if_due`] on **both**
//!   trigger paths: the breaker's own 5 s automatic ticker ([`run_auto`], the
//!   §7 demo behaviour) and the armed `POST /breaker/trigger` surface (the CRE
//!   workflow, §6). A stale-but-not-critical guardian never fires;
//! - idempotency: at most one fire per `(guardian, epoch)` with
//!   `epoch = floor((now_ms − last_ts_ms) / interval_ms)`; the gate is
//!   persisted **atomically** (tmp file + rename) to `BREAKER_STATE_FILE` and a
//!   fresh heartbeat (new `last_ts_ms`) resets it so the next stale episode can
//!   fire again. The shared gate covers races between the two trigger paths.
//!
//! The state file is deterministic JSON (sorted guardian keys, decimals-free
//! integers only):
//!
//! ```json
//! { "guardians": { "0xf39f…2266": { "last_ts_ms": 1791200000000, "fired_epoch": 3 } } }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::MissedTickBehavior;
use tracing::info;

use crate::config::BreakerConfig;
use crate::executor::{BreakerExecutor, JournalLine};
use crate::watcher::{GuardianHeartbeat, WatcherState};

/// Tier byte at which a guardian counts as critical (Orange).
pub const CRITICAL_TIER: u8 = 2;

/// Cadence of the breaker's automatic stale+critical check (the §7 demo
/// behaviour): one local pass every 5 s, first pass after one full interval.
pub const DEFAULT_AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Canonical guardian key: lowercase `0x`-prefixed, used in the state file,
/// the status JSON and every log line.
pub fn guardian_key(guardian: &Address) -> String {
    format!("{guardian:#x}")
}

/// Current wall clock in milliseconds since the Unix epoch (`0` before 1970).
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since the last heartbeat (`0` when `last_ts_ms` is in the future).
pub fn age_secs(now_ms: u64, last_ts_ms: u64) -> u64 {
    now_ms.saturating_sub(last_ts_ms) / 1000
}

/// Staleness with the frozen STRICT comparison: `age > mult × interval`.
pub fn is_stale(age_secs: u64, interval_secs: u64, stale_mult: u64) -> bool {
    age_secs > stale_mult.saturating_mul(interval_secs)
}

/// Criticality: Orange (`2`) or Red (`3`) — and anything above, verbatim.
pub fn is_critical(max_tier: u8) -> bool {
    max_tier >= CRITICAL_TIER
}

/// Staleness epoch: `floor((now_ms − last_ts_ms) / interval_ms)`.
pub fn epoch(now_ms: u64, last_ts_ms: u64, interval_ms: u64) -> u64 {
    now_ms.saturating_sub(last_ts_ms) / interval_ms.max(1)
}

/// Persisted per-guardian fire gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FireState {
    /// `last_ts_ms` of the heartbeat generation this fire belongs to.
    pub last_ts_ms: u64,
    /// Highest epoch fired for that generation.
    pub fired_epoch: u64,
}

/// On-disk shape of the state file (sorted keys → deterministic bytes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FireStateFile {
    /// Guardian key → fire gate.
    pub guardians: BTreeMap<String, FireState>,
}

/// Outcome of one idempotency decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireDecision {
    /// Whether this `(guardian, epoch)` may fire.
    pub due: bool,
    /// The epoch the decision was computed for.
    pub epoch: u64,
}

/// Typed state-file failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// Read/write failure (the path is included; contents never are).
    Io {
        /// The offending path.
        path: PathBuf,
        /// OS error description.
        detail: String,
    },
    /// The state file is not valid JSON in the expected shape.
    Json(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::Io { path, detail } => {
                write!(f, "state file {}: {detail}", path.display())
            }
            StateError::Json(detail) => write!(f, "state file is malformed: {detail}"),
        }
    }
}

impl std::error::Error for StateError {}

/// Atomic, restart-persistent one-fire-per-epoch gate.
#[derive(Debug)]
pub struct FireStore {
    path: PathBuf,
    state: BTreeMap<String, FireState>,
}

impl FireStore {
    /// Load the gate from `path`; a missing file is an empty gate.
    ///
    /// # Errors
    /// [`StateError::Json`] when the file exists but is malformed,
    /// [`StateError::Io`] for any other read failure.
    pub fn load(path: PathBuf) -> Result<Self, StateError> {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let file: FireStateFile = serde_json::from_str(&text)
                    .map_err(|err| StateError::Json(format!("{}: {err}", path.display())))?;
                Ok(Self {
                    path,
                    state: file.guardians,
                })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path,
                state: BTreeMap::new(),
            }),
            Err(err) => Err(StateError::Io {
                path,
                detail: err.to_string(),
            }),
        }
    }

    /// The state file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stored gate for one guardian, if any.
    pub fn state_of(&self, guardian_key: &str) -> Option<&FireState> {
        self.state.get(guardian_key)
    }

    /// Idempotency decision for `(guardian, epoch)`.
    ///
    /// A different `last_ts_ms` than the stored one is a fresh heartbeat
    /// generation → reset (due). Within one generation, any epoch at or below
    /// the fired epoch is a duplicate.
    pub fn decision(
        &self,
        guardian_key: &str,
        now_ms: u64,
        last_ts_ms: u64,
        interval_ms: u64,
    ) -> FireDecision {
        let epoch = epoch(now_ms, last_ts_ms, interval_ms);
        let due = !matches!(
            self.state.get(guardian_key),
            Some(stored) if stored.last_ts_ms == last_ts_ms && stored.fired_epoch >= epoch
        );
        FireDecision { due, epoch }
    }

    /// Whether `(guardian, epoch)` already fired for the current heartbeat
    /// generation ("armed" component of the status payload).
    pub fn fired_this_epoch(
        &self,
        guardian_key: &str,
        now_ms: u64,
        last_ts_ms: u64,
        interval_ms: u64,
    ) -> bool {
        let epoch = epoch(now_ms, last_ts_ms, interval_ms);
        matches!(
            self.state.get(guardian_key),
            Some(stored) if stored.last_ts_ms == last_ts_ms && stored.fired_epoch == epoch
        )
    }

    /// Record a fire for `(guardian, last_ts_ms, epoch)` and persist atomically
    /// (tmp file + rename).
    ///
    /// # Errors
    /// [`StateError`] when the file cannot be written.
    pub fn mark_fired(
        &mut self,
        guardian_key: &str,
        last_ts_ms: u64,
        epoch: u64,
    ) -> Result<(), StateError> {
        self.state.insert(
            guardian_key.to_string(),
            FireState {
                last_ts_ms,
                fired_epoch: epoch,
            },
        );
        self.save()
    }

    /// Current on-disk snapshot of the gate (tests/evidence).
    pub fn snapshot(&self) -> FireStateFile {
        FireStateFile {
            guardians: self.state.clone(),
        }
    }

    /// Deterministic pretty JSON, written to `<path>.tmp` then renamed.
    fn save(&self) -> Result<(), StateError> {
        let mut json = serde_json::to_string_pretty(&self.snapshot())
            .map_err(|err| StateError::Json(err.to_string()))?;
        json.push('\n');
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| StateError::Io {
                path: parent.to_path_buf(),
                detail: err.to_string(),
            })?;
        }
        let tmp = tmp_path(&self.path);
        std::fs::write(&tmp, json).map_err(|err| StateError::Io {
            path: tmp.clone(),
            detail: err.to_string(),
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|err| StateError::Io {
            path: self.path.clone(),
            detail: err.to_string(),
        })
    }
}

/// `<path>.tmp` — deterministic sibling used for the atomic write.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// One guardian's status as exposed by `GET /api/heartbeat-status` (§5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuardianStatus {
    /// Lowercase `0x` address.
    pub address: String,
    /// Block timestamp of the last heartbeat (ms); `0` when never seen.
    pub last_ts_ms: u64,
    /// Seconds since the last heartbeat (age of "never seen" = full age).
    pub age_secs: u64,
    /// Raw `maxTier` byte (last known; `0` when never seen).
    pub max_tier: u8,
    /// `age > stale_mult × interval` (strict).
    pub stale: bool,
    /// `max_tier >= 2`.
    pub critical: bool,
    /// Would fire now (stale && critical && not yet fired this epoch) or
    /// already fired in this epoch.
    pub armed: bool,
}

/// Compute [`GuardianStatus`] for one guardian.
pub fn status_for(
    cfg: &BreakerConfig,
    guardian: &Address,
    heartbeat: Option<&GuardianHeartbeat>,
    fires: &FireStore,
    now_ms: u64,
) -> GuardianStatus {
    let key = guardian_key(guardian);
    let last_ts_ms = heartbeat.map(|hb| hb.last_ts_ms).unwrap_or(0);
    let max_tier = heartbeat.map(|hb| hb.max_tier).unwrap_or(0);
    let age = age_secs(now_ms, last_ts_ms);
    let stale = is_stale(age, cfg.heartbeat_interval_secs, cfg.stale_mult);
    let critical = is_critical(max_tier);
    let fired = fires.fired_this_epoch(&key, now_ms, last_ts_ms, cfg.heartbeat_interval_ms());
    let due = fires
        .decision(&key, now_ms, last_ts_ms, cfg.heartbeat_interval_ms())
        .due;
    let armed = fired || (stale && critical && due);
    GuardianStatus {
        address: key,
        last_ts_ms,
        age_secs: age,
        max_tier,
        stale,
        critical,
        armed,
    }
}

/// Result of one `fire_if_due` call.
#[derive(Debug, Clone, PartialEq)]
pub enum FireOutcome {
    /// The `(guardian, epoch)` pair already fired for this heartbeat
    /// generation; nothing new was executed.
    Duplicate {
        /// The (duplicate) epoch.
        epoch: u64,
    },
    /// Stale but not critical (`max_tier < 2`): the §3 fire predicate
    /// `stale AND critical` is not satisfied, so nothing fired.
    NotCritical {
        /// The epoch the request landed in.
        epoch: u64,
    },
    /// A new fire was accepted, executed and journaled.
    Fired {
        /// The fired epoch.
        epoch: u64,
        /// The journal line written for the action.
        journal: JournalLine,
    },
}

/// Evaluate the §3 fire predicate and, when it holds for an unclaimed epoch,
/// execute the defensive reduce + journal + alert exactly once.
///
/// Order of gates:
/// 1. the guardian must be **critical** (`max_tier >= 2`) — `stale AND
///    critical` is the frozen fire predicate (§3); a stale non-critical
///    request is accepted but performs nothing;
/// 2. the `(guardian, epoch)` gate must be unclaimed — the claim is persisted
///    **before** execution starts, so a crash mid-action cannot double-fire.
///
/// `now_ms` is passed in (deterministic tests; callers use [`unix_ms`]).
///
/// # Errors
/// [`StateError`] when the gate cannot be persisted.
pub async fn fire_if_due(
    cfg: &BreakerConfig,
    watched: &RwLock<WatcherState>,
    fires: &Mutex<FireStore>,
    executor: &BreakerExecutor,
    guardian: Address,
    reason: &str,
    now_ms: u64,
) -> Result<FireOutcome, StateError> {
    let key = guardian_key(&guardian);
    let heartbeat = watched.read().await.latest(&guardian).cloned();
    let last_ts_ms = heartbeat.as_ref().map(|hb| hb.last_ts_ms).unwrap_or(0);
    let max_tier = heartbeat.as_ref().map(|hb| hb.max_tier).unwrap_or(0);
    let interval_ms = cfg.heartbeat_interval_ms();

    // Gate 1: the frozen fire predicate — stale (checked by the caller) AND
    // critical (checked here, the single source of truth).
    if !is_critical(max_tier) {
        let epoch = epoch(now_ms, last_ts_ms, interval_ms);
        info!(
            guardian = %key,
            epoch,
            max_tier,
            tier = crate::watcher::tier_name(max_tier),
            "breaker trigger: guardian is not critical; not firing"
        );
        return Ok(FireOutcome::NotCritical { epoch });
    }

    // Gate 2: decision + claim under one lock: no other task can slip in
    // between "due" and "claimed".
    let epoch = {
        let mut store = fires.lock().await;
        let decision = store.decision(&key, now_ms, last_ts_ms, interval_ms);
        if !decision.due {
            info!(
                guardian = %key,
                epoch = decision.epoch,
                "breaker trigger: epoch already fired; not firing"
            );
            return Ok(FireOutcome::Duplicate {
                epoch: decision.epoch,
            });
        }
        store.mark_fired(&key, last_ts_ms, decision.epoch)?;
        decision.epoch
    };

    let age = age_secs(now_ms, last_ts_ms);
    info!(
        guardian = %key,
        epoch,
        age_secs = age,
        max_tier,
        tier = crate::watcher::tier_name(max_tier),
        reason,
        "breaker trigger: firing"
    );
    let journal = executor
        .execute(&guardian, epoch, age, max_tier, reason)
        .await;
    Ok(FireOutcome::Fired { epoch, journal })
}

/// The breaker's automatic stale+critical check (the §7 demo behaviour).
///
/// Runs one local pass every `check_interval` until `shutdown` flips; the
/// pass itself needs no RPC (it reads the watcher's heartbeat table) and a
/// due fire resolves the snapshot and executes. The first pass happens after
/// one full interval, so a caller's immediate manual trigger is never raced
/// by a startup tick; afterwards the shared [`fire_if_due`] gate makes the
/// two trigger paths idempotent per epoch.
pub async fn run_auto(
    cfg: Arc<BreakerConfig>,
    watched: Arc<RwLock<WatcherState>>,
    fires: Arc<Mutex<FireStore>>,
    executor: Arc<BreakerExecutor>,
    check_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    info!(
        guardians = cfg.guardians.len(),
        cadence_secs = check_interval.as_secs(),
        "breaker trigger: auto-fire loop started"
    );
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + check_interval, check_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {
                check_all(&cfg, &watched, &fires, &executor).await;
            }
        }
    }
    info!("breaker trigger: auto-fire loop stopped");
}

/// One automatic pass over every configured guardian.
async fn check_all(
    cfg: &Arc<BreakerConfig>,
    watched: &Arc<RwLock<WatcherState>>,
    fires: &Arc<Mutex<FireStore>>,
    executor: &Arc<BreakerExecutor>,
) {
    let now_ms = unix_ms();
    let candidates: Vec<(Address, u64, u8)> = {
        let guard = watched.read().await;
        cfg.guardians
            .iter()
            .map(|guardian| {
                let heartbeat = guard.latest(guardian);
                (
                    *guardian,
                    heartbeat.map(|hb| hb.last_ts_ms).unwrap_or(0),
                    heartbeat.map(|hb| hb.max_tier).unwrap_or(0),
                )
            })
            .collect()
    };

    for (guardian, last_ts_ms, max_tier) in candidates {
        let age = age_secs(now_ms, last_ts_ms);
        if !is_stale(age, cfg.heartbeat_interval_secs, cfg.stale_mult) {
            continue;
        }
        // Criticality is enforced inside `fire_if_due` (single source of
        // truth); the precheck only avoids per-tick chatter for Green/Yellow.
        if !is_critical(max_tier) {
            continue;
        }
        match fire_if_due(
            cfg,
            watched,
            fires,
            executor,
            guardian,
            "auto: heartbeat stale and risk critical",
            now_ms,
        )
        .await
        {
            Ok(FireOutcome::Fired { epoch, .. }) => info!(
                guardian = %guardian_key(&guardian),
                epoch,
                "breaker trigger: auto fire completed"
            ),
            Ok(_) => {}
            Err(err) => {
                // `warn` is not imported in this module's scope after the
                // trigger refactor; use tracing's full path.
                tracing::warn!(
                    guardian = %guardian_key(&guardian),
                    error = %err,
                    "breaker trigger: auto fire failed to persist; will retry next tick"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BreakerConfig;
    use crate::executor::BreakerExecutor;
    use crate::test_support::TempDir;
    use crate::watcher::WatcherState;

    use alloy::primitives::B256;
    use std::collections::HashMap;
    use std::sync::Arc;

    const GUARDIAN: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    fn keys() -> Vec<String> {
        vec![guardian_key(&GUARDIAN.parse().expect("guardian"))]
    }

    fn test_cfg(state_file: &std::path::Path, journal: &std::path::Path) -> BreakerConfig {
        test_cfg_multi(state_file, journal, GUARDIAN)
    }

    fn test_cfg_multi(
        state_file: &std::path::Path,
        journal: &std::path::Path,
        guardians: &str,
    ) -> BreakerConfig {
        let vars = HashMap::from([
            (
                "BREAKER_ANCHOR_ADDRESS".to_string(),
                "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
            ),
            ("BREAKER_GUARDIANS".to_string(), guardians.to_string()),
            ("BREAKER_ARM_SECRET".to_string(), "test-secret".to_string()),
            (
                "BREAKER_STATE_FILE".to_string(),
                state_file.display().to_string(),
            ),
            ("BREAKER_JOURNAL".to_string(), journal.display().to_string()),
        ]);
        BreakerConfig::from_vars(vars).expect("test config")
    }

    fn heartbeat(block: u64, last_ts_ms: u64, max_tier: u8) -> GuardianHeartbeat {
        GuardianHeartbeat {
            guardian: GUARDIAN.parse().expect("guardian"),
            last_ts_ms,
            max_tier,
            open_positions: 1,
            risk_state_hash: B256::ZERO,
            block_number: block,
            log_index: 0,
        }
    }

    #[test]
    fn staleness_boundary_is_strict() {
        // Default: interval 60, mult 3 → threshold 180 s.
        assert!(!is_stale(0, 60, 3));
        assert!(!is_stale(179, 60, 3));
        assert!(!is_stale(180, 60, 3), "== 3× is NOT stale");
        assert!(is_stale(181, 60, 3), "3× + 1 s IS stale");
        // A different multiplier keeps the same strictness.
        assert!(!is_stale(60, 60, 1));
        assert!(is_stale(61, 60, 1));
        // Zero interval is degenerate but total: threshold 0 → any age > 0.
        assert!(is_stale(1, 0, 3));
        assert!(!is_stale(0, 0, 3));
    }

    #[test]
    fn criticality_threshold_is_orange() {
        assert!(!is_critical(0));
        assert!(!is_critical(1));
        assert!(is_critical(2), "Orange is critical");
        assert!(is_critical(3), "Red is critical");
        assert!(
            is_critical(4),
            "unknown above-Red stays critical (verbatim)"
        );
    }

    #[test]
    fn epoch_is_floor_of_interval_count() {
        // 60 s interval → 60 000 ms denominator.
        assert_eq!(epoch(1_000, 0, 60_000), 0);
        assert_eq!(epoch(59_999, 0, 60_000), 0);
        assert_eq!(epoch(60_000, 0, 60_000), 1);
        assert_eq!(epoch(180_000, 0, 60_000), 3);
        assert_eq!(epoch(180_001, 0, 60_000), 3);
        assert_eq!(epoch(240_000, 0, 60_000), 4);
        // last_ts in the future saturates to epoch 0 (never panics).
        assert_eq!(epoch(0, 1_000, 60_000), 0);
        // Zero interval is clamped to 1 ms (no division by zero).
        assert_eq!(epoch(10, 0, 0), 10);
    }

    #[test]
    fn fire_store_is_idempotent_across_restarts_and_resets() {
        let dir = TempDir::new("fire-store");
        let path = dir.join("breaker-state.json");
        let key = &keys()[0];
        let interval_ms = 60_000;

        let mut store = FireStore::load(path.clone()).expect("load empty");
        assert!(store.state_of(key).is_none());

        // First decision for epoch 0 is due.
        let decision = store.decision(key, 10_000, 0, interval_ms);
        assert_eq!(
            decision,
            FireDecision {
                due: true,
                epoch: 0
            }
        );
        store.mark_fired(key, 0, decision.epoch).expect("mark");

        // The file exists, is deterministic JSON, and no tmp file is left.
        assert!(path.exists());
        assert!(!tmp_path(&path).exists(), "atomic rename cleans up the tmp");
        let text = std::fs::read_to_string(&path).expect("read state");
        assert!(text.ends_with('\n'));
        assert!(text.contains("\"fired_epoch\": 0"), "{text}");
        let parsed: FireStateFile = serde_json::from_str(&text).expect("state JSON");
        assert_eq!(
            parsed.guardians.get(key),
            Some(&FireState {
                last_ts_ms: 0,
                fired_epoch: 0
            })
        );

        // Same epoch → duplicate; a higher epoch → due again.
        assert!(!store.decision(key, 10_000, 0, interval_ms).due);
        assert!(store.decision(key, 70_000, 0, interval_ms).due);
        store.mark_fired(key, 0, 1).expect("mark epoch 1");

        // Restart: reload from disk — epoch 1 stays fired, epoch 2 is due.
        let reloaded = FireStore::load(path.clone()).expect("reload");
        assert!(!reloaded.decision(key, 70_000, 0, interval_ms).due);
        assert!(reloaded.decision(key, 130_000, 0, interval_ms).due);

        // A fresh heartbeat (new last_ts_ms) resets the gate entirely.
        assert!(reloaded.decision(key, 200_000, 200_000, interval_ms).due);
        assert!(!reloaded.fired_this_epoch(key, 200_000, 200_000, interval_ms));
    }

    #[test]
    fn fire_store_missing_file_is_empty_and_malformed_is_typed() {
        let dir = TempDir::new("fire-store-errors");
        let missing = dir.join("nope.json");
        let store = FireStore::load(missing).expect("missing file loads empty");
        assert!(store.snapshot().guardians.is_empty());

        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").expect("write bad file");
        let err = FireStore::load(bad).expect_err("malformed must fail");
        assert!(matches!(err, StateError::Json(_)), "{err:?}");
    }

    #[test]
    fn guardian_keys_and_state_files_are_lowercase_and_sorted() {
        let dir = TempDir::new("fire-store-sorted");
        let path = dir.join("state.json");
        let mut store = FireStore::load(path.clone()).expect("load");

        let a = "0x00000000000000000000000000000000000000AA"
            .parse::<Address>()
            .expect("a");
        let b = "0x00000000000000000000000000000000000000bb"
            .parse::<Address>()
            .expect("b");
        assert_eq!(
            guardian_key(&a),
            "0x00000000000000000000000000000000000000aa"
        );

        // Insert out of order: the BTreeMap serialization must sort the keys.
        store.mark_fired(&guardian_key(&b), 0, 1).expect("mark b");
        store.mark_fired(&guardian_key(&a), 0, 1).expect("mark a");
        let text = std::fs::read_to_string(&path).expect("read");
        let pos_a = text.find(&guardian_key(&a)).expect("key a present");
        let pos_b = text.find(&guardian_key(&b)).expect("key b present");
        assert!(pos_a < pos_b, "sorted keys: {text}");
    }

    #[test]
    fn status_reflects_stale_critical_and_armed() {
        let dir = TempDir::new("status");
        let cfg = test_cfg(&dir.join("state.json"), &dir.join("journal.jsonl"));
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let key = guardian_key(&guardian);
        let now = 1_000_000_000u64;
        let fires = FireStore::load(cfg.state_file.clone()).expect("empty");

        // Fresh, critical → not stale, not armed.
        let fresh = heartbeat(1, now - 30_000, 3);
        let status = status_for(&cfg, &guardian, Some(&fresh), &fires, now);
        assert_eq!(status.address, key);
        assert_eq!(status.last_ts_ms, now - 30_000);
        assert_eq!(status.age_secs, 30);
        assert_eq!(status.max_tier, 3);
        assert!(!status.stale && status.critical && !status.armed);

        // Exactly 3× (180 s) → still NOT stale.
        let boundary = heartbeat(2, now - 180_000, 3);
        let status = status_for(&cfg, &guardian, Some(&boundary), &fires, now);
        assert!(!status.stale, "== 3× is not stale");
        assert!(!status.armed);

        // Stale + critical, never fired → armed.
        let stale = heartbeat(3, now - 181_000, 2);
        let status = status_for(&cfg, &guardian, Some(&stale), &fires, now);
        assert!(status.stale && status.critical && status.armed);

        // Stale but not critical → not armed.
        let stale_green = heartbeat(4, now - 181_000, 1);
        let status = status_for(&cfg, &guardian, Some(&stale_green), &fires, now);
        assert!(status.stale && !status.critical && !status.armed);

        // Already fired this epoch → armed stays true even though not due.
        let mut fired = FireStore::load(cfg.state_file.clone()).expect("load");
        let epoch_now = epoch(now, stale.last_ts_ms, cfg.heartbeat_interval_ms());
        fired
            .mark_fired(&key, stale.last_ts_ms, epoch_now)
            .expect("mark");
        let status = status_for(&cfg, &guardian, Some(&stale), &fired, now);
        assert!(status.armed, "already fired this epoch → armed");
        assert!(fired.fired_this_epoch(&key, now, stale.last_ts_ms, cfg.heartbeat_interval_ms()));

        // Never seen → stale (age = full age), tier 0 → not critical, not armed.
        let status = status_for(&cfg, &guardian, None, &fires, now);
        assert_eq!(status.last_ts_ms, 0);
        assert_eq!(status.max_tier, 0);
        assert!(status.stale && !status.critical && !status.armed);
    }

    #[tokio::test]
    async fn fire_if_due_fires_once_per_epoch_and_survives_restart() {
        let dir = TempDir::new("fire");
        let cfg = test_cfg(&dir.join("state.json"), &dir.join("journal.jsonl"));
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let watched = RwLock::new(WatcherState::new());
        watched.write().await.record(heartbeat(1, 1_000_000, 3));
        let fires = Mutex::new(FireStore::load(cfg.state_file.clone()).expect("load"));
        let executor = BreakerExecutor::new(Arc::new(test_cfg(
            &dir.join("state.json"),
            &dir.join("journal.jsonl"),
        )));

        // now = last_ts + 3× interval + 1 s: epoch 3, first fire.
        let now = 1_000_000 + 181_000;
        let outcome = fire_if_due(
            &cfg,
            &watched,
            &fires,
            &executor,
            guardian,
            "unit-test",
            now,
        )
        .await
        .expect("fire");
        let epoch_fired = match outcome {
            FireOutcome::Fired { epoch, ref journal } => {
                assert_eq!(journal.fraction, Some(cfg.fraction));
                assert_eq!(journal.guardian, guardian_key(&guardian));
                epoch
            }
            other => panic!("expected Fired, got {other:?}"),
        };
        assert_eq!(epoch_fired, 3);

        // Same epoch again → Duplicate, no new journal line.
        let outcome = fire_if_due(
            &cfg,
            &watched,
            &fires,
            &executor,
            guardian,
            "unit-test",
            now,
        )
        .await
        .expect("second call");
        assert_eq!(outcome, FireOutcome::Duplicate { epoch: 3 });

        // Restart the store from disk → still duplicate at the same epoch.
        let reloaded = Mutex::new(FireStore::load(cfg.state_file.clone()).expect("reload"));
        let outcome = fire_if_due(
            &cfg,
            &watched,
            &reloaded,
            &executor,
            guardian,
            "unit-test",
            now,
        )
        .await
        .expect("after restart");
        assert_eq!(outcome, FireOutcome::Duplicate { epoch: 3 });

        // The next epoch is a new fire.
        let outcome = fire_if_due(
            &cfg,
            &watched,
            &reloaded,
            &executor,
            guardian,
            "unit-test",
            now + cfg.heartbeat_interval_ms(),
        )
        .await
        .expect("next epoch");
        assert!(matches!(outcome, FireOutcome::Fired { epoch: 4, .. }));

        // A fresh heartbeat (new last_ts) resets: due again.
        watched.write().await.record(heartbeat(2, now, 3));
        let outcome = fire_if_due(
            &cfg,
            &watched,
            &reloaded,
            &executor,
            guardian,
            "unit-test",
            now + 1_000,
        )
        .await
        .expect("fresh heartbeat");
        assert!(matches!(outcome, FireOutcome::Fired { epoch: 0, .. }));

        // Journal has exactly three lines (one per fire).
        let text = std::fs::read_to_string(&cfg.journal).expect("journal");
        assert_eq!(text.lines().count(), 3, "one line per fire: {text}");
    }

    #[tokio::test]
    async fn fire_if_due_requires_criticality() {
        let dir = TempDir::new("not-critical");
        let cfg = test_cfg(&dir.join("state.json"), &dir.join("journal.jsonl"));
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let watched = RwLock::new(WatcherState::new());
        // Stale (300 s) but Green — the §3 fire predicate fails.
        watched.write().await.record(heartbeat(1, 1_000_000, 0));
        let fires = Mutex::new(FireStore::load(cfg.state_file.clone()).expect("load"));
        let executor = BreakerExecutor::new(Arc::new(test_cfg(
            &dir.join("state.json"),
            &dir.join("journal.jsonl"),
        )));

        let outcome = fire_if_due(
            &cfg,
            &watched,
            &fires,
            &executor,
            guardian,
            "unit-test",
            1_300_000,
        )
        .await
        .expect("decision");
        assert!(
            matches!(outcome, FireOutcome::NotCritical { .. }),
            "{outcome:?}"
        );
        // No gate was claimed and no journal line was written.
        assert!(fires.lock().await.state_of(&keys()[0]).is_none());
        assert!(!cfg.journal.exists());
    }

    #[tokio::test]
    async fn run_auto_fires_stale_critical_guardians_only() {
        let dir = TempDir::new("auto");
        let guardian_a = GUARDIAN;
        let guardian_b = "0x00000000000000000000000000000000000000cc";
        let cfg = Arc::new(test_cfg_multi(
            &dir.join("state.json"),
            &dir.join("journal.jsonl"),
            &format!("{guardian_a},{guardian_b}"),
        ));
        let watched = Arc::new(RwLock::new(WatcherState::new()));
        let now = unix_ms();
        // Guardian A: stale (300 s) + critical → the ticker fires it.
        // Guardian B: fresh + critical → skipped (not stale).
        watched.write().await.record(heartbeat(1, now - 300_000, 3));
        let mut fresh = heartbeat(1, now - 1_000, 3);
        fresh.guardian = guardian_b.parse().expect("guardian b");
        watched.write().await.record(fresh);
        let fires = Arc::new(Mutex::new(
            FireStore::load(cfg.state_file.clone()).expect("load"),
        ));
        let executor = Arc::new(BreakerExecutor::new(Arc::clone(&cfg)));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(run_auto(
            Arc::clone(&cfg),
            Arc::clone(&watched),
            Arc::clone(&fires),
            Arc::clone(&executor),
            Duration::from_millis(30),
            shutdown_rx,
        ));

        // Wait for the fire to land in the journal, then stop.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if cfg.journal.exists()
                && std::fs::read_to_string(&cfg.journal)
                    .map(|text| !text.is_empty())
                    .unwrap_or(false)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no journal line in time"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run_auto stops promptly")
            .expect("task joins");

        let text = std::fs::read_to_string(&cfg.journal).expect("journal");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "exactly one auto fire: {text}");
        let line: serde_json::Value = serde_json::from_str(lines[0]).expect("journal JSON");
        assert_eq!(
            line["guardian"].as_str(),
            Some(guardian_key(&guardian_a.parse().expect("a")).as_str()),
            "only the stale guardian fired"
        );
        assert!(
            line["detail"].as_str().unwrap_or("").contains("auto:"),
            "auto reason recorded: {line}"
        );
        let store = fires.lock().await;
        assert!(store.state_of(&keys()[0]).is_some(), "gate persisted for A");
        assert!(
            store
                .state_of(&guardian_key(&guardian_b.parse().expect("b")))
                .is_none(),
            "no gate for the fresh guardian"
        );
    }
}
