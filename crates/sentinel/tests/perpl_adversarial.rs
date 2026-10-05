//! Independent adversarial verification suite for the P03 perception layer.
//!
//! Every expectation here is re-derived from the frozen `SPEC.md` (v1.0 + §11
//! changelog), `vendor/api-docs/*.md` and `docs/FACTS.md` §1; the implementation
//! modules under `src/perpl/` were treated as a black box.
//!
//! ## Independent Ed25519 oracle (SPEC §3.1 REST-1/REST-2, §3.2 WS-1)
//!
//! The expected signatures below were computed with **Node's `crypto`**
//! (node v26.7.0), seed hex `0707…07` (32 bytes), PKCS#8 DER =
//! `302e020100300506032b657004220420 ‖ seed`, from the exact SPEC canonical
//! strings (REST-1/REST-2 hashes re-verified by the same script):
//!
//! ```text
//! $ node tests/fixtures/perpl/verifier/p03_ed25519_oracle.js
//!   (run from the repo root; full transcript in that script's header)
//! REST-1 → tg2unwMXTHrFEeR1NHsFwupj1iVLo69RYcPeLvYhHwt5pRCrTWa62GfbNNl5ozwv3i6vtHQ10Oj1qbTi2NtOBg
//! REST-2 → pbi1rC0vtS5Zbvooid41Mzq4ATq3mczW5kF3pcYzQuXymLl99g20mbJiVejoekJfMk7RvazhDfGKoeqxN-teAA
//! WS-1   → qrTi3fOt0EmW_IinRzbCc3UmeBADd1txA7BYLqy5KETUnrCUIzl5gYyp04JVA5huOUm_TvkNhYneWBIjdEDRCg
//! (each signature was also self-verified in-process with crypto.verify ⇒ true)
//! ```
//!
//! ## Fixture replay cross-check (SPEC §3.4)
//!
//! The newest `tests/fixtures/perpl/session-*.jsonl` is re-parsed line by line
//! here (own parser) and the expected `MarketEvent::MarkPrice` sequence is
//! compared, in order, against what `MockPerpl` emits through the public
//! `PerplFeed` API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use rust_decimal::Decimal;
use sentinel::error::{PerplError, SentinelError};
use sentinel::perpl::auth::{
    ApiKeySigner, b64url, canonical_rest, canonical_ws, random_nonce_16, sign_canonical,
};
use sentinel::perpl::types::{
    account_state, marks_from_ticker, parse_account_history, parse_account_update, parse_context,
    parse_fills, parse_positions, parse_wallet,
};
use sentinel::perpl::ws::{StaleDetector, backoff_base};
use sentinel::perpl::{FeedEvent, MarketEvent, MockPerpl, PerplFeed};
use sentinel_core::types::{Market, MarketId};
use serde_json::{Value, json};

/// Seed used by the Node oracle, as raw bytes (`0707…07`).
fn seed() -> [u8; 32] {
    [0x07u8; 32]
}

/// Signing key for the oracle seed.
fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&seed())
}

/// Decimal literal helper.
fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).expect("valid decimal literal")
}

/// `DateTime<Utc>` for a millisecond epoch.
fn ts_ms(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).expect("valid timestamp")
}

/// Extract the `PerplError::Rest` detail from a wrapped perpl error.
fn rest_error_message(err: SentinelError) -> String {
    match err {
        SentinelError::Perpl(PerplError::Rest(msg)) => msg,
        other => panic!("expected SentinelError::Perpl(PerplError::Rest(_)), got {other:?}"),
    }
}

/// Extract the `PerplError::Auth` detail from a wrapped perpl error.
fn auth_error_message(err: SentinelError) -> String {
    match err {
        SentinelError::Perpl(PerplError::Auth(msg)) => msg,
        other => panic!("expected SentinelError::Perpl(PerplError::Auth(_)), got {other:?}"),
    }
}

/// A `Market` shaped per SPEC §3.5 / `types.md` (fields used by the mapping are
/// the ones the payload exercises).
fn market(id: u32, symbol: &str, price_decimals: u32, size_decimals: u32) -> Market {
    Market {
        id: MarketId(id),
        symbol: symbol.to_string(),
        base: format!("{symbol} Perp"),
        price_decimals,
        size_decimals,
        initial_margin_fraction: Decimal::from(100u32) / Decimal::from(1200u32),
        maintenance_margin_fraction: dec("0.05"),
        max_leverage: dec("12"),
        min_size: Decimal::ZERO,
        tick_size: Decimal::new(1, 2),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

/// ETH market slice per SPEC §3.5 (id 32, 2 price decimals, 3 size decimals).
fn eth_markets() -> Vec<Market> {
    vec![market(32, "ETH", 2, 3)]
}

/// SPEC §3.5 ticker mark for ETH: 271370 → 2713.70.
fn eth_marks() -> HashMap<u32, Decimal> {
    HashMap::from([(32u32, dec("2713.70"))])
}

// ---------------------------------------------------------------------------
// 1. Auth oracle — canonical strings and Node-computed signatures
// ---------------------------------------------------------------------------

const SEED_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const TEST_TOKEN: &str = "test-token";

/// SPEC §3.1 REST-1 canonical string (verbatim).
const SPEC_REST1_CANONICAL: &str = concat!(
    "143\n",
    "GET\n",
    "/v1/trading/fills?count=100\n",
    "1728000000000\n",
    "AAAAAAAAAAAAAAAAAAAAAA\n",
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
);

/// SPEC §3.1 REST-2 canonical string (verbatim; body is 8 bytes per §11b).
const SPEC_REST2_CANONICAL: &str = concat!(
    "10143\n",
    "POST\n",
    "/v1/trading/orders\n",
    "1728000001000\n",
    "AAAAAAAAAAAAAAAAAAAAAA\n",
    "088214f816e99a2f4aedb5323c1c2eaf8b8143df9424ec46759966ddd9b72dd3",
);

/// SPEC §3.2 WS-1 canonical string (verbatim).
const SPEC_WS1_CANONICAL: &str = concat!(
    "10143\n",
    "trading-ws-signin\n",
    "1728000000000\n",
    "AAAAAAAAAAAAAAAAAAAAAA",
);

/// Node-computed Ed25519 signature for SPEC REST-1 (see module docs).
const NODE_REST1_SIG: &str =
    "tg2unwMXTHrFEeR1NHsFwupj1iVLo69RYcPeLvYhHwt5pRCrTWa62GfbNNl5ozwv3i6vtHQ10Oj1qbTi2NtOBg";
/// Node-computed Ed25519 signature for SPEC REST-2 (see module docs).
const NODE_REST2_SIG: &str =
    "pbi1rC0vtS5Zbvooid41Mzq4ATq3mczW5kF3pcYzQuXymLl99g20mbJiVejoekJfMk7RvazhDfGKoeqxN-teAA";
/// Node-computed Ed25519 signature for SPEC WS-1 (see module docs).
const NODE_WS1_SIG: &str =
    "qrTi3fOt0EmW_IinRzbCc3UmeBADd1txA7BYLqy5KETUnrCUIzl5gYyp04JVA5huOUm_TvkNhYneWBIjdEDRCg";

#[test]
fn spec_canonical_vectors_are_reproduced_byte_for_byte() {
    let rest1 = canonical_rest(
        143,
        "GET",
        "/v1/trading/fills?count=100",
        "1728000000000",
        "AAAAAAAAAAAAAAAAAAAAAA",
        b"",
    );
    assert_eq!(
        rest1, SPEC_REST1_CANONICAL,
        "canonical_rest must reproduce SPEC REST-1 verbatim"
    );

    let body = br#"{"d":[]}"#;
    assert_eq!(body.len(), 8, "REST-2 body is 8 bytes (SPEC §11b)");
    let rest2 = canonical_rest(
        10143,
        "POST",
        "/v1/trading/orders",
        "1728000001000",
        "AAAAAAAAAAAAAAAAAAAAAA",
        body,
    );
    assert_eq!(
        rest2, SPEC_REST2_CANONICAL,
        "canonical_rest must reproduce SPEC REST-2 verbatim"
    );

    let ws1 = canonical_ws(10143, "1728000000000", "AAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(
        ws1, SPEC_WS1_CANONICAL,
        "canonical_ws must reproduce SPEC WS-1 verbatim"
    );
}

#[test]
fn node_ed25519_oracle_signatures_match_sign_canonical() {
    let key = signing_key();

    let rest1 = canonical_rest(
        143,
        "GET",
        "/v1/trading/fills?count=100",
        "1728000000000",
        "AAAAAAAAAAAAAAAAAAAAAA",
        b"",
    );
    assert_eq!(
        sign_canonical(&key, &rest1),
        NODE_REST1_SIG,
        "sign_canonical(seed 0707…, REST-1) must equal the Node oracle signature"
    );

    let rest2 = canonical_rest(
        10143,
        "POST",
        "/v1/trading/orders",
        "1728000001000",
        "AAAAAAAAAAAAAAAAAAAAAA",
        br#"{"d":[]}"#,
    );
    assert_eq!(
        sign_canonical(&key, &rest2),
        NODE_REST2_SIG,
        "sign_canonical(seed 0707…, REST-2) must equal the Node oracle signature"
    );

    let ws1 = canonical_ws(10143, "1728000000000", "AAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(
        sign_canonical(&key, &ws1),
        NODE_WS1_SIG,
        "sign_canonical(seed 0707…, WS-1) must equal the Node oracle signature"
    );
}

#[test]
fn b64url_is_unpadded_url_safe() {
    assert_eq!(b64url(b"hello"), "aGVsbG8", "SPEC §9 vector");

    for sig in [NODE_REST1_SIG, NODE_REST2_SIG, NODE_WS1_SIG] {
        let raw = URL_SAFE_NO_PAD
            .decode(sig)
            .expect("oracle signature decodes as base64url");
        assert_eq!(raw.len(), 64, "Ed25519 signature is 64 bytes");
        assert_eq!(
            b64url(&raw),
            sig,
            "b64url round-trips the Node oracle signature"
        );
        assert!(!sig.contains('='), "no base64 padding allowed");
    }
}

#[test]
fn random_nonce_16_is_16_bytes_and_unique() {
    let a = random_nonce_16().expect("nonce A");
    let b = random_nonce_16().expect("nonce B");
    assert_ne!(a, b, "nonces must differ across calls");
    for n in [&a, &b] {
        assert!(!n.contains('='), "nonce is unpadded base64url");
        let raw = URL_SAFE_NO_PAD.decode(n).expect("nonce decodes");
        assert_eq!(raw.len(), 16, "nonce is 16 random bytes");
    }
}

// ---------------------------------------------------------------------------
// 2. Auth surface — headers, signature/nonce shapes, Debug redaction
// ---------------------------------------------------------------------------

#[test]
fn signed_request_headers_conform_to_spec() {
    let signer = ApiKeySigner::from_parts(TEST_TOKEN, &format!("0x{SEED_HEX}"), 10143)
        .expect("signer builds from parts");
    assert_eq!(signer.chain_id(), 10143);

    let h1 = signer
        .signed_request_headers("GET", "/v1/trading/wallet", b"")
        .expect("headers build");
    let h2 = signer
        .signed_request_headers("GET", "/v1/trading/wallet", b"")
        .expect("headers build");

    let names: Vec<&str> = h1.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "X-API-Key",
            "X-API-Timestamp",
            "X-API-Nonce",
            "X-API-Signature"
        ],
        "header names and order per SPEC §3.1"
    );

    assert_eq!(h1[0].1, TEST_TOKEN, "X-API-Key carries the opaque token");

    let ts_text = &h1[1].1;
    let ts: u64 = ts_text.parse().expect("X-API-Timestamp is decimal millis");
    let nonce = &h1[2].1;
    let sig_text = &h1[3].1;

    let nonce_bytes = URL_SAFE_NO_PAD.decode(nonce).expect("nonce is base64url");
    assert_eq!(nonce_bytes.len(), 16, "nonce decodes to 16 bytes");
    assert!(!nonce.contains('='), "nonce is unpadded");

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_text)
        .expect("signature is base64url");
    assert_eq!(sig_bytes.len(), 64, "signature decodes to 64 bytes");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as u64;
    assert!(
        ts.abs_diff(now) <= 5 * 60 * 1000,
        "timestamp must be within ±5 min of now (ts={ts}, now={now})"
    );

    assert_ne!(
        h1[2].1, h2[2].1,
        "two consecutive calls must use different nonces"
    );

    // The signature must cover exactly the returned timestamp + nonce values.
    let canonical = canonical_rest(10143, "GET", "/v1/trading/wallet", ts_text, nonce, b"");
    assert_eq!(
        sig_text,
        &sign_canonical(&signing_key(), &canonical),
        "signature must verify against the canonical string rebuilt from the headers"
    );
}

#[test]
fn signer_debug_never_leaks_token_or_seed() {
    let signer = ApiKeySigner::from_parts(TEST_TOKEN, &format!("0x{SEED_HEX}"), 10143)
        .expect("signer builds from parts");
    let dbg = format!("{signer:?}");

    assert!(
        !dbg.contains(TEST_TOKEN),
        "Debug output must not contain the token: {dbg}"
    );
    assert!(
        !dbg.contains(SEED_HEX),
        "Debug output must not contain the full seed hex: {dbg}"
    );
    assert!(
        !dbg.contains(&SEED_HEX[..16]),
        "Debug output must not contain a 16-hex-char seed prefix: {dbg}"
    );
    assert!(
        !dbg.contains(&format!("0x{SEED_HEX}")),
        "Debug output must not contain the 0x-prefixed seed: {dbg}"
    );
}

#[test]
fn ws_signin_frame_matches_spec_and_self_verifies() {
    let signer = ApiKeySigner::from_parts(TEST_TOKEN, &format!("0x{SEED_HEX}"), 10143)
        .expect("signer builds from parts");
    let frame = signer.ws_signin_frame().expect("frame builds");
    let v: Value = serde_json::from_str(&frame).expect("frame is JSON");

    assert_eq!(v["mt"].as_u64(), Some(29), "mt:29 ApiKeySignIn");
    assert_eq!(v["chain_id"].as_u64(), Some(10143));
    assert_eq!(v["api_key"].as_str(), Some(TEST_TOKEN));

    let ts = v["timestamp"]
        .as_str()
        .expect("timestamp is a decimal string");
    let ts_ms: u64 = ts.parse().expect("timestamp parses as millis");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as u64;
    assert!(ts_ms.abs_diff(now) <= 5 * 60 * 1000, "fresh timestamp");

    let nonce = v["nonce"].as_str().expect("nonce is a string");
    assert_eq!(
        URL_SAFE_NO_PAD.decode(nonce).expect("nonce decodes").len(),
        16
    );

    let sig = v["signature"].as_str().expect("signature is a string");
    assert_eq!(URL_SAFE_NO_PAD.decode(sig).expect("sig decodes").len(), 64);

    // The frame signature must cover the SPEC §3.2 canonical string rebuilt
    // from the frame's own fields, validated against the Node oracle scheme.
    let canonical = canonical_ws(10143, ts, nonce);
    assert_eq!(
        sig,
        &sign_canonical(&signing_key(), &canonical),
        "ws sign-in signature must cover chain_id, tag, timestamp, nonce"
    );
}

#[test]
fn from_parts_rejects_invalid_seeds() {
    let bad_seeds = [
        format!("0x{}", "zz".repeat(32)), // non-hex
        format!("0x{}", "07".repeat(31)), // 31 bytes
        format!("0x{}", "07".repeat(33)), // 33 bytes
        String::new(),                    // empty
    ];
    for bad in &bad_seeds {
        let err =
            ApiKeySigner::from_parts(TEST_TOKEN, bad, 10143).expect_err("seed must be rejected");
        let msg = auth_error_message(err);
        assert!(!msg.is_empty(), "auth error carries detail");
    }
}

// ---------------------------------------------------------------------------
// 3. Mapping adversarial — SPEC §3.3/§3.5 shapes
// ---------------------------------------------------------------------------

/// SPEC §3.5 ETH context payload.
fn eth_context_payload() -> Value {
    json!({
        "markets": [{
            "ver": 270, "id": 32, "instance_id": 12, "perpetual_id": 32,
            "symbol": "ETH", "name": "ETH Perp",
            "funding_interval_sec": 2580, "order_ttl_blocks": 20,
            "order_max_market_slippage_bps": 1000,
            "config": {
                "price_decimals": 2, "size_decimals": 3, "min_posting_amount": "0",
                "initial_margin": 1200, "maintenance_margin": 2000,
                "maker_fee": 45, "taker_fee": 345, "contract_version": [1, 7, 5]
            }
        }]
    })
}

#[test]
fn context_example_yields_spec_fields() {
    let markets = parse_context(&eth_context_payload()).expect("context parses");
    let eth = markets
        .iter()
        .find(|m| m.id == MarketId(32))
        .expect("ETH present");
    assert_eq!(eth.symbol, "ETH");
    assert_eq!(eth.base, "ETH Perp");
    assert_eq!(eth.price_decimals, 2);
    assert_eq!(eth.size_decimals, 3);
    assert_eq!(
        eth.initial_margin_fraction,
        Decimal::from(100u32) / Decimal::from(1200u32),
        "100 / 1200 per SPEC §3.3"
    );
    assert_eq!(eth.maintenance_margin_fraction, dec("0.05"));
    assert_eq!(eth.max_leverage, dec("12"));
    assert_eq!(eth.min_size, Decimal::ZERO, "venue minimum not exposed");
    assert_eq!(eth.tick_size, dec("0.01"));
    assert_eq!(eth.maker_fee_micros, 45);
    assert_eq!(eth.taker_fee_micros, 345);
    assert_eq!(eth.order_ttl_blocks, 20);
}

#[test]
fn context_symbol_empty_falls_back_to_name() {
    let mut payload = eth_context_payload();
    let m = &mut payload["markets"][0];
    m["id"] = json!(1);
    m["symbol"] = json!("");
    m["name"] = json!("BTC Perp");

    let markets = parse_context(&payload).expect("BTC-style context parses");
    assert_eq!(markets.len(), 1);
    assert_eq!(
        markets[0].symbol, "BTC Perp",
        "empty symbol falls back to name (mainnet BTC/MON quirk)"
    );
    assert_eq!(markets[0].base, "BTC Perp", "base is the raw name");
}

#[test]
fn context_malformed_payload_errors() {
    let err = parse_context(&json!({})).expect_err("context without markets must error");
    rest_error_message(err);
}

/// SPEC §3.5 long position (shape verbatim), with `sd`/`st`/`s` overridable.
fn eth_position(sd: u64, st: u64, s: u64) -> Value {
    json!({
        "at": {"b": 1u64, "t": 1791235000000u64},
        "mkt": 32, "acc": 7, "pid": 1001, "rq": 41, "oid": 555,
        "st": st, "sr": 21, "sd": sd, "c": "150000000", "ep": 271200,
        "s": s, "fee": "1000", "cfee": "0", "efs": 0, "lv": 500,
        "dpnl": "0", "fnd": "0", "ots": {"b": 1u64, "t": 1791234000000u64}
    })
}

/// Positions snapshot envelope (`mt:26`).
fn positions_payload(positions: Value) -> Value {
    json!({
        "mt": 26, "sn": 68507460u64,
        "at": {"b": 68507460u64, "t": 1791235369000u64},
        "d": positions
    })
}

#[test]
fn positions_map_example_values() {
    let markets = eth_markets();

    let long = parse_positions(
        &positions_payload(json!([eth_position(1, 1, 50000)])),
        &markets,
        &eth_marks(),
    )
    .expect("long position parses");
    assert_eq!(long.len(), 1);
    let p = &long[0];
    assert_eq!(p.market_id, MarketId(32));
    assert_eq!(p.symbol, "ETH");
    assert_eq!(p.size, dec("50.000"), "sd=1 keeps size positive");
    assert_eq!(p.entry_price, dec("2712.00"));
    assert_eq!(p.collateral, dec("150.000000"), "collateral = raw / 10^6");
    assert_eq!(p.leverage, dec("5"), "lv 500 → 5x");
    assert_eq!(p.mark_price, Some(dec("2713.70")));
    assert_eq!(p.unrealized_pnl, dec("85.000000"), "(2713.70-2712.00)*50");
    assert_eq!(p.liq_price, None, "gateway does not expose liq price");
    assert_eq!(p.margin_ratio, None, "gateway does not expose margin ratio");
    assert_eq!(p.opened_at, Some(ts_ms(1791234000000)));

    let short = parse_positions(
        &positions_payload(json!([eth_position(2, 1, 50000)])),
        &markets,
        &eth_marks(),
    )
    .expect("short position parses");
    assert_eq!(short[0].size, dec("-50.000"), "sd=2 negates the size");
    assert_eq!(short[0].unrealized_pnl, dec("-85.000000"));

    let no_mark = parse_positions(
        &positions_payload(json!([eth_position(1, 1, 50000)])),
        &markets,
        &HashMap::new(),
    )
    .expect("position without marks parses");
    assert_eq!(no_mark[0].mark_price, None);
    assert_eq!(no_mark[0].unrealized_pnl, Decimal::ZERO);
}

#[test]
fn positions_adversarial_edges() {
    let markets = eth_markets();
    let marks = eth_marks();

    let mixed = parse_positions(
        &positions_payload(json!([
            eth_position(1, 1, 50000),
            eth_position(1, 2, 50000),
            eth_position(1, 5, 50000)
        ])),
        &markets,
        &marks,
    )
    .expect("mixed-status snapshot parses");
    assert_eq!(mixed.len(), 1, "only st == 1 (Open) is kept");

    let zero = parse_positions(
        &positions_payload(json!([eth_position(1, 1, 0)])),
        &markets,
        &marks,
    )
    .expect("zero-size position parses");
    assert_eq!(zero[0].size, Decimal::ZERO);
    assert_eq!(zero[0].unrealized_pnl, Decimal::ZERO);

    let mut unknown = eth_position(1, 1, 50000);
    unknown["mkt"] = json!(999);
    let err = parse_positions(&positions_payload(json!([unknown])), &markets, &marks)
        .expect_err("unknown market must error");
    let msg = rest_error_message(err);
    assert!(
        msg.contains("unknown market id") && msg.contains("999"),
        "error must name the unknown id: {msg}"
    );

    let mut extra = eth_position(1, 1, 50000);
    extra["zzz_unknown"] = json!({"nested": true});
    let ok = parse_positions(&positions_payload(json!([extra])), &markets, &marks)
        .expect("unknown fields are ignored");
    assert_eq!(ok[0].size, dec("50.000"));

    let empty = parse_positions(&positions_payload(json!([])), &markets, &marks)
        .expect("empty array is fine");
    assert!(empty.is_empty());

    let err = parse_positions(&json!({"mt": 26, "sn": 1u64}), &markets, &marks)
        .expect_err("envelope without `d` must error");
    rest_error_message(err);
}

#[test]
fn positions_missing_required_fields_error() {
    let markets = eth_markets();
    let marks = eth_marks();
    for field in ["mkt", "sd", "s", "ep", "c", "lv"] {
        let mut pos = eth_position(1, 1, 50000);
        let removed = pos.as_object_mut().expect("object").remove(field);
        assert!(removed.is_some(), "sanity: {field} present before removal");
        let err = parse_positions(&positions_payload(json!([pos])), &markets, &marks)
            .expect_err(&format!("missing required field `{field}` must error"));
        let msg = rest_error_message(err);
        assert!(!msg.is_empty(), "rest error carries detail for `{field}`");
    }
}

#[test]
fn ticker_marks_example_and_adversarial() {
    let markets = vec![market(32, "ETH", 2, 3), market(64, "MON", 1, 5)];

    let spec_ticker = json!({"mt": 9, "sn": 1u64, "d": {
        "32": {"at": {"t": 1791235369000u64}, "orl": 271373, "mrk": 271370, "lst": 271438,
               "mid": 271401, "bid": 271386, "ask": 271416, "prv": 270329,
               "dv": 3849321, "oi": 238883, "tvl": "132421635870"}
    }});
    let marks = marks_from_ticker(&spec_ticker, &markets).expect("SPEC §3.5 ticker parses");
    assert_eq!(marks.get(&32), Some(&dec("2713.70")));

    let multi = json!({"mt": 9, "sn": 2u64, "d": {
        "32": {"mrk": 271370},
        "64": {"mrk": 3140}
    }});
    let marks = marks_from_ticker(&multi, &markets).expect("multi-market ticker parses");
    assert_eq!(marks.get(&32), Some(&dec("2713.70")));
    assert_eq!(marks.get(&64), Some(&dec("314.0")));

    let empty = marks_from_ticker(&json!({"mt": 9, "d": {}}), &markets).expect("empty ticker ok");
    assert!(empty.is_empty());

    for bad in [json!({"mt": 9, "d": []}), json!({"mt": 9, "d": "nope"})] {
        let err = marks_from_ticker(&bad, &markets).expect_err("malformed ticker must error");
        rest_error_message(err);
    }
}

#[test]
fn fills_example_and_adversarial() {
    let markets = eth_markets();

    let taker = parse_fills(
        &json!({"d": [{"at": {"t": 1791235369000u64}, "mkt": 32, "oid": 555, "t": 1,
                        "l": 2, "p": 271300, "s": 50000, "f": "9450"}]}),
        &markets,
    )
    .expect("SPEC §3.5 fill parses");
    let r = &taker[0];
    assert_eq!(r.market_id, MarketId(32));
    assert_eq!(r.order_id, 555);
    assert!(!r.is_maker, "l=2 is Taker");
    assert_eq!(r.price, Some(dec("2713.00")));
    assert_eq!(r.size, dec("50.000"));
    assert_eq!(r.fee, dec("0.009450"), "9450 micros → 0.009450 (§11a)");
    assert_eq!(r.ts, Some(ts_ms(1791235369000)));

    let maker = parse_fills(
        &json!({"d": [{"at": {"t": 1791235369000u64}, "mkt": 32, "oid": 1, "t": 1,
                        "l": 1, "p": 271300, "s": 50000, "f": "-100"}]}),
        &markets,
    )
    .expect("maker fill parses");
    assert!(maker[0].is_maker, "l=1 is Maker");

    let no_price = parse_fills(
        &json!({"d": [{"at": {"t": 1791235369000u64}, "mkt": 32, "oid": 2, "t": 1,
                        "l": 2, "s": 50000, "f": "9450"}]}),
        &markets,
    )
    .expect("optional `p` accepted");
    assert_eq!(no_price[0].price, None);

    // timestamps: object / bare number / digit string all mean the same ms.
    for at in [
        json!({"b": 1u64, "t": 1791235369000u64}),
        json!(1791235369000u64),
        json!("1791235369000"),
    ] {
        let recs = parse_fills(
            &json!({"d": [{"at": at, "mkt": 32, "oid": 3, "t": 1, "l": 2,
                            "p": 271300, "s": 50000, "f": "9450"}]}),
            &markets,
        )
        .expect("timestamp forms parse");
        assert_eq!(
            recs[0].ts,
            Some(ts_ms(1791235369000)),
            "timestamp form must map to the same ms"
        );
    }

    // garbage timestamps → ts None, the record still parses.
    for at in [
        json!("not-a-timestamp"),
        json!({"t": "later"}),
        json!({"b": 7u64}),
    ] {
        let recs = parse_fills(
            &json!({"d": [{"at": at, "mkt": 32, "oid": 4, "t": 1, "l": 2,
                            "p": 271300, "s": 50000, "f": "9450"}]}),
            &markets,
        )
        .expect("unparseable timestamp tolerated");
        assert_eq!(recs[0].ts, None, "unparseable timestamp must be None");
    }

    for field in ["mkt", "s"] {
        let mut fill = json!({"at": {"t": 1791235369000u64}, "mkt": 32, "oid": 5, "t": 1,
                              "l": 2, "p": 271300, "s": 50000, "f": "9450"});
        fill.as_object_mut().expect("object").remove(field);
        let err = parse_fills(&json!({"d": [fill]}), &markets)
            .expect_err(&format!("missing required field `{field}` must error"));
        rest_error_message(err);
    }

    let mut unknown = json!({"at": {"t": 1791235369000u64}, "mkt": 999, "oid": 6, "t": 1,
                             "l": 2, "p": 271300, "s": 50000, "f": "9450"});
    unknown["mkt"] = json!(999);
    let err = parse_fills(&json!({"d": [unknown]}), &markets).expect_err("unknown market errors");
    let msg = rest_error_message(err);
    assert!(
        msg.contains("unknown market id") && msg.contains("999"),
        "error must name the unknown id: {msg}"
    );

    let empty = parse_fills(&json!({"d": []}), &markets).expect("empty array is fine");
    assert!(empty.is_empty());
}

#[test]
fn account_history_example_and_adversarial() {
    let spec = json!({"d": [{"at": {"t": 1791235369000u64}, "in": 12, "id": 7, "et": 4,
                             "m": 32, "a": "-9450", "b": "999990550", "f": "9450"}]});
    let recs = parse_account_history(&spec).expect("SPEC §3.5 history parses");
    assert_eq!(recs[0].kind, 4);
    assert_eq!(recs[0].market_id, Some(MarketId(32)));
    assert_eq!(recs[0].amount, dec("-0.009450"));
    assert_eq!(recs[0].balance, dec("999.990550"));
    assert_eq!(recs[0].ts, Some(ts_ms(1791235369000)));

    let mut no_market = spec["d"][0].clone();
    no_market.as_object_mut().expect("object").remove("m");
    let recs = parse_account_history(&json!({"d": [no_market]})).expect("optional `m`");
    assert_eq!(recs[0].market_id, None);

    let mut no_amount = spec["d"][0].clone();
    no_amount.as_object_mut().expect("object").remove("a");
    let err = parse_account_history(&json!({"d": [no_amount]}))
        .expect_err("missing required amount `a` must error");
    rest_error_message(err);

    let empty = parse_account_history(&json!({"d": []})).expect("empty array is fine");
    assert!(empty.is_empty());
}

/// SPEC §3.5 wallet payload.
fn wallet_payload() -> Value {
    json!({
        "mt": 19, "sn": 68507460u64,
        "at": {"b": 68507460u64, "t": 1791235369000u64},
        "addr": "0xabc", "n": 12, "fl": 0,
        "as": [{
            "mt": 19, "in": 12, "id": 7, "fr": false, "fw": true, "ft": 0, "lfr": 41,
            "b": "1000000000", "lb": "0"
        }],
        "sts": []
    })
}

#[test]
fn wallet_example_and_missing_accounts() {
    let (id, free, balance, tier, forward, lfr) =
        parse_wallet(&wallet_payload()).expect("SPEC §3.5 wallet parses");
    assert_eq!(id, 7);
    assert_eq!(free, dec("1000.0"));
    assert_eq!(balance, dec("1000.0"));
    assert_eq!(tier, 0);
    assert!(forward);
    assert_eq!(lfr, 41);

    let mut no_accounts = wallet_payload();
    no_accounts.as_object_mut().expect("object").remove("as");
    let err = parse_wallet(&no_accounts).expect_err("missing as[] must error");
    rest_error_message(err);

    let mut empty_accounts = wallet_payload();
    empty_accounts["as"] = json!([]);
    let err = parse_wallet(&empty_accounts).expect_err("empty as[] must error");
    rest_error_message(err);
}

#[test]
fn account_update_example_and_adversarial() {
    let spec = json!({"mt": 21, "in": 12, "id": 7, "fr": false, "fw": true, "ft": 0,
                      "lfr": 42, "b": "900000000", "lb": "100000000"});
    let update = parse_account_update(&spec).expect("SPEC §3.5 mt:21 parses");
    assert_eq!(update.account_id, 7);
    assert_eq!(update.free_balance, dec("800.0"), "free = balance - locked");
    assert_eq!(update.fee_tier, 0);
    assert!(update.forward_enabled);
    assert_eq!(update.last_forwarded_request_id, 42);
    assert_eq!(update.ts, None, "no `at` in the §3.5 example → ts None");

    let mut with_at = spec.clone();
    with_at["at"] = json!({"b": 1u64, "t": 1791235369000u64});
    let update = parse_account_update(&with_at).expect("mt:21 with `at` parses");
    assert_eq!(update.ts, Some(ts_ms(1791235369000)));

    let mut no_id = spec.clone();
    no_id.as_object_mut().expect("object").remove("id");
    let err = parse_account_update(&no_id).expect_err("missing `id` must error");
    rest_error_message(err);
}

#[test]
fn account_state_equity_sums_balance_and_unrealized_pnl() {
    let long_state = account_state(
        &wallet_payload(),
        &positions_payload(json!([eth_position(1, 1, 50000)])),
        &eth_marks(),
        &eth_markets(),
    )
    .expect("state builds");
    assert_eq!(long_state.free_balance, dec("1000.0"));
    assert_eq!(long_state.fee_tier, 0);
    assert_eq!(long_state.positions.len(), 1);
    assert_eq!(long_state.positions[0].unrealized_pnl, dec("85.000000"));
    assert_eq!(
        long_state.equity,
        dec("1085.0"),
        "equity = balance + Σ upnl"
    );

    let short_state = account_state(
        &wallet_payload(),
        &positions_payload(json!([eth_position(2, 1, 50000)])),
        &eth_marks(),
        &eth_markets(),
    )
    .expect("state builds");
    assert_eq!(short_state.equity, dec("915.0"), "equity = 1000 - 85");
}

// ---------------------------------------------------------------------------
// 4. Staleness boundaries and backoff table
// ---------------------------------------------------------------------------

#[test]
fn stale_detector_boundaries() {
    let mut det = StaleDetector::new(Duration::from_secs(5));

    assert_eq!(det.observe(1_000_000), None, "no touch ever → never fires");

    det.touch(0);
    assert_eq!(
        det.observe(4_999),
        None,
        "1 ms below threshold fires nothing"
    );
    assert_eq!(
        det.observe(5_000),
        Some(5),
        "at exactly the threshold: Some(5)"
    );
    assert_eq!(det.observe(5_001), None, "fires exactly once per episode");
    assert_eq!(det.observe(600_000), None, "still the same episode");

    det.touch(600_000);
    assert_eq!(det.observe(604_999), None, "touch resets the window");
    assert_eq!(det.observe(605_000), Some(5), "second episode fires");
    assert_eq!(det.observe(605_001), None, "second episode fires once");

    det.touch(605_000);
    assert_eq!(
        det.observe(740_000),
        Some(135),
        "secs = whole seconds since last touch"
    );
}

#[test]
fn stale_detector_truncates_whole_seconds() {
    let mut det = StaleDetector::new(Duration::from_millis(1_500));
    det.touch(0);
    assert_eq!(
        det.observe(1_600),
        Some(1),
        "1.6 s elapsed at a 1.5 s threshold → Some(1), truncated not rounded"
    );
    assert_eq!(det.observe(9_000), None, "one episode, one fire");
}

#[test]
fn backoff_base_matches_spec_table() {
    let expected_ms = [500u64, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000];
    for (attempt, ms) in expected_ms.iter().enumerate() {
        assert_eq!(
            backoff_base(attempt as u32),
            Duration::from_millis(*ms),
            "attempt {attempt}"
        );
    }
    assert_eq!(
        backoff_base(40),
        Duration::from_millis(30_000),
        "backoff saturates at 30 s"
    );
}

// ---------------------------------------------------------------------------
// 5. Fixture replay cross-check (own parser vs MockPerpl)
// ---------------------------------------------------------------------------

/// Newest recorded session fixture under `tests/fixtures/perpl/`.
fn newest_fixture() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/perpl");
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir readable")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl"))
        })
        .collect();
    sessions.sort();
    sessions
        .pop()
        .expect("at least one session fixture recorded")
}

/// Price decimals per market id, read from the fixture's context recording.
fn fixture_decimals(raw: &str) -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("path").and_then(Value::as_str) != Some("/v1/pub/context") {
            continue;
        }
        let Some(markets) = v["resp"]["markets"].as_array() else {
            continue;
        };
        for m in markets {
            if let (Some(id), Some(pd)) = (m["id"].as_u64(), m["config"]["price_decimals"].as_u64())
            {
                out.insert(id as u32, pd as u32);
            }
        }
    }
    out
}

/// Expected `MarkPrice` frames derived independently from the raw JSONL: one
/// vector per `mt:9` message, one `(market_id, price)` per entry with a known
/// price scale.
fn fixture_expected_frames(raw: &str, decimals: &HashMap<u32, u32>) -> Vec<Vec<(u32, Decimal)>> {
    let mut frames = Vec::new();
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("ws") {
            continue;
        }
        let msg = &v["msg"];
        if msg.get("mt").and_then(Value::as_u64) != Some(9) {
            continue;
        }
        let Some(entries) = msg["d"].as_object() else {
            continue;
        };
        let mut frame = Vec::new();
        for (key, state) in entries {
            let Ok(id) = key.parse::<u32>() else { continue };
            let (Some(pd), Some(raw_price)) = (decimals.get(&id), state["mrk"].as_i64()) else {
                continue;
            };
            frame.push((id, Decimal::try_new(raw_price, *pd).expect("scaled mark")));
        }
        frames.push(frame);
    }
    frames
}

#[tokio::test]
async fn fixture_replay_matches_independent_parser() {
    let path = newest_fixture();
    let raw = std::fs::read_to_string(&path).expect("fixture readable");
    let decimals = fixture_decimals(&raw);
    assert!(!decimals.is_empty(), "fixture carries a context recording");
    let frames = fixture_expected_frames(&raw, &decimals);
    assert!(!frames.is_empty(), "fixture carries mt:9 frames");

    let feed = MockPerpl::from_fixture(&path).expect("fixture loads");
    let mut rx = feed.stream().await;

    let mut received: Vec<(u32, Decimal)> = Vec::new();
    let collect = async {
        while let Some(event) = rx.recv().await {
            match event {
                FeedEvent::Market(MarketEvent::MarkPrice {
                    market_id, price, ..
                }) => received.push((market_id.0, price)),
                other => panic!("public market-data fixture must not emit {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), collect)
        .await
        .expect("replay finishes within the timeout");

    // Frame by frame: frames are strictly ordered; within a frame the `d` map
    // order is unspecified, so compare each frame as a sorted set.
    let mut cursor = 0usize;
    for (frame_no, expected) in frames.iter().enumerate() {
        let n = expected.len();
        assert!(
            cursor + n <= received.len(),
            "frame {frame_no}: expected {n} marks, {} events left",
            received.len() - cursor
        );
        let mut got = received[cursor..cursor + n].to_vec();
        let mut want = expected.clone();
        got.sort();
        want.sort();
        assert_eq!(
            got, want,
            "frame {frame_no} marks must match the raw fixture"
        );
        cursor += n;
    }
    assert_eq!(
        cursor,
        received.len(),
        "no extra MarkPrice events beyond the independently derived sequence"
    );
}

#[tokio::test]
async fn snapshot_on_public_only_fixture_errors_naming_missing_recording() {
    let path = newest_fixture();
    let raw = std::fs::read_to_string(&path).expect("fixture readable");

    // Premise: the newest recording is public-only — no signed trading lines.
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) == Some("rest") {
            let p = v["path"].as_str().unwrap_or_default();
            assert!(
                !p.contains("/trading/"),
                "fixture must be public-only for this check, found {p}"
            );
        }
    }

    let feed = MockPerpl::from_fixture(&path).expect("fixture loads");
    let err = feed
        .snapshot()
        .await
        .expect_err("a public-only fixture cannot build an account snapshot");
    let text = format!("{err}").to_lowercase();
    assert!(
        text.contains("wallet") || text.contains("positions"),
        "error names the missing recording: {text}"
    );
}

#[tokio::test]
async fn spec_mt9_example_maps_to_mark_price_event() {
    // Verifier-owned fixture: SPEC §3.5's mt:9 example, replayed through the
    // public PerplFeed API only.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/perpl/verifier/mt9_spec_example.jsonl");
    let feed = MockPerpl::from_fixture(&path).expect("verifier fixture loads");
    let mut rx = feed.stream().await;

    let mut events = Vec::new();
    let collect = async {
        while let Some(event) = rx.recv().await {
            match event {
                FeedEvent::Market(MarketEvent::MarkPrice {
                    market_id,
                    price,
                    ts,
                }) => {
                    events.push((market_id.0, price, ts));
                }
                other => panic!("verifier fixture must only emit MarkPrice, got {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), collect)
        .await
        .expect("replay finishes within the timeout");

    assert_eq!(
        events,
        vec![
            (32, dec("2713.70"), ts_ms(1791235369000)),
            (32, dec("2713.71"), ts_ms(1791235370000)),
        ],
        "mrk 271370 → 2713.70 with ts from `at.t`, in frame order"
    );
}
