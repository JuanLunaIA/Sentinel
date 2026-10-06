//! Circuit breakers + `NoProvider` for the resilient provider chain.
//!
//! Frozen by `SPEC-P08.md` §2. "ProviderChain" semantics are implemented
//! inside `StrategyEngine` (provider + optional fallback + one breaker per
//! slot); this module exports the primitives. All clocks are caller-supplied
//! (`now_ms`) so the breaker is fully testable offline.
//!
//! **Status (P08):** interfaces frozen (`SPEC-P08.md` §2); implemented by the
//! P08 wave.

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
    ///
    /// While `now_ms < until_ms` the breaker is `Open { until_ms }`; at
    /// `now_ms >= until_ms` the window has elapsed and a single probe is
    /// allowed (`HalfOpen`); with no window recorded it is `Closed`.
    pub fn state(&self, now_ms: u64) -> BreakerState {
        match self.open_until_ms {
            Some(until_ms) if now_ms < until_ms => BreakerState::Open { until_ms },
            Some(_) => BreakerState::HalfOpen,
            None => BreakerState::Closed,
        }
    }

    /// Whether a call may go through at `now_ms`.
    ///
    /// `true` for `Closed` and `HalfOpen`; `false` only while an open window
    /// is still running (i.e. anything except an unelapsed `Open`).
    pub fn is_available(&self, now_ms: u64) -> bool {
        !matches!(self.state(now_ms), BreakerState::Open { .. })
    }

    /// A call succeeded (closes + resets).
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.open_until_ms = None;
    }

    /// A call failed: applies one strike; opens once `threshold` consecutive
    /// failures accumulate, or immediately when the failing call was the
    /// half-open probe.
    ///
    /// Opening always records a full fresh window `now_ms + open_ms` (a
    /// half-open failure re-opens rather than extending the old deadline).
    pub fn record_failure(&mut self, now_ms: u64) {
        let half_open_probe = matches!(self.state(now_ms), BreakerState::HalfOpen);
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.threshold || half_open_probe {
            self.open_until_ms = Some(now_ms.saturating_add(self.open_ms));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Engine default: 3 strikes, open for 5 minutes.
    const THRESHOLD: u32 = 3;
    const OPEN_MS: u64 = 300_000;

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new(THRESHOLD, OPEN_MS)
    }

    /// Drives `breaker()` to `Open` with 3 failures at 1_000/2_000/3_000 ms
    /// and returns the breaker plus the exact open deadline.
    fn tripped() -> (CircuitBreaker, u64) {
        let mut cb = breaker();
        cb.record_failure(1_000);
        cb.record_failure(2_000);
        cb.record_failure(3_000);
        let until = 3_000 + OPEN_MS;
        (cb, until)
    }

    #[test]
    fn fresh_breaker_is_closed_and_available() {
        let cb = breaker();
        assert_eq!(cb.state(0), BreakerState::Closed);
        assert!(cb.is_available(0));
        assert_eq!(cb.failures, 0);
        assert_eq!(cb.state(u64::MAX), BreakerState::Closed);
        assert!(cb.is_available(u64::MAX));
    }

    #[test]
    fn two_failures_stay_closed() {
        let mut cb = breaker();
        cb.record_failure(1_000);
        assert_eq!(cb.state(1_000), BreakerState::Closed);
        cb.record_failure(2_000);
        assert_eq!(cb.state(2_000), BreakerState::Closed);
        assert!(cb.is_available(2_000));
        assert_eq!(cb.failures, 2);
    }

    #[test]
    fn third_failure_opens_until_now_plus_open_ms() {
        let (cb, until) = tripped();
        assert_eq!(cb.state(3_000), BreakerState::Open { until_ms: until });
        assert_eq!(until, 3_000 + OPEN_MS);
        assert!(!cb.is_available(3_000));
    }

    #[test]
    fn open_window_boundary_is_exact() {
        let (cb, until) = tripped();

        assert_eq!(cb.state(until - 1), BreakerState::Open { until_ms: until });
        assert!(!cb.is_available(until - 1));

        assert_eq!(cb.state(until), BreakerState::HalfOpen);
        assert!(cb.is_available(until));
    }

    #[test]
    fn half_open_failure_reopens_full_fresh_window() {
        let (mut cb, first_until) = tripped();

        // The probe lands after the first window elapsed ⇒ half-open.
        let probe_at = first_until + 10;
        assert_eq!(cb.state(probe_at), BreakerState::HalfOpen);
        cb.record_failure(probe_at);

        // Re-opened for a full fresh window measured from the probe, not an
        // extension of the old deadline.
        let second_until = probe_at + OPEN_MS;
        assert_eq!(
            cb.state(probe_at),
            BreakerState::Open {
                until_ms: second_until
            }
        );
        assert_ne!(second_until, first_until + OPEN_MS);
        // Failures stay at/above the threshold.
        assert!(cb.failures >= THRESHOLD);
        assert!(!cb.is_available(second_until - 1));
        assert_eq!(cb.state(second_until), BreakerState::HalfOpen);
        assert!(cb.is_available(second_until));
    }

    #[test]
    fn half_open_success_closes_and_resets() {
        let (mut cb, until) = tripped();
        assert_eq!(cb.state(until), BreakerState::HalfOpen);

        cb.record_success();
        assert_eq!(cb.state(until), BreakerState::Closed);
        assert!(cb.is_available(until));
        assert_eq!(cb.failures, 0);

        // A subsequent single failure does not re-open (counter reached 0).
        cb.record_failure(until);
        assert_eq!(cb.state(until), BreakerState::Closed);
        assert!(cb.is_available(until));
        assert_eq!(cb.failures, 1);
    }

    #[test]
    fn success_resets_partially_failed_breaker() {
        let mut cb = breaker();
        cb.record_failure(1_000);
        cb.record_failure(2_000);
        assert_eq!(cb.failures, 2);

        cb.record_success();
        assert_eq!(cb.state(3_000), BreakerState::Closed);
        assert!(cb.is_available(3_000));
        assert_eq!(cb.failures, 0);

        // Two fresh strikes must not trip: the counter really reset.
        cb.record_failure(4_000);
        cb.record_failure(5_000);
        assert_eq!(cb.state(5_000), BreakerState::Closed);
        assert!(cb.is_available(5_000));
    }

    #[test]
    fn new_round_trips_threshold_and_open_ms() {
        let cb = CircuitBreaker::new(7, 123_456);
        assert_eq!(cb.threshold, 7);
        assert_eq!(cb.open_ms, 123_456);

        // Non-default values drive the machine, not hard-coded defaults.
        let mut one_strike = CircuitBreaker::new(1, 50);
        assert!(one_strike.is_available(10));
        one_strike.record_failure(10);
        assert_eq!(one_strike.state(10), BreakerState::Open { until_ms: 60 });
        assert_eq!(one_strike.state(59), BreakerState::Open { until_ms: 60 });
        assert!(!one_strike.is_available(59));
        assert_eq!(one_strike.state(60), BreakerState::HalfOpen);
        assert!(one_strike.is_available(60));
    }
}
