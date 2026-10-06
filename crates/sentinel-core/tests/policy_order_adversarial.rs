//! # P05 independent verification — adversarial suite for the policy gate and
//! reduce-only order sizing (`sentinel-core`).
//!
//! Written **without reading** `crates/sentinel-core/src/{policy.rs,order.rs}`
//! (black box): every expectation below is re-derived from `SPEC-P05.md` §3–§4
//! and `docs/FACTS.md` §1.6 (`CloseLong` = t 3 / `CloseShort` = t 4,
//! reduce-only by construction). Exact arithmetic was generated with Python
//! `decimal` (commands and complete outputs quoted below; the per-test `A|`…
//! `C|` line tags reference that output).
//!
//! ## Expectation generator (command 1 — run from the repo root)
//!
//! ```text
//! $ python3 /home/luna/.hermes/cache/scratch/p05_core_expectations.py
//! ```
//!
//! Output (complete):
//!
//! ```text
//! A| quantize_size_down table (truncate toward zero at 10^-d)
//! A| tq(0.1239 @ 3) = 0.123
//! A| tq(1.9999 @ 2) = 1.99
//! A| tq(0.999 @ 2) = 0.99
//! A| tq(12.3456 @ 0) = 12
//! A| tq(0.005 @ 2) = 0.00
//! A| tq(-0.1239 @ 3) = -0.123
//! A| tq(-1.9999 @ 2) = -1.99
//! A| tq(5 @ 2) = 5
//! A| tq(2.50 @ 1) = 2.5
//! B| reduce_by_fraction table: (pos.size, fraction, decimals, min_size) -> size/None
//! B| pos=4 f=0.25 d=3 min=0.001 -> 1.00
//! B| pos=0.1239 f=1 d=3 min=0 -> 0.123
//! B| pos=1.9999 f=1 d=2 min=0.01 -> 1.99
//! B| pos=1.0101 f=0.9 d=3 min=0 -> 0.909
//! B| pos=0.01 f=1 d=3 min=0.01 -> 0.01
//! B| pos=0.009 f=1 d=3 min=0.01 -> None
//! B| pos=0.0004 f=1 d=3 min=0 -> None
//! B| pos=0 f=0.5 d=3 min=0 -> None
//! B| pos=2.5 f=0.5 d=3 min=0 -> 1.25
//! B| pos=-2.5 f=0.5 d=3 min=0 -> 1.25
//! B| pos=3.7001 f=1 d=3 min=0 -> 3.700
//! B| pos=3.7 f=0.9999999 d=3 min=0 -> 3.699
//! B| pos=7.7777 f=1 d=2 min=1 -> 7.77
//! B| pos=4 f=0.25 d=4 min=0 -> 1.00
//! B2| fraction-bound skips (pos=4, d=3, min=0)
//! B2| f=0 -> None
//! B2| f=-0.5 -> None
//! B2| f=1 -> 4
//! B2| f=1.0000000000000000000000001 -> None
//! B2| f=1.0000000000000000000000002 -> None
//! B2| f=2 -> None
//! B3| clamp: quantized order <= quantize(|pos|) shown by equality of both truncations
//! B3| pos=3.7001 f=1 d=3: raw=3.7001 tq(raw)=3.700 tq(pos)=3.700
//! B3| pos=3.7 f=0.9999999 d=3: raw=3.69999963 tq(raw)=3.699 tq(pos)=3.7
//! B3| pos=7.7777 f=1 d=2: raw=7.7777 tq(raw)=7.77 tq(pos)=7.77
//! C| policy notional boundaries (strict > semantics)
//! C| reduce 0.5 x |200| x 1 = 100.0
//! C| reduce 0.50005 x |200| x 1 = 100.01000
//! C| ulp: 0.5 x 2.000000000000000000000000002 x 100 = 100.0000000000000000000000001000
//! C| equal: 0.5 x 2 x 100 = 100.0
//! C| close: |200| x 1 = 200
//! C| close above: 200.01 x 1 = 200.01
//! C| collateral amount 100 = 100
//! ```
//!
//! Line-tag map: `A|` quantize table ([`quantize_size_down`]), `B|` sizing
//! table ([`reduce_by_fraction`]), `B2|` fraction bounds, `B3|` clamp,
//! `C|` policy notional boundaries (rule 5–7).

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sentinel_core::order::{CloseSide, OrderType, quantize_size_down, reduce_by_fraction};
use sentinel_core::policy::{DayState, PolicyConfig, PolicyContext, PolicyEngine, PolicySource};
use sentinel_core::types::{
    AccountState, Intent, Market, MarketId, PolicyVerdict, Position, RiskTier,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse an exact literal (no float round-trip, used for the 28-digit cases).
fn exact(s: &str) -> Decimal {
    Decimal::from_str_exact(s).expect("test literal must parse exactly")
}

/// Build a flat-account position on `market_id` (other fields irrelevant to P05).
fn pos(market_id: u32, size: Decimal, mark: Option<Decimal>) -> Position {
    Position {
        market_id: MarketId(market_id),
        symbol: "ETH".into(),
        size,
        entry_price: dec!(100),
        mark_price: mark,
        liq_price: None,
        collateral: dec!(50),
        unrealized_pnl: dec!(0),
        margin_ratio: None,
        leverage: dec!(5),
        opened_at: None,
    }
}

/// Account snapshot carrying the given positions (balances irrelevant to P05).
fn account(positions: Vec<Position>) -> AccountState {
    AccountState {
        positions,
        free_balance: dec!(100),
        equity: dec!(150),
        fee_tier: 0,
        snapshot_ts: Utc::now(),
    }
}

/// Market with the given lot grid and minimum order size (P05-relevant fields).
fn market(id: u32, size_decimals: u32, min_size: Decimal) -> Market {
    Market {
        id: MarketId(id),
        symbol: "ETH".into(),
        base: "ETH".into(),
        price_decimals: 2,
        size_decimals,
        initial_margin_fraction: exact("0.0833333333333333333333333333"),
        maintenance_margin_fraction: dec!(0.05),
        max_leverage: dec!(12),
        min_size,
        tick_size: dec!(0.01),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

/// Policy config builder (`allow` = allowlisted market ids).
fn cfg(allow: &[u32], cap: Decimal, approval: Decimal, max_daily: u32, kill: bool) -> PolicyConfig {
    PolicyConfig {
        market_allowlist: allow.iter().copied().map(MarketId).collect(),
        max_order_size_usd: cap,
        max_daily_actions: max_daily,
        require_approval_above_usd: approval,
        kill_switch: kill,
    }
}

fn ctx(source: PolicySource, tier: RiskTier, market_id: u32) -> PolicyContext {
    PolicyContext {
        source,
        tier,
        market_id: MarketId(market_id),
    }
}

/// Evaluate one intent against one account snapshot and one day counter.
fn eval(
    intent: &Intent,
    positions: Vec<Position>,
    cfg: &PolicyConfig,
    actions_today: u32,
    ctx: PolicyContext,
) -> PolicyVerdict {
    PolicyEngine::evaluate(
        intent,
        &account(positions),
        cfg,
        &DayState { actions_today },
        &ctx,
    )
}

fn allow() -> PolicyVerdict {
    PolicyVerdict::Allow
}

fn deny(reason: &str) -> PolicyVerdict {
    PolicyVerdict::Deny {
        reason: reason.into(),
    }
}

fn approval(reason: &str) -> PolicyVerdict {
    PolicyVerdict::NeedsApproval {
        reason: reason.into(),
    }
}

fn reduce(fraction: Decimal) -> Intent {
    Intent::Reduce {
        fraction,
        reason: "test".into(),
    }
}

fn close() -> Intent {
    Intent::Close {
        reason: "test".into(),
    }
}

fn collateral(amount: Decimal) -> Intent {
    Intent::AddCollateral {
        amount,
        reason: "test".into(),
    }
}

fn alert() -> Intent {
    Intent::Alert {
        message: "test".into(),
    }
}

/// The three-outcome reference: exact reason strings are normative (SPEC §3).
const REASON_KILL: &str = "kill switch engaged";
const REASON_BIAS_REDUCE: &str = "invalid reduce fraction";
const REASON_FLAT: &str = "position already flat";
const REASON_COLLATERAL: &str = "collateral amount must be positive";
const REASON_MARK: &str = "mark unavailable for notional";
const REASON_CAP: &str = "notional exceeds MAX_ORDER_SIZE_USD";
const REASON_APPROVAL: &str = "notional above approval threshold";
const REASON_DAILY: &str = "daily action cap reached";

// ---------------------------------------------------------------------------
// Sizing (spec §4)
// ---------------------------------------------------------------------------

/// `A|` — lot-grid quantization truncates towards zero at `10^-decimals`.
#[test]
fn quantize_table_python_derived() {
    // A| tq(0.1239 @ 3) = 0.123
    assert_eq!(quantize_size_down(dec!(0.1239), 3), dec!(0.123));
    // A| tq(1.9999 @ 2) = 1.99
    assert_eq!(quantize_size_down(dec!(1.9999), 2), dec!(1.99));
    // A| tq(0.999 @ 2) = 0.99
    assert_eq!(quantize_size_down(dec!(0.999), 2), dec!(0.99));
    // A| tq(12.3456 @ 0) = 12
    assert_eq!(quantize_size_down(dec!(12.3456), 0), dec!(12));
    // A| tq(0.005 @ 2) = 0.00
    assert_eq!(quantize_size_down(dec!(0.005), 2), dec!(0));
    // A| tq(5 @ 2) = 5 (exact lot values are identity)
    assert_eq!(quantize_size_down(dec!(5), 2), dec!(5));
    // A| tq(2.50 @ 1) = 2.5
    assert_eq!(quantize_size_down(exact("2.50"), 1), dec!(2.5));
    // "truncates towards zero" applies to negatives too (spec §4):
    // A| tq(-0.1239 @ 3) = -0.123
    assert_eq!(quantize_size_down(dec!(-0.1239), 3), dec!(-0.123));
    // A| tq(-1.9999 @ 2) = -1.99
    assert_eq!(quantize_size_down(dec!(-1.9999), 2), dec!(-1.99));
}

/// `B|` — full sizing pipeline: quantization, min-size skip, clamp.
#[test]
fn sizing_table_python_derived() {
    // B| pos=4 f=0.25 d=3 min=0.001 -> 1.00
    let m = market(32, 3, dec!(0.001));
    let o = reduce_by_fraction(&pos(32, dec!(4), Some(dec!(100))), dec!(0.25), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(1));

    // B| pos=0.1239 f=1 d=3 min=0 -> 0.123
    let m = market(32, 3, dec!(0));
    let o = reduce_by_fraction(&pos(32, dec!(0.1239), Some(dec!(100))), dec!(1), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(0.123));

    // B| pos=1.9999 f=1 d=2 min=0.01 -> 1.99
    let m = market(32, 2, dec!(0.01));
    let o = reduce_by_fraction(&pos(32, dec!(1.9999), Some(dec!(100))), dec!(1), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(1.99));

    // B| pos=1.0101 f=0.9 d=3 min=0 -> 0.909
    let m = market(32, 3, dec!(0));
    let o = reduce_by_fraction(&pos(32, dec!(1.0101), Some(dec!(100))), dec!(0.9), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(0.909));

    // B| pos=3.7001 f=1 d=3 min=0 -> 3.700 (B3 clamp line: tq(raw)=3.700=tq(pos))
    let m = market(32, 3, dec!(0));
    let o = reduce_by_fraction(&pos(32, dec!(3.7001), Some(dec!(100))), dec!(1), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(3.7));

    // B| pos=3.7 f=0.9999999 d=3 min=0 -> 3.699 (B3: tq(raw)=3.699)
    let m = market(32, 3, dec!(0));
    let o = reduce_by_fraction(
        &pos(32, dec!(3.7), Some(dec!(100))),
        exact("0.9999999"),
        &m,
        10,
    )
    .expect("valid reduce must size");
    assert_eq!(o.size, dec!(3.699));

    // B| pos=7.7777 f=1 d=2 min=1 -> 7.77
    let m = market(32, 2, dec!(1));
    let o = reduce_by_fraction(&pos(32, dec!(7.7777), Some(dec!(100))), dec!(1), &m, 10)
        .expect("valid reduce must size");
    assert_eq!(o.size, dec!(7.77));
}

/// Min-size boundary: passes at exactly `min_size`, skips one lot below
/// (`B|` pos=0.01 → 0.01 vs pos=0.009 → None).
#[test]
fn sizing_min_size_boundary() {
    let m = market(32, 3, dec!(0.01));
    let pass = reduce_by_fraction(&pos(32, dec!(0.01), Some(dec!(100))), dec!(1), &m, 10);
    assert_eq!(
        pass.expect("exactly min_size must pass").size,
        dec!(0.01),
        "B| pos=0.01 f=1 d=3 min=0.01 -> 0.01 (equal passes)"
    );

    let below = reduce_by_fraction(&pos(32, dec!(0.009), Some(dec!(100))), dec!(1), &m, 10);
    assert!(below.is_none(), "B| pos=0.009 -> None (one lot below)");

    // min_size 0 disables the skip (venue without a minimum; Market.min_size doc).
    let m0 = market(32, 3, dec!(0));
    assert!(reduce_by_fraction(&pos(32, dec!(0.001), Some(dec!(100))), dec!(1), &m0, 10).is_some());
}

/// `B2|` — fraction bounds and zero-size skips.
#[test]
fn sizing_fraction_bounds_and_zero_skips() {
    let m = market(32, 3, dec!(0));
    let p = pos(32, dec!(4), Some(dec!(100)));
    // B2| f=0 -> None · f=-0.5 -> None · f=2 -> None
    assert!(reduce_by_fraction(&p, dec!(0), &m, 10).is_none());
    assert!(reduce_by_fraction(&p, dec!(-0.5), &m, 10).is_none());
    assert!(reduce_by_fraction(&p, dec!(2), &m, 10).is_none());
    // B2| f=1.0000000000000000000000001 -> None (strictly > 1 is invalid)
    let f_plus = exact("1.0000000000000000000000001");
    assert!(reduce_by_fraction(&p, f_plus, &m, 10).is_none());
    // B2| f=1 -> 4
    assert_eq!(
        reduce_by_fraction(&p, dec!(1), &m, 10)
            .expect("f=1 must size")
            .size,
        dec!(4)
    );
    // B| pos=0 -> None (flat position, any fraction)
    assert!(reduce_by_fraction(&pos(32, dec!(0), Some(dec!(100))), dec!(0.5), &m, 10).is_none());
    // B| pos=0.0004 f=1 d=3 -> None (quantizes to zero)
    assert!(reduce_by_fraction(&pos(32, dec!(0.0004), Some(dec!(100))), dec!(1), &m, 10).is_none());
}

/// Side mapping + `size_decimals` carried + default market order type.
#[test]
fn sizing_side_mapping_and_decimals() {
    // B| pos=2.5 f=0.5 -> 1.25 (CloseLong) · pos=-2.5 -> 1.25 (CloseShort)
    let m = market(32, 3, dec!(0.001));
    let long = reduce_by_fraction(&pos(32, dec!(2.5), Some(dec!(100))), dec!(0.5), &m, 25)
        .expect("long reduce must size");
    assert_eq!(long.close, CloseSide::CloseLong);
    assert_eq!(
        long.size,
        dec!(1.25),
        "size is in |real| units, always positive"
    );
    assert_eq!(long.market_id, MarketId(32));
    assert_eq!(long.max_slippage_bps, 25, "slippage cap carried");
    assert_eq!(long.order_type, OrderType::Market, "Market is the default");

    let short = reduce_by_fraction(&pos(32, dec!(-2.5), Some(dec!(100))), dec!(0.5), &m, 25)
        .expect("short reduce must size");
    assert_eq!(short.close, CloseSide::CloseShort);
    assert_eq!(short.size, dec!(1.25));

    // B| pos=4 f=0.25 d=4 -> 1.00, and size_decimals carried from the market
    let m4 = market(32, 4, dec!(0));
    let o4 = reduce_by_fraction(&pos(32, dec!(4), Some(dec!(100))), dec!(0.25), &m4, 10)
        .expect("valid reduce must size");
    assert_eq!(o4.size_decimals, 4);
    assert_eq!(o4.size, dec!(1));
}

/// Reduce-only bias invariant (spec §4): no (size, fraction, decimals)
/// combination can produce an order larger than the position's own quantized
/// size, a zero size, or a side flip.
#[test]
fn sizing_never_increases_position() {
    let fractions = [
        dec!(0.0001),
        dec!(0.1),
        dec!(0.25),
        dec!(0.5),
        dec!(0.999),
        dec!(1),
    ];
    let sizes = [
        dec!(0.0001),
        dec!(0.009),
        dec!(1.0101),
        dec!(3.7001),
        dec!(12.345678),
        dec!(-0.1239),
        dec!(-7.7777),
    ];
    for d in [0u32, 2, 3, 5] {
        let m = market(32, d, dec!(0));
        for size in sizes {
            let p = pos(32, size, Some(dec!(100)));
            let cap = quantize_size_down(size.abs(), d);
            for f in fractions {
                if let Some(o) = reduce_by_fraction(&p, f, &m, 10) {
                    assert!(o.size > Decimal::ZERO, "order size must be positive");
                    assert!(
                        o.size <= cap,
                        "order {o:?} exceeds quantized |pos| {cap} (d={d}, size={size}, f={f})"
                    );
                    assert!(o.size <= size.abs(), "order must never exceed |pos|");
                    assert_eq!(
                        o.close,
                        if size > Decimal::ZERO {
                            CloseSide::CloseLong
                        } else {
                            CloseSide::CloseShort
                        },
                        "side must follow the sign of the position"
                    );
                    assert_eq!(o.size_decimals, d);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Policy gate (spec §3)
// ---------------------------------------------------------------------------

/// Rule 1 — the kill switch denies every intent variant (three-outcome: deny).
#[test]
fn kill_switch_denies_every_intent() {
    let c = cfg(&[32], dec!(1000), dec!(400), 5, true);
    let p = vec![pos(32, dec!(2), Some(dec!(100)))];
    for intent in [reduce(dec!(0.5)), close(), collateral(dec!(10)), alert()] {
        assert_eq!(
            eval(
                &intent,
                p.clone(),
                &c,
                0,
                ctx(PolicySource::Reflex, RiskTier::Green, 32)
            ),
            deny(REASON_KILL),
            "kill switch must deny {intent:?}"
        );
    }
}

/// Rule 2 — allowlist denies (including Alert), kills precedence over the
/// allowlist, and allowlist hit lets the position lookup fail next.
#[test]
fn allowlist_rule_and_precedence() {
    let c = cfg(&[32, 16], dec!(1000), dec!(400), 5, false);
    let p = vec![pos(32, dec!(2), Some(dec!(100)))];
    // Deny: market 33 is not listed — every intent, Alert included.
    for intent in [reduce(dec!(0.5)), close(), collateral(dec!(10)), alert()] {
        assert_eq!(
            eval(
                &intent,
                p.clone(),
                &c,
                0,
                ctx(PolicySource::Strategy, RiskTier::Green, 33)
            ),
            deny("market 33 not in allowlist"),
            "allowlist must apply to {intent:?}"
        );
    }
    // Precedence kill ≻ allowlist: both violated ⇒ kill reason wins.
    let killed = cfg(&[32], dec!(1000), dec!(400), 5, true);
    assert_eq!(
        eval(
            &reduce(dec!(0.5)),
            p.clone(),
            &killed,
            0,
            ctx(PolicySource::Reflex, RiskTier::Red, 33)
        ),
        deny(REASON_KILL)
    );
    // Precedence allowlist ≻ position: market listed, position entry missing
    // for a *different* market than the one requested ⇒ still the allowlist
    // reason when the market is not listed.
    assert_eq!(
        eval(
            &reduce(dec!(0.5)),
            vec![pos(99, dec!(2), Some(dec!(100)))],
            &c,
            0,
            ctx(PolicySource::Strategy, RiskTier::Green, 20)
        ),
        deny("market 20 not in allowlist")
    );
}

/// Rule 3 — position resolution; Alert skips 3–7 (and the day cap is not an
/// execution gate for an informational intent).
#[test]
fn position_resolution_and_alert_short_circuit() {
    let c = cfg(&[32], dec!(1000), dec!(400), 5, false);
    // Missing position ⇒ deny for the three action variants.
    for intent in [reduce(dec!(0.5)), close(), collateral(dec!(10))] {
        assert_eq!(
            eval(
                &intent,
                vec![],
                &c,
                0,
                ctx(PolicySource::Reflex, RiskTier::Green, 32)
            ),
            deny("no position for market 32"),
            "missing position must deny {intent:?}"
        );
    }
    // A position for another market does not satisfy the lookup.
    assert_eq!(
        eval(
            &reduce(dec!(0.5)),
            vec![pos(99, dec!(2), Some(dec!(100)))],
            &c,
            0,
            ctx(PolicySource::Reflex, RiskTier::Green, 32)
        ),
        deny("no position for market 32")
    );
    // Alert skips rules 3–7 ⇒ Allow, regardless of position/day state.
    assert_eq!(
        eval(
            &alert(),
            vec![],
            &c,
            5,
            ctx(PolicySource::Manual, RiskTier::Red, 32)
        ),
        allow(),
        "Alert must not require a position"
    );
    // …but rules 1–2 still bound it (checked in the tests above).
}

/// Rule 4 — reduce-only bias: invalid fractions, flat close, non-positive
/// collateral; and the enumeration invariant that only `fraction ∈ (0, 1]`
/// can ever reach a non-deny verdict.
#[test]
fn reduce_only_bias_rule() {
    let c = cfg(&[32], dec!(1000), dec!(400), 5, false);
    let p = vec![pos(32, dec!(2), Some(dec!(100)))];
    let r = ctx(PolicySource::Reflex, RiskTier::Green, 32);
    // Invalid fractions.
    for f in [
        dec!(0),
        dec!(-0.5),
        dec!(2),
        exact("1.0000000000000000000000001"),
    ] {
        assert_eq!(
            eval(&reduce(f), p.clone(), &c, 0, r),
            deny(REASON_BIAS_REDUCE),
            "fraction {f} must be invalid"
        );
    }
    // Precedence rule 4 ≻ rule 6: a fraction of 2 would blow the cap, yet the
    // fraction reason wins.
    let tight = cfg(&[32], dec!(1), dec!(0), 5, false);
    assert_eq!(
        eval(&reduce(dec!(2)), p.clone(), &tight, 0, r),
        deny(REASON_BIAS_REDUCE)
    );
    // Close on a flat position.
    let flat = vec![pos(32, dec!(0), Some(dec!(100)))];
    assert_eq!(eval(&close(), flat.clone(), &c, 0, r), deny(REASON_FLAT));
    // AddCollateral on a flat position is *not* forbidden (only Close is).
    assert_eq!(eval(&collateral(dec!(10)), flat, &c, 0, r), allow());
    // Non-positive collateral.
    for amount in [dec!(0), dec!(-5)] {
        assert_eq!(
            eval(&collateral(amount), p.clone(), &c, 0, r),
            deny(REASON_COLLATERAL),
            "amount {amount} must be rejected"
        );
    }
    // Enumeration: non-deny verdicts imply fraction ∈ (0, 1].
    for f in [
        dec!(0),
        dec!(-1),
        dec!(0.0001),
        dec!(0.25),
        dec!(1),
        dec!(1.5),
        dec!(1000000),
    ] {
        let v = eval(&reduce(f), p.clone(), &c, 0, r);
        let valid = f > Decimal::ZERO && f <= Decimal::ONE;
        if valid {
            assert_ne!(v, deny(REASON_BIAS_REDUCE), "f={f} is valid");
        } else {
            assert_eq!(v, deny(REASON_BIAS_REDUCE), "f={f} must be invalid");
        }
    }
}

/// Rule 5 — notional + mark availability; Rule 6 — cap (strict `>`), with the
/// Python-derived boundary table `C|`.
#[test]
fn notional_mark_and_cap_boundaries() {
    let r = ctx(PolicySource::Strategy, RiskTier::Green, 32);
    // Mark missing / <= 0 blocks Reduce and Close with the mark reason.
    let nomark = vec![pos(32, dec!(2), None)];
    let zeromark = vec![pos(32, dec!(2), Some(dec!(0)))];
    for positions in [nomark.clone(), zeromark.clone()] {
        assert_eq!(
            eval(
                &reduce(dec!(0.5)),
                positions.clone(),
                &cfg(&[32], dec!(1000), dec!(400), 5, false),
                0,
                r
            ),
            deny(REASON_MARK)
        );
        assert_eq!(
            eval(
                &close(),
                positions.clone(),
                &cfg(&[32], dec!(1000), dec!(400), 5, false),
                0,
                r
            ),
            deny(REASON_MARK)
        );
    }
    // AddCollateral does not need a mark (notional = amount, rule 5).
    assert_eq!(
        eval(
            &collateral(dec!(10)),
            nomark,
            &cfg(&[32], dec!(1000), dec!(400), 5, false),
            0,
            r
        ),
        allow()
    );
    // C| reduce 0.5 x |200| x 1 = 100.0 — exactly at cap and approval passes.
    let p200 = vec![pos(32, dec!(200), Some(dec!(1)))];
    let c = cfg(&[32], dec!(100), dec!(100), 5, false);
    assert_eq!(eval(&reduce(dec!(0.5)), p200.clone(), &c, 0, r), allow());
    // C| reduce 0.50005 x |200| x 1 = 100.01 > cap ⇒ deny (strict >).
    assert_eq!(
        eval(&reduce(exact("0.50005")), p200.clone(), &c, 0, r),
        deny(REASON_CAP)
    );
    // C| close: |200| x 1 = 200 exactly at a 200 cap/approval passes.
    let cc = cfg(&[32], dec!(200), dec!(200), 5, false);
    assert_eq!(eval(&close(), p200.clone(), &cc, 0, r), allow());
    // C| close above: 200.01 x 1 = 200.01 ⇒ deny.
    let p200_01 = vec![pos(32, exact("200.01"), Some(dec!(1)))];
    assert_eq!(eval(&close(), p200_01, &cc, 0, r), deny(REASON_CAP));
    // C| collateral amount 100 exactly at cap/approval passes; over denies.
    assert_eq!(
        eval(
            &collateral(dec!(100)),
            vec![pos(32, dec!(2), Some(dec!(1)))],
            &c,
            0,
            r
        ),
        allow()
    );
    assert_eq!(
        eval(
            &collateral(exact("100.01")),
            vec![pos(32, dec!(2), Some(dec!(1)))],
            &c,
            0,
            r
        ),
        deny(REASON_CAP)
    );
    // One-ulp boundary: C| 0.5 x 2.000000000000000000000000002 x 100 =
    // 100.0000000000000000000000001 ⇒ strictly above a 100 cap.
    let ulp_pos = vec![pos(
        32,
        exact("2.000000000000000000000000002"),
        Some(dec!(100)),
    )];
    assert_eq!(
        eval(&reduce(dec!(0.5)), ulp_pos, &c, 0, r),
        deny(REASON_CAP)
    );
}

/// Rule 7 — approval threshold (strict `>`), and the deliberate REFLEX+Red
/// asymmetry: it bypasses approval only; the cap and the daily cap still deny.
#[test]
fn approval_threshold_and_reflex_red_asymmetry() {
    // Equal to the approval threshold passes for every source/tier.
    let p200 = vec![pos(32, dec!(200), Some(dec!(1)))];
    let c = cfg(&[32], dec!(1000), dec!(100), 5, false);
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
            assert_eq!(
                eval(
                    &reduce(dec!(0.5)),
                    p200.clone(),
                    &c,
                    0,
                    ctx(source, tier, 32)
                ),
                allow(),
                "notional == approval threshold must pass ({source:?}/{tier:?})"
            );
        }
    }
    // Above the threshold ⇒ NeedsApproval, except source=Reflex && tier=Red.
    let above = reduce(exact("0.50005")); // 100.01 > 100 (C|)
    for source in [
        PolicySource::Reflex,
        PolicySource::Strategy,
        PolicySource::Manual,
    ] {
        for tier in [RiskTier::Green, RiskTier::Yellow, RiskTier::Orange] {
            assert_eq!(
                eval(&above, p200.clone(), &c, 0, ctx(source, tier, 32)),
                approval(REASON_APPROVAL),
                "{source:?}/{tier:?} below Red must need approval"
            );
        }
    }
    assert_eq!(
        eval(
            &above,
            p200.clone(),
            &c,
            0,
            ctx(PolicySource::Strategy, RiskTier::Red, 32)
        ),
        approval(REASON_APPROVAL),
        "Strategy+Red has no asymmetry"
    );
    assert_eq!(
        eval(
            &above,
            p200.clone(),
            &c,
            0,
            ctx(PolicySource::Manual, RiskTier::Red, 32)
        ),
        approval(REASON_APPROVAL),
        "Manual+Red has no asymmetry"
    );
    assert_eq!(
        eval(
            &above,
            p200.clone(),
            &c,
            0,
            ctx(PolicySource::Reflex, RiskTier::Red, 32)
        ),
        allow(),
        "Reflex+Red bypasses approval above the threshold"
    );
    // …the bypass reaches ONLY approval: cap first (rule 6 ≻ rule 7).
    let c_small = cfg(&[32], dec!(50), dec!(10), 5, false);
    assert_eq!(
        eval(
            &above,
            p200.clone(),
            &c_small,
            0,
            ctx(PolicySource::Reflex, RiskTier::Red, 32)
        ),
        deny(REASON_CAP)
    );
    // …and the daily cap still denies it (rule 8 after the bypass).
    let c_ok = cfg(&[32], dec!(1000), dec!(100), 5, false);
    assert_eq!(
        eval(
            &above,
            p200.clone(),
            &c_ok,
            5,
            ctx(PolicySource::Reflex, RiskTier::Red, 32)
        ),
        deny(REASON_DAILY)
    );
}

/// Rule 8 — daily action cap (`>=` denies) and its place in the precedence
/// chain: rule 6 ≻ rule 7, and rule 7 ≻ rule 8 for non-Reflex sources.
#[test]
fn daily_cap_rule_and_precedence_chain() {
    let p200 = vec![pos(32, dec!(200), Some(dec!(1)))];
    let c = cfg(&[32], dec!(1000), dec!(100), 5, false);
    let r = ctx(PolicySource::Strategy, RiskTier::Green, 32);
    // Below the cap: normal verdicts.
    assert_eq!(eval(&reduce(dec!(0.5)), p200.clone(), &c, 4, r), allow());
    // Reaching the cap denies even an otherwise-allowed intent.
    assert_eq!(
        eval(&reduce(dec!(0.5)), p200.clone(), &c, 5, r),
        deny(REASON_DAILY)
    );
    // Precedence cap ≻ approval: over both ⇒ cap reason (rule 6 first).
    let tight = cfg(&[32], dec!(50), dec!(10), 5, false);
    assert_eq!(
        eval(&reduce(exact("0.50005")), p200.clone(), &tight, 0, r),
        deny(REASON_CAP)
    );
    // Precedence rule 7 ≻ rule 8: above approval AND at the daily cap ⇒ the
    // first failure (approval) wins for a non-bypassing source.
    assert_eq!(
        eval(&reduce(exact("0.50005")), p200.clone(), &c, 5, r),
        approval(REASON_APPROVAL)
    );
    // Reflex+Red (approval bypassed) reaches rule 8 ⇒ daily cap denies.
    assert_eq!(
        eval(
            &reduce(exact("0.50005")),
            p200.clone(),
            &c,
            5,
            ctx(PolicySource::Reflex, RiskTier::Red, 32)
        ),
        deny(REASON_DAILY)
    );
    // Alert is informational: the day counter does not gate it.
    assert_eq!(eval(&alert(), p200.clone(), &c, 5, r), allow());
}

/// Rules 4–7 across the short side (`size < 0`): |size| drives the notional
/// exactly like the long side.
#[test]
fn short_side_notional_symmetry() {
    let r = ctx(PolicySource::Strategy, RiskTier::Green, 32);
    // |−200| x 1 = 200; close at a 200 cap/approval passes (boundary).
    let short = vec![pos(32, dec!(-200), Some(dec!(1)))];
    let c = cfg(&[32], dec!(200), dec!(200), 5, false);
    assert_eq!(eval(&close(), short.clone(), &c, 0, r), allow());
    // reduce 0.5 x |−200| x 1 = 100 exactly at a 100 cap/approval passes.
    let c100 = cfg(&[32], dec!(100), dec!(100), 5, false);
    assert_eq!(
        eval(&reduce(dec!(0.5)), short.clone(), &c100, 0, r),
        allow()
    );
    // 0.50005 x 200 = 100.01 > 100 ⇒ cap deny (sign never enters the notional).
    assert_eq!(
        eval(&reduce(exact("0.50005")), short.clone(), &c100, 0, r),
        deny(REASON_CAP)
    );
    // Approval strictness across the short side.
    let ca = cfg(&[32], dec!(1000), dec!(100), 5, false);
    assert_eq!(eval(&reduce(dec!(0.5)), short.clone(), &ca, 0, r), allow());
    assert_eq!(
        eval(&reduce(exact("0.50005")), short, &ca, 0, r),
        approval(REASON_APPROVAL)
    );
}
