//! Spend ledger — the money-shot audit trail (SPEC-P09 §3.4).
//!
//! One JSON line per settled paid call at `data/nansen-spend.jsonl`; the
//! ledger doubles as the single source of truth for the sliding-hour budget.
//!
//! **P09 status:** implemented (`endpoints` agent); interfaces frozen.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{NansenError, Result, SentinelError};

/// One settled purchase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendEntry {
    /// Epoch ms of the settlement.
    pub ts_ms: u64,
    /// Endpoint path (e.g. `/api/v1/smart-money/netflow`).
    pub endpoint: String,
    /// Cost in USD, decimal string.
    pub cost_usd: String,
    /// Settlement transaction hash when reported.
    pub tx_hash: Option<String>,
    /// Payer address.
    pub payer: Option<String>,
    /// CAIP-2 network of the rail used.
    pub network: Option<String>,
}

/// Append-only JSONL ledger.
#[derive(Debug)]
pub struct SpendLedger {
    /// File path.
    pub path: PathBuf,
}

impl SpendLedger {
    /// Ledger at `path` (file may not exist yet).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// All entries (missing file ⇒ empty; malformed lines ⇒ warn + skip).
    pub fn load(&self) -> Vec<SpendEntry> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "spend ledger unreadable; treating as empty"
                );
                return Vec::new();
            }
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str::<SpendEntry>(line) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %err,
                        "malformed spend ledger line skipped"
                    );
                    None
                }
            })
            .collect()
    }

    /// Append one entry (create parent dirs; single-line JSON).
    ///
    /// # Errors
    /// `SentinelError::Nansen(Challenge)` on I/O failure.
    pub fn append(&self, entry: &SpendEntry) -> Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| io_failure("create parent dir for", parent, err))?;
        }
        let line = serde_json::to_string(entry)
            .map_err(|err| NansenError::Challenge(format!("spend ledger serialise: {err}")))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|err| io_failure("open", &self.path, err))?;
        writeln!(file, "{line}").map_err(|err| io_failure("write", &self.path, err))?;
        Ok(())
    }

    /// Paid calls with `now - ts < window_ms` (sliding window).
    pub fn calls_since(&self, now_ms: u64, window_ms: u64) -> u32 {
        let count = self
            .load()
            .into_iter()
            .filter(|entry| in_window(entry.ts_ms, now_ms, window_ms))
            .count();
        count.min(u32::MAX as usize) as u32
    }

    /// Sum of costs with `now - ts < window_ms`.
    pub fn cost_since(&self, now_ms: u64, window_ms: u64) -> Decimal {
        self.load()
            .into_iter()
            .filter(|entry| in_window(entry.ts_ms, now_ms, window_ms))
            .filter_map(|entry| Decimal::from_str(entry.cost_usd.trim()).ok())
            .fold(Decimal::ZERO, |acc, cost| acc + cost)
    }
}

/// Sliding-window predicate: `ts` inside `window_ms` before `now_ms`.
///
/// Boundary is exclusive (`now - ts == window_ms` ⇒ outside) and timestamps in
/// the future are ignored (guards the subtraction from underflow).
fn in_window(ts_ms: u64, now_ms: u64, window_ms: u64) -> bool {
    ts_ms <= now_ms && now_ms - ts_ms < window_ms
}

/// Map an I/O failure to the frozen error surface (see the stub contract).
fn io_failure(op: &str, path: &Path, err: std::io::Error) -> SentinelError {
    NansenError::Challenge(format!("spend ledger {op} {}: {err}", path.display())).into()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use tempfile::TempDir;

    use super::*;

    fn entry(ts_ms: u64, cost_usd: &str) -> SpendEntry {
        SpendEntry {
            ts_ms,
            endpoint: "/api/v1/smart-money/netflow".to_string(),
            cost_usd: cost_usd.to_string(),
            tx_hash: Some(format!("0x{ts_ms:064x}")),
            payer: Some("0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f".to_string()),
            network: Some("eip155:143".to_string()),
        }
    }

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        let ledger = SpendLedger::new(dir.path().join("nope.jsonl"));
        assert_eq!(ledger.load(), Vec::new());

        // An existing but empty file also loads as empty.
        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(SpendLedger::new(&empty).load(), Vec::new());
    }

    #[test]
    fn append_then_load_round_trips_in_order() {
        let dir = TempDir::new().unwrap();
        // Nested path proves parent-dir creation.
        let ledger = SpendLedger::new(dir.path().join("nested/deep/spend.jsonl"));
        let first = entry(1_000, "0.05");
        let second = entry(2_000, "0.01");
        let third = entry(3_000, "0.10");
        ledger.append(&first).unwrap();
        ledger.append(&second).unwrap();
        ledger.append(&third).unwrap();

        assert_eq!(ledger.load(), vec![first, second, third]);

        // One JSON object per line, each terminated by a newline.
        let raw = std::fs::read_to_string(&ledger.path).unwrap();
        assert!(raw.ends_with('\n'), "trailing newline");
        assert_eq!(raw.lines().count(), 3);
    }

    #[test]
    fn calls_since_window_boundaries() {
        let dir = TempDir::new().unwrap();
        let ledger = SpendLedger::new(dir.path().join("ledger.jsonl"));
        for (ts, cost) in [
            (1_000, "0.05"),
            (2_000, "0.01"),
            (3_000, "0.10"),
            (9_000, "0.50"),
        ] {
            ledger.append(&entry(ts, cost)).unwrap();
        }

        // now=3000, window=1000: ts=3000 in; ts=2000 at exactly the window
        // edge ⇒ excluded; ts=1000 falls out; ts=9000 is in the future ⇒
        // ignored (no underflow).
        assert_eq!(ledger.calls_since(3_000, 1_000), 1);
        // ts=1000 at exactly now-window=1000 for now=2000 ⇒ excluded.
        assert_eq!(ledger.calls_since(2_000, 1_000), 1);
        // Everything not future is inside the wide window.
        assert_eq!(ledger.calls_since(9_000, 10_000), 4);
        // The future entry is skipped even when older ones fit.
        assert_eq!(ledger.calls_since(8_000, 10_000), 3);
    }

    #[test]
    fn cost_since_sums_decimals_in_window() {
        let dir = TempDir::new().unwrap();
        let ledger = SpendLedger::new(dir.path().join("ledger.jsonl"));
        for (ts, cost) in [
            (1_000, "0.05"),
            (2_000, "0.01"),
            (3_000, "0.10"),
            (9_000, "999"),
        ] {
            ledger.append(&entry(ts, cost)).unwrap();
        }
        // now=3000, window=2000: ts=1000 at the edge ⇒ excluded; 2000 and
        // 3000 in ⇒ 0.01 + 0.10; the future entry is ignored.
        assert_eq!(ledger.cost_since(3_000, 2_000), dec("0.11"));
        assert_eq!(ledger.cost_since(3_000, 3_000), dec("0.16"));
    }

    #[test]
    fn cost_since_skips_unparseable_costs() {
        let dir = TempDir::new().unwrap();
        let ledger = SpendLedger::new(dir.path().join("ledger.jsonl"));
        ledger.append(&entry(2_000, "junk")).unwrap();
        ledger.append(&entry(2_000, "0.02")).unwrap();
        assert_eq!(ledger.cost_since(2_000, 10_000), dec("0.02"));
    }

    #[test]
    fn malformed_lines_are_skipped_but_valid_ones_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let first = entry(1_000, "0.05");
        let second = entry(2_000, "0.01");
        let body = format!(
            "{}\n{{ not json\n\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        std::fs::write(&path, body).unwrap();
        assert_eq!(SpendLedger::new(&path).load(), vec![first, second]);
    }
}
