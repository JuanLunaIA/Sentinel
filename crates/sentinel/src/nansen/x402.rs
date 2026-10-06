//! x402 v2 payment flow — wire types, rail selection, EIP-3009 signing.
//!
//! Frozen by `SPEC-P09.md` §3.1 from the recorded live challenges
//! (`docs/evidence/p01-nansen-402*.json`) plus the v2 specification. Money is
//! decimal strings as returned on the wire; the payer key is never logged.
//!
//! **Skeleton status (P09):** interfaces frozen; implemented by the P09 wave.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Header the 402 challenge may also arrive in (base64 JSON).
pub const HEADER_PAYMENT_REQUIRED: &str = "payment-required";
/// Header the signed payment travels in (base64 `PaymentPayload`).
pub const HEADER_PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
/// Header carrying settlement info on the paid response (base64 JSON).
pub const HEADER_PAYMENT_RESPONSE: &str = "payment-response";

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

/// Signs EIP-3009 authorizations with the x402 payer wallet.
pub struct PayerSigner {
    /// alloy signer (never `Debug`-printed raw).
    inner: alloy::signers::local::PrivateKeySigner,
}

impl PayerSigner {
    /// Build from a `0x…` 32-byte hex key.
    ///
    /// # Errors
    /// `NansenError::Sign` on a malformed key.
    pub fn from_hex(_key: &str) -> Result<Self> {
        todo!("P09 agent x402-core")
    }

    /// Payer address (`0x…`).
    pub fn address(&self) -> String {
        format!("{}", self.inner.address())
    }

    /// Sign the EIP-3009 typed data for `req` + `authorization`.
    ///
    /// # Errors
    /// `NansenError::Sign` when the typed-data hash/sign fails.
    pub fn sign_authorization(
        &self,
        _req: &PaymentRequirements,
        _authorization: &Authorization,
    ) -> Result<String> {
        todo!("P09 agent x402-core: EIP-712 TransferWithAuthorization")
    }
}

impl fmt::Debug for PayerSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerSigner")
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

/// Select the `exact` rail matching `network`.
///
/// # Errors
/// `NansenError::Challenge` when no acceptable rail exists.
pub fn select_rail<'a>(
    _req: &'a PaymentRequired,
    _network: &str,
) -> Result<&'a PaymentRequirements> {
    todo!("P09 agent x402-core")
}

/// Parse a 402 challenge (body JSON; base64 header fallback).
///
/// # Errors
/// `NansenError::Challenge` when neither parses.
pub fn parse_payment_required(
    _status: u16,
    _body: &[u8],
    _header_b64: Option<&str>,
) -> Result<PaymentRequired> {
    todo!("P09 agent x402-core")
}

/// Build the authorization for `req` signed by `payer` at `now_ms`.
pub fn build_authorization(
    _req: &PaymentRequirements,
    _payer: &str,
    _now_ms: u64,
) -> Authorization {
    todo!("P09 agent x402-core")
}

/// `amount / 10^6` (stablecoin rails; documented assumption).
pub fn cost_usd(_amount: &str) -> Decimal {
    todo!("P09 agent x402-core")
}

/// Base64(STANDARD) of the payment payload for the retry header.
pub fn encode_payload(_payload: &PaymentPayload) -> String {
    todo!("P09 agent x402-core")
}

/// Decode a `PAYMENT-SIGNATURE` header value (tests/verifier).
///
/// # Errors
/// `NansenError::Challenge` when base64/JSON parsing fails.
pub fn decode_payment_signature(_b64: &str) -> Result<PaymentPayload> {
    todo!("P09 agent x402-core")
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
    /// Chosen rail's network.
    pub rail_network: String,
}

/// One full paid flow: unpaid POST (`X-Payer-Address`) → 402 → sign → retry
/// with `PAYMENT-SIGNATURE` (+ `X-Payer-Address`).
///
/// # Errors
/// `NansenError::{Challenge, Sign, Retry}` per stage.
pub async fn fetch_with_payment(
    _http: &reqwest::Client,
    _url: &str,
    _body: &serde_json::Value,
    _signer: &PayerSigner,
    _network: &str,
    _now_ms: u64,
) -> Result<PaidResponse> {
    todo!("P09 agent x402-core: 402 → sign → retry flow")
}
