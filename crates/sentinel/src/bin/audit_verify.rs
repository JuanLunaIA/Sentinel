//! `audit-verify` — local chain verify + on-chain anchor cross-check.
//!
//! Usage (`SPEC-P10.md` §6):
//!   `cargo run --bin audit-verify -- [--journal <path>] [--from-block N] [--no-chain]`
//!
//! Prints the Trust-beat line on success (demo evidence):
//!
//! ```text
//! ✅ journal consistent; {N} entries; {M} anchored; root matches at seq {X}
//! ```
//!
//! `{N}` = journal entries verified locally, `{M}` = anchored entries on
//! chain (highest anchored seq — the contract keeps seqs contiguous from 1
//! per signer), `{X}` = highest on-chain seq whose batch root was recomputed
//! intact (`—` with `--no-chain` or when nothing anchored). On failure the
//! line is `❌ broken at seq {S}: {detail}`, followed by the explorer
//! links of the anchor transactions. Exit code 0 on success, 1 otherwise.
//!
//! # Cross-check rule (implemented, see `cross_check`)
//!
//! On-chain `seq = journal seq + 1` (the contract's replay guard is 1-based:
//! the first anchor is seq 1). Events are sorted by seq and each
//! `DecisionAnchored` must match the journal entry at `seq - 1` — for a
//! `batchAnchor` the event carries the batch's LAST entry hash (`toSeq`), for
//! a single `anchor` that entry's own hash. Successive anchors are contiguous
//! per signer (contract invariant), so every event after the first defines
//! the batch range `(prev_to_seq, to_seq]` whose Merkle root is recomputed
//! from the local hashes and must equal `runningRoot`; the highest seq that
//! matched is `{X}`. With `--from-block > 0` the first event's range start is
//! unknowable (earlier events were filtered out such that a missing
//! predecessor is possible), so only its entry hash is checked and it cannot
//! set `{X}`.

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use alloy::providers::{Provider as _, RootProvider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;

use sentinel::anchor::{abi::SentinelAuditAnchor, merkle_root};
use sentinel::api::{DEFAULT_AUDIT_DIR, latest_journal_file};
use sentinel_core::audit::{AuditEntry, verify_chain};

/// Monad testnet explorer base for anchor tx links.
const EXPLORER_TX_BASE: &str = "https://testnet.monadexplorer.com/tx/";

/// Fallback RPC when `PERPL_RPC_URL` is unset and config cannot provide one.
const DEFAULT_RPC_URL: &str = "https://testnet-rpc.monad.xyz";

/// On-chain seq = journal seq + 1 (contract replay guard is 1-based).
const SEQ_OFFSET: u64 = 1;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("audit-verify: {message}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match verify_cli(&cli).await {
        Ok(outcome) => {
            for line in &outcome.lines {
                println!("{line}");
            }
            if outcome.success {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("audit-verify: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Parsed command-line options (`SPEC-P10` §6; hand-rolled, no new deps).
#[derive(Debug, Clone, Default, PartialEq)]
struct Cli {
    /// Journal file to verify (default: latest `journal-*.jsonl` under
    /// `data/audit/`).
    journal: Option<PathBuf>,
    /// First block scanned for `DecisionAnchored` logs.
    from_block: u64,
    /// Local-only verification (skip the on-chain cross-check).
    no_chain: bool,
}

impl Cli {
    /// Parse arguments (the iterator must NOT include `argv[0]`).
    ///
    /// # Errors
    /// Human-readable message on unknown flags or missing/invalid values.
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut cli = Cli::default();
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--journal" => {
                    let value = iter.next().ok_or("--journal requires a path")?;
                    cli.journal = Some(PathBuf::from(value));
                }
                "--from-block" => {
                    let value = iter.next().ok_or("--from-block requires a number")?;
                    cli.from_block = value
                        .trim()
                        .parse()
                        .map_err(|err| format!("--from-block: {err}"))?;
                }
                "--no-chain" => cli.no_chain = true,
                other => {
                    return Err(format!(
                        "unknown argument {other:?} (supported: --journal, --from-block, --no-chain)"
                    ));
                }
            }
        }
        Ok(cli)
    }
}

/// Everything the CLI prints plus the exit-code decision (stdout-free so the
/// contract can be asserted in tests).
#[derive(Debug, Clone, PartialEq)]
struct CliOutcome {
    /// Lines to print, in order.
    lines: Vec<String>,
    /// `true` ⇒ exit 0, `false` ⇒ exit 1.
    success: bool,
}

/// Run the full verification for `cli`.
///
/// # Errors
/// I/O failures (missing journal file, unreadable file) and RPC/chain-check
/// setup failures (no contract configured, invalid address, unreachable
/// node). A broken chain or a cross-check mismatch is **not** an error: it
/// returns `success = false` with the ❌ line.
async fn verify_cli(cli: &Cli) -> anyhow::Result<CliOutcome> {
    let path = match &cli.journal {
        Some(path) => path.clone(),
        None => latest_journal_file(Path::new(DEFAULT_AUDIT_DIR)).with_context(|| {
            format!("no journal-*.jsonl found under {DEFAULT_AUDIT_DIR}; pass --journal <path>")
        })?,
    };
    let report = verify_chain(&path).with_context(|| format!("verify {}", path.display()))?;

    if let Some(seq) = report.broken_at {
        let detail = report
            .detail
            .clone()
            .unwrap_or_else(|| "hash chain mismatch".to_string());
        return Ok(CliOutcome {
            lines: vec![failure_line(seq, &detail)],
            success: false,
        });
    }
    let entries = report.entries;

    if cli.no_chain {
        return Ok(CliOutcome {
            lines: vec![success_line(entries, 0, None)],
            success: true,
        });
    }

    let rpc_url = resolve_rpc_url();
    let check = cross_check(cli, &rpc_url, &path).await?;
    let mut lines = Vec::new();
    let mut success = true;
    match &check.failure {
        Some((seq, detail)) => {
            lines.push(failure_line(*seq, detail));
            success = false;
        }
        None => lines.push(success_line(entries, check.anchored, check.root_matches_at)),
    }
    lines.extend(check.links.iter().map(|link| format!("  {link}")));
    Ok(CliOutcome { lines, success })
}

/// The exact success line (`SPEC-P10` §6); `—` when no root was checked.
fn success_line(entries: usize, anchored: u64, root_matches_at: Option<u64>) -> String {
    let root = root_matches_at.map_or_else(|| "—".to_string(), |seq| seq.to_string());
    format!(
        "✅ journal consistent; {entries} entries; {anchored} anchored; root matches at seq {root}"
    )
}

/// The exact failure line (`SPEC-P10` §6).
fn failure_line(seq: u64, detail: &str) -> String {
    format!("❌ broken at seq {seq}: {detail}")
}

/// RPC for the chain leg: `PERPL_RPC_URL` env, else best-effort `Config`
/// (the CLI may run without the full daemon environment), else the testnet
/// default.
fn resolve_rpc_url() -> String {
    if let Some(url) = std::env::var("PERPL_RPC_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return url;
    }
    match sentinel::config::Config::load() {
        Ok(cfg) => cfg.perpl.rpc_url,
        Err(_) => DEFAULT_RPC_URL.to_string(),
    }
}

/// One decoded `DecisionAnchored` log (hex normalized, no `0x`).
struct AnchorEvent {
    seq: u64,
    entry_hash: String,
    root: String,
    account: String,
    tx: Option<String>,
}

/// On-chain cross-check outcome.
struct CrossCheck {
    /// Anchored entries on chain: the highest anchored seq (the contract
    /// guarantees `1..=lastSeq` contiguity per signer), 0 when no event.
    anchored: u64,
    /// Highest on-chain seq whose batch root was recomputed intact.
    root_matches_at: Option<u64>,
    /// Explorer links, one per event, in seq order.
    links: Vec<String>,
    /// First break: `(journal seq, detail)`.
    failure: Option<(u64, String)>,
}

/// Fetch `DecisionAnchored` logs and cross-check them against `path`.
///
/// # Errors
/// Contract unset/malformed or the RPC leg failing (node unreachable,
/// undecodable log).
async fn cross_check(cli: &Cli, rpc_url: &str, path: &Path) -> anyhow::Result<CrossCheck> {
    let contract_raw = std::env::var("ANCHOR_CONTRACT_ADDRESS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context(
            "ANCHOR_CONTRACT_ADDRESS is not set — the anchor contract is not deployed yet \
             (PENDING-WALLET, STUB-17); pass --no-chain for local-only verification",
        )?;
    let contract: Address = contract_raw
        .parse()
        .context("ANCHOR_CONTRACT_ADDRESS: invalid address")?;
    let url = url::Url::parse(rpc_url).context("RPC URL is invalid")?;
    let provider: RootProvider = RootProvider::new_http(url);
    let filter = Filter::new()
        .address(contract)
        .from_block(cli.from_block)
        .event_signature(SentinelAuditAnchor::DecisionAnchored::SIGNATURE_HASH);
    let logs = provider
        .get_logs(&filter)
        .await
        .context("eth_getLogs failed")?;

    let mut events: Vec<AnchorEvent> = Vec::with_capacity(logs.len());
    for log in &logs {
        let decoded = log
            .log_decode::<SentinelAuditAnchor::DecisionAnchored>()
            .context("could not decode a DecisionAnchored log")?;
        events.push(AnchorEvent {
            seq: decoded.data().seq,
            entry_hash: normalize_hash(&decoded.data().entryHash.to_string()),
            root: normalize_hash(&decoded.data().runningRoot.to_string()),
            account: decoded.data().account.to_string(),
            tx: log.transaction_hash.map(|hash| hash.to_string()),
        });
    }
    events.sort_by_key(|event| event.seq);

    let entries = read_entries(path)?;
    let find = |seq: u64| entries.iter().find(|entry| entry.seq == seq);

    let mut check = CrossCheck {
        anchored: 0,
        root_matches_at: None,
        links: Vec::new(),
        failure: None,
    };
    let mut prev_to: Option<u64> = None;
    let mut signer: Option<String> = None;

    for event in &events {
        if let Some(tx) = &event.tx {
            check.links.push(format!("{EXPLORER_TX_BASE}{tx}"));
        }
        let journal_seq = event.seq.saturating_sub(SEQ_OFFSET);

        // One journal belongs to one anchor signer; a foreign event is a break.
        if let Some(expected) = &signer {
            if *expected != event.account {
                check.failure = Some((
                    journal_seq,
                    format!(
                        "anchor event signer {} differs from {} (the journal belongs to one signer)",
                        event.account, expected
                    ),
                ));
                break;
            }
        } else {
            signer = Some(event.account.clone());
        }

        // The event's entryHash must equal the journal entry at `seq - 1`
        // (last hash of the batch for `batchAnchor`, the entry itself for a
        // single `anchor`).
        let Some(local) = find(journal_seq) else {
            check.failure = Some((
                journal_seq,
                format!(
                    "no journal entry for the anchored seq {} (on-chain)",
                    event.seq
                ),
            ));
            break;
        };
        if normalize_hash(&local.entry_hash) != event.entry_hash {
            check.failure = Some((
                journal_seq,
                format!(
                    "on-chain entryHash at seq {} does not match journal entry {journal_seq}",
                    event.seq
                ),
            ));
            break;
        }

        // Contiguity: this event covers on-chain seqs `(prev_to, seq]`, i.e.
        // journal seqs `[prev_to ..= seq - 1]`; recompute that batch's root.
        let range_start: Option<u64> = match prev_to {
            Some(prev) => Some(prev),
            None if cli.from_block == 0 => Some(0),
            None => None, // filtered history: range start unknowable
        };
        if let Some(start) = range_start {
            if start > journal_seq {
                check.failure = Some((
                    journal_seq,
                    format!("anchor seq {} is out of order", event.seq),
                ));
                break;
            }
            let hashes: Option<Vec<String>> = (start..=journal_seq)
                .map(|seq| find(seq).map(|entry| entry.entry_hash.clone()))
                .collect();
            match hashes {
                Some(hashes) => {
                    if merkle_root(&hashes) != event.root {
                        check.failure = Some((
                            journal_seq,
                            format!(
                                "runningRoot at seq {} does not match the recomputed merkle root",
                                event.seq
                            ),
                        ));
                        break;
                    }
                    check.root_matches_at = Some(event.seq);
                }
                None => {
                    check.failure = Some((
                        journal_seq,
                        format!(
                            "journal entries missing for the batch ending at anchor seq {}",
                            event.seq
                        ),
                    ));
                    break;
                }
            }
        }
        prev_to = Some(event.seq);
    }

    // The contract requires `seq == lastSeq + 1` for every anchor, so the
    // anchored seqs of a signer are exactly `1..=lastSeq`: the highest event
    // seq equals the number of anchored entries.
    check.anchored = events.last().map_or(0, |event| event.seq);
    Ok(check)
}

/// Parse every entry line of `path`, in file order.
///
/// The chain was already verified, so an unparseable line can only be the
/// torn trailing write `verify_chain` tolerates; it is skipped with a warning
/// on stderr (stdout stays machine-readable).
fn read_entries(path: &Path) -> anyhow::Result<Vec<AuditEntry>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut entries = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                eprintln!("audit-verify: warning: skipping unparseable journal line ({err})");
            }
        }
    }
    Ok(entries)
}

/// Lowercase hex without a `0x` prefix (journal hashes have neither).
fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use tempfile::TempDir;

    use sentinel_core::audit::{AuditJournal, IntentRecord, Trigger};

    use super::*;

    const ACCOUNT: &str = "0x0000000000000000000000000000000000000007";

    /// Fabricate a valid journal of `entries` entries in a tempdir; returns
    /// the directory (kept alive) and the file path.
    fn fixture(entries: usize) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = AuditJournal::open(dir.path()).expect("open journal");
        for index in 0..entries {
            let record = IntentRecord {
                trigger: Trigger::Reflex,
                account: ACCOUNT.to_string(),
                market_id: Some(32),
                input_hash: format!("{:064x}", index + 1),
                decision: json!({ "index": index }),
                policy_verdict: json!({ "verdict": "allow" }),
            };
            journal.record_intent(&record, Utc::now()).expect("record");
        }
        let path = latest_journal_file(dir.path()).expect("journal file written");
        (dir, path)
    }

    fn local_only(path: PathBuf) -> Cli {
        Cli {
            journal: Some(path),
            from_block: 0,
            no_chain: true,
        }
    }

    #[tokio::test]
    async fn no_chain_prints_the_exact_success_line() {
        let (_dir, path) = fixture(3);
        let outcome = verify_cli(&local_only(path)).await.expect("verify");

        assert_eq!(
            outcome.lines,
            vec!["✅ journal consistent; 3 entries; 0 anchored; root matches at seq —".to_string()]
        );
        assert!(outcome.success, "exit 0");
    }

    #[tokio::test]
    async fn corrupted_journal_prints_the_failure_line_and_fails() {
        let (_dir, path) = fixture(2);
        let raw = std::fs::read_to_string(&path).expect("read journal");
        let corrupted = raw.replacen("\"allow\"", "\"deny\"", 1);
        assert_ne!(corrupted, raw, "fixture must contain the flipped byte");
        std::fs::write(&path, corrupted).expect("write corrupted journal");

        let outcome = verify_cli(&local_only(path)).await.expect("verify");
        assert!(!outcome.success, "exit 1");
        assert_eq!(outcome.lines.len(), 1);
        assert!(
            outcome.lines[0].starts_with("❌ broken at seq 0: "),
            "line: {}",
            outcome.lines[0]
        );
    }

    #[tokio::test]
    async fn missing_journal_file_is_an_error() {
        let cli = local_only(PathBuf::from("/nonexistent/audit/journal-19700101.jsonl"));
        let err = verify_cli(&cli).await.expect_err("must fail");
        assert!(format!("{err:#}").contains("verify"), "error: {err:#}");
    }

    #[test]
    fn parses_the_documented_flags() {
        let cli = Cli::parse(
            ["--journal", "x.jsonl", "--from-block", "42", "--no-chain"]
                .iter()
                .map(ToString::to_string),
        )
        .expect("parses");
        assert_eq!(cli.journal.as_deref(), Some(Path::new("x.jsonl")));
        assert_eq!(cli.from_block, 42);
        assert!(cli.no_chain);

        assert_eq!(
            Cli::parse(Vec::<String>::new()).expect("empty"),
            Cli::default()
        );
    }

    #[test]
    fn rejects_unknown_flags_and_bad_values() {
        let parse = |args: &[&str]| Cli::parse(args.iter().map(ToString::to_string));
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--journal"]).is_err());
        assert!(parse(&["--from-block"]).is_err());
        assert!(parse(&["--from-block", "later"]).is_err());
    }

    #[test]
    fn success_and_failure_lines_are_exact() {
        assert_eq!(
            success_line(12, 12, Some(12)),
            "✅ journal consistent; 12 entries; 12 anchored; root matches at seq 12"
        );
        assert_eq!(
            success_line(5, 0, None),
            "✅ journal consistent; 5 entries; 0 anchored; root matches at seq —"
        );
        assert_eq!(
            failure_line(7, "prev_hash mismatch"),
            "❌ broken at seq 7: prev_hash mismatch"
        );
    }
}
