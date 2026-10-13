//! `inference-verify` — reference verifier for proof-carrying AI decisions
//! (P23 / GAP-014 layer 1).
//!
//! The audit spine proves *what was decided and when*. This binary closes the
//! last unverifiable link: given an archived inference context and a journal
//! entry, it recomputes the commitment from the raw inputs and asserts that the
//! journaled hash is the one those inputs produce. It converts "trust our logs"
//! into "re-run our math": any party holding the archive and the journal can
//! run this and get the same answer.
//!
//! ```text
//! inference-verify --archive <inference.jsonl> --journal <journal-*.jsonl>
//! ```
//!
//! Archive line shape (written by the daemon next to its journal):
//!
//! ```json
//! {"decision_id":"ap-00000001","commitment":"<64 hex>",
//!  "system_prompt":"...","schema_doc":"...","user_prompt":"...",
//!  "model_id":"qwen3.8-max","provider":"qwen","temperature":"0.2",
//!  "max_tokens":4000,"raw_completion":"{...}"}
//! ```
//!
//! Exit codes: 0 = every archived commitment matches a journal entry,
//! 1 = a mismatch or an unreadable input, 2 = usage error.

use std::collections::BTreeMap;
use std::process::ExitCode;

use sentinel_core::inference::{COMMITMENT_VERSION, InferenceContext, commitment};
use serde::Deserialize;

/// One archived inference context.
#[derive(Debug, Deserialize)]
struct ArchiveLine {
    /// Decision id the commitment was journaled under.
    decision_id: String,
    /// The commitment as the daemon computed it.
    commitment: String,
    /// System prompt sent to the provider.
    system_prompt: String,
    /// Response schema appended to the user prompt.
    schema_doc: String,
    /// User prompt (rendered situation block).
    user_prompt: String,
    /// Model id as sent.
    model_id: String,
    /// Provider that answered.
    provider: String,
    /// Sampling temperature as rendered.
    temperature: String,
    /// Output token budget as rendered.
    max_tokens: u64,
    /// Raw completion text.
    raw_completion: String,
}

fn usage() -> ExitCode {
    eprintln!("usage: inference-verify --archive <inference.jsonl> --journal <journal-*.jsonl>");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut archive: Option<String> = None;
    let mut journal: Option<String> = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--archive" => archive = iter.next(),
            "--journal" => journal = iter.next(),
            _ => return usage(),
        }
    }
    let (Some(archive), Some(journal)) = (archive, journal) else {
        return usage();
    };

    // 1) Journal: decision_id -> journaled commitment.
    let journal_text = match std::fs::read_to_string(&journal) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("inference-verify: read {journal}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut journaled: BTreeMap<String, String> = BTreeMap::new();
    for (n, line) in journal_text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("inference-verify: {journal}:{}: {err}", n + 1);
                return ExitCode::FAILURE;
            }
        };
        let decision = &entry["decision"];
        let (Some(id), Some(commit)) = (
            decision["decision_id"].as_str(),
            decision["inference_commitment"].as_str(),
        ) else {
            continue; // not a strategy decision
        };
        if let Some(previous) = journaled.insert(id.to_string(), commit.to_string()) {
            eprintln!(
                "inference-verify: {id} appears twice in the journal ({previous} then {commit})"
            );
            return ExitCode::FAILURE;
        }
    }

    // 2) Archive: recompute every commitment from the raw inputs.
    let archive_text = match std::fs::read_to_string(&archive) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("inference-verify: read {archive}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut checked = 0usize;
    let mut matched = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (n, line) in archive_text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: ArchiveLine = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(err) => {
                failures.push(format!("{archive}:{}: {err}", n + 1));
                continue;
            }
        };

        let recomputed = commitment(&InferenceContext {
            system_prompt: &record.system_prompt,
            schema_doc: &record.schema_doc,
            user_prompt: &record.user_prompt,
            model_id: &record.model_id,
            provider: &record.provider,
            temperature: &record.temperature,
            max_tokens: record.max_tokens,
            raw_completion: &record.raw_completion,
        });

        checked += 1;

        if recomputed != record.commitment {
            failures.push(format!(
                "{}: archive commitment does not match its own inputs\n  archive:    {}\n  recomputed: {}",
                record.decision_id, record.commitment, recomputed
            ));
            continue;
        }

        match journaled.get(&record.decision_id) {
            Some(journaled_commitment) if journaled_commitment == &recomputed => {
                matched += 1;
            }
            Some(journaled_commitment) => {
                failures.push(format!(
                    "{}: journal commitment differs from the archived inputs\n  journal:   {}\n  recomputed: {}",
                    record.decision_id, journaled_commitment, recomputed
                ));
            }
            None => {
                failures.push(format!(
                    "{}: archived but absent from the journal",
                    record.decision_id
                ));
            }
        }
    }

    // 3) Report.
    println!(
        "inference-verify: commitment format {COMMITMENT_VERSION}; {checked} archived context(s); {matched} verified against the journal"
    );
    if failures.is_empty() {
        println!(
            "RESULT: OK — every archived commitment reproduces from its inputs and matches the journal"
        );
        ExitCode::SUCCESS
    } else {
        for failure in &failures {
            eprintln!("MISMATCH {failure}");
        }
        eprintln!("RESULT: FAIL — {} problem(s)", failures.len());
        ExitCode::FAILURE
    }
}
