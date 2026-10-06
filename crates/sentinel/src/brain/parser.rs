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

/// Longest serde error message kept in [`ParseError::Json`] before truncation.
const JSON_DETAIL_MAX: usize = 200;

/// Bound a third-party error message, cutting on a char boundary and marking
/// the truncation so it is never silent.
fn bounded_json_detail(msg: &str) -> String {
    if msg.len() <= JSON_DETAIL_MAX {
        return msg.to_owned();
    }
    let mut end = JSON_DETAIL_MAX;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &msg[..end])
}

/// Byte offset just past the final `</think…>`-style closing tag, if any.
///
/// A tag is a case-insensitive `</think` prefix at any position followed by
/// the next `>` after it (the tag suffix between prefix and `>` is free-form,
/// e.g. `>`, `ing>`, `ING>`, ` we are done>`). The scan restarts after each
/// complete tag, so the **last** one wins. `None` when no tag completes.
fn last_think_tag_end(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut offset = 0;
    let mut last_end = None;
    while offset + 7 <= bytes.len() {
        // `</think` is pure ASCII, so byte scanning stays on char boundaries.
        let window = &bytes[offset..offset + 7];
        if window[0] == b'<' && window[1] == b'/' && window[2..7].eq_ignore_ascii_case(b"think") {
            match raw[offset + 7..].find('>') {
                Some(gt) => {
                    let end = offset + 7 + gt + 1;
                    last_end = Some(end);
                    offset = end;
                    continue;
                }
                // No `>` remains anywhere: no later tag could close either.
                None => break,
            }
        }
        offset += 1;
    }
    last_end
}

/// Slice after the **last** `</think…>`-style closing tag, or the whole text.
pub fn strip_thinking(raw: &str) -> &str {
    match last_think_tag_end(raw) {
        Some(end) => &raw[end..],
        None => raw,
    }
}

/// First balanced, string-aware `{…}` in `raw`.
pub fn extract_balanced_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let bytes = raw.as_bytes();
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                // Depth is >= 1 here: the scan starts on the first `{`, and a
                // level that returns to zero returns immediately.
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// strip → extract → parse (unknown fields ignored) → validate.
///
/// # Errors
/// [`ParseError`] per stage.
pub fn parse_decision(raw: &str, allowed_markets: &[MarketId]) -> Result<Decision, ParseError> {
    let body = strip_thinking(raw);
    let json = extract_balanced_object(body).ok_or(ParseError::NoObject)?;
    let decision: Decision = serde_json::from_str(json).map_err(|err| ParseError::Json {
        detail: bounded_json_detail(&err.to_string()),
    })?;
    decision
        .validate(allowed_markets)
        .map_err(|detail| ParseError::Validation { detail })?;
    Ok(decision)
}

/// The single repair follow-up message (`SPEC-P07.md` §4).
pub fn repair_nudge(bad_raw: &str) -> String {
    format!(
        "Return ONLY the JSON object matching this schema: {SCHEMA_DOC}. Previous output was: {bad_raw}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sentinel_core::types::{DecisionAction, Urgency};
    use std::str::FromStr;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).expect("test decimal literal")
    }

    fn eth_only() -> [MarketId; 1] {
        [MarketId(32)]
    }

    fn valid_hold() -> &'static str {
        r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"flat exposure, nothing to do"}"#
    }

    // ---- strip_thinking ----------------------------------------------------

    #[test]
    fn strip_thinking_without_tag_returns_whole_text() {
        assert_eq!(strip_thinking("just prose, no tags"), "just prose, no tags");
        assert_eq!(strip_thinking(""), "");
    }

    #[test]
    fn strip_thinking_close_think_tag() {
        assert_eq!(
            strip_thinking("reasoning here</think>{\"k\":1}"),
            "{\"k\":1}"
        );
    }

    #[test]
    fn strip_thinking_close_thinking_tag() {
        assert_eq!(strip_thinking("a</thinking>b"), "b");
    }

    #[test]
    fn strip_thinking_uses_the_last_tag_when_two_appear() {
        assert_eq!(strip_thinking("one</think>two</thinking>three"), "three");
        assert_eq!(
            strip_thinking("<think>draft</think>hmm, more</thinking>tail"),
            "tail"
        );
    }

    #[test]
    fn strip_thinking_is_case_insensitive() {
        assert_eq!(strip_thinking("a</THINK>b"), "b");
        assert_eq!(strip_thinking("a</Thinking>b"), "b");
        assert_eq!(strip_thinking("a</THINKING>b"), "b");
        assert_eq!(strip_thinking("a</tHiNk>b"), "b");
    }

    #[test]
    fn strip_thinking_accepts_arbitrary_suffix_up_to_gt() {
        assert_eq!(strip_thinking("a</think we are done>b"), "b");
        assert_eq!(strip_thinking("a</thinking!>b"), "b");
        assert_eq!(strip_thinking("</think>"), "");
    }

    #[test]
    fn strip_thinking_ignores_unterminated_tag() {
        // No `>` after `</think`: nothing closes, so the text is untouched.
        assert_eq!(
            strip_thinking("text </think no close"),
            "text </think no close"
        );
        assert_eq!(strip_thinking("</think"), "</think");
    }

    #[test]
    fn strip_thinking_handles_unicode_around_tag() {
        assert_eq!(strip_thinking("🙈 héllo</THINKING>啊"), "啊");
    }

    // ---- extract_balanced_object -------------------------------------------

    #[test]
    fn extract_none_on_empty_or_absent_brace() {
        assert_eq!(extract_balanced_object(""), None);
        assert_eq!(extract_balanced_object("no braces at all"), None);
        assert_eq!(extract_balanced_object("I'm sorry, I can't help."), None);
    }

    #[test]
    fn extract_simple_object_in_prose() {
        assert_eq!(
            extract_balanced_object(r#"say {"k":1} done"#),
            Some(r#"{"k":1}"#)
        );
    }

    #[test]
    fn extract_returns_exact_slice_including_braces() {
        assert_eq!(
            extract_balanced_object(r#"xx{"action":"HOLD"}yy"#),
            Some(r#"{"action":"HOLD"}"#)
        );
    }

    #[test]
    fn extract_ignores_braces_inside_strings() {
        assert_eq!(
            extract_balanced_object(r#"{"reason":"a { b } c"}"#),
            Some(r#"{"reason":"a { b } c"}"#)
        );
        assert_eq!(
            extract_balanced_object(r#"{"s":"}{"}"#),
            Some(r#"{"s":"}{"}"#)
        );
    }

    #[test]
    fn extract_nested_objects() {
        assert_eq!(
            extract_balanced_object(r#"p{"a":{"b":{}},"c":2}q"#),
            Some(r#"{"a":{"b":{}},"c":2}"#)
        );
    }

    #[test]
    fn extract_handles_escaped_quotes() {
        // Naive scanners would end the string at the escaped quote and close
        // the object early; the slice must keep the `}` inside the string.
        assert_eq!(
            extract_balanced_object(r#"{"r":"a \" } b"}"#),
            Some(r#"{"r":"a \" } b"}"#)
        );
    }

    #[test]
    fn extract_first_of_two_objects() {
        assert_eq!(extract_balanced_object("{1}{2}"), Some("{1}"));
    }

    #[test]
    fn extract_none_when_unbalanced() {
        assert_eq!(extract_balanced_object(r#"{"a":1"#), None);
        assert_eq!(extract_balanced_object(r#"{"a":{"b":1}"#), None);
        assert_eq!(extract_balanced_object("{"), None);
    }

    // ---- parse_decision: acceptance corpus ---------------------------------

    #[test]
    fn happy_path_exact_field_assertions() {
        let raw = r#"{"action":"REDUCE","market_id":32,"amount":"1.5","confidence":0.8,"urgency":"ELEVATED","reason":"distance to liquidation 6%; trim half"}"#;
        let d = parse_decision(raw, &eth_only()).expect("valid decision");
        assert_eq!(d.action, DecisionAction::Reduce);
        assert_eq!(d.market_id, 32);
        assert_eq!(d.amount, Some(dec("1.5")));
        assert_eq!(d.confidence, dec("0.8"));
        assert_eq!(d.urgency, Urgency::Elevated);
        assert_eq!(d.reason, "distance to liquidation 6%; trim half");
    }

    #[test]
    fn prose_wrapped_json_parses() {
        let raw = r#"Here you go: {"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"quiet market"} hope that helps"#;
        let d = parse_decision(raw, &eth_only()).expect("prose-wrapped");
        assert_eq!(d.action, DecisionAction::Hold);
        assert_eq!(d.reason, "quiet market");
    }

    #[test]
    fn markdown_fenced_json_parses() {
        let raw = format!("```json\n{}\n```", valid_hold());
        let d = parse_decision(&raw, &eth_only()).expect("fenced");
        assert_eq!(d.action, DecisionAction::Hold);
        assert_eq!(d.confidence, dec("0.72"));
    }

    #[test]
    fn thinking_preamble_with_close_think_tag_parses() {
        let raw = format!(
            "<think>market is calm, no action needed</think>\nFinal answer:\n{}",
            valid_hold()
        );
        let d = parse_decision(&raw, &eth_only()).expect("strip think");
        assert_eq!(d.action, DecisionAction::Hold);
    }

    #[test]
    fn thinking_preamble_with_close_thinking_tag_parses() {
        let raw = format!("<thinking>weighing options…</thinking>{}", valid_hold());
        let d = parse_decision(&raw, &eth_only()).expect("strip thinking");
        assert_eq!(d.action, DecisionAction::Hold);
    }

    #[test]
    fn thinking_uses_last_close_tag_before_the_json() {
        let raw = format!(
            "<think>draft</think>wait, still thinking</thinking>{}",
            valid_hold()
        );
        let d = parse_decision(&raw, &eth_only()).expect("last tag wins");
        assert_eq!(d.action, DecisionAction::Hold);
    }

    #[test]
    fn trailing_comma_is_json_error() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"x",}"#;
        assert!(matches!(
            parse_decision(raw, &eth_only()),
            Err(ParseError::Json { .. })
        ));
    }

    #[test]
    fn braces_inside_reason_string_survive() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"a { b } c"}"#;
        let d = parse_decision(raw, &eth_only()).expect("string braces");
        assert_eq!(d.reason, "a { b } c");
    }

    #[test]
    fn nested_objects_in_extra_fields_are_ignored() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.72,"urgency":"ROUTINE","reason":"ok","meta":{"debug":{"steps":[1,2,{"x":true}]}}}"#;
        let d = parse_decision(raw, &eth_only()).expect("nested extras");
        assert_eq!(d.action, DecisionAction::Hold);
    }

    #[test]
    fn escaped_quotes_in_reason_parse() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.65,"urgency":"ROUTINE","reason":"He said \"}\" and left"}"#;
        let d = parse_decision(raw, &eth_only()).expect("escaped quotes");
        assert_eq!(d.reason, r#"He said "}" and left"#);
    }

    #[test]
    fn empty_response_is_no_object() {
        assert_eq!(parse_decision("", &eth_only()), Err(ParseError::NoObject));
        assert_eq!(
            parse_decision("   \n\t ", &eth_only()),
            Err(ParseError::NoObject)
        );
    }

    #[test]
    fn refusal_text_is_no_object() {
        let raw = "I'm sorry, but as an AI assistant I can't produce trading decisions.";
        assert_eq!(parse_decision(raw, &eth_only()), Err(ParseError::NoObject));
    }

    #[test]
    fn truncated_json_is_json_error() {
        // Stream cut mid-generation: the model had already closed a first,
        // schema-incomplete object, so extraction recovers it (`first {…}`
        // balances) and serde rejects it for missing required fields.
        let raw = r#"{"action":"REDUCE"}{"action":"REDUCE","market_id":32,"amount":"1.2","confidence":0.8,"urgency":"ELEV"#;
        assert!(matches!(
            parse_decision(raw, &eth_only()),
            Err(ParseError::Json { .. })
        ));
    }

    #[test]
    fn truncated_unclosed_object_yields_no_object() {
        // A cut that leaves no closed object at all: per the frozen §4
        // contract there is no balanced `{…}` to hand to serde → NoObject.
        let raw = r#"{"action":"REDUCE","market_id":32,"amount":"1.2","confidence":0.8,"urgency":"ELEVATED","reason":"cut mid-sentence"#;
        assert_eq!(parse_decision(raw, &eth_only()), Err(ParseError::NoObject));
        let nested = r#"{"action":"HOLD","extra":{"a":{"b":1}"#;
        assert_eq!(
            parse_decision(nested, &eth_only()),
            Err(ParseError::NoObject)
        );
    }

    #[test]
    fn two_objects_first_wins() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.6,"urgency":"ROUTINE","reason":"first"} {"action":"REDUCE","market_id":32,"amount":"9.9","confidence":0.9,"urgency":"CRITICAL","reason":"second"}"#;
        let d = parse_decision(raw, &eth_only()).expect("first object");
        assert_eq!(d.action, DecisionAction::Hold);
        assert_eq!(d.reason, "first");
        assert_eq!(
            extract_balanced_object(raw),
            Some(
                r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.6,"urgency":"ROUTINE","reason":"first"}"#
            )
        );
    }

    #[test]
    fn amount_as_json_number_is_tolerated() {
        let raw = r#"{"action":"REDUCE","market_id":32,"amount":1.25,"confidence":0.7,"urgency":"ELEVATED","reason":"numeric amount"}"#;
        let d = parse_decision(raw, &eth_only()).expect("number amount");
        assert_eq!(d.amount, Some(dec("1.25")));

        let integer = r#"{"action":"ADD_COLLATERAL","market_id":32,"amount":2,"confidence":0.9,"urgency":"CRITICAL","reason":"whole units"}"#;
        let d = parse_decision(integer, &eth_only()).expect("integer amount");
        assert_eq!(d.amount, Some(dec("2")));
    }

    #[test]
    fn unknown_top_level_fields_are_ignored() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0.5,"urgency":"ROUTINE","reason":"ok","model":"qwen","tokens":123,"trace":"…"}"#;
        let d = parse_decision(raw, &eth_only()).expect("unknown fields");
        assert_eq!(d.action, DecisionAction::Hold);
    }

    #[test]
    fn missing_confidence_is_json_error() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"urgency":"ROUTINE","reason":"no confidence at all"}"#;
        match parse_decision(raw, &eth_only()) {
            Err(ParseError::Json { detail }) => {
                assert!(detail.contains("confidence"), "detail: {detail}");
            }
            other => panic!("expected Json missing-field error, got {other:?}"),
        }
    }

    // ---- parse_decision: validation corpus ---------------------------------

    #[test]
    fn confidence_above_one_is_validation_error() {
        let raw = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":1.5,"urgency":"ROUTINE","reason":"overconfident"}"#;
        assert!(matches!(
            parse_decision(raw, &eth_only()),
            Err(ParseError::Validation { .. })
        ));
    }

    #[test]
    fn market_not_in_allowlist_is_validation_error() {
        let raw = r#"{"action":"HOLD","market_id":99,"amount":null,"confidence":0.5,"urgency":"ROUTINE","reason":"wrong market"}"#;
        assert!(matches!(
            parse_decision(raw, &eth_only()),
            Err(ParseError::Validation { .. })
        ));
    }

    #[test]
    fn reduce_without_amount_is_validation_error() {
        let raw = r#"{"action":"REDUCE","market_id":32,"amount":null,"confidence":0.8,"urgency":"ELEVATED","reason":"reduce but no size"}"#;
        assert!(matches!(
            parse_decision(raw, &eth_only()),
            Err(ParseError::Validation { .. })
        ));
    }

    #[test]
    fn confidence_bounds_are_inclusive() {
        let at_zero = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":0,"urgency":"ROUTINE","reason":"zero"}"#;
        let at_one = r#"{"action":"HOLD","market_id":32,"amount":null,"confidence":1,"urgency":"ROUTINE","reason":"one"}"#;
        assert!(parse_decision(at_zero, &eth_only()).is_ok());
        assert!(parse_decision(at_one, &eth_only()).is_ok());
    }

    #[test]
    fn allowed_market_from_multi_market_allowlist_parses() {
        let raw = r#"{"action":"CLOSE","market_id":20,"amount":null,"confidence":0.9,"urgency":"CRITICAL","reason":"hedge is done"}"#;
        let d = parse_decision(raw, &[MarketId(32), MarketId(20)]).expect("allowed");
        assert_eq!(d.action, DecisionAction::Close);
        assert_eq!(d.market_id, 20);
    }

    // ---- hygiene -----------------------------------------------------------

    #[test]
    fn json_error_detail_is_bounded() {
        let raw = format!(
            "{{\"action\":\"{}\",\"market_id\":32,\"confidence\":0.5,\"urgency\":\"ROUTINE\",\"reason\":\"r\"}}",
            "Z".repeat(4096)
        );
        match parse_decision(&raw, &eth_only()) {
            Err(ParseError::Json { detail }) => {
                assert!(
                    detail.len() <= 206,
                    "detail not bounded: {} bytes",
                    detail.len()
                );
                assert!(
                    detail.ends_with("..."),
                    "truncation marker missing: {detail:?}"
                );
            }
            other => panic!("expected Json unknown-variant error, got {other:?}"),
        }
    }

    #[test]
    fn repair_nudge_is_the_single_exact_message() {
        let bad = "oops, no JSON here";
        let msg = repair_nudge(bad);
        assert_eq!(
            msg,
            format!(
                "Return ONLY the JSON object matching this schema: {SCHEMA_DOC}. Previous output was: {bad}"
            )
        );
        assert!(msg.contains(SCHEMA_DOC));
        assert_eq!(msg.matches("Previous output was:").count(), 1);
    }

    #[test]
    fn repair_nudge_does_not_truncate_long_output() {
        let long = "x".repeat(10_000);
        assert!(repair_nudge(&long).contains(&long));
    }
}
