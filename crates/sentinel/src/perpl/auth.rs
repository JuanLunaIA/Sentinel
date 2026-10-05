//! Ed25519 API-key signing for Perpl (REST headers + trading-WS sign-in).
//!
//! SKELETON STUB — frozen interface (see `SPEC.md` §4.1). Replaced by agent
//! `auth`; do not change public signatures.
//!
//! Canonical strings (verified against `vendor/api-docs/authentication.md`):
//! - REST: `chain_id \n METHOD \n request-target \n timestamp_ms \n nonce \n sha256hex(body)`
//! - WS:   `chain_id \n trading-ws-signin \n timestamp_ms \n nonce`

use crate::config::PerplConfig;
use crate::error::Result;

/// Signs requests with an enrolled Perpl API key.
///
/// Holds the opaque `X-API-Key` token and the Ed25519 signing key. Its `Debug`
/// implementation must redact both (P00 invariant #5).
pub struct ApiKeySigner;

impl ApiKeySigner {
    /// Build from raw parts (token, hex seed `0x`-optional, chain id).
    ///
    /// # Errors
    /// `PerplError::Auth` on malformed hex / wrong seed length.
    pub fn from_parts(_token: &str, _secret_hex: &str, _chain_id: u64) -> Result<Self> {
        todo!("P03 agent auth")
    }

    /// Build from configuration.
    ///
    /// # Errors
    /// `PerplError::Auth` on malformed `PERPL_API_KEY_SECRET`.
    pub fn from_config(_perpl: &PerplConfig) -> Result<Self> {
        todo!("P03 agent auth")
    }

    /// Chain id this signer signs for.
    pub fn chain_id(&self) -> u64 {
        todo!("P03 agent auth")
    }

    /// Signed headers for one REST request, in canonical order:
    /// `X-API-Key`, `X-API-Timestamp`, `X-API-Nonce`, `X-API-Signature`.
    ///
    /// # Errors
    /// `PerplError::Auth` if signing fails.
    pub fn signed_request_headers(
        &self,
        _method: &str,
        _target: &str,
        _body: &[u8],
    ) -> Result<[(String, String); 4]> {
        todo!("P03 agent auth")
    }

    /// Fresh `mt:29` sign-in frame (new timestamp + nonce every call).
    ///
    /// # Errors
    /// `PerplError::Auth` if signing fails.
    pub fn ws_signin_frame(&self) -> Result<String> {
        todo!("P03 agent auth")
    }
}

/// Canonical REST string (pure; spec function).
pub fn canonical_rest(
    _chain_id: u64,
    _method: &str,
    _target: &str,
    _timestamp_ms: &str,
    _nonce: &str,
    _body: &[u8],
) -> String {
    todo!("P03 agent auth")
}

/// Canonical WS sign-in string (pure; spec function).
pub fn canonical_ws(_chain_id: u64, _timestamp_ms: &str, _nonce: &str) -> String {
    todo!("P03 agent auth")
}

/// Ed25519-sign a canonical string; returns base64url (no padding).
pub fn sign_canonical(_signing_key: &ed25519_dalek::SigningKey, _canonical: &str) -> String {
    todo!("P03 agent auth")
}

/// base64url without padding.
pub fn b64url(_bytes: &[u8]) -> String {
    todo!("P03 agent auth")
}

/// 16 random bytes, base64url without padding (via `getrandom`).
///
/// # Errors
/// `PerplError::Auth` if the OS entropy source fails.
pub fn random_nonce_16() -> Result<String> {
    todo!("P03 agent auth")
}
