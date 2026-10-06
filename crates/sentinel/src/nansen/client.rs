//! Typed Nansen endpoint bodies + tolerant response parsers.
//!
//! Frozen by `SPEC-P09.md` §3.2. `smart-money` bodies are FACTS-confirmed
//! (`{"chains":["<chain>"]}`); the other two are STUB-05 best-effort shapes —
//! pinned by tests and adjustable at the first paid call.
//!
//! **Skeleton status (P09):** interfaces frozen; implemented by the P09 wave.

use rust_decimal::Decimal;

/// Body for `POST /api/v1/smart-money/netflow`.
pub fn netflow_body(_chain: &str) -> serde_json::Value {
    todo!("P09 agent endpoints")
}

/// Body for `POST /api/v1/smart-money/holdings`.
pub fn holdings_body(_chain: &str) -> serde_json::Value {
    todo!("P09 agent endpoints")
}

/// Body for `POST /api/v1/perp-leaderboard` (STUB-05 best-effort).
pub fn leaderboard_body(_market: &str) -> serde_json::Value {
    todo!("P09 agent endpoints")
}

/// Body for `POST /api/v1/profiler/perp-positions` (STUB-05 best-effort).
pub fn perp_positions_body(_addresses: &[String]) -> serde_json::Value {
    todo!("P09 agent endpoints")
}

/// Candidate keys for the netflow value in an unknown response schema.
pub const NETFLOW_KEYS: &[&str] = &[
    "net_flow_usd",
    "netflow",
    "net_flow",
    "total_net_flow",
    "totalNetFlow",
    "value",
];

/// Candidate keys for a holdings delta.
pub const HOLDINGS_KEYS: &[&str] = &[
    "holdings_delta_usd",
    "holdings_delta",
    "delta_usd",
    "total_holdings_usd",
    "value",
];

/// First present numeric candidate (top-level or in `data[0]`), as `Decimal`.
pub fn extract_decimal(_resp: &serde_json::Value, _candidates: &[&str]) -> Option<Decimal> {
    todo!("P09 agent endpoints")
}
