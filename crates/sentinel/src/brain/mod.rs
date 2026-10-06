//! Strategy brain — providers, parser, prompts, engine, eval harness.
//!
//! The judgment half of the dual brain (`SPEC-P07.md`): never wired into the
//! reflex path (P00 invariant #1); every consult is rate-limited, schema-
//! validated, and audited before any action it proposes reaches the policy
//! gate.
//!
//! **Skeleton status (P07):** interfaces frozen; implemented by the P07 waves.

pub mod chain;
pub mod engine;
pub mod eval;
pub mod parser;
pub mod prompts;
pub mod providers;
