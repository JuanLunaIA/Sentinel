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
//! **Status (P10):** implemented by agent `journal-core`; format frozen in
//! `SPEC-P10.md` §2/§3.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
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

/// Prefix of a journal day file (`journal-YYYYMMDD.jsonl`).
const JOURNAL_FILE_PREFIX: &str = "journal-";
/// Suffix of a journal day file.
const JOURNAL_FILE_SUFFIX: &str = ".jsonl";

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
        seq: u64,
        ts: DateTime<Utc>,
        trigger: Trigger,
        account: String,
        market_id: Option<u32>,
        input_hash: String,
        decision: Value,
        policy_verdict: Value,
        execution: Value,
        prev_hash: String,
    ) -> Self {
        let mut entry = Self {
            seq,
            ts,
            trigger,
            account,
            market_id,
            input_hash,
            decision,
            policy_verdict,
            execution,
            prev_hash,
            entry_hash: String::new(),
        };
        entry.entry_hash = entry.compute_hash();
        entry
    }

    /// Recompute this entry's hash from its contents (`SPEC-P10` §2):
    ///
    /// `hex(sha256(raw32(prev_hash) ++ canonical_json(entry minus
    /// {prev_hash, entry_hash})))`.
    pub fn compute_hash(&self) -> String {
        let mut value = match serde_json::to_value(self) {
            Ok(value) => value,
            Err(_) => return String::new(),
        };
        if let Some(object) = value.as_object_mut() {
            object.remove("prev_hash");
            object.remove("entry_hash");
        }
        let mut preimage = raw32_or_bytes(&self.prev_hash);
        preimage.extend_from_slice(canonical_json(&value).as_bytes());
        hex::encode(sha256(&preimage))
    }
}

/// Raw 32 bytes of a hash when it is valid 64-char hex; the plain string
/// bytes otherwise. Keeps `compute_hash` total on malformed input so
/// `verify_chain` reports a mismatch instead of failing to compare.
fn raw32_or_bytes(hash: &str) -> Vec<u8> {
    match hex::decode(hash) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => hash.as_bytes().to_vec(),
    }
}

/// Canonical JSON (compact, sorted keys via `serde_json`'s BTreeMap).
pub fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `sha256` hex over the concatenated canonical JSON of `parts`.
pub fn hash_input(parts: &[&Value]) -> String {
    let mut buffer = Vec::new();
    for part in parts {
        buffer.extend_from_slice(canonical_json(part).as_bytes());
    }
    hex::encode(sha256(&buffer))
}

/// Append one entry as a single JSON line (create dirs; O_APPEND).
///
/// # Errors
/// `JournalError::Io` on I/O failure.
pub fn append_line(path: &Path, entry: &AuditEntry) -> Result<()> {
    let value = serde_json::to_value(entry)
        .map_err(|err| JournalError::Format(format!("entry serialization failed: {err}")))?;
    let mut line = canonical_json(&value);
    line.push('\n');

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            JournalError::Io(format!("create_dir_all {}: {err}", parent.display()))
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| JournalError::Io(format!("open {}: {err}", path.display())))?;
    // One write_all per line: a torn write can only ever damage the final
    // line, which `verify_chain` tolerates and `open` skips on resume.
    file.write_all(line.as_bytes())
        .map_err(|err| JournalError::Io(format!("append {}: {err}", path.display())))?;
    Ok(())
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
/// The first entry's `prev_hash` may be any 64-hex value (file boundary:
/// the CLI chains day files), but its recomputed hash must always match.
/// A malformed trailing line is tolerated as a torn write (noted in
/// `detail`, not a break); any other malformed line breaks the chain.
///
/// # Errors
/// `JournalError::Io` when the file cannot be read.
pub fn verify_chain(path: &Path) -> Result<VerifyReport> {
    let content = fs::read_to_string(path)
        .map_err(|err| JournalError::Io(format!("read {}: {err}", path.display())))?;

    // Parse pass: 1-based line numbers for details, blank lines ignored.
    let mut parsed: Vec<(usize, std::result::Result<AuditEntry, serde_json::Error>)> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        parsed.push((index + 1, serde_json::from_str::<AuditEntry>(line)));
    }
    // The last line that parsed. Malformed lines after it are the torn tail.
    let last_valid_pos = parsed.iter().rposition(|(_, result)| result.is_ok());

    let mut report = VerifyReport {
        entries: 0,
        first_seq: None,
        valid_up_to_seq: None,
        last_hash: None,
        broken_at: None,
        detail: None,
    };
    let mut expected_prev: Option<String> = None;
    let mut broken = false;

    for (pos, (line_no, result)) in parsed.iter().enumerate() {
        match result {
            Ok(entry) => {
                report.entries += 1;
                if report.first_seq.is_none() {
                    report.first_seq = Some(entry.seq);
                }
                if broken {
                    continue;
                }
                let mut fault = None;
                if let Some(expected) = &expected_prev
                    && entry.prev_hash != *expected
                {
                    fault = Some(format!(
                        "prev_hash mismatch at seq {}: prev_hash {} does not match previous entry_hash {}",
                        entry.seq, entry.prev_hash, expected
                    ));
                }
                if fault.is_none() {
                    let recomputed = entry.compute_hash();
                    if recomputed != entry.entry_hash {
                        fault = Some(format!(
                            "entry_hash mismatch at seq {}: stored {} does not match recomputed {}",
                            entry.seq, entry.entry_hash, recomputed
                        ));
                    }
                }
                match fault {
                    Some(detail) => {
                        report.broken_at = Some(entry.seq);
                        report.detail = Some(detail);
                        broken = true;
                    }
                    None => {
                        report.valid_up_to_seq = Some(entry.seq);
                        report.last_hash = Some(entry.entry_hash.clone());
                        expected_prev = Some(entry.entry_hash.clone());
                    }
                }
            }
            Err(err) => {
                let tolerated = match last_valid_pos {
                    Some(last) => pos > last,
                    None => true,
                };
                if tolerated {
                    if !broken && report.detail.is_none() {
                        report.detail =
                            Some(format!("ignored torn trailing line {line_no}: {err}"));
                    }
                } else if !broken {
                    // Interior malformed line: infer the seq it must have
                    // carried (previous valid seq + 1, else the next seq).
                    let next_seq = parsed
                        .iter()
                        .skip(pos + 1)
                        .find_map(|(_, result)| result.as_ref().ok().map(|entry| entry.seq));
                    let inferred = report
                        .valid_up_to_seq
                        .map(|seq| seq.saturating_add(1))
                        .or(next_seq);
                    report.broken_at = inferred;
                    report.detail = Some(format!("malformed journal line {line_no}: {err}"));
                    broken = true;
                }
            }
        }
    }
    Ok(report)
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
    /// Resumes `seq`/`last_hash` from the last valid line of the
    /// lexicographically-latest file (a torn trailing line is skipped with a
    /// warning); an empty journal starts at seq 0 / [`GENESIS_PREV_HASH`].
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .map_err(|err| JournalError::Io(format!("create_dir_all {}: {err}", dir.display())))?;
        let (seq, last_hash) = match latest_journal_path(&dir)? {
            Some(path) => match last_valid_entry(&path)? {
                Some(entry) => (entry.seq.saturating_add(1), entry.entry_hash),
                None => (0, GENESIS_PREV_HASH.to_string()),
            },
            None => (0, GENESIS_PREV_HASH.to_string()),
        };
        Ok(Self {
            dir,
            seq,
            last_hash,
        })
    }

    /// Next sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Journal an intent (`execution = {"status":"pending"}`) at `ts`.
    ///
    /// The entry lands in the day file named by `ts` (UTC) and the in-memory
    /// head only advances after a successful append.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn record_intent(
        &mut self,
        record: &IntentRecord,
        ts: DateTime<Utc>,
    ) -> Result<AuditEntry> {
        let entry = AuditEntry::new(
            self.seq,
            ts,
            record.trigger,
            record.account.clone(),
            record.market_id,
            record.input_hash.clone(),
            record.decision.clone(),
            record.policy_verdict.clone(),
            serde_json::json!({ "status": "pending" }),
            self.last_hash.clone(),
        );
        self.commit_entry(entry, ts)
    }

    /// Journal the outcome of a previously recorded intent.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn record_outcome(
        &mut self,
        record: &OutcomeRecord,
        ts: DateTime<Utc>,
    ) -> Result<AuditEntry> {
        let entry = AuditEntry::new(
            self.seq,
            ts,
            record.trigger,
            record.account.clone(),
            record.market_id,
            record.input_hash.clone(),
            record.decision.clone(),
            record.policy_verdict.clone(),
            record.execution.clone(),
            self.last_hash.clone(),
        );
        self.commit_entry(entry, ts)
    }

    /// Entries with `seq >= from_seq`, at most `limit`.
    ///
    /// Reads the journal's current day file (the latest `journal-*.jsonl`)
    /// and only it; blank/unparseable lines are skipped.
    ///
    /// # Errors
    /// `JournalError::Io` on I/O failure.
    pub fn read_entries(&self, from_seq: u64, limit: usize) -> Result<Vec<AuditEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(path) = latest_journal_path(&self.dir)? else {
            return Ok(Vec::new());
        };
        let content = fs::read_to_string(&path)
            .map_err(|err| JournalError::Io(format!("read {}: {err}", path.display())))?;
        let mut entries = Vec::new();
        for line in content.lines() {
            if entries.len() >= limit {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<AuditEntry>(line)
                && entry.seq >= from_seq
            {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Append `entry` to the day file for `ts` and advance the head.
    ///
    /// The in-memory head only moves after a successful append, so a failed
    /// write can be retried with the same seq/prev (no gaps, no phantom
    /// entries).
    fn commit_entry(&mut self, entry: AuditEntry, ts: DateTime<Utc>) -> Result<AuditEntry> {
        let Some(next_seq) = self.seq.checked_add(1) else {
            return Err(JournalError::Format(
                "journal sequence space exhausted at u64::MAX".to_string(),
            ));
        };
        append_line(&self.day_path(ts), &entry)?;
        self.seq = next_seq;
        self.last_hash = entry.entry_hash.clone();
        Ok(entry)
    }

    /// Day file for a UTC timestamp (`journal-YYYYMMDD.jsonl`).
    fn day_path(&self, ts: DateTime<Utc>) -> PathBuf {
        self.dir.join(format!(
            "{JOURNAL_FILE_PREFIX}{}{JOURNAL_FILE_SUFFIX}",
            ts.date_naive().format("%Y%m%d")
        ))
    }
}

/// Lexicographically-latest `journal-*.jsonl` file in `dir`, if any.
fn latest_journal_path(dir: &Path) -> Result<Option<PathBuf>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(JournalError::Io(format!(
                "read_dir {}: {err}",
                dir.display()
            )));
        }
    };
    let mut latest: Option<(String, PathBuf)> = None;
    for dirent in read_dir {
        let dirent =
            dirent.map_err(|err| JournalError::Io(format!("read_dir {}: {err}", dir.display())))?;
        let file_name = dirent.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(JOURNAL_FILE_PREFIX) || !name.ends_with(JOURNAL_FILE_SUFFIX) {
            continue;
        }
        if let Ok(file_type) = dirent.file_type()
            && !file_type.is_file()
        {
            continue;
        }
        let replace = match &latest {
            None => true,
            Some((best, _)) => name > best.as_str(),
        };
        if replace {
            latest = Some((name.to_string(), dirent.path()));
        }
    }
    Ok(latest.map(|(_, path)| path))
}

/// Last line of `path` that parses as an [`AuditEntry`] (a torn trailing
/// line is skipped with a warning on stderr).
fn last_valid_entry(path: &Path) -> Result<Option<AuditEntry>> {
    let content = fs::read_to_string(path)
        .map_err(|err| JournalError::Io(format!("read {}: {err}", path.display())))?;
    let mut last: Option<AuditEntry> = None;
    let mut last_line_malformed = false;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditEntry>(line) {
            Ok(entry) => {
                last = Some(entry);
                last_line_malformed = false;
            }
            Err(_) => last_line_malformed = true,
        }
    }
    if last_line_malformed {
        eprintln!(
            "sentinel-core audit: ignoring torn trailing line while resuming {}",
            path.display()
        );
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Account label used across the fixtures.
    const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

    /// Expected `entry_hash` values for [`chained_entries`], computed
    /// **independently** with a python3 sha256 oracle over the exact
    /// canonical pre-images (command run + output cited in the P10 report;
    /// script `/home/luna/.hermes/cache/scratch/p10_digest_oracle.py`):
    ///
    /// ```text
    /// python3 -c "$(cat p10_digest_oracle.py)"   # hashlib.sha256(bytes.fromhex(prev) + canon.encode())
    /// entry0 hash=0feae81cf62688b4a18ba0f4d7d23662b57d333127cd9c8c0e0f47be81a358e7
    /// entry1 hash=584d27d0529425140c83ba498f79a0fd8445d58f30890c99ed61ce23d256872f
    /// entry2 hash=d44ce90932682648bcead9f1cba6cddc60e40551dabaa9e3d559614420101a84
    /// hash_input ab=9610392a3396f1916b39cc898f68e2a8761d50953f7bd70a7c52f9c7a18174c6
    /// ```
    ///
    /// The oracle also asserts `json.dumps(json.loads(canon), sort_keys=True,
    /// separators=(",", ":")) == canon` for every pre-image.
    const DIGEST_VECTOR: [&str; 3] = [
        "0feae81cf62688b4a18ba0f4d7d23662b57d333127cd9c8c0e0f47be81a358e7",
        "584d27d0529425140c83ba498f79a0fd8445d58f30890c99ed61ce23d256872f",
        "d44ce90932682648bcead9f1cba6cddc60e40551dabaa9e3d559614420101a84",
    ];

    /// Expected `hash_input(&[part0, part1])` for the two parts asserted in
    /// [`hash_input_multi_part_and_order_sensitivity`], from the same
    /// python3 oracle run as [`DIGEST_VECTOR`].
    const HASH_INPUT_VECTOR: &str =
        "9610392a3396f1916b39cc898f68e2a8761d50953f7bd70a7c52f9c7a18174c6";

    static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid test timestamp")
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sentinel-p10-audit-{}-{tag}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn entry0() -> AuditEntry {
        AuditEntry::new(
            0,
            ts(2026, 10, 6, 2, 0, 0),
            Trigger::Reflex,
            ACCOUNT.to_string(),
            Some(32),
            "ab".repeat(32),
            json!({"action": "long", "confidence_bps": 8000, "provider": "qwen"}),
            json!({"verdict": "ALLOW", "note": "within caps"}),
            json!({"status": "pending"}),
            GENESIS_PREV_HASH.to_string(),
        )
    }

    fn entry1(prev_hash: &str) -> AuditEntry {
        AuditEntry::new(
            1,
            ts(2026, 10, 6, 2, 0, 1),
            Trigger::Reflex,
            ACCOUNT.to_string(),
            Some(32),
            "cd".repeat(32),
            json!({"action": "close", "reason": "reflex_take_profit"}),
            json!({"verdict": "ALLOW", "note": "position reduction"}),
            json!({"status": "simulated", "order_id": "ord-1"}),
            prev_hash.to_string(),
        )
    }

    fn entry2(prev_hash: &str) -> AuditEntry {
        AuditEntry::new(
            2,
            ts(2026, 10, 6, 2, 0, 2),
            Trigger::Strategy,
            ACCOUNT.to_string(),
            None,
            "ef".repeat(32),
            json!({"action": "none", "reason": "no_edge"}),
            json!({"verdict": "DENY", "note": "cooldown"}),
            json!({"status": "none"}),
            prev_hash.to_string(),
        )
    }

    fn chained_entries() -> Vec<AuditEntry> {
        let e0 = entry0();
        let e1 = entry1(&e0.entry_hash);
        let e2 = entry2(&e1.entry_hash);
        vec![e0, e1, e2]
    }

    fn intent_record() -> IntentRecord {
        IntentRecord {
            trigger: Trigger::Reflex,
            account: ACCOUNT.to_string(),
            market_id: Some(32),
            input_hash: "ab".repeat(32),
            decision: json!({"action": "long"}),
            policy_verdict: json!({"verdict": "ALLOW"}),
        }
    }

    fn outcome_record() -> OutcomeRecord {
        OutcomeRecord {
            trigger: Trigger::Reflex,
            account: ACCOUNT.to_string(),
            market_id: Some(32),
            input_hash: "ab".repeat(32),
            decision: json!({"action": "long"}),
            policy_verdict: json!({"verdict": "ALLOW"}),
            execution: json!({"status": "simulated", "order_id": "ord-1"}),
        }
    }

    fn canonical_line(entry: &AuditEntry) -> String {
        canonical_json(&serde_json::to_value(entry).expect("entry serializes"))
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::write(path, lines.join("\n") + "\n").expect("write journal file");
    }

    fn mutate_line(line: &str, mutate: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(line).expect("line parses");
        mutate(&mut value);
        serde_json::to_string(&value).expect("value serializes")
    }

    fn flip_first_hex(candidate: &str) -> String {
        let mut chars: Vec<char> = candidate.chars().collect();
        chars[0] = if chars[0] == '0' { '1' } else { '0' };
        chars.into_iter().collect()
    }

    #[test]
    fn digest_vector_matches_independent_python_sha256() {
        let entries = chained_entries();
        assert_eq!(entries[0].entry_hash, DIGEST_VECTOR[0]);
        assert_eq!(entries[1].entry_hash, DIGEST_VECTOR[1]);
        assert_eq!(entries[2].entry_hash, DIGEST_VECTOR[2]);
        assert_eq!(entries[0].prev_hash, GENESIS_PREV_HASH);
        assert_eq!(entries[1].prev_hash, entries[0].entry_hash);
        assert_eq!(entries[2].prev_hash, entries[1].entry_hash);

        // Re-parsed entries recompute to the same sealed hash.
        for entry in &entries {
            let round_trip: AuditEntry =
                serde_json::from_str(&canonical_line(entry)).expect("line parses");
            assert_eq!(round_trip.compute_hash(), entry.entry_hash);
        }

        // The pinned three-entry chain verifies as a file.
        let dir = tmp_dir("digest");
        let file = dir.join("journal-20261006.jsonl");
        let lines: Vec<String> = entries.iter().map(canonical_line).collect();
        write_lines(&file, &lines);
        let report = verify_chain(&file).expect("verify digest file");
        assert!(
            report.broken_at.is_none(),
            "digest chain intact: {report:?}"
        );
        assert_eq!(report.entries, 3);
        assert_eq!(report.first_seq, Some(0));
        assert_eq!(report.valid_up_to_seq, Some(2));
        assert_eq!(
            report.last_hash.as_deref(),
            Some(entries[2].entry_hash.as_str())
        );
    }

    #[test]
    fn canonical_json_sorts_keys_and_append_line_writes_canonical_line() {
        // Unknown-key insertion order cannot matter: Value maps are sorted.
        let value = json!({"b": 2, "a": 1});
        assert_eq!(canonical_json(&value), "{\"a\":1,\"b\":2}");

        let entry = chained_entries().remove(0);
        let line = canonical_line(&entry);
        let ordered = [
            "account",
            "decision",
            "entry_hash",
            "execution",
            "input_hash",
            "market_id",
            "policy_verdict",
            "prev_hash",
            "seq",
            "trigger",
            "ts",
        ];
        let mut cursor = 0;
        for key in ordered {
            let at = line
                .find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("key {key} missing from {line}"));
            assert!(at >= cursor, "key {key} out of canonical order in {line}");
            cursor = at;
        }

        let dir = tmp_dir("canonical");
        let path = dir.join("journal-20261006.jsonl");
        append_line(&path, &entry).expect("append");
        let raw = fs::read_to_string(&path).expect("read back");
        assert_eq!(raw, format!("{line}\n"));
    }

    #[test]
    fn fresh_journal_starts_with_genesis() {
        let dir = tmp_dir("fresh");
        let mut journal = AuditJournal::open(&dir).expect("open");
        assert_eq!(journal.seq(), 0);
        assert!(journal.read_entries(0, 10).expect("read").is_empty());

        let entry = journal
            .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 0))
            .expect("record");
        assert_eq!(entry.seq, 0);
        assert_eq!(entry.prev_hash, GENESIS_PREV_HASH);
        assert_eq!(entry.entry_hash, entry.compute_hash());
        assert_eq!(entry.entry_hash.len(), 64);
        assert!(
            entry
                .entry_hash
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert_eq!(entry.execution, json!({"status": "pending"}));

        let report = verify_chain(&dir.join("journal-20261006.jsonl")).expect("verify");
        assert!(
            report.broken_at.is_none(),
            "genesis chain intact: {report:?}"
        );
        assert_eq!(report.entries, 1);
        assert_eq!(report.valid_up_to_seq, Some(0));
    }

    #[test]
    fn reopen_resumes_seq_and_prev_hash() {
        let dir = tmp_dir("reopen");
        let (h0, h1);
        {
            let mut journal = AuditJournal::open(&dir).expect("open");
            let e0 = journal
                .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 0))
                .expect("record e0");
            let e1 = journal
                .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 1))
                .expect("record e1");
            assert_eq!(e1.prev_hash, e0.entry_hash);
            h0 = e0.entry_hash;
            h1 = e1.entry_hash;
        }
        let _ = h0;

        let mut journal = AuditJournal::open(&dir).expect("reopen");
        assert_eq!(journal.seq(), 2);
        let e2 = journal
            .record_outcome(&outcome_record(), ts(2026, 10, 6, 2, 0, 2))
            .expect("record e2");
        assert_eq!(e2.seq, 2);
        assert_eq!(e2.prev_hash, h1);

        let report = verify_chain(&dir.join("journal-20261006.jsonl")).expect("verify");
        assert!(
            report.broken_at.is_none(),
            "reopened chain intact: {report:?}"
        );
        assert_eq!(report.entries, 3);
        assert_eq!(report.valid_up_to_seq, Some(2));
        assert_eq!(report.last_hash, Some(e2.entry_hash));
    }

    #[test]
    fn day_rotation_continues_chain_across_files() {
        let dir = tmp_dir("rotate");
        let mut journal = AuditJournal::open(&dir).expect("open");
        let day1 = journal
            .record_intent(&intent_record(), ts(2026, 10, 6, 23, 59, 59))
            .expect("record day1");
        let day2 = journal
            .record_intent(&intent_record(), ts(2026, 10, 7, 0, 0, 1))
            .expect("record day2");
        assert_eq!(day2.seq, 1);
        assert_eq!(day2.prev_hash, day1.entry_hash);

        let file1 = dir.join("journal-20261006.jsonl");
        let file2 = dir.join("journal-20261007.jsonl");
        assert!(file1.exists() && file2.exists());

        // Each file verifies; day2's first entry keeps a boundary prev_hash.
        let report1 = verify_chain(&file1).expect("verify day1");
        assert!(report1.broken_at.is_none(), "{report1:?}");
        assert_eq!(report1.valid_up_to_seq, Some(0));
        let report2 = verify_chain(&file2).expect("verify day2");
        assert!(report2.broken_at.is_none(), "{report2:?}");
        assert_eq!(report2.first_seq, Some(1));
        assert_eq!(report2.valid_up_to_seq, Some(1));

        // read_entries reads the current (latest) day file only.
        let window = journal.read_entries(0, 10).expect("read");
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].seq, 1);

        // Reopen resumes across files and keeps the chain going.
        drop(journal);
        let mut reopened = AuditJournal::open(&dir).expect("reopen");
        assert_eq!(reopened.seq(), 2);
        let day2_next = reopened
            .record_intent(&intent_record(), ts(2026, 10, 7, 0, 0, 2))
            .expect("record day2 next");
        assert_eq!(day2_next.seq, 2);
        assert_eq!(day2_next.prev_hash, day2.entry_hash);
        let report2b = verify_chain(&file2).expect("verify day2 again");
        assert!(report2b.broken_at.is_none(), "{report2b:?}");
        assert_eq!(report2b.entries, 2);
        assert_eq!(report2b.valid_up_to_seq, Some(2));
    }

    #[test]
    fn tamper_matrix_reports_the_exact_seq() {
        let dir = tmp_dir("tamper");
        let path = dir.join("journal-20261006.jsonl");
        let base: Vec<String> = chained_entries().iter().map(canonical_line).collect();
        write_lines(&path, &base);
        let clean = verify_chain(&path).expect("verify clean");
        assert_eq!(clean.entries, 3);
        assert!(clean.broken_at.is_none(), "baseline intact: {clean:?}");

        // 1. one byte in `decision` (entry 1) -> entry_hash recompute fails.
        let mut lines = base.clone();
        lines[1] = mutate_line(&lines[1], |v| v["decision"]["action"] = json!("short"));
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(1), "decision tamper: {report:?}");
        assert_eq!(report.valid_up_to_seq, Some(0));
        assert!(
            report
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("entry_hash")
        );

        // 2. one byte in `execution` (last entry).
        let mut lines = base.clone();
        lines[2] = mutate_line(&lines[2], |v| v["execution"]["status"] = json!("failed"));
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(2), "execution tamper: {report:?}");
        assert!(
            report
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("entry_hash")
        );

        // 3. one byte in `prev_hash` (entry 1) -> linkage fails first.
        let mut lines = base.clone();
        lines[1] = mutate_line(&lines[1], |v| {
            let prev = v["prev_hash"].as_str().expect("prev_hash present");
            v["prev_hash"] = json!(flip_first_hex(prev));
        });
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(1), "prev_hash tamper: {report:?}");
        assert!(report.detail.as_deref().unwrap_or("").contains("prev_hash"));

        // 4. one byte in the FIRST entry's `prev_hash` (boundary): no
        //    linkage to check, the recompute must still catch it.
        let mut lines = base.clone();
        lines[0] = mutate_line(&lines[0], |v| {
            let prev = v["prev_hash"].as_str().expect("prev_hash present");
            v["prev_hash"] = json!(flip_first_hex(prev));
        });
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(0), "genesis prev tamper: {report:?}");
        assert!(
            report
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("entry_hash")
        );

        // 5. one byte in `entry_hash` (last entry).
        let mut lines = base.clone();
        lines[2] = mutate_line(&lines[2], |v| {
            let hash = v["entry_hash"].as_str().expect("entry_hash present");
            v["entry_hash"] = json!(flip_first_hex(hash));
        });
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(2), "entry_hash tamper: {report:?}");

        // 6. one byte in `seq` (entry 1 -> claims 9): the reported seq is the
        //    seq the tampered line now carries.
        let mut lines = base.clone();
        lines[1] = mutate_line(&lines[1], |v| v["seq"] = json!(9));
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(9), "seq tamper: {report:?}");
        assert_eq!(report.valid_up_to_seq, Some(0));
    }

    #[test]
    fn deleted_and_reordered_lines_break_the_chain() {
        let dir = tmp_dir("shape");
        let path = dir.join("journal-20261006.jsonl");
        let base: Vec<String> = chained_entries().iter().map(canonical_line).collect();

        // Deleted middle entry: seq 2 can no longer link to seq 0.
        let mut lines = base.clone();
        lines.remove(1);
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(2), "deleted middle: {report:?}");
        assert!(report.detail.as_deref().unwrap_or("").contains("prev_hash"));

        // Reordered first two entries: the boundary rule forgives entry 1 at
        // the top, but entry 0 can never link to it.
        let mut lines = base.clone();
        lines.swap(0, 1);
        write_lines(&path, &lines);
        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(0), "reordered: {report:?}");
        assert!(report.detail.as_deref().unwrap_or("").contains("prev_hash"));
    }

    #[test]
    fn torn_trailing_line_is_tolerated_and_resume_skips_it() {
        let dir = tmp_dir("torn");
        let file = dir.join("journal-20261006.jsonl");
        let entries = chained_entries();
        let lines: Vec<String> = entries.iter().map(canonical_line).collect();
        write_lines(&file, &lines);
        // Simulate kill -9 mid-append: a partial line with no newline.
        let mut content = fs::read_to_string(&file).expect("read");
        content.push_str(
            "{\"account\":\"0x0000000000000000000000000000000000000007\",\"entry_hash\":",
        );
        fs::write(&file, content).expect("write torn file");

        let report = verify_chain(&file).expect("verify torn");
        assert!(
            report.broken_at.is_none(),
            "torn tail tolerated: {report:?}"
        );
        assert_eq!(report.entries, 3);
        assert_eq!(report.valid_up_to_seq, Some(2));
        assert!(report.detail.as_deref().unwrap_or("").contains("torn"));

        // Reopen resumes from the last valid line, not the torn one.
        let mut journal = AuditJournal::open(&dir).expect("open");
        assert_eq!(journal.seq(), 3);
        let next = journal
            .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 3))
            .expect("record");
        assert_eq!(next.seq, 3);
        assert_eq!(next.prev_hash, entries[2].entry_hash);
    }

    #[test]
    fn interior_malformed_line_is_broken() {
        let dir = tmp_dir("interior");
        let path = dir.join("journal-20261006.jsonl");
        let mut lines: Vec<String> = chained_entries().iter().map(canonical_line).collect();
        lines[1] = "{\"seq\":1,\"broken".to_string();
        write_lines(&path, &lines);

        let report = verify_chain(&path).expect("verify");
        assert_eq!(report.broken_at, Some(1), "interior malformed: {report:?}");
        assert_eq!(report.valid_up_to_seq, Some(0));
        assert!(report.detail.as_deref().unwrap_or("").contains("malformed"));
    }

    #[test]
    fn read_entries_windows_and_limits() {
        let dir = tmp_dir("window");
        let mut journal = AuditJournal::open(&dir).expect("open");
        for second in 0..4u32 {
            journal
                .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, second))
                .expect("record");
        }

        let seqs = |entries: Vec<AuditEntry>| {
            entries
                .into_iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            seqs(journal.read_entries(0, 100).expect("read")),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            seqs(journal.read_entries(2, 100).expect("read")),
            vec![2, 3]
        );
        assert_eq!(seqs(journal.read_entries(0, 2).expect("read")), vec![0, 1]);
        assert!(journal.read_entries(9, 10).expect("read").is_empty());
        assert!(journal.read_entries(0, 0).expect("read").is_empty());
    }

    #[test]
    fn hash_input_multi_part_and_order_sensitivity() {
        let part0 = json!({"now_ms": 1234});
        let part1 = json!({"account": ACCOUNT});
        let forward = hash_input(&[&part0, &part1]);
        let backward = hash_input(&[&part1, &part0]);

        assert_eq!(forward.len(), 64);
        assert_ne!(forward, backward, "part order must change the hash");
        assert_eq!(forward, hash_input(&[&part0, &part1]));
        assert_eq!(forward, HASH_INPUT_VECTOR);

        // Same bytes as a single-shot sha256 over the concatenation.
        let concat = format!("{}{}", canonical_json(&part0), canonical_json(&part1));
        assert_eq!(forward, hex::encode(sha256(concat.as_bytes())));
    }

    #[test]
    fn record_intent_then_outcome_pairs() {
        let dir = tmp_dir("pair");
        let mut journal = AuditJournal::open(&dir).expect("open");
        let intent = journal
            .record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 0))
            .expect("intent");
        let outcome = journal
            .record_outcome(&outcome_record(), ts(2026, 10, 6, 2, 0, 1))
            .expect("outcome");

        assert_eq!(outcome.seq, intent.seq + 1);
        assert_eq!(outcome.prev_hash, intent.entry_hash);
        assert_eq!(outcome.input_hash, intent.input_hash);
        assert_eq!(
            outcome.execution,
            json!({"status": "simulated", "order_id": "ord-1"})
        );
        assert_eq!(journal.seq(), 2);

        let report = verify_chain(&dir.join("journal-20261006.jsonl")).expect("verify");
        assert!(report.broken_at.is_none(), "pair chain intact: {report:?}");
        assert_eq!(report.entries, 2);
        assert_eq!(report.valid_up_to_seq, Some(1));
    }

    #[test]
    fn u64_max_seq_boundary_is_guarded() {
        // A hand-made chain whose tail sits at the ceiling verifies without
        // arithmetic overflow, and resuming/recording past it is refused.
        let e0 = AuditEntry::new(
            7,
            ts(2026, 10, 6, 2, 0, 0),
            Trigger::System,
            ACCOUNT.to_string(),
            None,
            "ab".repeat(32),
            json!({"action": "none"}),
            json!({"verdict": "ALLOW"}),
            json!({"status": "none"}),
            GENESIS_PREV_HASH.to_string(),
        );
        let e1 = AuditEntry::new(
            u64::MAX,
            ts(2026, 10, 6, 2, 0, 1),
            Trigger::System,
            ACCOUNT.to_string(),
            None,
            "ab".repeat(32),
            json!({"action": "none"}),
            json!({"verdict": "ALLOW"}),
            json!({"status": "none"}),
            e0.entry_hash.clone(),
        );
        let dir = tmp_dir("umax");
        let path = dir.join("journal-20261006.jsonl");
        write_lines(&path, &[canonical_line(&e0), canonical_line(&e1)]);
        let report = verify_chain(&path).expect("verify");
        assert!(
            report.broken_at.is_none(),
            "ceiling chain intact: {report:?}"
        );
        assert_eq!(report.valid_up_to_seq, Some(u64::MAX));

        let mut journal = AuditJournal::open(&dir).expect("open");
        assert_eq!(journal.seq(), u64::MAX, "resume saturates at the ceiling");
        let err = journal.record_intent(&intent_record(), ts(2026, 10, 6, 2, 0, 2));
        assert!(matches!(err, Err(JournalError::Format(_))), "{err:?}");
    }

    #[test]
    fn verify_chain_missing_file_is_io_error() {
        let dir = tmp_dir("missing");
        let err = verify_chain(&dir.join("journal-20261006.jsonl"));
        assert!(matches!(err, Err(JournalError::Io(_))), "{err:?}");
    }
}
