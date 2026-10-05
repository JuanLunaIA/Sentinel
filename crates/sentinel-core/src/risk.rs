//! Deterministic reflex risk math — the millisecond-speed guardian.
//!
//! Everything here is pure: same input, same output, no I/O, no clock reads
//! (callers pass `now_ms`), no randomness. The rules and formulas are frozen
//! in `SPEC-P04.md`; ground truth for the liquidation math is `docs/FACTS.md`
//! §1.7 (Perpl SDK formula + unit vectors, `vendor/dex-sdk` @ `01b9910`).
//!
//! **P04 status:** the bodies are implemented and property-tested in this
//! module; the public signatures are frozen per `SPEC-P04.md` §4.

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

/// Distance from mark price to liquidation, in percent of mark price:
/// `|mark - liq| / mark * 100`, with `mark = pos.mark_price` and `liq` taken
/// from [`effective_liq_price`] — the exchange-provided `pos.liq_price` when
/// present, otherwise the price derived from first principles
/// ([`implied_liq_price`], via `market.maintenance_margin_fraction`).
///
/// Returns `None` when the position has no size, when `mark` is missing or
/// `mark <= 0`, or when neither an exchange nor a derived liquidation price
/// is available.
pub fn distance_to_liq_pct(pos: &Position, market: &Market) -> Option<Decimal> {
    // No notional to protect (SPEC-P04 §3.4 edge case).
    if pos.size.is_zero() {
        return None;
    }
    let mark = pos.mark_price?;
    if mark <= Decimal::ZERO {
        return None;
    }
    // Exchange value preferred, first-principles derivation as fallback
    // (SPEC-P04 §3.1, v1.0.1).
    let liq = effective_liq_price(pos, market)?.price;
    Some((mark - liq).abs() / mark * Decimal::ONE_HUNDRED)
}

/// Liquidation price derived from isolated-margin first principles.
///
/// Perpl formula (see `docs/FACTS.md` §1.7, sourced from the official SDK):
/// `liq = entry + side * (MMR - collateral) / |size|` with
/// `MMR = entry * |size| * maintenance_margin_fraction`, `side = +1 long`.
pub fn implied_liq_price(pos: &Position, market: &Market) -> Option<Decimal> {
    let size_abs = pos.size.abs();
    if size_abs.is_zero() || pos.entry_price <= Decimal::ZERO {
        return None;
    }
    let side = if pos.size > Decimal::ZERO {
        Decimal::ONE
    } else {
        -Decimal::ONE
    };
    let requirement = pos.entry_price * size_abs * market.maintenance_margin_fraction;
    Some(pos.entry_price + side * (requirement - pos.collateral) / size_abs)
}

/// Effective liquidation price: the exchange's value when present, otherwise
/// the derived one; `None` when neither is available.
pub fn effective_liq_price(pos: &Position, market: &Market) -> Option<LiqPrice> {
    // No notional to protect: a flat position reports no effective price,
    // exchange-provided or not (SPEC-P04 §3.4 edge case).
    if pos.size.is_zero() {
        return None;
    }
    if let Some(price) = pos.liq_price {
        return Some(LiqPrice {
            price,
            source: LiqSource::Exchange,
        });
    }
    implied_liq_price(pos, market).map(|price| LiqPrice {
        price,
        source: LiqSource::Derived,
    })
}

/// Relative divergence between the derived and exchange liquidation prices,
/// in percent of the exchange value. `None` unless both are available.
///
/// Rule of use (app layer): > 2 % ⇒ trust the exchange price and raise a
/// data-quality warning.
pub fn liq_divergence_pct(pos: &Position, market: &Market) -> Option<Decimal> {
    let exchange = pos.liq_price?;
    if exchange <= Decimal::ZERO {
        // The relative gap is undefined against a non-positive reference; a
        // zero/negative exchange price is itself unusable data.
        return None;
    }
    let derived = implied_liq_price(pos, market)?;
    Some((derived - exchange).abs() / exchange * Decimal::ONE_HUNDRED)
}

/// Margin health: how many multiples of the maintenance requirement the
/// position's equity covers (`1.0` = exactly at the requirement, `< 1.0` is
/// liquidatable territory).
///
/// `(collateral + upnl) / (mmr * |size| * mark)`, `upnl = (mark - entry) * size`
/// with signed size. `None` when `mark` is missing or notional is zero.
pub fn margin_health(pos: &Position, market: &Market) -> Option<MarginRatio> {
    let mark = pos.mark_price?;
    if mark <= Decimal::ZERO {
        return None;
    }
    let notional = pos.size.abs() * mark;
    if notional.is_zero() {
        return None;
    }
    let requirement = market.maintenance_margin_fraction * notional;
    if requirement <= Decimal::ZERO {
        // A degenerate maintenance fraction leaves the ratio undefined.
        return None;
    }
    let upnl = (mark - pos.entry_price) * pos.size;
    Some((pos.collateral + upnl) / requirement)
}

/// Classify a distance-to-liquidation percent into a [`RiskTier`].
///
/// Cuts use `>=` and a value exactly at a threshold belongs to the safer tier:
/// `>= soft → Green`, `>= warn → Yellow`, `>= hard → Orange`, else `Red`.
pub fn tier(distance_pct: Decimal, thresholds: &RiskThresholds) -> RiskTier {
    if distance_pct >= thresholds.soft {
        RiskTier::Green
    } else if distance_pct >= thresholds.warn {
        RiskTier::Yellow
    } else if distance_pct >= thresholds.hard {
        RiskTier::Orange
    } else {
        RiskTier::Red
    }
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

/// Reason attached to a first-breach Red reduce.
const REASON_RED_REDUCE: &str = "red tier: first-breach reduce";
/// Reason attached to an Orange de-risking reduce.
const REASON_ORANGE_REDUCE: &str = "orange tier: de-risking reduce";
/// Reason attached to a stale-gated reduce (`stale_reduce == true`).
const REASON_STALE_REDUCE: &str = "stale data: gated reduce";
/// Reason attached to the escalation close after a Red reduce already ran.
const REASON_RED_CLOSE: &str = "red tier: still in breach after reduce";

/// Build a [`Intent::Reduce`] with the given fraction and reason.
fn reduce_intent(fraction: Decimal, reason: &str) -> Intent {
    Intent::Reduce {
        fraction,
        reason: reason.to_string(),
    }
}

/// Stateless result for a stale/missing feed while Orange or Red: the gated
/// reduce when `cfg.stale_reduce`, otherwise an informational alert.
fn stale_result(tier: RiskTier, stale_secs: Option<u64>, cfg: &ReflexConfig) -> Intent {
    if cfg.stale_reduce {
        reduce_intent(cfg.orange_fraction, REASON_STALE_REDUCE)
    } else {
        let data = stale_secs.map_or_else(
            || "missing data".to_string(),
            |secs| format!("stale data ({secs}s)"),
        );
        Intent::Alert {
            message: format!("{data} at {tier:?} tier: reduce disabled, manual attention required"),
        }
    }
}

/// Stateless intent rules (see the table in `SPEC-P04.md` §3.4).
///
/// Returns `None` for Green/Yellow regardless of collateral size, and for any
/// tier when no rule applies. Never proposes increasing exposure.
pub fn reflex_intent(
    pos: &Position,
    tier: RiskTier,
    quality: DataQuality,
    cfg: &ReflexConfig,
) -> Option<Intent> {
    // No notional to protect (SPEC-P04 §3.4 edge case).
    if pos.size.is_zero() {
        return None;
    }
    match tier {
        RiskTier::Green | RiskTier::Yellow => None,
        RiskTier::Red => match quality {
            DataQuality::Fresh => Some(reduce_intent(cfg.reduce_fraction, REASON_RED_REDUCE)),
            DataQuality::Stale { secs } => Some(stale_result(RiskTier::Red, Some(secs), cfg)),
            DataQuality::Missing => Some(stale_result(RiskTier::Red, None, cfg)),
        },
        RiskTier::Orange => match quality {
            DataQuality::Fresh => Some(reduce_intent(cfg.orange_fraction, REASON_ORANGE_REDUCE)),
            DataQuality::Stale { secs } => Some(stale_result(RiskTier::Orange, Some(secs), cfg)),
            DataQuality::Missing => Some(stale_result(RiskTier::Orange, None, cfg)),
        },
    }
}

/// Per-market reflex bookkeeping (private to [`ReflexState`]).
#[derive(Debug, Clone, Default)]
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
    pub fn advance(
        &mut self,
        pos: &Position,
        tier: RiskTier,
        quality: DataQuality,
        cfg: &ReflexConfig,
        now_ms: u64,
    ) -> Option<Intent> {
        // No notional to protect: no decision, no bookkeeping (SPEC-P04 §3.4).
        if pos.size.is_zero() {
            return None;
        }
        let book = self.markets.entry(pos.market_id).or_default();

        // Step 1 — recovery (`tier <= Yellow`) resets the market's
        // bookkeeping, so a new breach starts with a first-breach Reduce.
        if tier <= RiskTier::Yellow {
            book.last_action_ms = None;
            book.red_reduced = false;
            return None;
        }

        // Step 2 — per-market cooldown; equality counts as elapsed.
        if let Some(last_action_ms) = book.last_action_ms
            && now_ms.saturating_sub(last_action_ms) < cfg.cooldown_ms
        {
            return None;
        }

        // Steps 3–5 — Red escalation, Orange cadence, stale/missing gate.
        let intent = match (tier, quality) {
            (RiskTier::Red, DataQuality::Fresh) => {
                if book.red_reduced {
                    // Step 3 — a Red reduce already happened: escalate.
                    Some(Intent::Close {
                        reason: REASON_RED_CLOSE.to_string(),
                    })
                } else {
                    book.red_reduced = true;
                    Some(reduce_intent(cfg.reduce_fraction, REASON_RED_REDUCE))
                }
            }
            // Step 4 (Orange + Fresh) and step 5 (stale/missing at Orange|Red).
            _ => reflex_intent(pos, tier, quality, cfg),
        };

        // Step 6 — any emitted intent records the market's action timestamp.
        if intent.is_some() {
            book.last_action_ms = Some(now_ms);
        }
        intent
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use proptest::prelude::*;
    use rust_decimal_macros::dec;

    use super::*;

    /// ETH-like market: `maintenance_margin = 2000` ⇒ `mmr = 100 / 2000 = 0.05`.
    fn eth_market() -> Market {
        Market {
            id: MarketId(32),
            symbol: "ETH".to_string(),
            base: "ETH".to_string(),
            price_decimals: 2,
            size_decimals: 3,
            initial_margin_fraction: dec!(0.083333),
            maintenance_margin_fraction: dec!(0.05),
            max_leverage: dec!(12),
            min_size: Decimal::ZERO,
            tick_size: dec!(0.01),
            maker_fee_micros: 45,
            taker_fee_micros: 345,
            order_ttl_blocks: 20,
        }
    }

    /// Position on market 32 with no exchange-provided liquidation price.
    fn position(
        size: Decimal,
        entry_price: Decimal,
        mark_price: Option<Decimal>,
        collateral: Decimal,
    ) -> Position {
        Position {
            market_id: MarketId(32),
            symbol: "ETH".to_string(),
            size,
            entry_price,
            mark_price,
            liq_price: None,
            collateral,
            unrealized_pnl: Decimal::ZERO,
            margin_ratio: None,
            leverage: dec!(10),
            opened_at: None,
        }
    }

    /// P04 defaults: soft 25 % / warn 15 % / hard 8 %.
    fn thresholds() -> RiskThresholds {
        RiskThresholds {
            soft: dec!(25),
            warn: dec!(15),
            hard: dec!(8),
        }
    }

    /// True when `d` is at least 0.001 percentage points away from every cut.
    fn away_from_cuts(d: Decimal, thresholds: &RiskThresholds) -> bool {
        [thresholds.soft, thresholds.warn, thresholds.hard]
            .iter()
            .all(|cut| (d - *cut).abs() > dec!(0.001))
    }

    #[test]
    fn sdk_vectors_implied_liq_prices() {
        // SDK fixture (`vendor/dex-sdk` `test_liquidation_price`): entry 100,
        // |size| 10, collateral (deposit) 100, maintenance margin 20
        // ⇒ mmr = 100 / 2000 = 0.05, MMR requirement = 100 * 10 * 0.05 = 50.
        let market = eth_market();
        let long = position(dec!(10), dec!(100), Some(dec!(100)), dec!(100));
        let short = position(dec!(-10), dec!(100), Some(dec!(100)), dec!(100));

        // Closed forms: liq_long  = entry * (1 + mmr) - collateral / |size|
        //                          = 100 * 1.05 - 10 = 95.
        //               liq_short = entry * (1 - mmr) + collateral / |size|
        //                          = 100 * 0.95 + 10 = 105.
        assert_eq!(implied_liq_price(&long, &market), Some(dec!(95)));
        assert_eq!(implied_liq_price(&short, &market), Some(dec!(105)));

        // The SDK's other two vectors are bankruptcy prices
        // (`entry - side * collateral / |size|` = 90 long / 110 short; the
        // 90/110 liq pair after +50 premium needs funding fields the core
        // `Position` does not carry — STUB-11). Bankruptcy is not part of the
        // P04 API surface, so only the liq vectors are verified here.

        // Distance values with mark 100: |100 - 95| / 100 * 100 = 5 exactly;
        // |100 - 105| / 100 * 100 = 5 exactly.
        let mut long = long;
        long.liq_price = Some(dec!(95));
        assert_eq!(distance_to_liq_pct(&long, &market), Some(dec!(5)));
        let mut short = short;
        short.liq_price = Some(dec!(105));
        assert_eq!(distance_to_liq_pct(&short, &market), Some(dec!(5)));

        // Margin health at the SDK fixture: (100 + 0) / 50 = 2.
        assert_eq!(margin_health(&long, &market), Some(dec!(2)));
        assert_eq!(margin_health(&short, &market), Some(dec!(2)));
    }

    #[test]
    fn hand_computed_green_scenario() {
        // ETH long: entry 2700.00, |size| 10, collateral 13 560.00, mmr 0.05,
        // mark 2713.70.
        //   MMR req = 2700 * 10 * 0.05            = 1 350
        //   liq     = 2700 + (1350 - 13560) / 10  = 2700 - 1221 = 1479.00
        //   dist    = (2713.70 - 1479.00) / 2713.70 * 100 ≈ 45.498765523 %
        //   health  = (13560 + (2713.70 - 2700) * 10) / (0.05 * 10 * 2713.70)
        //           = 13697 / 1356.85 ≈ 10.0947046468
        //   tier: 45.498… >= soft 25 ⇒ Green ⇒ no intent, no action.
        // (Exact decimals recomputed with Python `decimal`, prec = 50.)
        let market = eth_market();
        let mut pos = position(dec!(10), dec!(2700), Some(dec!(2713.70)), dec!(13560));

        assert_eq!(implied_liq_price(&pos, &market), Some(dec!(1479)));
        pos.liq_price = Some(dec!(1479));

        let distance = distance_to_liq_pct(&pos, &market).expect("distance computable");
        let expected = dec!(45.498765523);
        assert!(
            (distance - expected).abs() < dec!(0.000000001),
            "distance {distance} ≈ {expected}"
        );
        assert_eq!(tier(distance, &thresholds()), RiskTier::Green);

        let health = margin_health(&pos, &market).expect("health computable");
        assert!(
            (health - dec!(10.094704646792)).abs() < dec!(0.000000001),
            "health {health} ≈ 10.094704646792"
        );

        let cfg = ReflexConfig::default();
        assert_eq!(
            reflex_intent(&pos, RiskTier::Green, DataQuality::Fresh, &cfg),
            None
        );
        let mut state = ReflexState::new();
        assert_eq!(
            state.advance(&pos, RiskTier::Green, DataQuality::Fresh, &cfg, 1_000),
            None
        );
    }

    #[test]
    fn hand_computed_orange_scenario() {
        // Long: entry 100, |size| 10, collateral 150, mmr 0.05, mark 100.
        //   MMR req = 100 * 10 * 0.05          = 50
        //   liq     = 100 + (50 - 150) / 10    = 100 - 10 = 90
        //   dist    = |100 - 90| / 100 * 100   = 10.0 %  (8 <= 10 < 15 ⇒ Orange)
        //   health  = (150 + 0) / (0.05 * 10 * 100) = 150 / 50 = 3.0
        let market = eth_market();
        let mut pos = position(dec!(10), dec!(100), Some(dec!(100)), dec!(150));

        assert_eq!(implied_liq_price(&pos, &market), Some(dec!(90)));
        pos.liq_price = Some(dec!(90));
        assert_eq!(distance_to_liq_pct(&pos, &market), Some(dec!(10)));
        assert_eq!(margin_health(&pos, &market), Some(dec!(3)));
        assert_eq!(tier(dec!(10), &thresholds()), RiskTier::Orange);

        let intent = reflex_intent(
            &pos,
            RiskTier::Orange,
            DataQuality::Fresh,
            &ReflexConfig::default(),
        );
        assert!(
            matches!(intent, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)),
            "orange must reduce by orange_fraction 0.25, got {intent:?}"
        );
    }

    #[test]
    fn hand_computed_red_scenario() {
        // BTC long: entry 95 000.0, |size| 0.1, collateral 950.00, mmr 0.05,
        // mark 95 000.0 (mirrors the synthetic P04 fixture).
        //   MMR req = 95000 * 0.1 * 0.05          = 475
        //   liq     = 95000 + (475 - 950) / 0.1   = 95000 - 4750 = 90 250
        //   dist    = 4750 / 95000 * 100          = 5.0 %  (< 8 ⇒ Red)
        //   health  = (950 + 0) / (0.05 * 0.1 * 95000) = 950 / 475 = 2.0
        let market = eth_market();
        let mut pos = position(dec!(0.1), dec!(95000), Some(dec!(95000)), dec!(950));

        assert_eq!(implied_liq_price(&pos, &market), Some(dec!(90250)));
        pos.liq_price = Some(dec!(90250));
        assert_eq!(distance_to_liq_pct(&pos, &market), Some(dec!(5)));
        assert_eq!(margin_health(&pos, &market), Some(dec!(2)));
        assert_eq!(tier(dec!(5), &thresholds()), RiskTier::Red);

        let intent = reflex_intent(
            &pos,
            RiskTier::Red,
            DataQuality::Fresh,
            &ReflexConfig::default(),
        );
        assert!(
            matches!(intent, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.5)),
            "red first breach must reduce by reduce_fraction 0.5, got {intent:?}"
        );
    }

    #[test]
    fn effective_price_source_and_divergence() {
        let market = eth_market();
        let mut pos = position(dec!(10), dec!(100), Some(dec!(100)), dec!(100));

        // No exchange value ⇒ derived (liq_long = 95).
        assert_eq!(
            effective_liq_price(&pos, &market),
            Some(LiqPrice {
                price: dec!(95),
                source: LiqSource::Derived,
            })
        );
        // Divergence needs BOTH prices.
        assert_eq!(liq_divergence_pct(&pos, &market), None);

        // Exchange value present ⇒ preferred, even though derivation works.
        pos.liq_price = Some(dec!(100));
        assert_eq!(
            effective_liq_price(&pos, &market),
            Some(LiqPrice {
                price: dec!(100),
                source: LiqSource::Exchange,
            })
        );
        // |95 - 100| / 100 * 100 = 5 %.
        assert_eq!(liq_divergence_pct(&pos, &market), Some(dec!(5)));

        // Exactly matching prices ⇒ zero divergence.
        pos.liq_price = Some(dec!(95));
        assert_eq!(liq_divergence_pct(&pos, &market), Some(Decimal::ZERO));

        // Exchange above derived (95 vs 99.75):
        // |95 - 99.75| / 99.75 * 100 ≈ 4.761904761905.
        pos.liq_price = Some(dec!(99.75));
        let divergence = liq_divergence_pct(&pos, &market).expect("both prices present");
        assert!(
            (divergence - dec!(4.761904761905)).abs() < dec!(0.000000001),
            "divergence {divergence} ≈ 4.761904761905"
        );

        // A non-positive exchange reference has no defined relative gap, and
        // an exchange price without a derivable price yields no divergence.
        pos.liq_price = Some(Decimal::ZERO);
        assert_eq!(liq_divergence_pct(&pos, &market), None);
        pos.liq_price = Some(dec!(-1));
        assert_eq!(liq_divergence_pct(&pos, &market), None);
        let mut bad = position(dec!(10), Decimal::ZERO, Some(dec!(100)), dec!(100));
        assert_eq!(implied_liq_price(&bad, &market), None);
        bad.liq_price = Some(dec!(100));
        assert_eq!(liq_divergence_pct(&bad, &market), None);
        assert_eq!(
            effective_liq_price(&bad, &market),
            Some(LiqPrice {
                price: dec!(100),
                source: LiqSource::Exchange,
            })
        );
    }

    #[test]
    fn distance_requires_mark_and_derives_without_liq() {
        // v1.0.1 fallback order (SPEC-P04 §3.1): exchange price when present,
        // else the derived one; `None` when `mark` is missing or `mark <= 0`.
        let market = eth_market();
        let mut pos = position(dec!(10), dec!(100), None, dec!(100));
        assert_eq!(distance_to_liq_pct(&pos, &market), None, "mark missing");

        pos.mark_price = Some(Decimal::ZERO);
        assert_eq!(
            distance_to_liq_pct(&pos, &market),
            None,
            "mark not positive"
        );
        pos.mark_price = Some(dec!(-1));
        assert_eq!(
            distance_to_liq_pct(&pos, &market),
            None,
            "mark not positive"
        );

        // No exchange price, but the derivation covers it (mmr 0.05):
        //   MMR req = entry * |size| * mmr                    = 100 * 10 * 0.05 = 50
        //   liq     = entry + (MMR req - collateral) / |size| = 100 + (50 - 100) / 10 = 95
        //   dist    = |mark - liq| / mark * 100               = |100 - 95| / 100 * 100 = 5 %
        pos.mark_price = Some(dec!(100));
        assert_eq!(distance_to_liq_pct(&pos, &market), Some(dec!(5)), "derived");

        // Neither source is available (entry 0 ⇒ derivation undefined) ⇒ None.
        let underivable = position(dec!(10), Decimal::ZERO, Some(dec!(100)), dec!(100));
        assert_eq!(
            distance_to_liq_pct(&underivable, &market),
            None,
            "no liq source"
        );

        // An exchange price is still preferred when present.
        pos.liq_price = Some(dec!(95));
        assert_eq!(distance_to_liq_pct(&pos, &market), Some(dec!(5)));
    }

    #[test]
    fn margin_health_guards() {
        let market = eth_market();
        assert_eq!(
            margin_health(&position(dec!(10), dec!(100), None, dec!(100)), &market),
            None,
            "mark missing"
        );
        assert_eq!(
            margin_health(
                &position(dec!(10), dec!(100), Some(Decimal::ZERO), dec!(100)),
                &market
            ),
            None,
            "mark not positive"
        );
        assert_eq!(
            margin_health(
                &position(Decimal::ZERO, dec!(100), Some(dec!(100)), dec!(100)),
                &market
            ),
            None,
            "no notional"
        );
        let mut zero_mmr = eth_market();
        zero_mmr.maintenance_margin_fraction = Decimal::ZERO;
        assert_eq!(
            margin_health(
                &position(dec!(10), dec!(100), Some(dec!(100)), dec!(100)),
                &zero_mmr
            ),
            None,
            "undefined requirement"
        );
    }

    #[test]
    fn tier_boundary_cuts_belong_to_safer_tier() {
        let thresholds = thresholds();
        assert_eq!(tier(dec!(25), &thresholds), RiskTier::Green);
        assert_eq!(tier(dec!(24.999), &thresholds), RiskTier::Yellow);
        assert_eq!(tier(dec!(15), &thresholds), RiskTier::Yellow);
        assert_eq!(tier(dec!(14.999), &thresholds), RiskTier::Orange);
        assert_eq!(tier(dec!(8), &thresholds), RiskTier::Orange);
        assert_eq!(tier(dec!(7.999), &thresholds), RiskTier::Red);
        assert_eq!(tier(dec!(0), &thresholds), RiskTier::Red);
        assert_eq!(tier(dec!(-5), &thresholds), RiskTier::Red);
    }

    #[test]
    fn green_and_yellow_never_yield_intents() {
        let pos = position(dec!(10), dec!(100), Some(dec!(100)), dec!(150));
        let cfg = ReflexConfig::default();
        let qualities = [
            DataQuality::Fresh,
            DataQuality::Stale { secs: 60 },
            DataQuality::Missing,
        ];
        for tier in [RiskTier::Green, RiskTier::Yellow] {
            for quality in qualities {
                assert_eq!(
                    reflex_intent(&pos, tier, quality, &cfg),
                    None,
                    "{tier:?} + {quality:?} must not act"
                );
            }
        }
    }

    #[test]
    fn validators_reject_violations() {
        assert_eq!(thresholds().validate(), Ok(()));
        let inverted = RiskThresholds {
            soft: dec!(8),
            warn: dec!(15),
            hard: dec!(25),
        };
        assert!(inverted.validate().is_err());
        let equal = RiskThresholds {
            soft: dec!(15),
            warn: dec!(15),
            hard: dec!(8),
        };
        assert!(equal.validate().is_err());

        let cfg = ReflexConfig::default();
        assert_eq!(cfg.validate(), Ok(()));
        assert!(
            ReflexConfig {
                reduce_fraction: Decimal::ZERO,
                ..cfg
            }
            .validate()
            .is_err()
        );
        assert!(
            ReflexConfig {
                reduce_fraction: dec!(1.5),
                ..cfg
            }
            .validate()
            .is_err()
        );
        assert!(
            ReflexConfig {
                orange_fraction: Decimal::ZERO,
                ..cfg
            }
            .validate()
            .is_err()
        );
        assert!(
            ReflexConfig {
                orange_fraction: dec!(1),
                ..cfg
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn state_machine_red_escalation_and_recovery_reset() {
        // Red position (distance 5 %): first breach ⇒ Reduce, cooldown gates,
        // post-cooldown ⇒ Close, Yellow recovery resets, Red ⇒ Reduce again.
        let pos = position(dec!(0.1), dec!(95000), Some(dec!(95000)), dec!(950));
        let cfg = ReflexConfig::default();
        let cooldown = cfg.cooldown_ms;
        let t0 = 1_000;
        let mut state = ReflexState::new();

        // Step 3: first Red breach ⇒ Reduce { reduce_fraction }.
        let first = state.advance(&pos, RiskTier::Red, DataQuality::Fresh, &cfg, t0);
        assert!(
            matches!(first, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.5)),
            "first breach must reduce, got {first:?}"
        );

        // Step 2: inside the cooldown ⇒ None (cooldown - 1 is not elapsed).
        assert_eq!(
            state.advance(
                &pos,
                RiskTier::Red,
                DataQuality::Fresh,
                &cfg,
                t0 + cooldown - 1
            ),
            None,
            "cooldown not yet elapsed"
        );

        // Step 2: equality counts as elapsed ⇒ escalation (step 3).
        let escalated = state.advance(&pos, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + cooldown);
        assert!(
            matches!(escalated, Some(Intent::Close { .. })),
            "still Red after cooldown must close, got {escalated:?}"
        );

        // Step 1: recovery at Yellow resets the market's bookkeeping.
        assert_eq!(
            state.advance(
                &pos,
                RiskTier::Yellow,
                DataQuality::Fresh,
                &cfg,
                t0 + 2 * cooldown
            ),
            None
        );

        // A new Red breach after recovery starts from scratch ⇒ Reduce again.
        let again = state.advance(
            &pos,
            RiskTier::Red,
            DataQuality::Fresh,
            &cfg,
            t0 + 2 * cooldown + 1,
        );
        assert!(
            matches!(again, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.5)),
            "recovery must reset escalation, got {again:?}"
        );
    }

    #[test]
    fn state_machine_orange_cadence() {
        // Orange (distance 10 %): Reduce every cooldown, never escalating.
        let pos = position(dec!(10), dec!(100), Some(dec!(100)), dec!(150));
        let cfg = ReflexConfig::default();
        let cooldown = cfg.cooldown_ms;
        let t0 = 0;
        let mut state = ReflexState::new();

        let first = state.advance(&pos, RiskTier::Orange, DataQuality::Fresh, &cfg, t0);
        assert!(
            matches!(first, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)),
            "orange reduces by orange_fraction, got {first:?}"
        );
        assert_eq!(
            state.advance(
                &pos,
                RiskTier::Orange,
                DataQuality::Fresh,
                &cfg,
                t0 + cooldown - 1
            ),
            None
        );
        let second = state.advance(
            &pos,
            RiskTier::Orange,
            DataQuality::Fresh,
            &cfg,
            t0 + cooldown,
        );
        assert!(
            matches!(second, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)),
            "orange never escalates to Close, got {second:?}"
        );
        assert_eq!(
            state.advance(
                &pos,
                RiskTier::Orange,
                DataQuality::Fresh,
                &cfg,
                t0 + cooldown + 1
            ),
            None
        );
    }

    #[test]
    fn state_machine_stale_gate_both_branches() {
        let pos = position(dec!(0.1), dec!(95000), Some(dec!(95000)), dec!(950));
        let blocking = ReflexConfig::default(); // stale_reduce: false
        let gating = ReflexConfig {
            stale_reduce: true,
            ..ReflexConfig::default()
        };

        let stale = DataQuality::Stale { secs: 30 };
        let missing = DataQuality::Missing;

        // Stateless: stale/missing at Orange/Red ⇒ Alert when stale_reduce is
        // false, gated reduce by orange_fraction otherwise.
        for quality in [stale, missing] {
            for tier in [RiskTier::Red, RiskTier::Orange] {
                assert!(
                    matches!(
                        reflex_intent(&pos, tier, quality, &blocking),
                        Some(Intent::Alert { .. })
                    ),
                    "{tier:?} + {quality:?} with stale_reduce=false must alert"
                );
                let gated = reflex_intent(&pos, tier, quality, &gating);
                assert!(
                    matches!(gated, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)),
                    "gated stale reduce must use orange_fraction, got {gated:?}"
                );
            }
        }

        // Stateful: with stale_reduce = false the alert is emitted and counts
        // as the market's last action (step 6).
        let mut state = ReflexState::new();
        assert!(matches!(
            state.advance(&pos, RiskTier::Red, stale, &blocking, 0),
            Some(Intent::Alert { .. })
        ));
        assert_eq!(
            state.advance(&pos, RiskTier::Red, stale, &blocking, 1),
            None,
            "alert recorded as the market's last action"
        );

        // With stale_reduce = true the gated reduce fires, respects the
        // cooldown, and never escalates to Close.
        let mut state = ReflexState::new();
        let first = state.advance(&pos, RiskTier::Red, missing, &gating, 0);
        assert!(matches!(first, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)));
        assert_eq!(
            state.advance(
                &pos,
                RiskTier::Red,
                missing,
                &gating,
                gating.cooldown_ms - 1
            ),
            None
        );
        let second = state.advance(&pos, RiskTier::Red, missing, &gating, gating.cooldown_ms);
        assert!(
            matches!(second, Some(Intent::Reduce { fraction, .. }) if fraction == dec!(0.25)),
            "stale path never escalates to Close, got {second:?}"
        );
    }

    #[test]
    fn classification_10k_under_target() {
        // The README number comes from this test: 10 000 tier + reflex_intent
        // classifications must run far under 1 ms per iteration.
        let pos = position(dec!(10), dec!(2700), Some(dec!(2713.70)), dec!(13560));
        let thresholds = thresholds();
        let cfg = ReflexConfig::default();
        let iterations: u64 = 10_000;

        let started = Instant::now();
        let mut intents = 0u32;
        for i in 0..iterations {
            // Vary the input (4.00 … 53.99 %) so nothing can be hoisted; the
            // work measured is pure tier() + reflex_intent().
            let distance = Decimal::new(i as i64 % 5_000, 2) + dec!(4);
            let tier = tier(distance, &thresholds);
            if reflex_intent(&pos, tier, DataQuality::Fresh, &cfg).is_some() {
                intents += 1;
            }
        }
        let elapsed = started.elapsed();
        let micros_per_iteration = elapsed.as_micros() as f64 / iterations as f64;
        println!(
            "classification: {iterations} iterations in {elapsed:?} \
             ({micros_per_iteration:.3} µs/iteration), intents={intents}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "10 000 tier+reflex_intent iterations took {elapsed:?} (target < 1 s)"
        );
    }

    proptest! {
        #[test]
        fn monotone_distance_is_never_more_severe(
            v1 in -1_000_000i64..=2_000_000i64,
            v2 in -1_000_000i64..=2_000_000i64,
        ) {
            let thresholds = thresholds();
            let d1 = Decimal::new(v1, 4);
            let d2 = Decimal::new(v2, 4);
            let (lo, hi) = if d1 <= d2 { (d1, d2) } else { (d2, d1) };
            // Severity order: Green < Yellow < Orange < Red. A larger distance
            // must never be MORE severe, i.e. tier(lo) >= tier(hi) in that
            // order (smaller distance = at least as severe).
            prop_assert!(tier(lo, &thresholds) >= tier(hi, &thresholds), "tier({lo}) < tier({hi})");
        }

        #[test]
        fn threshold_boundaries_land_on_safer_tier(h in 1i64..=100_000i64) {
            let hard = Decimal::new(h, 3);
            let thresholds = RiskThresholds {
                hard,
                warn: hard + Decimal::new(1, 3),
                soft: hard + Decimal::new(2, 3),
            };
            prop_assert_eq!(thresholds.validate(), Ok(()));
            prop_assert_eq!(tier(thresholds.soft, &thresholds), RiskTier::Green);
            prop_assert_eq!(tier(thresholds.warn, &thresholds), RiskTier::Yellow);
            prop_assert_eq!(tier(thresholds.hard, &thresholds), RiskTier::Orange);
            prop_assert_eq!(
                tier(thresholds.hard - Decimal::new(1, 3), &thresholds),
                RiskTier::Red
            );
            // Between cuts: strictly below a cut is the next-severer tier.
            prop_assert_eq!(
                tier(thresholds.soft - Decimal::new(1, 4), &thresholds),
                RiskTier::Yellow
            );
            prop_assert_eq!(
                tier(thresholds.hard + Decimal::new(15, 4), &thresholds),
                RiskTier::Yellow
            );
        }

        #[test]
        fn long_short_mirror_symmetry_around_entry(
            entry_cents in 1i64..=10_000_000i64,
            size_cents in 1i64..=1_000_000i64,
            collateral_cents in 0i64..=10_000_000i64,
        ) {
            let market = eth_market();
            let entry = Decimal::new(entry_cents, 2);
            let size = Decimal::new(size_cents, 2);
            let collateral = Decimal::new(collateral_cents, 2);
            let mark = entry; // mirror axis: mark equals entry
            let mut long = position(size, entry, Some(mark), collateral);
            let mut short = position(-size, entry, Some(mark), collateral);

            let liq_long = implied_liq_price(&long, &market).expect("long liq derivable");
            let liq_short = implied_liq_price(&short, &market).expect("short liq derivable");
            long.liq_price = Some(liq_long);
            short.liq_price = Some(liq_short);
            let d_long = distance_to_liq_pct(&long, &market).expect("long distance");
            let d_short = distance_to_liq_pct(&short, &market).expect("short distance");

            // Mirrored constructions sit at the same distance from the mirror
            // axis: |C/s - E*m| on both sides (decimal rounding aside).
            prop_assert!((d_long - d_short).abs() < dec!(0.000001), "d_long={d_long} d_short={d_short}");
            let thresholds = thresholds();
            prop_assume!(
                away_from_cuts(d_long, &thresholds) && away_from_cuts(d_short, &thresholds)
            );
            prop_assert_eq!(
                tier(d_long, &thresholds),
                tier(d_short, &thresholds),
                "equal distance must yield equal tier"
            );
        }

        #[test]
        fn zero_size_position_yields_none_everywhere(
            entry_cents in 1i64..=100_000_000i64,
            collateral_cents in 0i64..=100_000_000i64,
            mark_cents in 1i64..=100_000_000i64,
            liq_cents in 1i64..=100_000_000i64,
        ) {
            let market = eth_market();
            let cfg = ReflexConfig::default();
            let mut pos = position(
                Decimal::ZERO,
                Decimal::new(entry_cents, 2),
                Some(Decimal::new(mark_cents, 2)),
                Decimal::new(collateral_cents, 2),
            );
            prop_assert_eq!(implied_liq_price(&pos, &market), None);
            prop_assert_eq!(effective_liq_price(&pos, &market), None);
            prop_assert_eq!(distance_to_liq_pct(&pos, &market), None);
            prop_assert_eq!(liq_divergence_pct(&pos, &market), None);
            prop_assert_eq!(margin_health(&pos, &market), None);
            for tier in [RiskTier::Green, RiskTier::Yellow, RiskTier::Orange, RiskTier::Red] {
                prop_assert_eq!(reflex_intent(&pos, tier, DataQuality::Fresh, &cfg), None);
                prop_assert_eq!(
                    reflex_intent(&pos, tier, DataQuality::Stale { secs: 60 }, &cfg),
                    None
                );
            }
            // An exchange-provided price does not change the "no notional" rule.
            pos.liq_price = Some(Decimal::new(liq_cents, 2));
            prop_assert_eq!(effective_liq_price(&pos, &market), None);
            prop_assert_eq!(distance_to_liq_pct(&pos, &market), None);
            let mut state = ReflexState::new();
            prop_assert_eq!(
                state.advance(&pos, RiskTier::Red, DataQuality::Fresh, &cfg, 0),
                None
            );
        }

        #[test]
        fn green_never_yields_an_intent_regardless_of_collateral(
            entry_cents in 1i64..=10_000_000i64,
            size_cents in 1i64..=10_000_000i64,
            extra_cents in 0i64..=100_000_000_000i64,
        ) {
            let market = eth_market();
            let entry = Decimal::new(entry_cents, 2);
            let size = Decimal::new(size_cents, 2);
            // Collateral above twice the notional ⇒ distance >= 195 % ≫ soft.
            let collateral = entry * size * dec!(2) + Decimal::new(extra_cents, 2);
            let mut pos = position(size, entry, Some(entry), collateral);

            let liq = implied_liq_price(&pos, &market).expect("derivable");
            pos.liq_price = Some(liq);
            let distance = distance_to_liq_pct(&pos, &market).expect("distance computable");
            let t = tier(distance, &thresholds());
            prop_assert_eq!(
                t,
                RiskTier::Green,
                "distance {} collateral {} entry {} size {}",
                distance,
                collateral,
                entry,
                size
            );
            let cfg = ReflexConfig::default();
            for quality in [
                DataQuality::Fresh,
                DataQuality::Stale { secs: 60 },
                DataQuality::Missing,
            ] {
                prop_assert_eq!(reflex_intent(&pos, t, quality, &cfg), None);
            }
            let mut state = ReflexState::new();
            prop_assert_eq!(state.advance(&pos, t, DataQuality::Fresh, &cfg, 0), None);
        }
    }
}
