//! Deterministic reflex risk math — the millisecond-speed guardian.
//!
//! Everything here is pure: same input, same output, no I/O, no clock reads
//! (callers pass `now_ms`), no randomness. The rules and formulas are frozen
//! in `SPEC-P04.md`; ground truth for the liquidation math is `docs/FACTS.md`
//! §1.7 (Perpl SDK formula + unit vectors, `vendor/dex-sdk` @ `01b9910`).
//!
//! **Skeleton status (P04):** signatures are frozen here; the bodies are
//! implemented and property-tested by the P04 wave.

use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::types::{DataQuality, Intent, Market, MarketId, Position, RiskTier};

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

/// Where an effective liquidation price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiqSource {
    /// Provided by the exchange (`Position::liq_price`).
    Exchange,
    /// Derived from isolated-margin first principles ([`implied_liq_price`]).
    Derived,
}

/// Effective liquidation price together with its provenance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiqPrice {
    /// The price in collateral units per base unit.
    pub price: Decimal,
    /// Which source produced it.
    pub source: LiqSource,
}

/// Margin ratio type: position equity relative to a requirement.
///
/// See [`margin_health`]: `1.0` means exactly at the maintenance requirement.
pub type MarginRatio = Decimal;

/// Distance from mark price to liquidation, in percent of mark price.
///
/// Uses [`effective_liq_price`] (exchange value preferred, derivation as
/// fallback). Returns `None` when a needed input is missing or `mark <= 0`.
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

/// Effective liquidation price: the exchange's value when present, otherwise
/// the derived one; `None` when neither is available.
///
/// **STUB — implemented in P04.**
pub fn effective_liq_price(pos: &Position, market: &Market) -> Option<LiqPrice> {
    let _ = (pos, market);
    todo!("P04: exchange preferred, derived fallback")
}

/// Relative divergence between the derived and exchange liquidation prices,
/// in percent of the exchange value. `None` unless both are available.
///
/// Rule of use (app layer): > 2 % ⇒ trust the exchange price and raise a
/// data-quality warning.
///
/// **STUB — implemented in P04.**
pub fn liq_divergence_pct(pos: &Position, market: &Market) -> Option<Decimal> {
    let _ = (pos, market);
    todo!("P04: |derived - exchange| / exchange * 100")
}

/// Margin health: how many multiples of the maintenance requirement the
/// position's equity covers (`1.0` = exactly at the requirement, `< 1.0` is
/// liquidatable territory).
///
/// `(collateral + upnl) / (mmr * |size| * mark)`, `upnl = (mark - entry) * size`
/// with signed size. `None` when `mark` is missing or notional is zero.
///
/// **STUB — implemented in P04.**
pub fn margin_health(pos: &Position, market: &Market) -> Option<MarginRatio> {
    let _ = (pos, market);
    todo!("P04: maintenance-coverage multiple")
}

/// Classify a distance-to-liquidation percent into a [`RiskTier`].
///
/// Cuts use `>=` and a value exactly at a threshold belongs to the safer tier:
/// `>= soft → Green`, `>= warn → Yellow`, `>= hard → Orange`, else `Red`.
///
/// **STUB — implemented in P04.**
pub fn tier(distance_pct: Decimal, thresholds: &RiskThresholds) -> RiskTier {
    let _ = (distance_pct, thresholds);
    todo!("P04: thresholds comparison")
}

/// Tunable reflex parameters (mapped from the application config in P05+).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflexConfig {
    /// Fraction removed on a Red first-breach reduce, `(0, 1]`.
    pub reduce_fraction: Decimal,
    /// Fraction removed on Orange (and gated-stale) reduces, `(0, 1]`.
    pub orange_fraction: Decimal,
    /// Per-market cooldown between reflexive actions, milliseconds.
    pub cooldown_ms: u64,
    /// Whether stale data while Orange/Red may trigger a reduce (else alert).
    pub stale_reduce: bool,
}

impl Default for ReflexConfig {
    fn default() -> Self {
        Self {
            reduce_fraction: Decimal::new(5, 1),
            orange_fraction: Decimal::new(25, 2),
            cooldown_ms: 600_000,
            stale_reduce: false,
        }
    }
}

impl ReflexConfig {
    /// Validate the fraction ranges (`(0, 1]`).
    ///
    /// # Errors
    /// Returns a static description of the first violated range.
    pub fn validate(&self) -> Result<(), &'static str> {
        let one = Decimal::ONE;
        let zero = Decimal::ZERO;
        if self.reduce_fraction <= zero || self.reduce_fraction > one {
            return Err("reduce_fraction must be in (0, 1]");
        }
        if self.orange_fraction <= zero || self.orange_fraction > one {
            return Err("orange_fraction must be in (0, 1]");
        }
        Ok(())
    }
}

/// Stateless intent rules (see the table in `SPEC-P04.md` §3.4).
///
/// Returns `None` for Green/Yellow regardless of collateral size, and for any
/// tier when no rule applies. Never proposes increasing exposure.
///
/// **STUB — implemented in P04.**
pub fn reflex_intent(
    pos: &Position,
    tier: RiskTier,
    quality: DataQuality,
    cfg: &ReflexConfig,
) -> Option<Intent> {
    let _ = (pos, tier, quality, cfg);
    todo!("P04: stateless rules table")
}

/// Per-market reflex bookkeeping (private to [`ReflexState`]).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // stub fields; consumed by the P04 implementation
struct MarketReflex {
    /// Timestamp of the last emitted action for this market.
    last_action_ms: Option<u64>,
    /// Whether a Red-stage reduce already happened (escalates to Close).
    red_reduced: bool,
}

/// Cooldown & escalation bookkeeping for the reflex engine.
///
/// Pure: the caller supplies `now_ms`, so sequences are fully deterministic in
/// tests. Recovery (`tier <= Yellow`) resets the market's bookkeeping, so a
/// new breach starts with a Reduce again. See `SPEC-P04.md` §3.4.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // stub field; consumed by the P04 implementation
pub struct ReflexState {
    markets: HashMap<MarketId, MarketReflex>,
}

impl ReflexState {
    /// Fresh state with no recorded actions.
    pub fn new() -> Self {
        Self {
            markets: HashMap::new(),
        }
    }

    /// One deterministic decision step (see `SPEC-P04.md` §3.4 steps 1–6).
    ///
    /// **STUB — implemented in P04.**
    pub fn advance(
        &mut self,
        pos: &Position,
        tier: RiskTier,
        quality: DataQuality,
        cfg: &ReflexConfig,
        now_ms: u64,
    ) -> Option<Intent> {
        let _ = (pos, tier, quality, cfg, now_ms);
        todo!("P04: cooldown + escalation state machine")
    }
}
