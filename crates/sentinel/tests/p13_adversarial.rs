//! P13 adversarial verification — independent black-box corpus for the P13
//! backtester (`crates/sentinel/src/sim/*`), written from SPEC-P13 only.
//!
//! The verifier never read the writer's engine sources. Oracle of record:
//! `tests/fixtures/p13/oracle.py` (Python `decimal`, hand-verified pins),
//! which recomputes the frozen SPEC-P13 §5 accounting from the scenario
//! files; the pinned expectations in `tests/fixtures/p13/expected/*.json`
//! are live-reproduced by that script from inside this suite.
//!
//! Coverage:
//! - **Conservation** (SPEC-P13 §5): `capital_saved_usd == baseline_loss_usd
//!   - sentinel_loss_usd` EXACT Decimal on every scenario of the verifier
//!   corpus, plus the sibling corpus in `tests/scenarios/` when present.
//! - **Oracle pins**: three+ hand-built mini-scenarios re-computed by the
//!   Python-decimal oracle (conservation + loss + fees + liquidation counts).
//! - **Determinism** (§6): two runs of `sim::engine::run` on the same
//!   scenario are byte-identical after dropping `metrics`.
//! - **Cooldown** (§4): a retest strictly inside `cooldown_ms` stays silent;
//!   a gap exactly EQUAL to `cooldown_ms` counts as elapsed and fires
//!   (SPEC-P04 §3.4 step 2, pinned against the implementation).
//! - **Label honesty** (§3/§8): `tests/scenarios/*.json` — recorded/
//!   reconstructed carry a note naming an existing source fixture; synthetic
//!   files never cite fixtures.
//! - **Tamper/falsification**: an extra tick after the liquidation crossing
//!   must change the report exactly as the spec says (no cached results).
//! - **Validation** (§3): ts order, unknown markets, non-positive marks,
//!   fraction bounds, structure requirements, malformed JSON, missing file.
//! - **Boundaries** (§4): flat position (no actions, no panic); dust below
//!   min_size (no order, reason recorded).
//! - **Policy gate** (§4): cap, approval and daily-cap skip recorded, no
//!   execution beyond the gate, `policy_violations == 0`.
//! - **Report/aggregate** (§6): frozen literal line in the markdown, exact
//!   aggregate sums.
//! - **Acceptance** (§7/§8, sibling-gated): the 14 repo scenarios conserve,
//!   aggregate saved > 0, and the released `backtest` binary runs
//!   `--scenario all` in < 60 s.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Instant;

use rust_decimal::Decimal;
use serde_json::Value;

use sentinel::sim::engine;
use sentinel::sim::report::{self, SimReport};
use sentinel::sim::scenario::{self, ScenarioError};

// ===========================================================================
// corpus helpers
// ===========================================================================

/// Repository root (`crates/sentinel` -> `crates` -> repo).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/sentinel has a repo root")
        .to_path_buf()
}

fn p13_dir() -> PathBuf {
    repo_root().join("tests/fixtures/p13")
}

fn scenarios_dir() -> PathBuf {
    p13_dir().join("scenarios")
}

fn invalid_dir() -> PathBuf {
    p13_dir().join("invalid")
}

fn expected_dir() -> PathBuf {
    p13_dir().join("expected")
}

/// Sorted `*.json` file names of a directory.
fn json_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf8 file name")
        })
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names
}

fn stem(name: &str) -> &str {
    name.strip_suffix(".json").expect("json file name")
}

fn raw_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Parse a JSON scalar (string or number) as an exact Decimal.
fn value_decimal(value: &Value) -> Decimal {
    match value {
        Value::String(text) => Decimal::from_str(text).expect("decimal string"),
        other => Decimal::from_str(&other.to_string()).expect("decimal number"),
    }
}

fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap_or_else(|e| panic!("bad decimal {text}: {e}"))
}

fn load(path: &Path) -> scenario::Scenario {
    scenario::load(path).unwrap_or_else(|e| panic!("scenario::load {}: {e}", path.display()))
}

fn run(path: &Path) -> SimReport {
    let scenario = load(path);
    engine::run(&scenario).unwrap_or_else(|e| panic!("engine::run {}: {e}", path.display()))
}

/// The report with `metrics` removed (SPEC-P13 §6 determinism section).
fn report_value(report: &SimReport) -> Value {
    let mut value = serde_json::to_value(report).expect("SimReport serializes");
    if let Value::Object(map) = &mut value {
        map.remove("metrics");
    }
    value
}

/// Size transitions recorded by the report: (ts_ms, size_after) every time
/// `size_after` moves away from the running size (initial = pre-sim size).
/// This is how this suite counts *executions* without parsing detail text.
fn size_transitions(report: &SimReport, initial: Decimal) -> Vec<(i64, Decimal)> {
    let mut previous = initial;
    let mut out = Vec::new();
    for action in &report.actions {
        if action.size_after != previous {
            out.push((action.ts_ms, action.size_after));
            previous = action.size_after;
        }
    }
    out
}

/// Concatenated human-readable `detail` + `notes` text, lowercased.
fn recorded_text(report: &SimReport) -> String {
    let mut text = String::new();
    for action in &report.actions {
        text.push_str(&action.detail);
        text.push('\n');
    }
    for note in &report.notes {
        text.push_str(note);
        text.push('\n');
    }
    text.to_lowercase()
}

fn initial_size_of(path: &Path) -> Decimal {
    let raw: Value = serde_json::from_str(&raw_text(path)).expect("scenario JSON");
    value_decimal(&raw["positions"][0]["size"])
}

fn check_conservation(report: &SimReport, tag: &str) {
    assert_eq!(
        report.capital_saved_usd,
        report.baseline_loss_usd - report.sentinel_loss_usd,
        "{tag}: conservation identity capital_saved == baseline_loss - sentinel_loss (SPEC-P13 §5)"
    );
    assert_eq!(
        report.policy_violations, 0,
        "{tag}: policy_violations must be 0 (SPEC-P13 §6)"
    );
}

fn python() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_string())
}

/// Run the oracle script on a scenario and return its JSON output.
fn oracle_output(scenario_file: &Path) -> Value {
    let output = Command::new(python())
        .arg("oracle.py")
        .arg(scenario_file)
        .current_dir(p13_dir())
        .output()
        .expect("python3 must be on PATH (or set $PYTHON)");
    assert!(
        output.status.success(),
        "oracle.py failed for {}: {}",
        scenario_file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle JSON output")
}

// ===========================================================================
// 1. conservation identity — every scenario of the verifier corpus
// ===========================================================================

#[test]
fn conservation_identity_on_own_corpus() {
    let names = json_files(&scenarios_dir());
    assert!(
        names.len() >= 12,
        "verifier corpus expected at least 12 scenarios, found {}",
        names.len()
    );
    for name in &names {
        let path = scenarios_dir().join(name);
        let report = run(&path);
        check_conservation(&report, name);
        assert_eq!(
            report.scenario_id,
            stem(name),
            "{name}: scenario_id must match the file stem"
        );
        assert_eq!(report.label, "synthetic", "{name}: corpus label");
        // liquidations_avoided is pinned to the definition (SPEC-P13 §5).
        assert_eq!(
            report.liquidations_avoided,
            report.baseline_liquidations - report.sentinel_liquidations,
            "{name}: liquidations_avoided == baseline - sentinel"
        );
    }
    println!("conservation verified on {} corpus scenarios", names.len());
}

// ===========================================================================
// 2. oracle recomputation — pinned hand-built mini-scenarios
// ===========================================================================

#[test]
fn oracle_pins_match_engine_on_own_corpus() {
    let pins = json_files(&expected_dir());
    assert!(pins.len() >= 12, "expected pin files");
    for pin_name in &pins {
        let pin: Value =
            serde_json::from_str(&raw_text(&expected_dir().join(pin_name))).expect("pin JSON");
        let scenario_name = pin["scenario"].as_str().expect("pin scenario");
        let path = scenarios_dir().join(format!("{scenario_name}.json"));
        let report = run(&path);
        let tag = format!("{scenario_name} (oracle pin)");

        assert_eq!(
            report.ticks,
            pin["ticks"].as_u64().expect("pin ticks"),
            "{tag}: ticks"
        );
        assert_eq!(
            report.baseline_liquidations,
            pin["baseline_liquidations"].as_u64().expect("pin") as u32,
            "{tag}: baseline_liquidations"
        );
        assert_eq!(
            report.liquidations_avoided,
            pin["liquidations_avoided"].as_u64().expect("pin") as u32,
            "{tag}: liquidations_avoided"
        );
        assert_eq!(
            report.false_positive_reduces,
            pin["false_positive_reduces"].as_u64().expect("pin") as u32,
            "{tag}: false_positive_reduces"
        );
        assert_eq!(
            report.baseline_loss_usd,
            value_decimal(&pin["baseline_loss_usd"]),
            "{tag}: baseline_loss_usd"
        );
        if pin["pin_sentinel"].as_bool().unwrap_or(true) {
            assert_eq!(
                report.sentinel_loss_usd,
                value_decimal(&pin["sentinel_loss_usd"]),
                "{tag}: sentinel_loss_usd"
            );
            assert_eq!(
                report.capital_saved_usd,
                value_decimal(&pin["capital_saved_usd"]),
                "{tag}: capital_saved_usd"
            );
            assert_eq!(
                report.sim_fees_usd,
                value_decimal(&pin["sim_fees_usd"]),
                "{tag}: sim_fees_usd"
            );
        }
        check_conservation(&report, &tag);

        // Executed fills: count and (ts_ms, size_after) sequence must match
        // the oracle's construction.
        let initial = initial_size_of(&path);
        let transitions = size_transitions(&report, initial);
        let expected_executed = pin["executed"].as_array().expect("pin executed");
        assert_eq!(
            transitions.len(),
            expected_executed.len(),
            "{tag}: executed action count (transitions {transitions:?})"
        );
        for (actual, expected) in transitions.iter().zip(expected_executed) {
            assert_eq!(
                actual.0,
                expected["ts_ms"].as_i64().expect("ts_ms"),
                "{tag}: executed ts order (actual {transitions:?})"
            );
            assert_eq!(
                actual.1,
                value_decimal(&expected["size_after"]),
                "{tag}: size_after after execution"
            );
        }
    }
    println!("oracle pins verified on {} scenarios", pins.len());
}

#[test]
fn oracle_script_reproduces_pins_and_engine() {
    // The Python oracle must be runnable from this suite, reproduce the pin
    // files it generated, and agree with the engine on the pinned scenarios.
    let pins = json_files(&expected_dir());
    let mut compared = 0;
    for pin_name in &pins {
        let pin: Value =
            serde_json::from_str(&raw_text(&expected_dir().join(pin_name))).expect("pin JSON");
        let scenario_name = pin["scenario"].as_str().expect("pin scenario");
        let path = scenarios_dir().join(format!("{scenario_name}.json"));
        let fresh = oracle_output(&path);
        for field in [
            "baseline_loss_usd",
            "sentinel_loss_usd",
            "capital_saved_usd",
            "sim_fees_usd",
        ] {
            assert_eq!(
                value_decimal(&fresh[field]),
                value_decimal(&pin[field]),
                "{scenario_name}: oracle.py live output must reproduce the pin for {field}"
            );
        }
        if pin["pin_sentinel"].as_bool().unwrap_or(true) {
            let report = run(&path);
            assert_eq!(
                report.sentinel_loss_usd,
                value_decimal(&fresh["sentinel_loss_usd"]),
                "{scenario_name}: engine sentinel_loss must equal the live oracle"
            );
            assert_eq!(
                report.baseline_loss_usd,
                value_decimal(&fresh["baseline_loss_usd"]),
                "{scenario_name}: engine baseline_loss must equal the live oracle"
            );
            compared += 1;
        }
    }
    assert!(
        compared >= 3,
        "at least 3 scenarios must be fully oracle-compared (got {compared})"
    );
}

// ===========================================================================
// 3. determinism
// ===========================================================================

#[test]
fn determinism_run_twice_byte_identical() {
    for name in [
        "o1-long-orange-recover.json",
        "o2-close-before-crash.json",
        "cd2-cooldown-boundary.json",
        "policy-daily-cap.json",
    ] {
        let path = scenarios_dir().join(name);
        let scenario = load(&path);
        let first = engine::run(&scenario).expect("first run");
        let second = engine::run(&scenario).expect("second run");
        let first_value = report_value(&first);
        let second_value = report_value(&second);
        assert_eq!(
            first_value, second_value,
            "{name}: two runs must be byte-identical minus metrics (Value)"
        );
        assert_eq!(
            serde_json::to_string(&first_value).expect("json"),
            serde_json::to_string(&second_value).expect("json"),
            "{name}: serialized forms must be byte-identical minus metrics"
        );
        // Re-load from disk and re-run: still identical.
        let reloaded = load(&path);
        let third = engine::run(&reloaded).expect("third run");
        assert_eq!(
            report_value(&third),
            first_value,
            "{name}: re-loaded scenario must reproduce the same report"
        );
    }
}

// ===========================================================================
// 4. cooldown — inside window silent; boundary equality elapses
// ===========================================================================

#[test]
fn cooldown_inside_window_fires_once() {
    let path = scenarios_dir().join("cd1-cooldown-inside.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    let transitions = size_transitions(&report, initial);
    assert_eq!(
        transitions.len(),
        1,
        "cd1: exactly one reduce may execute inside the cooldown window (got {transitions:?})"
    );
    assert_eq!(
        transitions[0].1,
        dec("1.5"),
        "cd1: only the 25% slice fills"
    );
    // No fill may have moved the size again to 1.125 (that would be a second
    // re-fire at +300000ms / +599999ms, both strictly inside 600000ms).
    assert!(
        !report.actions.iter().any(|a| a.size_after == dec("1.125")),
        "cd1: no second reduce may execute inside the cooldown window"
    );
    assert_eq!(report.false_positive_reduces, 1, "cd1: exactly one reduce");
}

#[test]
fn cooldown_boundary_equal_gap_fires() {
    let path = scenarios_dir().join("cd2-cooldown-boundary.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    let transitions = size_transitions(&report, initial);
    assert_eq!(
        transitions.len(),
        2,
        "cd2: the tick exactly cooldown_ms later must fire (equality counts as elapsed); \
         the tick 60000ms into the new window must stay blocked (got {transitions:?})"
    );
    assert_eq!(
        transitions[1].0 - transitions[0].0,
        600_000,
        "cd2: the second fill must be exactly cooldown_ms after the first"
    );
    assert_eq!(transitions[0].1, dec("1.5"));
    assert_eq!(transitions[1].1, dec("1.125"));
    // The blocked retest after the second action must leave the size alone.
    assert_eq!(
        report.actions.last().expect("actions").size_after,
        dec("1.125"),
        "cd2: the trailing retest must not execute"
    );
    // Fees prove two distinct fills (0.1423575 + 0.10714275).
    assert_eq!(
        report.sim_fees_usd,
        dec("0.24950025"),
        "cd2: exactly two fills' fees, pinned from the oracle"
    );
}

// ===========================================================================
// 5. label honesty — sibling corpus in tests/scenarios/
// ===========================================================================

/// Fixture-path tokens ("...fixtures/...") referenced by a scenario text.
fn fixture_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| {
        !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    })
    .filter(|token| token.contains("fixtures/"))
    .map(str::to_string)
    .collect()
}

fn fixture_token_exists(token: &str) -> bool {
    let root = repo_root();
    let direct = root.join(token);
    let under_tests = root.join("tests").join(token);
    direct.is_file() || under_tests.is_file()
}

/// Lowercased file names of every file under `tests/fixtures/` (any depth).
fn fixture_file_names() -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    out.push(name.to_lowercase());
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo_root().join("tests/fixtures"), &mut out);
    out
}

/// Does a note name an existing fixture, either by path token or by a
/// distinctive name token (e.g. "P06" for the p06 crash fixtures)?
fn note_names_source(note: &str) -> bool {
    if fixture_tokens(note)
        .iter()
        .any(|token| fixture_token_exists(token))
    {
        return true;
    }
    let names = fixture_file_names();
    note.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|token| token.len() >= 3)
        .any(|token| names.iter().any(|name| name.contains(token)))
}

#[test]
fn label_honesty_scenarios_dir() {
    let dir = repo_root().join("tests/scenarios");
    assert!(
        dir.is_dir(),
        "tests/scenarios/ (sibling corpus, SPEC-P13 §2/§8) is missing — pending sibling artifact"
    );
    let names = json_files(&dir);
    assert_eq!(
        names.len(),
        14,
        "SPEC-P13 §8 fixes 14 scenario files; found {} in {}",
        names.len(),
        dir.display()
    );
    let mut labels = std::collections::BTreeMap::new();
    for name in &names {
        let path = dir.join(name);
        let text = raw_text(&path);
        let raw: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: not valid JSON: {e}"));
        let label = raw["label"].as_str().expect("label field").to_string();
        *labels.entry(label.clone()).or_insert(0u32) += 1;

        match label.as_str() {
            "synthetic" => {
                let tokens = fixture_tokens(&text);
                assert!(
                    tokens.is_empty(),
                    "{name}: synthetic must not cite fixtures, found {tokens:?}"
                );
            }
            "recorded" | "reconstructed" => {
                let description = raw["description"].as_str().unwrap_or_default().to_string();
                let tokens = fixture_tokens(&text);
                assert!(
                    !tokens.is_empty() || !description.is_empty(),
                    "{name}: label {label} must carry a note naming its source fixture"
                );
                if label == "recorded" {
                    // A recorded replay must cite the recorded fixture path.
                    assert!(
                        tokens.iter().any(|token| fixture_token_exists(token)),
                        "{name}: label recorded must cite an EXISTING source fixture path, found {tokens:?}"
                    );
                } else {
                    // A reconstruction may name its source by fixture name
                    // (e.g. \"P06 crash position\") instead of by path.
                    assert!(
                        note_names_source(&description) || note_names_source(&text),
                        "{name}: label reconstructed must name its source fixture, description: {description:?}"
                    );
                }
            }
            other => panic!("{name}: unknown label {other:?}"),
        }

        // The report renders the label verbatim.
        let report = run(&path);
        assert_eq!(
            report.label, label,
            "{name}: report label must render the file label verbatim"
        );
        check_conservation(&report, name);
    }
    assert_eq!(
        labels.get("synthetic").copied().unwrap_or(0),
        12,
        "12 synthetic (§8)"
    );
    assert_eq!(
        labels.get("recorded").copied().unwrap_or(0),
        1,
        "1 recorded (§8)"
    );
    assert_eq!(
        labels.get("reconstructed").copied().unwrap_or(0),
        1,
        "1 reconstructed (§8)"
    );
}

// ===========================================================================
// 6. tamper/falsification — extra tick after the liquidation
// ===========================================================================

#[test]
fn tamper_extra_tick_changes_report() {
    let base_path = scenarios_dir().join("tamper-base.json");
    let extra_path = scenarios_dir().join("tamper-extra-tick.json");
    let base = run(&base_path);
    let extra = run(&extra_path);

    // The base path never liquidates; the extra tick at 2450 crosses the
    // 2500 liquidation price, so the baseline must lose the whole collateral
    // (SPEC-P13 §5) and every derived number must follow — no constant report.
    assert_eq!(base.ticks, 1, "base: one tick");
    assert_eq!(extra.ticks, 2, "extra: two ticks");
    assert_eq!(base.baseline_liquidations, 0, "base: no liquidation");
    assert_eq!(
        extra.baseline_liquidations, 1,
        "extra: liquidation on the new tick"
    );
    assert_eq!(
        base.baseline_loss_usd,
        dec("300"),
        "base: final unrealized loss"
    );
    assert_eq!(
        extra.baseline_loss_usd,
        dec("600"),
        "extra: liquidated => full collateral (600)"
    );
    assert_ne!(
        report_value(&base),
        report_value(&extra),
        "mutated scenario must produce a different report (no cached results)"
    );
    check_conservation(&base, "tamper-base");
    check_conservation(&extra, "tamper-extra-tick");
}

// ===========================================================================
// 7. validation errors
// ===========================================================================

fn expect_invalid(name: &str) -> ScenarioError {
    let path = invalid_dir().join(name);
    let error = scenario::load(&path).expect_err(&format!("{name} must fail validation"));
    assert!(
        matches!(error, ScenarioError::Invalid(_)),
        "{name}: expected ScenarioError::Invalid, got {error:?}"
    );
    error
}

#[test]
fn validation_rejects_ts_order_violations() {
    let a = expect_invalid("bad-ts-order.json");
    let b = expect_invalid("bad-ts-duplicate.json");
    assert!(!a.to_string().is_empty() && !b.to_string().is_empty());
}

#[test]
fn validation_rejects_unknown_markets() {
    expect_invalid("bad-market-price-path.json");
    expect_invalid("bad-market-position.json");
}

#[test]
fn validation_rejects_non_positive_marks() {
    expect_invalid("bad-mark-negative.json");
    expect_invalid("bad-mark-zero.json");
}

#[test]
fn validation_rejects_fraction_bounds() {
    let zero = expect_invalid("bad-fraction-zero.json");
    let above = expect_invalid("bad-fraction-above.json");
    assert!(zero.to_string().contains("fraction"), "message: {zero}");
    assert!(above.to_string().contains("fraction"), "message: {above}");
    // Positive control: the valid corpus loads and runs.
    let ok = load(&scenarios_dir().join("o1-long-orange-recover.json"));
    assert!(engine::run(&ok).is_ok());
}

#[test]
fn validation_rejects_missing_structure() {
    expect_invalid("bad-no-positions.json");
    expect_invalid("bad-no-ticks.json");
}

#[test]
fn validation_typed_errors_for_json_and_io() {
    let bad_json =
        scenario::load(&invalid_dir().join("bad-json.json")).expect_err("malformed JSON must fail");
    assert!(
        matches!(bad_json, ScenarioError::Json(_)),
        "malformed JSON must be ScenarioError::Json, got {bad_json:?}"
    );
    let missing = scenario::load(&invalid_dir().join("does-not-exist.json"))
        .expect_err("missing file must fail");
    assert!(
        matches!(missing, ScenarioError::Io(_)),
        "missing file must be ScenarioError::Io, got {missing:?}"
    );
}

// ===========================================================================
// 8. boundaries — flat position, dust
// ===========================================================================

#[test]
fn boundary_zero_size_position_has_no_actions() {
    let path = scenarios_dir().join("zero-size-position.json");
    let report = run(&path);
    assert!(
        report.actions.iter().all(|a| a.size_after == Decimal::ZERO),
        "zero-size: no action may change a flat position"
    );
    assert_eq!(report.ticks, 2);
    assert_eq!(report.baseline_loss_usd, Decimal::ZERO);
    assert_eq!(report.sentinel_loss_usd, Decimal::ZERO);
    assert_eq!(report.capital_saved_usd, Decimal::ZERO);
    assert_eq!(report.sim_fees_usd, Decimal::ZERO);
    assert_eq!(report.baseline_liquidations, 0);
    assert_eq!(report.sentinel_liquidations, 0);
    check_conservation(&report, "zero-size-position");
}

#[test]
fn boundary_dust_below_min_size_no_order_with_reason() {
    let path = scenarios_dir().join("dust-below-min-size.json");
    let report = run(&path);
    assert_eq!(
        report.sim_fees_usd,
        Decimal::ZERO,
        "dust: no fills, no fees"
    );
    let initial = initial_size_of(&path);
    assert!(
        report.actions.iter().all(|a| a.size_after == initial),
        "dust: no order may execute"
    );
    let text = recorded_text(&report);
    assert!(
        ["min", "size", "lot", "skip", "dust"]
            .iter()
            .any(|needle| text.contains(needle)),
        "dust: a reason naming the sizing skip must be recorded; recorded text: {text:?}"
    );
    check_conservation(&report, "dust-below-min-size");
}

// ===========================================================================
// 9. policy gate
// ===========================================================================

#[test]
fn policy_notional_cap_blocks_before_execution() {
    let path = scenarios_dir().join("policy-notional-cap.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    assert!(
        report.actions.iter().all(|a| a.size_after == initial),
        "cap: nothing may execute beyond the gate"
    );
    assert_eq!(report.sim_fees_usd, Decimal::ZERO, "cap: no fills");
    let text = recorded_text(&report);
    assert!(
        text.contains("notional") || text.contains("max_order") || text.contains("exceeds"),
        "cap: the Deny verdict must be recorded; recorded text: {text:?}"
    );
    assert_eq!(report.policy_violations, 0, "cap: policy_violations");
    assert_eq!(report.baseline_loss_usd, dec("100"));
    assert_eq!(report.sentinel_loss_usd, dec("100"));
}

#[test]
fn policy_approval_skip_recorded() {
    let path = scenarios_dir().join("policy-approval-skip.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    assert!(
        report.actions.iter().all(|a| a.size_after == initial),
        "approval: nothing may execute while approval is required"
    );
    let text = recorded_text(&report);
    assert!(
        text.contains("approval"),
        "approval: the NeedsApproval verdict must be recorded; recorded text: {text:?}"
    );
    assert_eq!(report.policy_violations, 0);
}

#[test]
fn policy_daily_cap_limits_executions() {
    let path = scenarios_dir().join("policy-daily-cap.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    let transitions = size_transitions(&report, initial);
    assert_eq!(
        transitions.len(),
        1,
        "daily cap 1: exactly one execution allowed (got {transitions:?})"
    );
    assert_eq!(transitions[0].1, dec("1"));
    let text = recorded_text(&report);
    assert!(
        text.contains("daily") || text.contains("cap"),
        "daily cap: the second action's denial must be recorded; recorded text: {text:?}"
    );
    assert_eq!(report.policy_violations, 0);
    assert_eq!(report.sim_fees_usd, dec("0.25974"), "one fill's fees only");
}

// ===========================================================================
// 10. stale feed semantics
// ===========================================================================

#[test]
fn stale_window_alerts_instead_of_reducing() {
    let path = scenarios_dir().join("stale-alert.json");
    let report = run(&path);
    let initial = initial_size_of(&path);
    assert!(
        report.actions.iter().all(|a| a.size_after == initial),
        "stale: no reduce may execute while the feed is stale (stale_reduce=false)"
    );
    assert_eq!(report.sim_fees_usd, Decimal::ZERO, "stale: no fills");
    assert_eq!(
        report.sentinel_loss_usd,
        dec("800"),
        "stale: unchanged accounting"
    );
    assert_eq!(report.baseline_loss_usd, dec("800"));
    check_conservation(&report, "stale-alert");
}

// ===========================================================================
// 11. report + aggregate
// ===========================================================================

#[test]
fn aggregate_and_render_md_frozen_literal() {
    let reports: Vec<SimReport> = [
        "o1-long-orange-recover.json",
        "o2-close-before-crash.json",
        "cd2-cooldown-boundary.json",
    ]
    .iter()
    .map(|name| run(&scenarios_dir().join(name)))
    .collect();

    let agg = report::aggregate(&reports);
    assert_eq!(agg.scenarios, 3, "aggregate scenario count");
    let saved_sum: Decimal = reports.iter().map(|r| r.capital_saved_usd).sum();
    assert_eq!(agg.total_saved_usd, saved_sum, "aggregate total_saved_usd");
    let notional_sum: Decimal = reports.iter().map(|r| r.notional_usd).sum();
    assert_eq!(
        agg.total_notional_usd, notional_sum,
        "aggregate total_notional_usd must equal the sum of per-report notional_usd"
    );
    let base_sum: u32 = reports.iter().map(|r| r.baseline_liquidations).sum();
    let sent_sum: u32 = reports.iter().map(|r| r.sentinel_liquidations).sum();
    assert_eq!(agg.baseline_liquidations, base_sum);
    assert_eq!(agg.sentinel_liquidations, sent_sum);
    assert!(
        agg.total_notional_usd > Decimal::ZERO,
        "notional must be positive"
    );

    let md = report::render_md(&reports, &agg);
    assert!(
        md.contains("Across 3 scenarios representing $"),
        "frozen literal (SPEC-P13 §6) missing its opening; md was:\n{md}"
    );
    assert!(
        md.contains("notional, Sentinel preserved $") && md.contains("); baseline liquidations: "),
        "frozen literal (SPEC-P13 §6) shape changed; md was:\n{md}"
    );
    assert!(
        md.contains(" -> with Sentinel: "),
        "frozen literal (SPEC-P13 §6) arrow missing; md was:\n{md}"
    );
    for report in &reports {
        assert!(
            md.contains(&report.scenario_id),
            "md must list scenario {}",
            report.scenario_id
        );
    }
}

// ===========================================================================
// 12. acceptance on the sibling corpus (pending-sibling gated)
// ===========================================================================

#[test]
fn repo_scenarios_conservation_and_acceptance() {
    let dir = repo_root().join("tests/scenarios");
    assert!(
        dir.is_dir(),
        "tests/scenarios/ (sibling corpus, SPEC-P13 §2/§8) is missing — pending sibling artifact"
    );
    let names = json_files(&dir);
    assert_eq!(names.len(), 14, "SPEC-P13 §8 fixes 14 scenarios");

    let mut reports: Vec<SimReport> = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for name in &names {
        let path = dir.join(name);
        let report = run(&path);
        check_conservation(&report, name);
        assert_eq!(report.scenario_id, stem(name), "{name}: id/stem");
        assert!(seen_ids.insert(report.scenario_id.clone()), "duplicate id");
        assert_eq!(
            report.liquidations_avoided,
            report.baseline_liquidations - report.sentinel_liquidations,
            "{name}"
        );
        reports.push(report);
    }

    let agg = report::aggregate(&reports);
    assert_eq!(agg.scenarios, 14);
    assert!(
        agg.total_saved_usd > Decimal::ZERO,
        "aggregate saved must be > 0 across the 14 scenarios (SPEC-P13 §8), got {}",
        agg.total_saved_usd
    );
    assert_eq!(
        agg.baseline_liquidations,
        reports.iter().map(|r| r.baseline_liquidations).sum::<u32>()
    );
    assert_eq!(
        agg.sentinel_liquidations,
        reports.iter().map(|r| r.sentinel_liquidations).sum::<u32>()
    );
}

#[test]
fn backtest_bin_all_scenarios_under_60s() {
    let dir = repo_root().join("tests/scenarios");
    assert!(
        dir.is_dir(),
        "tests/scenarios/ missing — backtest bin cannot run the suite; pending sibling artifact"
    );
    let out = tempfile::tempdir().expect("tempdir");
    // `--out` is a file stem: the bin writes `<stem>.json` + `<stem>.md`
    // (SPEC-P13 §7: `--out docs/backtest-report` writes `.json` + `.md`).
    let stem = out.path().join("p13-verify");
    let start = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_backtest"))
        .args(["--scenario", "all", "--out"])
        .arg(&stem)
        .current_dir(repo_root())
        .status()
        .expect("backtest binary runs");
    let elapsed = start.elapsed();
    assert!(status.success(), "backtest --scenario all must exit 0");
    assert!(
        elapsed.as_secs_f64() < 60.0,
        "SPEC-P13 §7 acceptance: all scenarios must run in < 60 s, took {:?}",
        elapsed
    );
    let json_path = stem.with_extension("json");
    let md_path = stem.with_extension("md");
    assert!(
        json_path.is_file(),
        "backtest must write {} (produced in {}: {:?})",
        json_path.display(),
        out.path().display(),
        fs::read_dir(out.path())
            .expect("out dir")
            .map(|e| e.expect("entry").file_name())
            .collect::<Vec<_>>()
    );
    assert!(
        md_path.is_file(),
        "backtest must write {}",
        md_path.display()
    );
    let doc: Value = serde_json::from_str(&raw_text(&json_path)).expect("report JSON parses");
    assert_eq!(
        doc["scenarios"]
            .as_array()
            .map(|a| a.len())
            .or_else(|| doc["aggregate"]["scenarios"].as_u64().map(|n| n as usize)),
        Some(14),
        "the report must cover the 14 scenarios; shape: {}",
        doc
    );
    println!("backtest --scenario all wall clock: {elapsed:?}");
}
