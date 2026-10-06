//! Spend ledger — the money-shot audit trail (SPEC-P09 §3.4).
//!
//! One JSON line per settled paid call at `data/nansen-spend.jsonl`; the
//! ledger doubles as the single source of truth for the sliding-hour budget.
//!
//! **Skeleton status (P09):** interfaces frozen; implemented by the P09 wave.

use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::Result;

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
        todo!("P09 agent endpoints")
    }

    /// Append one entry (create parent dirs; single-line JSON).
    ///
    /// # Errors
    /// `SentinelError::Nansen(Challenge)` on I/O failure.
    pub fn append(&self, _entry: &SpendEntry) -> Result<()> {
        todo!("P09 agent endpoints")
    }

    /// Paid calls with `now - ts < window_ms` (sliding window).
    pub fn calls_since(&self, _now_ms: u64, _window_ms: u64) -> u32 {
        todo!("P09 agent endpoints")
    }

    /// Sum of costs with `now - ts < window_ms`.
    pub fn cost_since(&self, _now_ms: u64, _window_ms: u64) -> Decimal {
        todo!("P09 agent endpoints")
    }
}
