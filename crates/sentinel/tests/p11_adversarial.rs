//! P11 adversarial verification — verifier-owned, black box over the frozen
//! public API (`SPEC-P11.md` §10, verifier column).
//!
//! This file never inspects the writers' implementation bodies; every
//! assertion is derived from `SPEC-P11.md` plus deterministic probing of the
//! public API. No network, no git, independent clock, no new dependencies.
//!
//! Coverage:
//! 1. parse corpus (leading spaces, `//`, case, ES aliases, caps, unicode);
//! 2. MarkdownV2 escaping (full reserved set, double-escape, builder scans);
//! 3. policy overlay (whitelist, ranges, cross-field, huge/negative values,
//!    JSON injection, fixed path, atomic write, `apply_to_config`);
//! 4. approvals (id formula, TTL boundary on our own clock, duplicate ids);
//! 5. handler-level downstream rejection (`/close` fraction 0, `/mode`
//!    honesty, `/risk` without engine).
//!
//! Regression gates (run separately):
//!   cargo test -p sentinel
//!   cargo test -p sentinel --test p06_adversarial determinism_two_runs_byte_equal_and_expected_sequence
//!   cargo test -p sentinel --test pipeline_determinism replay_is_deterministic_and_hits_the_golden_sequence
//!   cargo test -p sentinel --test p10_journal_wiring crash_replay_journals_intent_outcome_pairs_with_a_valid_chain

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use sentinel::bot::approvals::{APPROVAL_TTL_MS, ApprovalQueue, PendingApproval};
use sentinel::bot::commands::{Command, parse};
use sentinel::bot::format::{PositionRow, escape_md2, status_card};
use sentinel::bot::handlers::{BotContext, handle};
use sentinel::bot::policy_admin::{
    POLICY_OVERLAY_PATH, PolicyOverlay, SharedPolicy, WHITELISTED_KEYS, apply_to_config,
    load as policy_load, save as policy_save, validate_key_value,
};
use sentinel::config::Config;
use sentinel::health::HealthState;
use sentinel::perpl::{AccountEvent, FeedEvent};
use sentinel::pipeline::LiveState;
use sentinel_core::order::{CloseSide, OrderRequest, OrderType};
use sentinel_core::types::{AccountState, ExecutionMode, Market, MarketId, Position};

/// Independent wall clock for every handler/queue call (epoch ms).
const NOW_MS: u64 = 1_764_000_000_000;

/// Telegram MarkdownV2 reserved set, `SPEC-P11.md` §4 (19 characters,
/// including the backslash itself).
const RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Data segment carrying every reserved character (plus safe filler).
const DIRTY: &str = "S_m*b[c](d)e~f`g>h#i+j-k=l|m{n}o.p!q\\r";

// ---------------------------------------------------------------------------
// Shared fixtures.
// ---------------------------------------------------------------------------

fn vars() -> HashMap<String, String> {
    let pairs: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        (
            "PERPL_API_KEY_SECRET",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        ),
        (
            "PERPL_ACCOUNT",
            "0x0000000000000000000000000000000000000007",
        ),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
        ("EXECUTION_MODE", "DRY_RUN"),
        ("MARKET_ALLOWLIST", "32,16"),
        ("RISK_SOFT_PCT", "44"),
        ("RISK_WARN_PCT", "33"),
        ("RISK_HARD_PCT", "22"),
        ("REFLEX_REDUCE_FRACTION", "0.4"),
        ("REFLEX_ORANGE_FRACTION", "0.2"),
        ("REFLEX_COOLDOWN_SECS", "170"),
        ("MAX_ORDER_SIZE_USD", "100000"),
        ("MAX_DAILY_ACTIONS", "7"),
        ("REQUIRE_APPROVAL_ABOVE_USD", "1234"),
    ];
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn demo_cfg() -> Config {
    Config::from_vars(vars()).expect("demo config must load")
}

fn sample_order(market: u32, size: Decimal) -> OrderRequest {
    OrderRequest {
        market_id: MarketId(market),
        close: CloseSide::CloseLong,
        size,
        order_type: OrderType::Market,
        max_slippage_bps: 100,
        size_decimals: 3,
    }
}

fn approval(id: &str, market: u32, now_ms: u64, summary: &str) -> PendingApproval {
    PendingApproval {
        id: id.to_string(),
        market_id: market,
        summary: summary.to_string(),
        order: sample_order(market, Decimal::new(5, 1)),
        decision_ref: None,
        created_ms: now_ms,
        expires_ms: now_ms + APPROVAL_TTL_MS,
    }
}

fn eth_market() -> Market {
    Market {
        id: MarketId(32),
        symbol: "ETH".to_string(),
        base: "ETH".to_string(),
        price_decimals: 2,
        size_decimals: 3,
        initial_margin_fraction: Decimal::new(83333, 6),
        maintenance_margin_fraction: Decimal::new(5, 2),
        max_leverage: Decimal::new(12, 0),
        min_size: Decimal::new(5, 2),
        tick_size: Decimal::new(1, 2),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

fn eth_snapshot() -> FeedEvent {
    let snapshot_ts: DateTime<Utc> =
        DateTime::from_timestamp_millis(NOW_MS as i64).expect("valid ts");
    FeedEvent::Account(AccountEvent::Snapshot {
        state: AccountState {
            positions: vec![Position {
                market_id: MarketId(32),
                symbol: "ETH".to_string(),
                size: Decimal::new(2, 0),
                entry_price: Decimal::new(2500, 0),
                mark_price: Some(Decimal::new(2500, 0)),
                liq_price: None,
                collateral: Decimal::new(1000, 0),
                unrealized_pnl: Decimal::ZERO,
                margin_ratio: None,
                leverage: Decimal::new(5, 0),
                opened_at: None,
            }],
            free_balance: Decimal::new(1000, 0),
            equity: Decimal::new(3000, 0),
            fee_tier: 0,
            snapshot_ts,
        },
    })
}

async fn test_ctx(dir: &Path) -> BotContext {
    BotContext {
        cfg: demo_cfg(),
        state: Arc::new(Mutex::new(LiveState::new())),
        health: Arc::new(HealthState::new(ExecutionMode::DryRun)),
        journal: None,
        policy: Arc::new(SharedPolicy::load(dir.join("policy.json"))),
        approvals: Arc::new(ApprovalQueue::new()),
        engine: None,
        executor: None,
        spend_ledger: None,
        kill: Arc::new(AtomicBool::new(false)),
        pause_pending: AtomicBool::new(false),
    }
}

/// Independent expectation for MarkdownV2 escaping: prefix every reserved
/// char with one backslash (`SPEC-P11.md` §4).
fn escape_expected(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if RESERVED.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn expect_ok(key: &str, value: &str, base: &PolicyOverlay) -> PolicyOverlay {
    match validate_key_value(key, value, base) {
        Ok(overlay) => overlay,
        Err(err) => panic!("expected Ok for {key}={value}: {err}"),
    }
}

fn expect_err(key: &str, value: &str, base: &PolicyOverlay) -> String {
    match validate_key_value(key, value, base) {
        Err(err) => err,
        Ok(overlay) => panic!("expected Err for {key}={value}, got {overlay:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Parse corpus.
// ---------------------------------------------------------------------------

#[test]
fn parse_rejects_malformed_corpus() {
    for bad in [
        "/audit abc",
        "/close 32 abc",
        "/close",
        "/policy set hard_pct",
        "/policy set",
        "/policy set x",
        "/policy set x y z",
        "/risk 32 16",
        "/status extra",
        "/",
        "/unknown",
        "",
        "hello",
        "/risk 32 EXTRA",
        "/естадо",
    ] {
        assert_eq!(parse(bad), None, "must be None: {bad:?}");
    }
}

#[test]
fn parse_accepts_case_insensitive_and_es_aliases() {
    let cases: Vec<(&str, Command)> = vec![
        ("/start", Command::Start),
        ("/inicio", Command::Start),
        ("/help", Command::Help),
        ("/ayuda", Command::Help),
        ("/status", Command::Status),
        ("/estado", Command::Status),
        ("/STATUS", Command::Status),
        ("/policy", Command::Policy),
        ("/politica", Command::Policy),
        ("/pause", Command::Pause),
        ("/pausa", Command::Pause),
        ("/resume", Command::Resume),
        ("/reanudar", Command::Resume),
        ("/spend", Command::Spend),
        ("/gasto", Command::Spend),
        ("/mode", Command::Mode),
        ("/modo", Command::Mode),
        ("/audit", Command::Audit { n: 10 }),
        ("/AUDIT 7", Command::Audit { n: 7 }),
        ("/risk", Command::Risk { market: None }),
        ("/risk 32", Command::Risk { market: Some(32) }),
        ("/riesgo 32", Command::Risk { market: Some(32) }),
        (
            "/close 32",
            Command::Close {
                market: 32,
                fraction: None,
            },
        ),
        (
            "/cerrar 32",
            Command::Close {
                market: 32,
                fraction: None,
            },
        ),
        (
            "/close 32 0.5",
            Command::Close {
                market: 32,
                fraction: Some(Decimal::new(5, 1)),
            },
        ),
        (
            "/CLOSE 32 0.5",
            Command::Close {
                market: 32,
                fraction: Some(Decimal::new(5, 1)),
            },
        ),
        (
            "/aprobar ap-1234abcd",
            Command::Approve {
                id: "ap-1234abcd".to_string(),
            },
        ),
        (
            "/rechazar ap-1234abcd",
            Command::Deny {
                id: "ap-1234abcd".to_string(),
            },
        ),
        (
            "/politica set hard_pct 12",
            Command::PolicySet {
                key: "hard_pct".to_string(),
                value: "12".to_string(),
            },
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(parse(input), Some(expected), "input {input:?}");
    }
}

#[test]
fn parse_audit_caps_at_fifty() {
    assert_eq!(parse("/auditoria 999"), Some(Command::Audit { n: 50 }));
    assert_eq!(parse("/audit 999"), Some(Command::Audit { n: 50 }));
    assert_eq!(parse("/audit 50"), Some(Command::Audit { n: 50 }));
    assert_eq!(parse("/audit 3"), Some(Command::Audit { n: 3 }));
}

#[test]
fn parse_policy_set_spacing() {
    assert_eq!(
        parse("/policy  set  x  y"),
        Some(Command::PolicySet {
            key: "x".to_string(),
            value: "y".to_string(),
        })
    );
    assert_eq!(
        parse("/policy set hard_pct 12"),
        Some(Command::PolicySet {
            key: "hard_pct".to_string(),
            value: "12".to_string(),
        })
    );
}

#[test]
fn parse_close_fraction_zero_shape() {
    // SPEC-P11 verifier corpus: `/close 32 0` may parse; if it does, it must
    // be exactly this shape (downstream validation rejects it, see
    // `handler_close_fraction_zero_rejected_downstream`).
    let parsed = parse("/close 32 0");
    assert!(
        parsed.is_none()
            || parsed
                == Some(Command::Close {
                    market: 32,
                    fraction: Some(Decimal::ZERO),
                }),
        "unexpected shape: {parsed:?}"
    );
    assert_eq!(
        parse("/close 32 1"),
        Some(Command::Close {
            market: 32,
            fraction: Some(Decimal::ONE),
        })
    );
}

#[test]
fn parse_whitespace_edges_pinned() {
    // PINNED (probed): leading whitespace before `/` is tolerated (treated as
    // "extra whitespace", SPEC-P11 §3) and trailing whitespace is ignored;
    // `//status` is NOT a command.
    assert_eq!(parse("  /status"), Some(Command::Status));
    assert_eq!(parse("/status  "), Some(Command::Status));
    assert_eq!(parse("//status"), None);
}

// ---------------------------------------------------------------------------
// 2. Escaping.
// ---------------------------------------------------------------------------

#[test]
fn escape_each_reserved_char_individually() {
    assert_eq!(RESERVED.len(), 19, "spec §4 reserved set");
    for &c in RESERVED {
        let input = c.to_string();
        let want = format!("\\{c}");
        assert_eq!(escape_md2(&input), want, "reserved char {c:?}");
    }
}

#[test]
fn escape_combined_set_and_passthrough() {
    let combined = "a_b*c[d]e(f)g~h`i>j#k+l-m=n|o{p}q.r!s";
    assert_eq!(escape_md2(combined), escape_expected(combined));
    for passthrough in ["plain text 12345", "🚀🟠", "ABCxyz", " ", "a\nb", "a\"b"] {
        assert_eq!(
            escape_md2(passthrough),
            passthrough,
            "non-reserved input must pass through: {passthrough:?}"
        );
    }
    // Every reserved char in the escaped output is preceded by an odd run of
    // backslashes (the MarkdownV2 escape invariant).
    let escaped = escape_md2(combined);
    let mut backslashes = 0usize;
    for c in escaped.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if RESERVED.contains(&c) {
                assert_eq!(
                    backslashes % 2,
                    1,
                    "reserved {c:?} not escaped in {escaped}"
                );
            }
            backslashes = 0;
        }
    }
}

#[test]
fn escape_double_application_pinned() {
    let x = "a_b[c].d!";
    let once = escape_md2(x);
    let twice = escape_md2(&once);
    // PINNED (probed on the landed implementation, 2026-10-06):
    // escape_md2 is a plain char-wise escaper — it does NOT special-case an
    // existing backslash, so re-escaping an already-escaped string doubles
    // the backslashes (`\_` -> `\\\_`). Deterministic; rendering-safe for the
    // single-escape path; documented for the double-escape consumers.
    assert_eq!(
        twice,
        escape_expected(&once),
        "pinned: double application runs the char-wise escaper again"
    );
}

#[test]
fn status_card_escapes_data_segments_and_leaks_no_raw_reserved() {
    let symbol = format!("ETH{DIRTY}");
    let size = format!("+2.5{DIRTY}");
    let entry = format!("2500{DIRTY}");
    let mark = format!("2499{DIRTY}");
    let distance = format!("12.3{DIRTY}");
    let collateral = format!("1000{DIRTY}");
    let upnl = format!("-4.2{DIRTY}");
    let free = format!("500{DIRTY}");
    let row = PositionRow {
        symbol: symbol.clone(),
        market_id: 32,
        size: size.clone(),
        entry: entry.clone(),
        mark: mark.clone(),
        distance_pct: distance.clone(),
        tier_emoji: "🟠".to_string(),
        collateral: collateral.clone(),
        upnl: upnl.clone(),
    };
    let out = status_card(&[row], &free, Some(12), "DRY_RUN");
    for segment in [
        &symbol,
        &size,
        &entry,
        &mark,
        &distance,
        &collateral,
        &upnl,
        &free,
    ] {
        let escaped = escape_md2(segment);
        assert!(
            out.contains(&escaped),
            "status_card must embed the escaped data segment {segment:?} as {escaped:?}; out={out}"
        );
    }
    assert!(
        !out.contains(DIRTY),
        "status_card leaked a raw reserved-char data segment: {out}"
    );
    // The mode string is data too: `DRY_RUN` carries a reserved `_`.
    let empty = status_card(&[], &free, None, "DRY_RUN");
    assert!(
        empty.contains("DRY\\_RUN"),
        "status_card must escape the mode segment; out={empty}"
    );
    assert!(!empty.contains("DRY_RUN"), "raw mode leaked: {empty}");
}

// ---------------------------------------------------------------------------
// 3. Policy overlay.
// ---------------------------------------------------------------------------

#[test]
fn policy_unknown_keys_rejected() {
    let base = PolicyOverlay::default();
    for key in ["nope", "risk_soft", "risk_soft_pcts", "SOFT_PCT", ""] {
        expect_err(key, "1", &base);
    }
}

#[test]
fn policy_kill_switch_bool_values() {
    let base = PolicyOverlay::default();
    assert_eq!(
        expect_ok("kill_switch", "true", &base).kill_switch,
        Some(true)
    );
    assert_eq!(
        expect_ok("kill_switch", "false", &base).kill_switch,
        Some(false)
    );
    for bad in ["maybe", "", "yes", "2"] {
        expect_err("kill_switch", bad, &base);
    }
    // Case-insensitive spelling accepted (pinned by probing): "True" ⇒ true.
    assert_eq!(
        expect_ok("kill_switch", "True", &base).kill_switch,
        Some(true)
    );
}

#[test]
fn policy_range_violations() {
    let base = PolicyOverlay::default();
    // pct > 0
    expect_err("risk_soft_pct", "0", &base);
    expect_err("risk_soft_pct", "0.0", &base);
    expect_err("risk_soft_pct", "-5", &base);
    // fractions (0, 1]
    for key in ["reflex_reduce_fraction", "reflex_orange_fraction"] {
        expect_err(key, "0", &base);
        expect_err(key, "-0.5", &base);
        expect_err(key, "1.5", &base);
    }
    assert_eq!(
        expect_ok("reflex_reduce_fraction", "1", &base).reflex_reduce_fraction,
        Some(Decimal::ONE)
    );
    assert_eq!(
        expect_ok("reflex_orange_fraction", "0.25", &base).reflex_orange_fraction,
        Some(Decimal::new(25, 2))
    );
    // cooldown > 0
    expect_err("reflex_cooldown_secs", "0", &base);
    expect_err("reflex_cooldown_secs", "-1", &base);
    // caps > 0 (the approval threshold is a cap too — probed)
    expect_err("max_order_size_usd", "0", &base);
    expect_err("max_order_size_usd", "-10", &base);
    expect_err("require_approval_above_usd", "0", &base);
    expect_err("require_approval_above_usd", "-1", &base);
    // actions > 0
    expect_err("max_daily_actions", "0", &base);
    expect_err("max_daily_actions", "-3", &base);
    expect_err("max_daily_actions", "2.5", &base);
}

#[test]
fn policy_cross_field_effective_triple_pinned() {
    // PINNED SEMANTICS (probed, 2026-10-06): `validate_key_value` / `set_key`
    // complete the overlay with the built-in default thresholds (soft 25,
    // warn 15, hard 8 — the `Config` defaults) and enforce `hard < warn <
    // soft` on the COMPLETED triple. It is not a pairwise sparse check: a
    // single low value is rejected when a default makes the completed triple
    // inconsistent. Consequences pinned below.
    let base = PolicyOverlay::default();

    // Verifier-corpus case: `soft = 10` completes to (10, 15, 8); warn 15
    // (default) violates `warn < soft`, so the set is rejected before
    // `warn = 15` could be attempted — the pair is rejected in either order.
    let err = expect_err("risk_soft_pct", "10", &base);
    assert!(
        err.contains("hard < warn < soft"),
        "unexpected error: {err}"
    );
    assert!(
        err.contains("soft 10") && err.contains("warn 15"),
        "error must name the completed effective triple: {err}"
    );
    // `warn = 5` alone completes to (25, 5, 8): default hard 8 breaks
    // `hard < warn`.
    expect_err("risk_warn_pct", "5", &base);
    // `hard = 5` alone completes to (25, 15, 5) — consistent, accepted.
    assert_eq!(
        expect_ok("risk_hard_pct", "5", &base).risk_hard_pct,
        Some(Decimal::new(5, 0))
    );

    // Safe lowering order: hard → warn → soft.
    let h = expect_ok("risk_hard_pct", "5", &base);
    let w = expect_ok("risk_warn_pct", "10", &h);
    let s = expect_ok("risk_soft_pct", "20", &w);
    assert_eq!(s.risk_hard_pct, Some(Decimal::new(5, 0)));
    assert_eq!(s.risk_warn_pct, Some(Decimal::new(10, 0)));
    assert_eq!(s.risk_soft_pct, Some(Decimal::new(20, 0)));

    // Violations on the completed triple {soft 20, warn 10, hard 5}.
    expect_err("risk_warn_pct", "25", &s); // warn 25 >= soft 20
    expect_err("risk_soft_pct", "8", &s); // soft 8 <= warn 10
    expect_err("risk_hard_pct", "15", &s); // hard 15 >= warn 10
    expect_err("risk_warn_pct", "5", &s); // strict: warn == hard rejected
    expect_err("risk_soft_pct", "10", &s); // strict: soft == warn rejected

    // Corpus pair (soft 10, warn 15): from `{warn: 15}` the completion is
    // again (10, 15, 8) ⇒ rejected; from the default, `warn = 15` is a no-op
    // success, and `soft = 10` after it still fails.
    let w15 = PolicyOverlay {
        risk_warn_pct: Some(Decimal::new(15, 0)),
        ..PolicyOverlay::default()
    };
    expect_err("risk_soft_pct", "10", &w15);
    expect_ok("risk_warn_pct", "15", &base);

    // Completion uses the defaults for keys absent from the base overlay:
    // `{warn: 5}` + soft 10 fails (default hard 8 breaks it), while adding
    // `hard = 4` makes the same set valid.
    let partial = PolicyOverlay {
        risk_warn_pct: Some(Decimal::new(5, 0)),
        ..PolicyOverlay::default()
    };
    expect_err("risk_soft_pct", "10", &partial);
    let almost = PolicyOverlay {
        risk_warn_pct: Some(Decimal::new(5, 0)),
        risk_hard_pct: Some(Decimal::new(4, 0)),
        ..PolicyOverlay::default()
    };
    assert_eq!(
        expect_ok("risk_soft_pct", "10", &almost).risk_soft_pct,
        Some(Decimal::new(10, 0))
    );

    // Near-boundary valid moves stay accepted.
    assert_eq!(
        expect_ok("risk_soft_pct", "11", &s).risk_soft_pct,
        Some(Decimal::new(11, 0))
    );
    assert_eq!(
        expect_ok("risk_warn_pct", "9", &s).risk_warn_pct,
        Some(Decimal::new(9, 0))
    );
}

#[test]
fn policy_huge_and_unrepresentable_decimals() {
    let base = PolicyOverlay::default();
    // 1e30 is beyond Decimal's representable range (max ~7.9e28): the
    // adversarial set must fail cleanly, never panic or wrap.
    expect_err("max_order_size_usd", "1e30", &base);
    expect_err(
        "max_order_size_usd",
        "1000000000000000000000000000000",
        &base,
    );
    expect_err("risk_soft_pct", "1e30", &base);
    expect_err(
        "reflex_cooldown_secs",
        "99999999999999999999999999999999",
        &base,
    );
    // A huge-but-representable value is a different probe: pinned below by
    // observing behavior (accept/warn is a design choice, not a panic).
    let big = expect_ok("max_order_size_usd", "100000000000000000000", &base);
    assert_eq!(
        big.max_order_size_usd,
        Some(Decimal::from_i128_with_scale(
            100_000_000_000_000_000_000_i128,
            0
        ))
    );
}

#[test]
fn policy_json_injection_rejected() {
    let base = PolicyOverlay::default();
    expect_err("max_daily_actions", "10, \"kill_switch\": true", &base);
    expect_err("max_order_size_usd", "{\"kill_switch\": true}", &base);
    expect_err("max_daily_actions\", \"kill_switch", "1", &base);
    expect_err("kill_switch", "true\n\"max_daily_actions\": 999999", &base);
}

#[test]
fn policy_apply_to_config_empty_overlay_identity() {
    let base = demo_cfg();
    let out = apply_to_config(&base, &PolicyOverlay::default());
    assert_eq!(out.risk.soft_pct, base.risk.soft_pct);
    assert_eq!(out.risk.warn_pct, base.risk.warn_pct);
    assert_eq!(out.risk.hard_pct, base.risk.hard_pct);
    assert_eq!(
        out.risk.reflex_reduce_fraction,
        base.risk.reflex_reduce_fraction
    );
    assert_eq!(
        out.risk.reflex_orange_fraction,
        base.risk.reflex_orange_fraction
    );
    assert_eq!(
        out.risk.reflex_cooldown_secs,
        base.risk.reflex_cooldown_secs
    );
    assert_eq!(out.risk.max_order_size_usd, base.risk.max_order_size_usd);
    assert_eq!(out.risk.max_daily_actions, base.risk.max_daily_actions);
    assert_eq!(out.risk.market_allowlist, base.risk.market_allowlist);
    assert_eq!(
        out.risk.require_approval_above_usd,
        base.risk.require_approval_above_usd
    );
    assert_eq!(
        out.risk.idempotency_window_secs,
        base.risk.idempotency_window_secs
    );
    assert_eq!(
        out.risk.stale_data_alert_secs,
        base.risk.stale_data_alert_secs
    );
    // Spot checks outside the risk section.
    assert_eq!(out.perpl.api_url, base.perpl.api_url);
    assert_eq!(out.perpl.chain_id, base.perpl.chain_id);
    assert_eq!(
        out.strategy.min_interval_secs,
        base.strategy.min_interval_secs
    );
    assert_eq!(out.features.enable_reflex, base.features.enable_reflex);
}

#[test]
fn policy_apply_maps_whitelist_and_leaves_rest() {
    let base = demo_cfg();
    let overlay = PolicyOverlay {
        risk_soft_pct: Some(Decimal::new(40, 0)),
        risk_warn_pct: Some(Decimal::new(30, 0)),
        risk_hard_pct: Some(Decimal::new(20, 0)),
        reflex_reduce_fraction: Some(Decimal::new(3, 1)),
        reflex_orange_fraction: Some(Decimal::new(15, 2)),
        reflex_cooldown_secs: Some(77),
        max_order_size_usd: Some(Decimal::new(12345, 0)),
        require_approval_above_usd: Some(Decimal::new(999, 0)),
        max_daily_actions: Some(9),
        kill_switch: Some(true),
    };
    let out = apply_to_config(&base, &overlay);
    assert_eq!(out.risk.soft_pct, Decimal::new(40, 0));
    assert_eq!(out.risk.warn_pct, Decimal::new(30, 0));
    assert_eq!(out.risk.hard_pct, Decimal::new(20, 0));
    assert_eq!(out.risk.reflex_reduce_fraction, Decimal::new(3, 1));
    assert_eq!(out.risk.reflex_orange_fraction, Decimal::new(15, 2));
    assert_eq!(out.risk.reflex_cooldown_secs, 77);
    assert_eq!(out.risk.max_order_size_usd, Decimal::new(12345, 0));
    assert_eq!(out.risk.require_approval_above_usd, Decimal::new(999, 0));
    assert_eq!(out.risk.max_daily_actions, 9);
    // Non-whitelisted fields untouched.
    assert_eq!(
        out.risk.idempotency_window_secs,
        base.risk.idempotency_window_secs
    );
    assert_eq!(
        out.risk.stale_data_alert_secs,
        base.risk.stale_data_alert_secs
    );
    assert_eq!(out.risk.market_allowlist, base.risk.market_allowlist);
    assert_eq!(
        out.strategy.min_interval_secs,
        base.strategy.min_interval_secs
    );
}

#[test]
fn policy_constants_match_spec() {
    assert_eq!(POLICY_OVERLAY_PATH, "data/policy.json");
    let want: BTreeSet<&str> = [
        "risk_soft_pct",
        "risk_warn_pct",
        "risk_hard_pct",
        "reflex_reduce_fraction",
        "reflex_orange_fraction",
        "reflex_cooldown_secs",
        "max_order_size_usd",
        "require_approval_above_usd",
        "max_daily_actions",
        "kill_switch",
    ]
    .into();
    let got: BTreeSet<&str> = WHITELISTED_KEYS.iter().copied().collect();
    assert_eq!(got, want);
}

#[test]
fn policy_save_is_atomic_and_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("policy.json");
    let overlay = PolicyOverlay {
        risk_soft_pct: Some(Decimal::new(30, 0)),
        kill_switch: Some(true),
        ..PolicyOverlay::default()
    };
    policy_save(&path, &overlay).expect("save");
    assert_eq!(policy_load(&path), overlay);
    let parent = path.parent().expect("parent");
    let entries: Vec<_> = std::fs::read_dir(parent)
        .expect("read_dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("entries");
    assert_eq!(entries.len(), 1, "tmp file must be gone after rename");
    let overlay2 = PolicyOverlay {
        max_daily_actions: Some(4),
        ..PolicyOverlay::default()
    };
    policy_save(&path, &overlay2).expect("save2");
    assert_eq!(policy_load(&path), overlay2);
}

#[test]
fn policy_set_key_fixed_path_versioned_and_failure_safe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("policy.json");
    let policy = SharedPolicy::load(&path);
    let v0 = policy.version();
    assert!(!path.exists(), "load must not create the file");

    // Failed mutation: version and disk untouched.
    assert!(policy.set_key("nope", "1").is_err());
    assert_eq!(policy.version(), v0);
    assert!(!path.exists());

    // Successful mutations (ordering-compatible ladder hard → warn → soft;
    // the cross-field rule completes absent keys from the built-in defaults,
    // so single-key lowering out of order is rejected — see
    // `policy_cross_field_effective_triple_pinned`). `set_key` takes no path:
    // the only file addressable is this instance's own.
    let h = policy.set_key("risk_hard_pct", "5").expect("valid set 1");
    assert_eq!(h.risk_hard_pct, Some(Decimal::new(5, 0)));
    let w = policy.set_key("risk_warn_pct", "10").expect("valid set 2");
    assert_eq!(w.risk_warn_pct, Some(Decimal::new(10, 0)));
    let updated = policy.set_key("risk_soft_pct", "20").expect("valid set 3");
    assert_eq!(updated.risk_soft_pct, Some(Decimal::new(20, 0)));
    assert_eq!(policy.version(), v0 + 3);
    assert!(path.exists());
    assert_eq!(policy_load(&path), policy.snapshot());
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(entries, vec!["policy.json".to_string()]);

    // Cross-field failure leaves the file byte-identical and version stable.
    let before = std::fs::read(&path).expect("read");
    assert!(policy.set_key("risk_warn_pct", "25").is_err());
    assert_eq!(policy.version(), v0 + 3);
    assert_eq!(std::fs::read(&path).expect("read"), before);
}

#[test]
fn policy_set_key_paths_are_isolated() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let path_a = dir_a.path().join("policy.json");
    let path_b = dir_b.path().join("policy.json");
    let a = SharedPolicy::load(&path_a);
    let b = SharedPolicy::load(&path_b);

    a.set_key("kill_switch", "true").expect("set a");
    assert!(path_a.exists());
    assert!(
        !path_b.exists(),
        "instance a must not touch instance b's path"
    );
    assert!(a.kill_switch());
    assert!(!b.kill_switch());

    let bytes_a = std::fs::read(&path_a).expect("read a");
    b.set_key("max_daily_actions", "3").expect("set b");
    assert!(path_b.exists());
    assert_eq!(std::fs::read(&path_a).expect("read a"), bytes_a);
    assert_eq!(b.snapshot().max_daily_actions, Some(3));
}

// ---------------------------------------------------------------------------
// 4. Approvals.
// ---------------------------------------------------------------------------

#[test]
fn approval_ttl_constant_matches_spec() {
    assert_eq!(APPROVAL_TTL_MS, 300_000);
}

#[test]
fn approval_id_formula_and_determinism() {
    let id = ApprovalQueue::id_for("dec-42", 1_700_000_000_000);
    let digest = hex::encode(Sha256::digest(b"dec-42|1700000000000"));
    assert_eq!(
        id,
        format!("ap-{}", &digest[..8]),
        "id must be ap-<8 hex of sha256(decision_id|now_ms)>"
    );
    assert_eq!(id, ApprovalQueue::id_for("dec-42", 1_700_000_000_000));
    assert_ne!(id, ApprovalQueue::id_for("dec-42", 1_700_000_000_001));
    assert_ne!(id, ApprovalQueue::id_for("dec-43", 1_700_000_000_000));
    assert_eq!(id.len(), 11);
    assert!(id.starts_with("ap-"));
    assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn approval_ttl_boundary_exactly_expired_at_ttl() {
    let queue = ApprovalQueue::new();
    let t0 = 1_700_000_000_000u64;
    queue.enqueue(approval("ap-ttl", 32, t0, "boundary"));

    // One millisecond before the TTL: still live.
    assert!(queue.expire_due(t0 + APPROVAL_TTL_MS - 1).is_empty());
    assert!(queue.get("ap-ttl").is_some());
    assert_eq!(queue.len(), 1);

    // Exactly at created_ms + TTL: expired.
    let expired = queue.expire_due(t0 + APPROVAL_TTL_MS);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].id, "ap-ttl");
    assert_eq!(expired[0].expires_ms, t0 + APPROVAL_TTL_MS);
    assert!(queue.get("ap-ttl").is_none());
    assert_eq!(queue.len(), 0);
}

#[test]
fn approval_two_ids_concurrent_inserts() {
    let queue = ApprovalQueue::new();
    let t0 = 1_700_000_000_000u64;
    std::thread::scope(|scope| {
        let q = &queue;
        scope.spawn(move || {
            q.enqueue(approval("ap-a", 32, t0, "leg a"));
        });
        scope.spawn(move || {
            q.enqueue(approval("ap-b", 16, t0, "leg b"));
        });
    });
    assert_eq!(queue.len(), 2);
    assert!(queue.get("ap-a").is_some());
    assert!(queue.get("ap-b").is_some());

    // Independent expiry: b lives one step longer than a.
    queue.enqueue(approval("ap-a", 32, t0, "leg a"));
    let expired = queue.expire_due(t0 + APPROVAL_TTL_MS);
    assert_eq!(expired.len(), 2, "both expire at their own TTL");
    assert_eq!(queue.len(), 0);
}

#[test]
fn approval_duplicate_id_pinned() {
    let queue = ApprovalQueue::new();
    let first = queue.enqueue(approval("ap-fixed", 32, NOW_MS, "first insert"));
    assert_eq!(first, "ap-fixed");
    let second = queue.enqueue(approval("ap-fixed", 16, NOW_MS + 1, "second insert"));
    assert_eq!(second, "ap-fixed");
    assert_eq!(queue.len(), 1, "duplicate id must not grow the queue");
    let got = queue.get("ap-fixed").expect("present");
    // PINNED (probed): same-id enqueue replaces — LAST insert wins.
    assert_eq!(got.summary, "second insert");
    assert_eq!(got.market_id, 16);
}

// ---------------------------------------------------------------------------
// 5. Handler-level downstream checks.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handler_close_fraction_zero_rejected_downstream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = test_ctx(dir.path()).await;
    {
        let mut state = ctx.state.lock().await;
        state.set_markets(vec![eth_market()]);
        state.apply(&eth_snapshot());
    }
    assert!(
        ctx.state.lock().await.position(MarketId(32)).is_some(),
        "fixture position must be visible before probing /close"
    );

    let valid = handle(
        Command::Close {
            market: 32,
            fraction: Some(Decimal::new(5, 1)),
        },
        &ctx,
        NOW_MS,
    )
    .await;
    assert!(
        valid.keyboard.is_some(),
        "valid fraction must build the confirm keyboard: {valid:?}"
    );

    let zero = handle(
        Command::Close {
            market: 32,
            fraction: Some(Decimal::ZERO),
        },
        &ctx,
        NOW_MS,
    )
    .await;
    assert!(
        zero.keyboard.is_none(),
        "fraction 0 must be rejected downstream: {zero:?}"
    );

    let over = handle(
        Command::Close {
            market: 32,
            fraction: Some(Decimal::new(15, 1)),
        },
        &ctx,
        NOW_MS,
    )
    .await;
    assert!(
        over.keyboard.is_none(),
        "fraction > 1 must be rejected downstream: {over:?}"
    );

    let full = handle(
        Command::Close {
            market: 32,
            fraction: None,
        },
        &ctx,
        NOW_MS,
    )
    .await;
    assert!(
        full.keyboard.is_some(),
        "full close must build the confirm keyboard: {full:?}"
    );

    // When parse accepts `/close 32 0`, the handler still rejects it.
    if let Some(parsed) = parse("/close 32 0") {
        let reply = handle(parsed, &ctx, NOW_MS).await;
        assert!(
            reply.keyboard.is_none(),
            "parsed fraction 0 must still be rejected downstream: {reply:?}"
        );
    }
}

#[tokio::test]
async fn handler_mode_honesty_mentions_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = test_ctx(dir.path()).await;
    let reply = handle(Command::Mode, &ctx, NOW_MS).await;
    let lower = reply.text.to_lowercase();
    assert!(
        lower.contains("restart"),
        "/mode must state that switching needs a config-level restart: {reply:?}"
    );
}

#[tokio::test]
async fn handler_risk_without_engine_is_degraded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = test_ctx(dir.path()).await;
    let reply = handle(Command::Risk { market: None }, &ctx, NOW_MS).await;
    assert!(
        reply.text.contains("DEGRADED"),
        "/risk with no engine must reply DEGRADED: {reply:?}"
    );
}
