//! Proof-carrying AI decisions: the inference commitment (P23, GAP-014 layer 1).
//!
//! The audit spine already proves *what was decided and when*: every decision
//! entry is hash-chained and batch-anchored on Monad. It does not prove *which
//! model, prompt, and parameters produced it*. This module closes that gap by
//! committing the full inference context into the journal entry, so any party
//! holding the archived inputs can recompute the commitment and re-run the
//! deterministic post-processing (parse → schema validation → confidence floor)
//! to reproduce the journaled outcome.
//!
//! **Encoding.** The commitment is SHA-256 over a *length-prefixed* encoding of
//! the fields, not over their concatenation. Plain concatenation is ambiguous:
//! `("ab", "c")` and `("a", "bc")` collide, which would let an operator move
//! text between the system prompt and the user prompt without changing the
//! commitment. Each field is written as `<byte-length>:<bytes>`, so the mapping
//! from inputs to digest is injective by construction.
//!
//! The field order is frozen: system prompt, schema document, user prompt,
//! model id, provider, temperature, max tokens, raw completion. Changing the
//! order or adding a field is a breaking change to the commitment format and
//! must bump [`COMMITMENT_VERSION`].

use sha2::{Digest, Sha256};

/// Version tag for the commitment format. Emitted alongside every commitment
/// so a verifier can refuse a format it does not know.
pub const COMMITMENT_VERSION: &str = "v1";

/// The inference context committed into a strategy journal entry.
///
/// Every field is the value *actually sent or received*, in the same rendering
/// the provider saw: the temperature as the request rendered it, the completion
/// exactly as the provider returned it (before any parsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceContext<'a> {
    /// System prompt sent to the provider.
    pub system_prompt: &'a str,
    /// Response schema documentation appended to the user prompt.
    pub schema_doc: &'a str,
    /// User prompt (the rendered situation block).
    pub user_prompt: &'a str,
    /// Model id as sent in the request.
    pub model_id: &'a str,
    /// Provider name that answered (`qwen`, `kimi`, `mock`).
    pub provider: &'a str,
    /// Sampling temperature as rendered into the request body.
    pub temperature: &'a str,
    /// Output token budget as rendered into the request body.
    pub max_tokens: u64,
    /// Raw completion text, before any parsing or repair.
    pub raw_completion: &'a str,
}

impl InferenceContext<'_> {
    /// The length-prefixed canonical byte string the digest is taken over.
    ///
    /// Exposed so a reference verifier can display or diff the exact bytes
    /// instead of trusting this function's equal output.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let fields: [&[u8]; 7] = [
            self.system_prompt.as_bytes(),
            self.schema_doc.as_bytes(),
            self.user_prompt.as_bytes(),
            self.model_id.as_bytes(),
            self.provider.as_bytes(),
            self.temperature.as_bytes(),
            self.raw_completion.as_bytes(),
        ];
        let mut out = Vec::with_capacity(128 + fields.iter().map(|f| f.len()).sum::<usize>());
        out.extend_from_slice(COMMITMENT_VERSION.as_bytes());
        out.push(b'\n');
        for field in fields {
            out.extend_from_slice(field.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(field);
            out.push(b'\n');
        }
        out.extend_from_slice(self.max_tokens.to_string().as_bytes());
        out.push(b'\n');
        out
    }
}

/// SHA-256 hex commitment over the inference context.
pub fn commitment(ctx: &InferenceContext<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ctx.canonical_bytes());
    hex::encode(hasher.finalize())
}

/// SHA-256 hex of a single archive field, for size-bounded journal entries.
///
/// The journal carries the commitment plus these per-field digests; the full
/// texts live in the archive sidecar. A verifier checks the digests against the
/// archive before recomputing the commitment, so a swapped field is caught even
/// if the archive was tampered with.
pub fn field_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
