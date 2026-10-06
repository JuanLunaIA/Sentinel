//! P09 adversarial verification — independent black-box corpus for the Nansen
//! x402 client (SPEC-P09.md).
//!
//! Written from `SPEC-P09.md` + the frozen public API + the **recorded** live
//! fixtures (`docs/evidence/p01-nansen-402*`) only; the sibling implementations
//! (`nansen/{x402,client,cache,spend,mod}.rs`) are treated as black boxes.
//! The EIP-712 expectation is NOT taken from our code: it is pinned from an
//! independent oracle run (ethers 6.17.0). Script + exact command:
//! `tests/fixtures/nansen/eip712_oracle.js`:
//!
//! ```text
//! cp tests/fixtures/nansen/eip712_oracle.js ~/.hermes/cache/scratch/oracle/
//! cd ~/.hermes/cache/scratch/oracle && node eip712_oracle.js
//! ```
//!
//! Coverage (SPEC-P09 §7 verifier row):
//! 1. recorded body parse; base64 header fallback (incl. no-pad); body-first.
//! 2. rail selection: duplicate rails ⇒ first; non-`exact` scheme skipped.
//! 3. EIP-712 signature vector vs the independent oracle + alloy recovery.
//! 4. wiremock flow: 402 → signed retry → settlement; tamper binding.
//! 5. budget boundary + sliding-window edge via a tempfile ledger.
//! 6. cache: second identical call issues ZERO HTTP.
//! 7. `smart_money_context`: success fills + proxy note; failure ⇒ unavailable.
//! 8. `test-nansen --check` bin surface against a bogus base URL.
//!
//! The P07/P08 regression gates (`brain_adversarial`, `chain_adversarial`) run
//! as separate cargo test invocations.

use std::process::Command;
use std::str::FromStr;

use alloy::primitives::{Address, B256, Signature, U256, keccak256};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rust_decimal::Decimal;
use sentinel::brain::prompts::SmartMoneyContext;
use sentinel::config::{NansenConfig, SecretString};
use sentinel::error::{NansenError, SentinelError};
use sentinel::nansen::NansenClient;
use sentinel::nansen::cache::{TtlCache, key as cache_key};
use sentinel::nansen::spend::{SpendEntry, SpendLedger};
use sentinel::nansen::x402::{
    Authorization, ExactEvmPayload, HEADER_PAYMENT_REQUIRED, HEADER_PAYMENT_RESPONSE,
    HEADER_PAYMENT_SIGNATURE, PayerSigner, PaymentPayload, PaymentRequired, PaymentRequirements,
    ResourceInfo, SettlementResponse, build_authorization, cost_usd, decode_payment_signature,
    encode_payload, fetch_with_payment, parse_payment_required, select_rail,
};
use serde_json::{Value, json};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Independent-oracle pinned constants (ethers 6.17.0; see file header).
// ---------------------------------------------------------------------------

/// `wallet.address` for `TEST_KEY` (checksummed).
const ORACLE_SIGNER_ADDRESS: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
/// `ethers.TypedDataEncoder.hash(domain, types, message)` for the fixed vector.
const ORACLE_DIGEST: &str = "0xab74a1066d05d2a495f2d5938ba4db686a7039cbf63cd6e456a2f11c641c0951";
/// `wallet.signTypedData(...)` for the fixed vector (65-byte, v = 0x1c = 28).
const ORACLE_SIGNATURE: &str = "0xff34689facaa18cc7c1d0547639a216a049f38eec5f5b4ba75f09f6fa41e77405b3e12a037d6ac80c66cf6385cbd1b5169c370307f328663d374af55fc6907641c";

/// Well-known public test key (hardhat account #1); never a real wallet.
const TEST_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

const NETWORK: &str = "eip155:143";
const MONAD_ASSET: &str = "0x754704Bc059F8C67012fEd69BC8A327a5aafb603";
const PAY_TO: &str = "0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f";
const PROXY_NOTE: &str =
    "cross-venue smart-money signal as directional proxy (Nansen coverage is not Perpl-specific)";

/// Fixed `now` (epoch ms; whole second to avoid sub-second skew ambiguity).
const NOW_MS: u64 = 1_740_672_000_000;

const RECORDED_PROFILER_402: &str =
    include_str!("../../../docs/evidence/p01-nansen-402-profiler-perp-positions.json");
const RECORDED_NETFLOW_402: &str = include_str!("../../../docs/evidence/p01-nansen-402.json");
const RECORDED_402_HEADERS: &str = include_str!("../../../docs/evidence/p01-nansen-402.headers");

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The recorded `payment-required` header value (base64 STANDARD, padded).
fn recorded_header_b64() -> String {
    RECORDED_402_HEADERS
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("payment-required:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
        .expect("recorded payment-required header present")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("valid decimal")
}

/// The exact Monad rail bytes from the recorded challenges (SPEC-P09 §2).
fn monad_rail(amount: &str) -> PaymentRequirements {
    PaymentRequirements {
        scheme: "exact".to_string(),
        network: NETWORK.to_string(),
        asset: MONAD_ASSET.to_string(),
        amount: amount.to_string(),
        pay_to: PAY_TO.to_string(),
        max_timeout_seconds: 300,
        extra: json!({"name": "USDC", "version": "2"}),
    }
}

fn base_rail(amount: &str) -> PaymentRequirements {
    PaymentRequirements {
        scheme: "exact".to_string(),
        network: "eip155:8453".to_string(),
        asset: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string(),
        amount: amount.to_string(),
        pay_to: PAY_TO.to_string(),
        max_timeout_seconds: 300,
        extra: json!({"name": "USD Coin", "version": "2"}),
    }
}

fn resource_info() -> ResourceInfo {
    ResourceInfo {
        url: "https://api.nansen.ai/api/v1/smart-money/netflow".to_string(),
        description: "test".to_string(),
        mime_type: String::new(),
    }
}

/// The frozen verifier vector's domain source (rail) and authorization.
fn fixed_req() -> PaymentRequirements {
    monad_rail("10000")
}

fn fixed_authorization() -> Authorization {
    Authorization {
        from: "0x1111111111111111111111111111111111111111".to_string(),
        to: "0x2222222222222222222222222222222222222222".to_string(),
        value: "10000".to_string(),
        valid_after: "1740672089".to_string(),
        valid_before: "1740672389".to_string(),
        nonce: "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
    }
}

// ---- independent EIP-712 digest (hand-rolled; no implementation code) ------

fn word_address(addr: &str) -> [u8; 32] {
    let parsed = Address::from_str(addr).expect("valid address");
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(parsed.as_slice());
    word
}

fn word_u256(decimal: &str) -> [u8; 32] {
    U256::from_str(decimal)
        .expect("decimal uint256")
        .to_be_bytes::<32>()
}

fn word_bytes32(hex_str: &str) -> [u8; 32] {
    B256::from_str(hex_str).expect("bytes32 hex").0
}

/// EIP-712 digest for `TransferWithAuthorization` under the rail's domain:
/// `keccak256(0x1901 ‖ domainSeparator ‖ structHash)` (EIP-3009/EIP-712).
fn eip712_digest(rail: &PaymentRequirements, auth: &Authorization) -> B256 {
    let name = rail
        .extra
        .get("name")
        .and_then(Value::as_str)
        .expect("domain name in rail extra");
    let version = rail
        .extra
        .get("version")
        .and_then(Value::as_str)
        .expect("domain version in rail extra");
    let chain_id: u64 = rail
        .network
        .strip_prefix("eip155:")
        .expect("CAIP-2 eip155 network")
        .parse()
        .expect("chain id");

    let domain_type_hash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::with_capacity(5 * 32);
    domain.extend_from_slice(domain_type_hash.as_slice());
    domain.extend_from_slice(keccak256(name.as_bytes()).as_slice());
    domain.extend_from_slice(keccak256(version.as_bytes()).as_slice());
    domain.extend_from_slice(&word_u256(&chain_id.to_string()));
    domain.extend_from_slice(&word_address(&rail.asset));
    let domain_separator = keccak256(domain);

    let struct_type_hash = keccak256(
        b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
    );
    let mut encoded = Vec::with_capacity(7 * 32);
    encoded.extend_from_slice(struct_type_hash.as_slice());
    encoded.extend_from_slice(&word_address(&auth.from));
    encoded.extend_from_slice(&word_address(&auth.to));
    encoded.extend_from_slice(&word_u256(&auth.value));
    encoded.extend_from_slice(&word_u256(&auth.valid_after));
    encoded.extend_from_slice(&word_u256(&auth.valid_before));
    encoded.extend_from_slice(&word_bytes32(&auth.nonce));
    let struct_hash = keccak256(encoded);

    let mut prefixed = Vec::with_capacity(66);
    prefixed.extend_from_slice(&[0x19, 0x01]);
    prefixed.extend_from_slice(domain_separator.as_slice());
    prefixed.extend_from_slice(struct_hash.as_slice());
    keccak256(prefixed)
}

fn recover_or_none(digest: &B256, sig_hex: &str) -> Option<Address> {
    let raw = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    Signature::from_str(raw)
        .ok()
        .and_then(|sig| sig.recover_address_from_prehash(digest).ok())
}

fn recover_address(digest: &B256, sig_hex: &str) -> Address {
    recover_or_none(digest, sig_hex).expect("signature must recover to an address")
}

fn assert_signature_shape(sig: &str) {
    assert!(sig.starts_with("0x"), "signature must be 0x-hex: {sig}");
    let raw = hex::decode(sig.strip_prefix("0x").unwrap()).expect("signature hex");
    assert_eq!(raw.len(), 65, "signature must be 65 bytes");
    assert!(
        raw[64] == 27 || raw[64] == 28,
        "v must be 27/28, got {}",
        raw[64]
    );
}

fn assert_authorization_matches(
    auth: &Authorization,
    rail: &PaymentRequirements,
    payer: &str,
    now_ms: u64,
) {
    assert_eq!(auth.from, payer, "authorization.from = payer");
    assert_eq!(auth.to, rail.pay_to, "authorization.to = rail payTo");
    assert_eq!(auth.value, rail.amount, "authorization.value = rail amount");
    let valid_after: u64 = auth.valid_after.parse().expect("validAfter u64");
    let valid_before: u64 = auth.valid_before.parse().expect("validBefore u64");
    let now_s = now_ms / 1000;
    assert_eq!(valid_after, now_s - 60, "validAfter = now − 60 s (skew)");
    assert_eq!(
        valid_before,
        now_s + rail.max_timeout_seconds,
        "validBefore = now + maxTimeoutSeconds"
    );
    assert_eq!(
        valid_before - valid_after,
        rail.max_timeout_seconds + 60,
        "auth window covers the full rail timeout plus skew"
    );
    let nonce = auth.nonce.strip_prefix("0x").expect("nonce 0x prefix");
    assert_eq!(nonce.len(), 64, "nonce is 32 bytes");
}

// ---- wiremock flow helpers -------------------------------------------------

fn settlement_b64(tx: &str) -> String {
    B64.encode(
        json!({
            "success": true,
            "transaction": tx,
            "network": NETWORK,
            "payer": ORACLE_SIGNER_ADDRESS,
        })
        .to_string(),
    )
}

fn settled_200(tx: &str, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .append_header(HEADER_PAYMENT_RESPONSE, settlement_b64(tx).as_str())
        .set_body_json(body)
}

/// Mount a paid flow: 402 challenge (body + optional recorded header) first,
/// then the given response for the `PAYMENT-SIGNATURE` retry.
async fn mount_paid_flow(
    server: &MockServer,
    endpoint: &str,
    challenge_body: &[u8],
    challenge_header: Option<&str>,
    retry_response: ResponseTemplate,
) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .and(header_exists(HEADER_PAYMENT_SIGNATURE))
        .respond_with(retry_response)
        .with_priority(1)
        .mount(server)
        .await;

    let mut challenge =
        ResponseTemplate::new(402).set_body_raw(challenge_body.to_vec(), "application/json");
    if let Some(b64) = challenge_header {
        challenge = challenge.append_header(HEADER_PAYMENT_REQUIRED, b64);
    }
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(challenge)
        .up_to_n_times(1)
        .with_priority(2)
        .mount(server)
        .await;
}

fn test_config(base_url: &str, max_calls_per_hour: u32, cache_ttl_secs: u64) -> NansenConfig {
    NansenConfig {
        base_url: base_url.to_string(),
        payer_key: SecretString::new(TEST_KEY),
        cache_ttl_secs,
        max_calls_per_hour,
        payment_network: NETWORK.to_string(),
    }
}

fn ledger_entry(ts_ms: u64, cost_usd: &str) -> SpendEntry {
    SpendEntry {
        ts_ms,
        endpoint: "/api/v1/smart-money/netflow".to_string(),
        cost_usd: cost_usd.to_string(),
        tx_hash: Some("0xtx".to_string()),
        payer: Some(ORACLE_SIGNER_ADDRESS.to_string()),
        network: Some(NETWORK.to_string()),
    }
}

const NETFLOW_PATH: &str = "/api/v1/smart-money/netflow";

// ---------------------------------------------------------------------------
// 1. 402 parse: recorded body, header fallback (no-pad), body-first
// ---------------------------------------------------------------------------

#[test]
fn parse_recorded_profiler_body_monad_rail_exact() {
    let req = parse_payment_required(402, RECORDED_PROFILER_402.as_bytes(), None)
        .expect("recorded 402 body parses");
    assert_eq!(req.x402_version, 2);
    assert_eq!(req.error.as_deref(), Some("Payment required"));
    assert_eq!(req.accepts.len(), 8, "recorded challenge has 8 rails");
    assert_eq!(
        req.resource.url,
        "https://api.nansen.ai/api/v1/profiler/perp-positions"
    );
    let monad = req
        .accepts
        .iter()
        .find(|rail| rail.network == NETWORK)
        .expect("eip155:143 rail present");
    assert_eq!(
        monad,
        &monad_rail("10000"),
        "Monad rail must match byte-for-byte"
    );
}

#[test]
fn parse_malformed_body_uses_recorded_header() {
    let header = recorded_header_b64();
    let req = parse_payment_required(402, b"{ this is not json", Some(&header))
        .expect("header fallback parses");
    let expected: PaymentRequired = serde_json::from_slice(&B64.decode(&header).unwrap())
        .expect("recorded header decodes to the challenge JSON");
    assert_eq!(
        req, expected,
        "header fallback yields the header's challenge"
    );

    let monad = req
        .accepts
        .iter()
        .find(|rail| rail.network == NETWORK)
        .expect("eip155:143 rail present");
    assert_eq!(monad.amount, "50000");
    assert_eq!(monad.asset, MONAD_ASSET);
    assert_eq!(monad.pay_to, PAY_TO);
    assert_eq!(monad.max_timeout_seconds, 300);
    assert_eq!(monad.extra, json!({"name": "USDC", "version": "2"}));
    assert_eq!(
        req.resource.url,
        "https://api.nansen.ai/api/v1/smart-money/netflow"
    );
}

#[test]
fn parse_valid_body_wins_over_header() {
    let header = recorded_header_b64();
    let mut body: Value = serde_json::from_str(RECORDED_PROFILER_402).unwrap();
    body["resource"]["description"] = json!("BODY-VARIANT-MARKER");
    let bytes = serde_json::to_vec(&body).unwrap();

    let req = parse_payment_required(402, &bytes, Some(&header)).expect("body parses");
    assert_eq!(req.resource.description, "BODY-VARIANT-MARKER");
    assert_eq!(
        req.resource.url, "https://api.nansen.ai/api/v1/profiler/perp-positions",
        "body resource wins over the header's (netflow) resource"
    );
    let monad = req
        .accepts
        .iter()
        .find(|rail| rail.network == NETWORK)
        .unwrap();
    assert_eq!(
        monad.amount, "10000",
        "body rail amount, not the header's 50000"
    );
}

#[test]
fn parse_rejects_when_neither_side_parses() {
    let header = recorded_header_b64();
    let b64_text = B64.encode("plain text, not json");
    let cases: Vec<(u16, &[u8], Option<&str>)> = vec![
        (402, b"", None),
        (402, b"{", Some("!!!not-base64!!!")),
        (402, b"{", Some(b64_text.as_str())),
        // a valid body on a non-402 status is still a Challenge (SPEC §3.1)
        (200, RECORDED_PROFILER_402.as_bytes(), Some(header.as_str())),
    ];
    for (status, body, hdr) in cases {
        let err = parse_payment_required(status, body, hdr).expect_err("must be a Challenge");
        assert!(
            matches!(err, SentinelError::Nansen(NansenError::Challenge(_))),
            "status {status}: expected Challenge, got {err:?}"
        );
    }
}

#[test]
fn parse_header_tolerates_missing_padding() {
    let header = recorded_header_b64();
    assert!(
        header.ends_with('='),
        "recorded header should carry padding"
    );
    let no_pad = header.trim_end_matches('=');
    let padded = parse_payment_required(402, b"", Some(&header)).unwrap();
    let tolerant = parse_payment_required(402, b"", Some(no_pad))
        .expect("no-pad base64 must decode (SPEC §3.1)");
    assert_eq!(padded, tolerant);
}

// ---------------------------------------------------------------------------
// 2. rail selection
// ---------------------------------------------------------------------------

#[test]
fn select_rail_duplicate_monad_rails_selects_first() {
    let req = PaymentRequired {
        x402_version: 2,
        error: None,
        resource: resource_info(),
        accepts: vec![
            monad_rail("50000"),
            monad_rail("50000"),
            base_rail("1000000"),
        ],
    };
    let selected = select_rail(&req, NETWORK).expect("Monad rail selectable");
    assert!(
        std::ptr::eq(selected, &req.accepts[0]),
        "the FIRST identical rail must be selected"
    );
    assert_eq!(selected.amount, "50000");
}

#[test]
fn select_rail_skips_non_exact_scheme() {
    let mut upto = monad_rail("50000");
    upto.scheme = "upto".to_string();

    // non-exact for our network is skipped; the exact one (even later) wins
    let req = PaymentRequired {
        x402_version: 2,
        error: None,
        resource: resource_info(),
        accepts: vec![upto.clone(), monad_rail("10000")],
    };
    let selected = select_rail(&req, NETWORK).expect("exact rail selectable");
    assert!(
        std::ptr::eq(selected, &req.accepts[1]),
        "scheme != exact must be skipped"
    );

    // only a non-exact rail for our network ⇒ Challenge
    let req = PaymentRequired {
        x402_version: 2,
        error: None,
        resource: resource_info(),
        accepts: vec![upto, base_rail("10000")],
    };
    let err = select_rail(&req, NETWORK).expect_err("no exact rail for network");
    assert!(
        matches!(err, SentinelError::Nansen(NansenError::Challenge(_))),
        "{err:?}"
    );

    // no rail at all for our network ⇒ Challenge
    let req = PaymentRequired {
        x402_version: 2,
        error: None,
        resource: resource_info(),
        accepts: vec![base_rail("10000")],
    };
    let err = select_rail(&req, NETWORK).expect_err("network missing");
    assert!(
        matches!(err, SentinelError::Nansen(NansenError::Challenge(_))),
        "{err:?}"
    );
}

#[test]
fn cost_usd_table() {
    assert_eq!(cost_usd("50000"), dec("0.05"));
    assert_eq!(cost_usd("10000"), dec("0.01"));
    assert_eq!(cost_usd("0"), Decimal::ZERO);
    assert_eq!(cost_usd("1000000"), dec("1"));
    assert_eq!(cost_usd("123456789"), dec("123.456789"));
}

// ---------------------------------------------------------------------------
// 3. EIP-712 vs the independent oracle
// ---------------------------------------------------------------------------

#[test]
fn eip712_signature_matches_independent_oracle() {
    let signer = PayerSigner::from_hex(TEST_KEY).expect("test key parses");
    assert_eq!(signer.address(), ORACLE_SIGNER_ADDRESS);

    let rail = fixed_req();
    let auth = fixed_authorization();

    let signature = signer
        .sign_authorization(&rail, &auth)
        .expect("signing succeeds");
    assert_eq!(
        signature, ORACLE_SIGNATURE,
        "client signature must byte-equal the ethers-6.17 oracle signature"
    );
    assert_signature_shape(&signature);

    // our own hand-rolled EIP-712 digest must equal the oracle digest
    let digest = eip712_digest(&rail, &auth);
    assert_eq!(digest.to_string(), ORACLE_DIGEST);

    // alloy recovery of the client signature over that digest returns the signer
    let recovered = recover_address(&digest, &signature);
    assert_eq!(recovered, Address::from_str(ORACLE_SIGNER_ADDRESS).unwrap());

    // deterministic ECDSA: signing the same vector twice is byte-identical
    let again = signer.sign_authorization(&rail, &auth).unwrap();
    assert_eq!(signature, again);
}

#[test]
fn payer_signer_debug_redacts_key() {
    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let debug = format!("{signer:?}");
    assert!(
        !debug.contains("59c6995e"),
        "key material leaked in Debug: {debug}"
    );
    assert!(
        !debug.contains(&TEST_KEY[2..]),
        "key material leaked in Debug: {debug}"
    );

    for bad in ["nope", "0x1234", "0x", ""] {
        let err = PayerSigner::from_hex(bad).expect_err("malformed key must fail");
        assert!(
            matches!(err, SentinelError::Nansen(NansenError::Sign(_))),
            "key {bad:?}: expected Sign error, got {err:?}"
        );
    }
}

#[test]
fn build_authorization_fields_skew_and_nonce() {
    let rail = monad_rail("50000");
    let auth = build_authorization(&rail, ORACLE_SIGNER_ADDRESS, NOW_MS);
    assert_authorization_matches(&auth, &rail, ORACLE_SIGNER_ADDRESS, NOW_MS);

    let other = build_authorization(&rail, ORACLE_SIGNER_ADDRESS, NOW_MS);
    assert_ne!(auth.nonce, other.nonce, "nonce must be random per build");
}

#[test]
fn payload_encode_decode_roundtrip_wire_casing() {
    let payload = PaymentPayload {
        x402_version: 2,
        resource: resource_info(),
        accepted: monad_rail("10000"),
        payload: ExactEvmPayload {
            signature: ORACLE_SIGNATURE.to_string(),
            authorization: fixed_authorization(),
        },
    };
    let encoded = encode_payload(&payload);
    let raw = B64.decode(&encoded).expect("payload is base64 STANDARD");
    let json_value: Value = serde_json::from_slice(&raw).expect("payload is JSON");

    assert!(json_value.get("x402Version").is_some());
    let accepted = json_value.get("accepted").unwrap();
    assert!(accepted.get("payTo").is_some());
    assert!(accepted.get("maxTimeoutSeconds").is_some());
    let auth = json_value
        .get("payload")
        .unwrap()
        .get("authorization")
        .unwrap();
    assert!(auth.get("validAfter").is_some());
    assert!(auth.get("validBefore").is_some());

    let back = decode_payment_signature(&encoded).expect("round-trip decodes");
    assert_eq!(back, payload, "encode → decode round-trip is lossless");
}

#[test]
fn decode_payment_signature_rejects_garbage() {
    let not_json = B64.encode(b"not json");
    for bad in ["", "!!!not-base64!!!", not_json.as_str()] {
        let err = decode_payment_signature(bad).expect_err("garbage must fail");
        assert!(
            matches!(err, SentinelError::Nansen(NansenError::Challenge(_))),
            "input {bad:?}: expected Challenge, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. wiremock paid flow + tamper binding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flow_recorded_402_signed_retry_settles() {
    let server = MockServer::start().await;
    let tx = format!("0x{}", "ab".repeat(32));
    mount_paid_flow(
        &server,
        "/api/v1/profiler/perp-positions",
        RECORDED_PROFILER_402.as_bytes(),
        None,
        settled_200(&tx, json!({"data": []})),
    )
    .await;

    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/v1/profiler/perp-positions", server.uri());
    let request_body = json!({"addresses": []});
    let paid = fetch_with_payment(&http, &url, &request_body, &signer, NETWORK, NOW_MS)
        .await
        .expect("paid flow succeeds");

    assert_eq!(paid.cost_usd, dec("0.01"));
    assert_eq!(paid.rail_network, NETWORK);
    assert_eq!(paid.body, json!({"data": []}));
    assert_eq!(
        paid.settlement,
        Some(SettlementResponse {
            success: true,
            transaction: tx.clone(),
            network: NETWORK.to_string(),
            payer: ORACLE_SIGNER_ADDRESS.to_string(),
        })
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one unpaid request + one signed retry");

    // initial request: JSON body + X-Payer-Address, NO payment signature
    assert!(requests[0].headers.get(HEADER_PAYMENT_SIGNATURE).is_none());
    assert_eq!(
        requests[0]
            .headers
            .get("X-Payer-Address")
            .unwrap()
            .to_str()
            .unwrap(),
        signer.address()
    );
    assert_eq!(requests[0].body_json::<Value>().unwrap(), request_body);

    // retry: both headers present
    assert!(requests[1].headers.get(HEADER_PAYMENT_SIGNATURE).is_some());
    assert_eq!(
        requests[1]
            .headers
            .get("X-Payer-Address")
            .unwrap()
            .to_str()
            .unwrap(),
        signer.address()
    );

    // decode the payload we actually put on the wire
    let payload = decode_payment_signature(
        requests[1]
            .headers
            .get(HEADER_PAYMENT_SIGNATURE)
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload.x402_version, 2);
    let fixture: PaymentRequired = serde_json::from_str(RECORDED_PROFILER_402).unwrap();
    assert_eq!(
        payload.resource, fixture.resource,
        "resource echoed verbatim"
    );
    let expected_rail = monad_rail("10000");
    assert_eq!(
        payload.accepted, expected_rail,
        "accepted rail echoed verbatim"
    );
    assert_authorization_matches(
        &payload.payload.authorization,
        &expected_rail,
        &signer.address(),
        NOW_MS,
    );
    assert_signature_shape(&payload.payload.signature);
}

#[tokio::test]
async fn flow_retry_without_settlement_header() {
    let server = MockServer::start().await;
    mount_paid_flow(
        &server,
        "/api/v1/profiler/perp-positions",
        RECORDED_PROFILER_402.as_bytes(),
        None,
        ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
    )
    .await;

    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/v1/profiler/perp-positions", server.uri());
    let paid = fetch_with_payment(&http, &url, &json!({}), &signer, NETWORK, NOW_MS)
        .await
        .unwrap();
    assert_eq!(
        paid.settlement, None,
        "absent PAYMENT-RESPONSE ⇒ settlement None"
    );
    assert_eq!(paid.cost_usd, dec("0.01"));
}

#[tokio::test]
async fn flow_retry_non_200_is_retry_error() {
    let server = MockServer::start().await;
    mount_paid_flow(
        &server,
        "/api/v1/profiler/perp-positions",
        RECORDED_PROFILER_402.as_bytes(),
        None,
        ResponseTemplate::new(403),
    )
    .await;

    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/v1/profiler/perp-positions", server.uri());
    let err = fetch_with_payment(&http, &url, &json!({}), &signer, NETWORK, NOW_MS)
        .await
        .expect_err("post-payment 403 must fail");
    assert!(
        matches!(
            err,
            SentinelError::Nansen(NansenError::Retry { status: 403 })
        ),
        "expected Retry{{403}}, got {err:?}"
    );
}

#[tokio::test]
async fn flow_header_fallback_end_to_end() {
    let server = MockServer::start().await;
    let header = recorded_header_b64();
    mount_paid_flow(
        &server,
        NETFLOW_PATH,
        b"{ malformed challenge body",
        Some(&header),
        ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
    )
    .await;

    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}{NETFLOW_PATH}", server.uri());
    let paid = fetch_with_payment(
        &http,
        &url,
        &json!({"chains": ["ethereum"]}),
        &signer,
        NETWORK,
        NOW_MS,
    )
    .await
    .expect("header fallback drives the flow");

    // the header's rail is the generic one: amount 50000 ⇒ $0.05
    assert_eq!(paid.cost_usd, dec("0.05"));
    assert_eq!(paid.rail_network, NETWORK);

    let requests = server.received_requests().await.unwrap();
    let payload = decode_payment_signature(
        requests[1]
            .headers
            .get(HEADER_PAYMENT_SIGNATURE)
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let fixture: PaymentRequired = serde_json::from_str(RECORDED_NETFLOW_402).unwrap();
    assert_eq!(payload.resource, fixture.resource);
    assert_eq!(payload.accepted, monad_rail("50000"));
}

#[tokio::test]
async fn tamper_accepted_echo_binds_signed_rail() {
    let server = MockServer::start().await;

    // Challenge = recorded profiler body + an appended swapped-amount rail
    // ("amount swapped after signing must NOT be the accepted one").
    let mut challenge: Value = serde_json::from_str(RECORDED_PROFILER_402).unwrap();
    let mut swap_rail = challenge["accepts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rail| rail["network"] == json!(NETWORK))
        .unwrap()
        .clone();
    swap_rail["amount"] = json!("999999");
    challenge["accepts"].as_array_mut().unwrap().push(swap_rail);
    let challenge_bytes = serde_json::to_vec(&challenge).unwrap();

    mount_paid_flow(
        &server,
        "/api/v1/profiler/perp-positions",
        &challenge_bytes,
        None,
        ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
    )
    .await;

    let signer = PayerSigner::from_hex(TEST_KEY).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/v1/profiler/perp-positions", server.uri());
    let paid = fetch_with_payment(&http, &url, &json!({}), &signer, NETWORK, NOW_MS)
        .await
        .unwrap();
    assert!(paid.settlement.is_none());

    let requests = server.received_requests().await.unwrap();
    let payload = decode_payment_signature(
        requests[1]
            .headers
            .get(HEADER_PAYMENT_SIGNATURE)
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();

    // `accepted` echoes the ORIGINAL first rail byte-for-byte, never the swap.
    assert_eq!(payload.accepted, monad_rail("10000"));
    assert_ne!(payload.accepted.amount, "999999");
    assert_eq!(payload.payload.authorization.value, "10000");
    assert_eq!(payload.payload.authorization.value, payload.accepted.amount);

    // The signature commits to (accepted rail domain + authorization):
    // recovery over the ORIGINAL wire values returns the signer …
    let digest = eip712_digest(&payload.accepted, &payload.payload.authorization);
    assert_eq!(
        recover_address(&digest, &payload.payload.signature).to_string(),
        ORACLE_SIGNER_ADDRESS
    );

    // … and stops doing so once any *signed* field is swapped post-signing:
    // the authorization value …
    let signer_address = Address::from_str(ORACLE_SIGNER_ADDRESS).unwrap();
    let mut swapped_auth = payload.payload.authorization.clone();
    swapped_auth.value = "999999".to_string();
    let swapped_auth_digest = eip712_digest(&payload.accepted, &swapped_auth);
    assert_ne!(
        recover_or_none(&swapped_auth_digest, &payload.payload.signature),
        Some(signer_address),
        "signature must not verify against a swapped authorization value"
    );

    // … and the rail-derived domain (asset/name/version/chainId).
    let mut swapped_domain = payload.accepted.clone();
    swapped_domain.asset = "0x0000000000000000000000000000000000000001".to_string();
    let swapped_domain_digest = eip712_digest(&swapped_domain, &payload.payload.authorization);
    assert_ne!(
        recover_or_none(&swapped_domain_digest, &payload.payload.signature),
        Some(signer_address),
        "signature must not verify against a swapped accepted-rail domain"
    );

    // A pure `accepted.amount`-only swap does NOT invalidate the signature:
    // the rail amount is not an EIP-712 field (only the domain comes from the
    // rail). Such a tamper is caught by the `accepted.amount ==
    // authorization.value` echo invariant asserted above, not by recovery —
    // pinned here so the detector's design is explicit.
    let mut amount_only_swap = payload.accepted.clone();
    amount_only_swap.amount = "999999".to_string();
    let amount_only_digest = eip712_digest(&amount_only_swap, &payload.payload.authorization);
    assert_eq!(
        recover_or_none(&amount_only_digest, &payload.payload.signature),
        Some(signer_address),
        "accepted.amount is not a signed EIP-712 field; the echo invariant is the detector"
    );
}

// ---------------------------------------------------------------------------
// 5./6. client: budget, ledger, cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_cache_second_call_issues_zero_http() {
    let server = MockServer::start().await;
    let tx = format!("0x{}", "cd".repeat(32));
    mount_paid_flow(
        &server,
        NETFLOW_PATH,
        RECORDED_NETFLOW_402.as_bytes(),
        None,
        settled_200(&tx, json!({"net_flow_usd": -123.5})),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("spend.jsonl");
    let client = NansenClient::new(&test_config(&server.uri(), 10, 300))
        .unwrap()
        .with_ledger_path(ledger_path.clone());

    let (first, meta1) = client.sm_netflow("ethereum", NOW_MS).await.unwrap();
    assert!(!meta1.cached, "first call is paid");
    assert_eq!(meta1.cost_usd, dec("0.05"));
    assert_eq!(meta1.tx_hash.as_deref(), Some(tx.as_str()));
    assert!(meta1.endpoint.contains(NETFLOW_PATH));
    assert_eq!(first, json!({"net_flow_usd": -123.5}));

    let http_after_first = server.received_requests().await.unwrap().len();
    assert_eq!(http_after_first, 2, "unpaid + signed retry");

    // identical call within TTL ⇒ cache hit, zero HTTP, no budget spend
    let (second, meta2) = client.sm_netflow("ethereum", NOW_MS + 1_000).await.unwrap();
    assert_eq!(second, first);
    assert!(
        meta2.cached,
        "second identical call must be served from cache"
    );
    assert_eq!(meta2.cost_usd, Decimal::ZERO);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        http_after_first,
        "cache hit must issue ZERO HTTP requests"
    );

    let entries = SpendLedger::new(ledger_path).load();
    assert_eq!(entries.len(), 1, "cache hit must not append to the ledger");
    assert_eq!(entries[0].cost_usd, "0.05");
    assert_eq!(entries[0].ts_ms, NOW_MS);
    assert_eq!(entries[0].payer.as_deref(), Some(ORACLE_SIGNER_ADDRESS));
    assert_eq!(entries[0].network.as_deref(), Some(NETWORK));
    assert_eq!(entries[0].tx_hash.as_deref(), Some(tx.as_str()));
    assert!(entries[0].endpoint.contains(NETFLOW_PATH));
}

#[tokio::test]
async fn client_budget_two_entries_at_limit_blocks_before_http() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("spend.jsonl");
    let ledger = SpendLedger::new(&ledger_path);
    ledger
        .append(&ledger_entry(NOW_MS - 1_000, "0.05"))
        .unwrap();
    ledger
        .append(&ledger_entry(NOW_MS - 2_000, "0.05"))
        .unwrap();

    let client = NansenClient::new(&test_config(&server.uri(), 2, 300))
        .unwrap()
        .with_ledger_path(ledger_path);

    let err = client.sm_netflow("ethereum", NOW_MS).await.unwrap_err();
    match err {
        SentinelError::Nansen(NansenError::Budget { spent, limit }) => {
            assert_eq!(spent, 2);
            assert_eq!(limit, 2);
        }
        other => panic!("expected Budget{{spent:2,limit:2}}, got {other:?}"),
    }
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "budget must be enforced before ANY HTTP"
    );
}

#[tokio::test]
async fn client_budget_window_edge_is_excluded() {
    let server = MockServer::start().await;
    let tx = format!("0x{}", "ef".repeat(32));
    mount_paid_flow(
        &server,
        NETFLOW_PATH,
        RECORDED_NETFLOW_402.as_bytes(),
        None,
        settled_200(&tx, json!({"net_flow_usd": -1.0})),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("spend.jsonl");
    // exactly at the window edge ⇒ outside (`now - ts < window`, not <=)
    SpendLedger::new(&ledger_path)
        .append(&ledger_entry(NOW_MS - 3_600_000, "0.05"))
        .unwrap();

    let client = NansenClient::new(&test_config(&server.uri(), 2, 300))
        .unwrap()
        .with_ledger_path(ledger_path.clone());
    let (_value, meta) = client
        .sm_netflow("ethereum", NOW_MS)
        .await
        .expect("edge entry excluded ⇒ below max ⇒ call allowed");
    assert!(!meta.cached);
    assert_eq!(
        SpendLedger::new(&ledger_path).load().len(),
        2,
        "edge entry + this settled call"
    );
}

#[test]
fn ledger_window_boundary_append_format_and_tolerance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("spend.jsonl");
    let ledger = SpendLedger::new(&path);
    assert!(ledger.load().is_empty(), "missing file ⇒ empty ledger");

    let window = 3_600_000u64;
    let edge = ledger_entry(NOW_MS - window, "0.05"); // == window ⇒ OUTSIDE
    let inside = ledger_entry(NOW_MS - window + 1, "0.01"); // 1 ms inside
    let current = ledger_entry(NOW_MS, "0.02");
    ledger.append(&edge).unwrap();
    ledger.append(&inside).unwrap();
    ledger.append(&current).unwrap();

    assert_eq!(ledger.calls_since(NOW_MS, window), 2, "edge entry excluded");
    assert_eq!(ledger.cost_since(NOW_MS, window), dec("0.03"));

    let entries = ledger.load();
    assert_eq!(
        entries,
        vec![edge, inside, current],
        "append order + fields"
    );

    // one JSON line per entry; dirs were created on demand
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 3);
    for line in text.lines() {
        serde_json::from_str::<Value>(line).expect("each line is valid JSON");
    }

    // malformed lines are skipped, not fatal
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{ this is not json\n").unwrap();
    }
    assert_eq!(ledger.load().len(), 3, "malformed line skipped");
    assert_eq!(ledger.calls_since(NOW_MS, window), 2);
}

#[test]
fn ledger_future_dated_entry_does_not_panic() {
    // Clock skew: an entry stamped ahead of `now` (shared ledger, another
    // machine's clock) must not crash the sliding-window arithmetic.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spend.jsonl");
    let ledger = SpendLedger::new(&path);
    ledger
        .append(&ledger_entry(NOW_MS + 60_000, "0.05"))
        .unwrap();

    let calls = ledger.calls_since(NOW_MS, 3_600_000);
    let cost = ledger.cost_since(NOW_MS, 3_600_000);
    assert!(
        calls == 0 || calls == 1,
        "future entry counted ({calls}) or not — never a panic"
    );
    assert!(cost >= Decimal::ZERO);
}

#[test]
fn cache_expiry_inclusive_and_key_determinism() {
    let mut cache = TtlCache::new(300);
    cache.put("k", json!(1), 1_000);
    assert_eq!(cache.get("k", 1_299), Some(json!(1)), "inside TTL");
    assert_eq!(cache.get("k", 1_300), None, "now >= expiry ⇒ miss");
    assert_eq!(cache.len(), 0, "expired entry dropped on miss");

    cache.put("k2", json!(2), 1_000);
    assert_eq!(cache.get("k2", 1_299), Some(json!(2)));
    cache.put("k3", json!(3), 1_000);
    cache.prune(1_300);
    assert_eq!(cache.len(), 0);
    assert!(cache.is_empty());

    // key determinism: object order must not matter (canonical JSON)
    let a = cache_key("/api/v1/x", &json!({"b": 1, "a": 2}));
    let b = cache_key("/api/v1/x", &json!({"a": 2, "b": 1}));
    assert_eq!(a, b);
    assert!(a.starts_with("/api/v1/x|"), "{a}");
}

// ---------------------------------------------------------------------------
// 7. smart_money_context: never fails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smart_money_context_failure_degrades_to_unavailable() {
    // (a) initial request fails outright
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(NETFLOW_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let client = NansenClient::new(&test_config(&server.uri(), 10, 300))
        .unwrap()
        .with_ledger_path(dir.path().join("a.jsonl"));

    let ctx = client.smart_money_context("ETH", "ethereum", NOW_MS).await;
    assert_eq!(ctx, SmartMoneyContext::unavailable("ETH"));
    assert_eq!(ctx.asset, "ETH");
    assert!(ctx.netflow_24h.is_none());
    assert!(ctx.fetched_at_ms.is_none());
    assert!(ctx.total_cost_usd.is_none());

    // (b) 402 parsed, payment retry fails ⇒ still unavailable, no ledger entry
    let server2 = MockServer::start().await;
    mount_paid_flow(
        &server2,
        NETFLOW_PATH,
        RECORDED_NETFLOW_402.as_bytes(),
        None,
        ResponseTemplate::new(500),
    )
    .await;
    let dir2 = tempfile::tempdir().unwrap();
    let ledger2 = dir2.path().join("b.jsonl");
    let client2 = NansenClient::new(&test_config(&server2.uri(), 10, 300))
        .unwrap()
        .with_ledger_path(ledger2.clone());

    let ctx2 = client2.smart_money_context("ETH", "ethereum", NOW_MS).await;
    assert_eq!(ctx2, SmartMoneyContext::unavailable("ETH"));
    assert_eq!(
        server2.received_requests().await.unwrap().len(),
        2,
        "challenge + signed retry were attempted before degrading"
    );
    assert!(
        SpendLedger::new(&ledger2).load().is_empty(),
        "a failed payment must not be recorded as spend"
    );
}

#[tokio::test]
async fn smart_money_context_success_fills_fields_and_proxy_note() {
    let server = MockServer::start().await;
    let tx = format!("0x{}", "12".repeat(32));
    // Several candidate keys carry the same value: whichever the parser pins,
    // the extracted netflow must be this number (SPEC §3.2 candidate scan).
    let netflow_body = json!({
        "net_flow_usd": -123.5,
        "netflow": -123.5,
        "net_flow": -123.5,
        "total_net_flow": -123.5,
    });
    mount_paid_flow(
        &server,
        NETFLOW_PATH,
        RECORDED_NETFLOW_402.as_bytes(),
        None,
        settled_200(&tx, netflow_body),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let client = NansenClient::new(&test_config(&server.uri(), 10, 300))
        .unwrap()
        .with_ledger_path(dir.path().join("spend.jsonl"));

    let ctx = client.smart_money_context("ETH", "ethereum", NOW_MS).await;
    assert_eq!(ctx.asset, "ETH");
    assert_eq!(ctx.netflow_24h, Some(dec("-123.5")));
    assert_eq!(ctx.fetched_at_ms, Some(NOW_MS));
    assert_eq!(ctx.total_cost_usd, Some(dec("0.05")));
    assert_eq!(
        ctx.note.as_deref(),
        Some(PROXY_NOTE),
        "SPEC-P09 §3.5 honesty note"
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<Value>().unwrap(),
        json!({"chains": ["ethereum"]}),
        "FACTS-confirmed smart-money body shape"
    );
}

// ---------------------------------------------------------------------------
// 8. bin `test-nansen --check`
// ---------------------------------------------------------------------------

#[test]
fn bin_check_bogus_base_url_fails_loudly_without_key() {
    let output = Command::new(env!("CARGO_BIN_EXE_test-nansen"))
        .arg("--check")
        .env_remove("NANSEN_PAYER_KEY")
        .env("NANSEN_BASE_URL", "http://127.0.0.1:1")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("spawn test-nansen");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    let lower = combined.to_ascii_lowercase();

    assert!(
        !output.status.success(),
        "bogus base URL must exit non-zero (fail loudly); exit={:?}; output: {combined}",
        output.status.code()
    );
    assert!(
        !lower.contains("not yet implemented"),
        "bin is still a todo!() stub: {combined}"
    );
    assert!(
        !lower.contains("panicked"),
        "bin panicked instead of returning an error: {combined}"
    );
    assert!(
        lower.contains("error") || lower.contains("refused") || lower.contains("connect"),
        "expected a clear connection error for http://127.0.0.1:1; got: {combined}"
    );
    assert!(
        !lower.contains("nansen_payer_key"),
        "--check must not require the payer key; got: {combined}"
    );
}
