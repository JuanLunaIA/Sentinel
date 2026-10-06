//! Hash-chained audit journal — tamper-evident decision log.
//!
//! Every decision (intent, policy verdict, execution outcome) is journaled
//! **before** the action it authorizes and appended with its outcome after
//! (P00 invariant #2: audit-before-action). Entries form a SHA-256 hash
//! chain whose head is periodically anchored on Monad by the
//! `SentinelAuditAnchor` contract. Format frozen in `SPEC-P10.md` §2/§3.
//!
//! `std::fs` is used here deliberately (journal persistence); the crate
//! still has **zero network dependencies** (P00 invariant #4).
//!
//! **Skeleton status (P10):** interfaces frozen; implemented by the P10 wave.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Journal I/O and format failures (the app maps these onto its `Audit`
/// error subsystem; the core crate has no shared error module by design).
#[derive(Debug)]
pub enum JournalError {
    /// File-system failure (open/read/append).
    Io(String),
    /// Malformed journal content.
    Format(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Io(detail) => write!(f, "journal io: {detail}"),
            JournalError::Format(detail) => write!(f, "journal format: {detail}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// Result alias for journal operations.
pub type Result<T> = std::result::Result<T, JournalError>;

/// SHA-256 digest helper (also used to hash decision payloads).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Hex of 32 zero bytes — the genesis `prev_hash`.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// What produced a journaled decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Trigger {
    /// Deterministic reflex engine.
    Reflex,
    /// LLM strategy brain.
    Strategy,
    /// Human command (bot/API).
    Human,
    /// Backtest / replay run.
    Backtest,
    /// System event (startup, shutdown, guard).
    System,
}

/// One entry of the tamper-evident decision journal (`SPEC-P10` §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonically increasing sequence number (per journal).
    pub seq: u64,
    /// UTC timestamp assigned when the entry was created.
    pub ts: DateTime<Utc>,
    /// Decision trigger.
    pub trigger: Trigger,
    /// Account the decision belongs to (address string).
    pub account: String,
    /// Market the decision concerns, when applicable.
    pub market_id: Option<u32>,
    /// `sha256` hex over the canonical inputs fed to the decision.
    pub input_hash: String,
    /// Full decision document (provider, prompt_version, confidence, …).
    pub decision: Value,
    /// Policy verdict (+ details) for the decision.
    pub policy_verdict: Value,
    /// Execution block (`{"status": "pending"}` for intents).
    pub execution: Value,
    /// Previous entry's hash (64 hex; genesis = zeros).
    pub prev_hash: String,
    /// This entry's hash (64 hex).
    pub entry_hash: String,
}

impl AuditEntry {
    /// Build an entry and seal its `entry_hash`.
    #[allow(clippy::too_many_arguments)] // journal fields are explicit by design
    pub fn new(
        _seq: u64,
        _ts: DateTime<Utc>,
        _trigger: Trigger,
        _account: String,
        _market_id: Option<u32>,
        _input_hash: String,
        _decision: Value,
        _policy_verdict: Value,
        _execution: Value,
        _prev_hash: String,
    ) -> Self {
        todo!("P10 agent journal-core: seal entry_hash")
    }

    /// Recompute this entry's hash from its contents (`SPEC-P10` §2).
    pub fn compute_hash(&self) -> String {
        todo!("P10 agent journal-core")
    }
}

/// Canonical JSON (compact, sorted keys via `serde_json`'s BTreeMap).
pub fn canonical_json(value: &Value) -> String {
    let _ = value;
    todo!("P10 agent journal-core")
}

/// `sha256` hex over the concatenated canonical JSON of `parts`.
pub fn hash_input(parts: &[&Value]) -> String {
    let _ = parts;
    todo!("P10 agent journal-core")
}

/// Append one entry as a single JSON line (create dirs; O_APPEND).
///
/// # Errors
/// `JournalError::Io` on I/O failure.
pub fn append_line(_path: &Path, _entry: &AuditEntry) -> Result<()> {
    todo!("P10 agent journal-core")
}

/// Verification outcome for one journal file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Entries examined.
    pub entries: usize,
    /// First seq in the file.
    pub first_seq: Option<u64>,
    /// Highest seq whose chain is intact.
    pub valid_up_to_seq: Option<u64>,
    /// Last valid entry hash.
    pub last_hash: Option<String>,
    /// Seq of the first broken entry, if any.
    pub broken_at: Option<u64>,
    /// Human detail for a break.
    pub detail: Option<String>,
}

/// Verify one journal file (recompute hashes; link every prev).
///
/// # Errors
/// `JournalError::Io` when the file cannot be read.
pub fn verify_chain(_path: &Path) -> Result<VerifyReport> {
    todo!("P10 agent journal-core")
}

/// Context shared by intent/outcome records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentRecord {
    /// Trigger.
    pub trigger: Trigger,
    /// Account.
    pub account: String,
    /// Market.
    pub market_id: Option<u32>,
    /// Inputs hash.
    pub input_hash: String,
    /// Decision document.
    pub decision: Value,
    /// Policy verdict document.
    pub policy_verdict: Value,
}

/// Outcome of an intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// Trigger (same as the intent).
    pub trigger: Trigger,
    /// Account.
    pub account: String,
    /// Market.
    pub market_id: Option<u32>,
    /// Inputs hash (same as the intent).
    pub input_hash: String,
    /// Decision document (same as the intent).
    pub decision: Value,
    /// Policy verdict document (same as the intent).
    pub policy_verdict: Value,
    /// Execution block for the outcome.
    pub execution: Value,
}

/// Stateful append-only journal with crash-safe resume (`SPEC-P10` §3).
#[derive(Debug)]
pub struct AuditJournal {
    dir: PathBuf,
    seq: u64,
    last_hash: String,
}

impl AuditJournal {
    /// Open (or initialize) the journal in `dir`, resuming from the latest
    /// `journal-*.jsonl` file.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn open(_dir: impl Into<PathBuf>) -> Result<Self> {
        todo!("P10 agent journal-core")
    }

    /// Next sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Journal an intent (`execution = {"status":"pending"}`) at `ts`.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn record_intent(
        &mut self,
        _record: &IntentRecord,
        _ts: DateTime<Utc>,
    ) -> Result<AuditEntry> {
        todo!("P10 agent journal-core")
    }

    /// Journal the outcome of a previously recorded intent.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn record_outcome(
        &mut self,
        _record: &OutcomeRecord,
        _ts: DateTime<Utc>,
    ) -> Result<AuditEntry> {
        todo!("P10 agent journal-core")
    }

    /// Entries with `seq >= from_seq`, at most `limit`.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn read_entries(&self, _from_seq: u64, _limit: usize) -> Result<Vec<AuditEntry>> {
        todo!("P10 agent journal-core")
    }
}
