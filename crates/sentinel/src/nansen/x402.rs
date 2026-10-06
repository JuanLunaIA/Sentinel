//! x402 v2 payment flow — wire types, rail selection, EIP-3009 signing.
//!
//! Frozen by `SPEC-P09.md` §3.1 from the recorded live challenges
//! (`docs/evidence/p01-nansen-402*.json`) plus the v2 specification. Money is
//! decimal strings as returned on the wire; the payer key is never logged.
//!
//! **Implementation status (P09):** implemented by the `x402-core` agent and
//! proven offline against the recorded challenges (rail fields, signature
//! recovery, wiremock flow); the live paid smoke stays PENDING-WALLET until
//! the payer key is funded (STUB-16). EIP-712 typed data is hashed with
//! alloy's `sol!`/`Eip712Domain` (available with this crate's feature set),
//! not hand-rolled.
//!
//! Wire notes:
//! - the 402 challenge is parsed from the body first, then from the base64
//!   `payment-required` header (STANDARD, no-pad tolerated);
//! - the paid retry re-sends `X-Payer-Address` (promo hook, STUB-04) and adds
//!   `PAYMENT-SIGNATURE` (base64 STANDARD [`PaymentPayload`]);
//! - `PAYMENT-RESPONSE` (base64 [`SettlementResponse`]) is informational: an
//!   absent or malformed header yields `settlement: None`, never an error;
//! - a transport failure (no HTTP response at all) surfaces as
//!   [`NansenError::Retry`] with `status: 0`, a documented sentinel for
//!   "no status was received".

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::{Address, B256, U256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{Eip712Domain, SolStruct};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::{NansenError, Result};

/// Header the 402 challenge may also arrive in (base64 JSON).
pub const HEADER_PAYMENT_REQUIRED: &str = "payment-required";
/// Header the signed payment travels in (base64 `PaymentPayload`).
pub const HEADER_PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
/// Header carrying settlement info on the paid response (base64 JSON).
pub const HEADER_PAYMENT_RESPONSE: &str = "payment-response";

/// Payer address sent on the initial (unpaid) request — promo hook, not
/// advertised (STUB-04).
const HEADER_PAYER_ADDRESS: &str = "X-Payer-Address";

/// The only x402 scheme this client signs (`exact` = EIP-3009).
const SCHEME_EXACT: &str = "exact";

/// Clock-drift skew subtracted from `validAfter`.
const AUTHORIZATION_SKEW_SECS: u64 = 60;

/// Base units per USD for the stablecoin rails we accept (6-decimal
/// USDC-likes; the assumption is documented on [`cost_usd`]).
const STABLE_UNITS_PER_USD: i64 = 1_000_000;

/// 'Electrum'-notation base of the ECDSA `v` byte (EIP-3009 wants 27/28).
const SIGNATURE_V_BASE: u8 = 27;

/// Fallback entropy counter used only if the OS entropy source fails (see
/// [`random_nonce`]).
static NONCE_FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One accepted rail, echoed **verbatim** from the 402 body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequirements {
    /// `exact` (EIP-3009) for our flows.
    pub scheme: String,
    /// CAIP-2 network (`eip155:143` Monad).
    pub network: String,
    /// Token contract.
    pub asset: String,
    /// Amount in token base units, decimal string.
    pub amount: String,
    /// Destination address.
    #[serde(rename = "payTo")]
    pub pay_to: String,
    /// Validity window for the authorization, seconds.
    #[serde(rename = "maxTimeoutSeconds")]
    pub max_timeout_seconds: u64,
    /// Scheme extras (`{name, version}` drive the EIP-712 domain).
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// The v2 402 body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequired {
    /// Protocol version (2).
    #[serde(rename = "x402Version")]
    pub x402_version: u32,
    /// Server-side error string (`"Payment required"`).
    #[serde(default)]
    pub error: Option<String>,
    /// Resource identity (echoed into the payload).
    pub resource: ResourceInfo,
    /// Accepted rails.
    pub accepts: Vec<PaymentRequirements>,
}

/// Resource identity from the challenge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceInfo {
    /// Resource URL.
    pub url: String,
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// MIME type (often empty).
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
}

/// EIP-3009 `TransferWithAuthorization` parameters (wire casing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Authorization {
    /// Payer address.
    pub from: String,
    /// Destination (rail `payTo`).
    pub to: String,
    /// Amount in base units.
    pub value: String,
    /// Unix seconds after which valid.
    #[serde(rename = "validAfter")]
    pub valid_after: String,
    /// Unix seconds before which valid.
    #[serde(rename = "validBefore")]
    pub valid_before: String,
    /// 32-byte unique nonce, `0x…`.
    pub nonce: String,
}

/// Scheme payload for `exact`/EVM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExactEvmPayload {
    /// 65-byte EIP-712 signature, `0x…`.
    pub signature: String,
    /// The signed authorization.
    pub authorization: Authorization,
}

/// The v2 payment payload (base64 into `PAYMENT-SIGNATURE`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentPayload {
    /// Protocol version (2).
    #[serde(rename = "x402Version")]
    pub x402_version: u32,
    /// Resource identity, verbatim from the challenge.
    pub resource: ResourceInfo,
    /// The chosen rail, verbatim from the challenge.
    pub accepted: PaymentRequirements,
    /// Scheme payload.
    pub payload: ExactEvmPayload,
}

/// Settlement info from the `PAYMENT-RESPONSE` header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettlementResponse {
    /// Facilitator-reported success.
    pub success: bool,
    /// Settlement transaction hash, `0x…`.
    pub transaction: String,
    /// CAIP-2 network.
    pub network: String,
    /// Payer address.
    #[serde(default)]
    pub payer: String,
}

alloy::sol! {
    /// EIP-3009 `TransferWithAuthorization` — the typed data the `exact`
    /// scheme signs (field order is part of the EIP-712 type string).
    struct TransferWithAuthorization {
        address from;
        address to;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 nonce;
    }
}

/// Signs EIP-3009 authorizations with the x402 payer wallet.
pub struct PayerSigner {
    /// alloy signer (never `Debug`-printed raw).
    inner: alloy::signers::local::PrivateKeySigner,
}

impl PayerSigner {
    /// Build from a `0x…` 32-byte hex key.
    ///
    /// The `0x` prefix is optional; surrounding whitespace is ignored. The
    /// error never echoes the key (or any part of it).
    ///
    /// # Errors
    /// `NansenError::Sign` on a malformed key.
    pub fn from_hex(key: &str) -> Result<Self> {
        let trimmed = key.trim();
        let body = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if body.len() != 64 {
            return Err(NansenError::Sign(
                "payer key must be 32 bytes of hex (64 chars, `0x` optional)".into(),
            )
            .into());
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(body, &mut bytes)
            .map_err(|_| NansenError::Sign("payer key is not valid hex".into()))?;
        let inner = PrivateKeySigner::from_slice(&bytes)
            .map_err(|e| NansenError::Sign(format!("payer key was rejected: {e}")))?;
        Ok(Self { inner })
    }

    /// Payer address (`0x…`).
    pub fn address(&self) -> String {
        format!("{}", self.inner.address())
    }

    /// Sign the EIP-3009 typed data for `req` + `authorization`.
    ///
    /// The digest is `keccak256(0x1901 ‖ domainSeparator ‖ hashStruct)`
    /// computed by alloy over `TransferWithAuthorization` with the domain
    /// `{name, version}` taken from `req.extra`, `chainId` from the CAIP-2
    /// `req.network` and `verifyingContract` = `req.asset`. The returned hex
    /// is the 65-byte `r ‖ s ‖ v` signature with `v ∈ {27, 28}`.
    ///
    /// # Errors
    /// `NansenError::Sign` when the typed-data hash/sign fails.
    pub fn sign_authorization(
        &self,
        req: &PaymentRequirements,
        authorization: &Authorization,
    ) -> Result<String> {
        let digest = authorization_digest(req, authorization)?;
        let signature = self
            .inner
            .sign_hash_sync(&digest)
            .map_err(|e| NansenError::Sign(format!("EIP-3009 signing failed: {e}")))?;

        // The wire format is `r ‖ s ‖ v` with the 'Electrum' v byte (27/28);
        // alloy stores the raw y-parity, so the v byte is rebuilt explicitly.
        let mut bytes = [0_u8; 65];
        bytes[..32].copy_from_slice(&signature.r().to_be_bytes::<32>());
        bytes[32..64].copy_from_slice(&signature.s().to_be_bytes::<32>());
        bytes[64] = SIGNATURE_V_BASE + u8::from(signature.v());
        Ok(format!("0x{}", hex::encode(bytes)))
    }
}

impl fmt::Debug for PayerSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerSigner")
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

/// The EIP-712 digest [`PayerSigner::sign_authorization`] signs (kept
/// separate so tests can recover the signer without duplicating the encoding).
///
/// # Errors
/// `NansenError::Sign` when the domain or authorization cannot be encoded.
fn authorization_digest(req: &PaymentRequirements, authorization: &Authorization) -> Result<B256> {
    let domain = eip712_domain(req)?;
    let message = transfer_with_authorization(authorization)?;
    Ok(message.eip712_signing_hash(&domain))
}

/// The EIP-712 domain of a rail: `{name, version}` from `extra`, `chainId`
/// from the CAIP-2 network, `verifyingContract` = `asset`.
fn eip712_domain(req: &PaymentRequirements) -> Result<Eip712Domain> {
    let name = req
        .extra
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NansenError::Sign("rail extra is missing `name`".into()))?;
    let version = req
        .extra
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NansenError::Sign("rail extra is missing `version`".into()))?;
    let chain_id = caip2_chain_id(&req.network)?;
    let verifying_contract = parse_address(&req.asset, "rail asset")?;
    Ok(Eip712Domain::new(
        Some(Cow::Owned(name.to_owned())),
        Some(Cow::Owned(version.to_owned())),
        Some(U256::from(chain_id)),
        Some(verifying_contract),
        None,
    ))
}

/// Decode the wire [`Authorization`] into its typed-data struct.
fn transfer_with_authorization(authorization: &Authorization) -> Result<TransferWithAuthorization> {
    Ok(TransferWithAuthorization {
        from: parse_address(&authorization.from, "authorization.from")?,
        to: parse_address(&authorization.to, "authorization.to")?,
        value: parse_u256_decimal(&authorization.value, "authorization.value")?,
        validAfter: parse_u256_decimal(&authorization.valid_after, "authorization.validAfter")?,
        validBefore: parse_u256_decimal(&authorization.valid_before, "authorization.validBefore")?,
        nonce: parse_b256(&authorization.nonce, "authorization.nonce")?,
    })
}

/// `eip155:<id>` → `<id>` (the EIP-712 `chainId`).
fn caip2_chain_id(network: &str) -> Result<u64, NansenError> {
    let (namespace, reference) = network.split_once(':').ok_or_else(|| {
        NansenError::Sign(format!(
            "network `{network}` is not CAIP-2 (want `eip155:<id>`)"
        ))
    })?;
    if namespace != "eip155" {
        return Err(NansenError::Sign(format!(
            "network `{network}` is not an `eip155` chain"
        )));
    }
    reference
        .parse::<u64>()
        .map_err(|_| NansenError::Sign(format!("network `{network}` has a non-numeric chain id")))
}

/// Parse a `0x…` (checksummed) address.
fn parse_address(value: &str, what: &str) -> Result<Address, NansenError> {
    Address::from_str(value.trim())
        .map_err(|e| NansenError::Sign(format!("{what}: invalid address: {e}")))
}

/// Parse a decimal uint256 string (wire form for `value`/`validAfter`/`validBefore`).
fn parse_u256_decimal(value: &str, what: &str) -> Result<U256, NansenError> {
    U256::from_str_radix(value.trim(), 10)
        .map_err(|e| NansenError::Sign(format!("{what}: invalid decimal uint256: {e}")))
}

/// Parse a `0x…` bytes32 string (wire form for `nonce`).
fn parse_b256(value: &str, what: &str) -> Result<B256, NansenError> {
    B256::from_str(value.trim())
        .map_err(|e| NansenError::Sign(format!("{what}: invalid bytes32: {e}")))
}

/// Select the `exact` rail matching `network`.
///
/// Non-`exact` schemes are skipped; the first matching rail wins (the
/// recorded challenges carry one rail per network, duplicates resolved
/// deterministically by wire order).
///
/// # Errors
/// `NansenError::Challenge` when no acceptable rail exists.
pub fn select_rail<'a>(req: &'a PaymentRequired, network: &str) -> Result<&'a PaymentRequirements> {
    let rail = req
        .accepts
        .iter()
        .find(|rail| rail.scheme == SCHEME_EXACT && rail.network == network)
        .ok_or_else(|| {
            NansenError::Challenge(format!(
                "no `{SCHEME_EXACT}` rail for network `{network}` ({} advertised)",
                req.accepts.len()
            ))
        })?;
    Ok(rail)
}

/// Parse a 402 challenge (body JSON; base64 header fallback).
///
/// The `payment-required` header is decoded with STANDARD base64 first and
/// NO_PAD second (recorded servers are inconsistent about padding).
///
/// # Errors
/// `NansenError::Challenge` when the status is not 402 or neither the body
/// nor the header parses.
pub fn parse_payment_required(
    status: u16,
    body: &[u8],
    header_b64: Option<&str>,
) -> Result<PaymentRequired> {
    if status != 402 {
        return Err(NansenError::Challenge(format!("expected HTTP 402, got {status}")).into());
    }
    let body_error = match serde_json::from_slice::<PaymentRequired>(body) {
        Ok(parsed) => return Ok(parsed),
        Err(e) => e,
    };
    if let Some(header) = header_b64
        && let Ok(raw) = decode_base64(header)
        && let Ok(parsed) = serde_json::from_slice::<PaymentRequired>(&raw)
    {
        return Ok(parsed);
    }
    Err(NansenError::Challenge(format!(
        "402 challenge could not be parsed from the body ({body_error}) or the payment-required header"
    ))
    .into())
}

/// Decode base64 tolerating missing padding (STANDARD, then NO_PAD).
fn decode_base64(value: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    let trimmed = value.trim();
    STANDARD
        .decode(trimmed)
        .or_else(|_| STANDARD_NO_PAD.decode(trimmed))
}

/// Build the authorization for `req` signed by `payer` at `now_ms`.
///
/// `validAfter` = `now − 60 s` (skew), `validBefore` =
/// `now + maxTimeoutSeconds`, both as decimal Unix-second strings; `nonce` is
/// 32 random bytes (`getrandom`) as `0x…`.
pub fn build_authorization(req: &PaymentRequirements, payer: &str, now_ms: u64) -> Authorization {
    let now_secs = now_ms / 1_000;
    Authorization {
        from: payer.to_owned(),
        to: req.pay_to.clone(),
        value: req.amount.clone(),
        valid_after: now_secs.saturating_sub(AUTHORIZATION_SKEW_SECS).to_string(),
        valid_before: now_secs.saturating_add(req.max_timeout_seconds).to_string(),
        nonce: random_nonce(now_ms),
    }
}

/// `0x` + 32 random bytes from the OS entropy pool.
///
/// If `getrandom` fails (practically unreachable on supported targets) the
/// nonce is mixed with `now_ms` and a process-local counter instead of being
/// left all-zero, so consecutive authorizations cannot collide.
fn random_nonce(now_ms: u64) -> String {
    let mut bytes = [0_u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        bytes[..8].copy_from_slice(&now_ms.to_be_bytes());
        let n = NONCE_FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        bytes[8..16].copy_from_slice(&n.to_be_bytes());
    }
    format!("0x{}", hex::encode(bytes))
}

/// `amount / 10^6` (stablecoin rails; documented assumption).
///
/// Every `exact` rail on the recorded challenges is a 6-decimal USDC-like, so
/// base units map 1:1 to micro-USD. A non-numeric amount yields zero (the
/// wire always carries decimal strings; this is purely defensive).
pub fn cost_usd(amount: &str) -> Decimal {
    Decimal::from_str_exact(amount.trim()).unwrap_or_default() / Decimal::from(STABLE_UNITS_PER_USD)
}

/// Base64(STANDARD) of the payment payload for the retry header.
///
/// Serializing this crate's own structs cannot fail in practice; the empty
/// fallback keeps the function infallible by contract.
pub fn encode_payload(payload: &PaymentPayload) -> String {
    serde_json::to_vec(payload)
        .map(|raw| STANDARD.encode(raw))
        .unwrap_or_default()
}

/// Decode a `PAYMENT-SIGNATURE` header value (tests/verifier).
///
/// # Errors
/// `NansenError::Challenge` when base64/JSON parsing fails.
pub fn decode_payment_signature(b64: &str) -> Result<PaymentPayload> {
    let raw = decode_base64(b64)
        .map_err(|e| NansenError::Challenge(format!("PAYMENT-SIGNATURE base64: {e}")))?;
    serde_json::from_slice(&raw)
        .map_err(|e| NansenError::Challenge(format!("PAYMENT-SIGNATURE JSON: {e}")).into())
}

/// Decode a `PAYMENT-RESPONSE` header value; `None` when absent or
/// undecodable (settlement is informational — the chain is the source of
/// truth).
fn decode_settlement(header: &str) -> Option<SettlementResponse> {
    let raw = decode_base64(header).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Map a `reqwest` failure onto the frozen error set: [`NansenError::Retry`]
/// with `status: 0` (documented sentinel — no HTTP status was received).
fn transport_failure(_err: reqwest::Error) -> crate::error::SentinelError {
    NansenError::Retry { status: 0 }.into()
}

/// Read a response body as JSON.
async fn read_json(response: reqwest::Response) -> Result<serde_json::Value> {
    let bytes = response.bytes().await.map_err(transport_failure)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| NansenError::Challenge(format!("expected a JSON response body: {e}")).into())
}

/// Outcome of one paid request.
#[derive(Debug, Clone)]
pub struct PaidResponse {
    /// Parsed response body.
    pub body: serde_json::Value,
    /// Settlement info when the facilitator reported it.
    pub settlement: Option<SettlementResponse>,
    /// Cost of the chosen rail, USD.
    pub cost_usd: Decimal,
    /// Chosen rail's network; empty on a free pass-through (no rail chosen).
    pub rail_network: String,
}

/// One full paid flow: unpaid POST (`X-Payer-Address`) → 402 → sign → retry
/// with `PAYMENT-SIGNATURE` (+ `X-Payer-Address`).
///
/// A 200 to the first request is a promo pass-through: `settlement` is
/// `None`, `cost_usd` is zero and `rail_network` is empty. Any other status
/// before or after the payment leg surfaces as [`NansenError::Retry`] with
/// the status (a transport failure reports `0`, see [`transport_failure`]).
/// The payer key and signature bytes are never logged.
///
/// # Errors
/// `NansenError::{Challenge, Sign, Retry}` per stage.
pub async fn fetch_with_payment(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    signer: &PayerSigner,
    network: &str,
    now_ms: u64,
) -> Result<PaidResponse> {
    let payer = signer.address();
    let challenge_response = http
        .post(url)
        .header(HEADER_PAYER_ADDRESS, payer.as_str())
        .json(body)
        .send()
        .await
        .map_err(transport_failure)?;
    let status = challenge_response.status().as_u16();

    // Promo/free pass-through — this call needed no payment.
    if status == 200 {
        return Ok(PaidResponse {
            body: read_json(challenge_response).await?,
            settlement: None,
            cost_usd: Decimal::ZERO,
            rail_network: String::new(),
        });
    }
    if status != 402 {
        return Err(NansenError::Retry { status }.into());
    }

    let header_value = challenge_response
        .headers()
        .get(HEADER_PAYMENT_REQUIRED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let challenge_bytes = challenge_response
        .bytes()
        .await
        .map_err(transport_failure)?;
    let challenge = parse_payment_required(status, &challenge_bytes, header_value.as_deref())?;
    let rail = select_rail(&challenge, network)?.clone();

    let authorization = build_authorization(&rail, &payer, now_ms);
    let signature = signer.sign_authorization(&rail, &authorization)?;
    let payload = PaymentPayload {
        x402_version: challenge.x402_version,
        resource: challenge.resource,
        accepted: rail.clone(),
        payload: ExactEvmPayload {
            signature,
            authorization,
        },
    };
    let encoded = encode_payload(&payload);

    let paid_response = http
        .post(url)
        .header(HEADER_PAYER_ADDRESS, payer.as_str())
        .header(HEADER_PAYMENT_SIGNATURE, encoded.as_str())
        .json(body)
        .send()
        .await
        .map_err(transport_failure)?;
    let status = paid_response.status().as_u16();
    if status != 200 {
        return Err(NansenError::Retry { status }.into());
    }

    let settlement = paid_response
        .headers()
        .get(HEADER_PAYMENT_RESPONSE)
        .and_then(|v| v.to_str().ok())
        .and_then(decode_settlement);
    Ok(PaidResponse {
        body: read_json(paid_response).await?,
        settlement,
        cost_usd: cost_usd(&rail.amount),
        rail_network: rail.network.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alloy::primitives::Signature;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Test-only key: 32 bytes of `0x07` (never a real wallet).
    const TEST_KEY: &str = "0x0707070707070707070707070707070707070707070707070707070707070707";

    /// The recorded live profiler challenge path.
    const PROFILER_PATH: &str = "/api/v1/profiler/perp-positions";

    /// Read a recorded fixture from `docs/evidence` (repo-relative).
    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/evidence")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path:?} unreadable: {e}"))
    }

    fn recorded_profiler_challenge() -> PaymentRequired {
        parse_payment_required(
            402,
            &fixture("p01-nansen-402-profiler-perp-positions.json"),
            None,
        )
        .expect("recorded profiler challenge parses")
    }

    fn test_signer() -> PayerSigner {
        PayerSigner::from_hex(TEST_KEY).expect("test key")
    }

    fn signer_address() -> Address {
        Address::from_str(&test_signer().address()).expect("signer address")
    }

    #[test]
    fn from_hex_accepts_prefixed_and_bare_keys_without_echoing_them() {
        let prefixed = PayerSigner::from_hex(TEST_KEY).expect("0x-prefixed key");
        let bare = PayerSigner::from_hex(&TEST_KEY[2..]).expect("bare key");
        assert_eq!(prefixed.address(), bare.address());

        assert!(
            PayerSigner::from_hex("0x1234").is_err(),
            "short key rejected"
        );
        let err =
            PayerSigner::from_hex(&format!("0x{}", "zz".repeat(32))).expect_err("non-hex rejected");
        let message = err.to_string();
        assert!(
            !message.contains("zz"),
            "error must not echo key material: {message}"
        );
    }

    #[test]
    fn debug_redacts_key_material() {
        let signer = test_signer();
        let debug = format!("{signer:?}");
        assert!(debug.contains("PayerSigner"));
        assert!(debug.contains(&signer.address()));
        assert!(
            !debug.contains(&TEST_KEY[2..]),
            "payer key leaked in Debug: {debug}"
        );

        let rail = select_rail(&recorded_profiler_challenge(), "eip155:143")
            .expect("monad rail")
            .clone();
        let authorization = build_authorization(&rail, &signer.address(), 1_740_672_149_000);
        let signature = signer
            .sign_authorization(&rail, &authorization)
            .expect("signs");
        assert!(!signature.is_empty());
        assert!(
            !format!("{signer:?}").contains(&signature[2..]),
            "signature bytes leaked in Debug"
        );
    }

    #[test]
    fn parses_recorded_profiler_challenge() {
        let challenge = recorded_profiler_challenge();

        assert_eq!(challenge.x402_version, 2);
        assert_eq!(challenge.error.as_deref(), Some("Payment required"));
        assert_eq!(
            challenge.resource.url,
            "https://api.nansen.ai/api/v1/profiler/perp-positions"
        );
        assert_eq!(challenge.accepts.len(), 8, "recorded 8-rail challenge");

        let monad = challenge
            .accepts
            .iter()
            .find(|rail| rail.network == "eip155:143")
            .expect("Monad rail recorded");
        assert_eq!(monad.scheme, "exact");
        assert_eq!(monad.asset, "0x754704Bc059F8C67012fEd69BC8A327a5aafb603");
        assert_eq!(monad.amount, "10000");
        assert_eq!(monad.pay_to, "0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f");
        assert_eq!(monad.max_timeout_seconds, 300);
        assert_eq!(monad.extra["name"], "USDC");
        assert_eq!(monad.extra["version"], "2");
        // The seven EVM rails share the 0x treasury; the Solana rail pays its
        // own base58 address, so only the EVM subset is held to this.
        let evm_rails: Vec<_> = challenge
            .accepts
            .iter()
            .filter(|rail| rail.network.starts_with("eip155:"))
            .collect();
        assert_eq!(evm_rails.len(), 7, "seven recorded EVM rails");
        assert!(
            evm_rails.iter().all(|rail| rail.pay_to == monad.pay_to),
            "every recorded EVM rail pays the same treasury"
        );
    }

    #[test]
    fn parses_recorded_header_fallback_and_tolerates_missing_padding() {
        let headers = String::from_utf8(fixture("p01-nansen-402-payer.headers"))
            .expect(".headers fixture is UTF-8");
        let value = headers
            .lines()
            .find_map(|line| line.strip_prefix("payment-required:"))
            .map(str::trim)
            .expect("recorded payment-required header");

        // The on-disk value is the FULL header, not a truncation: it decodes
        // to exactly the recorded body of the same (payer) run.
        let decoded = decode_base64(value).expect("recorded header is base64");
        assert_eq!(decoded, fixture("p01-nansen-402-payer.json"));

        let from_header =
            parse_payment_required(402, b"{\"broken\":true}", Some(value)).expect("header path");
        let from_body = parse_payment_required(402, &fixture("p01-nansen-402-payer.json"), None)
            .expect("body path");
        assert_eq!(from_header, from_body);
        assert_eq!(
            from_header.resource.url,
            "https://api.nansen.ai/api/v1/smart-money/netflow"
        );
        let monad = from_header
            .accepts
            .iter()
            .find(|rail| rail.network == "eip155:143")
            .expect("Monad rail");
        assert_eq!(monad.amount, "50000", "netflow rail price");

        let unpadded = value.trim_end_matches('=');
        assert_ne!(unpadded, value, "recorded value carries padding");
        assert_eq!(
            parse_payment_required(402, b"junk", Some(unpadded)).expect("no-pad tolerated"),
            from_body
        );

        assert!(parse_payment_required(200, &fixture("p01-nansen-402-payer.json"), None).is_err());
        assert!(parse_payment_required(402, b"junk", None).is_err());
        assert!(parse_payment_required(402, b"junk", Some("!!!")).is_err());
    }

    #[test]
    fn select_rail_requires_a_matching_exact_scheme() {
        let challenge = recorded_profiler_challenge();

        let rail = select_rail(&challenge, "eip155:143").expect("Monad rail selected");
        assert_eq!(rail.network, "eip155:143");
        assert_eq!(rail.asset, "0x754704Bc059F8C67012fEd69BC8A327a5aafb603");

        assert!(matches!(
            select_rail(&challenge, "eip155:9999"),
            Err(crate::error::SentinelError::Nansen(NansenError::Challenge(
                _
            )))
        ));

        // A same-network rail with a non-`exact` scheme must be skipped.
        let mut decoy = challenge.clone();
        let monad_index = decoy
            .accepts
            .iter()
            .position(|rail| rail.network == "eip155:143")
            .expect("Monad rail present");
        let mut decoy_rail = decoy.accepts[monad_index].clone();
        decoy_rail.scheme = "permit2-exact".to_owned();
        decoy.accepts.insert(0, decoy_rail);
        let chosen = select_rail(&decoy, "eip155:143").expect("exact rail still wins");
        assert_eq!(chosen.scheme, "exact");

        // Only decoys advertised ⇒ Challenge.
        let mut decoys_only = challenge.clone();
        for rail in &mut decoys_only.accepts {
            rail.scheme = "permit2-exact".to_owned();
        }
        assert!(select_rail(&decoys_only, "eip155:143").is_err());
    }

    #[test]
    fn payload_roundtrips_through_base64() {
        let challenge = recorded_profiler_challenge();
        let rail = select_rail(&challenge, "eip155:143")
            .expect("monad rail")
            .clone();
        let authorization = build_authorization(&rail, &test_signer().address(), 1_740_672_149_000);
        let payload = PaymentPayload {
            x402_version: challenge.x402_version,
            resource: challenge.resource.clone(),
            accepted: rail,
            payload: ExactEvmPayload {
                signature: format!("0x{}", "11".repeat(65)),
                authorization,
            },
        };

        let encoded = encode_payload(&payload);
        assert!(
            STANDARD.decode(&encoded).is_ok(),
            "STANDARD base64 on the wire"
        );
        assert_eq!(
            decode_payment_signature(&encoded).expect("decodes"),
            payload
        );
        assert!(decode_payment_signature("not base64 ///").is_err());
        assert!(decode_payment_signature(&STANDARD.encode("{}")).is_err());
    }

    #[test]
    fn build_authorization_matches_the_rail_window() {
        let challenge = recorded_profiler_challenge();
        let rail = select_rail(&challenge, "eip155:143")
            .expect("monad rail")
            .clone();
        let now_ms = 1_740_672_149_000_u64;
        let authorization =
            build_authorization(&rail, "0x1111111111111111111111111111111111111111", now_ms);

        assert_eq!(
            authorization.from,
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(authorization.to, rail.pay_to);
        assert_eq!(authorization.value, rail.amount);

        let valid_after: u64 = authorization.valid_after.parse().expect("decimal seconds");
        let valid_before: u64 = authorization.valid_before.parse().expect("decimal seconds");
        // §3.1 anchors validBefore at now + maxTimeoutSeconds and only the
        // skew extends validity backwards, so the window is maxTimeout + 60.
        assert_eq!(
            valid_before - valid_after,
            rail.max_timeout_seconds + AUTHORIZATION_SKEW_SECS
        );
        assert_eq!(valid_after, now_ms / 1_000 - AUTHORIZATION_SKEW_SECS);
        assert_eq!(valid_before, now_ms / 1_000 + rail.max_timeout_seconds);

        let nonce = hex::decode(authorization.nonce.trim_start_matches("0x")).expect("hex nonce");
        assert_eq!(nonce.len(), 32);
        assert!(
            nonce.iter().any(|byte| *byte != 0),
            "nonce must not be zero"
        );

        let other =
            build_authorization(&rail, "0x1111111111111111111111111111111111111111", now_ms);
        assert_ne!(authorization.nonce, other.nonce, "nonces are unique");
    }

    #[test]
    fn cost_usd_maps_base_units_to_usd() {
        assert_eq!(cost_usd("10000").normalize().to_string(), "0.01");
        assert_eq!(cost_usd("50000").normalize().to_string(), "0.05");
        assert_eq!(cost_usd("1").normalize().to_string(), "0.000001");
        assert_eq!(
            cost_usd("10000000000000000").normalize().to_string(),
            "10000000000"
        );
        assert_eq!(cost_usd("not-a-number"), Decimal::ZERO, "defensive zero");
    }

    /// Fixed-vector EIP-712 test: signs with a fixed key + fixed
    /// authorization and checks alloy recovers the signer. The printed hex is
    /// the cross-check target for the independent ethers oracle.
    #[test]
    fn signature_vector_recovers_signer_and_prints_for_oracle() {
        let signer = test_signer();
        let rail = select_rail(&recorded_profiler_challenge(), "eip155:143")
            .expect("monad rail")
            .clone();
        let authorization = Authorization {
            from: "0x1111111111111111111111111111111111111111".to_owned(),
            to: "0x2222222222222222222222222222222222222222".to_owned(),
            value: "10000".to_owned(),
            valid_after: "1740672089".to_owned(),
            valid_before: "1740672389".to_owned(),
            nonce: format!(
                "0x{}",
                (0_u8..32)
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        };

        let signature_hex = signer
            .sign_authorization(&rail, &authorization)
            .expect("signs the fixed vector");
        assert_eq!(signature_hex.len(), 2 + 130, "0x + 65 bytes");
        let raw = hex::decode(signature_hex.trim_start_matches("0x")).expect("hex");
        assert_eq!(raw.len(), 65);
        assert!(
            raw[64] == 27 || raw[64] == 28,
            "v must be 27/28, got {}",
            raw[64]
        );

        let signature = Signature::from_raw(&raw).expect("65-byte signature");
        let raw_array: [u8; 65] = raw.as_slice().try_into().expect("65 bytes");
        assert_eq!(
            signature.as_bytes(),
            raw_array,
            "alloy as_bytes agrees with our electrum-v encoding"
        );

        let digest = authorization_digest(&rail, &authorization).expect("digest");
        let recovered = signature
            .recover_address_from_prehash(&digest)
            .expect("recovers");
        assert_eq!(recovered, signer_address());

        println!("x402 EIP-3009 signature vector: {signature_hex}");
        println!("x402 EIP-3009 signer address:   {}", signer.address());
        println!("x402 EIP-3009 digest:           0x{}", hex::encode(digest));
    }

    #[tokio::test]
    async fn pays_when_challenged_and_parses_settlement() {
        let server = MockServer::start().await;
        let challenge_bytes = fixture("p01-nansen-402-profiler-perp-positions.json");
        // The profiler run has no recorded `payment-required` header on disk
        // (only its body); the header is reconstructed by re-encoding that
        // body (STANDARD), which is exactly what the relay serves.
        let challenge_header = STANDARD.encode(&challenge_bytes);

        let settlement = SettlementResponse {
            success: true,
            transaction: format!("0x{}", "ab".repeat(32)),
            network: "eip155:143".to_owned(),
            payer: test_signer().address(),
        };
        let settlement_b64 =
            STANDARD.encode(serde_json::to_vec(&settlement).expect("settlement json"));

        let paid_mock = Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .and(header_exists(HEADER_PAYMENT_SIGNATURE))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(HEADER_PAYMENT_RESPONSE, settlement_b64.as_str())
                    .set_body_json(serde_json::json!({"data": {"net_flow_usd": "-12345.6"}})),
            )
            .with_priority(1);
        paid_mock.mount(&server).await;

        let challenge_mock = Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header(HEADER_PAYMENT_REQUIRED, challenge_header.as_str())
                    .set_body_bytes(challenge_bytes.clone()),
            )
            .with_priority(2);
        challenge_mock.mount(&server).await;

        let signer = test_signer();
        let now_ms = 1_740_672_149_000_u64;
        let request_body =
            serde_json::json!({"addresses": ["0x1111111111111111111111111111111111111111"]});
        let url = format!("{}{PROFILER_PATH}", server.uri());
        let paid = fetch_with_payment(
            &reqwest::Client::new(),
            &url,
            &request_body,
            &signer,
            "eip155:143",
            now_ms,
        )
        .await
        .expect("pays and settles");

        assert_eq!(paid.body["data"]["net_flow_usd"], "-12345.6");
        assert_eq!(paid.cost_usd.normalize().to_string(), "0.01");
        assert_eq!(paid.rail_network, "eip155:143");
        let settled = paid.settlement.expect("settlement header parsed");
        assert!(settled.success);
        assert_eq!(settled.transaction, format!("0x{}", "ab".repeat(32)));
        assert_eq!(settled.payer, signer.address());

        let recorded = server.received_requests().await.expect("recording enabled");
        assert_eq!(recorded.len(), 2, "unpaid probe + paid retry");
        assert_eq!(
            recorded[0]
                .headers
                .get("x-payer-address")
                .and_then(|v| v.to_str().ok()),
            Some(signer.address().as_str()),
            "probe carries X-Payer-Address"
        );
        assert!(
            recorded[0].headers.get(HEADER_PAYMENT_SIGNATURE).is_none(),
            "probe must stay unpaid"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&recorded[1].body).expect("json body"),
            request_body,
            "retry re-sends the same body"
        );
        assert_eq!(
            recorded[1]
                .headers
                .get("x-payer-address")
                .and_then(|v| v.to_str().ok()),
            Some(signer.address().as_str()),
            "retry keeps X-Payer-Address"
        );

        let signature_header = recorded[1]
            .headers
            .get(HEADER_PAYMENT_SIGNATURE)
            .expect("retry carries PAYMENT-SIGNATURE")
            .to_str()
            .expect("ascii header");
        let payload = decode_payment_signature(signature_header).expect("retry payload decodes");
        assert_eq!(payload.x402_version, 2);
        assert_eq!(payload.accepted.network, "eip155:143");
        assert_eq!(payload.accepted.amount, "10000");
        assert_eq!(
            payload.accepted.asset,
            "0x754704Bc059F8C67012fEd69BC8A327a5aafb603"
        );
        assert_eq!(
            payload.resource.url,
            "https://api.nansen.ai/api/v1/profiler/perp-positions"
        );
        assert_eq!(payload.payload.authorization.from, signer.address());
        assert_eq!(
            payload.payload.authorization.to,
            "0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f"
        );
        assert_eq!(payload.payload.authorization.value, "10000");

        let valid_after: u64 = payload
            .payload
            .authorization
            .valid_after
            .parse()
            .expect("decimal");
        let valid_before: u64 = payload
            .payload
            .authorization
            .valid_before
            .parse()
            .expect("decimal");
        assert_eq!(valid_before - valid_after, 300 + AUTHORIZATION_SKEW_SECS);
        assert_eq!(valid_after, now_ms / 1_000 - AUTHORIZATION_SKEW_SECS);
        assert_eq!(valid_before, now_ms / 1_000 + 300);
        let nonce = hex::decode(payload.payload.authorization.nonce.trim_start_matches("0x"))
            .expect("nonce hex");
        assert_eq!(nonce.len(), 32);

        let raw = hex::decode(payload.payload.signature.trim_start_matches("0x")).expect("hex sig");
        let recovered = Signature::from_raw(&raw)
            .expect("65-byte signature")
            .recover_address_from_prehash(
                &authorization_digest(&payload.accepted, &payload.payload.authorization)
                    .expect("digest"),
            )
            .expect("recovers");
        assert_eq!(
            recovered,
            signer_address(),
            "the sent signature is the payer's"
        );
    }

    #[tokio::test]
    async fn passes_through_when_the_call_is_free() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let body = serde_json::json!({"addresses": ["0x1111111111111111111111111111111111111111"]});
        let paid = fetch_with_payment(
            &reqwest::Client::new(),
            &format!("{}{PROFILER_PATH}", server.uri()),
            &body,
            &test_signer(),
            "eip155:143",
            1_740_672_149_000,
        )
        .await
        .expect("free pass-through");

        assert!(paid.settlement.is_none());
        assert_eq!(paid.cost_usd, Decimal::ZERO);
        assert_eq!(paid.rail_network, "");
        assert_eq!(paid.body["ok"], true);

        let recorded = server.received_requests().await.expect("recording enabled");
        assert_eq!(recorded.len(), 1, "no retry without a 402");
        assert!(recorded[0].headers.get("x-payer-address").is_some());
        assert!(recorded[0].headers.get(HEADER_PAYMENT_SIGNATURE).is_none());
    }

    #[tokio::test]
    async fn retry_failure_surfaces_the_status() {
        let server = MockServer::start().await;
        let challenge_bytes = fixture("p01-nansen-402-profiler-perp-positions.json");

        Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .and(header_exists(HEADER_PAYMENT_SIGNATURE))
            .respond_with(ResponseTemplate::new(403).set_body_string("nope"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .respond_with(ResponseTemplate::new(402).set_body_bytes(challenge_bytes))
            .with_priority(2)
            .mount(&server)
            .await;

        let err = fetch_with_payment(
            &reqwest::Client::new(),
            &format!("{}{PROFILER_PATH}", server.uri()),
            &serde_json::json!({"addresses": ["0x1111111111111111111111111111111111111111"]}),
            &test_signer(),
            "eip155:143",
            1_740_672_149_000,
        )
        .await
        .expect_err("the paid retry was refused");

        match err {
            crate::error::SentinelError::Nansen(NansenError::Retry { status }) => {
                assert_eq!(status, 403, "retry status is preserved");
            }
            other => panic!("expected Retry{{403}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn initial_non_402_status_is_a_retry_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PROFILER_PATH))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = fetch_with_payment(
            &reqwest::Client::new(),
            &format!("{}{PROFILER_PATH}", server.uri()),
            &serde_json::json!({"addresses": ["0x1111111111111111111111111111111111111111"]}),
            &test_signer(),
            "eip155:143",
            1_740_672_149_000,
        )
        .await
        .expect_err("500 is not payable");

        match err {
            crate::error::SentinelError::Nansen(NansenError::Retry { status }) => {
                assert_eq!(status, 500);
            }
            other => panic!("expected Retry{{500}}, got {other:?}"),
        }
    }
}
