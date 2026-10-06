//! Circuit breakers + `NoProvider` for the resilient provider chain.
//!
//! Frozen by `SPEC-P08.md` §2. "ProviderChain" semantics are implemented
//! inside `StrategyEngine` (provider + optional fallback + one breaker per
//! slot); this module exports the primitives. All clocks are caller-supplied
//! (`now_ms`) so the breaker is fully testable offline.
//!
//! **Skeleton status (P08):** interfaces frozen; implemented by the P08 wave.

use std::sync::Mutex;

use crate::brain::providers::Provider;
use crate::error::{BrainError, Result};

/// Circuit-breaker state at a given clock value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Healthy; calls allowed.
    Closed,
    /// Open until `until_ms`; calls skipped.
    Open {
        /// Wall/logical ms at which a half-open probe becomes allowed.
        until_ms: u64,
    },
    /// Open window elapsed; a single probe call is allowed.
    HalfOpen,
}

/// Consecutive-failure breaker: `threshold` strikes ⇒ open for `open_ms`.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    /// Consecutive failures needed to open.
    pub threshold: u32,
    /// Open window, ms.
    pub open_ms: u64,
    failures: u32,
    open_until_ms: Option<u64>,
}

impl CircuitBreaker {
    /// Fresh closed breaker.
    pub fn new(threshold: u32, open_ms: u64) -> Self {
        Self {
            threshold,
            open_ms,
            failures: 0,
            open_until_ms: None,
        }
    }

    /// State at `now_ms` (`SPEC-P08.md` §2 semantics).
    pub fn state(&self, now_ms: u64) -> BreakerState {
        let _ = now_ms;
        todo!("P08 agent chain: Closed / Open{{until}} / HalfOpen")
    }

    /// Whether a call may go through at `now_ms`.
    pub fn is_available(&self, now_ms: u64) -> bool {
        let _ = now_ms;
        todo!("P08 agent chain")
    }

    /// A call succeeded (closes + resets).
    pub fn record_success(&mut self) {
        todo!("P08 agent chain")
    }

    /// A call failed (3rd strike / half-open failure opens the window).
    pub fn record_failure(&mut self, now_ms: u64) {
        let _ = now_ms;
        todo!("P08 agent chain")
    }
}

/// Placeholder provider used as the default fallback type parameter.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProvider;

impl Provider for NoProvider {
    async fn complete(
        &self,
        _system: &str,
        _user: &str,
    ) -> Result<crate::brain::providers::RawCompletion> {
        Err(BrainError::AllProvidersFailed {
            last: "no fallback provider configured".to_string(),
        }
        .into())
    }

    fn name(&self) -> &'static str {
        "none"
    }
}

/// Shared breaker pair helper used by the engine's tests (`[primary, fallback]`).
pub fn breaker_pair(threshold: u32, open_ms: u64) -> Mutex<[CircuitBreaker; 2]> {
    Mutex::new([
        CircuitBreaker::new(threshold, open_ms),
        CircuitBreaker::new(threshold, open_ms),
    ])
}
