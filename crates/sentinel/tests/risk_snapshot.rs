//! P04 acceptance (app-side, parent): classify a **full account snapshot**
//! from a fixture through the risk engine, offline.
//!
//! The fixture is SYNTHETIC (`tests/fixtures/perpl/synthetic-account.jsonl`):
//! live recorded positions await the testnet key (STUB-09), so the scenarios
//! below are hand-built with the arithmetic shown inline — exactly the
//! acceptance criterion "intents for liquidation-adjacent positions match
//! hand-computed expectations".
//!
//! Hand computation (mmr = 0.05 from `maintenance_margin = 2000`):
//!
//! ETH long (market 32): entry 2700.00, |size| 10.0, collateral 13 560.00
//!   MMR = 2700 * 10 * 0.05 = 1 350
//!   liq = 2700 + (1350 - 13560) / 10 = 2700 - 1221 = 1479.00
//!   mark 2713.70 → distance = (2713.70 - 1479.00) / 2713.70 * 100 ≈ 45.50 %
//!   ⇒ tier Green (≥ 25) ⇒ no reflex intent.
//!
//! BTC long (market 16): entry 95 000.0, |size| 0.1, collateral 950.00
//!   MMR = 95000 * 0.1 * 0.05 = 475
//!   liq = 95000 + (475 - 950) / 0.1 = 95000 - 4750 = 90 250.0
//!   mark 95 000.0 → distance = 4750 / 95000 * 100 = 5.0 %
//!   ⇒ tier Red (< 8, liquidation-adjacent) ⇒ Reduce { 0.5 } (default cfg).

use std::path::Path;

use rust_decimal::Decimal;
use sentinel::perpl::{MockPerpl, PerplFeed};
use sentinel_core::risk::{self, ReflexConfig, ReflexState, RiskThresholds};
use sentinel_core::types::{DataQuality, Intent, RiskTier};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/perpl/synthetic-account.jsonl")
}

fn thresholds() -> RiskThresholds {
    RiskThresholds {
        soft: Decimal::new(25, 0),
        warn: Decimal::new(15, 0),
        hard: Decimal::new(8, 0),
    }
}

#[tokio::test]
async fn classifies_fixture_snapshot_per_position() {
    let feed = MockPerpl::from_fixture(&fixture()).expect("synthetic fixture loads");
    let account = feed.snapshot().await.expect("snapshot composes");
    let markets = feed.context().await.expect("context parses");

    assert_eq!(account.positions.len(), 2, "two positions in the fixture");

    let cfg = ReflexConfig::default();
    let mut reflex = ReflexState::new();
    let mut eth = None;
    let mut btc = None;
    for pos in &account.positions {
        let market = markets
            .iter()
            .find(|m| m.id == pos.market_id)
            .expect("market in context");
        let distance = risk::distance_to_liq_pct(pos, market).expect("distance computable");
        let tier = risk::tier(distance, &thresholds());
        let intent = reflex.advance(pos, tier, DataQuality::Fresh, &cfg, 1_000);
        match pos.market_id.0 {
            32 => eth = Some((distance, tier, intent)),
            16 => btc = Some((distance, tier, intent)),
            other => panic!("unexpected market {other}"),
        }
    }

    let (eth_distance, eth_tier, eth_intent) = eth.expect("ETH position present");
    assert_eq!(eth_tier, RiskTier::Green, "ETH is comfortably above soft");
    assert_eq!(eth_intent, None, "Green never yields an intent");
    // distance = (2713.70 - 1479.00) / 2713.70 * 100, exact-decimal recompute:
    let eth_expected = (Decimal::new(271370, 2) - Decimal::new(147900, 2))
        / Decimal::new(271370, 2)
        * Decimal::new(100, 0);
    let eth_delta = (eth_distance - eth_expected).abs();
    assert!(
        eth_delta < Decimal::new(1, 6),
        "ETH distance {eth_distance} ≈ {eth_expected} (Δ {eth_delta})"
    );

    let (btc_distance, btc_tier, btc_intent) = btc.expect("BTC position present");
    assert_eq!(
        btc_distance,
        Decimal::new(5, 0),
        "BTC distance is exactly 5.0 %"
    );
    assert_eq!(btc_tier, RiskTier::Red, "5 % < hard 8 % ⇒ Red");
    assert!(
        matches!(btc_intent, Some(Intent::Reduce { fraction, .. }) if fraction == Decimal::new(5, 1)),
        "liquidation-adjacent position ⇒ Reduce {{ 0.5 }}, got {btc_intent:?}"
    );
}

#[tokio::test]
async fn escalation_after_cooldown_on_fixture_position() {
    let feed = MockPerpl::from_fixture(&fixture()).expect("fixture loads");
    let account = feed.snapshot().await.expect("snapshot");
    let btc = account
        .positions
        .iter()
        .find(|p| p.market_id.0 == 16)
        .expect("BTC position");

    let cfg = ReflexConfig::default();
    let mut reflex = ReflexState::new();
    let markets = feed.context().await.expect("context parses");
    let market = markets
        .iter()
        .find(|m| m.id == btc.market_id)
        .expect("BTC market in context");
    let t_dist = risk::distance_to_liq_pct(btc, market).expect("distance");
    let red = risk::tier(t_dist, &thresholds());
    assert_eq!(red, RiskTier::Red);

    // First breach ⇒ Reduce{0.5}; inside the cooldown ⇒ silent; after it ⇒ Close.
    let first = reflex.advance(btc, red, DataQuality::Fresh, &cfg, 0);
    assert!(matches!(first, Some(Intent::Reduce { .. })));
    let inside = reflex.advance(btc, red, DataQuality::Fresh, &cfg, cfg.cooldown_ms - 1);
    assert_eq!(inside, None, "cooldown not yet elapsed");
    let escalate = reflex.advance(btc, red, DataQuality::Fresh, &cfg, cfg.cooldown_ms);
    assert!(
        matches!(escalate, Some(Intent::Close { .. })),
        "still Red after cooldown ⇒ Close, got {escalate:?}"
    );
}
