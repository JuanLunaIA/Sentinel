//! Adversarial suite for the per-market budget layer and the policy
//! fingerprint (GAP-029 / GAP-024).
//!
//! Properties that matter, in order:
//!
//! 1. **Behaviour preservation.** With empty override maps, `BudgetLimits`
//!    reproduces the global values exactly, so adopting the ledger cannot
//!    change the frozen `P05` §3 gate's outcome for any existing config.
//! 2. **Binding.** A per-market cap binds only its own market; the book
//!    ceiling binds every market.
//! 3. **Purity.** `check` never mutates; committing is explicit.
//! 4. **Fingerprint injectivity.** Two different policies never collide, and
//!    every field changes the digest.

use std::collections::BTreeMap;

use rust_decimal_macros::dec;
use sentinel_core::policy::{
    BudgetLedger, BudgetLimits, BudgetVerdict, PolicyConfig, policy_fingerprint,
};
use sentinel_core::types::MarketId;

const ETH: MarketId = MarketId(32);
const BTC: MarketId = MarketId(16);

fn base_limits() -> BudgetLimits {
    BudgetLimits {
        global_max_order_size_usd: dec!(1000),
        per_market_max_order_size_usd: BTreeMap::new(),
        global_daily_actions: 10,
        per_market_daily_actions: BTreeMap::new(),
        max_book_daily_actions: None,
    }
}

fn config() -> PolicyConfig {
    PolicyConfig {
        market_allowlist: vec![ETH, BTC],
        max_order_size_usd: dec!(1000),
        max_daily_actions: 10,
        require_approval_above_usd: dec!(500),
        kill_switch: false,
    }
}

#[test]
fn empty_overrides_reproduce_the_globals_exactly() {
    // The adoption-safety property: every market sees the global values.
    let limits = base_limits();
    for market in [ETH, BTC, MarketId(48)] {
        assert_eq!(limits.order_cap(market), dec!(1000), "order cap");
        assert_eq!(limits.daily_cap(market), 10, "daily cap");
    }
}

#[test]
fn a_per_market_cap_binds_only_its_own_market() {
    let mut limits = base_limits();
    limits.per_market_max_order_size_usd.insert(ETH, dec!(250));
    assert_eq!(limits.order_cap(ETH), dec!(250), "ETH is tightened");
    assert_eq!(limits.order_cap(BTC), dec!(1000), "BTC keeps the global");

    limits.per_market_daily_actions.insert(ETH, 2);
    assert_eq!(limits.daily_cap(ETH), 2);
    assert_eq!(limits.daily_cap(BTC), 10);
}

#[test]
fn the_book_ceiling_binds_every_market_and_wins_when_tighter() {
    let mut limits = base_limits();
    limits.max_book_daily_actions = Some(3);
    assert_eq!(limits.daily_cap(ETH), 3, "book ceiling applies to ETH");
    assert_eq!(limits.daily_cap(BTC), 3, "book ceiling applies to BTC");

    // A market cap tighter than the book ceiling still wins for its market.
    limits.per_market_daily_actions.insert(ETH, 1);
    assert_eq!(limits.daily_cap(ETH), 1);
    assert_eq!(limits.daily_cap(BTC), 3);
}

#[test]
fn a_correlated_cascade_cannot_burn_the_book_on_one_market() {
    // The [P-1] finding, reproduced. Semantics worth being explicit about:
    // `global_daily_actions` is the *per-market* default, so it alone does not
    // stop one market from spending everything it wants; the book-wide limit
    // is `max_book_daily_actions`.
    let mut limits = base_limits();
    limits.global_daily_actions = 4;
    let mut ledger = BudgetLedger::default();
    for _ in 0..4 {
        assert_eq!(ledger.record(ETH, &limits), BudgetVerdict::Ok);
    }
    assert_eq!(
        ledger.check(BTC, &limits),
        BudgetVerdict::Ok,
        "a per-market default cap leaves BTC untouched; that is the [P-1] hole"
    );

    // Closing the hole: a book ceiling plus per-market caps.
    let mut limits = base_limits();
    limits.global_daily_actions = 10;
    limits.per_market_daily_actions.insert(ETH, 2);
    limits.max_book_daily_actions = Some(5);
    let mut ledger = BudgetLedger::default();

    assert_eq!(ledger.record(ETH, &limits), BudgetVerdict::Ok);
    assert_eq!(ledger.record(ETH, &limits), BudgetVerdict::Ok);
    assert_eq!(
        ledger.record(ETH, &limits),
        BudgetVerdict::MarketDailyExhausted {
            market_id: ETH,
            cap: 2
        },
        "ETH's own cap binds before the book ceiling"
    );
    assert_eq!(ledger.total_actions(), 2, "a refused action spends nothing");
    assert_eq!(ledger.record(BTC, &limits), BudgetVerdict::Ok);
    assert_eq!(ledger.record(BTC, &limits), BudgetVerdict::Ok);
    assert_eq!(ledger.record(BTC, &limits), BudgetVerdict::Ok);
    assert_eq!(
        ledger.record(BTC, &limits),
        BudgetVerdict::BookDailyExhausted { cap: 5 },
        "the book ceiling binds across markets"
    );
    assert_eq!(ledger.total_actions(), 5);
}

#[test]
fn check_is_pure_and_commit_is_explicit() {
    let limits = base_limits();
    let ledger = BudgetLedger::default();
    for _ in 0..20 {
        assert_eq!(ledger.check(ETH, &limits), BudgetVerdict::Ok);
    }
    assert_eq!(ledger.total_actions(), 0, "check never mutates");

    let mut ledger = BudgetLedger::default();
    ledger.record(ETH, &limits);
    assert_eq!(ledger.actions_for(ETH), 1);
    assert_eq!(ledger.actions_for(BTC), 0, "per-market accounting");
    assert_eq!(ledger.total_actions(), 1);
}

#[test]
fn a_refused_action_is_not_recorded() {
    let mut limits = base_limits();
    limits.per_market_daily_actions.insert(ETH, 1);
    let mut ledger = BudgetLedger::default();
    assert_eq!(ledger.record(ETH, &limits), BudgetVerdict::Ok);
    assert_eq!(
        ledger.record(ETH, &limits),
        BudgetVerdict::MarketDailyExhausted {
            market_id: ETH,
            cap: 1
        }
    );
    assert_eq!(ledger.actions_for(ETH), 1, "the refusal did not count");
}

#[test]
fn reset_clears_the_day() {
    let limits = base_limits();
    let mut ledger = BudgetLedger::default();
    ledger.record(ETH, &limits);
    ledger.record(BTC, &limits);
    assert_eq!(ledger.total_actions(), 2);
    ledger.reset();
    assert_eq!(ledger.total_actions(), 0);
    assert_eq!(ledger.actions_for(ETH), 0);
}

#[test]
fn fingerprint_is_deterministic_and_field_sensitive() {
    let base = config();
    let digest = policy_fingerprint(&base);
    assert_eq!(digest.len(), 64, "sha256 hex");
    assert_eq!(digest, policy_fingerprint(&base), "deterministic");

    let mut other = base.clone();
    other.max_order_size_usd = dec!(1001);
    assert_ne!(policy_fingerprint(&other), digest, "order cap");

    let mut other = base.clone();
    other.max_daily_actions = 11;
    assert_ne!(policy_fingerprint(&other), digest, "daily cap");

    let mut other = base.clone();
    other.require_approval_above_usd = dec!(501);
    assert_ne!(policy_fingerprint(&other), digest, "approval threshold");

    let mut other = base.clone();
    other.kill_switch = true;
    assert_ne!(policy_fingerprint(&other), digest, "kill switch");

    let mut other = base.clone();
    other.market_allowlist = vec![ETH];
    assert_ne!(policy_fingerprint(&other), digest, "allowlist");
}

#[test]
fn fingerprint_encoding_is_unambiguous() {
    // Same discipline as the inference commitment: a policy whose allowlist
    // renders as "32,16" must not collide with one whose cap renders into the
    // same position. Length framing makes the field boundaries explicit.
    let mut a = config();
    a.market_allowlist = vec![MarketId(3), MarketId(2)];
    a.max_order_size_usd = dec!(16);
    let mut b = config();
    b.market_allowlist = vec![MarketId(32)];
    b.max_order_size_usd = dec!(16);
    assert_ne!(policy_fingerprint(&a), policy_fingerprint(&b));
}
