//! Backtester CLI (SPEC-P13 §7): replays scenario files through the real
//! decision path ([`sentinel::sim::engine`]) and writes the JSON + Markdown
//! report pair.
//!
//! Usage:
//!
//! ```text
//! backtest [--scenario <file|all>] [--scenarios-dir <dir>]
//!          [--mode replay|live-brain] [--out <base>]
//! ```
//!
//! Defaults: `--scenario all`, `--scenarios-dir tests/scenarios`,
//! `--mode replay`, `--out docs/backtest-report` (writes `<base>.json` and
//! `<base>.md`).
//!
//! Exit codes (SPEC-P13 §7): `0` ok; `2` missing key/config (e.g. live-brain
//! without `QWEN_API_KEY`, unreadable scenarios directory, unknown flag);
//! `1` internal error (scenario load/run/report failure).
//!
//! Replay mode is fully offline and deterministic: it never reads the wall
//! clock, never touches the network, and writes no timestamps — the artifact
//! JSON carries the per-scenario determinism view (metrics zeroed, SPEC-P13
//! §6 excludes the latency percentiles from the determinism byte-compare).
//!
//! `--mode live-brain` (SPEC-P13 §7) is reserved for the curated
//! [`LIVE_BRAIN_CURATED`] subset driven through real Qwen consults at the
//! engine's consult points; the `sentinel::sim` engine currently exposes only
//! the deterministic replay entry point, so a keyed run reports the missing
//! integration hook as an internal error (exit 1) instead of faking it.

use std::path::PathBuf;
use std::process::ExitCode;

use sentinel::sim::report::SimReport;
use sentinel::sim::{engine, report, scenario};

/// Default scenarios directory (SPEC-P13 §7).
const DEFAULT_SCENARIOS_DIR: &str = "tests/scenarios";
/// Default output base; the CLI appends `.json` + `.md` (SPEC-P13 §7).
const DEFAULT_OUT: &str = "docs/backtest-report";
/// Curated 3-scenario subset for `--mode live-brain` (SPEC-P13 §7): a crash, a
/// recovery and a data-quality scenario.
const LIVE_BRAIN_CURATED: [&str; 3] = ["flash-crash-30", "recovery-v", "stale-feed-outage"];

/// Engine mode selected by `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Deterministic offline replay (default).
    Replay,
    /// Real Qwen consults at the engine's consult points (requires a key).
    LiveBrain,
}

/// Parsed command-line options (hand-rolled, no new dependencies).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    /// `all` or a scenario file/name (`--scenario`, default `all`).
    scenario: String,
    /// Directory holding the scenario suite (`--scenarios-dir`).
    scenarios_dir: PathBuf,
    /// Engine mode (`--mode`).
    mode: Mode,
    /// Output base path; `.json` and `.md` are appended (`--out`).
    out: PathBuf,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            scenario: "all".to_string(),
            scenarios_dir: PathBuf::from(DEFAULT_SCENARIOS_DIR),
            mode: Mode::Replay,
            out: PathBuf::from(DEFAULT_OUT),
        }
    }
}

impl Cli {
    /// Parse arguments (the iterator must NOT include `argv[0]`).
    ///
    /// # Errors
    /// Human-readable message on unknown flags, missing values or an invalid
    /// `--mode` string.
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut cli = Cli::default();
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--scenario" => {
                    let value = iter.next().ok_or("--scenario requires a file or \"all\"")?;
                    cli.scenario = value;
                }
                "--scenarios-dir" => {
                    let value = iter.next().ok_or("--scenarios-dir requires a path")?;
                    cli.scenarios_dir = PathBuf::from(value);
                }
                "--mode" => {
                    let value = iter.next().ok_or("--mode requires a value")?;
                    cli.mode = match value.as_str() {
                        "replay" => Mode::Replay,
                        "live-brain" => Mode::LiveBrain,
                        other => {
                            return Err(format!(
                                "invalid --mode {other:?} (expected replay|live-brain)"
                            ));
                        }
                    };
                }
                "--out" => {
                    let value = iter.next().ok_or("--out requires a path base")?;
                    cli.out = PathBuf::from(value);
                }
                other => {
                    return Err(format!(
                        "unknown argument {other:?} \
                         (supported: --scenario, --scenarios-dir, --mode, --out)"
                    ));
                }
            }
        }
        Ok(cli)
    }
}

/// Fatal CLI failure, mapped to the SPEC-P13 §7 exit codes.
#[derive(Debug)]
enum Failure {
    /// Missing key/config; exit code 2.
    Config(String),
    /// Internal error; exit code 1.
    Internal(String),
}

fn main() -> ExitCode {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("backtest: {message}");
            return ExitCode::from(2);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Config(message)) => {
            eprintln!("backtest: {message}");
            ExitCode::from(2)
        }
        Err(Failure::Internal(message)) => {
            eprintln!("backtest: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch on the selected mode.
fn run(cli: &Cli) -> Result<(), Failure> {
    match cli.mode {
        Mode::Replay => run_replay(cli),
        Mode::LiveBrain => run_live_brain(cli),
    }
}

/// Offline replay: load every selected scenario, run it, write the reports.
fn run_replay(cli: &Cli) -> Result<(), Failure> {
    let paths = resolve_paths(cli)?;
    let mut reports: Vec<SimReport> = Vec::with_capacity(paths.len());
    for path in &paths {
        let scenario = scenario::load(path)
            .map_err(|err| Failure::Internal(format!("load {}: {err}", path.display())))?;
        let report = engine::run(&scenario)
            .map_err(|err| Failure::Internal(format!("run {}: {err}", scenario.id)))?;
        println!(
            "{} [{}]: ticks={} saved=${} baseline_liq={} sentinel_liq={} \
             avoided={} false_positives={} policy_violations={}",
            report.scenario_id,
            report.label,
            report.ticks,
            report.capital_saved_usd,
            report.baseline_liquidations,
            report.sentinel_liquidations,
            report.liquidations_avoided,
            report.false_positive_reduces,
            report.policy_violations,
        );
        reports.push(report);
    }
    write_outputs(cli, &reports)
}

/// `--mode live-brain` (SPEC-P13 §7): requires `QWEN_API_KEY` (exit 2 when
/// absent); the actual consult hook in the sim engine is not wired yet, so a
/// keyed run fails loudly instead of silently degrading to replay.
fn run_live_brain(cli: &Cli) -> Result<(), Failure> {
    let key_present = std::env::var("QWEN_API_KEY")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !key_present {
        return Err(Failure::Config(
            "live-brain requires QWEN_API_KEY (SPEC-P13 §7); replay mode stays offline".to_string(),
        ));
    }
    // Resolve the curated subset so a misconfigured directory still fails as
    // config before the pending-hook error.
    let curated: Vec<PathBuf> = LIVE_BRAIN_CURATED
        .iter()
        .map(|name| cli.scenarios_dir.join(format!("{name}.json")))
        .collect();
    for path in &curated {
        if !path.is_file() {
            return Err(Failure::Config(format!(
                "live-brain curated scenario missing: {}",
                path.display()
            )));
        }
    }
    Err(Failure::Internal(
        "live-brain consult hook is not wired in sentinel::sim::engine yet \
         (P13 sim-core); run --mode replay for the deterministic suite"
            .to_string(),
    ))
}

/// Resolve the scenario files to replay, deterministically sorted.
fn resolve_paths(cli: &Cli) -> Result<Vec<PathBuf>, Failure> {
    if cli.scenario == "all" {
        let entries = std::fs::read_dir(&cli.scenarios_dir).map_err(|err| {
            Failure::Config(format!(
                "scenarios dir {}: {err}",
                cli.scenarios_dir.display()
            ))
        })?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                Failure::Config(format!(
                    "scenarios dir {}: {err}",
                    cli.scenarios_dir.display()
                ))
            })?;
            let path = entry.path();
            let is_json = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
            if is_json && path.is_file() {
                paths.push(path);
            }
        }
        // Byte-order sort: locale-independent (LC_ALL=C determinism).
        paths.sort();
        if paths.is_empty() {
            return Err(Failure::Config(format!(
                "no *.json scenarios found under {}",
                cli.scenarios_dir.display()
            )));
        }
        Ok(paths)
    } else {
        let given = PathBuf::from(&cli.scenario);
        if given.is_file() {
            return Ok(vec![given]);
        }
        // Bare scenario id: look it up under the scenarios directory.
        let bare = cli.scenario.strip_suffix(".json").unwrap_or(&cli.scenario);
        let candidate = cli.scenarios_dir.join(format!("{bare}.json"));
        if candidate.is_file() {
            return Ok(vec![candidate]);
        }
        Err(Failure::Config(format!(
            "scenario {:?} not found (tried {} and {})",
            cli.scenario,
            given.display(),
            candidate.display()
        )))
    }
}

/// Aggregate + render, then write `<out>.json` and `<out>.md`.
fn write_outputs(cli: &Cli, reports: &[SimReport]) -> Result<(), Failure> {
    let aggregate = report::aggregate(reports);
    let markdown = report::render_md(reports, &aggregate);

    if let Some(parent) = cli.out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| Failure::Internal(format!("create {}: {err}", parent.display())))?;
    }

    // Artifact determinism (SPEC-P13 §6): metrics are timing-dependent and
    // excluded from the byte-compare, so artifacts carry the determinism view.
    let scenarios_json: Vec<serde_json::Value> = reports
        .iter()
        .map(|report| serde_json::to_value(report.determinism_view()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Failure::Internal(format!("serialize scenario reports: {err}")))?;
    let aggregate_json = serde_json::to_value(&aggregate)
        .map_err(|err| Failure::Internal(format!("serialize aggregate: {err}")))?;
    let document = serde_json::json!({
        "aggregate": aggregate_json,
        "scenarios": scenarios_json,
    });
    let json_text = serde_json::to_string_pretty(&document)
        .map_err(|err| Failure::Internal(format!("serialize report json: {err}")))?;

    let json_path = with_extension(&cli.out, "json");
    let md_path = with_extension(&cli.out, "md");
    std::fs::write(&json_path, format!("{json_text}\n"))
        .map_err(|err| Failure::Internal(format!("write {}: {err}", json_path.display())))?;
    std::fs::write(&md_path, markdown)
        .map_err(|err| Failure::Internal(format!("write {}: {err}", md_path.display())))?;

    println!(
        "aggregate: {} scenarios, notional ${}, saved ${} ({}%), \
         baseline liquidations: {} -> with Sentinel: {}",
        aggregate.scenarios,
        aggregate.total_notional_usd,
        aggregate.total_saved_usd,
        aggregate.pct_saved,
        aggregate.baseline_liquidations,
        aggregate.sentinel_liquidations,
    );
    println!("wrote {} and {}", json_path.display(), md_path.display());
    Ok(())
}

/// Replace or append the extension of `base` (`docs/report` + `json` →
/// `docs/report.json`; `report.md` + `json` → `report.json`).
fn with_extension(base: &std::path::Path, extension: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    path.set_extension(extension);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(ToString::to_string))
    }

    #[test]
    fn defaults_match_spec_p13_7() {
        let cli = Cli::default();
        assert_eq!(cli.scenario, "all");
        assert_eq!(cli.scenarios_dir, PathBuf::from("tests/scenarios"));
        assert_eq!(cli.mode, Mode::Replay);
        assert_eq!(cli.out, PathBuf::from("docs/backtest-report"));
        assert_eq!(parse(&[]).expect("empty"), cli);
    }

    #[test]
    fn parses_all_documented_flags() {
        let cli = parse(&[
            "--scenario",
            "flash-crash-30.json",
            "--scenarios-dir",
            "custom/dir",
            "--mode",
            "live-brain",
            "--out",
            "/tmp/report",
        ])
        .expect("parses");
        assert_eq!(cli.scenario, "flash-crash-30.json");
        assert_eq!(cli.scenarios_dir, PathBuf::from("custom/dir"));
        assert_eq!(cli.mode, Mode::LiveBrain);
        assert_eq!(cli.out, PathBuf::from("/tmp/report"));
    }

    #[test]
    fn rejects_unknown_flags_and_values() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--scenario"]).is_err());
        assert!(parse(&["--mode", "live"]).is_err());
        assert!(parse(&["--out"]).is_err());
    }

    #[test]
    fn with_extension_replaces_or_appends() {
        assert_eq!(
            with_extension(std::path::Path::new("docs/backtest-report"), "json"),
            PathBuf::from("docs/backtest-report.json")
        );
        assert_eq!(
            with_extension(std::path::Path::new("out.md"), "md"),
            PathBuf::from("out.md")
        );
    }

    #[test]
    fn resolve_paths_sorts_the_json_suite() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["b.json", "a.json", "notes.txt"] {
            std::fs::write(dir.path().join(name), "{}").expect("write");
        }
        let cli = Cli {
            scenarios_dir: dir.path().to_path_buf(),
            ..Cli::default()
        };
        let paths = resolve_paths(&cli).expect("resolve");
        let names: Vec<String> = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.json", "b.json"], "sorted, txt excluded");
    }
}
