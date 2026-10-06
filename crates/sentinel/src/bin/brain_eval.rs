//! `brain_eval` — score the strategy brain over golden scenarios.
//!
//! Usage (`SPEC-P07.md` §7):
//!   `cargo run --bin brain_eval -- [--scenarios tests/golden] [--mock]
//!    [--provider qwen|kimi] [--out docs/evidence/p07-brain-eval.txt]`
//!
//! `--mock` replays each scenario's canned `mock_completion` through the real
//! engine (no network); live mode builds the configured Qwen/Kimi provider.
//! Every scenario produces a row (errors are rendered, never panics), the
//! per-scenario `now_ms` advances by `(min_interval + 1) s` so the rate
//! limiter cannot interfere, and an all-errored run still exits 0 with the
//! DEGRADED banner. Exit is non-zero only when setup fails (unreadable
//! scenarios directory, malformed flags/scenario files, live config load).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rust_decimal::Decimal;
use sentinel::brain::engine::{ConsultInput, StrategyEngine};
use sentinel::brain::eval::{Scenario, ScenarioResult, grounding_check, render_scoreboard, score};
use sentinel::brain::prompts::{self, PromptInput};
use sentinel::brain::providers::{KimiProvider, MockProvider, Provider, QwenProvider};
use sentinel::config::Config;
use sentinel_core::types::{DecisionAction, MarketId};

/// Usage line for `--help` and flag errors.
const USAGE: &str =
    "usage: brain_eval [--scenarios DIR] [--mock] [--provider qwen|kimi] [--out PATH]";

/// Minimum consult interval assumed in `--mock` runs, seconds; mirrors the
/// `STRATEGY_MIN_INTERVAL_SECS` default. A fresh engine per scenario means the
/// rate limiter cannot interfere anyway; the same `now_ms` formula as live
/// mode is kept.
const MOCK_MIN_INTERVAL_SECS: u64 = 120;

/// Provider label rendered on mock-mode error rows.
const MOCK_PROVIDER: &str = "mock";

/// Which live provider to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderChoice {
    /// Qwen (primary).
    Qwen,
    /// Kimi (fallback; pulled forward for P07).
    Kimi,
}

/// Parsed command-line options.
#[derive(Debug)]
struct Options {
    /// Directory holding `*.json` scenarios.
    scenarios_dir: PathBuf,
    /// Whether `--scenarios` was passed explicitly (used by
    /// [`resolve_scenarios_dir`]).
    scenarios_explicit: bool,
    /// Replay `mock_completion`s instead of calling a live provider.
    mock: bool,
    /// Live provider to build (ignored with `--mock`).
    provider: ProviderChoice,
    /// Where to write a copy of the scoreboard.
    out: Option<PathBuf>,
}

impl Options {
    /// Hand-rolled argument parsing (no CLI-framework dependency).
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut options = Options {
            scenarios_dir: PathBuf::from("tests/golden"),
            scenarios_explicit: false,
            mock: false,
            provider: ProviderChoice::Qwen,
            out: None,
        };
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--scenarios" => {
                    let value = args
                        .next()
                        .context("--scenarios needs a directory argument")?;
                    options.scenarios_dir = PathBuf::from(value);
                    options.scenarios_explicit = true;
                }
                "--mock" => options.mock = true,
                "--provider" => {
                    let value = args.next().context("--provider needs an argument")?;
                    options.provider = match value.as_str() {
                        "qwen" => ProviderChoice::Qwen,
                        "kimi" => ProviderChoice::Kimi,
                        other => bail!("unknown provider {other:?} (expected qwen or kimi)"),
                    };
                }
                "--out" => {
                    let value = args.next().context("--out needs a path argument")?;
                    options.out = Some(PathBuf::from(value));
                }
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => bail!("unknown argument {other:?}\n{USAGE}"),
            }
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = Options::parse(std::env::args().skip(1))?;
    let scenarios_dir = resolve_scenarios_dir(&options);
    let scenarios = load_scenarios(&scenarios_dir)?;
    let results = run_scenarios(&options, &scenarios).await?;

    let board = score(&scenarios, &results);
    let text = render_scoreboard(&board, &results);
    println!("{text}");

    if let Some(out) = &options.out {
        prepare_output_parent(out)?;
        let mut file = std::fs::File::create(out)
            .with_context(|| format!("create output file {}", out.display()))?;
        file.write_all(format!("{text}\n").as_bytes())
            .with_context(|| format!("write scoreboard to {}", out.display()))?;
    }
    Ok(())
}

/// Create the parent directory of an output path when there is one.
fn prepare_output_parent(path: &Path) -> anyhow::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display())),
        _ => Ok(()),
    }
}

/// Resolve the scenarios directory.
///
/// An explicit `--scenarios` path is used verbatim (missing ⇒ setup error).
/// The implicit default `tests/golden` resolves against the current working
/// directory first, then falls back to the repository's `tests/golden` (baked
/// in at build time) so the harness also runs from the crate directory.
fn resolve_scenarios_dir(options: &Options) -> PathBuf {
    if options.scenarios_explicit || options.scenarios_dir.is_dir() {
        return options.scenarios_dir.clone();
    }
    let repo_golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests/golden");
    if repo_golden.is_dir() {
        return repo_golden;
    }
    options.scenarios_dir.clone()
}

/// Load every `*.json` scenario under `dir`, sorted by file name.
fn load_scenarios(dir: &Path) -> anyhow::Result<Vec<Scenario>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("read scenarios directory {}", dir.display()))?;
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry of {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or_default()
            .to_string();
        files.push((name, path));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut scenarios = Vec::with_capacity(files.len());
    for (name, path) in files {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read scenario {name}"))?;
        let scenario: Scenario = serde_json::from_str(&raw)
            .with_context(|| format!("parse scenario {name} ({})", path.display()))?;
        scenarios.push(scenario);
    }
    Ok(scenarios)
}

/// Run one consult per scenario, capturing every failure as a rendered error.
async fn run_scenarios(
    options: &Options,
    scenarios: &[Scenario],
) -> anyhow::Result<Vec<ScenarioResult>> {
    // Live mode loads the configuration once; mock mode is config-free.
    let live = if options.mock {
        None
    } else {
        Some(Config::load().context("load configuration for the live provider")?)
    };

    let mut results = Vec::with_capacity(scenarios.len());
    for (index, scenario) in scenarios.iter().enumerate() {
        let input = consult_input(scenario);
        let result = match &live {
            None => {
                let min_interval_secs = MOCK_MIN_INTERVAL_SECS;
                let now_ms = now_ms_for(index, min_interval_secs);
                match scenario.mock_completion.as_deref() {
                    Some(completion) => {
                        let provider = MockProvider::canned(vec![completion.to_string()]);
                        run_consult(
                            scenario,
                            &input,
                            provider,
                            min_interval_secs,
                            mock_confidence_floor(),
                            now_ms,
                        )
                        .await
                    }
                    None => error_result(scenario, MOCK_PROVIDER, "no mock_completion".to_string()),
                }
            }
            Some(config) => {
                let min_interval_secs = config.strategy.min_interval_secs;
                let confidence_floor = config.strategy.confidence_floor;
                let now_ms = now_ms_for(index, min_interval_secs);
                match options.provider {
                    ProviderChoice::Qwen => {
                        let provider = QwenProvider::new(&config.qwen);
                        run_consult(
                            scenario,
                            &input,
                            provider,
                            min_interval_secs,
                            confidence_floor,
                            now_ms,
                        )
                        .await
                    }
                    ProviderChoice::Kimi => {
                        let provider = KimiProvider::new(&config.kimi);
                        run_consult(
                            scenario,
                            &input,
                            provider,
                            min_interval_secs,
                            confidence_floor,
                            now_ms,
                        )
                        .await
                    }
                }
            }
        };
        results.push(result);
    }
    Ok(results)
}

/// Assemble the engine input for one scenario.
fn consult_input(scenario: &Scenario) -> ConsultInput {
    ConsultInput {
        account: scenario.snapshot.clone(),
        markets: scenario.markets.clone(),
        focus_market: MarketId(scenario.focus_market_id),
        policy: scenario.policy.clone(),
        sm: scenario.sm.clone(),
        reflex: scenario.reflex.clone(),
    }
}

/// Per-scenario timestamp; `(min_interval + 1) s` steps keep the rate limiter
/// out of the way even when markets repeat between scenarios.
fn now_ms_for(index: usize, min_interval_secs: u64) -> u64 {
    1_000_000_000 + index as u64 * (min_interval_secs + 1) * 1000
}

/// Confidence floor for `--mock` runs; mirrors the
/// `STRATEGY_CONFIDENCE_FLOOR` default (0.6).
fn mock_confidence_floor() -> Decimal {
    Decimal::new(6, 1)
}

/// Run one consult and map its outcome (or its error) to a [`ScenarioResult`].
async fn run_consult<P: Provider>(
    scenario: &Scenario,
    input: &ConsultInput,
    provider: P,
    min_interval_secs: u64,
    confidence_floor: Decimal,
    now_ms: u64,
) -> ScenarioResult {
    let provider_label = provider.name();
    let Some(focus) = input
        .account
        .positions
        .iter()
        .find(|position| position.market_id == input.focus_market)
    else {
        return error_result(
            scenario,
            provider_label,
            format!(
                "focus market #{} has no position in the snapshot",
                scenario.focus_market_id
            ),
        );
    };

    let engine = StrategyEngine::new(provider, min_interval_secs, confidence_floor);
    match engine.consult(input, now_ms).await {
        Err(error) => error_result(scenario, provider_label, error.to_string()),
        Ok(outcome) => {
            let decided = action_name(outcome.decision.action);
            let prompt_text = prompts::user_prompt(&PromptInput {
                account: &input.account,
                markets: &input.markets,
                focus,
                policy: &input.policy,
                sm: &input.sm,
                reflex: &input.reflex,
                now_ms,
            });
            ScenarioResult {
                name: scenario.name.clone(),
                schema_valid: true,
                action_class_ok: scenario
                    .expected
                    .action_class
                    .iter()
                    .any(|class| class.as_str() == decided),
                grounding_ok: grounding_check(&outcome.decision.reason, &prompt_text),
                decided_action: Some(decided.to_string()),
                expected_classes: scenario.expected.action_class.clone(),
                provider: outcome.provider_used,
                latency_ms: outcome.latency_ms,
                repaired: outcome.repaired,
                error: None,
            }
        }
    }
}

/// A failed scenario row (the error is rendered, never raised).
fn error_result(scenario: &Scenario, provider: &str, error: String) -> ScenarioResult {
    ScenarioResult {
        name: scenario.name.clone(),
        schema_valid: false,
        action_class_ok: false,
        grounding_ok: false,
        decided_action: None,
        expected_classes: scenario.expected.action_class.clone(),
        provider: provider.to_string(),
        latency_ms: 0,
        repaired: false,
        error: Some(error),
    }
}

/// Wire name of a decision action (`HOLD`, `REDUCE`, ...).
fn action_name(action: DecisionAction) -> &'static str {
    match action {
        DecisionAction::Hold => "HOLD",
        DecisionAction::Reduce => "REDUCE",
        DecisionAction::Close => "CLOSE",
        DecisionAction::AddCollateral => "ADD_COLLATERAL",
        DecisionAction::Escalate => "ESCALATE",
    }
}
