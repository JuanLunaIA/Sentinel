//! Policy gate — pure evaluation of intents against configured limits.
//!
//! The gate sits between intent producers (reflex engine, strategy brain,
//! manual commands) and executors: every intent passes through
//! [`PolicyEngine::evaluate`] before execution (P00 invariant #1).
//!
//! Check precedence, exact reasons, and the deliberate REFLEX+Red approval
//! asymmetry are frozen in `SPEC-P05.md` §3.
//!
//! **P05 status:** implemented; checks run in `SPEC-P05.md` §3 order and the
//! first failure wins. The `Reflex`+`Red` asymmetry bypasses only the approval
//! threshold — the per-order cap and the daily action cap still apply.

use rust_decimal::Decimal;

use crate::types::{AccountState, Intent, MarketId, PolicyVerdict, Position, RiskTier};

/// Where an intent came from (affects only the approval asymmetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// Deterministic reflex engine.
    Reflex,
    /// LLM strategy brain (P07+).
    Strategy,
    /// Human command via bot/API (P11+).
    Manual,
}

/// Per-call context: which position an intent targets and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyContext {
    /// Producer of the intent.
    pub source: PolicySource,
    /// Current risk tier of the targeted position.
    pub tier: RiskTier,
    /// Market the intent acts on.
    pub market_id: MarketId,
}

/// Executed-action counter for the current day (caller-maintained).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DayState {
    /// Actions already executed today.
    pub actions_today: u32,
}

/// Static policy limits (mapped from the application configuration).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyConfig {
    /// Markets the engine may act on.
    pub market_allowlist: Vec<MarketId>,
    /// Per-action notional ceiling, USD.
    pub max_order_size_usd: Decimal,
    /// Daily action cap.
    pub max_daily_actions: u32,
    /// Notional above which human approval is required, USD.
    pub require_approval_above_usd: Decimal,
    /// Kill switch: when `true`, every intent is denied.
    pub kill_switch: bool,
}

/// The policy gate (pure associated function; no state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyEngine;

impl PolicyEngine {
    /// Evaluate an intent against the configured limits.
    ///
    /// Returns `Allow`, `Deny { reason }` or `NeedsApproval { reason }`;
    /// precedence and messages are normative in `SPEC-P05.md` §3.
    ///
    /// The checks run in spec order and the first failure wins: kill switch →
    /// market allowlist → position resolution (`Alert` short-circuits to
    /// `Allow` here, after the first two checks) → reduce-only bias checks →
    /// notional (`Reduce` = `fraction × |size| × mark`, `Close` = `|size| ×
    /// mark`, `AddCollateral` = `amount`; `Reduce`/`Close` require a positive
    /// mark) → per-order cap (strict `>`) → approval threshold (strict `>`,
    /// with the deliberate `Reflex`+`Red` bypass) → daily action cap.
    ///
    /// The `Reflex`+`Red` bypass skips only the approval check: such an
    /// intent is still denied by the cap or the daily cap.
    pub fn evaluate(
        intent: &Intent,
        account: &AccountState,
        cfg: &PolicyConfig,
        day: &DayState,
        ctx: &PolicyContext,
    ) -> PolicyVerdict {
        // 1. Kill switch: deny everything, unconditionally.
        if cfg.kill_switch {
            return deny("kill switch engaged");
        }

        // 2. Market allowlist (applies to every intent, alerts included).
        if !cfg.market_allowlist.contains(&ctx.market_id) {
            return deny(format!("market {} not in allowlist", ctx.market_id.0));
        }

        // 3. `Alert` skips position resolution and checks 4–7 (SPEC-P05 §3
        // step 3): an informational alert needs no position and no notional.
        if matches!(intent, Intent::Alert { .. }) {
            return PolicyVerdict::Allow;
        }

        // 3 (cont.). Every actionable intent must resolve to a position.
        let Some(position) = account
            .positions
            .iter()
            .find(|position| position.market_id == ctx.market_id)
        else {
            return deny(format!("no position for market {}", ctx.market_id.0));
        };

        // 4–5. Variant bias checks, then notional.
        let notional = match intent {
            Intent::Reduce { fraction, .. } => {
                if *fraction <= Decimal::ZERO || *fraction > Decimal::ONE {
                    return deny("invalid reduce fraction");
                }
                let Some(mark) = usable_mark(position) else {
                    return deny("mark unavailable for notional");
                };
                *fraction * position.size.abs() * mark
            }
            Intent::Close { .. } => {
                if position.size.is_zero() {
                    return deny("position already flat");
                }
                let Some(mark) = usable_mark(position) else {
                    return deny("mark unavailable for notional");
                };
                position.size.abs() * mark
            }
            Intent::AddCollateral { amount, .. } => {
                if *amount <= Decimal::ZERO {
                    return deny("collateral amount must be positive");
                }
                // Collateral top-ups need no mark: notional = amount.
                *amount
            }
            // Returned in check 3 above; listed for exhaustiveness.
            Intent::Alert { .. } => return PolicyVerdict::Allow,
        };

        // 6. Per-order notional cap (strict `>`: exactly at the cap passes).
        if notional > cfg.max_order_size_usd {
            return deny("notional exceeds MAX_ORDER_SIZE_USD");
        }

        // 7. Approval threshold (strict `>`: exactly at the threshold passes).
        // The REFLEX+Red asymmetry bypasses only this check — a Red reflex
        // breach still cannot exceed the cap (6) or the daily cap (8).
        let reflex_red = ctx.source == PolicySource::Reflex && ctx.tier == RiskTier::Red;
        if notional > cfg.require_approval_above_usd && !reflex_red {
            return PolicyVerdict::NeedsApproval {
                reason: "notional above approval threshold".to_string(),
            };
        }

        // 8. Daily action cap. `actions_today` counts executed actions; the
        // caller increments it after execution.
        if day.actions_today >= cfg.max_daily_actions {
            return deny("daily action cap reached");
        }

        PolicyVerdict::Allow
    }
}

/// Build a `Deny` verdict with the given reason.
fn deny(reason: impl Into<String>) -> PolicyVerdict {
    PolicyVerdict::Deny {
        reason: reason.into(),
    }
}

/// Mark price usable for notional math: present and strictly positive.
fn usable_mark(position: &Position) -> Option<Decimal> {
    position.mark_price.filter(|mark| *mark > Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::types::{AccountState, Intent, MarketId, PolicyVerdict, Position, RiskTier};

    // --- fixtures ----------------------------------------------------------

    /// Canonical limits: market 32 allowlisted, cap 250, approval 150, day 5.
    fn cfg() -> PolicyConfig {
        PolicyConfig {
            market_allowlist: vec![MarketId(32)],
            max_order_size_usd: dec!(250),
            max_daily_actions: 5,
            require_approval_above_usd: dec!(150),
            kill_switch: false,
        }
    }

    /// Market-32 position with the given signed size and mark price.
    fn position(size: Decimal, mark: Option<Decimal>) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price: dec!(2500),
            mark_price: mark,
            liq_price: None,
            collateral: dec!(1000),
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: dec!(10),
            opened_at: None,
        }
    }

    fn account(positions: Vec<Position>) -> AccountState {
        AccountState {
            positions,
            free_balance: dec!(10000),
            equity: dec!(10000),
            fee_tier: 0,
            snapshot_ts: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// Market-32 account holding a 2-unit long at mark 100 (close = 200).
    fn long_account() -> AccountState {
        account(vec![position(dec!(2), Some(dec!(100)))])
    }

    /// Mirrored short account (same |size| and mark).
    fn short_account() -> AccountState {
        account(vec![position(dec!(-2), Some(dec!(100)))])
    }

    fn ctx(source: PolicySource, tier: RiskTier) -> PolicyContext {
        PolicyContext {
            source,
            tier,
            market_id: MarketId(32),
        }
    }

    fn day(actions_today: u32) -> DayState {
        DayState { actions_today }
    }

    fn reduce(fraction: Decimal) -> Intent {
        Intent::Reduce {
            fraction,
            reason: "test".to_string(),
        }
    }

    fn close() -> Intent {
        Intent::Close {
            reason: "test".to_string(),
        }
    }

    fn collateral(amount: Decimal) -> Intent {
        Intent::AddCollateral {
            amount,
            reason: "test".to_string(),
        }
    }

    fn alert() -> Intent {
        Intent::Alert {
            message: "test".to_string(),
        }
    }

    fn denied(reason: &str) -> PolicyVerdict {
        PolicyVerdict::Deny {
            reason: reason.to_string(),
        }
    }

    fn needs_approval() -> PolicyVerdict {
        PolicyVerdict::NeedsApproval {
            reason: "notional above approval threshold".to_string(),
        }
    }

    /// Evaluate with the canonical `Strategy`/`Green` producer context.
    fn gate(
        intent: &Intent,
        account: &AccountState,
        cfg: &PolicyConfig,
        day: &DayState,
    ) -> PolicyVerdict {
        PolicyEngine::evaluate(
            intent,
            account,
            cfg,
            day,
            &ctx(PolicySource::Strategy, RiskTier::Green),
        )
    }

    // --- rule 1: kill switch ----------------------------------------------

    #[test]
    fn rule1_kill_switch_beats_everything() {
        let mut killed = cfg();
        killed.kill_switch = true;

        // Allowed, invalid, missing-position, collateral and alert intents:
        // every one is denied with the kill-switch reason.
        for (intent, acct) in [
            (reduce(dec!(0.5)), long_account()),
            (reduce(dec!(-1)), long_account()),
            (close(), account(vec![])),
            (collateral(dec!(10)), long_account()),
            (alert(), account(vec![])),
        ] {
            assert_eq!(
                gate(&intent, &acct, &killed, &day(0)),
                denied("kill switch engaged"),
                "intent {intent:?}"
            );
        }

        // It also beats a market that is not even allowlisted.
        let v = PolicyEngine::evaluate(
            &alert(),
            &account(vec![]),
            &killed,
            &day(0),
            &PolicyContext {
                source: PolicySource::Reflex,
                tier: RiskTier::Red,
                market_id: MarketId(99),
            },
        );
        assert_eq!(v, denied("kill switch engaged"));
    }

    // --- rule 2: allowlist -------------------------------------------------

    #[test]
    fn rule2_allowlist_before_position_lookup() {
        let outside = PolicyContext {
            source: PolicySource::Strategy,
            tier: RiskTier::Green,
            market_id: MarketId(99),
        };

        // Not allowlisted AND no position: the allowlist reason wins.
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(0.5)),
                &account(vec![]),
                &cfg(),
                &day(0),
                &outside
            ),
            denied("market 99 not in allowlist")
        );

        // Allowlisted but no position: the position reason wins next.
        assert_eq!(
            gate(&reduce(dec!(0.5)), &account(vec![]), &cfg(), &day(0)),
            denied("no position for market 32")
        );

        // An empty allowlist denies every market.
        let mut empty = cfg();
        empty.market_allowlist.clear();
        assert_eq!(
            gate(&close(), &long_account(), &empty, &day(0)),
            denied("market 32 not in allowlist")
        );

        // Alerts are subject to the allowlist too (rules 1–2 always apply).
        assert_eq!(
            PolicyEngine::evaluate(&alert(), &long_account(), &cfg(), &day(0), &outside),
            denied("market 99 not in allowlist")
        );
    }

    // --- rule 3: alert short-circuit + position resolution -----------------

    #[test]
    fn rule3_alert_short_circuits_after_rules_1_and_2() {
        // Marked reading of SPEC-P05 §3 step 3: `Alert` skips steps 3–7 and
        // returns `Allow` even when the market has no position at all ...
        assert_eq!(
            gate(&alert(), &account(vec![]), &cfg(), &day(0)),
            PolicyVerdict::Allow
        );

        // ... or the position is flat, or the daily cap is saturated: the
        // short-circuit returns before the daily rule (alerts are not actions).
        let flat = account(vec![position(dec!(0), Some(dec!(100)))]);
        assert_eq!(
            gate(&alert(), &flat, &cfg(), &day(99)),
            PolicyVerdict::Allow
        );

        // Rules 1–2 still apply to alerts.
        let mut killed = cfg();
        killed.kill_switch = true;
        assert_eq!(
            gate(&alert(), &long_account(), &killed, &day(0)),
            denied("kill switch engaged")
        );
        assert_eq!(
            PolicyEngine::evaluate(
                &alert(),
                &long_account(),
                &cfg(),
                &day(0),
                &PolicyContext {
                    source: PolicySource::Reflex,
                    tier: RiskTier::Red,
                    market_id: MarketId(99),
                },
            ),
            denied("market 99 not in allowlist")
        );
    }

    #[test]
    fn rule3_missing_position_denied_for_actionable_intents() {
        for intent in [reduce(dec!(0.5)), close(), collateral(dec!(10))] {
            assert_eq!(
                gate(&intent, &account(vec![]), &cfg(), &day(0)),
                denied("no position for market 32"),
                "intent {intent:?}"
            );
        }
        // A position on a different market does not resolve.
        let other = account(vec![Position {
            market_id: MarketId(20),
            ..position(dec!(2), Some(dec!(100)))
        }]);
        assert_eq!(
            gate(&close(), &other, &cfg(), &day(0)),
            denied("no position for market 32")
        );
    }

    // --- rule 4: reduce-only bias checks -----------------------------------

    #[test]
    fn rule4_reduce_fraction_bounds() {
        // Outside (0, 1] → deny, for every producer and both sides.
        for bad in [dec!(-0.5), dec!(0), dec!(1.0001), dec!(2), dec!(100)] {
            for acct in [long_account(), short_account()] {
                for source in [
                    PolicySource::Reflex,
                    PolicySource::Strategy,
                    PolicySource::Manual,
                ] {
                    for tier in [
                        RiskTier::Green,
                        RiskTier::Yellow,
                        RiskTier::Orange,
                        RiskTier::Red,
                    ] {
                        let v = PolicyEngine::evaluate(
                            &reduce(bad),
                            &acct,
                            &cfg(),
                            &day(0),
                            &ctx(source, tier),
                        );
                        assert_eq!(v, denied("invalid reduce fraction"), "fraction {bad}");
                    }
                }
            }
        }

        // The admitted boundaries of (0, 1]: a tiny fraction passes straight
        // to `Allow`; fraction 1 is admitted by the bias check and then held
        // by the approval threshold (notional 200 > 150).
        assert_eq!(
            gate(&reduce(dec!(0.0000001)), &long_account(), &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
        assert_eq!(
            gate(&reduce(dec!(1)), &long_account(), &cfg(), &day(0)),
            needs_approval()
        );

        // The fraction check precedes the mark check: with no mark a bad
        // fraction still reports the fraction problem.
        let no_mark = account(vec![position(dec!(2), None)]);
        assert_eq!(
            gate(&reduce(dec!(2)), &no_mark, &cfg(), &day(0)),
            denied("invalid reduce fraction")
        );
    }

    #[test]
    fn rule4_close_on_flat_position_denied() {
        let flat = account(vec![position(dec!(0), Some(dec!(100)))]);
        assert_eq!(
            gate(&close(), &flat, &cfg(), &day(0)),
            denied("position already flat")
        );

        // The flat check precedes the mark check.
        let flat_no_mark = account(vec![position(dec!(0), None)]);
        assert_eq!(
            gate(&close(), &flat_no_mark, &cfg(), &day(0)),
            denied("position already flat")
        );

        // Non-flat positions pass the bias check.
        assert_ne!(
            gate(&close(), &long_account(), &cfg(), &day(0)),
            denied("position already flat")
        );
    }

    #[test]
    fn rule4_collateral_amount_must_be_positive() {
        for bad in [dec!(0), dec!(-0.001), dec!(-100)] {
            assert_eq!(
                gate(&collateral(bad), &long_account(), &cfg(), &day(0)),
                denied("collateral amount must be positive"),
                "amount {bad}"
            );
        }
        // A tiny positive amount passes the bias check (and the rest).
        assert_eq!(
            gate(&collateral(dec!(0.01)), &long_account(), &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
    }

    // --- rule 5: mark + notional -------------------------------------------

    #[test]
    fn rule5_mark_required_for_reduce_and_close_only() {
        for mark in [None, Some(dec!(0)), Some(dec!(-5))] {
            let acct = account(vec![position(dec!(2), mark)]);
            assert_eq!(
                gate(&reduce(dec!(0.5)), &acct, &cfg(), &day(0)),
                denied("mark unavailable for notional"),
                "reduce mark {mark:?}"
            );
            assert_eq!(
                gate(&close(), &acct, &cfg(), &day(0)),
                denied("mark unavailable for notional"),
                "close mark {mark:?}"
            );
        }
        // AddCollateral and Alert need no mark.
        let no_mark = account(vec![position(dec!(2), None)]);
        assert_eq!(
            gate(&collateral(dec!(100)), &no_mark, &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
        assert_eq!(
            gate(&alert(), &no_mark, &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
    }

    #[test]
    fn rule5_notional_formulas() {
        // Reduce: fraction × |size| × mark = 0.5 × 2 × 100 = 100.
        let mut under = cfg();
        under.max_order_size_usd = dec!(99);
        under.require_approval_above_usd = dec!(99);
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &under, &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD"),
            "reduce notional is 100"
        );
        let mut at = cfg();
        at.max_order_size_usd = dec!(100);
        at.require_approval_above_usd = dec!(100);
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &at, &day(0)),
            PolicyVerdict::Allow,
            "reduce notional 100 is exactly at the 100 cap"
        );

        // Close: |size| × mark = 200, identical for longs and shorts.
        for acct in [long_account(), short_account()] {
            let mut under = cfg();
            under.max_order_size_usd = dec!(199);
            under.require_approval_above_usd = dec!(199);
            assert_eq!(
                gate(&close(), &acct, &under, &day(0)),
                denied("notional exceeds MAX_ORDER_SIZE_USD"),
                "close notional is 200"
            );
            let mut at = cfg();
            at.max_order_size_usd = dec!(200);
            at.require_approval_above_usd = dec!(200);
            assert_eq!(gate(&close(), &acct, &at, &day(0)), PolicyVerdict::Allow);
        }

        // AddCollateral: notional = amount, and no mark is involved.
        let no_mark = account(vec![position(dec!(2), None)]);
        let mut under = cfg();
        under.max_order_size_usd = dec!(199);
        under.require_approval_above_usd = dec!(199);
        assert_eq!(
            gate(&collateral(dec!(200)), &no_mark, &under, &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD"),
            "collateral notional is the amount"
        );
        let mut at = cfg();
        at.max_order_size_usd = dec!(200);
        at.require_approval_above_usd = dec!(200);
        assert_eq!(
            gate(&collateral(dec!(200)), &no_mark, &at, &day(0)),
            PolicyVerdict::Allow
        );
    }

    // --- rule 6: per-order cap ---------------------------------------------

    #[test]
    fn rule6_cap_boundary_strictly_greater() {
        // Exactly at the cap passes: 200 == cap. The approval threshold is
        // set equal so the verdict isolates the cap boundary → Allow.
        let mut cfg200 = cfg();
        cfg200.max_order_size_usd = dec!(200);
        cfg200.require_approval_above_usd = dec!(200);
        assert_eq!(
            gate(&reduce(dec!(1)), &long_account(), &cfg200, &day(0)),
            PolicyVerdict::Allow,
            "notional 200 == cap 200 must pass"
        );

        // A hair above the cap → denied by rule 6 (not by approval).
        let above = account(vec![position(dec!(2), Some(dec!(100.05)))]);
        assert_eq!(
            gate(&reduce(dec!(1)), &above, &cfg200, &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD")
        );

        // Standard fixture (cap 250): 250 == cap passes (then approval), 251
        // denies.
        let at = account(vec![position(dec!(2), Some(dec!(125)))]);
        let over = account(vec![position(dec!(2), Some(dec!(125.5)))]);
        assert_eq!(gate(&close(), &at, &cfg(), &day(0)), needs_approval());
        assert_eq!(
            gate(&close(), &over, &cfg(), &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD")
        );
    }

    #[test]
    fn rule6_cap_beats_approval() {
        // Notional above BOTH the cap (250) and the approval threshold (150):
        // the cap denial wins.
        let over = account(vec![position(dec!(2), Some(dec!(130)))]); // 260
        assert_eq!(
            gate(&close(), &over, &cfg(), &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD")
        );
        assert_eq!(
            gate(&collateral(dec!(300)), &long_account(), &cfg(), &day(0)),
            denied("notional exceeds MAX_ORDER_SIZE_USD")
        );
    }

    // --- rule 7: approval threshold + REFLEX+Red asymmetry ------------------

    #[test]
    fn rule7_approval_threshold_strictly_greater() {
        let mut cfg200 = cfg();
        cfg200.require_approval_above_usd = dec!(200);
        // Exactly at the threshold passes: 200 == threshold → Allow.
        assert_eq!(
            gate(&reduce(dec!(1)), &long_account(), &cfg200, &day(0)),
            PolicyVerdict::Allow,
            "notional 200 == threshold 200 must pass"
        );
        // A hair above → needs approval.
        let above = account(vec![position(dec!(2), Some(dec!(100.05)))]);
        assert_eq!(
            gate(&reduce(dec!(1)), &above, &cfg200, &day(0)),
            needs_approval()
        );
        // Below with the standard config → Allow.
        let one_unit = account(vec![position(dec!(1), Some(dec!(140)))]);
        assert_eq!(
            gate(&close(), &one_unit, &cfg(), &day(0)),
            PolicyVerdict::Allow
        );
    }

    #[test]
    fn rule7_reflex_red_bypass_is_approval_only() {
        let above = account(vec![position(dec!(2), Some(dec!(100.5)))]); // 201 > 150

        // (a) Reflex+Red above the approval threshold → immediate Allow.
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(1)),
                &above,
                &cfg(),
                &day(0),
                &ctx(PolicySource::Reflex, RiskTier::Red)
            ),
            PolicyVerdict::Allow
        );

        // (b) Every other producer/tier combination needs approval.
        for (source, tier) in [
            (PolicySource::Strategy, RiskTier::Red),
            (PolicySource::Manual, RiskTier::Red),
            (PolicySource::Reflex, RiskTier::Orange),
            (PolicySource::Reflex, RiskTier::Green),
        ] {
            assert_eq!(
                PolicyEngine::evaluate(
                    &reduce(dec!(1)),
                    &above,
                    &cfg(),
                    &day(0),
                    &ctx(source, tier)
                ),
                needs_approval(),
                "source {source:?} tier {tier:?}"
            );
        }

        // (c) The bypass does NOT reach the per-order cap: 251 > 250.
        let over_cap = account(vec![position(dec!(2), Some(dec!(125.5)))]);
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(1)),
                &over_cap,
                &cfg(),
                &day(0),
                &ctx(PolicySource::Reflex, RiskTier::Red)
            ),
            denied("notional exceeds MAX_ORDER_SIZE_USD")
        );

        // (d) ... nor the daily cap: same bypass setup, day saturated.
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(1)),
                &above,
                &cfg(),
                &day(5),
                &ctx(PolicySource::Reflex, RiskTier::Red)
            ),
            denied("daily action cap reached")
        );

        // (e) A Red reflex below the threshold is plain Allow (bypass moot).
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(0.5)),
                &long_account(),
                &cfg(),
                &day(4),
                &ctx(PolicySource::Reflex, RiskTier::Red)
            ),
            PolicyVerdict::Allow
        );
    }

    // --- rule 8: daily cap --------------------------------------------------

    #[test]
    fn rule8_daily_cap_boundary_and_precedence() {
        // Below the cap → Allow; at the cap → Deny (`>=` semantics).
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &cfg(), &day(4)),
            PolicyVerdict::Allow
        );
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &cfg(), &day(5)),
            denied("daily action cap reached")
        );

        // A zero cap denies immediately.
        let mut zero = cfg();
        zero.max_daily_actions = 0;
        assert_eq!(
            gate(&reduce(dec!(0.5)), &long_account(), &zero, &day(0)),
            denied("daily action cap reached")
        );

        // Above the approval threshold with a saturated day: rule 7 fires
        // first (spec order, first failure wins), so the verdict is the
        // approval request — the daily cap is consulted only when rule 7
        // passes (below threshold, or via the Reflex+Red exception).
        assert_eq!(
            gate(&reduce(dec!(1)), &long_account(), &cfg(), &day(5)),
            needs_approval()
        );
        assert_eq!(
            PolicyEngine::evaluate(
                &reduce(dec!(1)),
                &long_account(),
                &cfg(),
                &day(5),
                &ctx(PolicySource::Reflex, RiskTier::Red)
            ),
            denied("daily action cap reached")
        );
    }

    // --- cross-cutting matrices ---------------------------------------------

    #[test]
    fn matrix_benign_intents_allow_across_sources_and_tiers() {
        // Notionals below every limit: Allow for every producer and tier.
        let one_unit = account(vec![position(dec!(1), Some(dec!(100)))]); // close = 100
        let cases = [
            (reduce(dec!(0.5)), long_account()), // 100
            (close(), one_unit),                 // 100
            (collateral(dec!(100)), long_account()),
        ];
        for source in [
            PolicySource::Reflex,
            PolicySource::Strategy,
            PolicySource::Manual,
        ] {
            for tier in [
                RiskTier::Green,
                RiskTier::Yellow,
                RiskTier::Orange,
                RiskTier::Red,
            ] {
                for (intent, acct) in &cases {
                    assert_eq!(
                        PolicyEngine::evaluate(intent, acct, &cfg(), &day(0), &ctx(source, tier)),
                        PolicyVerdict::Allow,
                        "{source:?}/{tier:?} {intent:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_intent_can_increase_or_flip_exposure() {
        // Static audit: the intent set cannot express an exposure increase
        // (exhaustive match — a new variant breaks compilation here).
        fn exposure_effect(intent: &Intent) -> &'static str {
            match intent {
                Intent::Reduce { .. } => "reduce-or-hold",
                Intent::Close { .. } => "close-to-zero",
                Intent::AddCollateral { .. } => "collateral-only",
                Intent::Alert { .. } => "none",
            }
        }

        // Fractions that would grow |size| or flip the side (<= 0 or > 1) are
        // denied outright, on longs and shorts alike.
        for acct in [long_account(), short_account()] {
            for fraction in [dec!(-1), dec!(0), dec!(1.0001), dec!(2)] {
                assert_eq!(
                    gate(&reduce(fraction), &acct, &cfg(), &day(0)),
                    denied("invalid reduce fraction"),
                    "fraction {fraction}"
                );
            }
            // Admitted reduces keep 0 < fraction <= 1, so the executed
            // quantity never exceeds |size| (rule 4 enforces the bound).
            for fraction in [dec!(0.0001), dec!(0.25), dec!(1)] {
                let v = gate(&reduce(fraction), &acct, &cfg(), &day(0));
                assert!(
                    matches!(
                        &v,
                        PolicyVerdict::Allow | PolicyVerdict::NeedsApproval { .. }
                    ),
                    "fraction {fraction} admitted: {v:?}"
                );
                assert!(fraction > Decimal::ZERO && fraction <= Decimal::ONE);
            }
        }

        // Every variant, with its audited exposure effect.
        for (intent, effect) in [
            (reduce(dec!(0.5)), "reduce-or-hold"),
            (close(), "close-to-zero"),
            (collateral(dec!(10)), "collateral-only"),
            (alert(), "none"),
        ] {
            assert_eq!(exposure_effect(&intent), effect);
        }
    }
}
