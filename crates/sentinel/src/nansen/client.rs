//! Typed Nansen endpoint bodies + tolerant response parsers.
//!
//! Frozen by `SPEC-P09.md` §3.2. `smart-money` bodies are FACTS-confirmed
//! (`{"chains":["<chain>"]}`); the other two are STUB-05 best-effort shapes —
//! pinned by tests and adjustable at the first paid call.
//!
//! **P09 status:** implemented (`endpoints` agent); interfaces frozen.

use std::str::FromStr;

use rust_decimal::Decimal;

/// Body for `POST /api/v1/smart-money/netflow`.
pub fn netflow_body(chain: &str) -> serde_json::Value {
    serde_json::json!({ "chains": [chain] })
}

/// Body for `POST /api/v1/smart-money/holdings`.
pub fn holdings_body(chain: &str) -> serde_json::Value {
    serde_json::json!({ "chains": [chain] })
}

/// Body for `POST /api/v1/perp-leaderboard`.
///
/// STUB-05 best-effort shape — confirm at the first keyed/paid call.
pub fn leaderboard_body(market: &str) -> serde_json::Value {
    // STUB-05 best-effort shape: `{"market": <market>}` is unconfirmed against
    // the live schema; pinned by tests, revisit at the first paid call.
    serde_json::json!({ "market": market })
}

/// Body for `POST /api/v1/profiler/perp-positions`.
///
/// STUB-05 best-effort shape — confirm at the first keyed/paid call.
pub fn perp_positions_body(addresses: &[String]) -> serde_json::Value {
    // STUB-05 best-effort shape: `{"addresses": [...]}` is unconfirmed against
    // the live schema; pinned by tests, revisit at the first paid call.
    serde_json::json!({ "addresses": addresses })
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
///
/// Scan order: every candidate at the top level in list order, then every
/// candidate inside the first element of a `data` array (when `data` is an
/// array). A candidate whose value does not parse as a number is skipped, not
/// fatal. Accepted value forms: JSON numbers, numeric strings, and strings
/// with `$` and thousands `,` stripped (surrounding whitespace trimmed).
/// Anything else (missing key, boolean, null, empty/non-numeric string) is
/// ignored; `None` when nothing parses.
pub fn extract_decimal(resp: &serde_json::Value, candidates: &[&str]) -> Option<Decimal> {
    for candidate in candidates {
        if let Some(decimal) = resp.get(candidate).and_then(value_to_decimal) {
            return Some(decimal);
        }
    }
    let nested = resp
        .get("data")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(serde_json::Value::as_object)?;
    for candidate in candidates {
        if let Some(decimal) = nested.get(*candidate).and_then(value_to_decimal) {
            return Some(decimal);
        }
    }
    None
}

/// Convert a JSON value to `Decimal` when it is numeric or a numeric string.
fn value_to_decimal(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::Number(number) => number_to_decimal(number),
        serde_json::Value::String(text) => parse_decimal_text(text),
        _ => None,
    }
}

/// Parse a JSON number exactly (text form first; float fallback for the rare
/// exponent notation).
fn number_to_decimal(number: &serde_json::Number) -> Option<Decimal> {
    let text = number.to_string();
    if let Ok(decimal) = Decimal::from_str(&text) {
        return Some(decimal);
    }
    number.as_f64().and_then(Decimal::from_f64_retain)
}

/// Parse a string value after stripping `$` and `,` and trimming whitespace.
fn parse_decimal_text(text: &str) -> Option<Decimal> {
    let cleaned: String = text.chars().filter(|ch| !matches!(ch, ',' | '$')).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Decimal::from_str(cleaned).ok()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use serde_json::json;

    use super::*;

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    #[test]
    fn netflow_body_is_exact() {
        assert_eq!(netflow_body("ethereum"), json!({ "chains": ["ethereum"] }));
    }

    #[test]
    fn holdings_body_is_exact() {
        assert_eq!(holdings_body("monad"), json!({ "chains": ["monad"] }));
    }

    #[test]
    fn leaderboard_body_is_exact() {
        assert_eq!(
            leaderboard_body("MON-USDC"),
            json!({ "market": "MON-USDC" })
        );
    }

    #[test]
    fn perp_positions_body_is_exact() {
        let addresses = vec!["0xabc".to_string(), "0xdef".to_string()];
        assert_eq!(
            perp_positions_body(&addresses),
            json!({ "addresses": ["0xabc", "0xdef"] })
        );
    }

    #[test]
    fn extract_decimal_top_level_number() {
        let resp = json!({ "net_flow_usd": 1234.5, "unrelated": "x" });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("1234.5")));
    }

    #[test]
    fn extract_decimal_nested_data_string_with_commas() {
        let resp = json!({ "data": [{ "netflow": "1,234.5" }] });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("1234.5")));
    }

    #[test]
    fn extract_decimal_strips_dollar_signs_and_negatives() {
        let resp = json!({ "net_flow": "$-12.3" });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("-12.3")));

        let resp = json!({ "data": [{ "net_flow_usd": "$1,000.00" }] });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("1000.00")));
    }

    #[test]
    fn extract_decimal_missing_is_none() {
        let resp = json!({ "foo": 1, "data": [{ "bar": 2 }] });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), None);

        // `data` present but not an array row with any candidate.
        let resp = json!({ "data": "not-an-array" });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), None);
    }

    #[test]
    fn extract_decimal_non_numeric_string_is_none() {
        let resp = json!({ "net_flow_usd": "abc", "data": [{ "netflow": "n/a" }] });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), None);

        let resp = json!({ "netflow": "" });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), None);
    }

    #[test]
    fn extract_decimal_skips_non_numeric_and_falls_through() {
        // A non-numeric candidate must not block a later valid one.
        let resp = json!({ "net_flow_usd": null, "net_flow": "42.5" });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("42.5")));

        // ... nor a numeric value nested in `data[0]`.
        let resp = json!({ "net_flow_usd": "n/a", "data": [{ "total_net_flow": "7.25" }] });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("7.25")));
    }

    #[test]
    fn extract_decimal_candidate_order_wins() {
        let resp = json!({ "netflow": 5, "net_flow_usd": 9 });
        assert_eq!(extract_decimal(&resp, NETFLOW_KEYS), Some(dec("9")));
    }
}
