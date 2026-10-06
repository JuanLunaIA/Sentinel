//! P09 acceptance link (parent-owned): the smart-money context the x402
//! client fills must render into the strategy prompt, and the bearish
//! scenario's decision reason must cite that netflow while staying grounded.
//!
//! This is the "SM context visibly changes a strategy decision" artifact from
//! SPEC-P09 §1: `nansen::smart_money_context` produces the same
//! `SmartMoneyContext` consumed here — the linkage is type-checked by the
//! constructor and asserted end-to-end by this test.

use sentinel::brain::eval::{Scenario, grounding_check};
use sentinel::brain::prompts::{PromptInput, user_prompt};
use sentinel_core::types::MarketId;

#[test]
fn bearish_sm_scenario_cites_netflow_in_a_grounded_reason() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/03-orange-bearish-sm.json");
    let raw = std::fs::read_to_string(&path).expect("read 03-orange-bearish-sm.json");
    let scenario: Scenario = serde_json::from_str(&raw).expect("parse scenario");

    // The context the x402 client fills is present and bearish.
    let netflow = scenario.sm.netflow_24h.expect("netflow_24h present");
    assert!(netflow < rust_decimal::Decimal::ZERO, "bearish scenario");
    assert_eq!(netflow.to_string(), "-2500000");

    // It renders into the prompt the model actually sees.
    let focus_market = MarketId(scenario.focus_market_id);
    let focus = scenario
        .snapshot
        .positions
        .iter()
        .find(|position| position.market_id == focus_market)
        .expect("focus position present");
    let input = PromptInput {
        account: &scenario.snapshot,
        markets: &scenario.markets,
        focus,
        policy: &scenario.policy,
        sm: &scenario.sm,
        reflex: &scenario.reflex,
        now_ms: 1_000_000_000,
    };
    let prompt = user_prompt(&input);
    assert!(
        prompt.contains("2500000"),
        "the netflow value must appear in the rendered prompt"
    );

    // The decision reason cites it and every number stays grounded.
    let decision: serde_json::Value = serde_json::from_str(
        scenario
            .mock_completion
            .as_deref()
            .expect("mock completion present"),
    )
    .expect("mock completion is JSON");
    let reason = decision["reason"].as_str().expect("reason string");
    assert!(
        reason.contains("2500000"),
        "the bearish decision must cite the netflow"
    );
    assert!(
        grounding_check(reason, &prompt),
        "reason numbers must all appear in the prompt input"
    );
}
