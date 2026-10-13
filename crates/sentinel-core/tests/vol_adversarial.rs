//! Adversarial + property suite for the volatility-adaptive layer (P22).
//!
//! The acceptance criteria this file defends:
//!
//! 1. Warmup keeps the operator's static bands (never a zero-width band).
//! 2. At reference volatility the adaptive band reproduces the configured
//!    base band exactly (the calibration property).
//! 3. Stress and Dislocation may only *tighten* protection: `hard` is never
//!    smaller than the static band in a violent regime.
//! 4. Calm may loosen, but never below the floor.
//! 5. Every returned band set satisfies `hard < warn < soft` and the clamps,
//!    for the whole sigma range (property sweep).
//! 6. `time_to_liq_ms` is monotone in both arguments and rejects degenerate
//!    inputs.

use proptest::prelude::*;
use rust_decimal::Decimal;
use sentinel_core::risk::RiskThresholds;
use sentinel_core::types::MarketId;
use sentinel_core::vol::{Regime, VolConfig, VolState, adaptive_bands, time_to_liq_ms};

const ETH: MarketId = MarketId(32);

/// Build a state and feed it `marks` at a 60 s cadence from a fixed anchor.
fn fed(cfg: &VolConfig, marks: &[&str]) -> VolState {
    let mut state = VolState::new(ETH);
    for (i, m) in marks.iter().enumerate() {
        let mark: Decimal = m.parse().expect("mark");
        state.observe(1_791_854_400_000 + (i as u64) * cfg.step_ms, mark, cfg);
    }
    state
}

/// A tape with the measured real-tape sigma: tiny per-step moves around 0.0775%.
fn reference_tape(cfg: &VolConfig, n: usize) -> Vec<String> {
    let sigma_step = cfg.reference_sigma_step;
    let mut price = Decimal::new(247451, 2); // real ETH testnet close
    let mut out = vec![price.to_string()];
    for i in 0..n {
        // Alternate +/- sigma so the EWMA variance lands on sigma^2.
        let sign = if i % 2 == 0 {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        price *= Decimal::ONE + sign * sigma_step;
        out.push(price.to_string());
    }
    out
}

fn static_hard() -> Decimal {
    Decimal::new(8, 0)
}

#[test]
fn warmup_yields_none_and_caller_keeps_static_bands() {
    let cfg = VolConfig::default();
    let mut state = VolState::new(ETH);
    for i in 0..(cfg.min_samples - 1) {
        state.observe(1_000 + u64::from(i), Decimal::new(247451, 2), &cfg);
        assert!(state.sigma_step(&cfg).is_none(), "sigma before warmup");
        assert!(
            adaptive_bands(&state, &cfg, 1_000, static_hard()).is_none(),
            "bands before warmup"
        );
    }
}

#[test]
fn reference_volatility_reproduces_the_base_band() {
    let cfg = VolConfig::default();
    let tape = reference_tape(&cfg, 200);
    let marks: Vec<&str> = tape.iter().map(String::as_str).collect();
    let state = fed(&cfg, &marks);
    let bands =
        adaptive_bands(&state, &cfg, 2_000_000_000_000, static_hard()).expect("bands after warmup");
    // The alternation makes the EWMA variance land on sigma^2, so the scaled
    // band must reproduce the configured base within a small tolerance.
    let delta = (bands.hard - cfg.base_hard_pct).abs();
    assert!(
        delta < Decimal::new(2, 0),
        "hard {} should be near base {} at reference vol",
        bands.hard,
        cfg.base_hard_pct
    );
    assert_eq!(
        state.regime(&cfg, 2_000_000_000_000),
        Regime::Elevated,
        "reference vol is the elevated (baseline) regime"
    );
}

#[test]
fn flat_tape_clamps_to_the_floor() {
    let cfg = VolConfig::default();
    let mut state = VolState::new(ETH);
    for i in 0..100u64 {
        state.observe(1_000 + i * 60_000, Decimal::new(247451, 2), &cfg);
    }
    // Zero returns: sigma collapses. The floor is what keeps a band.
    assert_eq!(state.regime(&cfg, 2_000_000_000_000), Regime::Calm);
    let bands = adaptive_bands(&state, &cfg, 2_000_000_000_000, static_hard()).expect("bands");
    assert!(
        bands.hard >= cfg.floor_pct,
        "hard {} must respect the floor {}",
        bands.hard,
        cfg.floor_pct
    );
}

#[test]
fn stress_and_dislocation_can_only_tighten_never_loosen() {
    // The structural safety property of the whole feature: in a violent
    // regime the adaptive hard band is never smaller than the static one.
    let cfg = VolConfig::default();
    for multiplier in ["1.7", "2.0", "3.0", "5.0", "10.0"] {
        let step = cfg.reference_sigma_step * multiplier.parse::<Decimal>().unwrap();
        let mut state = VolState::new(ETH);
        let mut price = Decimal::new(247451, 2);
        for i in 0..120u64 {
            let sign = if i % 2 == 0 {
                Decimal::ONE
            } else {
                -Decimal::ONE
            };
            price *= Decimal::ONE + sign * step;
            state.observe(1_000 + i * 60_000, price, &cfg);
        }
        let now = 2_000_000_000_000;
        let regime = state.regime(&cfg, now);
        if matches!(regime, Regime::Stress | Regime::Dislocation) {
            let bands = adaptive_bands(&state, &cfg, now, static_hard()).expect("bands");
            assert!(
                bands.hard >= static_hard(),
                "regime {regime:?} at {multiplier}x vol produced hard {} < static {}",
                bands.hard,
                static_hard()
            );
        }
    }
}

#[test]
fn a_jump_pins_the_dislocation_regime_for_the_hold_window() {
    let cfg = VolConfig::default();
    let mut state = VolState::new(ETH);
    let mut price = Decimal::new(247451, 2);
    // Quiet tape to warm up.
    for i in 0..60u64 {
        let sign = if i % 2 == 0 {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        price *= Decimal::ONE + sign * cfg.reference_sigma_step;
        state.observe(1_000 + i * 60_000, price, &cfg);
    }
    let jump_ts = 1_000 + 60 * 60_000;
    price *= Decimal::new(85, 2); // -15% in one step: far beyond jump_k
    state.observe(jump_ts, price, &cfg);

    assert_eq!(
        state.regime(&cfg, jump_ts + 1),
        Regime::Dislocation,
        "jump must pin dislocation"
    );
    assert_eq!(
        state.regime(&cfg, jump_ts + cfg.jump_hold_ms),
        Regime::Dislocation,
        "the hold window itself is pinned"
    );

    // Dislocation here is doubly sourced: the jump hold AND the elevated
    // EWMA variance the jump left behind. To prove the hold expires, let the
    // volatility actually decay back: feed a long quiet tape past the window
    // and the classifier must leave Dislocation.
    let mut quiet_ts = jump_ts + cfg.jump_hold_ms + 1;
    for _ in 0..400u64 {
        let sign = if (quiet_ts / 60_000) % 2 == 0 {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        price *= Decimal::ONE + sign * cfg.reference_sigma_step;
        state.observe(quiet_ts, price, &cfg);
        quiet_ts += 60_000;
    }
    assert_ne!(
        state.regime(&cfg, quiet_ts),
        Regime::Dislocation,
        "once the hold window passes and volatility decays, dislocation must clear"
    );
}

#[test]
fn degenerate_observations_do_not_corrupt_state() {
    let cfg = VolConfig::default();
    let mut state = VolState::new(ETH);
    state.observe(1_000, Decimal::new(247451, 2), &cfg);
    let before = state.samples();
    // Zero / negative mark rejected.
    state.observe(2_000, Decimal::ZERO, &cfg);
    state.observe(3_000, Decimal::NEGATIVE_ONE, &cfg);
    // Out-of-order timestamp rejected.
    state.observe(500, Decimal::new(247500, 2), &cfg);
    assert_eq!(state.samples(), before, "bad observations must not count");
}

#[test]
fn time_to_liq_is_monotone_and_rejects_degenerate_inputs() {
    let sigma = Decimal::new(775, 6);
    let step = 60_000u64;

    let near = time_to_liq_ms(Decimal::new(4, 0), sigma, step).unwrap();
    let far = time_to_liq_ms(Decimal::new(16, 0), sigma, step).unwrap();
    assert!(far > near, "farther distance must take longer");

    let calm = time_to_liq_ms(Decimal::new(8, 0), sigma, step).unwrap();
    let wild = time_to_liq_ms(Decimal::new(8, 0), sigma * Decimal::new(4, 0), step).unwrap();
    assert!(wild < calm, "higher volatility must shorten the horizon");

    // Scaling property: distance doubled => 4x the time (t = (d/sigma)^2),
    // within the tolerance of finite-precision Decimal division.
    let ratio = far / near;
    assert!(
        (ratio - Decimal::new(16, 0)).abs() < Decimal::new(1, 9),
        "quadratic scaling: got {ratio}"
    );

    assert!(time_to_liq_ms(Decimal::ZERO, sigma, step).is_none());
    assert!(time_to_liq_ms(Decimal::new(8, 0), Decimal::ZERO, step).is_none());
    assert!(time_to_liq_ms(Decimal::new(8, 0), sigma, 0).is_none());
}

#[test]
fn config_validation_rejects_broken_settings() {
    let mut cfg = VolConfig::default();
    assert!(cfg.validate().is_ok());

    cfg = VolConfig::default();
    cfg.lambda = Decimal::ONE;
    assert_eq!(cfg.validate(), Err("lambda"));

    cfg = VolConfig::default();
    cfg.floor_pct = Decimal::new(50, 0);
    cfg.ceil_pct = Decimal::new(10, 0);
    assert_eq!(cfg.validate(), Err("floor_pct/ceil_pct"));

    cfg = VolConfig::default();
    cfg.soft_ratio = cfg.warn_ratio;
    assert_eq!(cfg.validate(), Err("warn_ratio/soft_ratio"));
}

proptest! {
    /// Every band set the layer can emit satisfies the clamps and the
    /// ordering invariant, across five orders of magnitude of volatility.
    #[test]
    fn bands_always_ordered_and_clamped(ratio_milli in 1u32..=20_000u32) {
        let cfg = VolConfig::default();
        let step = cfg.reference_sigma_step * Decimal::new(i64::from(ratio_milli), 3);
        let mut state = VolState::new(ETH);
        let mut price = Decimal::new(247451, 2);
        for i in 0..120u64 {
            let sign = if i % 2 == 0 { Decimal::ONE } else { -Decimal::ONE };
            price *= Decimal::ONE + sign * step;
            state.observe(1_000 + i * 60_000, price, &cfg);
        }
        let now = 2_000_000_000_000u64;
        if let Some(bands) = adaptive_bands(&state, &cfg, now, static_hard()) {
            prop_assert!(bands.hard > Decimal::ZERO, "hard must be positive: {}", bands.hard);
            prop_assert!(bands.hard < bands.warn, "hard {} < warn {}", bands.hard, bands.warn);
            prop_assert!(bands.warn < bands.soft, "warn {} < soft {}", bands.warn, bands.soft);
            prop_assert!(bands.hard >= cfg.floor_pct, "hard {} >= floor {}", bands.hard, cfg.floor_pct);
            prop_assert!(bands.soft <= cfg.ceil_pct, "soft {} <= ceil {}", bands.soft, cfg.ceil_pct);
            let validated: Result<(), &'static str> = bands.validate();
            prop_assert!(validated.is_ok());
        }
    }

    /// The safety property, swept: if the regime is violent, the adaptive
    /// hard band is never below the static one.
    #[test]
    fn violent_regimes_never_loosen_below_static(ratio_milli in 1u32..=20_000u32) {
        let cfg = VolConfig::default();
        let step = cfg.reference_sigma_step * Decimal::new(i64::from(ratio_milli), 3);
        let mut state = VolState::new(ETH);
        let mut price = Decimal::new(247451, 2);
        for i in 0..120u64 {
            let sign = if i % 2 == 0 { Decimal::ONE } else { -Decimal::ONE };
            price *= Decimal::ONE + sign * step;
            state.observe(1_000 + i * 60_000, price, &cfg);
        }
        let now = 2_000_000_000_000u64;
        let regime = state.regime(&cfg, now);
        if matches!(regime, Regime::Stress | Regime::Dislocation)
            && let Some(bands) = adaptive_bands(&state, &cfg, now, static_hard())
        {
            prop_assert!(
                bands.hard >= static_hard(),
                "regime {:?}: hard {} < static {}",
                regime,
                bands.hard,
                static_hard()
            );
        }
    }
}

#[test]
fn adaptive_calm_band_is_tighter_than_static_protection_is_earlier_only_when_violent() {
    // Documented behavior in one assertion: calm regimes act *later* than the
    // static bands (fewer false trims), violent regimes act *earlier* or the
    // same. This is the whole point of GAP-009.
    let cfg = VolConfig::default();
    let mut calm = VolState::new(ETH);
    let mut price = Decimal::new(247451, 2);
    for i in 0..120u64 {
        let sign = if i % 2 == 0 {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        price *= Decimal::ONE + sign * cfg.reference_sigma_step * Decimal::new(3, 1);
        calm.observe(1_000 + i * 60_000, price, &cfg);
    }
    let now = 2_000_000_000_000u64;
    let bands = adaptive_bands(&calm, &cfg, now, static_hard()).expect("bands");
    assert_eq!(calm.regime(&cfg, now), Regime::Calm);
    assert!(
        bands.hard < static_hard(),
        "calm band {} should be tighter than static {}",
        bands.hard,
        static_hard()
    );
    assert!(bands.hard >= cfg.floor_pct);

    // And the ordering survives the down-scaling.
    let validated: RiskThresholds = bands;
    assert!(validated.validate().is_ok());
}
