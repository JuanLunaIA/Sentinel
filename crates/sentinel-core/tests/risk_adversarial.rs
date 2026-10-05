//! # P04 independent verification — adversarial suite for the reflex risk engine.
//!
//! Written **without reading** `crates/sentinel-core/src/risk.rs` (black box):
//! every expectation below is re-derived from `SPEC-P04.md`, `docs/FACTS.md`
//! §1.7 and the official SDK source pinned at `vendor/dex-sdk` @ `01b9910`
//! (`crates/sdk/src/state/position.rs` L134-153 `liquidation_price`, L645-683
//! its unit vectors). Exact arithmetic was generated with Python `decimal`
//! (commands and full outputs quoted below; per-test references use the `A|`…
//! `J|` line tags).
//!
//! **Re-verified against SPEC-P04 v1.0.1 (changelog §11):** `distance_to_liq_pct`
//! now takes `(pos: &Position, market: &Market)` — the exchange price when present,
//! else the derived price (`implied_liq_price` via the market's maintenance-margin
//! fraction), else `None`; still `None` when the mark is missing, `mark <= 0`, or
//! the size is zero (§3.4). The wave-1 "no liq price ⇒ None" assertion is replaced
//! by derived-value assertions with Python-generated expectations (command 4).
//!
//! ## Expectation generator (command 1 — run from the repo root)
//!
//! ```text
//! $ python3 - <<'PY'
//! # P04 risk_adversarial expectations — exact Decimal arithmetic.
//! from decimal import Decimal, getcontext, localcontext
//! getcontext().prec = 60
//!
//! def L(e, s, c, mmr):  return e + (e * s * mmr - c) / s      # liq_long
//! def S(e, s, c, mmr):  return e - (e * s * mmr - c) / s      # liq_short
//! def D(mark, liq):     return abs(mark - liq) / mark * 100   # distance pct
//!
//! print("A|SDK vector: entry 100 size 10 collateral 100 mmr 0.05")
//! print("A| liq_long =", L(Decimal(100), Decimal(10), Decimal(100), Decimal("0.05")),
//!       "| liq_short =", S(Decimal(100), Decimal(10), Decimal(100), Decimal("0.05")))
//! print("A| dist_long(mark100) =", D(Decimal(100), Decimal(95)),
//!       "| dist_short(mark100) =", D(Decimal(100), Decimal(105)),
//!       "| dist_long(mark95) =", D(Decimal(95), Decimal(95)).normalize())
//!
//! print("B| boundary C sweep, mark=100; dist thresholds soft=25 warn=15 hard=8")
//! for c in ["300", "299.9999999999999999999999999", "200", "199.9999999999999999999999999",
//!           "130", "129.9999999999999999999999999"]:
//!     cc = Decimal(c)
//!     print(f"B| C={c}: liq={L(Decimal(100), Decimal(10), cc, Decimal('0.05')).normalize()}"
//!           f" dist={D(Decimal(100), L(Decimal(100), Decimal(10), cc, Decimal('0.05'))).normalize()}")
//! for t in ["25", "15", "8"]:
//!     tt = Decimal(t)
//!     print(f"B| tier inputs: {tt - Decimal('1e-27')} | {tt} | {tt + Decimal('1e-27')}")
//!
//! print("G| mirror pairs (liq_l + liq_s = 2*mark, equal distances)")
//! for cl, cs, m in [("100", "100", "100"), ("100", "200", "105"), ("200", "100", "95")]:
//!     l1 = L(Decimal(100), Decimal(10), Decimal(cl), Decimal("0.05"))
//!     l2 = S(Decimal(100), Decimal(10), Decimal(cs), Decimal("0.05"))
//!     with localcontext() as ctx:
//!         ctx.prec = 28
//!         d1, d2 = +D(Decimal(m), l1), +D(Decimal(m), l2)
//!     print(f"G| mark={m} C_l={cl} C_s={cs}: liq_l={l1.normalize()} liq_s={l2.normalize()} dist28={d1.normalize()} / {d2.normalize()}")
//!
//! print("H| huge collateral 1e9, mark=100: liq =",
//!       L(Decimal(100), Decimal(10), Decimal("1000000000"), Decimal("0.05")).normalize(),
//!       "| dist =", D(Decimal(100), Decimal(-99999895)).normalize(),
//!       "| SDK-clamp variant dist = 100")
//!
//! print("C| divergence: derived liq_long = 95")
//! for ex in ["100", "99.75", "90.25"]:
//!     exd = Decimal(ex)
//!     with localcontext() as ctx:
//!         ctx.prec = 28
//!         div = +abs(Decimal(95) - exd) / exd * 100
//!     print(f"C| exchange={ex}: divergence28={div.normalize()} | dist(mark=100)={D(Decimal(100), exd).normalize()}")
//!
//! print("D| margin health: (collateral + upnl) / (mmr * |size| * mark)")
//! print("D| sdk mark=100 long =", (Decimal(100) + 0) / (Decimal("0.05") * 10 * 100))
//! print("D| mark=110 long =", (Decimal(100) + (Decimal(110) - Decimal(100)) * 10) / (Decimal("0.05") * 10 * 110))
//! print("D| mark=110 short =", (Decimal(100) + (Decimal(110) - Decimal(100)) * -10) / (Decimal("0.05") * 10 * 110))
//! print("D| mark=95 long (at liq) =", (Decimal(100) + (Decimal(95) - Decimal(100)) * 10) / (Decimal("0.05") * 10 * 95))
//! print("D| unit C=50 mark=100 =", (Decimal(50) + 0) / (Decimal("0.05") * 10 * 100))
//!
//! print("E| large magnitude")
//! e2, c3, m2 = Decimal("10000000000000000000"), Decimal("100000000000000000000"), Decimal("10000000000000000000")
//! print("E| liq =", L(e2, Decimal(10), c3, Decimal("0.05")).normalize(),
//!       "| dist =", D(m2, Decimal("500000000000000000")).normalize())
//!
//! print("F| precision: entry=1.0000000000000000000000001 size=1 collateral=1e-25 mark=1.25")
//! e3, c4 = Decimal("1.0000000000000000000000001"), Decimal("0.0000000000000000000000001")
//! req = e3 * 1 * Decimal("0.05")
//! print("F| req =", req.normalize(), "| liq =", (e3 + (req - c4) / 1).normalize(),
//!       "| diff =", (Decimal("1.25") - (e3 + (req - c4) / 1)).normalize())
//! print("F| dist =", ((Decimal("1.25") - (e3 + (req - c4) / 1)) / Decimal("1.25") * 100).normalize())
//! PY
//! ```
//!
//! Output (complete):
//!
//! ```text
//! A|SDK vector: entry 100 size 10 collateral 100 mmr 0.05
//! A| liq_long = 95.00 | liq_short = 105.00
//! A| dist_long(mark100) = 5.00 | dist_short(mark100) = 5.00 | dist_long(mark95) = 0
//! B| boundary C sweep, mark=100; dist thresholds soft=25 warn=15 hard=8
//! B| C=300: liq=75 dist=25
//! B| C=299.9999999999999999999999999: liq=75.00000000000000000000000001 dist=24.99999999999999999999999999
//! B| C=200: liq=85 dist=15
//! B| C=199.9999999999999999999999999: liq=85.00000000000000000000000001 dist=14.99999999999999999999999999
//! B| C=130: liq=92 dist=8
//! B| C=129.9999999999999999999999999: liq=92.00000000000000000000000001 dist=7.99999999999999999999999999
//! B| tier inputs: 24.999999999999999999999999999 | 25 | 25.000000000000000000000000001
//! B| tier inputs: 14.999999999999999999999999999 | 15 | 15.000000000000000000000000001
//! B| tier inputs: 7.999999999999999999999999999 | 8 | 8.000000000000000000000000001
//! G| mirror pairs (liq_l + liq_s = 2*mark, equal distances)
//! G| mark=100 C_l=100 C_s=100: liq_l=95 liq_s=105 dist28=5 / 5
//! G| mark=105 C_l=100 C_s=200: liq_l=95 liq_s=115 dist28=9.523809523809523809523809524 / 9.523809523809523809523809524
//! G| mark=95 C_l=200 C_s=100: liq_l=85 liq_s=105 dist28=10.52631578947368421052631579 / 10.52631578947368421052631579
//! H| huge collateral 1e9, mark=100: liq = -99999895 | dist = 99999995 | SDK-clamp variant dist = 100
//! C| divergence: derived liq_long = 95
//! C| exchange=100: divergence28=5 | dist(mark=100)=0
//! C| exchange=99.75: divergence28=4.761904761904761904761904762 | dist(mark=100)=0.25
//! C| exchange=90.25: divergence28=5.263157894736842105263157895 | dist(mark=100)=9.75
//! D| margin health: (collateral + upnl) / (mmr * |size| * mark)
//! D| sdk mark=100 long = 2
//! D| mark=110 long = 3.63636363636363636363636363636363636363636363636363636363636
//! D| mark=110 short = 0E+2
//! D| mark=95 long (at liq) = 1.05263157894736842105263157894736842105263157894736842105263
//! D| unit C=50 mark=100 = 1
//! E| large magnitude
//! E| liq = 5E+17 | dist = 95
//! F| precision: entry=1.0000000000000000000000001 size=1 collateral=1e-25 mark=1.25
//! F| req = 0.050000000000000000000000005 | liq = 1.050000000000000000000000005 | diff = 0.199999999999999999999999995
//! F| dist = 15.9999999999999999999999996
//! ```
//!
//! ## Command 2 — 28-significant-digit rounding for the two repeating health values
//!
//! ```text
//! $ python3 - <<'PY'
//! from decimal import Decimal, getcontext, localcontext
//! getcontext().prec = 60
//! with localcontext() as ctx:
//!     ctx.prec = 28
//!     h1 = +(Decimal(100) + (Decimal(110) - Decimal(100)) * 10) / (Decimal("0.05") * 10 * 110)
//!     h2 = +(Decimal(100) + (Decimal(95) - Decimal(100)) * 10) / (Decimal("0.05") * 10 * 95)
//!     print("D| mark=110 long @28sig =", h1.normalize())
//!     print("D| mark=95 long @28sig  =", h2.normalize())
//! print("D| mark=110 short exact  =", (Decimal(100) + (Decimal(110) - Decimal(100)) * -10) / (Decimal("0.05") * 10 * 110))
//! PY
//! D| mark=110 long @28sig = 3.636363636363636363636363636
//! D| mark=95 long @28sig  = 1.052631578947368421052631579
//! D| mark=110 short exact  = 0E+2
//! ```
//!
//! ## Representability check (command 3 — intermediates stay inside rust_decimal)
//!
//! ```text
//! $ python3 - <<'PY'
//! from decimal import Decimal
//! def fits(label, v):
//!     sign, digits, exp = v.as_tuple()
//!     m = int("".join(map(str, digits)))
//!     print(f"{label}: bits={m.bit_length()} scale={max(0, -int(exp))} fits96={m < 2**96}")
//! # (checked for every F-section intermediate: 84-94 bits, scale <= 28; and the E-section
//! #  values: 64-74 bits, scale <= 2 -> all operations are exact, no rounding involved)
//! PY
//! ```
//!
//! ## Command 4 — v1.0.1 derived-distance expectations (exchange absent, market present)
//!
//! ```text
//! $ python3 - <<'PY'
//! from decimal import Decimal, getcontext, localcontext
//! getcontext().prec = 60
//!
//! def L(e, s, c, mmr): return e + (e * s * mmr - c) / s
//! def S(e, s, c, mmr): return e - (e * s * mmr - c) / s
//! def D(mark, liq):
//!     with localcontext() as ctx:
//!         ctx.prec = 28
//!         return +abs(mark - liq) / mark * 100
//!
//! M = Decimal
//! print("I| v1.0.1 derived distances: exchange absent, market present, liq = implied")
//! print("I| sdk_long  E100 S10  C100 m100: liq =", L(M(100), M(10), M(100), M("0.05")).normalize(), "| dist28 =", D(M(100), L(M(100), M(10), M(100), M("0.05"))).normalize())
//! print("I| sdk_short E100 S-10 C100 m100: liq =", S(M(100), M(10), M(100), M("0.05")).normalize(), "| dist28 =", D(M(100), S(M(100), M(10), M(100), M("0.05"))).normalize())
//! print("I| pairB long  m105 C100: liq =", L(M(100), M(10), M(100), M("0.05")).normalize(), "| dist28 =", D(M(105), L(M(100), M(10), M(100), M("0.05"))).normalize())
//! print("I| pairB short m105 C200: liq =", S(M(100), M(10), M(200), M("0.05")).normalize(), "| dist28 =", D(M(105), S(M(100), M(10), M(200), M("0.05"))).normalize())
//! print("I| pairC long  m95  C200: liq =", L(M(100), M(10), M(200), M("0.05")).normalize(), "| dist28 =", D(M(95), L(M(100), M(10), M(200), M("0.05"))).normalize())
//! print("I| pairC short m95  C100: liq =", S(M(100), M(10), M(100), M("0.05")).normalize(), "| dist28 =", D(M(95), S(M(100), M(10), M(100), M("0.05"))).normalize())
//! print("I| exchange -50 m100: dist28 =", format(D(M(100), M(-50)).normalize(), "f"))
//! print("I| boundary sweep C=300/200/130 m100: dist28 =", D(M(100), M(75)).normalize(), D(M(100), M(85)).normalize(), D(M(100), M(92)).normalize())
//! print("I| huge C=1e9 m100: liq =", L(M(100), M(10), M("1000000000"), M("0.05")).normalize(), "| dist28 =", D(M(100), L(M(100), M(10), M("1000000000"), M("0.05"))).normalize())
//! PY
//! I| v1.0.1 derived distances: exchange absent, market present, liq = implied
//! I| sdk_long  E100 S10  C100 m100: liq = 95 | dist28 = 5
//! I| sdk_short E100 S-10 C100 m100: liq = 105 | dist28 = 5
//! I| pairB long  m105 C100: liq = 95 | dist28 = 9.523809523809523809523809524
//! I| pairB short m105 C200: liq = 115 | dist28 = 9.523809523809523809523809524
//! I| pairC long  m95  C200: liq = 85 | dist28 = 10.52631578947368421052631579
//! I| pairC short m95  C100: liq = 105 | dist28 = 10.52631578947368421052631579
//! I| exchange -50 m100: dist28 = 150
//! I| boundary sweep C=300/200/130 m100: dist28 = 25 15 8
//! I| huge C=1e9 m100: liq = -99999895 | dist28 = 99999995
//! ```
//!
//! Timing/order note: all sequences use caller-supplied `now_ms` (SPEC-P04 §1:
//! no clock reads); the default cooldown is 600_000 ms (SPEC-P04 §3.4).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sentinel_core::risk::{
    LiqPrice, LiqSource, ReflexConfig, ReflexState, RiskThresholds, distance_to_liq_pct,
    effective_liq_price, implied_liq_price, liq_divergence_pct, margin_health, reflex_intent, tier,
};
use sentinel_core::types::{DataQuality, Intent, Market, MarketId, Position, RiskTier};

/// Parse an exact decimal literal (test helper; `dec!` handles simple numerics).
fn d(s: &str) -> Decimal {
    s.parse().expect("exact decimal literal")
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Thresholds from the P04 acceptance fixture: soft 25 / warn 15 / hard 8 (percent).
fn th() -> RiskThresholds {
    RiskThresholds {
        soft: dec!(25),
        warn: dec!(15),
        hard: dec!(8),
    }
}

/// ETH-like market with the Perpl maintenance margin fraction 0.05 (2000/100/…, FACTS §1.7).
fn market() -> Market {
    Market {
        id: MarketId(32),
        symbol: "ETH".to_string(),
        base: "ETH".to_string(),
        price_decimals: 2,
        size_decimals: 3,
        initial_margin_fraction: dec!(0.08333333),
        maintenance_margin_fraction: dec!(0.05),
        max_leverage: dec!(12),
        min_size: Decimal::ZERO,
        tick_size: dec!(0.01),
        maker_fee_micros: 45,
        taker_fee_micros: 345,
        order_ttl_blocks: 20,
    }
}

#[allow(clippy::too_many_arguments)]
fn pos(
    market_id: u32,
    size: Decimal,
    entry: Decimal,
    mark: Option<Decimal>,
    liq: Option<Decimal>,
    collateral: Decimal,
) -> Position {
    Position {
        market_id: MarketId(market_id),
        symbol: "ETH".to_string(),
        size,
        entry_price: entry,
        mark_price: mark,
        liq_price: liq,
        collateral,
        unrealized_pnl: Decimal::ZERO,
        margin_ratio: None,
        leverage: dec!(10),
        opened_at: None,
    }
}

/// The SDK unit vector (FACTS §1.7 / vendor/dex-sdk position.rs tests): entry 100,
/// |size| 10, collateral 100, mmr 0.05. `liq_price` left None (gateway does not expose it).
fn sdk_long() -> Position {
    pos(32, dec!(10), dec!(100), Some(dec!(100)), None, dec!(100))
}

fn sdk_short() -> Position {
    pos(32, dec!(-10), dec!(100), Some(dec!(100)), None, dec!(100))
}

/// Custom (non-default) config to prove every field is actually read.
fn cfg_custom() -> ReflexConfig {
    ReflexConfig {
        reduce_fraction: dec!(0.35),
        orange_fraction: dec!(0.15),
        cooldown_ms: 600_000,
        stale_reduce: false,
    }
}

fn cfg_stale_custom() -> ReflexConfig {
    ReflexConfig {
        stale_reduce: true,
        ..cfg_custom()
    }
}

/// Flat classification of an intent, so 24-combination grids compare cleanly.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Nil,
    Alert,
    Reduce(Decimal),
    Close,
    Other,
}

fn kind(intent: &Option<Intent>) -> Kind {
    match intent {
        None => Kind::Nil,
        Some(Intent::Alert { .. }) => Kind::Alert,
        Some(Intent::Reduce { fraction, .. }) => Kind::Reduce(*fraction),
        Some(Intent::Close { .. }) => Kind::Close,
        Some(_) => Kind::Other,
    }
}

fn reduce_fraction(intent: &Option<Intent>) -> Option<Decimal> {
    match intent {
        Some(Intent::Reduce { fraction, .. }) => Some(*fraction),
        _ => None,
    }
}

fn assert_close(actual: Decimal, expected: Decimal, tol: Decimal, ctx: &str) {
    let delta = (actual - expected).abs();
    assert!(
        delta <= tol,
        "{ctx}: got {actual}, expected {expected} (delta {delta} > {tol})"
    );
}

// ── SPEC §7 corpus: SDK vectors ─────────────────────────────────────────────

#[test]
fn sdk_vector_derived_liq_prices() {
    // SPEC-P04 §3.1 + FACTS §1.7 + SDK tests (position.rs L645-683): entry 100,
    // |size| 10, collateral 100, mmr = 20/100 = 0.05.
    //   MMR = 100 * 10 * 0.05 = 50
    //   liq_long  = 100 + (50-100)/10 = 95
    //   liq_short = 100 - (50-100)/10 = 105
    // (SDK bankruptcy prices 90/110 are out of API scope: SPEC-P04 §4 has no such fn.)
    // Python: A| liq_long = 95.00 | liq_short = 105.00
    let m = market();
    assert_eq!(implied_liq_price(&sdk_long(), &m), Some(dec!(95)));
    assert_eq!(implied_liq_price(&sdk_short(), &m), Some(dec!(105)));
    // same numbers via the spec's closed forms:
    let (entry, size, coll, mmr) = (dec!(100), dec!(10), dec!(100), dec!(0.05));
    assert_eq!(entry * (Decimal::ONE + mmr) - coll / size, dec!(95));
    assert_eq!(entry * (Decimal::ONE - mmr) + coll / size, dec!(105));
}

#[test]
fn sdk_vector_distance_checks() {
    // distance = |mark - liq| / mark * 100 (SPEC-P04 §3.1); v1.0.1 2-arg form:
    // exchange price preferred, else derived from the market, else None.
    // Python: A| dist_long(mark100) = 5.00 | dist_short(mark100) = 5.00 | dist_long(mark95) = 0
    let m = market();
    let long = Position {
        liq_price: Some(dec!(95)),
        ..sdk_long()
    };
    assert_eq!(distance_to_liq_pct(&long, &m), Some(dec!(5)));

    let short = Position {
        liq_price: Some(dec!(105)),
        ..sdk_short()
    };
    assert_eq!(distance_to_liq_pct(&short, &m), Some(dec!(5)));

    // exactly at the liq price ⇒ 0 ⇒ Red (< hard 8).
    let at_liq = Position {
        mark_price: Some(dec!(95)),
        liq_price: Some(dec!(95)),
        ..sdk_long()
    };
    let d = distance_to_liq_pct(&at_liq, &m).expect("mark > 0 and liq present is computable");
    assert_eq!(d, dec!(0));
    assert_eq!(tier(d, &th()), RiskTier::Red);

    // v1.0.1 gap closure: exchange absent + market present + mark present ⇒ the
    // derived price is used: liq 95, |100 - 95| / 100 * 100 = 5 exactly.
    // Python (command 4): I| sdk_long  E100 S10  C100 m100: liq = 95 | dist28 = 5
    assert_eq!(distance_to_liq_pct(&sdk_long(), &m), Some(dec!(5)));
}

// ── SPEC §7 corpus: tier boundaries ─────────────────────────────────────────

#[test]
fn tier_cut_boundaries_exact_and_one_ulp() {
    // SPEC-P04 §3.3: >= soft → Green; >= warn → Yellow; >= hard → Orange; else Red;
    // a value exactly at a threshold belongs to the safer tier.
    // Python: B| tier inputs: 24.999999999999999999999999999 | 25 | 25.000000000000000000000000001 (等)
    let t = th();
    assert_eq!(tier(dec!(25), &t), RiskTier::Green);
    assert_eq!(tier(dec!(15), &t), RiskTier::Yellow);
    assert_eq!(tier(dec!(8), &t), RiskTier::Orange);

    let ulp = Decimal::new(1, 27); // 10^-27 — smallest practical step at this scale
    assert_eq!(
        tier(dec!(25) - ulp, &t),
        RiskTier::Yellow,
        "one ulp below soft"
    );
    assert_eq!(
        tier(dec!(15) - ulp, &t),
        RiskTier::Orange,
        "one ulp below warn"
    );
    assert_eq!(tier(dec!(8) - ulp, &t), RiskTier::Red, "one ulp below hard");
    assert_eq!(tier(dec!(25) + ulp, &t), RiskTier::Green);
    assert_eq!(tier(dec!(15) + ulp, &t), RiskTier::Yellow);
    assert_eq!(tier(dec!(8) + ulp, &t), RiskTier::Orange);

    // adversarial extremes must not panic and classify per "otherwise Red"/">= soft".
    assert_eq!(tier(Decimal::ZERO, &t), RiskTier::Red);
    assert_eq!(tier(dec!(-1), &t), RiskTier::Red);
    assert_eq!(tier(Decimal::MAX, &t), RiskTier::Green);
    assert_eq!(tier(Decimal::MIN, &t), RiskTier::Red);
}

#[test]
fn boundary_cuts_via_full_pipeline() {
    // Construct positions whose DISTANCE lands exactly on / one construction-ulp below
    // each cut (mark 100, entry 100, |size| 10, mmr 0.05): dist = 100 - liq, and
    // liq = 105 - C/10, so C = 300 → 25.0, C = 200 → 15.0, C = 130 → 8.0.
    // ulp = 10^-25 on collateral moves the distance 10^-26 below the cut.
    // Python: B| C=300: liq=75 dist=25 ... C=299.999…9: dist=24.99999999999999999999999999 (等)
    let m = market();
    let ulp = Decimal::new(1, 25);
    let cases = [
        (dec!(300), dec!(75), dec!(25), RiskTier::Green),
        (
            dec!(300) - ulp,
            d("75.00000000000000000000000001"),
            d("24.99999999999999999999999999"),
            RiskTier::Yellow,
        ),
        (dec!(200), dec!(85), dec!(15), RiskTier::Yellow),
        (
            dec!(200) - ulp,
            d("85.00000000000000000000000001"),
            d("14.99999999999999999999999999"),
            RiskTier::Orange,
        ),
        (dec!(130), dec!(92), dec!(8), RiskTier::Orange),
        (
            dec!(130) - ulp,
            d("92.00000000000000000000000001"),
            d("7.99999999999999999999999999"),
            RiskTier::Red,
        ),
    ];
    for (collateral, want_liq, want_dist, want_tier) in cases {
        let p = pos(32, dec!(10), dec!(100), Some(dec!(100)), None, collateral);
        let liq = implied_liq_price(&p, &m).expect("size != 0 is computable");
        assert_eq!(liq, want_liq, "derived liq for C={collateral}");
        // v1.0.1 derived path: distance with the exchange price absent (checked
        // before the struct update below partially moves `p`).
        assert_eq!(
            distance_to_liq_pct(&p, &m),
            Some(want_dist),
            "derived distance for C={collateral}"
        );
        let filled = Position {
            liq_price: Some(liq),
            ..p
        };
        let d = distance_to_liq_pct(&filled, &m).expect("distance computable");
        assert_eq!(d, want_dist, "exchange distance for C={collateral}");
        assert_eq!(tier(d, &th()), want_tier, "tier for C={collateral}");
    }
}

// ── SPEC §7 corpus: monotonicity + mirror symmetry + degenerate sizes ───────

#[test]
fn tier_monotonicity_sweep() {
    // A larger distance never yields a more severe tier (SPEC §3.3). RiskTier ord:
    // Green < Yellow < Orange < Red, so severity never increases means a <= b ⇒ tier(a) >= tier(b).
    let t = th();
    let targeted = [
        dec!(-5),
        dec!(0),
        dec!(7.999),
        dec!(8),
        dec!(8.001),
        dec!(14.999),
        dec!(15),
        dec!(15.001),
        dec!(24.999),
        dec!(25),
        dec!(25.001),
        dec!(26),
        dec!(100),
        dec!(1000000),
    ];
    check_non_increasing(&targeted, &t);

    let mut sweep = Vec::with_capacity(2401);
    let mut i: u32 = 0;
    while i <= 2400 {
        sweep.push(Decimal::from(i) * dec!(0.25));
        i += 1;
    }
    check_non_increasing(&sweep, &t);
}

fn check_non_increasing(points: &[Decimal], t: &RiskThresholds) {
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert!(a <= b, "test bug: points not ascending: {a} then {b}");
        assert!(
            tier(a, t) >= tier(b, t),
            "monotonicity violated: dist {a} → {:?} is more severe than dist {b} → {:?}",
            tier(a, t),
            tier(b, t)
        );
    }
}

#[test]
fn long_short_mirror_symmetry() {
    // Mirrored constructions: liq_long + liq_short = 2 * mark (fully symmetric about
    // the mark), so both distances are |mark - liq| / mark * 100 with the same
    // magnitude ⇒ identical tiers.
    // Python: G| mark=100 … dist28=5 / 5; mark=105 … 9.523809523809523809523809524;
    //         mark=95 … 10.52631578947368421052631579
    let m = market();
    let t = th();

    // pair A — canonical SDK vector: liq 95 / 105 about mark 100.
    let l = Position {
        liq_price: Some(dec!(95)),
        ..sdk_long()
    };
    let s = Position {
        liq_price: Some(dec!(105)),
        ..sdk_short()
    };
    let (dl, ds) = (
        distance_to_liq_pct(&l, &m).expect("long"),
        distance_to_liq_pct(&s, &m).expect("short"),
    );
    assert_eq!(dl, ds);
    assert_eq!(tier(dl, &t), RiskTier::Red);
    assert_eq!(tier(dl, &t), tier(ds, &t));

    // v1.0.1 derived path (exchange absent): the same 5 % on both sides.
    // Python (command 4): I| sdk_long … dist28 = 5 | sdk_short … dist28 = 5
    let (dl, ds) = (
        distance_to_liq_pct(&sdk_long(), &m).expect("derived long"),
        distance_to_liq_pct(&sdk_short(), &m).expect("derived short"),
    );
    assert_eq!(dl, ds);
    assert_eq!(dl, dec!(5));
    assert_eq!(tier(dl, &t), RiskTier::Red, "derived pair A");

    // pair B — mark 105: long C=100 → liq 95; short C=200 → liq 115.
    let lb = pos(32, dec!(10), dec!(100), Some(dec!(105)), None, dec!(100));
    let sb = pos(32, dec!(-10), dec!(100), Some(dec!(105)), None, dec!(200));
    assert_eq!(implied_liq_price(&lb, &m), Some(dec!(95)));
    assert_eq!(implied_liq_price(&sb, &m), Some(dec!(115)));
    // v1.0.1 derived path (exchange absent): mirror holds and the value matches
    // the Python expectation. Python (command 4):
    // I| pairB long  m105 C100: liq = 95  | dist28 = 9.523809523809523809523809524
    // I| pairB short m105 C200: liq = 115 | dist28 = 9.523809523809523809523809524
    let (dl, ds) = (
        distance_to_liq_pct(&lb, &m).expect("derived long"),
        distance_to_liq_pct(&sb, &m).expect("derived short"),
    );
    assert_eq!(
        dl, ds,
        "derived mirror: |105-95|/105*100 == |105-115|/105*100"
    );
    assert_close(
        dl,
        d("9.523809523809523809523809524"),
        d("0.000000001"),
        "derived pair B",
    );
    let (lb, sb) = (
        Position {
            liq_price: Some(dec!(95)),
            ..lb
        },
        Position {
            liq_price: Some(dec!(115)),
            ..sb
        },
    );
    let (dl, ds) = (
        distance_to_liq_pct(&lb, &m).expect("long"),
        distance_to_liq_pct(&sb, &m).expect("short"),
    );
    assert_eq!(dl, ds, "mirror: |105-95|/105*100 == |105-115|/105*100");
    assert_close(
        dl,
        d("9.523809523809523809523809524"),
        d("0.00000000000000000001"),
        "pair B",
    );
    assert_eq!(tier(dl, &t), tier(ds, &t));
    assert_eq!(tier(dl, &t), RiskTier::Orange);

    // pair C — mark 95: long C=200 → liq 85; short C=100 → liq 105.
    let lc = pos(32, dec!(10), dec!(100), Some(dec!(95)), None, dec!(200));
    let sc = pos(32, dec!(-10), dec!(100), Some(dec!(95)), None, dec!(100));
    assert_eq!(implied_liq_price(&lc, &m), Some(dec!(85)));
    assert_eq!(implied_liq_price(&sc, &m), Some(dec!(105)));
    // v1.0.1 derived path (exchange absent): mirror holds and the value matches
    // the Python expectation. Python (command 4):
    // I| pairC long  m95 C200: liq = 85  | dist28 = 10.52631578947368421052631579
    // I| pairC short m95 C100: liq = 105 | dist28 = 10.52631578947368421052631579
    let (dl, ds) = (
        distance_to_liq_pct(&lc, &m).expect("derived long"),
        distance_to_liq_pct(&sc, &m).expect("derived short"),
    );
    assert_eq!(dl, ds, "derived mirror: |95-85|/95*100 == |95-105|/95*100");
    assert_close(
        dl,
        d("10.52631578947368421052631579"),
        d("0.000000001"),
        "derived pair C",
    );
    let (lc, sc) = (
        Position {
            liq_price: Some(dec!(85)),
            ..lc
        },
        Position {
            liq_price: Some(dec!(105)),
            ..sc
        },
    );
    let (dl, ds) = (
        distance_to_liq_pct(&lc, &m).expect("long"),
        distance_to_liq_pct(&sc, &m).expect("short"),
    );
    assert_eq!(dl, ds, "mirror: |95-85|/95*100 == |95-105|/95*100");
    assert_close(
        dl,
        d("10.52631578947368421052631579"),
        d("0.00000000000000000001"),
        "pair C",
    );
    assert_eq!(tier(dl, &t), tier(ds, &t));
    assert_eq!(tier(dl, &t), RiskTier::Orange);
}

#[test]
fn negative_size_is_a_short_not_a_none() {
    // SPEC §3.1: side = -1 for size < 0 — a short carries notional and MUST be
    // protected. Only size == 0 yields None (SPEC §3.4). (If negative sizes mapped
    // to None, every short would be left unprotected — a critical finding.)
    let m = market();
    let s = sdk_short();
    assert_eq!(implied_liq_price(&s, &m), Some(dec!(105)));
    assert_eq!(margin_health(&s, &m), Some(dec!(2)));
    // v1.0.1 derived path: exchange absent, market supplies the mmr — derived
    // liq_short 105, |100 - 105| / 100 * 100 = 5.
    // Python (command 4): I| sdk_short E100 S-10 C100 m100: liq = 105 | dist28 = 5
    assert_eq!(distance_to_liq_pct(&s, &m), Some(dec!(5)));
    let filled = Position {
        liq_price: Some(dec!(105)),
        ..s
    };
    assert_eq!(distance_to_liq_pct(&filled, &m), Some(dec!(5)));
    assert_eq!(
        reduce_fraction(&reflex_intent(
            &filled,
            RiskTier::Red,
            DataQuality::Fresh,
            &cfg_custom()
        )),
        Some(dec!(0.35))
    );
}

#[test]
fn zero_size_yields_none_everywhere() {
    // SPEC §3.4 edge case: size == 0 ⇒ None everywhere (no notional to protect).
    let m = market();
    let flat = pos(
        32,
        Decimal::ZERO,
        dec!(100),
        Some(dec!(100)),
        None,
        dec!(100),
    );
    assert_eq!(
        implied_liq_price(&flat, &m),
        None,
        "|size| = 0 ⇒ no derivation"
    );
    assert_eq!(
        effective_liq_price(&flat, &m),
        None,
        "no exchange value + no derivation"
    );
    assert_eq!(distance_to_liq_pct(&flat, &m), None);
    // v1.0.1: a flat position stays None even with a market and an exchange
    // price available — the no-notional rule beats derivation (§3.4).
    let flat_with_exchange = Position {
        liq_price: Some(dec!(95)),
        ..flat.clone()
    };
    assert_eq!(distance_to_liq_pct(&flat_with_exchange, &m), None);
    assert_eq!(liq_divergence_pct(&flat, &m), None);
    assert_eq!(margin_health(&flat, &m), None, "notional is zero");
    assert_eq!(
        reflex_intent(&flat, RiskTier::Red, DataQuality::Fresh, &cfg_custom()),
        None
    );

    // advance: None, and no bookkeeping is recorded (SPEC §3.4 step 6 records only Some).
    let cfg = cfg_custom();
    let mut st = ReflexState::new();
    assert_eq!(
        st.advance(&flat, RiskTier::Red, DataQuality::Fresh, &cfg, 0),
        None
    );
    assert_eq!(
        reduce_fraction(&st.advance(&sdk_long(), RiskTier::Red, DataQuality::Fresh, &cfg, 1)),
        Some(dec!(0.35)),
        "the size-0 call must not have armed the cooldown"
    );
}

#[test]
fn huge_collateral_is_green_and_never_acts() {
    // C = 1e9 ⇒ MMR = 50, C/|size| = 1e8, derived liq = 105 - 1e8 = -99 999 895.
    // SPEC §3.1's formula has no zero clamp (the SDK source does clamp — accepted
    // here as an alternative faithful reading; in both cases distance >= 100 ⇒ Green).
    // Python: H| liq = -99999895 | dist = 99999995 | SDK-clamp variant dist = 100
    let huge = pos(
        32,
        dec!(10),
        dec!(100),
        Some(dec!(100)),
        None,
        d("1000000000"),
    );
    let m = market();
    let liq = implied_liq_price(&huge, &m).expect("computable");
    eprintln!("probe: huge-collateral derived liq = {liq} (SPEC value -99999895; SDK clamp 0)");
    assert!(
        liq == dec!(-99999895) || liq == Decimal::ZERO,
        "expected SPEC formula value or SDK clamp, got {liq}"
    );
    // v1.0.1 derived path (exchange absent), computed before the exchange value
    // is filled in. Python (command 4): I| huge C=1e9 m100: dist28 = 99999995
    let derived = distance_to_liq_pct(&huge, &m).expect("derived distance computable");
    let filled = Position {
        liq_price: Some(liq),
        ..huge
    };
    let d = distance_to_liq_pct(&filled, &m).expect("distance computable");
    assert!(
        d == dec!(99999995) || d == dec!(100),
        "expected 99_999_995 or clamp 100, got {d}"
    );
    // the derived path (computed above) must agree with the exchange-filled path.
    assert_eq!(derived, d);
    assert_eq!(tier(d, &th()), RiskTier::Green);

    // Green ⇒ no intent, in both the stateless and the stateful path (SPEC §3.4).
    assert_eq!(
        reflex_intent(&filled, RiskTier::Green, DataQuality::Fresh, &cfg_custom()),
        None
    );
    assert_eq!(
        reflex_intent(
            &filled,
            RiskTier::Green,
            DataQuality::Stale { secs: 300 },
            &cfg_stale_custom()
        ),
        None
    );
    let mut st = ReflexState::new();
    assert_eq!(
        st.advance(
            &filled,
            RiskTier::Green,
            DataQuality::Fresh,
            &cfg_custom(),
            42
        ),
        None
    );
}

// ── SPEC §7 corpus: effective price + divergence ────────────────────────────

#[test]
fn divergence_and_effective_source_precedence() {
    let m = market();

    // exchange == derived (SDK long liq 95): divergence exactly 0, target ≤ 0.5.
    // Python: C| exchange=100 is the exact-5 % case; for exchange=95 divergence is 0.
    let p_eq = Position {
        liq_price: Some(dec!(95)),
        ..sdk_long()
    };
    let div = liq_divergence_pct(&p_eq, &m).expect("both prices available");
    assert_eq!(div, dec!(0));
    assert!(div <= d("0.5"), "agreement target");
    assert_eq!(
        effective_liq_price(&p_eq, &m),
        Some(LiqPrice {
            price: dec!(95),
            source: LiqSource::Exchange
        })
    );

    // exchange == derived*… "5 % away": exchange 100 → |95-100|/100*100 = 5 exactly.
    // Python: C| exchange=100: divergence28=5 | dist(mark=100)=0
    let p_5 = Position {
        liq_price: Some(dec!(100)),
        ..sdk_long()
    };
    let div = liq_divergence_pct(&p_5, &m).expect("both prices available");
    assert_eq!(div, dec!(5), "divergence ≈ 5 %");
    assert!(
        div > dec!(2),
        "> 2 % ⇒ trust the exchange price (SPEC §3.2 rule of use)"
    );
    let e = effective_liq_price(&p_5, &m).expect("exchange value present");
    assert_eq!(e.source, LiqSource::Exchange);
    assert_eq!(e.price, dec!(100));
    // distance must use the exchange value: |100-100|/100*100 = 0, not the derived 5.
    assert_eq!(distance_to_liq_pct(&p_5, &m), Some(dec!(0)));

    // exchange 99.75 (5 % above the derived price 95): divergence 4.7619… %.
    // Python: C| exchange=99.75: divergence28=4.761904761904761904761904762 | dist=0.25
    let p_off = Position {
        liq_price: Some(d("99.75")),
        ..sdk_long()
    };
    let div = liq_divergence_pct(&p_off, &m).expect("both prices available");
    assert_close(
        div,
        d("4.761904761904761904761904762"),
        d("0.00000000000000000001"),
        "divergence @99.75",
    );
    assert!(div > dec!(4) && div < dec!(6), "≈ 5 % window, got {div}");
    assert_eq!(
        effective_liq_price(&p_off, &m).map(|e| e.source),
        Some(LiqSource::Exchange)
    );
    assert_eq!(distance_to_liq_pct(&p_off, &m), Some(d("0.25")));

    // exchange 90.25 (5 % below derived): divergence 5.2631… %; distance 9.75.
    // Python: C| exchange=90.25: divergence28=5.263157894736842105263157895 | dist=9.75
    let p_off2 = Position {
        liq_price: Some(d("90.25")),
        ..sdk_long()
    };
    let div = liq_divergence_pct(&p_off2, &m).expect("both prices available");
    assert_close(
        div,
        d("5.263157894736842105263157895"),
        d("0.00000000000000000001"),
        "divergence @90.25",
    );
    assert_eq!(distance_to_liq_pct(&p_off2, &m), Some(d("9.75")));

    // derived fallback when the exchange value is absent.
    assert_eq!(
        effective_liq_price(&sdk_long(), &m),
        Some(LiqPrice {
            price: dec!(95),
            source: LiqSource::Derived
        })
    );
    assert_eq!(
        liq_divergence_pct(&sdk_long(), &m),
        None,
        "needs both prices"
    );

    // divergence needs the two prices only — a missing mark does not matter (SPEC §3.2),
    // while distance does need the mark (SPEC §3.1).
    let no_mark = Position {
        mark_price: None,
        liq_price: Some(dec!(95)),
        ..sdk_long()
    };
    assert_eq!(liq_divergence_pct(&no_mark, &m), Some(dec!(0)));
    assert_eq!(
        distance_to_liq_pct(&no_mark, &m),
        None,
        "mark missing ⇒ None"
    );
}

// ── SPEC §7 corpus: margin health ───────────────────────────────────────────

#[test]
fn margin_health_values_and_none_rules() {
    let m = market();

    // SDK vector: (100 + 0) / (0.05 * 10 * 100) = 2.
    assert_eq!(margin_health(&sdk_long(), &m), Some(dec!(2)));

    // mark 110 long: (100 + 100) / 55 = 3.6363…
    // Python: D| mark=110 long @28sig = 3.636363636363636363636363636
    let m110 = Position {
        mark_price: Some(dec!(110)),
        ..sdk_long()
    };
    assert_close(
        margin_health(&m110, &m).expect("computable"),
        d("3.636363636363636363636363636"),
        d("0.00000000000000000001"),
        "health @110 long",
    );

    // mark 110 short: (100 - 100) / 55 = 0.
    let m110s = Position {
        mark_price: Some(dec!(110)),
        ..sdk_short()
    };
    assert_eq!(margin_health(&m110s, &m), Some(Decimal::ZERO));

    // 1.0 = exactly at the (mark-based) maintenance requirement: C=50, mark=100 → 50/50.
    let unit = pos(32, dec!(10), dec!(100), Some(dec!(100)), None, dec!(50));
    assert_eq!(margin_health(&unit, &m), Some(dec!(1)));

    // At the liquidation mark the health is NOT 1.0: the requirement is mark-based
    // while the SDK/SPEC liq uses the entry-based requirement: 50/47.5 = 20/19.
    // Python: D| mark=95 long @28sig = 1.052631578947368421052631579
    let at_liq = Position {
        mark_price: Some(dec!(95)),
        ..sdk_long()
    };
    assert_close(
        margin_health(&at_liq, &m).expect("computable"),
        d("1.052631578947368421052631579"),
        d("0.00000000000000000001"),
        "health @liq",
    );

    // None rules: mark missing, notional zero (mark 0), notional zero (size 0).
    let no_mark = Position {
        mark_price: None,
        ..sdk_long()
    };
    assert_eq!(margin_health(&no_mark, &m), None);
    let zero_mark = Position {
        mark_price: Some(Decimal::ZERO),
        ..sdk_long()
    };
    assert_eq!(margin_health(&zero_mark, &m), None);
    let zero_size = pos(
        32,
        Decimal::ZERO,
        dec!(100),
        Some(dec!(100)),
        None,
        dec!(100),
    );
    assert_eq!(margin_health(&zero_size, &m), None);
}

// ── SPEC §7 corpus: stateless intent table (full grid) ──────────────────────

#[test]
fn reflex_intent_stateless_table_grid() {
    // SPEC-P04 §3.4 table, swept over tier × quality × stale_reduce with a custom
    // config (0.35 / 0.15) to prove the fraction fields are read, not hardcoded.
    let p = sdk_long();
    let qualities = [
        DataQuality::Fresh,
        DataQuality::Stale { secs: 120 },
        DataQuality::Missing,
    ];
    let tiers = [
        RiskTier::Green,
        RiskTier::Yellow,
        RiskTier::Orange,
        RiskTier::Red,
    ];

    for cfg in [cfg_custom(), cfg_stale_custom()] {
        for t in tiers {
            for q in qualities {
                let got = reflex_intent(&p, t, q, &cfg);
                let expected = match (t, q) {
                    (RiskTier::Green | RiskTier::Yellow, _) => Kind::Nil,
                    (RiskTier::Red, DataQuality::Fresh) => Kind::Reduce(cfg.reduce_fraction),
                    (RiskTier::Orange, DataQuality::Fresh) => Kind::Reduce(cfg.orange_fraction),
                    (
                        RiskTier::Red | RiskTier::Orange,
                        DataQuality::Stale { .. } | DataQuality::Missing,
                    ) => {
                        if cfg.stale_reduce {
                            Kind::Reduce(cfg.orange_fraction)
                        } else {
                            Kind::Alert
                        }
                    }
                };
                assert_eq!(
                    kind(&got),
                    expected,
                    "tier={t:?} quality={q:?} stale_reduce={}",
                    cfg.stale_reduce
                );
                assert!(
                    !matches!(got, Some(Intent::AddCollateral { .. })),
                    "reflex intents never increase exposure (SPEC §3.4)"
                );
                // every returned reason/message must be a human-readable string.
                match &got {
                    Some(Intent::Reduce { reason, .. })
                    | Some(Intent::Alert { message: reason }) => {
                        assert!(
                            !reason.is_empty(),
                            "empty reason for tier={t:?} quality={q:?}"
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // shorts are treated identically (SPEC §3.1 side = -1).
    let s = sdk_short();
    assert_eq!(
        reduce_fraction(&reflex_intent(
            &s,
            RiskTier::Red,
            DataQuality::Fresh,
            &cfg_custom()
        )),
        Some(dec!(0.35))
    );
    assert_eq!(
        reduce_fraction(&reflex_intent(
            &s,
            RiskTier::Orange,
            DataQuality::Fresh,
            &cfg_custom()
        )),
        Some(dec!(0.15))
    );

    // zero size ⇒ None on every rule that could otherwise fire.
    let flat = pos(
        32,
        Decimal::ZERO,
        dec!(100),
        Some(dec!(100)),
        None,
        dec!(100),
    );
    assert_eq!(
        reflex_intent(&flat, RiskTier::Red, DataQuality::Fresh, &cfg_custom()),
        None
    );
    assert_eq!(
        reflex_intent(&flat, RiskTier::Orange, DataQuality::Fresh, &cfg_custom()),
        None
    );
    assert_eq!(
        reflex_intent(
            &flat,
            RiskTier::Red,
            DataQuality::Stale { secs: 5 },
            &cfg_stale_custom()
        ),
        None
    );
}

// ── SPEC §7 corpus: state machine sequences ─────────────────────────────────

#[test]
fn reflex_state_red_escalation_and_cooldown() {
    // Red@t0 ⇒ Reduce{0.5}; t0+599_999 ⇒ None; t0+600_000 ⇒ Close (SPEC §3.4 steps 2-3).
    // Defaults: SPEC §3.4 — reduce_fraction 0.5, cooldown 600_000 ms.
    let cfg = ReflexConfig::default();
    assert_eq!(cfg.reduce_fraction, dec!(0.5));
    assert_eq!(cfg.orange_fraction, dec!(0.25));
    assert_eq!(cfg.cooldown_ms, 600_000);
    assert!(!cfg.stale_reduce);

    let p = sdk_long();
    let t0: u64 = 1_000_000;
    let mut st = ReflexState::new();

    match st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0) {
        Some(Intent::Reduce { fraction, reason }) => {
            assert_eq!(fraction, dec!(0.5), "first breach ⇒ reduce_fraction");
            assert!(!reason.is_empty());
        }
        other => panic!("Red + Fresh @t0 must Reduce{{0.5}}, got {other:?}"),
    }
    assert_eq!(
        st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + 599_999),
        None,
        "cooldown not yet elapsed (equality counts as elapsed)"
    );
    match st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + 600_000) {
        Some(Intent::Close { reason }) => assert!(!reason.is_empty()),
        other => panic!("Red still set after cooldown must Close, got {other:?}"),
    }
}

#[test]
fn reflex_state_recovery_reset() {
    // Recovery (tier <= Yellow) clears the market bookkeeping: the next Red starts
    // with a fresh Reduce (no cooldown wait, no Close) — SPEC §3.4 step 1.
    let cfg = cfg_custom();
    let p = sdk_long();
    let t0: u64 = 10_000;
    let mut st = ReflexState::new();

    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0)),
        Some(dec!(0.35))
    );
    assert_eq!(
        st.advance(&p, RiskTier::Yellow, DataQuality::Fresh, &cfg, t0 + 1),
        None,
        "Yellow recovery returns None and resets"
    );
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + 2)),
        Some(dec!(0.35)),
        "after reset a new breach reduces again (not Close, not cooldown-gated)"
    );
    assert_eq!(
        st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + 3),
        None,
        "the new breach arms a fresh cooldown"
    );
    assert_eq!(
        st.advance(&p, RiskTier::Green, DataQuality::Missing, &cfg, t0 + 4),
        None,
        "Green + Missing also resets (row: Green/Yellow any quality ⇒ None)"
    );
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, t0 + 5)),
        Some(dec!(0.35))
    );
}

#[test]
fn reflex_state_orange_cadence() {
    // Orange + Fresh ⇒ Reduce{orange_fraction} once per cooldown; no escalation
    // (SPEC §3.4 step 4 — only Red escalates to Close).
    let cfg = ReflexConfig::default();
    let p = sdk_long();
    let mut st = ReflexState::new();

    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Orange, DataQuality::Fresh, &cfg, 0)),
        Some(dec!(0.25))
    );
    assert_eq!(
        st.advance(&p, RiskTier::Orange, DataQuality::Fresh, &cfg, 599_999),
        None
    );
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Orange, DataQuality::Fresh, &cfg, 600_000)),
        Some(dec!(0.25)),
        "Orange repeats after each cooldown, never escalates"
    );
    assert_eq!(
        st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, 600_001),
        None,
        "the Orange action armed the cooldown"
    );
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg, 1_200_000)),
        Some(dec!(0.5)),
        "a later first RED breach still starts with Reduce (red_reduced was never set)"
    );
}

#[test]
fn reflex_state_stale_gate_both_branches() {
    // SPEC §3.4 step 5: stale_reduce=false ⇒ Alert (rate-limited like any Some);
    // stale_reduce=true ⇒ gated Reduce{orange_fraction}; staleness never escalates.
    let p = sdk_long();
    let stale = DataQuality::Stale { secs: 120 };

    // false branch — default config.
    let cfg_off = ReflexConfig::default();
    let mut st = ReflexState::new();
    match st.advance(&p, RiskTier::Red, stale, &cfg_off, 0) {
        Some(Intent::Alert { message }) => assert!(!message.is_empty()),
        other => panic!("stale Red with stale_reduce=false must Alert, got {other:?}"),
    }
    assert_eq!(
        st.advance(&p, RiskTier::Red, stale, &cfg_off, 1),
        None,
        "Alert counts as an action and arms the cooldown (step 6: any Some)"
    );
    match st.advance(&p, RiskTier::Red, stale, &cfg_off, 600_000) {
        Some(Intent::Alert { .. }) => {}
        other => panic!("stale Red repeats as Alert after cooldown, got {other:?}"),
    }

    // true branch — stale_reduce=true keeps the default orange_fraction 0.25.
    let cfg_on = ReflexConfig {
        stale_reduce: true,
        ..ReflexConfig::default()
    };
    let mut st = ReflexState::new();
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, stale, &cfg_on, 0)),
        Some(dec!(0.25)),
        "gated stale reduce uses orange_fraction, not reduce_fraction"
    );
    assert_eq!(st.advance(&p, RiskTier::Red, stale, &cfg_on, 1), None);
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Orange, DataQuality::Missing, &cfg_on, 600_000)),
        Some(dec!(0.25))
    );
    assert_eq!(
        st.advance(&p, RiskTier::Green, stale, &cfg_on, 600_001),
        None,
        "Green resets regardless of staleness"
    );
}

#[test]
fn reflex_state_zero_cooldown_edge() {
    // cooldown_ms = 0: equality counts as elapsed, so a repeat within the same
    // instant proceeds (Red escalates to Close; Orange reduces again) — SPEC §3.4 step 2.
    let cfg0 = ReflexConfig {
        cooldown_ms: 0,
        ..ReflexConfig::default()
    };
    assert_eq!(
        cfg0.validate(),
        Ok(()),
        "cooldown 0 is a valid config (no range promised)"
    );
    let p = sdk_long();
    let mut st = ReflexState::new();

    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg0, 7)),
        Some(dec!(0.5))
    );
    match st.advance(&p, RiskTier::Red, DataQuality::Fresh, &cfg0, 7) {
        Some(Intent::Close { .. }) => {}
        other => panic!("cooldown 0 ⇒ same-instant repeat escalates to Close, got {other:?}"),
    }
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Orange, DataQuality::Fresh, &cfg0, 7)),
        Some(dec!(0.25))
    );
    assert_eq!(
        reduce_fraction(&st.advance(&p, RiskTier::Orange, DataQuality::Fresh, &cfg0, 7)),
        Some(dec!(0.25))
    );
}

#[test]
fn reflex_state_per_market_isolation() {
    // Bookkeeping is per market (SPEC §3.4: "same market", state keyed by MarketId):
    // market B's first breach must not be silenced by market A's cooldown.
    let cfg = ReflexConfig::default();
    let a = sdk_long(); // market 32
    let b = pos(16, dec!(10), dec!(100), Some(dec!(100)), None, dec!(100)); // market 16
    let mut st = ReflexState::new();

    assert_eq!(
        reduce_fraction(&st.advance(&a, RiskTier::Red, DataQuality::Fresh, &cfg, 0)),
        Some(dec!(0.5))
    );
    assert_eq!(
        reduce_fraction(&st.advance(&b, RiskTier::Red, DataQuality::Fresh, &cfg, 0)),
        Some(dec!(0.5)),
        "market B is not gated by market A's cooldown"
    );
    assert_eq!(
        st.advance(&a, RiskTier::Red, DataQuality::Fresh, &cfg, 1),
        None
    );
}

// ── SPEC §7 corpus: config validation ───────────────────────────────────────

#[test]
fn config_validation_rules() {
    // ReflexConfig: fractions in (0, 1]; 1.0 valid, 0 and > 1 invalid (SPEC §3.4 + doc).
    assert_eq!(
        ReflexConfig::default(),
        ReflexConfig {
            reduce_fraction: dec!(0.5),
            orange_fraction: dec!(0.25),
            cooldown_ms: 600_000,
            stale_reduce: false
        }
    );

    let ok = ReflexConfig {
        reduce_fraction: dec!(1),
        orange_fraction: dec!(1),
        cooldown_ms: 0,
        stale_reduce: false,
    };
    assert_eq!(ok.validate(), Ok(()), "fraction 1.0 is inside (0, 1]");

    let tiny = Decimal::new(1, 28); // smallest positive scale-28 fraction
    let tiny_ok = ReflexConfig {
        reduce_fraction: tiny,
        orange_fraction: tiny,
        cooldown_ms: u64::MAX,
        stale_reduce: true,
    };
    assert_eq!(
        tiny_ok.validate(),
        Ok(()),
        "smallest positive fraction is inside (0, 1]"
    );

    assert!(
        ReflexConfig {
            reduce_fraction: Decimal::ZERO,
            ..ok
        }
        .validate()
        .is_err(),
        "0 invalid"
    );
    assert!(
        ReflexConfig {
            orange_fraction: Decimal::ZERO,
            ..ok
        }
        .validate()
        .is_err(),
        "0 invalid"
    );
    assert!(
        ReflexConfig {
            reduce_fraction: dec!(1.5),
            ..ok
        }
        .validate()
        .is_err(),
        "> 1 invalid"
    );
    assert!(
        ReflexConfig {
            orange_fraction: dec!(1.0000000000000000000000000001),
            ..ok
        }
        .validate()
        .is_err(),
        "just above 1 invalid"
    );
    assert!(
        ReflexConfig {
            reduce_fraction: dec!(-0.1),
            ..ok
        }
        .validate()
        .is_err(),
        "negative invalid"
    );
}

#[test]
fn thresholds_validation_edges() {
    // RiskThresholds: ordering invariant hard < warn < soft; equality violates it.
    assert_eq!(th().validate(), Ok(()));

    let bad = [
        RiskThresholds {
            soft: dec!(25),
            warn: dec!(25),
            hard: dec!(8),
        },
        RiskThresholds {
            soft: dec!(25),
            warn: dec!(15),
            hard: dec!(15),
        },
        RiskThresholds {
            soft: dec!(8),
            warn: dec!(15),
            hard: dec!(25),
        },
        RiskThresholds {
            soft: dec!(10),
            warn: dec!(10),
            hard: dec!(10),
        },
        RiskThresholds {
            soft: dec!(0),
            warn: dec!(0),
            hard: dec!(0),
        },
        RiskThresholds {
            soft: dec!(-1),
            warn: dec!(0),
            hard: dec!(1),
        },
        RiskThresholds {
            soft: dec!(8),
            warn: dec!(7),
            hard: dec!(9),
        },
    ];
    for t in bad {
        assert!(t.validate().is_err(), "{t:?} violates hard < warn < soft");
    }

    // extreme but correctly ordered values must not panic (no range promise beyond order).
    let extreme = RiskThresholds {
        soft: Decimal::MAX,
        warn: dec!(0.0000000000000000000000000001),
        hard: Decimal::MIN,
    };
    let _ = extreme.validate();
}

// ── adversarial: degenerate inputs must not panic ───────────────────────────

#[test]
fn adversarial_nonpositive_entry_probe() {
    // SPEC-P04 §3.1 gives no None condition for entry_price <= 0. This probe pins
    // determinism and records the behavior (run with --nocapture to see both lines);
    // any panic or per-call difference is a finding, a None is spec-unspecified.
    let m = market();
    for entry in [Decimal::ZERO, dec!(-50)] {
        let p = pos(32, dec!(10), entry, Some(dec!(100)), None, dec!(100));
        let first = implied_liq_price(&p, &m);
        let second = implied_liq_price(&p, &m);
        assert_eq!(first, second, "deterministic for entry={entry}");
        eprintln!("probe: entry={entry} ⇒ implied_liq_price={first:?}");
        // distance path must also stay total:
        let filled = Position {
            liq_price: first,
            ..p
        };
        let _ = distance_to_liq_pct(&filled, &m);
        eprintln!(
            "probe: entry={entry} ⇒ distance={:?}",
            distance_to_liq_pct(&filled, &m)
        );
    }
}

#[test]
fn adversarial_mark_nonpositive() {
    // SPEC §3.1: distance None whenever mark <= 0; margin_health None at zero notional.
    let m = market();
    let zero_mark = pos(
        32,
        dec!(10),
        dec!(100),
        Some(Decimal::ZERO),
        Some(dec!(95)),
        dec!(100),
    );
    assert_eq!(
        distance_to_liq_pct(&zero_mark, &m),
        None,
        "mark == 0 ⇒ None"
    );
    assert_eq!(margin_health(&zero_mark, &m), None, "notional zero ⇒ None");

    let neg_mark = pos(
        32,
        dec!(10),
        dec!(100),
        Some(dec!(-5)),
        Some(dec!(95)),
        dec!(100),
    );
    assert_eq!(distance_to_liq_pct(&neg_mark, &m), None, "mark < 0 ⇒ None");
}

#[test]
fn adversarial_negative_exchange_liq_price() {
    // A negative exchange liq price is used as-is (no clamp promised for exchange
    // values): distance = |100 - (-50)| / 100 * 100 = 150 ⇒ Green ⇒ no intent.
    let m = market();
    let p = Position {
        liq_price: Some(dec!(-50)),
        ..sdk_long()
    };
    let e = effective_liq_price(&p, &m).expect("exchange value present");
    assert_eq!(
        e,
        LiqPrice {
            price: dec!(-50),
            source: LiqSource::Exchange
        }
    );
    // exchange preferred even when negative: |100 - (-50)| / 100 * 100 = 150,
    // not the derived 5 (precedence proof).
    // Python (command 4): I| exchange -50 m100: dist28 = 150
    let d = distance_to_liq_pct(&p, &m).expect("computable");
    assert_eq!(d, dec!(150));
    let derived_only = Position {
        liq_price: None,
        ..p.clone()
    };
    assert_eq!(
        distance_to_liq_pct(&derived_only, &m),
        Some(dec!(5)),
        "derivation alone would use liq 95 (command 4 sdk_long row)"
    );
    assert_eq!(tier(d, &th()), RiskTier::Green);
    assert_eq!(
        reflex_intent(&p, RiskTier::Green, DataQuality::Fresh, &cfg_custom()),
        None
    );
}

#[test]
fn adversarial_derived_distance_when_exchange_absent() {
    // v1.0.1 (§11) gap closure: exchange absent + market present + mark present
    // ⇒ the distance comes from the derived liquidation price, not None.
    // Python (command 4):
    // I| pairB long  m105 C100: liq = 95  | dist28 = 9.523809523809523809523809524
    // I| pairB short m105 C200: liq = 115 | dist28 = 9.523809523809523809523809524
    let m = market();
    let lb = pos(32, dec!(10), dec!(100), Some(dec!(105)), None, dec!(100));
    assert_eq!(lb.liq_price, None, "exchange value absent by construction");
    let dl = distance_to_liq_pct(&lb, &m).expect("derived distance computable");
    assert_close(
        dl,
        d("9.523809523809523809523809524"),
        d("0.000000001"),
        "derived pair-B long",
    );
    assert_eq!(tier(dl, &th()), RiskTier::Orange);

    let sb = pos(32, dec!(-10), dec!(100), Some(dec!(105)), None, dec!(200));
    assert_eq!(sb.liq_price, None);
    let ds = distance_to_liq_pct(&sb, &m).expect("derived short distance computable");
    assert_close(
        ds,
        d("9.523809523809523809523809524"),
        d("0.000000001"),
        "derived pair-B short",
    );
    assert_eq!(tier(ds, &th()), RiskTier::Orange);
    assert_eq!(dl, ds, "derived mirror");
}

#[test]
fn adversarial_derived_missing_inputs_are_none() {
    // v1.0.1: exchange absent + market present + mark absent ⇒ None; the
    // non-positive-mark rule also beats any derivation (§3.1).
    let m = market();
    let base = pos(32, dec!(10), dec!(100), Some(dec!(105)), None, dec!(100));

    let no_mark = Position {
        mark_price: None,
        ..base.clone()
    };
    assert_eq!(
        distance_to_liq_pct(&no_mark, &m),
        None,
        "mark absent ⇒ None even with market + derivable liq"
    );

    let zero_mark = Position {
        mark_price: Some(Decimal::ZERO),
        ..base.clone()
    };
    assert_eq!(
        distance_to_liq_pct(&zero_mark, &m),
        None,
        "mark == 0 ⇒ None"
    );

    let neg_mark = Position {
        mark_price: Some(dec!(-1)),
        ..base
    };
    assert_eq!(distance_to_liq_pct(&neg_mark, &m), None, "mark < 0 ⇒ None");
}

#[test]
fn adversarial_28_scale_precision() {
    // Scale-25 entry and scale-28 collateral; every intermediate fits rust_decimal's
    // 96-bit mantissa (Python command 3: 84-94 bits, scale <= 28 ⇒ all ops exact).
    // entry 1 + 1e-25, |size| 1, mmr 0.05, collateral 1e-28, mark 1.25.
    // Python: F| liq = 1.050000000000000000000000005 | dist = 15.9999999999999999999999996
    let m = market();
    let p = pos(
        32,
        dec!(1),
        d("1.0000000000000000000000001"),
        Some(d("1.25")),
        None,
        d("0.0000000000000000000000001"),
    );
    let liq = implied_liq_price(&p, &m).expect("computable");
    assert_eq!(liq, d("1.050000000000000000000000005"));
    // v1.0.1 derived path (exchange absent): equals the exchange-filled path below.
    // Python: F| dist = 15.9999999999999999999999996
    let derived_dist = distance_to_liq_pct(&p, &m).expect("derived computable");
    let filled = Position {
        liq_price: Some(liq),
        ..p
    };
    let dist = distance_to_liq_pct(&filled, &m).expect("computable");
    assert_eq!(derived_dist, dist, "derived and exchange paths agree");
    assert_close(
        dist,
        d("15.9999999999999999999999996"),
        d("0.000000000000000000000001"),
        "scale-28 distance",
    );
    assert_eq!(tier(dist, &th()), RiskTier::Yellow);
}

#[test]
fn adversarial_very_large_magnitude() {
    // ~1e20 intermediates (entry 1e19, collateral 1e20): exact result, no overflow.
    // Python: E| liq = 5E+17 | dist = 95
    let m = market();
    let p = pos(
        32,
        dec!(10),
        d("10000000000000000000"),
        Some(d("10000000000000000000")),
        None,
        d("100000000000000000000"),
    );
    let liq = implied_liq_price(&p, &m).expect("computable");
    assert_eq!(liq, dec!(500000000000000000));
    // v1.0.1 derived path (exchange absent). Python: E| liq = 5E+17 | dist = 95
    assert_eq!(distance_to_liq_pct(&p, &m), Some(dec!(95)));
    let filled = Position {
        liq_price: Some(liq),
        ..p
    };
    assert_eq!(distance_to_liq_pct(&filled, &m), Some(dec!(95)));
    assert_eq!(tier(dec!(95), &th()), RiskTier::Green);
    assert_eq!(
        reflex_intent(&filled, RiskTier::Green, DataQuality::Fresh, &cfg_custom()),
        None
    );
}
