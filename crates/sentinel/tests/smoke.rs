//! Smoke tests: configuration loads from an explicit variable map and
//! fail-fast validation rejects unsafe combinations.

use std::collections::HashMap;

use sentinel::config::{Config, LogFormat};
use sentinel_core::types::ExecutionMode;

const VALID_SECRET: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn vars(extra: &[(&str, &str)]) -> HashMap<String, String> {
    let base: &[(&str, &str)] = &[
        ("PERPL_ENV", "testnet"),
        ("PERPL_API_KEY", "test-token"),
        ("PERPL_API_KEY_SECRET", VALID_SECRET),
        ("QWEN_API_KEY", "qwen-test-key"),
        ("KIMI_API_KEY", "kimi-test-key"),
        ("TELOXIDE_TOKEN", "123456:test-token"),
        ("TELEGRAM_ALLOWED_USER_IDS", "1,2"),
        ("NANSEN_PAYER_KEY", "0x00"),
    ];
    base.iter()
        .chain(extra.iter())
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn config_loads_from_fixture_with_testnet_defaults() {
    let cfg = Config::from_vars(vars(&[])).expect("fixture config must load");

    assert_eq!(cfg.execution.mode, ExecutionMode::DryRun);
    assert_eq!(cfg.perpl.chain_id, 10143);
    assert_eq!(
        cfg.perpl.exchange_address,
        "0x1964c32f0be608e7d29302aff5e61268e72080cc"
    );
    assert_eq!(
        cfg.perpl.collateral_token,
        "0xa9012a055bd4e0edff8ce09f960291c09d5322dc"
    );
    assert_eq!(cfg.risk.market_allowlist, vec![20, 1]);
    assert!(cfg.risk.hard_pct < cfg.risk.warn_pct);
    assert!(cfg.risk.warn_pct < cfg.risk.soft_pct);
    assert_eq!(cfg.observability.log_format, LogFormat::Pretty);
    assert_eq!(cfg.nansen.payment_network, "eip155:143");
    assert!(cfg.features.enable_reflex);
}

#[test]
fn mainnet_requires_explicit_acknowledgement() {
    let err = Config::from_vars(vars(&[("EXECUTION_MODE", "MAINNET")]))
        .expect_err("MAINNET without acknowledgement must be rejected");
    assert!(format!("{err}").contains("I_UNDERSTAND_MAINNET_RISK"));

    let ok = Config::from_vars(vars(&[
        ("EXECUTION_MODE", "MAINNET"),
        ("I_UNDERSTAND_MAINNET_RISK", "yes"),
        ("PERPL_ENV", "mainnet"),
    ]))
    .expect("MAINNET with acknowledgement must load");
    assert_eq!(ok.execution.mode, ExecutionMode::Mainnet);
    assert_eq!(ok.perpl.chain_id, 143);
    assert_eq!(
        ok.perpl.exchange_address,
        "0x34B6552d57a35a1D042CcAe1951BD1C370112a6F"
    );
}

#[test]
fn rejected_when_mode_and_env_disagree() {
    let err = Config::from_vars(vars(&[
        ("EXECUTION_MODE", "MAINNET"),
        ("I_UNDERSTAND_MAINNET_RISK", "yes"),
    ]))
    .expect_err("MAINNET mode with testnet env must be rejected");
    assert!(format!("{err}").contains("PERPL_ENV=mainnet"));

    let err = Config::from_vars(vars(&[
        ("EXECUTION_MODE", "TESTNET"),
        ("PERPL_ENV", "mainnet"),
    ]))
    .expect_err("TESTNET mode with mainnet env must be rejected");
    assert!(format!("{err}").contains("PERPL_ENV=testnet"));
}

#[test]
fn rejected_when_threshold_order_broken() {
    let err = Config::from_vars(vars(&[("RISK_HARD_PCT", "20")]))
        .expect_err("hard >= warn must be rejected");
    assert!(format!("{err}").contains("hard < warn < soft"));
}

#[test]
fn rejected_on_invalid_secret_hex() {
    let err = Config::from_vars(vars(&[("PERPL_API_KEY_SECRET", "zz-not-hex")]))
        .expect_err("non-hex secret must be rejected");
    assert!(format!("{err}").contains("PERPL_API_KEY_SECRET"));
}

#[test]
fn rejected_when_no_telegram_users() {
    let err = Config::from_vars(vars(&[("TELEGRAM_ALLOWED_USER_IDS", " , ")]))
        .expect_err("empty telegram allowlist must be rejected");
    assert!(format!("{err}").contains("TELEGRAM_ALLOWED_USER_IDS"));
}
