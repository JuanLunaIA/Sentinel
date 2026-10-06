//! CLI argument parsing for the daemon — hand-rolled (two flags, no new deps;
//! P06 freeze: `--mode dry-run|testnet|mainnet`, `--replay <fixture.jsonl>`).
//!
//! `--replay` implies the DRY_RUN executor and a `MockPerpl` feed; the whole
//! pipeline then runs against the fixture with a logical clock (deterministic).

use std::path::PathBuf;

use sentinel_core::types::ExecutionMode;

/// Parsed command-line options for the `sentinel` daemon.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cli {
    /// Override for `EXECUTION_MODE` (`--mode <value>`).
    pub mode: Option<ExecutionMode>,
    /// Fixture path for deterministic replay (`--replay <file.jsonl>`).
    pub replay: Option<PathBuf>,
}

impl Cli {
    /// Parse arguments (the iterator must NOT include `argv[0]`).
    ///
    /// # Errors
    /// Returns a human-readable message on unknown flags, missing values, or
    /// an invalid mode string.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut cli = Cli::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--mode" => {
                    let value = it.next().ok_or("--mode requires a value")?;
                    cli.mode = Some(match value.as_str() {
                        "dry-run" => ExecutionMode::DryRun,
                        "testnet" => ExecutionMode::Testnet,
                        "mainnet" => ExecutionMode::Mainnet,
                        other => {
                            return Err(format!(
                                "invalid --mode {other:?} (expected dry-run|testnet|mainnet)"
                            ));
                        }
                    });
                }
                "--replay" => {
                    let value = it.next().ok_or("--replay requires a path")?;
                    cli.replay = Some(PathBuf::from(value));
                }
                other => {
                    return Err(format!(
                        "unknown argument {other:?} (supported: --mode, --replay)"
                    ));
                }
            }
        }
        Ok(cli)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(ToString::to_string))
    }

    #[test]
    fn parses_mode_and_replay() {
        let cli = parse(&["--mode", "dry-run", "--replay", "f.jsonl"]).unwrap();
        assert_eq!(cli.mode, Some(ExecutionMode::DryRun));
        assert_eq!(cli.replay.as_deref(), Some(std::path::Path::new("f.jsonl")));
    }

    #[test]
    fn parses_modes() {
        assert_eq!(
            parse(&["--mode", "testnet"]).unwrap().mode,
            Some(ExecutionMode::Testnet)
        );
        assert_eq!(
            parse(&["--mode", "mainnet"]).unwrap().mode,
            Some(ExecutionMode::Mainnet)
        );
    }

    #[test]
    fn empty_args_yield_default() {
        assert_eq!(parse(&[]).unwrap(), Cli::default());
    }

    #[test]
    fn rejects_unknown_flag_and_bad_values() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--mode"]).is_err());
        assert!(parse(&["--mode", "mainnet2"]).is_err());
        assert!(parse(&["--replay"]).is_err());
    }
}
