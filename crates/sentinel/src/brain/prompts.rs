//! Prompt construction — versioned, grounded, budget-bounded.
//!
//! Frozen by `SPEC-P07.md` §5. The system text is static; every number the
//! model may cite lives in the user block (grounding rule).
//!
//! **P07 status:** implemented. [`system_prompt`] carries no digits and reads
//! no config; the exact response schema ([`SCHEMA_DOC`]) is appended to the
//! user message by [`user_prompt`] instead of being duplicated here.

use rust_decimal::Decimal;
use sentinel_core::risk::{RiskThresholds, distance_to_liq_pct, tier};
use sentinel_core::types::{AccountState, Market, MarketId, Position, RiskTier};
use serde::{Deserialize, Serialize};

use crate::brain::parser::SCHEMA_DOC;

/// Prompt version logged with every consult and decision.
pub const PROMPT_VERSION: &str = "v3.0";

/// The static system prompt.
///
/// Static by contract (`SPEC-P07.md` §5): no runtime numbers, no config reads.
/// All numeric context is rendered into the user block by [`user_prompt`],
/// which also appends the exact schema ([`SCHEMA_DOC`]) as the final
/// instruction.
pub fn system_prompt() -> &'static str {
    r#"You are the Sentinel risk guardian for isolated-margin perpetual futures.
In isolated margin each position carries its own collateral: free account balance does NOT protect a position — only that position's collateral and its distance to liquidation do.
Severity tiers run Green, Yellow, Orange, Red, safest first; each position's tier and distance to liquidation appear in the user message.
Weigh the focus position against the account snapshot, policy caps, smart-money context, and recent reflex actions given in the user message.
Available actions: HOLD, REDUCE, CLOSE, ADD_COLLATERAL, ESCALATE. ESCALATE means the decision needs a human — choose it when the data is missing or ambiguous, or when acting would breach policy.
Never recommend increasing exposure: reducing, closing, or adding collateral to the isolated position are the only admissible directions.
Ground every claim in the user message: cite only numbers present there, and never invent, round, or extrapolate one.
In the Orange zone, when uncertain between HOLD and REDUCE, prefer REDUCE — capital preservation outranks upside.
Respond with ONLY one JSON object — strict JSON, no markdown, no prose — matching the schema stated at the end of the user message."#
}

/// Policy facts the model must respect (summary of the live gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySummary {
    /// Markets the engine may act on.
    pub market_allowlist: Vec<MarketId>,
    /// Per-action notional ceiling, USD.
    pub max_order_size_usd: Decimal,
    /// Notional above which human approval is required, USD.
    pub require_approval_above_usd: Decimal,
    /// Actions left under the daily cap.
    pub daily_actions_left: u32,
}

/// Smart-money enrichment (filled by the Nansen x402 client in P09).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartMoneyContext {
    /// Asset the context is about.
    pub asset: String,
    /// 24 h net flow, USD (positive = inflow).
    pub netflow_24h: Option<Decimal>,
    /// Holdings delta, USD.
    pub holdings_delta: Option<Decimal>,
    /// Long/short ratio from the leaderboard.
    pub long_short_ratio: Option<Decimal>,
    /// Top-trader net bias text.
    pub top_traders_net_bias: Option<String>,
    /// When the data was fetched (epoch ms).
    pub fetched_at_ms: Option<u64>,
    /// Total x402 cost of the context, USD.
    pub total_cost_usd: Option<Decimal>,
    /// Honesty note (e.g. cross-venue proxy wording).
    pub note: Option<String>,
}

impl SmartMoneyContext {
    /// Placeholder when no smart-money data is available.
    ///
    /// Every value is `None`; the renderer ([`user_prompt`]) turns this into
    /// the literal unavailable line, so the note here is informational only.
    pub fn unavailable(asset: impl Into<String>) -> Self {
        Self {
            asset: asset.into(),
            netflow_24h: None,
            holdings_delta: None,
            long_short_ratio: None,
            top_traders_net_bias: None,
            fetched_at_ms: None,
            total_cost_usd: None,
            note: Some("smart-money data unavailable".to_string()),
        }
    }
}

/// What the deterministic reflex layer recently did (context for judgment).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReflexSummary {
    /// Recent reflex actions, newest last.
    pub actions: Vec<String>,
}

/// Everything the user prompt renders.
pub struct PromptInput<'a> {
    /// Full account snapshot.
    pub account: &'a AccountState,
    /// Market table.
    pub markets: &'a [Market],
    /// The position under consultation.
    pub focus: &'a Position,
    /// Policy facts.
    pub policy: &'a PolicySummary,
    /// Smart-money context (or [`SmartMoneyContext::unavailable`]).
    pub sm: &'a SmartMoneyContext,
    /// Recent reflex actions.
    pub reflex: &'a ReflexSummary,
    /// Event/consult timestamp (epoch ms).
    pub now_ms: u64,
}

/// Render the user prompt (≤ ~6000 chars for a 3-position account).
pub fn user_prompt(input: &PromptInput<'_>) -> String {
    let thresholds = default_thresholds();
    let mut lines: Vec<String> = Vec::with_capacity(32);

    // Header: role line, version, timestamp.
    lines.push("# Sentinel strategy consult — isolated-margin risk guardian".to_string());
    lines.push(format!("- prompt_version: {PROMPT_VERSION}"));
    lines.push(format!("- now_ms: {}", input.now_ms));
    lines.push(String::new());

    // Account snapshot: one row per position, focus marked with `>>`.
    lines.push("## ACCOUNT".to_string());
    lines.push(
        "symbol | #market_id | side | size | entry | mark | collateral | uPnL | dist_pct | tier"
            .to_string(),
    );
    for position in &input.account.positions {
        lines.push(account_row(
            position,
            input.markets,
            input.focus,
            &thresholds,
        ));
    }
    lines.push(format!(
        "- free_balance: {} (isolated: free balance does not protect a position)",
        input.account.free_balance
    ));
    lines.push(format!("- equity: {}", input.account.equity));
    lines.push(String::new());

    // Policy block.
    lines.push("## POLICY".to_string());
    lines.push(format!(
        "- market_allowlist: {}",
        allowlist_text(&input.policy.market_allowlist)
    ));
    lines.push(format!(
        "- max_order_size_usd: {}",
        input.policy.max_order_size_usd
    ));
    lines.push(format!(
        "- require_approval_above_usd: {}",
        input.policy.require_approval_above_usd
    ));
    lines.push(format!(
        "- daily_actions_left: {}",
        input.policy.daily_actions_left
    ));
    lines.push(String::new());

    // Reflex block.
    lines.push("## REFLEX (recent actions, newest last)".to_string());
    if input.reflex.actions.is_empty() {
        lines.push("- none yet".to_string());
    } else {
        for action in &input.reflex.actions {
            lines.push(format!("- {action}"));
        }
    }
    lines.push(String::new());

    // Smart-money block.
    lines.push("## SMART MONEY".to_string());
    lines.extend(smart_money_lines(input.sm));
    lines.push(String::new());

    // Focus position restated with its distance/tier.
    lines.push("## FOCUS".to_string());
    lines.push(focus_line(input.focus, input.markets, &thresholds));
    lines.push(String::new());

    // Final instruction: exact schema + required behaviors.
    lines.push("## OUTPUT".to_string());
    lines.push(format!(
        "Respond with ONLY one JSON object matching: {SCHEMA_DOC}"
    ));
    lines.push(
        "- reason: at most two sentences, grounded in this message; cite only numbers that appear above."
            .to_string(),
    );
    lines.push(
        "- action: exactly one of the schema's actions; use ESCALATE when this decision needs a human."
            .to_string(),
    );

    lines.join("\n")
}

/// P04 default distance-to-liquidation cuts in **percent**: soft 25 / warn 15
/// / hard 8 (`SPEC-P04.md` §3.2, mirrored by the `crate::config::RiskConfig`
/// defaults `RISK_SOFT_PCT` / `RISK_WARN_PCT` / `RISK_HARD_PCT`). The prompt
/// layer reads no config (`SPEC-P07.md` §5), so the defaults are hardcoded.
fn default_thresholds() -> RiskThresholds {
    RiskThresholds {
        soft: Decimal::new(25, 0),
        warn: Decimal::new(15, 0),
        hard: Decimal::new(8, 0),
    }
}

/// Distance-to-liquidation percent and tier for `position`, when computable.
fn distance_and_tier(
    position: &Position,
    markets: &[Market],
    thresholds: &RiskThresholds,
) -> Option<(Decimal, RiskTier)> {
    let market = markets.iter().find(|m| m.id == position.market_id)?;
    let distance = distance_to_liq_pct(position, market)?;
    Some((distance, tier(distance, thresholds)))
}

/// `LONG` / `SHORT` for a signed size (`FLAT` when zero).
fn side(position: &Position) -> &'static str {
    if position.size > Decimal::ZERO {
        "LONG"
    } else if position.size < Decimal::ZERO {
        "SHORT"
    } else {
        "FLAT"
    }
}

/// `mark` column: the price, or `n/a` when unknown.
fn mark_text(position: &Position) -> String {
    position
        .mark_price
        .map_or_else(|| "n/a".to_string(), |mark| mark.to_string())
}

/// `#a, #b` allowlist text (`(none)` when empty).
fn allowlist_text(allowlist: &[MarketId]) -> String {
    if allowlist.is_empty() {
        "(none)".to_string()
    } else {
        allowlist
            .iter()
            .map(|id| format!("#{}", id.0))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One account-table row; the focus position carries a `>>` prefix.
fn account_row(
    position: &Position,
    markets: &[Market],
    focus: &Position,
    thresholds: &RiskThresholds,
) -> String {
    let prefix = if position.market_id == focus.market_id {
        ">> "
    } else {
        ""
    };
    let mark = mark_text(position);
    let (dist_pct, tier_name) = match distance_and_tier(position, markets, thresholds) {
        Some((distance, pos_tier)) => (distance.to_string(), format!("{pos_tier:?}")),
        None => ("n/a".to_string(), "n/a".to_string()),
    };
    format!(
        "{prefix}{} | #{} | {} | {} | {} | {} | {} | {} | {} | {}",
        position.symbol,
        position.market_id.0,
        side(position),
        position.size,
        position.entry_price,
        mark,
        position.collateral,
        position.unrealized_pnl,
        dist_pct,
        tier_name,
    )
}

/// Smart-money block body: the literal unavailable line when every data field
/// is `None` (`SPEC-P07.md` §5), otherwise the asset plus each present value.
fn smart_money_lines(sm: &SmartMoneyContext) -> Vec<String> {
    if sm.netflow_24h.is_none()
        && sm.holdings_delta.is_none()
        && sm.long_short_ratio.is_none()
        && sm.top_traders_net_bias.is_none()
    {
        return vec!["- smart-money data unavailable — decide without it".to_string()];
    }
    let mut lines = vec![format!("- asset: {}", sm.asset)];
    if let Some(value) = sm.netflow_24h {
        lines.push(format!("- netflow_24h_usd: {value}"));
    }
    if let Some(value) = sm.holdings_delta {
        lines.push(format!("- holdings_delta_usd: {value}"));
    }
    if let Some(value) = sm.long_short_ratio {
        lines.push(format!("- long_short_ratio: {value}"));
    }
    if let Some(value) = &sm.top_traders_net_bias {
        lines.push(format!("- top_traders_net_bias: {value}"));
    }
    if let Some(value) = sm.fetched_at_ms {
        lines.push(format!("- fetched_at_ms: {value}"));
    }
    if let Some(value) = sm.total_cost_usd {
        lines.push(format!("- total_cost_usd: {value}"));
    }
    if let Some(value) = &sm.note {
        lines.push(format!("- note: {value}"));
    }
    lines
}

/// Restated focus position with its distance-to-liquidation and tier.
fn focus_line(focus: &Position, markets: &[Market], thresholds: &RiskThresholds) -> String {
    let mark = mark_text(focus);
    let distance = match distance_and_tier(focus, markets, thresholds) {
        Some((distance, focus_tier)) => {
            format!("distance-to-liquidation {distance}% ({focus_tier:?} tier)")
        }
        None => "distance-to-liquidation n/a".to_string(),
    };
    format!(
        "- {} | #{} | {} | size {} | entry {} | mark {} | collateral {} | uPnL {} | {}",
        focus.symbol,
        focus.market_id.0,
        side(focus),
        focus.size,
        focus.entry_price,
        mark,
        focus.collateral,
        focus.unrealized_pnl,
        distance,
    )
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;

    /// ETH-like market with the P04 fixture's maintenance margin (0.05).
    fn market(id: u32, symbol: &str) -> Market {
        Market {
            id: MarketId(id),
            symbol: symbol.to_string(),
            base: symbol.to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: Decimal::new(83333, 6),
            maintenance_margin_fraction: Decimal::new(5, 2),
            max_leverage: Decimal::new(12, 0),
            min_size: Decimal::ZERO,
            tick_size: Decimal::new(1, 2),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    fn position(
        market_id: u32,
        symbol: &str,
        size: Decimal,
        entry_price: Decimal,
        mark_price: Option<Decimal>,
        collateral: Decimal,
        unrealized_pnl: Decimal,
    ) -> Position {
        Position {
            market_id: MarketId(market_id),
            symbol: symbol.to_string(),
            size,
            entry_price,
            mark_price,
            liq_price: None,
            collateral,
            unrealized_pnl,
            margin_ratio: None,
            leverage: Decimal::new(10, 0),
            opened_at: None,
        }
    }

    fn ts() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).expect("valid timestamp")
    }

    fn sample_policy() -> PolicySummary {
        PolicySummary {
            market_allowlist: vec![MarketId(32), MarketId(20)],
            max_order_size_usd: Decimal::new(5000, 0),
            require_approval_above_usd: Decimal::new(2500, 0),
            daily_actions_left: 5,
        }
    }

    /// Three-position account: ETH long (Green) as the focus, BTC short (Red),
    /// SOL long (Yellow).
    ///
    /// Distances (derived from first principles, P04 formula): ETH
    /// ≈ 45.498765523 %, BTC 5.00 %, SOL 20.00 %.
    fn render(sm: &SmartMoneyContext, reflex: &ReflexSummary) -> String {
        let markets = vec![market(32, "ETH"), market(20, "BTC"), market(28, "SOL")];
        let positions = vec![
            position(
                32,
                "ETH",
                Decimal::new(10, 0),
                Decimal::new(2700, 0),
                Some(Decimal::new(271370, 2)),
                Decimal::new(13560, 0),
                Decimal::new(137, 0),
            ),
            position(
                20,
                "BTC",
                Decimal::new(-1, 1),
                Decimal::new(95000, 0),
                Some(Decimal::new(95000, 0)),
                Decimal::new(950, 0),
                Decimal::ZERO,
            ),
            position(
                28,
                "SOL",
                Decimal::new(20, 0),
                Decimal::new(150, 0),
                Some(Decimal::new(150, 0)),
                Decimal::new(750, 0),
                Decimal::ZERO,
            ),
        ];
        let focus = positions[0].clone();
        let account = AccountState {
            positions,
            free_balance: Decimal::new(5000, 0),
            equity: Decimal::new(25000, 0),
            fee_tier: 0,
            snapshot_ts: ts(),
        };
        let policy = sample_policy();
        let input = PromptInput {
            account: &account,
            markets: &markets,
            focus: &focus,
            policy: &policy,
            sm,
            reflex,
            now_ms: 1_699_999_999_999,
        };
        user_prompt(&input)
    }

    #[test]
    fn system_prompt_roles_actions_and_no_digits() {
        let system = system_prompt();
        for needle in [
            "ESCALATE",
            "isolated",
            "HOLD",
            "REDUCE",
            "CLOSE",
            "ADD_COLLATERAL",
            "human",
            "schema",
        ] {
            assert!(system.contains(needle), "system prompt missing {needle}");
        }
        assert!(system.contains("Never recommend increasing exposure"));
        assert!(system.contains("prefer REDUCE"));
        assert!(system.contains("cite only numbers present there"));
        assert!(
            !system.chars().any(|c| c.is_ascii_digit()),
            "the system text must stay digit-free"
        );
        let lines: Vec<&str> = system.lines().collect();
        assert!((8..=14).contains(&lines.len()), "{} lines", lines.len());
    }

    #[test]
    fn builder_contains_every_required_section_marker() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );
        for marker in [
            "# Sentinel strategy consult",
            "- prompt_version: v3.0",
            "- now_ms: 1699999999999",
            "## ACCOUNT",
            "## POLICY",
            "## REFLEX",
            "## SMART MONEY",
            "## FOCUS",
            "## OUTPUT",
        ] {
            assert!(prompt.contains(marker), "missing marker: {marker}");
        }
        assert!(prompt.contains("- market_allowlist: #32, #20"));
        assert!(prompt.contains("- max_order_size_usd: 5000"));
        assert!(prompt.contains("- require_approval_above_usd: 2500"));
        assert!(prompt.contains("- daily_actions_left: 5"));
    }

    #[test]
    fn account_table_rows_and_focus_prefix() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );

        let marked: Vec<&str> = prompt
            .lines()
            .filter(|line| line.starts_with(">> "))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one focus row: {marked:?}");
        let focus_row = marked[0];
        assert!(
            focus_row.starts_with(">> ETH | #32 | LONG | 10 | 2700 | 2713.70 | 13560 | 137 | "),
            "focus row: {focus_row}"
        );
        assert!(focus_row.contains("45.498765523"), "focus row: {focus_row}");
        assert!(focus_row.ends_with("Green"), "focus row: {focus_row}");

        let btc_row = prompt
            .lines()
            .find(|line| line.contains("BTC | #20"))
            .expect("BTC row");
        assert!(btc_row.ends_with("Red"), "BTC row: {btc_row}");
        assert!(btc_row.contains("SHORT | -0.1 |"), "BTC row: {btc_row}");

        let sol_row = prompt
            .lines()
            .find(|line| line.contains("SOL | #28"))
            .expect("SOL row");
        assert!(sol_row.ends_with("Yellow"), "SOL row: {sol_row}");
    }

    #[test]
    fn missing_mark_and_uncomputable_distance_render_n_a() {
        let markets = vec![market(32, "ETH")];
        let pos = position(
            32,
            "ETH",
            Decimal::new(10, 0),
            Decimal::new(2700, 0),
            None,
            Decimal::new(13560, 0),
            Decimal::ZERO,
        );
        let focus = pos.clone();
        let account = AccountState {
            positions: vec![pos],
            free_balance: Decimal::ZERO,
            equity: Decimal::ZERO,
            fee_tier: 0,
            snapshot_ts: ts(),
        };
        let policy = sample_policy();
        let sm = SmartMoneyContext::unavailable("ETH");
        let reflex = ReflexSummary::default();
        let input = PromptInput {
            account: &account,
            markets: &markets,
            focus: &focus,
            policy: &policy,
            sm: &sm,
            reflex: &reflex,
            now_ms: 1,
        };
        let prompt = user_prompt(&input);
        assert!(
            prompt.contains("| n/a | 13560 | 0 | n/a | n/a"),
            "mark and distance render n/a: {prompt}"
        );
        assert!(prompt.contains("distance-to-liquidation n/a"));
    }

    #[test]
    fn focus_section_restates_the_focus_position() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );
        let focus_line = prompt
            .lines()
            .find(|line| line.starts_with("- ETH | #32 | LONG | size"))
            .expect("focus line");
        assert!(
            focus_line.contains("distance-to-liquidation 45.498765523"),
            "focus line: {focus_line}"
        );
        assert!(
            focus_line.contains("(Green tier)"),
            "focus line: {focus_line}"
        );
    }

    #[test]
    fn unavailable_smart_money_renders_the_exact_line() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );
        let line = prompt
            .lines()
            .find(|line| line.contains("smart-money data unavailable"))
            .expect("unavailable line rendered");
        assert_eq!(line, "- smart-money data unavailable — decide without it");
        assert!(!prompt.contains("netflow"));
    }

    #[test]
    fn smart_money_with_netflow_renders_the_number() {
        let sm = SmartMoneyContext {
            netflow_24h: Some(Decimal::new(123456, 2)),
            note: Some("cross-venue proxy".to_string()),
            ..SmartMoneyContext::unavailable("ETH")
        };
        let prompt = render(&sm, &ReflexSummary::default());
        assert!(prompt.contains("- asset: ETH"));
        assert!(prompt.contains("- netflow_24h_usd: 1234.56"));
        assert!(prompt.contains("- note: cross-venue proxy"));
        assert!(!prompt.contains("smart-money data unavailable"));
    }

    #[test]
    fn reflex_block_renders_actions_or_none_yet() {
        let sm = SmartMoneyContext::unavailable("ETH");

        let empty = render(&sm, &ReflexSummary::default());
        assert!(empty.contains("## REFLEX (recent actions, newest last)"));
        assert!(empty.contains("- none yet"));

        let reflex = ReflexSummary {
            actions: vec![
                "reduce 25% ETH (orange)".to_string(),
                "alert: stale feed".to_string(),
            ],
        };
        let filled = render(&sm, &reflex);
        assert!(filled.contains("- reduce 25% ETH (orange)"));
        assert!(filled.contains("- alert: stale feed"));
        assert!(!filled.contains("- none yet"));
    }

    #[test]
    fn final_instruction_names_the_exact_schema() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );
        assert!(prompt.contains(&format!(
            "Respond with ONLY one JSON object matching: {}",
            crate::brain::parser::SCHEMA_DOC
        )));
        assert!(prompt.contains("at most two sentences"));
        assert!(prompt.contains("exactly one of the schema's actions"));
    }

    #[test]
    fn budget_three_positions_under_6000_chars() {
        let prompt = render(
            &SmartMoneyContext::unavailable("ETH"),
            &ReflexSummary::default(),
        );
        assert!(prompt.len() <= 6000, "prompt is {} chars", prompt.len());
        assert!(
            prompt.len() > 300,
            "prompt is suspiciously short: {} chars",
            prompt.len()
        );
    }

    #[test]
    fn unavailable_smart_money_context_has_no_values() {
        let sm = SmartMoneyContext::unavailable("ETH");
        assert_eq!(sm.asset, "ETH");
        assert!(sm.netflow_24h.is_none());
        assert!(sm.holdings_delta.is_none());
        assert!(sm.long_short_ratio.is_none());
        assert!(sm.top_traders_net_bias.is_none());
        assert!(sm.fetched_at_ms.is_none());
        assert!(sm.total_cost_usd.is_none());
        assert_eq!(sm.note.as_deref(), Some("smart-money data unavailable"));
    }

    #[test]
    fn summary_types_serde_round_trip() {
        let policy = sample_policy();
        let encoded = serde_json::to_string(&policy).expect("serialize policy");
        let decoded: PolicySummary = serde_json::from_str(&encoded).expect("deserialize policy");
        assert_eq!(decoded, policy);

        let sm = SmartMoneyContext::unavailable("ETH");
        let encoded = serde_json::to_string(&sm).expect("serialize sm");
        let decoded: SmartMoneyContext = serde_json::from_str(&encoded).expect("deserialize sm");
        assert_eq!(decoded, sm);

        let reflex = ReflexSummary {
            actions: vec!["hold".to_string()],
        };
        let encoded = serde_json::to_string(&reflex).expect("serialize reflex");
        let decoded: ReflexSummary = serde_json::from_str(&encoded).expect("deserialize reflex");
        assert_eq!(decoded, reflex);
    }
}
