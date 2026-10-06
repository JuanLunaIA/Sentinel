//! Extract and validate a [`Decision`] from raw LLM output.
//!
//! Frozen by `SPEC-P07.md` §4: strip the thinking trace, string-aware balanced
//! `{…}` scan, tolerant serde parse, schema validation — plus the single
//! repair nudge text.
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by wave 1.

use sentinel_core::types::{Decision, MarketId};

/// Schema skeleton shared by the system prompt and the repair nudge.
pub const SCHEMA_DOC: &str = "{\"action\":\"HOLD|REDUCE|CLOSE|ADD_COLLATERAL|ESCALATE\",\"market_id\":<u32>,\"amount\":\"<decimal string>|null\",\"confidence\":<0.0-1.0>,\"urgency\":\"ROUTINE|ELEVATED|CRITICAL\",\"reason\":\"<=2 sentences grounded in provided data\"}";

/// Parse-stage failure; the engine maps it onto [`crate::error::BrainError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No balanced `{…}` object exists in the completion.
    NoObject,
    /// The extracted text is not decision JSON.
    Json {
        /// Parser detail.
        detail: String,
    },
    /// JSON parsed but failed schema/business validation.
    Validation {
        /// Validation detail.
        detail: String,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoObject => f.write_str("no JSON object found in completion"),
            ParseError::Json { detail } => write!(f, "invalid decision JSON: {detail}"),
            ParseError::Validation { detail } => write!(f, "decision failed validation: {detail}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Slice after the **last** `</think…>`-style closing tag, or the whole text.
pub fn strip_thinking(raw: &str) -> &str {
    let _ = raw;
    todo!("P07 agent parser")
}

/// First balanced, string-aware `{…}` in `raw`.
pub fn extract_balanced_object(raw: &str) -> Option<&str> {
    let _ = raw;
    todo!("P07 agent parser")
}

/// strip → extract → parse (unknown fields ignored) → validate.
///
/// # Errors
/// [`ParseError`] per stage.
pub fn parse_decision(raw: &str, allowed_markets: &[MarketId]) -> Result<Decision, ParseError> {
    let _ = (raw, allowed_markets);
    todo!("P07 agent parser")
}

/// The single repair follow-up message (`SPEC-P07.md` §4).
pub fn repair_nudge(bad_raw: &str) -> String {
    let _ = bad_raw;
    todo!("P07 agent parser")
}
