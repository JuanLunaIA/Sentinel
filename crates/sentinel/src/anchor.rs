//! On-chain anchoring service — journal head → `SentinelAuditAnchor`.
//!
//! Implemented per `SPEC-P10.md` §5. The journal is the source of truth; RPC
//! failures back off and retry, never losing entries.
//!
//! # Seq domains
//!
//! The journal is 0-based (first entry `seq = 0`); the contract's replay
//! guard is 1-based (`seq` must equal `lastSeq[msg.sender] + 1`, so the first
//! anchor is `1`). The service therefore anchors with
//! `on-chain seq = journal seq + 1` and tracks `next` = the next journal seq
//! to anchor — on startup that is exactly the contract's `lastSeq` (both
//! equal the number of entries already anchored).
//!
//! # Cadence (§5, freeze clarification)
//!
//! - heartbeat: immediately, then every `HEARTBEAT_INTERVAL_SECS`; the
//!   summary is canonical JSON
//!   `{"batches":u64,"entries_anchored":u64,"last_seq":u64}` counting **this
//!   service's** anchored work plus the journal cursor
//!   (`AuditJournal::seq()`), hashed as `{"max_tier":3,"summary":"…"}`.
//!   `max_tier = 3` and `open_positions = 0` are placeholders until P14
//!   supplies the real breaker inputs;
//! - batch: immediately, then every 60 s, **plus** a 500 ms poll that anchors
//!   as soon as any un-anchored entry has `trigger = REFLEX`;
//! - failures: exponential backoff 1 s → 30 s for both batch retries
//!   (keeping the same `from_seq`) and heartbeat retries; nothing is dropped
//!   (the journal stays the source of truth).
//!
//! # Heartbeat status file (`SPEC-P16` §2)
//!
//! After every successful beat **and** batch anchor the service writes
//! `data/heartbeat.json` (`HEARTBEAT_PATH` overrides the location) —
//! atomically, via a sibling `*.tmp` file renamed over the target — with
//! `{"ts_ms":u64,"tx_hash":str|null,"seq":u64}`. `ts_ms` is the write time
//! (unix ms), `tx_hash` the confirmed transaction hash, and `seq` the
//! journal cursor that success confirms: the last journal seq covered by a
//! batch anchor, or the journal head (`AuditJournal::seq()`) read for a
//! heartbeat summary. Writing is best-effort: failures are logged and never
//! disturb anchoring, and an RPC outage (nothing written) leaves the file
//! untouched.
//!
//! `AlloyAnchorSink::new` returns a PENDING-WALLET error while
//! `ANCHOR_CONTRACT_ADDRESS` / `RPC_SIGNER_KEY` are missing or placeholder
//! values (STUB-17).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use tokio::sync::{Mutex, watch};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use sentinel_core::audit::{AuditJournal, Trigger};

use crate::config::Config;
use crate::error::{Result, SentinelError};

/// Generated alloy bindings for the `SentinelAuditAnchor` contract (§4).
///
/// The interface mirrors the Solidity source: `mapping … public` state
/// variables expand to their ABI getters (`lastSeq(address)` /
/// `lastRoot(address)`) and every function/event gets typed call builders.
#[allow(missing_docs)] // the Solidity source documents the generated items
pub mod abi {
    alloy::sol! {
        #[sol(rpc)]
        contract SentinelAuditAnchor {
            event DecisionAnchored(uint64 seq, bytes32 entryHash, bytes32 runningRoot, address account);
            event Heartbeat(address guardian, bytes32 riskStateHash, uint32 openPositions, uint8 maxTier);

            error StaleSeq(uint64 got, uint64 want);

            mapping(address => uint64) public lastSeq;
            mapping(address => bytes32) public lastRoot;

            function anchor(uint64 seq, bytes32 entryHash, bytes32 runningRoot) external;
            function batchAnchor(uint64 fromSeq, bytes32[] calldata entryHashes, bytes32 root) external;
            function beat(bytes32 riskStateHash, uint32 openPositions, uint8 maxTier) external;
        }
    }
}

/// Seconds between periodic batch passes (`SPEC-P10` §5).
const BATCH_INTERVAL_SECS: u64 = 60;

/// How often the un-anchored tail is polled for a REFLEX entry (§5).
const REFLEX_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum entries read — and Merkle-rooted — per batch pass.
const BATCH_MAX_ENTRIES: usize = 64;

/// First retry delay after a sink failure.
const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Retry delay ceiling (`SPEC-P10` §5: 1 s → 30 s).
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Risk tier committed by heartbeats until P14 wires real breaker inputs.
const HEARTBEAT_MAX_TIER: u8 = 3;

/// What the run loop reports when it stops.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnchorRunReport {
    /// Successful batch anchors.
    pub batches: u64,
    /// Successfully anchored entries.
    pub entries_anchored: u64,
    /// Successful heartbeats.
    pub heartbeats: u64,
    /// Sink failures seen (batch attempts and heartbeats).
    pub failures: u64,
}

/// RPC-side abstraction (mockable in tests).
#[allow(async_fn_in_trait)]
pub trait AnchorSink {
    /// Anchor `entry_hashes` (seq range starting at `from_seq`) under `root`.
    async fn anchor_batch(
        &self,
        from_seq: u64,
        entry_hashes: &[String],
        root: &str,
    ) -> Result<String>;

    /// Post a heartbeat.
    async fn beat(
        &self,
        risk_state_hash: &str,
        open_positions: u32,
        max_tier: u8,
    ) -> Result<String>;

    /// Highest on-chain seq this sink has already recorded, when it can read
    /// the contract's `lastSeq` getter; `None` for offline/mock sinks.
    ///
    /// `run` resumes from it; the value equals the next journal seq to anchor
    /// (on-chain seq = journal seq + 1). When `None` the
    /// `ANCHOR_FROM_SEQ` env override (else 0) is used.
    async fn last_anchored_seq(&self) -> Option<u64> {
        None
    }
}

/// alloy-backed sink (real RPC).
pub struct AlloyAnchorSink {
    provider: DynProvider,
    contract: Address,
    wallet: PrivateKeySigner,
}

impl fmt::Debug for AlloyAnchorSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlloyAnchorSink")
            .field("contract", &self.contract)
            .field("signer", &self.wallet.address())
            .finish_non_exhaustive()
    }
}

impl AlloyAnchorSink {
    /// Build from `AnchorConfig` + RPC url (`RPC_SIGNER_KEY`,
    /// `ANCHOR_CONTRACT_ADDRESS`); clear PENDING-WALLET error when the key
    /// or address is missing/placeholder.
    ///
    /// The provider is `ProviderBuilder::new()` (recommended fillers + local
    /// wallet signing) pointed at `PERPL_RPC_URL`; on anvil that is the local
    /// endpoint, on Monad the configured testnet/mainnet RPC.
    ///
    /// # Errors
    /// `SentinelError::Internal` with a PENDING-WALLET message.
    pub fn new(cfg: &Config) -> Result<Self> {
        let contract_raw = match cfg.anchor.contract_address.as_deref().map(str::trim) {
            Some(value) if !is_placeholder(value) => value,
            _ => {
                return Err(pending_wallet(
                    "ANCHOR_CONTRACT_ADDRESS (deploy SentinelAuditAnchor, SPEC-P10 §4)",
                ));
            }
        };
        let contract: Address = contract_raw.parse().map_err(|err| {
            SentinelError::Internal(format!(
                "ANCHOR_CONTRACT_ADDRESS is not a 0x-prefixed address: {err}"
            ))
        })?;

        let key = cfg
            .anchor
            .rpc_signer_key
            .as_ref()
            .map(|secret| secret.expose().trim().to_string())
            .filter(|value| !is_placeholder(value))
            .ok_or_else(|| pending_wallet("RPC_SIGNER_KEY (funded anchor signer, STUB-17)"))?;
        // The parse error never echoes the key (or any part of it).
        let wallet: PrivateKeySigner = key.parse().map_err(|_| {
            SentinelError::Internal(
                "RPC_SIGNER_KEY was rejected (want 0x + 32 bytes of hex)".to_string(),
            )
        })?;

        let url = url::Url::parse(cfg.perpl.rpc_url.trim()).map_err(|err| {
            SentinelError::Internal(format!("PERPL_RPC_URL is not a valid URL: {err}"))
        })?;
        let provider = ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_http(url)
            .erased();
        Ok(Self {
            provider,
            contract,
            wallet,
        })
    }

    /// Contract address configured for this sink.
    pub fn contract_address(&self) -> Address {
        self.contract
    }

    /// Signer address that posts anchors/heartbeats.
    pub fn signer_address(&self) -> Address {
        self.wallet.address()
    }
}

impl AnchorSink for AlloyAnchorSink {
    async fn anchor_batch(
        &self,
        from_seq: u64,
        entry_hashes: &[String],
        root: &str,
    ) -> Result<String> {
        if entry_hashes.is_empty() {
            return Err(SentinelError::Internal(
                "anchor_batch: refusing an empty batch".to_string(),
            ));
        }
        let hashes: Vec<FixedBytes<32>> = entry_hashes
            .iter()
            .map(|hash| parse_hash(hash, "entry hash"))
            .collect::<Result<_>>()?;
        let root_hash = parse_hash(root, "merkle root")?;

        let contract = abi::SentinelAuditAnchor::new(self.contract, &self.provider);
        let receipt = contract
            .batchAnchor(from_seq, hashes, root_hash)
            .send()
            .await
            .map_err(|err| {
                SentinelError::Internal(format!("anchor_batch: transaction failed: {err}"))
            })?
            .get_receipt()
            .await
            .map_err(|err| {
                SentinelError::Internal(format!("anchor_batch: receipt unavailable: {err}"))
            })?;
        ensure_success(receipt.status(), receipt.transaction_hash)?;
        Ok(receipt.transaction_hash.to_string())
    }

    async fn beat(
        &self,
        risk_state_hash: &str,
        open_positions: u32,
        max_tier: u8,
    ) -> Result<String> {
        let hash = parse_hash(risk_state_hash, "risk state hash")?;
        let contract = abi::SentinelAuditAnchor::new(self.contract, &self.provider);
        let receipt = contract
            .beat(hash, open_positions, max_tier)
            .send()
            .await
            .map_err(|err| SentinelError::Internal(format!("beat: transaction failed: {err}")))?
            .get_receipt()
            .await
            .map_err(|err| SentinelError::Internal(format!("beat: receipt unavailable: {err}")))?;
        ensure_success(receipt.status(), receipt.transaction_hash)?;
        Ok(receipt.transaction_hash.to_string())
    }

    async fn last_anchored_seq(&self) -> Option<u64> {
        let contract = abi::SentinelAuditAnchor::new(self.contract, &self.provider);
        match contract.lastSeq(self.wallet.address()).call().await {
            Ok(seq) => Some(seq),
            Err(err) => {
                warn!(error = %err, "anchor: contract lastSeq unreadable; using ANCHOR_FROM_SEQ/0");
                None
            }
        }
    }
}

/// Pairwise sha256 Merkle root over hex hashes (`SPEC-P10` §5).
///
/// Each level hashes the 32 decoded bytes of neighbouring leaves
/// (`sha256(left ‖ right)`); an odd node is promoted unchanged; a single leaf
/// is returned as-is; an empty list is 64 zeros. Non-hex inputs are hashed as
/// raw bytes so the function stays total (journal hashes are always 64 hex —
/// §2 — so this is purely defensive).
pub fn merkle_root(hashes: &[String]) -> String {
    if hashes.is_empty() {
        return "0".repeat(64);
    }
    if hashes.len() == 1 {
        return hashes[0].clone();
    }
    let mut level: Vec<[u8; 32]> = hashes.iter().map(|hash| decode_hash(hash)).collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for chunk in level.chunks(2) {
            match chunk {
                [left, right] => next.push(hash_pair(left, right)),
                odd => next.extend_from_slice(odd),
            }
        }
        level = next;
    }
    let root = level.first().copied().unwrap_or([0_u8; 32]);
    hex::encode(root)
}

/// `sha256` hex over canonical `{"max_tier":t,"summary":"…"}`.
pub fn risk_state_hash(canonical_summary: &str, max_tier: u8) -> String {
    let summary_literal = serde_json::Value::String(canonical_summary.to_owned()).to_string();
    let canonical = format!("{{\"max_tier\":{max_tier},\"summary\":{summary_literal}}}");
    hex::encode(sentinel_core::audit::sha256(canonical.as_bytes()))
}

/// Run heartbeats + batch anchoring until `shutdown` flips.
///
/// See the module docs for the cadence. `last_anchored` resumes from
/// [`AnchorSink::last_anchored_seq`] when available, else `ANCHOR_FROM_SEQ`,
/// else 0. Journal read errors are logged and retried on the next tick;
/// in-flight sink calls are awaited to completion (never abandoned), so
/// shutdown never loses a possibly-landed transaction. Successful beats and
/// batch anchors update the heartbeat status file (`SPEC-P16` §2) at
/// `HEARTBEAT_PATH` (default `data/heartbeat.json`).
///
/// # Errors
/// `SentinelError::Internal` for startup-level problems only.
pub async fn run<S: AnchorSink>(
    cfg: &Config,
    journal: Arc<Mutex<AuditJournal>>,
    sink: S,
    shutdown: watch::Receiver<bool>,
) -> Result<AnchorRunReport> {
    let status_path = heartbeat_status_path();
    run_with_status_path(cfg, journal, sink, shutdown, &status_path).await
}

/// [`run`] with an explicit heartbeat status file path (the daemon resolves
/// `HEARTBEAT_PATH` / `data/heartbeat.json`; tests use a scratch path).
async fn run_with_status_path<S: AnchorSink>(
    cfg: &Config,
    journal: Arc<Mutex<AuditJournal>>,
    sink: S,
    mut shutdown: watch::Receiver<bool>,
    status_path: &Path,
) -> Result<AnchorRunReport> {
    let mut report = AnchorRunReport::default();

    // Startup resume (§5): the contract's `lastSeq` when the sink can read
    // it, else the ANCHOR_FROM_SEQ override, else 0.
    let mut next_seq = match sink.last_anchored_seq().await {
        Some(seq) => seq,
        None => std::env::var("ANCHOR_FROM_SEQ")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0),
    };
    info!(next_seq, "anchor: service started");

    let heartbeat_secs = cfg.execution.heartbeat_interval_secs.max(1);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut batch_tick = tokio::time::interval(Duration::from_secs(BATCH_INTERVAL_SECS));
    batch_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reflex_tick = tokio::time::interval(REFLEX_POLL_INTERVAL);
    reflex_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut backoff = BACKOFF_MIN;
    let mut pending: Option<PendingBatch> = None;
    let mut want_pass = false;
    // Heartbeat scheduler: the first interval tick arms the first beat
    // immediately (tokio fires it right away), then on the cadence; a failed
    // beat is retried with the shared 1 s → 30 s backoff schedule
    // (`SPEC-P16` §2).
    let mut beat_due = false;
    let mut beat_attempt_at = Instant::now();
    let mut beat_backoff = BACKOFF_MIN;

    loop {
        if *shutdown.borrow_and_update() {
            break;
        }

        // Build a pass when one was requested and nothing is in flight.
        if want_pass && pending.is_none() {
            want_pass = false;
            match build_pending(&journal, next_seq).await {
                Ok(Some(batch)) => pending = Some(batch),
                Ok(None) => backoff = BACKOFF_MIN,
                Err(err) => {
                    warn!(error = %err, "anchor: journal read failed; retrying on the next tick")
                }
            }
        }

        // Attempt the pending batch when its deadline is due.
        if let Some(batch) = pending.as_mut()
            && Instant::now() >= batch.attempt_at
        {
            match sink
                .anchor_batch(batch.from_seq, &batch.hashes, &batch.root)
                .await
            {
                Ok(tx) => {
                    info!(
                        from_seq = batch.from_seq,
                        entries = batch.hashes.len(),
                        tx = %tx,
                        "anchor: batch anchored"
                    );
                    next_seq = next_seq.saturating_add(batch.hashes.len() as u64);
                    report.batches += 1;
                    report.entries_anchored += batch.hashes.len() as u64;
                    backoff = BACKOFF_MIN;
                    pending = None;
                    want_pass = true; // drain any remaining tail immediately
                    // Status file (P16 §2): `seq` = last journal seq covered.
                    write_heartbeat_status(status_path, next_seq.saturating_sub(1), Some(&tx));
                }
                Err(err) => {
                    report.failures += 1;
                    let delay = backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    batch.attempt_at = Instant::now() + delay;
                    warn!(
                        from_seq = batch.from_seq,
                        retry_in_ms = delay.as_millis() as u64,
                        error = %err,
                        "anchor: batch failed; backing off"
                    );
                }
            }
            continue;
        }

        // Attempt a due heartbeat: the first beat fires immediately, then on
        // the cadence; a failed beat retries with backoff (`SPEC-P16` §2).
        if beat_due && Instant::now() >= beat_attempt_at {
            let last_seq = journal.lock().await.seq();
            let summary = format!(
                "{{\"batches\":{},\"entries_anchored\":{},\"last_seq\":{last_seq}}}",
                report.batches, report.entries_anchored
            );
            let hash = risk_state_hash(&summary, HEARTBEAT_MAX_TIER);
            match sink.beat(&hash, 0, HEARTBEAT_MAX_TIER).await {
                Ok(tx) => {
                    info!(tx = %tx, seq = last_seq, "anchor: heartbeat posted");
                    report.heartbeats += 1;
                    beat_backoff = BACKOFF_MIN;
                    beat_due = false;
                    // Status file (P16 §2): `seq` = the journal head read for
                    // this beat's summary (matches the summary's `last_seq`).
                    write_heartbeat_status(status_path, last_seq, Some(&tx));
                }
                Err(err) => {
                    report.failures += 1;
                    let delay = beat_backoff;
                    beat_backoff = (beat_backoff * 2).min(BACKOFF_MAX);
                    beat_attempt_at = Instant::now() + delay;
                    warn!(
                        retry_in_ms = delay.as_millis() as u64,
                        error = %err,
                        "anchor: heartbeat failed; backing off"
                    );
                }
            }
            continue;
        }

        let retry_at = [
            pending.as_ref().map(|batch| batch.attempt_at),
            beat_due.then_some(beat_attempt_at),
        ]
        .into_iter()
        .flatten()
        .min();
        tokio::select! {
            _ = shutdown.changed() => {}
            _ = heartbeat.tick() => {
                // Arm the next cadence beat; a scheduled retry keeps its
                // deadline (the attempt block above runs before the select).
                beat_due = true;
            }
            _ = batch_tick.tick() => {
                want_pass = true;
            }
            _ = reflex_tick.tick(), if pending.is_none() && !want_pass => {
                if journal_tail_has_reflex(&journal, next_seq).await {
                    want_pass = true;
                }
            }
            _ = sleep_until(retry_at) => {}
        }
    }

    info!(?report, "anchor: service stopped");
    Ok(report)
}

/// One batch waiting to be anchored (or retried).
#[derive(Debug)]
struct PendingBatch {
    /// On-chain `fromSeq` (= journal seq of the first entry + 1).
    from_seq: u64,
    /// `entry_hash` of every entry in the batch, ascending seq order.
    hashes: Vec<String>,
    /// Merkle root over `hashes`.
    root: String,
    /// Earliest instant for the next attempt.
    attempt_at: Instant,
}

/// Read the un-anchored tail and build the next batch, if any.
///
/// The contract's replay guard is 1-based while the journal is 0-based
/// (`on-chain seq = journal seq + 1`), so the passed `from_seq` is
/// `next_seq + 1`. The first attempt is immediate; retries are scheduled by
/// the caller after a failure.
async fn build_pending(
    journal: &Arc<Mutex<AuditJournal>>,
    next_seq: u64,
) -> Result<Option<PendingBatch>> {
    let entries = journal
        .lock()
        .await
        .read_entries(next_seq, BATCH_MAX_ENTRIES)
        .map_err(|err| SentinelError::Internal(format!("journal read failed: {err}")))?;
    let Some(first) = entries.first() else {
        return Ok(None);
    };
    if first.seq != next_seq {
        // Append-only journals are contiguous; a gap would make the on-chain
        // replay guard reject us, so refuse to anchor past it (logged above).
        warn!(
            expected = next_seq,
            found = first.seq,
            "anchor: journal gap detected; not anchoring past it"
        );
        return Ok(None);
    }
    let hashes: Vec<String> = entries
        .iter()
        .map(|entry| entry.entry_hash.clone())
        .collect();
    let root = merkle_root(&hashes);
    Ok(Some(PendingBatch {
        from_seq: next_seq + 1,
        hashes,
        root,
        attempt_at: Instant::now(),
    }))
}

/// True when any of the first [`BATCH_MAX_ENTRIES`] un-anchored entries is a
/// REFLEX entry (the immediate-anchor trigger, §5).
async fn journal_tail_has_reflex(journal: &Arc<Mutex<AuditJournal>>, next_seq: u64) -> bool {
    match journal
        .lock()
        .await
        .read_entries(next_seq, BATCH_MAX_ENTRIES)
    {
        Ok(entries) => entries.iter().any(|entry| entry.trigger == Trigger::Reflex),
        Err(err) => {
            warn!(error = %err, "anchor: reflex scan failed");
            false
        }
    }
}

/// Sleep until `deadline`; pending forever when none is scheduled.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

/// Default heartbeat status file (`SPEC-P16` §2).
const HEARTBEAT_STATUS_DEFAULT: &str = "data/heartbeat.json";

/// Status file path: `HEARTBEAT_PATH` when set (non-empty), else the default.
fn heartbeat_status_path() -> PathBuf {
    std::env::var("HEARTBEAT_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(HEARTBEAT_STATUS_DEFAULT))
}

/// Write the heartbeat status file after a successful beat/batch anchor:
/// `{"ts_ms":u64,"tx_hash":str|null,"seq":u64}` (`SPEC-P16` §2). `seq` is the
/// journal cursor that success confirms (see the module docs). Failures are
/// logged and never disturb anchoring.
fn write_heartbeat_status(path: &Path, seq: u64, tx_hash: Option<&str>) {
    match write_status_file(path, seq, tx_hash) {
        Ok(()) => tracing::debug!(path = %path.display(), seq, "anchor: heartbeat status written"),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "anchor: heartbeat status not written")
        }
    }
}

/// Serialize and atomically replace `path` (write a sibling `*.tmp` file,
/// then rename it over the target). Missing parent directories are created.
fn write_status_file(path: &Path, seq: u64, tx_hash: Option<&str>) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({
        "ts_ms": unix_ms(),
        "tx_hash": tx_hash,
        "seq": seq,
    });
    let mut line = serde_json::to_string(&payload).map_err(std::io::Error::other)?;
    line.push('\n');

    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Milliseconds since the Unix epoch (the status file's `ts_ms`).
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// PENDING-WALLET error for missing/placeholder anchor configuration.
fn pending_wallet(what: &str) -> SentinelError {
    SentinelError::Internal(format!(
        "PENDING-WALLET: {what} is not configured (or is a placeholder); \
         deploy/fund per SPEC-P10 §4 and set it in the environment — \
         live anchors stay offline until then (STUB-17)"
    ))
}

/// True for obvious non-values: empty, placeholders, all-zero hex bodies.
fn is_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "placeholder",
        "changeme",
        "change-me",
        "your_",
        "todo",
        "dummy",
        "example",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return true;
    }
    let body = lower.strip_prefix("0x").unwrap_or(&lower);
    !body.is_empty() && body.chars().all(|c| c == '0')
}

/// Parse a `0x`-optional 32-byte hex value; errors never echo inputs.
fn parse_hash(value: &str, what: &str) -> Result<FixedBytes<32>> {
    value.trim().parse::<FixedBytes<32>>().map_err(|_| {
        SentinelError::Internal(format!("{what} is not 32 bytes of hex (0x optional)"))
    })
}

/// Map a mined-but-reverted receipt onto the frozen error set (the tx hash is
/// logged by the caller, never swallowed).
fn ensure_success(status: bool, tx_hash: alloy::primitives::TxHash) -> Result<()> {
    if status {
        Ok(())
    } else {
        Err(SentinelError::Internal(format!(
            "transaction {tx_hash} reverted on-chain"
        )))
    }
}

/// Decode a 64-hex hash into 32 bytes (lenient: raw-byte hash fallback).
fn decode_hash(hash: &str) -> [u8; 32] {
    let trimmed = hash.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if body.len() == 64 {
        let mut out = [0_u8; 32];
        if hex::decode_to_slice(body, &mut out).is_ok() {
            return out;
        }
    }
    sentinel_core::audit::sha256(trimmed.as_bytes())
}

/// `sha256(left ‖ right)` over two 32-byte nodes.
fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buffer = [0_u8; 64];
    buffer[..32].copy_from_slice(left);
    buffer[32..].copy_from_slice(right);
    sentinel_core::audit::sha256(&buffer)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use chrono::Utc;
    use serde_json::json;
    use tempfile::TempDir;

    use sentinel_core::audit::{AuditEntry, IntentRecord};

    use super::*;

    /// Fixed oracle vectors, computed with the documented python3 command in
    /// each test (SHA-256 over explicit byte concatenations — the same bytes
    /// a third party recomputes by hand).
    const LEAF_A: &str = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
    const LEAF_B: &str = "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d";
    const LEAF_C: &str = "2e7d2c03a9507ae265ecf5b5356885a53393a2029d241394997265a1a25aefc6";
    const LEAF_D: &str = "18ac3e7343f016890c510e93f935261169d9e3f565436429830faf0934f4f8e4";
    const ROOT_AB: &str = "e5a01fee14e0ed5c48714f22180f25ad8365b53f9779f79dc4a3d7e93963f94a";
    const ROOT_ABC: &str = "7075152d03a5cd92104887b476862778ec0c87be5c2fa1c0a90f87c49fad6eff";
    const ROOT_ABCD: &str = "14ede5e8e97ad9372327728f5099b95604a39593cac3bd38a343ad76205213e7";

    /// `risk_state_hash(s, 0)` for `s = ""`.
    const RISK_EMPTY_T0: &str = "365c685bb2f41a670b598d1bda47ec062c7d9d7119d4a5c744721e4afd06462f";
    /// `risk_state_hash({"batches":0,"entries_anchored":0,"last_seq":0}, 3)`.
    const RISK_ZERO_T3: &str = "2ebd6ea4757616255378a38f0a55cf44ffe801bcfbe547685f47060a7d54196e";
    /// `risk_state_hash({"batches":0,"entries_anchored":0,"last_seq":2}, 3)`.
    const RISK_TWO_T3: &str = "6f3b6ab3e537041104758d9850b9b71e0d712816abbdf00adfe8cf9ef8a7f7b4";

    const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";
    /// 32 bytes of zero, hex (the config layer wants 64 hex chars).
    const ZERO_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    // ---- merkle_root -----------------------------------------------------

    #[test]
    fn merkle_root_matches_python_sha256_oracle() {
        // python3:
        //   import hashlib
        //   h = lambda b: hashlib.sha256(b).hexdigest()
        //   a, b, c, d = (hashlib.sha256(x.encode()).hexdigest() for x in "abcd")
        //   ab = h(bytes.fromhex(a) + bytes.fromhex(b))
        //   assert ab == ROOT_AB
        //   assert h(bytes.fromhex(ab) + bytes.fromhex(c)) == ROOT_ABC   # odd tail promoted
        //   assert h(bytes.fromhex(ab) + bytes.fromhex(h(c+d))) == ROOT_ABCD
        let leaf = |hex: &str| hex.to_owned();

        // 0 leaves ⇒ 64 zeros (the genesis prev hash).
        assert_eq!(merkle_root(&[]), "0".repeat(64));
        assert_eq!(merkle_root(&[]), sentinel_core::audit::GENESIS_PREV_HASH);

        // 1 leaf ⇒ itself.
        assert_eq!(merkle_root(&[leaf(LEAF_A)]), LEAF_A);

        // 2 leaves ⇒ sha256(a ‖ b).
        assert_eq!(merkle_root(&[leaf(LEAF_A), leaf(LEAF_B)]), ROOT_AB);

        // 3 leaves ⇒ sha256(sha256(a ‖ b) ‖ c) — c promoted, NOT duplicated.
        assert_eq!(
            merkle_root(&[leaf(LEAF_A), leaf(LEAF_B), leaf(LEAF_C)]),
            ROOT_ABC
        );

        // 4 leaves ⇒ sha256(sha256(a ‖ b) ‖ sha256(c ‖ d)).
        assert_eq!(
            merkle_root(&[leaf(LEAF_A), leaf(LEAF_B), leaf(LEAF_C), leaf(LEAF_D)]),
            ROOT_ABCD
        );

        // Promotion is not duplication: [a,b,c] differs from [a,b,c,c].
        assert_ne!(
            merkle_root(&[leaf(LEAF_A), leaf(LEAF_B), leaf(LEAF_C)]),
            merkle_root(&[leaf(LEAF_A), leaf(LEAF_B), leaf(LEAF_C), leaf(LEAF_C)])
        );
    }

    #[test]
    fn merkle_root_tolerates_0x_prefix_and_uppercase() {
        let prefixed = format!("0x{}", LEAF_A.to_uppercase());
        assert_eq!(
            merkle_root(&[prefixed]),
            format!("0x{}", LEAF_A.to_uppercase())
        );
        assert_eq!(
            merkle_root(&[format!("0x{LEAF_A}"), format!("0X{LEAF_B}")]),
            ROOT_AB
        );
    }

    /// Documents the defensive path: garbage is hashed raw (total function).
    #[test]
    fn merkle_root_is_total_for_non_hex_inputs() {
        let garbage = merkle_root(&["not-a-hash".to_owned(), LEAF_B.to_owned()]);
        assert_eq!(garbage.len(), 64);
        assert!(garbage.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ---- risk_state_hash -------------------------------------------------

    #[test]
    fn risk_state_hash_matches_python_oracle() {
        // python3:
        //   import hashlib, json
        //   s = '{"batches":0,"entries_anchored":0,"last_seq":0}'
        //   canon = '{"max_tier":3,"summary":%s}' % json.dumps(s)
        //   assert hashlib.sha256(canon.encode()).hexdigest() == RISK_ZERO_T3
        assert_eq!(risk_state_hash("", 0), RISK_EMPTY_T0);
        assert_eq!(
            risk_state_hash("{\"batches\":0,\"entries_anchored\":0,\"last_seq\":0}", 3),
            RISK_ZERO_T3
        );
        assert_eq!(
            risk_state_hash("{\"batches\":0,\"entries_anchored\":0,\"last_seq\":2}", 3),
            RISK_TWO_T3
        );

        // The tier and the summary both commit.
        assert_ne!(risk_state_hash("same", 3), risk_state_hash("same", 4));
        assert_ne!(risk_state_hash("a", 3), risk_state_hash("b", 3));
    }

    // ---- test doubles ----------------------------------------------------

    /// One `anchor_batch` attempt seen by the mock sink.
    #[derive(Debug, Clone)]
    struct BatchCall {
        from_seq: u64,
        hashes: Vec<String>,
        root: String,
        at: Instant,
    }

    #[derive(Debug, Default)]
    struct MockState {
        batches: Vec<BatchCall>,
        beats: Vec<(String, u32, u8, Instant)>,
        fail_anchor_attempts: usize,
        fail_beat_attempts: usize,
    }

    #[derive(Clone)]
    struct MockSink {
        state: Arc<StdMutex<MockState>>,
        hint: Option<u64>,
    }

    impl MockSink {
        fn new(hint: Option<u64>) -> Self {
            Self {
                state: Arc::new(StdMutex::new(MockState::default())),
                hint,
            }
        }

        fn failing(hint: Option<u64>, failures: usize) -> Self {
            let sink = Self::new(hint);
            sink.state.lock().expect("mock lock").fail_anchor_attempts = failures;
            sink
        }

        fn failing_beats(hint: Option<u64>, failures: usize) -> Self {
            let sink = Self::new(hint);
            sink.state.lock().expect("mock lock").fail_beat_attempts = failures;
            sink
        }

        fn calls(&self) -> Vec<BatchCall> {
            self.state.lock().expect("mock lock").batches.clone()
        }

        fn beats(&self) -> Vec<(String, u32, u8, Instant)> {
            self.state.lock().expect("mock lock").beats.clone()
        }
    }

    impl AnchorSink for MockSink {
        async fn anchor_batch(
            &self,
            from_seq: u64,
            entry_hashes: &[String],
            root: &str,
        ) -> Result<String> {
            let mut state = self.state.lock().expect("mock lock");
            state.batches.push(BatchCall {
                from_seq,
                hashes: entry_hashes.to_vec(),
                root: root.to_owned(),
                at: Instant::now(),
            });
            if state.fail_anchor_attempts > 0 {
                state.fail_anchor_attempts -= 1;
                return Err(SentinelError::Internal("mock sink failure".to_string()));
            }
            Ok(format!("0x{}", "ab".repeat(32)))
        }

        async fn beat(
            &self,
            risk_state_hash: &str,
            open_positions: u32,
            max_tier: u8,
        ) -> Result<String> {
            let mut state = self.state.lock().expect("mock lock");
            state.beats.push((
                risk_state_hash.to_owned(),
                open_positions,
                max_tier,
                Instant::now(),
            ));
            if state.fail_beat_attempts > 0 {
                state.fail_beat_attempts -= 1;
                return Err(SentinelError::Internal("mock beat failure".to_string()));
            }
            Ok(format!("0x{}", "cd".repeat(32)))
        }

        async fn last_anchored_seq(&self) -> Option<u64> {
            self.hint
        }
    }

    // ---- run() -----------------------------------------------------------

    fn test_cfg(heartbeat_secs: u64) -> Config {
        let base: &[(&str, &str)] = &[
            ("PERPL_ENV", "testnet"),
            ("PERPL_API_KEY", "test-token"),
            ("PERPL_API_KEY_SECRET", ZERO_SECRET),
            ("PERPL_ACCOUNT", ACCOUNT),
            ("QWEN_API_KEY", "qwen-test-key"),
            ("KIMI_API_KEY", "kimi-test-key"),
            ("TELOXIDE_TOKEN", "123456:test-token"),
            ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
            ("NANSEN_PAYER_KEY", "0x00"),
            ("EXECUTION_MODE", "DRY_RUN"),
        ];
        let heartbeat = heartbeat_secs.to_string();
        let mut vars: std::collections::HashMap<String, String> = base
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        vars.insert("HEARTBEAT_INTERVAL_SECS".to_string(), heartbeat);
        Config::from_vars(vars).expect("test config must load")
    }

    fn test_journal(entries: usize) -> (TempDir, Arc<Mutex<AuditJournal>>, Vec<AuditEntry>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = AuditJournal::open(dir.path()).expect("open journal");
        let mut recorded = Vec::new();
        for index in 0..entries {
            let record = IntentRecord {
                trigger: Trigger::Reflex,
                account: ACCOUNT.to_string(),
                market_id: Some(32),
                input_hash: format!("{:064x}", index + 1),
                decision: json!({ "index": index }),
                policy_verdict: json!({ "verdict": "allow" }),
            };
            recorded.push(journal.record_intent(&record, Utc::now()).expect("record"));
        }
        (dir, Arc::new(Mutex::new(journal)), recorded)
    }

    /// Poll the sink until it saw at least `n` batch attempts.
    async fn wait_for_batches(sink: &MockSink, n: usize, timeout: Duration) -> Vec<BatchCall> {
        let deadline = Instant::now() + timeout;
        loop {
            let calls = sink.calls();
            if calls.len() >= n {
                return calls;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n} batch attempt(s); saw {}",
                calls.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Start [`run_with_status_path`] on a journal, returning (handle,
    /// shutdown). `status_path` is a scratch heartbeat-status file — never
    /// the real `data/heartbeat.json` (tests must not touch the repo tree).
    fn spawn_run(
        cfg: Config,
        journal: Arc<Mutex<AuditJournal>>,
        sink: MockSink,
        status_path: PathBuf,
    ) -> (
        tokio::task::JoinHandle<Result<AnchorRunReport>>,
        watch::Sender<bool>,
    ) {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            run_with_status_path(&cfg, journal, sink, rx, &status_path).await
        });
        (handle, tx)
    }

    /// A scratch heartbeat-status path inside `dir`.
    fn status_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("heartbeat.json")
    }

    #[tokio::test]
    async fn run_anchors_the_existing_tail_immediately() {
        let (_dir, journal, recorded) = test_journal(3);
        let sink = MockSink::new(Some(0));
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        let calls = wait_for_batches(&sink, 1, Duration::from_secs(5)).await;
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        assert_eq!(calls.len(), 1, "exactly one batch for the initial tail");
        assert_eq!(calls[0].from_seq, 1, "journal seq 0 maps to on-chain seq 1");
        assert_eq!(
            calls[0].hashes,
            recorded
                .iter()
                .map(|e| e.entry_hash.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(calls[0].root, merkle_root(&calls[0].hashes));
        assert_eq!(report.batches, 1);
        assert_eq!(report.entries_anchored, 3);
        assert_eq!(report.failures, 0);
    }

    #[tokio::test]
    async fn run_resumes_from_the_sink_last_anchored_seq() {
        let (_dir, journal, recorded) = test_journal(5);
        // Contract already holds seqs 1..=2 (journal 0..=1) ⇒ resume at 2.
        let sink = MockSink::new(Some(2));
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        let calls = wait_for_batches(&sink, 1, Duration::from_secs(5)).await;
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        assert_eq!(calls[0].from_seq, 3, "first un-anchored journal seq is 2");
        assert_eq!(
            calls[0].hashes,
            recorded[2..]
                .iter()
                .map(|e| e.entry_hash.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(report.entries_anchored, 3, "only the tail is anchored");
    }

    #[tokio::test]
    async fn run_beats_on_the_heartbeat_cadence_with_the_documented_summary() {
        let (_dir, journal, _) = test_journal(2);
        // Resume past the head: no batch work, only heartbeats.
        let sink = MockSink::new(Some(2));
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        let deadline = Instant::now() + Duration::from_secs(4);
        while sink.beats().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "expected two heartbeats within 4 s; saw {}",
                sink.beats().len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        let beats = sink.beats();
        assert!(beats.len() >= 2, "cadence: {beats:?}");
        assert!(
            beats[1].3.duration_since(beats[0].3) >= Duration::from_millis(900),
            "second beat ~1 s after the first: {beats:?}"
        );
        for (hash, positions, tier, _) in &beats {
            // Journal has 2 entries (already anchored), none anchored by this run.
            assert_eq!(
                hash,
                &risk_state_hash("{\"batches\":0,\"entries_anchored\":0,\"last_seq\":2}", 3),
                "risk state hash commits to the anchored summary"
            );
            assert_eq!(*positions, 0, "positions placeholder until P14");
            assert_eq!(*tier, 3);
        }
        assert_eq!(report.heartbeats as usize, beats.len());
        assert!(sink.calls().is_empty(), "nothing left to anchor");
    }

    #[tokio::test]
    async fn run_anchors_reflex_entries_immediately_but_waits_for_others() {
        let (_dir, journal, _) = test_journal(0);
        let sink = MockSink::new(Some(0));
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        // Let the initial (empty-tail) pass and a few reflex polls settle.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Non-REFLEX entries must NOT anchor off-schedule (60 s tick only).
        for index in 0..2 {
            let record = IntentRecord {
                trigger: Trigger::Human,
                account: ACCOUNT.to_string(),
                market_id: Some(32),
                input_hash: format!("{:064x}", 100 + index),
                decision: json!({ "human": index }),
                policy_verdict: json!({ "verdict": "allow" }),
            };
            journal
                .lock()
                .await
                .record_intent(&record, Utc::now())
                .expect("record human");
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            sink.calls().is_empty(),
            "HUMAN/STRATEGY entries wait for the 60 s tick: {:?}",
            sink.calls()
        );

        // REFLEX entries anchor on the next poll (fast path).
        for index in 0..2 {
            let record = IntentRecord {
                trigger: Trigger::Reflex,
                account: ACCOUNT.to_string(),
                market_id: Some(32),
                input_hash: format!("{:064x}", 200 + index),
                decision: json!({ "reflex": index }),
                policy_verdict: json!({ "verdict": "allow" }),
            };
            journal
                .lock()
                .await
                .record_intent(&record, Utc::now())
                .expect("record reflex");
        }
        let calls = wait_for_batches(&sink, 1, Duration::from_secs(3)).await;
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        // One pass usually covers all four; a split pass is also legal.
        let mut anchored: Vec<String> = Vec::new();
        let mut cursor = 1;
        for call in &calls {
            assert_eq!(call.from_seq, cursor, "contiguous on-chain batches");
            cursor += call.hashes.len() as u64;
            anchored.extend(call.hashes.clone());
        }
        let expected: Vec<String> = journal
            .lock()
            .await
            .read_entries(0, 64)
            .expect("read journal")
            .iter()
            .map(|entry| entry.entry_hash.clone())
            .collect();
        assert_eq!(anchored, expected, "every entry anchored, in order");
        assert_eq!(report.entries_anchored, 4);
    }

    #[tokio::test]
    async fn run_retries_with_exponential_backoff_keeping_from_seq() {
        let (_dir, journal, _) = test_journal(2);
        let sink = MockSink::failing(Some(0), 2);
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        // Attempt 1 at once, attempt 2 after 1 s, attempt 3 after 2 more.
        let calls = wait_for_batches(&sink, 3, Duration::from_secs(10)).await;
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        assert_eq!(calls[0].from_seq, 1);
        assert_eq!(calls[1].from_seq, 1, "retry keeps from_seq");
        assert_eq!(calls[2].from_seq, 1, "retry keeps from_seq");
        assert_eq!(calls[0].hashes, calls[1].hashes);
        assert_eq!(calls[1].hashes, calls[2].hashes);
        assert!(
            calls[1].at.duration_since(calls[0].at) >= Duration::from_millis(800),
            "first backoff ~1 s"
        );
        assert!(
            calls[2].at.duration_since(calls[1].at) >= Duration::from_millis(1600),
            "second backoff ~2 s"
        );
        assert_eq!(report.batches, 1);
        assert_eq!(report.failures, 2);
        assert_eq!(report.entries_anchored, 2, "entries are never lost");
    }

    #[tokio::test]
    async fn run_stops_cleanly_when_shutdown_is_already_flipped() {
        let (_dir, journal, _) = test_journal(0);
        let sink = MockSink::new(Some(0));
        let status_dir = tempfile::tempdir().expect("status dir");
        let (_tx, rx) = watch::channel(true);

        let report = tokio::time::timeout(
            Duration::from_secs(1),
            run_with_status_path(
                &test_cfg(1),
                Arc::clone(&journal),
                sink.clone(),
                rx,
                &status_file(&status_dir),
            ),
        )
        .await
        .expect("immediate shutdown")
        .expect("run is Ok");

        assert_eq!(report, AnchorRunReport::default());
        assert!(sink.calls().is_empty() && sink.beats().is_empty());
    }

    #[tokio::test]
    async fn run_retries_failed_beats_with_exponential_backoff() {
        let (_dir, journal, _) = test_journal(0);
        // Two beat failures in a row (the journal is empty: no batch work).
        let sink = MockSink::failing_beats(Some(0), 2);
        let status_dir = tempfile::tempdir().expect("status dir");
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status_file(&status_dir),
        );

        // Attempt 1 at once, attempt 2 after 1 s, attempt 3 after 2 more.
        let deadline = Instant::now() + Duration::from_secs(10);
        while sink.beats().len() < 3 {
            assert!(
                Instant::now() < deadline,
                "expected three beat attempts within 10 s; saw {}",
                sink.beats().len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown.send(true);
        let report = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        let beats = sink.beats();
        assert!(
            beats[1].3.duration_since(beats[0].3) >= Duration::from_millis(800),
            "first heartbeat retry ~1 s backoff: {beats:?}"
        );
        assert!(
            beats[2].3.duration_since(beats[1].3) >= Duration::from_millis(1600),
            "second heartbeat retry ~2 s backoff: {beats:?}"
        );
        assert_eq!(report.failures, 2, "two failed beats, then success");
        assert_eq!(
            report.heartbeats as usize,
            beats.len() - 2,
            "every success after the failures is counted: {beats:?}"
        );
        assert!(
            sink.calls().is_empty(),
            "no batch work with an empty journal"
        );
    }

    #[tokio::test]
    async fn run_writes_heartbeat_status_after_a_successful_batch_anchor() {
        let (_dir, journal, recorded) = test_journal(2);
        // Beats fail forever: the file then holds the batch write only,
        // which pins "written after every successful batch anchor" exactly.
        let sink = MockSink::failing_beats(Some(0), usize::MAX);
        let status_dir = tempfile::tempdir().expect("status dir");
        let status = status_file(&status_dir);
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status.clone(),
        );

        let _ = wait_for_batches(&sink, 1, Duration::from_secs(5)).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !status.exists() {
            assert!(
                Instant::now() < deadline,
                "heartbeat status file not written after the batch anchor"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&status).expect("status file"))
                .expect("status file is JSON");
        assert_eq!(value["seq"], serde_json::json!(recorded.len() as u64 - 1));
        assert_eq!(
            value["tx_hash"],
            serde_json::json!(format!("0x{}", "ab".repeat(32))),
            "the mock batch tx hash"
        );
        assert!(value["ts_ms"].as_u64().is_some(), "ts_ms present");
        // Atomic write: no *.tmp remnant.
        assert!(!status.with_extension("json.tmp").exists());
    }

    #[tokio::test]
    async fn run_writes_heartbeat_status_after_a_successful_beat() {
        let (_dir, journal, _) = test_journal(2);
        // Resume past the head: no batch work; every beat succeeds and the
        // file must track the journal head (`seq` = 2) and the beat tx hash.
        let sink = MockSink::new(Some(2));
        let status_dir = tempfile::tempdir().expect("status dir");
        let status = status_file(&status_dir);
        let (handle, shutdown) = spawn_run(
            test_cfg(1),
            Arc::clone(&journal),
            sink.clone(),
            status.clone(),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(raw) = std::fs::read_to_string(&status)
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
                && value["seq"] == serde_json::json!(2)
            {
                assert_eq!(
                    value["tx_hash"],
                    serde_json::json!(format!("0x{}", "cd".repeat(32))),
                    "the mock beat tx hash"
                );
                assert!(value["ts_ms"].as_u64().is_some());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "heartbeat status file not written after a successful beat"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("shutdown is prompt")
            .expect("task joins")
            .expect("run is Ok");
    }

    #[test]
    fn write_status_file_replaces_atomically_with_the_frozen_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/deep/heartbeat.json");
        write_status_file(&path, 7, Some("0xabc")).expect("write");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(value["seq"], serde_json::json!(7));
        assert_eq!(value["tx_hash"], serde_json::json!("0xabc"));
        assert!(value["ts_ms"].as_u64().is_some());

        // `tx_hash` may be null (shape allows str|null), and the previous
        // contents are replaced, not appended.
        write_status_file(&path, 8, None).expect("write null");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(value["seq"], serde_json::json!(8));
        assert!(value["tx_hash"].is_null());
        assert!(value["ts_ms"].as_u64().is_some());
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("read")
                .lines()
                .count(),
            1,
            "exactly one line (replaced, not appended)"
        );

        // No temp remnant is left behind.
        let names: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["heartbeat.json".to_string()]);
    }

    #[test]
    fn placeholder_detection_covers_missing_and_dummy_values() {
        assert!(is_placeholder(""));
        assert!(is_placeholder("   "));
        assert!(is_placeholder(&"0".repeat(40)));
        assert!(is_placeholder(&format!("0x{}", "0".repeat(64))));
        assert!(is_placeholder("0xCHANGEME"));
        assert!(is_placeholder("your_key_here"));
        assert!(!is_placeholder(&"ac".repeat(32)));
        assert!(!is_placeholder(
            "0x1964c32f0be608e7d29302aff5e61268e72080cc"
        ));
    }
}
