//! MarkdownV2 escaping + message builders (`SPEC-P11.md` §4).
//!
//! [`escape_md2`] escapes every Telegram MarkdownV2 reserved character with
//! a backslash, backslash included. Builders construct cards from plain
//! input strings: every **data** segment goes through [`escape_md2`], while
//! static scaffolding is kept free of reserved characters (bold headers via
//! `*…*` are the only deliberate MarkdownV2 entities; the audit footer uses
//! explicit scaffolding escapes).

use sentinel_core::audit::AuditEntry;
use sentinel_core::types::RiskTier;

/// Every MarkdownV2 reserved character (`SPEC-P11.md` §4).
const RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Escape every Telegram MarkdownV2 reserved character with a backslash.
///
/// Single pass over the input: each reserved character (including `\`
/// itself) is prefixed with `\`; characters inserted by the escaping are
/// never re-escaped, so already-escaped input re-escapes safely.
pub fn escape_md2(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        if RESERVED.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Rows for the status card (one per position).
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRow {
    /// Symbol.
    pub symbol: String,
    /// Market id.
    pub market_id: u32,
    /// Signed size (base units).
    pub size: String,
    /// Entry price.
    pub entry: String,
    /// Mark price (or `n/a`).
    pub mark: String,
    /// Distance to liquidation, rendered %.
    pub distance_pct: String,
    /// Tier emoji.
    pub tier_emoji: String,
    /// Collateral.
    pub collateral: String,
    /// Unrealized PnL.
    pub upnl: String,
}

/// `/status` card.
pub fn status_card(
    rows: &[PositionRow],
    free_balance: &str,
    feed_age_s: Option<u64>,
    mode: &str,
) -> String {
    let feed = match feed_age_s {
        Some(secs) => format!("{secs}s ago"),
        None => "no data".to_string(),
    };
    let mut lines = vec![
        "*Sentinel status*".to_string(),
        format!("Mode: {}", escape_md2(mode)),
        format!("Free balance: {}", escape_md2(free_balance)),
        format!("Last update: {}", escape_md2(&feed)),
    ];
    if rows.is_empty() {
        lines.push("No open positions".to_string());
    } else {
        lines.push("*Positions*".to_string());
        for row in rows {
            lines.push(format!(
                "{} {} {}: size {}, entry {}, mark {}, distance {}, collateral {}, uPnL {}",
                escape_md2(&row.tier_emoji),
                row.market_id,
                escape_md2(&row.symbol),
                escape_md2(&row.size),
                escape_md2(&row.entry),
                escape_md2(&row.mark),
                escape_md2(&row.distance_pct),
                escape_md2(&row.collateral),
                escape_md2(&row.upnl),
            ));
        }
    }
    lines.join("\n")
}

/// `/risk` consult card (amount is absent for alerts).
#[allow(clippy::too_many_arguments)] // fields are explicit by card design
pub fn risk_card(
    market_id: u32,
    action: &str,
    amount: Option<&str>,
    confidence: &str,
    urgency: &str,
    reason: &str,
    provider: &str,
    verdict: &str,
) -> String {
    let mut lines = vec![
        format!("*Risk consult* — market {market_id}"),
        format!("Action: {}", escape_md2(action)),
    ];
    if let Some(amount) = amount {
        lines.push(format!("Amount: {}", escape_md2(amount)));
    }
    lines.push(format!(
        "Confidence: {} · Urgency: {}",
        escape_md2(confidence),
        escape_md2(urgency)
    ));
    lines.push(format!("Reason: {}", escape_md2(reason)));
    lines.push(format!("Provider: {}", escape_md2(provider)));
    lines.push(format!("Verdict: {}", escape_md2(verdict)));
    lines.join("\n")
}

/// `/policy` overlay card (one pre-rendered `key: value` line each).
pub fn policy_card(lines: &[String]) -> String {
    let mut out = vec!["*Policy overlay*".to_string()];
    if lines.is_empty() {
        out.push("No overlay entries".to_string());
    } else {
        out.extend(lines.iter().map(|line| escape_md2(line)));
    }
    out.join("\n")
}

/// `/audit` card: `seq: trigger — action — status` per entry, plus the
/// latest on-chain anchor reference.
pub fn audit_list(entries: &[AuditEntry], last_anchor_tx: Option<&str>) -> String {
    let mut lines = vec!["*Audit journal*".to_string()];
    if entries.is_empty() {
        lines.push("No entries yet".to_string());
    } else {
        for entry in entries {
            let trigger = trigger_label(&entry.trigger);
            let action = entry
                .decision
                .get("action")
                .and_then(|value| value.as_str())
                .unwrap_or("n/a");
            let status = entry
                .execution
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("n/a");
            lines.push(format!(
                "{}: {} — {} — {}",
                entry.seq,
                escape_md2(&trigger),
                escape_md2(action),
                escape_md2(status),
            ));
        }
    }
    match last_anchor_tx {
        Some(tx) => lines.push(format!("Anchor: {}", escape_md2(tx))),
        None => lines.push("Anchor: none yet".to_string()),
    }
    lines.push("Cross\\-check: audit\\-verify".to_string());
    lines.join("\n")
}

/// Journal trigger rendered in its journal (SCREAMING_SNAKE_CASE) form.
fn trigger_label(trigger: &sentinel_core::audit::Trigger) -> String {
    serde_json::to_value(trigger)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{trigger:?}"))
}

/// `/spend` card: x402 ledger totals + latest purchases.
pub fn spend_card(calls_hour: u32, cost_day: &str, limit_hour: u32, recent: &[String]) -> String {
    let mut lines = vec![
        "*Spend — Nansen x402*".to_string(),
        format!("Calls this hour: {calls_hour} / {limit_hour}"),
        format!("Cost today: {}", escape_md2(cost_day)),
    ];
    if recent.is_empty() {
        lines.push("No recent calls".to_string());
    } else {
        lines.push("*Recent calls*".to_string());
        lines.extend(recent.iter().map(|entry| escape_md2(entry)));
    }
    lines.join("\n")
}

/// Approval request card (Approve/Deny buttons are attached by the caller).
pub fn approval_card(
    id: &str,
    summary: &str,
    notional: &str,
    reason: &str,
    confidence: &str,
) -> String {
    [
        "*Approval required*".to_string(),
        format!("ID: {}", escape_md2(id)),
        format!("Summary: {}", escape_md2(summary)),
        format!("Notional: {}", escape_md2(notional)),
        format!("Reason: {}", escape_md2(reason)),
        format!("Confidence: {}", escape_md2(confidence)),
    ]
    .join("\n")
}

/// `/close` confirmation card (the Execute/Cancel buttons are attached by
/// the caller).
pub fn closeconfirm_card(market_id: u32, symbol: &str, size: &str, notional: &str) -> String {
    [
        "*Confirm close*".to_string(),
        format!("Market: {market_id} — {}", escape_md2(symbol)),
        format!("Size: {}", escape_md2(size)),
        format!("Notional: {}", escape_md2(notional)),
    ]
    .join("\n")
}

/// Tier emoji for the reflex tiers (`SPEC-P11.md` §3 status card).
pub fn tier_emoji(tier: &RiskTier) -> &'static str {
    match tier {
        RiskTier::Green => "🟢",
        RiskTier::Yellow => "🟡",
        RiskTier::Orange => "🟠",
        RiskTier::Red => "🔴",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_core::audit::Trigger;
    use serde_json::json;

    /// Assert every given data segment was escaped into `out`; for segments
    /// that carry a non-`*` reserved character, also assert no raw copy
    /// leaked (bare `*` may legitimately appear in `*bold*` scaffolding).
    fn assert_data_escaped(out: &str, segments: &[&str]) {
        for &segment in segments {
            let escaped = escape_md2(segment);
            assert!(
                out.contains(&escaped),
                "escaped segment missing: {segment:?} -> {escaped:?}\n---\n{out}"
            );
            let has_non_star_reserved = segment.chars().any(|c| c != '*' && RESERVED.contains(&c));
            if has_non_star_reserved {
                assert!(
                    !out.contains(segment),
                    "raw (unescaped) segment leaked: {segment:?}\n---\n{out}"
                );
            }
        }
    }

    /// Scan an output for reserved characters that are neither escaped with
    /// a backslash nor part of the allowed `*bold*` scaffolding.
    fn assert_no_bare_reserved(out: &str) {
        let mut chars = out.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next();
                continue;
            }
            assert!(
                !RESERVED.contains(&c) || c == '*',
                "unescaped reserved char {c:?} left in builder output:\n{out}"
            );
        }
    }

    fn sample_position() -> PositionRow {
        PositionRow {
            symbol: "SOL_USD*".to_string(),
            market_id: 32,
            size: "-2.5".to_string(),
            entry: "150.25".to_string(),
            mark: "151.10".to_string(),
            distance_pct: "18.3%".to_string(),
            tier_emoji: "🟢".to_string(),
            collateral: "1_000.50".to_string(),
            upnl: "+4.20".to_string(),
        }
    }

    fn sample_entry(seq: u64, trigger: Trigger, action: &str, status: &str) -> AuditEntry {
        AuditEntry::new(
            seq,
            chrono::Utc::now(),
            trigger,
            "0xAccount_1".to_string(),
            Some(32),
            "deadbeef".to_string(),
            json!({ "action": action, "tier": "Red" }),
            json!({ "verdict": "Allow" }),
            json!({ "status": status }),
            sentinel_core::audit::GENESIS_PREV_HASH.to_string(),
        )
    }

    #[test]
    fn escapes_every_reserved_character_individually() {
        for &ch in RESERVED {
            assert_eq!(
                escape_md2(&ch.to_string()),
                format!("\\{ch}"),
                "char {ch:?}"
            );
        }
        assert_eq!(RESERVED.len(), 19);
    }

    #[test]
    fn escapes_a_combined_string_in_one_pass() {
        let input = "a_b*c[d]e(f)g~h`i>j#k+l-m=n|o{p}q.r!s\\t";
        let expected = r"a\_b\*c\[d\]e\(f\)g\~h\`i\>j\#k\+l\-m\=n\|o\{p\}q\.r\!s\\t";
        assert_eq!(escape_md2(input), expected);
    }

    #[test]
    fn leaves_non_reserved_characters_alone() {
        assert_eq!(escape_md2("🟢🟡🟠🔴"), "🟢🟡🟠🔴");
        assert_eq!(escape_md2("héllo wörld ÁÉÍ"), "héllo wörld ÁÉÍ");
        assert_eq!(escape_md2("plain 123 :,;%/"), "plain 123 :,;%/");
        assert_eq!(escape_md2(""), "");
    }

    #[test]
    fn re_escaping_already_escaped_input_is_safe() {
        // A literal backslash is itself reserved: escaping an already-escaped
        // string is correct and never leaves a bare `\` before a reserved char.
        assert_eq!(escape_md2(r"\_"), r"\\\_");
        assert_eq!(escape_md2(r"\."), r"\\\.");
        let once = escape_md2("_");
        assert_eq!(once, r"\_");
        assert_eq!(escape_md2(&once), r"\\\_");
    }

    #[test]
    fn status_card_renders_and_escapes_every_data_segment() {
        let rows = vec![
            sample_position(),
            PositionRow {
                symbol: "BTC[perp]".to_string(),
                market_id: 1,
                size: "0.10".to_string(),
                entry: "60000".to_string(),
                mark: "n/a".to_string(),
                distance_pct: "9.9%".to_string(),
                tier_emoji: "🔴".to_string(),
                collateral: "500".to_string(),
                upnl: "-12.0".to_string(),
            },
        ];
        let out = status_card(&rows, "2_500.00", Some(12), "DRY_RUN");

        assert!(out.contains("*Sentinel status*"));
        assert!(out.contains("*Positions*"));
        assert!(out.contains("🟢"));
        assert!(out.contains("🔴"));
        assert!(out.contains(r"18\.3%"));
        assert!(out.contains("12s ago"));
        assert_data_escaped(
            &out,
            &[
                "SOL_USD*",
                "-2.5",
                "150.25",
                "151.10",
                "18.3%",
                "1_000.50",
                "+4.20",
                "BTC[perp]",
                "-12.0",
                "2_500.00",
                "DRY_RUN",
            ],
        );
        assert_no_bare_reserved(&out);

        let empty = status_card(&[], "0", None, "DRY_RUN");
        assert!(empty.contains("No open positions"));
        assert!(empty.contains("no data"));
        assert_data_escaped(&empty, &["no data", "0", "DRY_RUN"]);
        assert_no_bare_reserved(&empty);
    }

    #[test]
    fn risk_card_renders_amount_honestly_and_escapes() {
        let with_amount = risk_card(
            32,
            "reduce",
            Some("1_234.56"),
            "0.82",
            "high",
            "distance[red]",
            "qwen3.8-max",
            "allow",
        );
        assert!(with_amount.contains("*Risk consult* — market 32"));
        assert!(with_amount.contains("Action: reduce"));
        assert!(with_amount.contains("Amount: 1\\_234\\.56"));
        assert_data_escaped(
            &with_amount,
            &[
                "reduce",
                "1_234.56",
                "0.82",
                "high",
                "distance[red]",
                "qwen3.8-max",
                "allow",
            ],
        );
        assert_no_bare_reserved(&with_amount);

        let without_amount = risk_card(1, "close!", None, "low", "low", "ok", "kimi-k3", "deny");
        assert!(!without_amount.contains("Amount:"));
        assert!(without_amount.contains("Action: close\\!"));
        assert_data_escaped(&without_amount, &["close!", "kimi-k3", "deny"]);
        assert_no_bare_reserved(&without_amount);
    }

    #[test]
    fn policy_card_escapes_lines_and_handles_empty() {
        let lines = vec![
            "risk_hard_pct: 9".to_string(),
            "kill_switch: false (overridden!)".to_string(),
        ];
        let out = policy_card(&lines);
        assert!(out.contains("*Policy overlay*"));
        assert_data_escaped(
            &out,
            &["risk_hard_pct: 9", "kill_switch: false (overridden!)"],
        );
        assert_no_bare_reserved(&out);

        let empty = policy_card(&[]);
        assert!(empty.contains("No overlay entries"));
        assert_no_bare_reserved(&empty);
    }

    #[test]
    fn audit_list_renders_seq_trigger_action_status_and_anchor() {
        let entries = vec![
            sample_entry(7, Trigger::Reflex, "reduce_50%", "simulated"),
            sample_entry(8, Trigger::Human, "close[all]", "human_approved!"),
        ];
        let out = audit_list(&entries, Some("0xanchor_tx"));

        assert!(out.contains(r"7: REFLEX — reduce\_50% — simulated"));
        assert!(out.contains(r"8: HUMAN — close\[all\] — human\_approved\!"));
        assert!(out.contains(r"Anchor: 0xanchor\_tx"));
        assert!(out.contains(r"audit\-verify"));
        assert_data_escaped(
            &out,
            &[
                "REFLEX",
                "reduce_50%",
                "simulated",
                "HUMAN",
                "close[all]",
                "human_approved!",
                "0xanchor_tx",
            ],
        );
        assert_no_bare_reserved(&out);

        let no_anchor = audit_list(&[], None);
        assert!(no_anchor.contains("No entries yet"));
        assert!(no_anchor.contains("Anchor: none yet"));
        assert_no_bare_reserved(&no_anchor);
    }

    #[test]
    fn audit_list_falls_back_when_decision_fields_are_missing() {
        let entry = AuditEntry::new(
            1,
            chrono::Utc::now(),
            Trigger::System,
            "0xAcct".to_string(),
            None,
            "cafe".to_string(),
            json!({}),
            json!({}),
            json!({}),
            sentinel_core::audit::GENESIS_PREV_HASH.to_string(),
        );
        let out = audit_list(&[entry], None);
        assert!(out.contains(r"1: SYSTEM — n/a — n/a"));
        assert_no_bare_reserved(&out);
    }

    #[test]
    fn spend_card_renders_totals_and_recent_escaped() {
        let recent = vec!["smart_money 0.01 USDC (12:00)".to_string()];
        let out = spend_card(3, "0.03", 40, &recent);
        assert!(out.contains("*Spend — Nansen x402*"));
        assert!(out.contains("Calls this hour: 3 / 40"));
        assert_data_escaped(&out, &["0.03", "smart_money 0.01 USDC (12:00)"]);
        assert_no_bare_reserved(&out);

        let empty = spend_card(0, "0.00", 40, &[]);
        assert!(empty.contains("No recent calls"));
        assert_no_bare_reserved(&empty);
    }

    #[test]
    fn approval_card_escapes_all_fields() {
        let out = approval_card(
            "ap-1234abcd",
            "reduce 50% on SOL_USD (crash)",
            "1_500.00",
            "distance to liq < hard!",
            "0.77",
        );
        assert!(out.contains("*Approval required*"));
        assert!(out.contains("ID: ap\\-1234abcd"));
        assert_data_escaped(
            &out,
            &[
                "ap-1234abcd",
                "reduce 50% on SOL_USD (crash)",
                "1_500.00",
                "distance to liq < hard!",
                "0.77",
            ],
        );
        assert_no_bare_reserved(&out);
    }

    #[test]
    fn closeconfirm_card_escapes_all_fields() {
        let out = closeconfirm_card(32, "SOL/USD*", "-2.5", "1_234.56");
        assert!(out.contains("*Confirm close*"));
        assert!(out.contains("Market: 32 — SOL/USD\\*"));
        assert_data_escaped(&out, &["SOL/USD*", "-2.5", "1_234.56"]);
        assert_no_bare_reserved(&out);
    }

    #[test]
    fn tier_emoji_matches_each_tier() {
        assert_eq!(tier_emoji(&RiskTier::Green), "🟢");
        assert_eq!(tier_emoji(&RiskTier::Yellow), "🟡");
        assert_eq!(tier_emoji(&RiskTier::Orange), "🟠");
        assert_eq!(tier_emoji(&RiskTier::Red), "🔴");
    }
}
