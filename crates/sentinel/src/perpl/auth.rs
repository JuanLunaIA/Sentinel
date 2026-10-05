//! Ed25519 API-key signing for Perpl (REST headers + trading-WS sign-in).
//!
//! Canonical strings (verified against `vendor/api-docs/authentication.md`,
//! frozen in `SPEC.md` §3.1/§3.2):
//! - REST: `chain_id \n METHOD \n request-target \n timestamp_ms \n nonce \n sha256hex(body)`
//! - WS:   `chain_id \n trading-ws-signin \n timestamp_ms \n nonce`
//!
//! The signature is `base64url(ed25519_sign(seed, canonical))` with **no
//! padding**. Authenticated REST requests carry the four headers in the order
//! `X-API-Key`, `X-API-Timestamp`, `X-API-Nonce`, `X-API-Signature`; the
//! trading WebSocket opens with the `mt:29` sign-in frame produced by
//! [`ApiKeySigner::ws_signin_frame`].

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::config::{PerplConfig, SecretString};
use crate::error::{PerplError, Result};

/// Signs requests with an enrolled Perpl API key.
///
/// Holds the opaque `X-API-Key` token and the Ed25519 signing key. Its `Debug`
/// implementation must redact both (P00 invariant #5).
pub struct ApiKeySigner {
    /// Opaque `X-API-Key` token, sent verbatim on every request.
    token: SecretString,
    /// Ed25519 key over the canonical strings.
    signing_key: SigningKey,
    /// Chain id baked into every signed canonical string.
    chain_id: u64,
}

// Hand-written (never derived): must not leak the token or key material.
impl fmt::Debug for ApiKeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeySigner")
            .field("token", &"REDACTED")
            .field("signing_key", &"REDACTED")
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

impl ApiKeySigner {
    /// Build from raw parts (token, hex seed `0x`-optional, chain id).
    ///
    /// # Errors
    /// `PerplError::Auth` on malformed hex / wrong seed length.
    pub fn from_parts(token: &str, secret_hex: &str, chain_id: u64) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(auth_err("API key token must not be empty"));
        }
        let raw = secret_hex.trim();
        let hex_part = raw.strip_prefix("0x").unwrap_or(raw);
        let bytes =
            hex::decode(hex_part).map_err(|_| auth_err("API key secret is not valid hex"))?;
        let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            auth_err(format!(
                "API key secret must be exactly 32 bytes (64 hex chars), got {}",
                bytes.len()
            ))
        })?;
        Ok(Self {
            token: SecretString::new(token),
            signing_key: SigningKey::from_bytes(&seed),
            chain_id,
        })
    }

    /// Build from configuration.
    ///
    /// # Errors
    /// `PerplError::Auth` on malformed `PERPL_API_KEY_SECRET`.
    pub fn from_config(perpl: &PerplConfig) -> Result<Self> {
        Self::from_parts(
            perpl.api_key.expose(),
            perpl.api_key_secret.expose(),
            perpl.chain_id,
        )
    }

    /// Chain id this signer signs for.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Signed headers for one REST request, in canonical order:
    /// `X-API-Key`, `X-API-Timestamp`, `X-API-Nonce`, `X-API-Signature`.
    ///
    /// # Errors
    /// `PerplError::Auth` if signing fails.
    pub fn signed_request_headers(
        &self,
        method: &str,
        target: &str,
        body: &[u8],
    ) -> Result<[(String, String); 4]> {
        let timestamp_ms = now_ms()?;
        let nonce = random_nonce_16()?;
        let canonical = canonical_rest(self.chain_id, method, target, &timestamp_ms, &nonce, body);
        let signature = sign_canonical(&self.signing_key, &canonical);
        Ok([
            ("X-API-Key".to_string(), self.token.expose().to_string()),
            ("X-API-Timestamp".to_string(), timestamp_ms),
            ("X-API-Nonce".to_string(), nonce),
            ("X-API-Signature".to_string(), signature),
        ])
    }

    /// Fresh `mt:29` sign-in frame (new timestamp + nonce every call).
    ///
    /// # Errors
    /// `PerplError::Auth` if signing fails.
    pub fn ws_signin_frame(&self) -> Result<String> {
        let timestamp_ms = now_ms()?;
        let nonce = random_nonce_16()?;
        let canonical = canonical_ws(self.chain_id, &timestamp_ms, &nonce);
        let signature = sign_canonical(&self.signing_key, &canonical);
        let frame = serde_json::json!({
            "mt": 29,
            "chain_id": self.chain_id,
            "api_key": self.token.expose(),
            "timestamp": timestamp_ms,
            "nonce": nonce,
            "signature": signature,
        });
        Ok(frame.to_string())
    }
}

/// Canonical REST string (pure; spec function).
pub fn canonical_rest(
    chain_id: u64,
    method: &str,
    target: &str,
    timestamp_ms: &str,
    nonce: &str,
    body: &[u8],
) -> String {
    let body_hash = hex::encode(Sha256::digest(body));
    format!("{chain_id}\n{method}\n{target}\n{timestamp_ms}\n{nonce}\n{body_hash}")
}

/// Canonical WS sign-in string (pure; spec function).
pub fn canonical_ws(chain_id: u64, timestamp_ms: &str, nonce: &str) -> String {
    format!("{chain_id}\ntrading-ws-signin\n{timestamp_ms}\n{nonce}")
}

/// Ed25519-sign a canonical string; returns base64url (no padding).
pub fn sign_canonical(signing_key: &ed25519_dalek::SigningKey, canonical: &str) -> String {
    b64url(&signing_key.sign(canonical.as_bytes()).to_bytes())
}

/// base64url without padding.
pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 16 random bytes, base64url without padding (via `getrandom`).
///
/// # Errors
/// `PerplError::Auth` if the OS entropy source fails.
pub fn random_nonce_16() -> Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| auth_err(format!("OS entropy source failed: {e}")))?;
    Ok(b64url(&nonce))
}

/// Current UNIX time in milliseconds, as a decimal string.
fn now_ms() -> Result<String> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| auth_err(format!("system clock is before the UNIX epoch: {e}")))?
        .as_millis();
    Ok(ms.to_string())
}

/// Build a `PerplError::Auth` wrapped in the crate-wide error type.
fn auth_err(message: impl Into<String>) -> crate::error::SentinelError {
    PerplError::Auth(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    use crate::error::SentinelError;

    /// Opaque token shared by the tests; must never appear in `Debug` output.
    const TEST_TOKEN: &str = "sentinel-test-token-0123456789abcdef";

    /// Fixed Ed25519 seed: 32 × `0x07` (SPEC §9 `auth`).
    const SEED_BYTES: [u8; 32] = [0x07; 32];

    /// The same seed, hex-encoded (64 chars).
    const SEED_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    /// The spec vectors' nonce: 16 zero bytes, base64url without padding.
    const NONCE_ZERO: &str = "AAAAAAAAAAAAAAAAAAAAAA";

    /// Build the shared signer over the fixed test seed (chain 10143, testnet).
    fn test_signer() -> ApiKeySigner {
        ApiKeySigner::from_parts(TEST_TOKEN, SEED_HEX, 10143).expect("test signer builds")
    }

    /// Decode a base64url (no pad) Ed25519 signature into its byte form.
    fn decode_signature(b64: &str) -> Signature {
        let bytes: [u8; 64] = URL_SAFE_NO_PAD
            .decode(b64)
            .expect("signature is base64url")
            .try_into()
            .expect("Ed25519 signatures are 64 bytes");
        Signature::from_bytes(&bytes)
    }

    #[test]
    fn canonical_rest_matches_rest1_vector() {
        let canonical = canonical_rest(
            143,
            "GET",
            "/v1/trading/fills?count=100",
            "1728000000000",
            NONCE_ZERO,
            b"",
        );
        assert_eq!(
            canonical,
            "143\nGET\n/v1/trading/fills?count=100\n1728000000000\nAAAAAAAAAAAAAAAAAAAAAA\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn canonical_rest_with_body_matches_rest2_vector() {
        // SPEC §3.1 calls this a "literal 7 bytes" body; `{"d":[]}` is 8 bytes
        // and its sha256 is the spec's hash (independently re-derived, and
        // reported as a spec typo in `open_issues`). The canonical literal
        // below is the normative assertion.
        let body = br#"{"d":[]}"#;
        let canonical = canonical_rest(
            10143,
            "POST",
            "/v1/trading/orders",
            "1728000001000",
            NONCE_ZERO,
            body,
        );
        assert_eq!(
            canonical,
            "10143\nPOST\n/v1/trading/orders\n1728000001000\nAAAAAAAAAAAAAAAAAAAAAA\n\
             088214f816e99a2f4aedb5323c1c2eaf8b8143df9424ec46759966ddd9b72dd3"
        );
    }

    #[test]
    fn canonical_ws_matches_ws1_vector() {
        let canonical = canonical_ws(10143, "1728000000000", NONCE_ZERO);
        assert_eq!(
            canonical,
            "10143\ntrading-ws-signin\n1728000000000\nAAAAAAAAAAAAAAAAAAAAAA"
        );
    }

    #[test]
    fn b64url_encodes_without_padding() {
        assert_eq!(b64url(b"hello"), "aGVsbG8");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(b64url(b"hello"))
                .expect("roundtrips"),
            b"hello"
        );
        assert!(!b64url(&[0u8; 16]).contains('='), "no padding");
    }

    #[test]
    fn random_nonce_is_16_bytes_and_changes() {
        let a = random_nonce_16().expect("nonce A");
        let b = random_nonce_16().expect("nonce B");
        assert_ne!(a, b, "two fresh nonces must differ");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(&a)
                .expect("nonce A is base64url")
                .len(),
            16
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(&b)
                .expect("nonce B is base64url")
                .len(),
            16
        );
        assert!(!a.contains('='), "nonce carries no padding");
    }

    #[test]
    fn sign_canonical_roundtrips_through_verifying_key() {
        let signing_key = SigningKey::from_bytes(&SEED_BYTES);
        let canonical = canonical_rest(
            10143,
            "GET",
            "/v1/trading/wallet",
            "1728000000000",
            NONCE_ZERO,
            b"",
        );
        let signature = decode_signature(&sign_canonical(&signing_key, &canonical));

        let verifying_key = VerifyingKey::from_bytes(&signing_key.verifying_key().to_bytes())
            .expect("valid public key");
        verifying_key
            .verify(canonical.as_bytes(), &signature)
            .expect("signature must verify");
        assert!(
            verifying_key.verify(b"tampered", &signature).is_err(),
            "signature must not verify a different message"
        );
    }

    #[test]
    fn signed_request_headers_are_ordered_and_verifiable() {
        let signer = test_signer();
        let target = "/v1/trading/positions?count=50";
        let headers = signer
            .signed_request_headers("GET", target, b"")
            .expect("headers build");

        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "X-API-Key",
                "X-API-Timestamp",
                "X-API-Nonce",
                "X-API-Signature"
            ]
        );
        assert_eq!(headers[0].1, TEST_TOKEN);

        let timestamp = &headers[1].1;
        let nonce = &headers[2].1;
        let _ts_ms: u128 = timestamp.parse().expect("timestamp is decimal ms");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(nonce)
                .expect("nonce is base64url")
                .len(),
            16
        );

        // Rebuild the canonical string from the emitted headers and verify.
        let canonical = canonical_rest(10143, "GET", target, timestamp, nonce, b"");
        let signature = decode_signature(&headers[3].1);
        signer
            .signing_key
            .verifying_key()
            .verify(canonical.as_bytes(), &signature)
            .expect("signature verifies against the reconstructed canonical string");
    }

    #[test]
    fn ws_signin_frame_is_mt29_json_with_verifiable_signature() {
        let signer = test_signer();
        let frame = signer.ws_signin_frame().expect("frame builds");
        let value: serde_json::Value = serde_json::from_str(&frame).expect("frame is JSON");

        assert_eq!(value["mt"], serde_json::json!(29));
        assert_eq!(value["chain_id"], serde_json::json!(10143));
        assert_eq!(value["api_key"], serde_json::json!(TEST_TOKEN));

        let timestamp = value["timestamp"].as_str().expect("timestamp is a string");
        let nonce = value["nonce"].as_str().expect("nonce is a string");
        let signature_b64 = value["signature"].as_str().expect("signature is a string");
        let _ts_ms: u128 = timestamp.parse().expect("timestamp is decimal ms");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(nonce)
                .expect("nonce is base64url")
                .len(),
            16
        );

        let canonical = canonical_ws(10143, timestamp, nonce);
        let signature = decode_signature(signature_b64);
        signer
            .signing_key
            .verifying_key()
            .verify(canonical.as_bytes(), &signature)
            .expect("frame signature verifies against the canonical string");

        // Fresh timestamp + nonce on every call.
        assert_ne!(frame, signer.ws_signin_frame().expect("second frame"));
    }

    #[test]
    fn from_parts_rejects_malformed_seeds() {
        let not_hex = "zz".repeat(32);
        assert!(matches!(
            ApiKeySigner::from_parts(TEST_TOKEN, &not_hex, 10143),
            Err(SentinelError::Perpl(PerplError::Auth(_)))
        ));

        let short_31 = "07".repeat(31);
        assert!(matches!(
            ApiKeySigner::from_parts(TEST_TOKEN, &short_31, 10143),
            Err(SentinelError::Perpl(PerplError::Auth(_)))
        ));

        let long_33 = "07".repeat(33);
        assert!(matches!(
            ApiKeySigner::from_parts(TEST_TOKEN, &long_33, 10143),
            Err(SentinelError::Perpl(PerplError::Auth(_)))
        ));

        // An empty token is refused as well (fail fast, never 401 silently).
        assert!(matches!(
            ApiKeySigner::from_parts("   ", SEED_HEX, 10143),
            Err(SentinelError::Perpl(PerplError::Auth(_)))
        ));
    }

    #[test]
    fn from_parts_accepts_plain_and_0x_prefixed_seeds() {
        let plain = ApiKeySigner::from_parts(TEST_TOKEN, SEED_HEX, 143).expect("plain hex");
        assert_eq!(plain.chain_id(), 143);
        let prefixed = ApiKeySigner::from_parts(TEST_TOKEN, &format!("0x{SEED_HEX}"), 10143)
            .expect("0x-prefixed hex");
        assert_eq!(prefixed.chain_id(), 10143);
        // Same seed ⇒ same public key, with or without the `0x` prefix.
        assert_eq!(
            plain.signing_key.verifying_key().to_bytes(),
            prefixed.signing_key.verifying_key().to_bytes()
        );
    }

    #[test]
    fn from_config_reads_perpl_config() {
        let perpl = PerplConfig {
            env_name: crate::config::PerplEnv::Testnet,
            chain_id: 10143,
            api_url: "https://testnet.perpl.xyz/api".to_string(),
            ws_url: "wss://testnet.perpl.xyz".to_string(),
            rpc_url: "https://testnet-rpc.monad.xyz".to_string(),
            exchange_address: "0x1964c32f0be608e7d29302aff5e61268e72080cc".to_string(),
            collateral_token: "0xa9012a055bd4e0edff8ce09f960291c09d5322dc".to_string(),
            api_key: SecretString::new(TEST_TOKEN),
            api_key_secret: SecretString::new(format!("0x{SEED_HEX}")),
            account: None,
        };
        let signer = ApiKeySigner::from_config(&perpl).expect("from_config builds");
        assert_eq!(signer.chain_id(), 10143);

        let broken = PerplConfig {
            api_key_secret: SecretString::new("not-hex".to_string()),
            ..perpl
        };
        assert!(matches!(
            ApiKeySigner::from_config(&broken),
            Err(SentinelError::Perpl(PerplError::Auth(_)))
        ));
    }

    #[test]
    fn debug_redacts_token_and_seed() {
        let signer = test_signer();
        let debug = format!("{signer:?}");
        assert!(!debug.contains(TEST_TOKEN), "token leaked: {debug}");
        assert!(!debug.contains(SEED_HEX), "seed hex leaked: {debug}");
        assert!(!debug.contains("0707"), "seed bytes leaked: {debug}");
        assert!(
            !debug.contains(&format!("{SEED_BYTES:?}")),
            "raw seed leaked: {debug}"
        );
        // Non-secret metadata is still conveyed.
        assert!(debug.contains("10143"), "chain id missing: {debug}");
        assert!(
            debug.contains("REDACTED"),
            "redaction marker missing: {debug}"
        );
    }
}
