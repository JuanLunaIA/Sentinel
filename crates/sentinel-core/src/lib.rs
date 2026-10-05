//! # sentinel-core
//!
//! Pure, deterministic core of Sentinel: domain types, risk math, policy
//! evaluation and the audit hash-chain.
//!
//! **Zero network dependencies** (P00 invariant #4): this crate performs no
//! I/O and reads no clock implicitly, which makes every rule it enforces
//! unit-testable and replayable. Non-determinism (network, LLM, wall time)
//! lives in the `sentinel` application crate and is mapped into these types
//! at the trait boundary.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod policy;
pub mod risk;
pub mod types;

pub use rust_decimal::Decimal;
