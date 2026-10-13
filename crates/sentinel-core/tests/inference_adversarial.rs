//! Adversarial suite for the inference commitment (P23 / GAP-014 layer 1).
//!
//! The properties that matter, in order of how badly they would hurt if false:
//!
//! 1. **Injective encoding.** Moving text between fields must change the
//!    commitment. Plain concatenation fails this, and a guardian whose
//!    commitment can be re-interpreted is not proof of anything.
//! 2. **Field sensitivity.** Every committed field changes the digest.
//! 3. **Determinism.** The same context always produces the same digest, so a
//!    reference verifier can recompute it years later.
//! 4. **Version binding.** The format version is inside the digest, so a v1
//!    commitment can never be confused with a future format.

use sentinel_core::inference::{COMMITMENT_VERSION, InferenceContext, commitment, field_digest};

fn ctx<'a>(system: &'a str, user: &'a str, completion: &'a str) -> InferenceContext<'a> {
    InferenceContext {
        system_prompt: system,
        schema_doc: "SCHEMA",
        user_prompt: user,
        model_id: "qwen3.8-max",
        provider: "qwen",
        temperature: "0.2",
        max_tokens: 4000,
        raw_completion: completion,
    }
}

#[test]
fn is_deterministic() {
    let a = ctx("sys", "user", "{\"action\":\"Hold\"}");
    let b = ctx("sys", "user", "{\"action\":\"Hold\"}");
    assert_eq!(commitment(&a), commitment(&b));
    assert_eq!(commitment(&a).len(), 64, "sha256 hex");
}

#[test]
fn boundary_between_fields_is_unambiguous() {
    // The classic concatenation collision: ("ab","c") vs ("a","bc").
    let left = ctx("ab", "c", "x");
    let right = ctx("a", "bc", "x");
    assert_ne!(
        commitment(&left),
        commitment(&right),
        "the length-prefixed encoding must make field boundaries explicit"
    );

    // Same attack across the user-prompt / completion boundary.
    let left = ctx("s", "ab", "c");
    let right = ctx("s", "a", "bc");
    assert_ne!(commitment(&left), commitment(&right));

    // And across system / user.
    let left = ctx("ab", "c", "same");
    let right = ctx("a", "bc", "same");
    assert_ne!(commitment(&left), commitment(&right));
}

#[test]
fn every_field_changes_the_digest() {
    let base = ctx("sys", "user", "comp");
    let baseline = commitment(&base);

    let mut other = base.clone();
    other.system_prompt = "sys!";
    assert_ne!(commitment(&other), baseline, "system_prompt");

    let mut other = base.clone();
    other.schema_doc = "SCHEMA!";
    assert_ne!(commitment(&other), baseline, "schema_doc");

    let mut other = base.clone();
    other.user_prompt = "user!";
    assert_ne!(commitment(&other), baseline, "user_prompt");

    let mut other = base.clone();
    other.model_id = "kimi-k3";
    assert_ne!(commitment(&other), baseline, "model_id");

    let mut other = base.clone();
    other.provider = "kimi";
    assert_ne!(commitment(&other), baseline, "provider");

    let mut other = base.clone();
    other.temperature = "0.7";
    assert_ne!(commitment(&other), baseline, "temperature");

    let mut other = base.clone();
    other.max_tokens = 4001;
    assert_ne!(commitment(&other), baseline, "max_tokens");

    let mut other = base.clone();
    other.raw_completion = "comp!";
    assert_ne!(commitment(&other), baseline, "raw_completion");
}

#[test]
fn field_order_is_part_of_the_format() {
    // Two contexts identical except for which string sits where must differ.
    let a = InferenceContext {
        system_prompt: "AAA",
        schema_doc: "BBB",
        ..ctx("x", "y", "z")
    };
    let b = InferenceContext {
        system_prompt: "BBB",
        schema_doc: "AAA",
        ..ctx("x", "y", "z")
    };
    assert_ne!(commitment(&a), commitment(&b));
}

#[test]
fn version_is_inside_the_digest() {
    // The canonical bytes must start with the version tag: a verifier reading
    // a v1 commitment can rely on the format being the one it knows.
    let context = ctx("sys", "user", "comp");
    let bytes = context.canonical_bytes();
    let prefix = format!("{COMMITMENT_VERSION}\n");
    assert!(
        bytes.starts_with(prefix.as_bytes()),
        "canonical bytes must lead with the commitment version"
    );
}

#[test]
fn empty_and_large_fields_are_handled() {
    let empty = ctx("", "", "");
    let digest = commitment(&empty);
    assert_eq!(digest.len(), 64);

    let big = "x".repeat(1_000_000);
    let large = ctx(&big, &big, &big);
    assert_eq!(commitment(&large).len(), 64);

    // An empty field must not equal a missing one.
    let with_empty_system = ctx("", "user", "comp");
    let with_content = ctx("s", "user", "comp");
    assert_ne!(commitment(&with_empty_system), commitment(&with_content));
}

#[test]
fn max_tokens_zero_is_distinct_from_absent_effect() {
    let mut zero = ctx("s", "u", "c");
    zero.max_tokens = 0;
    let with_zero = commitment(&zero);
    let normal = commitment(&ctx("s", "u", "c"));
    assert_ne!(with_zero, normal);
}

#[test]
fn field_digest_is_a_plain_sha256() {
    // Known vector: sha256("abc").
    assert_eq!(
        field_digest(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(field_digest(b""), field_digest(b""), "deterministic");
}

#[test]
fn canonical_bytes_round_trip_shape() {
    // The canonical form is inspectable: a verifier can print it, and a human
    // can read the field boundaries.
    let context = ctx("S", "U", "C");
    let text = String::from_utf8(context.canonical_bytes()).expect("utf8");
    assert!(text.contains("1:S"), "length-prefixed system prompt");
    assert!(text.contains("1:U"), "length-prefixed user prompt");
    assert!(text.contains("1:C"), "length-prefixed completion");
    assert!(text.contains("4000"), "max tokens present");
}
