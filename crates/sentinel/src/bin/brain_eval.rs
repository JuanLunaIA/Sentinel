//! `brain_eval` — score the strategy brain over golden scenarios.
//!
//! Usage (`SPEC-P07.md` §7; provider chain per `SPEC-P08.md` §5):
//!   `cargo run --bin brain_eval -- [--scenarios tests/golden] [--mock]
//!    [--provider qwen|kimi] [--out docs/evidence/p08-brain-eval.txt]`
//!
//! `--mock` replays each scenario's canned `mock_completion` through the real
//! engine (no network). Live mode builds the configured provider chain — Qwen
//! primary, Kimi fallback, consult/token budget armed — or a single provider
//! when `--provider qwen|kimi` is passed explicitly.
//!
//! `FORCE_PROVIDER_FAIL=<name>` (environment, optional) force-fails the named
//! provider without a call (`with_forced_failure`); a name matching no provider
//! is a no-op. In `--mock` mode a set value builds an equivalent mock chain —
//! a force-failed primary plus the scenario's canned completion on the `kimi`
//! fallback — so the failover machinery is demoed offline and every row
//! reports `provider=kimi`.
//!
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

/// Environment variable naming a provider to force-fail (no network call).
const FORCE_PROVIDER_FAIL_ENV: &str = "FORCE_PROVIDER_FAIL";

/// Minimum consult interval assumed in `--mock` runs, seconds; mirrors the
/// `STRATEGY_MIN_INTERVAL_SECS` default. A fresh engine per scenario means the
/// rate limiter cannot interfere anyway; the same `now_ms` formula as live
/// mode is kept.
const MOCK_MIN_INTERVAL_SECS: u64 = 120;

/// Fallback provider name of the mock chain (`SPEC-P08.md` §5).
const MOCK_FALLBACK_NAME: &str = "kimi";

/// Provider label rendered on single-provider mock rows that carry no outcome
/// (the P07 `MockProvider::canned` identity).
const MOCK_PROVIDER: &str = "mock";

/// Which live provider to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderChoice {
    /// Qwen (chain primary).
    Qwen,
    /// Kimi (chain fallback; pulled forward for P07).
    Kimi,
}

impl ProviderChoice {
    /// Provider name as the provider implementations themselves report it.
    fn name(self) -> &'static str {
        match self {
            ProviderChoice::Qwen => "qwen",
            ProviderChoice::Kimi => "kimi",
        }
    }
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
    /// Live provider to build (ignored with `--mock` unless explicit; the
    /// implicit default is the full Qwen→Kimi chain).
    provider: ProviderChoice,
    /// Whether `--provider` was passed explicitly: selects that single
    /// provider with no chain (live and mock mode alike).
    provider_explicit: bool,
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
            provider_explicit: false,
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
                    options.provider_explicit = true;
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

/// `FORCE_PROVIDER_FAIL` (environment), when set to a non-empty value.
///
/// The mock chain needs the name as a `&'static str` (`MockProvider::named`,
/// `SPEC-P08.md` §4) while the env value is runtime data; the harness is a
/// short-lived process, so leaking the one small name per run is the honest
/// bridge. The name is compared by equality against each provider slot, so a
/// value matching no provider is a no-op.
fn forced_failure_from_env() -> Option<&'static str> {
    std::env::var(FORCE_PROVIDER_FAIL_ENV)
        .ok()
        .filter(|name| !name.is_empty())
        .map(|name| &*Box::leak(name.into_boxed_str()))
}

/// Apply a forced failure, when configured, to a freshly built engine.
fn apply_forced_failure<P: Provider, F: Provider>(
    engine: StrategyEngine<P, F>,
    forced_failure: Option<&str>,
) -> StrategyEngine<P, F> {
    match forced_failure {
        Some(name) => engine.with_forced_failure(name),
        None => engine,
    }
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
    let forced_failure = forced_failure_from_env();

    let mut results = Vec::with_capacity(scenarios.len());
    for (index, scenario) in scenarios.iter().enumerate() {
        let input = consult_input(scenario);
        let result = match &live {
            None => {
                let min_interval_secs = MOCK_MIN_INTERVAL_SECS;
                let now_ms = now_ms_for(index, min_interval_secs);
                match scenario.mock_completion.as_deref() {
                    Some(completion) => {
                        run_mock_scenario(
                            scenario,
                            &input,
                            completion,
                            options.provider,
                            options.provider_explicit,
                            forced_failure,
                            now_ms,
                        )
                        .await
                    }
                    None => error_result(scenario, MOCK_PROVIDER, "no mock_completion".to_string()),
                }
            }
            Some(config) => {
                let now_ms = now_ms_for(index, config.strategy.min_interval_secs);
                run_live_scenario(
                    scenario,
                    &input,
                    config,
                    options.provider,
                    options.provider_explicit,
                    forced_failure,
                    now_ms,
                )
                .await
            }
        };
        results.push(result);
    }
    Ok(results)
}

/// Run one `--mock` scenario through the right mock engine shape.
///
/// Without `FORCE_PROVIDER_FAIL` this is the P07 single canned provider
/// (named `mock`, or the explicitly chosen provider's name). With it, the
/// mock chain mirrors the live shape: a force-failed primary (empty queue —
/// the forced failure short-circuits, so it is never called) plus the
/// scenario's canned completion on the `kimi` fallback, so every row reports
/// `provider=kimi` (`SPEC-P08.md` §5). A fresh engine is built per scenario
/// because a consult consumes its single canned completion.
async fn run_mock_scenario(
    scenario: &Scenario,
    input: &ConsultInput,
    completion: &str,
    choice: ProviderChoice,
    choice_explicit: bool,
    forced_failure: Option<&'static str>,
    now_ms: u64,
) -> ScenarioResult {
    match forced_failure {
        Some(name) => {
            let primary = MockProvider::named(name);
            let fallback =
                MockProvider::canned_named(MOCK_FALLBACK_NAME, vec![completion.to_string()]);
            let engine =
                StrategyEngine::new(primary, MOCK_MIN_INTERVAL_SECS, mock_confidence_floor())
                    .with_fallback(fallback)
                    .with_forced_failure(name);
            run_consult(scenario, input, engine, name, now_ms).await
        }
        None if choice_explicit => {
            let provider = MockProvider::canned_named(choice.name(), vec![completion.to_string()]);
            let engine =
                StrategyEngine::new(provider, MOCK_MIN_INTERVAL_SECS, mock_confidence_floor());
            run_consult(scenario, input, engine, choice.name(), now_ms).await
        }
        None => {
            let provider = MockProvider::canned(vec![completion.to_string()]);
            let engine =
                StrategyEngine::new(provider, MOCK_MIN_INTERVAL_SECS, mock_confidence_floor());
            run_consult(scenario, input, engine, MOCK_PROVIDER, now_ms).await
        }
    }
}

/// Run one live scenario against the configured provider chain — Qwen primary
/// with the Kimi fallback, consult/token budget armed — or, when `--provider`
/// was passed explicitly, that single provider (no chain, same budget).
///
/// The engine is built fresh per scenario (as in P07); each scenario consults
/// a different market at a later `now_ms`, so breakers never span scenarios.
async fn run_live_scenario(
    scenario: &Scenario,
    input: &ConsultInput,
    config: &Config,
    choice: ProviderChoice,
    choice_explicit: bool,
    forced_failure: Option<&'static str>,
    now_ms: u64,
) -> ScenarioResult {
    let min_interval_secs = config.strategy.min_interval_secs;
    let confidence_floor = config.strategy.confidence_floor;
    let max_consults = config.strategy.max_consults_per_hour;
    let max_tokens = config.strategy.max_tokens_per_day;

    if choice_explicit {
        match choice {
            ProviderChoice::Qwen => {
                let provider = QwenProvider::new(&config.qwen);
                let engine = StrategyEngine::new(provider, min_interval_secs, confidence_floor)
                    .with_budget(max_consults, max_tokens);
                let engine = apply_forced_failure(engine, forced_failure);
                run_consult(scenario, input, engine, choice.name(), now_ms).await
            }
            ProviderChoice::Kimi => {
                let provider = KimiProvider::new(&config.kimi);
                let engine = StrategyEngine::new(provider, min_interval_secs, confidence_floor)
                    .with_budget(max_consults, max_tokens);
                let engine = apply_forced_failure(engine, forced_failure);
                run_consult(scenario, input, engine, choice.name(), now_ms).await
            }
        }
    } else {
        let primary = QwenProvider::new(&config.qwen);
        let fallback = KimiProvider::new(&config.kimi);
        let engine = StrategyEngine::new(primary, min_interval_secs, confidence_floor)
            .with_fallback(fallback)
            .with_budget(max_consults, max_tokens);
        let engine = apply_forced_failure(engine, forced_failure);
        run_consult(scenario, input, engine, ProviderChoice::Qwen.name(), now_ms).await
    }
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
///
/// `provider_label` is what the row's `provider` field reads when the consult
/// fails without an outcome (chains report their primary); successful rows
/// report the provider that produced the winning decision (`provider_used`).
async fn run_consult<P: Provider, F: Provider>(
    scenario: &Scenario,
    input: &ConsultInput,
    engine: StrategyEngine<P, F>,
    provider_label: &str,
    now_ms: u64,
) -> ScenarioResult {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden directory (repo root `tests/golden`), the same resolution as the
    /// other brain suites.
    fn golden_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
    }

    /// Every golden scenario driven through the mock failover chain (the
    /// `FORCE_PROVIDER_FAIL=qwen` shape: force-failed primary, canned `kimi`
    /// fallback) must produce a successful row reporting `provider=kimi` —
    /// the data-level form of the offline failover demo (`SPEC-P08.md` §5).
    #[tokio::test]
    async fn mock_chain_forced_primary_reports_kimi_on_every_golden_row() {
        let scenarios = load_scenarios(&golden_dir()).expect("golden scenarios load");
        assert_eq!(scenarios.len(), 14, "14 golden scenarios");
        for (index, scenario) in scenarios.iter().enumerate() {
            let completion = scenario
                .mock_completion
                .as_deref()
                .expect("golden scenario carries a mock_completion");
            let input = consult_input(scenario);
            let now_ms = now_ms_for(index, MOCK_MIN_INTERVAL_SECS);
            let result = run_mock_scenario(
                scenario,
                &input,
                completion,
                ProviderChoice::Qwen,
                false,
                Some("qwen"),
                now_ms,
            )
            .await;
            assert!(
                result.error.is_none(),
                "{}: failover chain must succeed, got {:?}",
                scenario.name,
                result.error
            );
            assert_eq!(
                result.provider, "kimi",
                "{}: every failover row reports the kimi fallback",
                scenario.name
            );
        }
    }

    /// Without `FORCE_PROVIDER_FAIL` the plain mock run keeps the P07
    /// `provider=mock` identity, and an explicit `--provider kimi` selects the
    /// single named provider (no chain) — both on every golden row.
    #[tokio::test]
    async fn mock_single_provider_rows_report_mock_or_the_chosen_name() {
        let scenarios = load_scenarios(&golden_dir()).expect("golden scenarios load");
        assert_eq!(scenarios.len(), 14, "14 golden scenarios");
        for (index, scenario) in scenarios.iter().enumerate() {
            let completion = scenario
                .mock_completion
                .as_deref()
                .expect("golden scenario carries a mock_completion");
            let input = consult_input(scenario);
            let now_ms = now_ms_for(index, MOCK_MIN_INTERVAL_SECS);

            let plain = run_mock_scenario(
                scenario,
                &input,
                completion,
                ProviderChoice::Qwen,
                false,
                None,
                now_ms,
            )
            .await;
            assert_eq!(
                plain.provider, "mock",
                "{}: plain mock rows keep provider=mock",
                scenario.name
            );

            let explicit = run_mock_scenario(
                scenario,
                &input,
                completion,
                ProviderChoice::Kimi,
                true,
                None,
                now_ms,
            )
            .await;
            assert_eq!(
                explicit.provider, "kimi",
                "{}: explicit --provider kimi rows report kimi",
                scenario.name
            );
        }
    }
}
