//! Deterministic reflex risk math — the millisecond-speed guardian.
//!
//! Everything here is pure: same input, same output, no I/O, no clock. The
//! functions are **stubs in P02** (signatures frozen, bodies `todo!()`);
//! P04 implements and property-tests them, including:
//!
//! * liquidation-price derivation from isolated-margin first principles
//!   (Perpl's gateway API exposes no `liq_price`; see `docs/FACTS.md` §1.7
//!   for the exact formula and its unit vectors), cross-checked against the
//!   official SDK's `Position::liquidation_price`;
//! * tier classification against soft/warn/hard thresholds;
//! * the deterministic intent rules table (Red → reduce, then close;
//!   Orange → reduce once per cooldown; stale data while Orange/Red → alert);
//! * monotonicity and boundary property tests.

use rust_decimal::Decimal;

use crate::types::{DataQuality, Intent, Market, Position, RiskTier};

/// Soft / warn / hard distance-to-liquidation thresholds, in **percent**
/// (e.g. `8` = 8%). Invariant: `hard < warn < soft`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RiskThresholds {
    /// Below this distance the data starts feeding strategy consults.
    pub soft: Decimal,
    /// Below this distance the position is flagged for de-risking.
    pub warn: Decimal,
    /// Below this distance the reflex engine acts immediately.
    pub hard: Decimal,
}

impl RiskThresholds {
    /// Validate the ordering invariant `hard < warn < soft`.
    ///
    /// # Errors
    /// Returns a static description when the ordering is violated.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.hard < self.warn && self.warn < self.soft {
            Ok(())
        } else {
            Err("risk thresholds must satisfy hard < warn < soft")
        }
    }
}

/// Distance from mark price to liquidation, in percent of mark price.
///
/// Uses the exchange-provided `liq_price` when present, otherwise the derived
/// liquidation price ([`implied_liq_price`]). Returns `None` when there is not
/// enough data for either path.
///
/// **STUB — implemented in P04.**
pub fn distance_to_liq_pct(pos: &Position) -> Option<Decimal> {
    let _ = pos;
    todo!("P04: |mark - liq| / mark * 100 with derivation fallback")
}

/// Liquidation price derived from isolated-margin first principles.
///
/// Perpl formula (see `docs/FACTS.md` §1.7, sourced from the official SDK):
/// `liq = entry + side * (MMR - collateral) / |size|` with
/// `MMR = entry * |size| * maintenance_margin_fraction`, `side = +1 long`.
///
/// **STUB — implemented in P04.**
pub fn implied_liq_price(pos: &Position, market: &Market) -> Option<Decimal> {
    let _ = (pos, market);
    todo!("P04: derive from maintenance-margin fraction")
}

/// Classify a distance-to-liquidation percent into a [`RiskTier`].
///
/// **STUB — implemented in P04.**
pub fn tier(distance_pct: Decimal, thresholds: &RiskThresholds) -> RiskTier {
    let _ = (distance_pct, thresholds);
    todo!("P04: thresholds comparison")
}

/// Deterministic intent rules for the reflex engine.
///
/// Returns `None` when no action is warranted (e.g. Green/Yellow with fresh
/// data). The full documented rules table lands in P04.
///
/// **STUB — implemented in P04.**
pub fn reflex_intent(
    pos: &Position,
    tier: RiskTier,
    quality: DataQuality,
    reduce_fraction: Decimal,
) -> Option<Intent> {
    let _ = (pos, tier, quality, reduce_fraction);
    todo!("P04: rules table")
}
