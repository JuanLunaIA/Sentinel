//! P16 adversarial verifier suite (agent M) — SPEC-P16.
//!
//! Written against the frozen SPEC text only; exercises the writers' public
//! surfaces as a black box:
//!
//! 1. **secret scan** (SPEC-P16 §3): the evidence corpus, data/log samples and
//!    a generated sample log must never contain non-placeholder `.env` secret
//!    values, Bearer tokens, `sk-`-style keys, or 64-hex key material in
//!    key context — while redaction markers, hash digests and the one
//!    documented public anvil dev key stay tolerated. A negative control
//!    proves the scanner is not vacuous.
//! 2. **audit degrade** (SPEC-P16 §2, L): read-only dir AND a 64 KiB tmpfs
//!    disk-full path; the in-flight entry must never be lost and the next
//!    append must persist BOTH entries with the chain intact.
//! 3. **supervisor** (SPEC-P16 §2, K): panic ⇒ restart + survive; shutdown ⇒
//!    restarts stop and the supervising loop exits (no zombie loop).
//! 4. **telegram queue** (SPEC-P16 §2, K): 500s never reach the caller,
//!    retries are bounded, overflow drops OLDEST (newest always delivered).
//! 5. **P15 regression gate** (SPEC-P15 §2/§4): admin-key 503/401/200 chain,
//!    rate-limit 429 with /healthz exempt, frozen endpoint shapes + CORS.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sentinel::api::{DashboardPaths, DashboardState, dashboard_router};
use sentinel::health::{HealthState, router as health_router};
use sentinel::notify::{Alert, AlertKind, AlertSink, TelegramSink};
use sentinel_core::audit::{AuditJournal, IntentRecord, OutcomeRecord, Trigger, verify_chain};
use sentinel_core::types::ExecutionMode;

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// Repository root; this test lives at `<root>/crates/sentinel/tests/`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn p16_fixtures() -> PathBuf {
    repo_root().join("tests/fixtures/p16")
}

// ---------------------------------------------------------------------------
// secret scanner
// ---------------------------------------------------------------------------

/// One non-placeholder secret candidate loaded from `.env`.
#[derive(Debug)]
struct SecretValue {
    key: String,
    value: String,
}

/// Marker written to the generated sample when a value must appear only in
/// redacted form.
const REDACTED_MARKER: &str = "[REDACTED]";

/// Public, well-documented Foundry/anvil dev keys. SPEC-P16 §3 allows the
/// known public anvil dev key; these carry zero secrecy by construction and
/// appear verbatim in runbooks (`--private-key 0xac09…`).
const ANVIL_DEV_KEYS: &[&str] = &[
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80", // anvil account #0
    "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d", // anvil account #1
];

/// Keys whose *name* suggests secret material. Tuning knobs that merely
/// contain a marker word (e.g. `QWEN_MAX_TOKENS`) are excluded.
fn name_is_secretish(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const MARKERS: &[&str] = &[
        "KEY", "SECRET", "TOKEN", "PAYER", "MNEMONIC", "PRIVATE", "PASSWORD", "WEBHOOK", "SIGNER",
    ];
    const EXCLUDED: &[&str] = &[
        "MAX_TOKENS",
        "PER_HOUR",
        "PER_DAY",
        "TTL",
        "INTERVAL",
        "ALLOWED_USER",
        "WINDOW",
        "TIMEOUT",
        "SECS",
    ];
    MARKERS.iter().any(|m| upper.contains(m)) && !EXCLUDED.iter().any(|m| upper.contains(m))
}

/// SPEC-P16 §3: placeholders (`replace-me` prefixes, `1234567890…`, all-zero
/// hex) are exempt from the verbatim scan.
fn is_placeholder_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("replace-me")
        || lower.contains("replace_me")
        || lower.contains("changeme")
        || lower.contains("your-")
    {
        return true;
    }
    if value.starts_with("1234567890") {
        return true;
    }
    if lower.starts_with("0x") && value[2..].chars().all(|c| c == '0') {
        return true;
    }
    false
}

/// Acceptable, non-leaking renderings of a secret: redaction vocabulary,
/// explicit omission markers, `***`, `...`, `<redacted>` shapes, all-zero hex.
fn is_redacted_form(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.contains("redact")
        || lower.contains("replace-me")
        || lower.contains("replace_me")
        || lower.contains("changeme")
        || lower.contains("your-")
    {
        return true;
    }
    if lower.contains("***")
        || lower.contains("...")
        || lower.contains("(omitted)")
        || lower.contains("hidden")
        || lower.contains("masked")
    {
        return true;
    }
    if lower.starts_with("0x") && lower[2..].chars().all(|c| c == '0') {
        return true;
    }
    lower.contains('<') && lower.contains('>')
}

/// Load the non-placeholder secret candidates from `.env`. Missing `.env`
/// yields an empty set (the pattern scans still run).
fn load_env_secret_values(env_path: &Path) -> Vec<SecretValue> {
    let Ok(text) = fs::read_to_string(env_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !name_is_secretish(key) || is_placeholder_value(value) || value.len() < 10 {
            continue;
        }
        out.push(SecretValue {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    out
}

/// Context words that turn a 64-hex run into key material rather than a hash.
const KEY_CONTEXT: &[&str] = &[
    "private key",
    "private_key",
    "private-key",
    "privkey",
    "signer_key",
    "signer key",
    "payer_key",
    "payer key",
    "mnemonic",
    "secret",
    "api_key",
    "api key",
];

/// Maximal ASCII-hex runs (>= 40 chars) on a line with a `0x`-prefix flag.
fn hex_runs(line: &str) -> Vec<(String, bool)> {
    let bytes = line.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start >= 40 {
                let prefixed = start >= 2 && &bytes[start - 2..start] == b"0x";
                runs.push((line[start..i].to_string(), prefixed));
            }
        } else {
            i += 1;
        }
    }
    runs
}

/// `Bearer <token>` tokens on one line (token char class excludes quotes,
/// backticks, shell punctuation and brackets).
fn bearer_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(pos) = rest.find("Bearer") {
        let before_ok = rest[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after = &rest[pos + "Bearer".len()..];
        let token: String = after
            .trim_start_matches(' ')
            .chars()
            .take_while(|c| !c.is_whitespace() && !"\"'`,;()[]{}<>".contains(*c))
            .collect();
        if before_ok && !token.is_empty() {
            out.push(token);
        }
        rest = &rest[pos + "Bearer".len()..];
    }
    out
}

/// `sk-` style tokens on one line. The boundary check keeps ordinary words
/// (`task-list`, `risk-based`) out of the result set.
fn sk_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < line.len() {
        let Some(pos) = line[offset..].find("sk-") else {
            break;
        };
        let abs = offset + pos;
        let before_ok = line[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let cont: String = line[abs + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let token = format!("sk-{cont}");
        if before_ok && token.len() >= 12 && !cont.is_empty() {
            out.push(token);
        }
        offset = abs + 3;
    }
    out
}

/// Redact a finding snippet so evidence output never re-leaks material.
fn redact_snippet(s: &str) -> String {
    let head: String = s.chars().take(4).collect();
    format!("{head}...({} chars)", s.chars().count())
}

/// Scan one text against the secret policy. Returns human-readable violations;
/// snippets are always redacted.
fn scan_secrets(label: &str, text: &str, secrets: &[SecretValue]) -> Vec<String> {
    let mut violations = Vec::new();

    // (1) verbatim non-placeholder secret values.
    for secret in secrets {
        if text.contains(&secret.value) {
            violations.push(format!(
                "{label}: verbatim non-placeholder value of {} appears in the text",
                secret.key
            ));
        }
    }

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;

        // (2) Bearer tokens.
        for token in bearer_tokens(line) {
            if token.len() >= 12 && !is_redacted_form(&token) {
                violations.push(format!(
                    "{label}:L{line_no}: unredacted Bearer token {}",
                    redact_snippet(&token)
                ));
            }
        }

        // (3) sk- style keys.
        for token in sk_tokens(line) {
            if !is_redacted_form(&token) {
                violations.push(format!(
                    "{label}:L{line_no}: unredacted sk- style key {}",
                    redact_snippet(&token)
                ));
            }
        }

        // (4) 64-hex key material in a key context (hash digests/tx ids are
        // not key material and must not be flagged).
        let lower = line.to_ascii_lowercase();
        let key_context = KEY_CONTEXT.iter().any(|ctx| lower.contains(ctx));
        if key_context {
            for (run, _prefixed) in hex_runs(line) {
                if run.len() != 64 {
                    continue;
                }
                let normalized = run.to_ascii_lowercase();
                let all_zero = normalized.chars().all(|c| c == '0');
                if all_zero || ANVIL_DEV_KEYS.contains(&normalized.as_str()) {
                    continue;
                }
                violations.push(format!(
                    "{label}:L{line_no}: 64-hex private key in key context {}",
                    redact_snippet(&run)
                ));
            }
        }
    }

    violations
}

fn scan_path(label: &str, path: &Path, secrets: &[SecretValue], violations: &mut Vec<String>) {
    match fs::read_to_string(path) {
        Ok(text) => violations.extend(scan_secrets(label, &text, secrets)),
        Err(err) => violations.push(format!("{label}: unreadable: {err}")),
    }
}

// ---------------------------------------------------------------------------
// (1) secret scan — SPEC-P16 §3
// ---------------------------------------------------------------------------

#[test]
fn secret_scan_evidence_logs_and_fixture_are_clean() {
    let root = repo_root();
    let secrets = load_env_secret_values(&root.join(".env"));
    let mut violations = Vec::new();
    let mut scanned = 0usize;

    for dir_rel in ["docs/evidence", "data", "logs"] {
        let dir = root.join(dir_rel);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("txt") | Some("log")
                    )
            })
            .collect();
        files.sort();
        for file in files {
            scanned += 1;
            let label = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            scan_path(&label, &file, &secrets, &mut violations);
        }
    }

    let fixture = p16_fixtures().join("redaction-sample.log");
    scanned += 1;
    scan_path(
        "tests/fixtures/p16/redaction-sample.log",
        &fixture,
        &secrets,
        &mut violations,
    );

    assert!(
        violations.is_empty(),
        "secret scan violations (snippets redacted): {violations:#?}"
    );
    assert!(
        scanned >= 30,
        "expected the evidence corpus to be scanned; only {scanned} files seen"
    );
    eprintln!(
        "secret scan: {scanned} files clean; {} non-placeholder secret values enforced",
        secrets.len()
    );
}

#[test]
fn secret_scan_generated_sample_log_is_clean_and_dirty_twin_is_caught() {
    let root = repo_root();
    let live = load_env_secret_values(&root.join(".env"));

    // (a) generated sample: every live non-placeholder value appears ONLY as a
    // redaction marker (plus the fixture-style marker vocabulary).
    let mut sample = String::new();
    for secret in &live {
        sample.push_str(&format!(
            "2026-10-06T03:10:00Z  INFO sentinel::config: {}={REDACTED_MARKER}\n",
            secret.key
        ));
    }
    let long_marked_bearer = format!("Bearer {}", REDACTED_MARKER);
    sample.push_str(&format!(
        "2026-10-06T03:10:01Z  INFO sentinel::http: Authorization: {long_marked_bearer}\n"
    ));
    sample.push_str("2026-10-06T03:10:02Z  INFO sentinel::brain: api_key=sk-REDACTED-000\n");
    sample.push_str("2026-10-06T03:10:03Z  INFO sentinel::test: --private-key 0x<redacted>\n");

    let temp = tempfile::tempdir().expect("tempdir");
    let sample_path = temp.path().join("generated-sample.log");
    fs::write(&sample_path, &sample).expect("write generated sample");
    let read_back = fs::read_to_string(&sample_path).expect("read generated sample");
    let clean_violations = scan_secrets("generated-sample", &read_back, &live);
    assert!(
        clean_violations.is_empty(),
        "redacted sample must scan clean: {clean_violations:#?}"
    );

    // (b) dirty twin: embedding ANY of those values raw must be caught, so the
    // clean result above is meaningful (non-vacuous). Synthetics keep the
    // check alive even when `.env` holds only placeholders (current state).
    let mut synthetics = vec![
        SecretValue {
            key: "FAKE_RPC_SIGNER_KEY".to_string(),
            value: format!("0x{}", "a1b2c3d4".repeat(8)),
        },
        SecretValue {
            key: "FAKE_API_TOKEN".to_string(),
            value: "tok_live_9f2c0e11aa77b3".to_string(),
        },
    ];
    let mut all = Vec::new();
    all.extend(live.iter().map(|s| SecretValue {
        key: s.key.clone(),
        value: s.value.clone(),
    }));
    all.append(&mut synthetics);
    for secret in &all {
        let dirty = format!("INFO sentinel::x: {}={}\n", secret.key, secret.value);
        let caught = scan_secrets("dirty", &dirty, &all);
        assert!(
            !caught.is_empty(),
            "raw value of {} must be caught by the scanner",
            secret.key
        );
    }
    assert!(
        all.len() >= 2,
        "negative control must never be vacuous (got {} values)",
        all.len()
    );
}

#[test]
fn secret_scanner_negative_control_flags_each_category_and_spares_hashes() {
    let key64 = format!("0x{}", "deadbeefcafebabe".repeat(4));
    let dirty = format!(
        "2026-10-06T00:00:00Z  INFO x: RPC_SIGNER_KEY={key64}\n\
         2026-10-06T00:00:00Z  INFO x: Authorization: Bearer eyJhbGciOiJIUzI1NiJ9payloadsig\n\
         2026-10-06T00:00:00Z  INFO x: api_key=sk-live-4f2b9c1e8a7d6c5b\n"
    );
    let violations = scan_secrets("dirty", &dirty, &[]);
    let joined = violations.join("\n");
    assert!(
        joined.contains("64-hex private key"),
        "key-context hex must be flagged: {joined}"
    );
    assert!(
        joined.contains("Bearer token"),
        "bearer token must be flagged: {joined}"
    );
    assert!(
        joined.contains("sk- style key"),
        "sk- token must be flagged: {joined}"
    );

    // Hash digests and transaction ids are NOT key material.
    let hashes_only = "2026-10-06T00:00:00Z DEBUG x: prev_hash=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
                       2026-10-06T00:00:00Z DEBUG x: tx=0xe67e2d0d5c1f4a9b3e77aa0211d4c8f30b5a9e6d2c4f1a8b7d9e0f3a6c5b4d2e\n";
    assert!(
        scan_secrets("hashes", hashes_only, &[]).is_empty(),
        "plain hash/tx digests must not be flagged"
    );

    // Ordinary words containing `sk-` must not be mistaken for keys.
    let words = "note: task-list rebuilt; risk-based sizing applied\n";
    assert!(
        scan_secrets("words", words, &[]).is_empty(),
        "ordinary `sk-` inside words must not be flagged"
    );
}

#[test]
fn anvil_dev_key_exemption_only_covers_the_public_dev_key() {
    // The documented public anvil dev key is permitted in `--private-key`
    // position.
    let exempt = format!(
        "forge create --private-key 0x{} --broadcast\n",
        ANVIL_DEV_KEYS[0]
    );
    assert!(
        scan_secrets("anvil", &exempt, &[]).is_empty(),
        "public anvil dev key is exempt"
    );

    // Any *other* 64-hex key in the same context is flagged.
    let other = format!(
        "forge create --private-key 0x{} --broadcast\n",
        "ab".repeat(32)
    );
    let violations = scan_secrets("other", &other, &[]);
    assert_eq!(
        violations.len(),
        1,
        "non-public key material must be flagged: {violations:#?}"
    );

    // The all-zero placeholder is tolerated.
    let zero = format!("--private-key 0x{}\n", "0".repeat(64));
    assert!(scan_secrets("zero", &zero, &[]).is_empty());
}

// ---------------------------------------------------------------------------
// (2) audit degrade — SPEC-P16 §2 (L): persistence failures (disk full /
//     read-only dir) degrade to in-memory, retry on the next append, and the
//     in-flight entry is never lost.
// ---------------------------------------------------------------------------

fn degrade_ts(offset_secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_767_000_000 + offset_secs, 0).expect("valid timestamp")
}

fn degrade_intent(marker: u64) -> IntentRecord {
    IntentRecord {
        trigger: Trigger::Reflex,
        account: "0x0000000000000000000000000000000000000007".to_string(),
        market_id: Some(32),
        input_hash: format!("{marker:064x}"),
        decision: serde_json::json!({"action": "REDUCE", "market": 32, "note": "p16-degrade"}),
        policy_verdict: serde_json::json!({"verdict": "allow"}),
    }
}

fn degrade_outcome(marker: u64) -> OutcomeRecord {
    OutcomeRecord {
        trigger: Trigger::Reflex,
        account: "0x0000000000000000000000000000000000000007".to_string(),
        market_id: Some(32),
        input_hash: format!("{marker:064x}"),
        decision: serde_json::json!({"action": "REDUCE", "market": 32, "note": "p16-degrade"}),
        policy_verdict: serde_json::json!({"verdict": "allow"}),
        execution: serde_json::json!({"status": "simulated"}),
    }
}

fn journal_file(audit_dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = fs::read_dir(audit_dir)
        .expect("audit dir readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files.pop().expect("journal file exists")
}

fn set_dir_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn audit_degrade_readonly_dir_keeps_inflight_entry_and_persists_on_next_append() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let audit_dir = workspace.path().join("audit");
    fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut journal = AuditJournal::open(&audit_dir).expect("open journal");
    let seq_before = journal.seq();

    // Read-only directory: persistence must fail, the record must survive in
    // memory instead (SPEC-P16 §2).
    set_dir_mode(&audit_dir, 0o555);
    let record_a = journal.record_intent(&degrade_intent(0xA), degrade_ts(10));
    let degraded_after_a = journal.degraded();
    set_dir_mode(&audit_dir, 0o755); // restore early so failures still clean up

    assert!(
        record_a.is_ok(),
        "record during degrade must succeed in memory: {record_a:?}"
    );
    assert!(
        degraded_after_a,
        "degraded() must be true after the failed persist (read-only dir)"
    );

    // The very next append after recovery must flush the in-flight entry too.
    let record_b = journal.record_outcome(&degrade_outcome(0xA), degrade_ts(11));
    assert!(record_b.is_ok(), "recovery append: {record_b:?}");

    let entries = journal.read_entries(0, 16).expect("read journal");
    let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(
        seqs,
        (seq_before..seq_before + 2).collect::<Vec<_>>(),
        "in-memory entries keep order (A then B): {seqs:?}"
    );

    let file = journal_file(&audit_dir);
    let report = verify_chain(&file).expect("verify chain");
    assert!(
        report.broken_at.is_none(),
        "persisted chain intact: {report:?}"
    );
    assert_eq!(
        report.entries, 2,
        "BOTH entries (in-flight A + recovery B) must be on disk: {report:?}"
    );

    let recovered_flag = journal.degraded();
    eprintln!(
        "degrade(ro-dir): recovered_flag={recovered_flag} on_disk={}",
        report.entries
    );
    assert!(
        !recovered_flag,
        "degraded() must clear after a successful recovery append"
    );
}

struct MountGuard {
    path: PathBuf,
    mounted: bool,
}

impl MountGuard {
    fn tmpfs_64k(path: &Path) -> Option<Self> {
        let status = Command::new("sudo")
            .args(["-n", "mount", "-t", "tmpfs", "-o", "size=64k", "tmpfs"])
            .arg(path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            mounted: true,
        })
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if !self.mounted {
            return;
        }
        let _ = Command::new("sudo")
            .args(["-n", "umount"])
            .arg(&self.path)
            .status();
        if let Ok(mounts) = fs::read_to_string("/proc/mounts")
            && mounts.contains(&format!(" {}", self.path.display()))
        {
            // Never leak a busy mount: force-detach as the last resort.
            let _ = Command::new("sudo")
                .args(["-n", "umount", "-l"])
                .arg(&self.path)
                .status();
        }
    }
}

#[test]
fn audit_degrade_tmpfs_disk_full_persists_both_entries_after_recovery() {
    // Precondition: passwordless sudo for the SPEC-P16 §2 64 KiB tmpfs
    // disk-full simulation.
    let sudo_ok = Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|status| status.success());
    assert!(
        sudo_ok,
        "sudo -n (NOPASSWD) is required for the tmpfs disk-full probe"
    );

    let workspace = tempfile::tempdir().expect("tempdir");
    let mnt = workspace.path().join("tmpfs");
    fs::create_dir_all(&mnt).expect("create mountpoint");
    // Guard is held for its Drop (umount); never referenced otherwise.
    let _guard = MountGuard::tmpfs_64k(&mnt).expect("mount 64k tmpfs");

    let mut journal = AuditJournal::open(&mnt).expect("open journal on tmpfs");

    // Fill the tmpfs to zero free bytes (smaller and smaller writes until
    // even a 1-byte write fails with ENOSPC).
    let filler = mnt.join("filler.bin");
    {
        use std::io::Write;
        let mut file = fs::File::create(&filler).expect("create filler");
        'fit: for chunk in [4096usize, 1024, 256, 64, 16, 4, 1] {
            let buf = vec![0xAAu8; chunk];
            loop {
                match file.write(&buf) {
                    Ok(0) => break 'fit,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = file.sync_all();
    }

    // A: the disk-full record must degrade to memory instead of failing.
    let record_a = journal.record_intent(&degrade_intent(0xD), degrade_ts(20));
    let degraded_after_a = journal.degraded();
    assert!(
        record_a.is_ok(),
        "record with ENOSPC must succeed in memory: {record_a:?}"
    );
    assert!(degraded_after_a, "degraded() must be true after ENOSPC");

    // Free space; the next append must flush the in-flight entry too.
    fs::remove_file(&filler).expect("remove filler");
    let record_b = journal.record_outcome(&degrade_outcome(0xD), degrade_ts(21));
    assert!(record_b.is_ok(), "recovery append on tmpfs: {record_b:?}");

    let entries = journal.read_entries(0, 16).expect("read journal");
    assert_eq!(
        entries.len(),
        2,
        "A (in-flight) + B (recovery): {entries:#?}"
    );

    let file = journal_file(&mnt);
    let report = verify_chain(&file).expect("verify chain on tmpfs");
    assert!(report.broken_at.is_none(), "tmpfs chain intact: {report:?}");
    assert_eq!(
        report.entries, 2,
        "BOTH entries must be on disk after recovery: {report:?}"
    );
    let recovered_flag = journal.degraded();
    std::mem::drop(journal); // close before unmounting
    eprintln!(
        "degrade(tmpfs-64k): degraded_after_a={degraded_after_a} recovered_flag={recovered_flag} on_disk={}",
        report.entries
    );
    assert!(
        !recovered_flag,
        "degraded() must clear after a successful recovery append (tmpfs)"
    );
}

// ---------------------------------------------------------------------------
// (3) supervisor — SPEC-P16 §2 (K): panics are caught and restarted with
//     backoff; shutdown stops restarts (no zombie loop).
// ---------------------------------------------------------------------------

async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return cond();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn supervisor_restarts_a_task_that_panics_once_and_the_daemon_survives() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let runs_clone = Arc::clone(&runs);
    let handle = sentinel::supervisor::spawn("p16-panic-once", shutdown_rx, move || {
        let runs = Arc::clone(&runs_clone);
        async move {
            let attempt = runs.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                panic!("p16: first attempt panics");
            }
            // Second attempt succeeds and parks until the supervisor stops it.
            std::future::pending::<()>().await;
        }
    });

    assert!(
        wait_until(|| runs.load(Ordering::SeqCst) >= 2, Duration::from_secs(8)).await,
        "the panicking task must be restarted (runs={})",
        runs.load(Ordering::SeqCst)
    );
    assert!(
        !handle.is_finished(),
        "the supervisor loop must survive the restarted task"
    );
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn supervisor_shutdown_stops_restarts_no_zombie_loop() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let runs_clone = Arc::clone(&runs);
    let handle = sentinel::supervisor::spawn("p16-panic-always", shutdown_rx, move || {
        let runs = Arc::clone(&runs_clone);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            panic!("p16: always panics");
        }
    });

    assert!(
        wait_until(|| runs.load(Ordering::SeqCst) >= 2, Duration::from_secs(8)).await,
        "restart #2 must be observed before shutdown (runs={})",
        runs.load(Ordering::SeqCst)
    );

    shutdown_tx.send(true).expect("shutdown signal");
    tokio::time::sleep(Duration::from_millis(1500)).await; // settle in-flight cycle
    let frozen = runs.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let later = runs.load(Ordering::SeqCst);
    assert_eq!(
        later, frozen,
        "no restarts after shutdown (zombie loop detected): {frozen} -> {later}"
    );

    // The supervisor loop itself must stop, not spin forever.
    let stopped = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(stopped.is_ok(), "supervisor task must stop after shutdown");
}

// ---------------------------------------------------------------------------
// (4) telegram queue — SPEC-P16 §2 (K): failures never propagate; bounded
//     retry; overflow drops the OLDEST entry (never the newest).
// ---------------------------------------------------------------------------

fn p16_alert(text: String) -> Alert {
    Alert {
        kind: AlertKind::FeedStale { secs: 7 },
        market_id: None,
        text,
        at_ms: 1,
    }
}

fn p16_sink(server: &wiremock::MockServer) -> TelegramSink {
    let url = url::Url::parse(&server.uri()).expect("mock uri");
    let bot = teloxide::Bot::new("123456789:P16TESTTOKEN").set_api_url(url);
    TelegramSink::with_bot(bot, 424242)
}

fn send_message_requests(requests: &[wiremock::Request]) -> Vec<&wiremock::Request> {
    // teloxide's payload NAME is the struct name (`SendMessage`); method_url
    // pushes it verbatim, so the path segment case is NOT guaranteed. Match
    // case-insensitively.
    requests
        .iter()
        .filter(|request| {
            request
                .url
                .path()
                .to_ascii_lowercase()
                .contains("sendmessage")
        })
        .collect()
}

#[tokio::test]
async fn telegram_sink_absorbs_500s_and_bounded_retries_are_observed() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/bot[^/]+/[sS]endMessage$",
        ))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let sink = p16_sink(&server);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        sink.send(&p16_alert("p16-retry-probe".to_string())),
    )
    .await
    .expect("send() must return promptly (non-blocking enqueue)");
    assert!(
        result.is_ok(),
        "TelegramSink failure must never propagate: {result:?}"
    );

    // teloxide sleeps 10 s (DELAY_ON_SERVER_ERROR) before surfacing each 5xx
    // response, so the sink only sees the first failure at ~10 s; the window
    // below is sized to observe the follow-up retry attempt(s) too.
    tokio::time::sleep(Duration::from_secs(26)).await;
    let requests = server.received_requests().await.expect("request log");
    eprintln!("telegram retry probe: raw requests={}", requests.len());
    for request in requests.iter().take(5) {
        eprintln!("  {} {}", request.method, request.url.path());
    }
    let attempts = send_message_requests(&requests).len();
    eprintln!("telegram retry probe: attempts={attempts} (SPEC: retry 3x, 500ms base)");
    assert!(
        attempts >= 2,
        "at least one retry must be observed, got {attempts}"
    );
    assert!(attempts <= 6, "retries must be bounded, got {attempts}");
}

#[tokio::test]
async fn telegram_queue_overflow_drops_oldest_not_newest() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/bot[^/]+/[sS]endMessage$",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(250))
                .set_body_json(serde_json::json!({
                    "ok": true,
                    "result": {
                        "message_id": 1,
                        "date": 1_700_000_000,
                        "chat": { "id": 424242, "type": "private" }
                    }
                })),
        )
        .mount(&server)
        .await;
    let sink = p16_sink(&server);

    // 40 alerts > queue capacity 32 (SPEC-P16 §2), worker deliberately slow.
    // The burst must complete promptly: overflow drops, it never blocks.
    tokio::time::timeout(Duration::from_secs(20), async {
        for i in 0..40u32 {
            let result = sink.send(&p16_alert(format!("p16-idx-{i:02}"))).await;
            assert!(result.is_ok(), "send #{i} must not fail: {result:?}");
        }
    })
    .await
    .expect("send() burst must not block on a full queue");

    // Drain: wait until the received-request count is stable for ~1.5 s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last = 0usize;
    let mut stable_since = tokio::time::Instant::now();
    while tokio::time::Instant::now() < deadline {
        let count = server
            .received_requests()
            .await
            .map_or(0, |requests| send_message_requests(&requests).len());
        if count != last {
            last = count;
            stable_since = tokio::time::Instant::now();
        } else if last > 0 && stable_since.elapsed() >= Duration::from_millis(1500) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let requests = server.received_requests().await.expect("request log");
    eprintln!("queue overflow probe: raw requests={}", requests.len());
    for request in requests.iter().take(5) {
        eprintln!("  {} {}", request.method, request.url.path());
    }
    let mut delivered: BTreeSet<u32> = BTreeSet::new();
    for request in send_message_requests(&requests) {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
        if let Some(text) = body["text"].as_str()
            && let Some(rest) = text.strip_prefix("p16-idx-")
        {
            delivered.insert(rest.parse::<u32>().expect("index"));
        }
    }
    let missing: Vec<u32> = (0..40).filter(|i| !delivered.contains(i)).collect();
    eprintln!(
        "queue overflow probe: delivered={} missing={missing:?}",
        delivered.len()
    );
    assert!(
        delivered.contains(&39),
        "the NEWEST entry must be kept (drop-oldest, not drop-newest): {missing:?}"
    );
    assert!(
        !missing.is_empty(),
        "40 sends must exceed the 32-slot queue (delivered={})",
        delivered.len()
    );
    assert!(
        missing.iter().all(|&i| i <= 20),
        "only OLD entries may be dropped: {missing:?}"
    );
    assert!(
        delivered.len() >= 30,
        "the queue must retain roughly its capacity: {}",
        delivered.len()
    );
}

// ---------------------------------------------------------------------------
// (5) P15 regression gate — rate limit (429), admin key (503/401/200) and the
//     frozen endpoint shapes (SPEC-P15 §2/§4; "M re-verifies 429/401/503").
// ---------------------------------------------------------------------------

struct DashboardHarness {
    base: String,
    kill: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
    _tmp: tempfile::TempDir,
}

async fn start_dashboard() -> DashboardHarness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let paths = DashboardPaths {
        audit_dir: root.join("audit"),
        backtest_report: root.join("backtest-report.json"),
        breaker_journal: root.join("breaker-journal.jsonl"),
        breaker_state: root.join("breaker-state.json"),
        heartbeat: root.join("heartbeat.json"),
        spend_ledger: root.join("nansen-spend.jsonl"),
    };
    let health = Arc::new(HealthState::new(ExecutionMode::DryRun));
    let kill = Arc::new(AtomicBool::new(false));
    let state = DashboardState {
        health: Arc::clone(&health),
        journal: None,
        kill: Arc::clone(&kill),
        live_state: None,
        paths,
    };
    let app = health_router(Arc::clone(&health)).merge(dashboard_router(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    DashboardHarness {
        base: format!("http://{addr}"),
        kill,
        server,
        _tmp: tmp,
    }
}

#[tokio::test]
async fn p15_kill_switch_admin_key_503_401_200_and_resume() {
    let harness = start_dashboard().await;
    let client = reqwest::Client::new();

    // Unset key: mutations must be disabled with 503 (frozen contract).
    unsafe { std::env::remove_var("DASHBOARD_ADMIN_KEY") };
    let response = client
        .post(format!("{}/api/pause", harness.base))
        .send()
        .await
        .expect("POST pause");
    assert_eq!(
        response.status(),
        503,
        "unset admin key must disable mutations"
    );
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["error"], "admin key not configured");

    // Wrong key: 401, kill flag untouched.
    unsafe { std::env::set_var("DASHBOARD_ADMIN_KEY", "p16-admin-key-verify") };
    let response = client
        .post(format!("{}/api/pause?key=nope", harness.base))
        .send()
        .await
        .expect("POST pause wrong key");
    assert_eq!(response.status(), 401, "wrong key must be rejected");
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["error"], "unauthorized");
    assert!(
        !harness.kill.load(Ordering::SeqCst),
        "failed auth must not touch the kill flag"
    );

    // Right key: 200, kill flag engaged and reflected in /api/state.
    let response = client
        .post(format!(
            "{}/api/pause?key=p16-admin-key-verify",
            harness.base
        ))
        .send()
        .await
        .expect("POST pause");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["paused"], true);
    assert!(harness.kill.load(Ordering::SeqCst), "kill flag engaged");
    let state: serde_json::Value = client
        .get(format!("{}/api/state", harness.base))
        .send()
        .await
        .expect("GET state")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["paused"], true, "/api/state reflects the kill switch");

    // Resume flips it back.
    let response = client
        .post(format!(
            "{}/api/resume?key=p16-admin-key-verify",
            harness.base
        ))
        .send()
        .await
        .expect("POST resume");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["paused"], false);
    assert!(!harness.kill.load(Ordering::SeqCst));

    unsafe { std::env::remove_var("DASHBOARD_ADMIN_KEY") };
    harness.server.abort();
}

#[tokio::test]
async fn p15_rate_limit_returns_429_and_spares_healthz() {
    let harness = start_dashboard().await;
    let client = reqwest::Client::new();

    let mut ok = 0usize;
    let mut limited = 0usize;
    let mut body_ok = false;
    for _ in 0..210 {
        let response = client
            .get(format!("{}/api/decisions", harness.base))
            .send()
            .await
            .expect("GET decisions");
        match response.status().as_u16() {
            200 => ok += 1,
            429 => {
                limited += 1;
                let body: serde_json::Value = response.json().await.expect("429 json");
                if body["error"] == "rate limited" {
                    body_ok = true;
                }
            }
            other => panic!("unexpected status {other} on /api/decisions"),
        }
    }
    eprintln!("rate limit probe: ok={ok} limited={limited}");
    assert!(
        (100..=160).contains(&ok),
        "burst window shape off (SPEC: 60/min, burst 120): ok={ok} limited={limited}"
    );
    assert!(
        limited >= 30,
        "429s must appear past the burst: ok={ok} limited={limited}"
    );
    assert!(body_ok, "429 body must be {{\"error\":\"rate limited\"}}");

    // /healthz is exempt; it must never be throttled.
    for _ in 0..40 {
        let response = client
            .get(format!("{}/healthz", harness.base))
            .send()
            .await
            .expect("GET healthz");
        assert_eq!(
            response.status(),
            200,
            "/healthz stays exempt from the limiter"
        );
    }
    harness.server.abort();
}

#[tokio::test]
async fn p15_frozen_endpoint_shapes_and_cors() {
    let harness = start_dashboard().await;
    let client = reqwest::Client::new();

    // GET /api/state — frozen top-level shape (SPEC-P15 §2).
    let response = client
        .get(format!("{}/api/state", harness.base))
        .send()
        .await
        .expect("GET state");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "CORS header on /api/*"
    );
    let state: serde_json::Value = response.json().await.expect("state json");
    let object = state.as_object().expect("state is an object");
    for key in [
        "mode",
        "version",
        "uptime_s",
        "feed_age_s",
        "feed_fresh",
        "paused",
        "heartbeat",
        "account",
        "positions",
    ] {
        assert!(object.contains_key(key), "state missing {key}: {state}");
    }
    assert!(object["mode"].is_string(), "mode is a string");
    assert!(object["version"].is_string(), "version is a string");
    assert!(object["uptime_s"].is_u64(), "uptime_s is u64");
    assert!(object["feed_fresh"].is_boolean(), "feed_fresh is bool");
    assert!(object["paused"].is_boolean(), "paused is bool");
    let heartbeat = object["heartbeat"].as_object().expect("heartbeat object");
    for key in ["age_s", "tx_hash", "seq"] {
        assert!(heartbeat.contains_key(key), "heartbeat missing {key}");
    }
    assert!(object["positions"].is_array(), "positions is an array");

    // GET /api/decisions — array.
    let decisions: serde_json::Value = client
        .get(format!("{}/api/decisions", harness.base))
        .send()
        .await
        .expect("GET decisions")
        .json()
        .await
        .expect("decisions json");
    assert!(
        decisions.is_array(),
        "decisions must be an array: {decisions}"
    );

    // GET /api/backtest — frozen 404 when no report exists.
    let response = client
        .get(format!("{}/api/backtest", harness.base))
        .send()
        .await
        .expect("GET backtest");
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.expect("backtest json");
    assert_eq!(body["error"], "backtest report not found");

    // GET /api/nansen/spend — zeros when the ledger is absent.
    let spend: serde_json::Value = client
        .get(format!("{}/api/nansen/spend", harness.base))
        .send()
        .await
        .expect("GET spend")
        .json()
        .await
        .expect("spend json");
    assert_eq!(spend["total_calls"], 0, "spend zeros: {spend}");
    assert_eq!(spend["calls_1h"], 0, "spend zeros: {spend}");
    assert!(spend["recent"].is_array(), "spend recent is an array");

    // GET /api/breaker-status — unavailable when no state files exist.
    let breaker: serde_json::Value = client
        .get(format!("{}/api/breaker-status", harness.base))
        .send()
        .await
        .expect("GET breaker-status")
        .json()
        .await
        .expect("breaker json");
    assert_eq!(
        breaker["available"], false,
        "breaker unavailable: {breaker}"
    );
    assert!(breaker["journal_tail"].is_array());

    // GET / — the dashboard HTML within the 200 KiB budget.
    let response = client
        .get(format!("{}/", harness.base))
        .send()
        .await
        .expect("GET /");
    assert_eq!(response.status(), 200);
    let html = response.text().await.expect("root html");
    assert!(
        html.contains("<html") || html.contains("<!DOCTYPE"),
        "root serves the dashboard HTML"
    );
    assert!(
        html.len() <= 204_800,
        "dashboard within the 200 KiB budget: {} bytes",
        html.len()
    );
    harness.server.abort();
}
