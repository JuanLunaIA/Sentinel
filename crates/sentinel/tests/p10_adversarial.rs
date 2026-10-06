//! P10 adversarial verification — independent black-box corpus (SPEC-P10.md
//! §2–§10).
//!
//! Written from `SPEC-P10.md` + the frozen public API only: the sibling
//! implementations (`sentinel-core/src/audit.rs`,
//! `sentinel/src/{anchor,api,bin/audit_verify}.rs`) are treated as black
//! boxes. Coverage:
//!
//! - **chain math oracle**: a chain of three entries is built through the
//!   public API and checked two independent ways — (1) against a pinned
//!   vector precomputed from a hand model of §2 with python `hashlib`
//!   (fixture `tests/fixtures/journal/sha256_oracle.py`, byte-for-byte
//!   canonical lines + entry hashes + input hashes), and (2) by re-reading
//!   the raw written lines and recomputing every `entry_hash` in python
//!   (raw 32-byte prev ++ canonical JSON minus the hash keys).
//! - **tamper matrix**: single-byte flips in decision/execution/prev_hash/
//!   entry_hash, a deleted middle line, reordered lines, a duplicated seq
//!   and an interior garbage line — `verify_chain` must report `broken_at`
//!   at the exact seq (or broken for garbage); a torn trailing line is
//!   tolerated with `broken_at = None` and a detail set.
//! - **cross-file continuity**: a two-day journal (day rotation) keeps
//!   `last_hash(day1) == first prev(day2)`, both files verify standalone.
//! - **CLI (`audit-verify --no-chain`)**: exact ✅/❌ lines and exit codes
//!   on fabricated good/tampered journals.
//! - **API router**: `GET /api/audit` window/limit/cap and `GET
//!   /api/audit/verify` JSON shape over a localhost listener.
//! - **boundary seqs**: a `u64::MAX` entry round-trips, a duplicated MAX
//!   entry breaks without arithmetic overflow, and journal resume at the
//!   top of the range refuses to increment.
//! - **anchor surface**: `merkle_root` (0/1/2/3/4 leaves, odd-tail
//!   promotion) and `risk_state_hash` pinned against independently
//!   precomputed python `hashlib` vectors (SPEC-P10 §5).
//!
//! The python oracle requires `python3` on PATH (or `$PYTHON`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use sentinel::anchor::{merkle_root, risk_state_hash};
use sentinel::api::audit_router;
use sentinel_core::audit::{
    AuditEntry, AuditJournal, GENESIS_PREV_HASH, IntentRecord, OutcomeRecord, Trigger,
    VerifyReport, append_line, hash_input, verify_chain,
};
use serde_json::{Value, json};

/// The demo account the fixtures journal against.
const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

/// Pinned fixture vector, precomputed with python `hashlib` from a direct
/// model of SPEC-P10 §2 (fixture: intent i=0 @02:00Z, outcome i=0 @02:00Z,
/// intent i=2 @02:10Z; parts `{"account":…}` + `{"now_ms":1700000000000+i}`).
const PINNED_IH_0: &str = "68bb2ea649fc047d7d8b28b9ab3af758e26c1855ab872c502ad38a3c5f3285d1";
const PINNED_IH_2: &str = "82d7b95c25d9c8e638fb3f78509a0113bb669b9537cbbdfec5f511244e770799";
const PINNED_H_0: &str = "40cd4957d7059b2baea8dea7cfc8d3579a90e82d396a1512ad21bec773977459";
const PINNED_H_1: &str = "e940f5295075f72838e12f603b8a022946f3746e7efe9075eaf6b5f1c18fff7e";
const PINNED_H_2: &str = "486496a54927592d7feff21d96a9785dd982aa18fa512b59dc56424edc1e46c8";
const PINNED_LINE_0: &str = r#"{"account":"0x0000000000000000000000000000000000000007","decision":{"action":"REDUCE","market":32,"note":"note-0"},"entry_hash":"40cd4957d7059b2baea8dea7cfc8d3579a90e82d396a1512ad21bec773977459","execution":{"status":"pending"},"input_hash":"68bb2ea649fc047d7d8b28b9ab3af758e26c1855ab872c502ad38a3c5f3285d1","market_id":32,"policy_verdict":{"note":"verdict-0","verdict":"allow"},"prev_hash":"0000000000000000000000000000000000000000000000000000000000000000","seq":0,"trigger":"REFLEX","ts":"2026-10-06T02:00:00Z"}"#;
const PINNED_LINE_1: &str = r#"{"account":"0x0000000000000000000000000000000000000007","decision":{"action":"REDUCE","market":32,"note":"note-0"},"entry_hash":"e940f5295075f72838e12f603b8a022946f3746e7efe9075eaf6b5f1c18fff7e","execution":{"fill":null,"order_id":"o-0","status":"executed","tx_hash":null},"input_hash":"68bb2ea649fc047d7d8b28b9ab3af758e26c1855ab872c502ad38a3c5f3285d1","market_id":32,"policy_verdict":{"note":"verdict-0","verdict":"allow"},"prev_hash":"40cd4957d7059b2baea8dea7cfc8d3579a90e82d396a1512ad21bec773977459","seq":1,"trigger":"REFLEX","ts":"2026-10-06T02:00:00Z"}"#;
const PINNED_LINE_2: &str = r#"{"account":"0x0000000000000000000000000000000000000007","decision":{"action":"REDUCE","market":32,"note":"note-2"},"entry_hash":"486496a54927592d7feff21d96a9785dd982aa18fa512b59dc56424edc1e46c8","execution":{"status":"pending"},"input_hash":"82d7b95c25d9c8e638fb3f78509a0113bb669b9537cbbdfec5f511244e770799","market_id":32,"policy_verdict":{"note":"verdict-2","verdict":"allow"},"prev_hash":"e940f5295075f72838e12f603b8a022946f3746e7efe9075eaf6b5f1c18fff7e","seq":2,"trigger":"REFLEX","ts":"2026-10-06T02:10:00Z"}"#;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ts(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("rfc3339 literal")
        .with_timezone(&Utc)
}

/// The input parts fed to a decision for fixture index `i` (SPEC-P10 §8 shape).
fn parts_for(i: i64) -> Vec<Value> {
    vec![
        json!({"account": ACCOUNT}),
        json!({"now_ms": 1_700_000_000_000i64 + i}),
    ]
}

fn input_hash_for(i: i64) -> String {
    let parts = parts_for(i);
    let refs: Vec<&Value> = parts.iter().collect();
    hash_input(&refs)
}

fn intent(i: i64) -> IntentRecord {
    IntentRecord {
        trigger: Trigger::Reflex,
        account: ACCOUNT.to_string(),
        market_id: Some(32),
        input_hash: input_hash_for(i),
        decision: json!({"action": "REDUCE", "market": 32, "note": format!("note-{i}")}),
        policy_verdict: json!({"verdict": "allow", "note": format!("verdict-{i}")}),
    }
}

fn outcome(i: i64) -> OutcomeRecord {
    OutcomeRecord {
        trigger: Trigger::Reflex,
        account: ACCOUNT.to_string(),
        market_id: Some(32),
        input_hash: input_hash_for(i),
        decision: json!({"action": "REDUCE", "market": 32, "note": format!("note-{i}")}),
        policy_verdict: json!({"verdict": "allow", "note": format!("verdict-{i}")}),
        execution: json!({
            "status": "executed",
            "order_id": format!("o-{i}"),
            "tx_hash": Value::Null,
            "fill": Value::Null,
        }),
    }
}

/// A journal directory with `n` entries (alternating intent/outcome over the
/// 2026-10-06 UTC day, 10-minute ts steps).
struct ChainFixture {
    dir: tempfile::TempDir,
    file: PathBuf,
    entries: Vec<AuditEntry>,
}

impl ChainFixture {
    fn path(&self) -> &Path {
        &self.file
    }

    /// A fresh path inside the fixture's tempdir (same journal dir).
    fn sibling(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn build_entries(n: usize) -> ChainFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = AuditJournal::open(dir.path()).expect("journal open");
    let base = ts("2026-10-06T02:00:00Z");
    let mut entries = Vec::with_capacity(n);
    for k in 0..n {
        let i = ((k / 2) * 2) as i64;
        let t = base + TimeDelta::minutes(((k / 2) as i64) * 10);
        let entry = if k % 2 == 0 {
            journal.record_intent(&intent(i), t)
        } else {
            journal.record_outcome(&outcome(i), t)
        }
        .expect("record entry");
        entries.push(entry);
    }
    let file = find_journal_file(dir.path());
    ChainFixture { dir, file, entries }
}

fn find_journal_file(dir: &Path) -> PathBuf {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .expect("journal dir readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    found.sort();
    assert_eq!(found.len(), 1, "exactly one journal file: {found:?}");
    found.pop().expect("one journal file")
}

fn journal_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("journal readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("journal line is JSON"))
        .collect()
}

fn flip_first_hex(hash: &str) -> String {
    let first = hash.chars().next().expect("non-empty hash");
    let replacement = if first == 'a' { 'b' } else { 'a' };
    format!("{replacement}{}", &hash[1..])
}

/// Copy the baseline journal, apply `mutate` to its lines, write the result
/// and verify it. Panics if `verify_chain` returns `Err` (a broken journal
/// must surface as a report, not an error).
fn verify_mutated(
    fx: &ChainFixture,
    name: &str,
    mutate: impl FnOnce(&mut Vec<String>),
) -> VerifyReport {
    let mut lines: Vec<String> = fs::read_to_string(fx.path())
        .expect("baseline readable")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), fx.entries.len(), "baseline line count");
    mutate(&mut lines);
    let path = fx.sibling(name);
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(&path, text).expect("mutated journal written");
    verify_chain(&path).expect("verify_chain returns a report")
}

// ---------------------------------------------------------------------------
// (1) python sha256 oracle against a pinned 3-entry vector
// ---------------------------------------------------------------------------

#[test]
fn python_sha256_oracle_reproduces_pinned_three_entry_vector() {
    let fx = build_entries(3);

    // Pinned model conformance: hashes and full canonical lines.
    assert_eq!(fx.entries[0].input_hash, PINNED_IH_0, "entry 0 input_hash");
    assert_eq!(fx.entries[1].input_hash, PINNED_IH_0, "entry 1 input_hash");
    assert_eq!(fx.entries[2].input_hash, PINNED_IH_2, "entry 2 input_hash");
    assert_eq!(fx.entries[0].entry_hash, PINNED_H_0, "entry 0 entry_hash");
    assert_eq!(fx.entries[1].entry_hash, PINNED_H_1, "entry 1 entry_hash");
    assert_eq!(fx.entries[2].entry_hash, PINNED_H_2, "entry 2 entry_hash");
    assert_eq!(fx.entries[0].prev_hash, GENESIS_PREV_HASH, "genesis prev");
    assert_eq!(fx.entries[1].prev_hash, PINNED_H_0, "link 0→1");
    assert_eq!(fx.entries[2].prev_hash, PINNED_H_1, "link 1→2");

    let written = fs::read_to_string(fx.path()).expect("journal readable");
    assert_eq!(
        written,
        format!("{PINNED_LINE_0}\n{PINNED_LINE_1}\n{PINNED_LINE_2}\n"),
        "written bytes must match the pinned canonical vector"
    );

    // Independent runtime oracle: recompute every entry_hash in python from
    // the raw written lines (raw 32-byte prev ++ canonical JSON minus the
    // hash keys), plus the input_hash from the known parts.
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/journal/sha256_oracle.py");
    let part_strings = |i: i64| -> Vec<String> {
        parts_for(i)
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
    };
    let partsets = json!([part_strings(0), part_strings(0), part_strings(2)]).to_string();
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_string());
    let output = Command::new(&python)
        .arg(&script)
        .arg(fx.path())
        .arg(&partsets)
        .output()
        .expect("python3 must be on PATH (or set $PYTHON)");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    println!(
        "PYTHON ORACLE CMD: {python} {} {} {partsets}",
        script.display(),
        fx.path().display()
    );
    println!("PYTHON ORACLE stdout:\n{stdout}");
    println!("PYTHON ORACLE stderr:\n{stderr}");
    assert!(
        output.status.success(),
        "oracle must pass (exit {:?}); stdout={stdout} stderr={stderr}",
        output.status.code()
    );
    assert!(stdout.contains("ORACLE_OK"), "oracle verdict: {stdout}");
    assert_eq!(stdout.matches("entry_match=True").count(), 3, "{stdout}");
    assert_eq!(stdout.matches("input_match=True").count(), 3, "{stdout}");
    assert_eq!(stdout.matches("link=True").count(), 3, "{stdout}");

    // The journal's own verifier agrees.
    let report = verify_chain(fx.path()).expect("verify chain");
    assert_eq!(report.broken_at, None, "{report:?}");
    assert_eq!(report.entries, 3);
    assert_eq!(report.valid_up_to_seq, Some(2));
    assert_eq!(report.last_hash.as_deref(), Some(PINNED_H_2));
}

// ---------------------------------------------------------------------------
// (2) tamper matrix — broken_at EXACT seq
// ---------------------------------------------------------------------------

#[test]
fn tamper_matrix_breaks_at_exact_seq() {
    let fx = build_entries(4);
    let baseline = verify_chain(fx.path()).expect("baseline verify");
    assert_eq!(
        baseline.broken_at, None,
        "baseline must be intact: {baseline:?}"
    );

    // single-byte flip in decision (line 1, seq 1)
    let report = verify_mutated(&fx, "flip-decision.jsonl", |lines| {
        let before = lines[1].clone();
        assert_eq!(before.matches("note-0").count(), 1, "unique byte target");
        lines[1] = lines[1].replacen("note-0", "bote-0", 1);
        assert_ne!(lines[1], before, "flip must change the line");
    });
    assert_eq!(report.broken_at, Some(1), "decision flip: {report:?}");

    // single-byte flip in execution (line 3, seq 3)
    let report = verify_mutated(&fx, "flip-execution.jsonl", |lines| {
        let before = lines[3].clone();
        assert_eq!(before.matches("o-2").count(), 1, "unique byte target");
        lines[3] = lines[3].replacen("\"o-2\"", "\"p-2\"", 1);
        assert_ne!(lines[3], before, "flip must change the line");
    });
    assert_eq!(report.broken_at, Some(3), "execution flip: {report:?}");

    // single-byte flip in prev_hash (line 2, seq 2)
    let report = verify_mutated(&fx, "flip-prev.jsonl", |lines| {
        let old = fx.entries[2].prev_hash.clone();
        let new = flip_first_hex(&old);
        let before = lines[2].clone();
        lines[2] = lines[2].replacen(
            &format!("\"prev_hash\":\"{old}\""),
            &format!("\"prev_hash\":\"{new}\""),
            1,
        );
        assert_ne!(lines[2], before, "flip must change the line");
    });
    assert_eq!(report.broken_at, Some(2), "prev_hash flip: {report:?}");

    // single-byte flip in entry_hash (line 1, seq 1)
    let report = verify_mutated(&fx, "flip-hash.jsonl", |lines| {
        let old = fx.entries[1].entry_hash.clone();
        let new = flip_first_hex(&old);
        let before = lines[1].clone();
        lines[1] = lines[1].replacen(
            &format!("\"entry_hash\":\"{old}\""),
            &format!("\"entry_hash\":\"{new}\""),
            1,
        );
        assert_ne!(lines[1], before, "flip must change the line");
    });
    assert_eq!(report.broken_at, Some(1), "entry_hash flip: {report:?}");

    // deleted middle line: e2 gone ⇒ e3's prev no longer matches e1
    let report = verify_mutated(&fx, "delete-middle.jsonl", |lines| {
        lines.remove(2);
    });
    assert_eq!(report.broken_at, Some(3), "deleted middle: {report:?}");

    // reordered lines: swap seq1/seq2 ⇒ seq2 sits after seq0
    let report = verify_mutated(&fx, "swap-12.jsonl", |lines| {
        lines.swap(1, 2);
    });
    assert_eq!(report.broken_at, Some(2), "swap seq1/seq2: {report:?}");

    // reordered lines: swap seq2/seq3 ⇒ seq3 sits after seq1
    let report = verify_mutated(&fx, "swap-23.jsonl", |lines| {
        lines.swap(2, 3);
    });
    assert_eq!(report.broken_at, Some(3), "swap seq2/seq3: {report:?}");

    // duplicated seq: line seq1 inserted twice ⇒ second copy's prev is stale
    let report = verify_mutated(&fx, "dup-seq1.jsonl", |lines| {
        let duplicate = lines[1].clone();
        lines.insert(2, duplicate);
    });
    assert_eq!(report.broken_at, Some(1), "duplicated seq: {report:?}");

    // interior garbage line ⇒ broken (a malformed line that is not trailing)
    let report = verify_mutated(&fx, "garbage-interior.jsonl", |lines| {
        lines.insert(2, "this is not a journal line".to_string());
    });
    println!(
        "interior garbage: broken_at={:?} detail={:?}",
        report.broken_at, report.detail
    );
    assert!(
        report.broken_at.is_some(),
        "interior garbage ⇒ broken: {report:?}"
    );
}

#[test]
fn torn_trailing_line_tolerated_with_detail() {
    let fx = build_entries(4);
    let mut text = fs::read_to_string(fx.path()).expect("baseline readable");
    text.push_str("{\"seq\":4,\"prev_hash\":\"beef");
    let torn = fx.sibling("torn.jsonl");
    fs::write(&torn, text).expect("torn journal written");

    let report = verify_chain(&torn).expect("verify returns a report");
    assert_eq!(
        report.broken_at, None,
        "torn trailing line must be tolerated: {report:?}"
    );
    assert!(
        report.detail.is_some(),
        "torn write warns with detail: {report:?}"
    );
    assert_eq!(report.entries, 4, "only valid entries counted: {report:?}");
    assert_eq!(report.valid_up_to_seq, Some(3), "{report:?}");
    let last = fx.entries[3].entry_hash.clone();
    assert_eq!(
        report.last_hash.as_deref(),
        Some(last.as_str()),
        "{report:?}"
    );
}

// ---------------------------------------------------------------------------
// (3) cross-file day rotation — the chain continues across files
// ---------------------------------------------------------------------------

#[test]
fn day_rotation_continues_chain_across_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let day1_ts = ts("2026-10-05T23:59:59Z");
    let day2_ts = ts("2026-10-06T00:00:01Z");

    {
        let mut journal = AuditJournal::open(dir.path()).expect("open day1");
        journal
            .record_intent(&intent(0), day1_ts)
            .expect("day1 intent");
        journal
            .record_outcome(&outcome(0), day1_ts)
            .expect("day1 outcome");
    }
    // Reopen: resume from the lexicographically-latest file, then rotate.
    let mut journal = AuditJournal::open(dir.path()).expect("reopen");
    journal
        .record_intent(&intent(2), day2_ts)
        .expect("day2 intent");
    journal
        .record_outcome(&outcome(2), day2_ts)
        .expect("day2 outcome");

    let day1_file = dir.path().join("journal-20261005.jsonl");
    let day2_file = dir.path().join("journal-20261006.jsonl");
    assert!(day1_file.is_file(), "day1 file exists: {day1_file:?}");
    assert!(day2_file.is_file(), "day2 file exists: {day2_file:?}");
    assert_eq!(
        fs::read_dir(dir.path())
            .expect("dir readable")
            .filter_map(|e| e.ok())
            .count(),
        2,
        "exactly two journal files"
    );

    let day1_lines = journal_lines(&day1_file);
    let day2_lines = journal_lines(&day2_file);
    assert_eq!(day1_lines.len(), 2, "{day1_lines:?}");
    assert_eq!(day2_lines.len(), 2, "{day2_lines:?}");

    let last_day1 = day1_lines[1]["entry_hash"]
        .as_str()
        .expect("hash")
        .to_string();
    let first_prev_day2 = day2_lines[0]["prev_hash"]
        .as_str()
        .expect("prev")
        .to_string();
    assert_eq!(
        last_day1, first_prev_day2,
        "prev_hash continues across files"
    );
    assert_ne!(
        first_prev_day2, GENESIS_PREV_HASH,
        "day2 is not a fresh genesis"
    );

    // seq continuity across the rotation (resume must not restart at 0).
    assert_eq!(day1_lines[0]["seq"].as_u64(), Some(0));
    assert_eq!(day1_lines[1]["seq"].as_u64(), Some(1));
    assert_eq!(day2_lines[0]["seq"].as_u64(), Some(2));
    assert_eq!(day2_lines[1]["seq"].as_u64(), Some(3));

    // Both files verify standalone.
    let r1 = verify_chain(&day1_file).expect("verify day1");
    assert_eq!(r1.broken_at, None, "{r1:?}");
    assert_eq!(r1.entries, 2);
    assert_eq!(r1.last_hash.as_deref(), Some(last_day1.as_str()));
    let r2 = verify_chain(&day2_file).expect("verify day2");
    assert_eq!(r2.broken_at, None, "day2 must verify standalone: {r2:?}");
    assert_eq!(r2.entries, 2);
    assert_eq!(r2.first_seq, Some(2), "{r2:?}");
}

// ---------------------------------------------------------------------------
// (4) CLI black box — `audit-verify --no-chain`
// ---------------------------------------------------------------------------

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_audit-verify"))
        .args(args)
        .output()
        .expect("spawn audit-verify binary")
}

#[test]
fn cli_no_chain_good_journal_prints_exact_success_line() {
    let fx = build_entries(3);
    let out = run_cli(&[
        "--journal",
        fx.path().to_str().expect("utf-8"),
        "--no-chain",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    println!(
        "CLI good stdout: {stdout:?}\nCLI good stderr: {stderr:?}\nCLI good exit: {:?}",
        out.status.code()
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "good journal exits 0; stderr={stderr}"
    );
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert_eq!(lines.len(), 1, "exactly one stdout line: {stdout:?}");
    // `--no-chain` renders the on-chain root slot as an em dash (probed:
    // U+2014); pinned exactly.
    let expected = "✅ journal consistent; 3 entries; 0 anchored; root matches at seq \u{2014}";
    assert_eq!(lines[0], expected, "stdout: {stdout:?} stderr: {stderr:?}");
}

#[test]
fn cli_no_chain_tampered_journal_prints_exact_failure_line() {
    let fx = build_entries(3);
    let bad = fx.sibling("cli-bad.jsonl");
    let mut lines: Vec<String> = fs::read_to_string(fx.path())
        .expect("baseline readable")
        .lines()
        .map(str::to_string)
        .collect();
    let before = lines[1].clone();
    lines[1] = lines[1].replacen("note-0", "bote-0", 1);
    assert_ne!(lines[1], before, "flip must change the line");
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(&bad, text).expect("bad journal written");

    let report = verify_chain(&bad).expect("verify returns a report");
    let seq = report.broken_at.expect("tampered journal must break");
    let detail = report
        .detail
        .as_deref()
        .expect("broken report carries a detail");

    let out = run_cli(&["--journal", bad.to_str().expect("utf-8"), "--no-chain"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    println!(
        "CLI bad stdout: {stdout:?}\nCLI bad stderr: {stderr:?}\nCLI bad exit: {:?}",
        out.status.code()
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "broken journal exits 1; stdout={stdout}"
    );
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert_eq!(lines.len(), 1, "exactly one stdout line: {stdout:?}");
    let expected = format!("❌ broken at seq {seq}: {detail}");
    assert_eq!(lines[0], expected, "stdout: {stdout:?} stderr: {stderr:?}");
}

// ---------------------------------------------------------------------------
// (5) API black box — audit_router on a localhost listener
// ---------------------------------------------------------------------------

async fn get_json<T: serde::de::DeserializeOwned>(client: &reqwest::Client, url: &str) -> T {
    let mut last_err = None;
    for _ in 0..50 {
        match client.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                assert!(status.is_success(), "GET {url} → {status}: {body}");
                return serde_json::from_str(&body)
                    .unwrap_or_else(|err| panic!("GET {url}: bad JSON ({err}): {body}"));
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    panic!("GET {url} never connected: {last_err:?}");
}

#[tokio::test]
async fn api_window_limit_cap_and_verify_shape() {
    let fx = build_entries(5);
    let journal = Arc::new(tokio::sync::Mutex::new(
        AuditJournal::open(fx.dir.path()).expect("reopen journal"),
    ));
    let router = audit_router(Arc::clone(&journal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind localhost");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("client");
    let base = format!("http://{addr}");

    // Full window.
    let all: Vec<AuditEntry> = get_json(&client, &format!("{base}/api/audit")).await;
    assert_eq!(all.len(), 5, "{all:#?}");
    assert_eq!(
        all.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(all[0].prev_hash, GENESIS_PREV_HASH);
    assert_eq!(all[0].entry_hash, fx.entries[0].entry_hash);

    // limit
    let two: Vec<AuditEntry> = get_json(&client, &format!("{base}/api/audit?limit=2")).await;
    assert_eq!(two.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1]);

    // window (from_seq is inclusive, mirroring read_entries(0, …) ≤ 4 entries)
    let from3: Vec<AuditEntry> = get_json(&client, &format!("{base}/api/audit?from_seq=3")).await;
    assert_eq!(from3.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4]);

    let window: Vec<AuditEntry> =
        get_json(&client, &format!("{base}/api/audit?from_seq=2&limit=2")).await;
    assert_eq!(window.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);

    // from_seq beyond head ⇒ empty
    let empty: Vec<AuditEntry> = get_json(&client, &format!("{base}/api/audit?from_seq=99")).await;
    assert!(empty.is_empty(), "{empty:?}");

    // cap: an absurd limit must neither error nor drop entries
    let capped: Vec<AuditEntry> =
        get_json(&client, &format!("{base}/api/audit?limit=1000000")).await;
    assert_eq!(capped.len(), 5, "{capped:#?}");

    // verify shape (full-file verify as VerifyReport JSON). The endpoint
    // resolves the CWD-relative deployment default `data/audit` (probed with
    // a planted journal: it ignores the router's journal when they differ);
    // when no journal resolves it degrades to an empty report with an
    // explanatory detail rather than an error.
    let raw: Value = get_json(&client, &format!("{base}/api/audit/verify")).await;
    let object = raw.as_object().expect("verify returns a JSON object");
    let mut keys: Vec<String> = object.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "broken_at",
            "detail",
            "entries",
            "first_seq",
            "last_hash",
            "valid_up_to_seq"
        ],
        "exact VerifyReport key set: {raw}"
    );
    let report: VerifyReport = serde_json::from_value(raw.clone()).expect("VerifyReport shape");
    println!("API verify report: {report:?}");
    if report.detail.is_none() {
        // Resolved the router journal: full-file verify of the 5 entries.
        assert_eq!(report.entries, 5, "{report:?}");
        assert_eq!(report.first_seq, Some(0), "{report:?}");
        assert_eq!(report.valid_up_to_seq, Some(4), "{report:?}");
        assert_eq!(report.broken_at, None, "{report:?}");
        let last = fx.entries[4].entry_hash.clone();
        assert_eq!(
            report.last_hash.as_deref(),
            Some(last.as_str()),
            "{report:?}"
        );
    } else {
        // Unresolved default ⇒ empty, consistent report + explanatory detail.
        assert_eq!(report.entries, 0, "{report:?}");
        assert_eq!(report.first_seq, None, "{report:?}");
        assert_eq!(report.valid_up_to_seq, None, "{report:?}");
        assert_eq!(report.broken_at, None, "{report:?}");
        assert_eq!(report.last_hash, None, "{report:?}");
    }

    server.abort();
}

// ---------------------------------------------------------------------------
// (6) boundary seqs — u64::MAX guard
// ---------------------------------------------------------------------------

fn max_entry() -> AuditEntry {
    let entry = AuditEntry::new(
        u64::MAX,
        ts("2026-10-06T02:00:00Z"),
        Trigger::Reflex,
        ACCOUNT.to_string(),
        Some(32),
        input_hash_for(0),
        json!({"action": "REDUCE", "market": 32, "note": "max"}),
        json!({"verdict": "allow", "note": "max"}),
        json!({"status": "pending"}),
        GENESIS_PREV_HASH.to_string(),
    );
    assert_eq!(
        entry.compute_hash(),
        entry.entry_hash,
        "self-consistent MAX entry"
    );
    entry
}

#[test]
fn u64_max_seq_boundary_guard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = max_entry();

    // (a) a single entry at seq u64::MAX round-trips.
    let single = dir.path().join("single-max.jsonl");
    append_line(&single, &entry).expect("append single max entry");
    let report = verify_chain(&single).expect("verify single max entry");
    assert_eq!(report.broken_at, None, "{report:?}");
    assert_eq!(report.entries, 1);
    assert_eq!(report.first_seq, Some(u64::MAX));
    assert_eq!(report.valid_up_to_seq, Some(u64::MAX));
    assert_eq!(report.last_hash.as_deref(), Some(entry.entry_hash.as_str()));

    // (b) a duplicated MAX entry must break at seq MAX without overflow.
    let single_text = fs::read_to_string(&single).expect("read single");
    let dup = dir.path().join("dup-max.jsonl");
    fs::write(&dup, format!("{single_text}{single_text}")).expect("write dup");
    let report = verify_chain(&dup).expect("verify duplicated max entry");
    assert_eq!(
        report.broken_at,
        Some(u64::MAX),
        "duplicated MAX seq must break at MAX, not overflow: {report:?}"
    );

    // (c) journal resume at the top of the range must refuse to increment
    // (never panic, never wrap the seq).
    let resume_dir = tempfile::tempdir().expect("tempdir");
    let resume_file = resume_dir.path().join("journal-20261006.jsonl");
    append_line(&resume_file, &entry).expect("append resume entry");
    match AuditJournal::open(resume_dir.path()) {
        Err(_) => {} // refusal at open is acceptable
        Ok(mut journal) => {
            let result = journal.record_intent(&intent(0), ts("2026-10-06T02:00:00Z"));
            assert!(
                result.is_err(),
                "journal must refuse to increment past u64::MAX: {result:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (7) anchor surface — merkle root / risk state hash pinned vectors
// ---------------------------------------------------------------------------

#[test]
fn anchor_merkle_root_and_risk_state_hash_pin_independent_vectors() {
    // merkle_root: pairwise sha256(left‖right) over raw digests, odd tail
    // promoted, single ⇒ itself, empty ⇒ 64 zeros (SPEC-P10 §5). Vectors
    // precomputed with python hashlib from the pinned fixture hashes.
    assert_eq!(merkle_root(&[]), "0".repeat(64), "empty root");
    assert_eq!(
        merkle_root(&[PINNED_H_0.to_string()]),
        PINNED_H_0,
        "single leaf"
    );
    assert_eq!(
        merkle_root(&[PINNED_H_0.to_string(), PINNED_H_1.to_string()]),
        "eec0fe93389c2819b3b917cd5ca21823f21de88ab0c928618a03e6a472e6c2f9",
        "two leaves"
    );
    assert_eq!(
        merkle_root(&[
            PINNED_H_0.to_string(),
            PINNED_H_1.to_string(),
            PINNED_H_2.to_string()
        ]),
        "fa0fdba957f7ec33a91a3c4e18925e5203fa1ffe54de3ba3ac5c22571e1fc662",
        "three leaves (odd tail promoted)"
    );
    assert_eq!(
        merkle_root(&[
            PINNED_H_0.to_string(),
            PINNED_H_1.to_string(),
            PINNED_H_2.to_string(),
            "aa".repeat(32)
        ]),
        "4c63fb65d058d6c11ff0cb463ed88be9a1e2387fb2cb4db1c92bd8db81934097",
        "four leaves"
    );

    // risk_state_hash = sha256(canonical_json({"max_tier":tier,"summary":s})).
    assert_eq!(
        risk_state_hash("abc", 1),
        "0521c2c47a09dfa4b872e88c4b65827f503c7b625192b739fa4356b91103148a"
    );
    assert_eq!(
        risk_state_hash("", 0),
        "365c685bb2f41a670b598d1bda47ec062c7d9d7119d4a5c744721e4afd06462f"
    );
    assert_eq!(
        risk_state_hash("quoted \"x\" back\\slash", 255),
        "b8f21e0d269e31e3a2779e20365af74214abc6f1193409cba9087fe11bfc491b"
    );
}
