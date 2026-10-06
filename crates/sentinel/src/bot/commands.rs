//! Bot commands — parsing with EN primary + ES aliases (`SPEC-P11.md` §3).
//!
//! Parsing is pure and total: [`parse`] maps one message text to a
//! [`Command`] or `None` for anything unknown or malformed. The dispatcher
//! replies with a short help text on `None` (for allowed users); plain text
//! that does not start with `/` never parses, which keeps the typed `PAUSE`
//! confirmation reachable in `handlers::handle_text`.

use rust_decimal::Decimal;

/// One parsed bot command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `/start` onboarding card.
    Start,
    /// `/help`.
    Help,
    /// `/status` portfolio card.
    Status,
    /// `/risk [market]` — force a strategy consult now.
    Risk {
        /// Optional focus market.
        market: Option<u32>,
    },
    /// `/close <market> [fraction]`.
    Close {
        /// Market to reduce.
        market: u32,
        /// Fraction of the position (default = full close).
        fraction: Option<Decimal>,
    },
    /// `/policy` — show the overlay + caps.
    Policy,
    /// `/policy set <key> <value>`.
    PolicySet {
        /// Whitelisted key.
        key: String,
        /// Raw value string (validated by `policy_admin`).
        value: String,
    },
    /// `/approve <id>`.
    Approve {
        /// Approval id.
        id: String,
    },
    /// `/deny <id>`.
    Deny {
        /// Approval id.
        id: String,
    },
    /// `/pause` — kill switch (typed PAUSE confirmation follows).
    Pause,
    /// `/resume`.
    Resume,
    /// `/audit [n]` (default 10, cap 50).
    Audit {
        /// How many entries.
        n: usize,
    },
    /// `/spend` — Nansen x402 ledger totals.
    Spend,
    /// `/mode`.
    Mode,
}

/// Default audit depth when `/audit` is sent without an argument.
pub const AUDIT_DEFAULT: usize = 10;

/// Upper bound on the audit depth (larger requests are silently capped).
pub const AUDIT_MAX: usize = 50;

/// Parse one message text into a command (`None` for anything unrecognized
/// or malformed). EN primary plus ES aliases (ASCII, per `SPEC-P11.md` §3).
///
/// Rules (frozen): trim; require and strip exactly ONE leading `/`; the
/// command token is case-insensitive (lowercased), arguments are preserved
/// as typed; tokens are split on whitespace runs; unknown names or extra
/// tokens yield `None`.
pub fn parse(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut tokens = rest.split_whitespace();
    let head = tokens.next()?.to_ascii_lowercase();
    let args: Vec<&str> = tokens.collect();

    match head.as_str() {
        "start" | "inicio" => no_args(&args).then_some(Command::Start),
        "help" | "ayuda" => no_args(&args).then_some(Command::Help),
        "status" | "estado" => no_args(&args).then_some(Command::Status),
        "risk" | "riesgo" => risk(&args),
        "close" | "cerrar" => close(&args),
        "policy" | "politica" => policy(&args),
        "approve" | "aprobar" => single_id(&args).map(|id| Command::Approve { id }),
        "deny" | "rechazar" => single_id(&args).map(|id| Command::Deny { id }),
        "pause" | "pausa" => no_args(&args).then_some(Command::Pause),
        "resume" | "reanudar" => no_args(&args).then_some(Command::Resume),
        "audit" | "auditoria" => audit(&args),
        "spend" | "gasto" => no_args(&args).then_some(Command::Spend),
        "mode" | "modo" => no_args(&args).then_some(Command::Mode),
        _ => None,
    }
}

/// Whether a token list is empty (no-argument commands reject extras).
fn no_args(args: &[&str]) -> bool {
    args.is_empty()
}

/// `/risk [market u32]`.
fn risk(args: &[&str]) -> Option<Command> {
    match args {
        [] => Some(Command::Risk { market: None }),
        [market] => market.parse::<u32>().ok().map(|market| Command::Risk {
            market: Some(market),
        }),
        _ => None,
    }
}

/// `/close <market u32> [fraction Decimal]`.
///
/// The fraction is only parsed here; the `(0, 1]` range check belongs to
/// the handlers (confirm-card validation).
fn close(args: &[&str]) -> Option<Command> {
    match args {
        [market] => market.parse::<u32>().ok().map(|market| Command::Close {
            market,
            fraction: None,
        }),
        [market, fraction] => {
            let market = market.parse::<u32>().ok()?;
            let fraction = fraction.parse::<Decimal>().ok()?;
            Some(Command::Close {
                market,
                fraction: Some(fraction),
            })
        }
        _ => None,
    }
}

/// `/policy` and `/policy set <key> <value>` (EN + ES).
fn policy(args: &[&str]) -> Option<Command> {
    match args {
        [] => Some(Command::Policy),
        [set, key, value] if set.eq_ignore_ascii_case("set") => Some(Command::PolicySet {
            key: (*key).to_string(),
            value: (*value).to_string(),
        }),
        _ => None,
    }
}

/// Exactly one argument, preserved verbatim (`/approve`, `/deny`).
fn single_id(args: &[&str]) -> Option<String> {
    match args {
        [id] => Some((*id).to_string()),
        _ => None,
    }
}

/// `/audit [n]` — default 10, values above 50 are capped at 50, a
/// non-numeric argument is malformed.
fn audit(args: &[&str]) -> Option<Command> {
    match args {
        [] => Some(Command::Audit { n: AUDIT_DEFAULT }),
        [n] => n.parse::<usize>().ok().map(|n| Command::Audit {
            n: n.min(AUDIT_MAX),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_argument_en_commands() {
        assert_eq!(parse("/start"), Some(Command::Start));
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/status"), Some(Command::Status));
        assert_eq!(parse("/pause"), Some(Command::Pause));
        assert_eq!(parse("/resume"), Some(Command::Resume));
        assert_eq!(parse("/spend"), Some(Command::Spend));
        assert_eq!(parse("/mode"), Some(Command::Mode));
        assert_eq!(parse("/policy"), Some(Command::Policy));
    }

    #[test]
    fn parses_all_es_aliases() {
        assert_eq!(parse("/inicio"), Some(Command::Start));
        assert_eq!(parse("/ayuda"), Some(Command::Help));
        assert_eq!(parse("/estado"), Some(Command::Status));
        assert_eq!(parse("/riesgo"), Some(Command::Risk { market: None }));
        assert_eq!(
            parse("/riesgo 32"),
            Some(Command::Risk { market: Some(32) })
        );
        assert_eq!(
            parse("/cerrar 32"),
            Some(Command::Close {
                market: 32,
                fraction: None
            })
        );
        assert_eq!(
            parse("/cerrar 32 0.4"),
            Some(Command::Close {
                market: 32,
                fraction: Some(Decimal::new(4, 1))
            })
        );
        assert_eq!(parse("/politica"), Some(Command::Policy));
        assert_eq!(
            parse("/aprobar ap-1"),
            Some(Command::Approve {
                id: "ap-1".to_string()
            })
        );
        assert_eq!(
            parse("/rechazar ap-2"),
            Some(Command::Deny {
                id: "ap-2".to_string()
            })
        );
        assert_eq!(parse("/pausa"), Some(Command::Pause));
        assert_eq!(parse("/reanudar"), Some(Command::Resume));
        assert_eq!(
            parse("/auditoria"),
            Some(Command::Audit { n: AUDIT_DEFAULT })
        );
        assert_eq!(parse("/gasto"), Some(Command::Spend));
        assert_eq!(parse("/modo"), Some(Command::Mode));
        assert_eq!(
            parse("/politica set risk_hard_pct 9"),
            Some(Command::PolicySet {
                key: "risk_hard_pct".to_string(),
                value: "9".to_string()
            })
        );
    }

    #[test]
    fn command_token_is_case_insensitive_but_arguments_are_not() {
        assert_eq!(parse("/STATUS"), Some(Command::Status));
        assert_eq!(parse("/StAtUs"), Some(Command::Status));
        assert_eq!(parse("/RiSk 32"), Some(Command::Risk { market: Some(32) }));
        assert_eq!(
            parse("/POLICY SET KeyCase ValueCase"),
            Some(Command::PolicySet {
                key: "KeyCase".to_string(),
                value: "ValueCase".to_string()
            })
        );
        assert_eq!(
            parse("/APPROVE AP-9F"),
            Some(Command::Approve {
                id: "AP-9F".to_string()
            })
        );
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(parse("  /status  "), Some(Command::Status));
        assert_eq!(
            parse("\t/risk\t32\t"),
            Some(Command::Risk { market: Some(32) })
        );
        assert_eq!(
            parse(" /close   32    0.3 "),
            Some(Command::Close {
                market: 32,
                fraction: Some(Decimal::new(3, 1))
            })
        );
        assert_eq!(parse("/audit   5"), Some(Command::Audit { n: 5 }));
    }

    #[test]
    fn risk_argument_matrix() {
        assert_eq!(parse("/risk"), Some(Command::Risk { market: None }));
        assert_eq!(parse("/risk 0"), Some(Command::Risk { market: Some(0) }));
        assert_eq!(
            parse("/risk 4294967295"),
            Some(Command::Risk {
                market: Some(u32::MAX)
            })
        );
        assert_eq!(parse("/risk 4294967296"), None);
        assert_eq!(parse("/risk -1"), None);
        assert_eq!(parse("/risk abc"), None);
        assert_eq!(parse("/risk 3.2"), None);
        assert_eq!(parse("/risk 32 extra"), None);
        assert_eq!(parse("/RISK 32 EXTRA"), None);
    }

    #[test]
    fn close_argument_matrix() {
        assert_eq!(
            parse("/close 32"),
            Some(Command::Close {
                market: 32,
                fraction: None
            })
        );
        assert_eq!(
            parse("/close 32 0.3"),
            Some(Command::Close {
                market: 32,
                fraction: Some(Decimal::new(3, 1))
            })
        );
        assert_eq!(
            parse("/close 32 1"),
            Some(Command::Close {
                market: 32,
                fraction: Some(Decimal::ONE)
            })
        );
        // Parse only decides shape; the `(0, 1]` range check is handler-side.
        assert_eq!(
            parse("/close 32 1.25"),
            Some(Command::Close {
                market: 32,
                fraction: Some("1.25".parse::<Decimal>().unwrap())
            })
        );
        assert_eq!(
            parse("/close 32 0"),
            Some(Command::Close {
                market: 32,
                fraction: Some(Decimal::ZERO)
            })
        );
        assert_eq!(parse("/close"), None);
        assert_eq!(parse("/close abc"), None);
        assert_eq!(parse("/close 32 abc"), None);
        assert_eq!(parse("/close 32 0.3 extra"), None);
        assert_eq!(parse("/close -32"), None);
    }

    #[test]
    fn policy_argument_matrix() {
        assert_eq!(parse("/policy"), Some(Command::Policy));
        assert_eq!(
            parse("/policy set risk_hard_pct 9"),
            Some(Command::PolicySet {
                key: "risk_hard_pct".to_string(),
                value: "9".to_string()
            })
        );
        assert_eq!(
            parse("/policy set max_order_size_usd 2500"),
            Some(Command::PolicySet {
                key: "max_order_size_usd".to_string(),
                value: "2500".to_string()
            })
        );
        assert_eq!(
            parse("/policy SET k v"),
            Some(Command::PolicySet {
                key: "k".to_string(),
                value: "v".to_string()
            })
        );
        assert_eq!(parse("/policy set k"), None);
        assert_eq!(parse("/policy set"), None);
        assert_eq!(parse("/policy set k v w"), None);
        assert_eq!(parse("/policy get"), None);
        assert_eq!(parse("/policy extra"), None);
    }

    #[test]
    fn approve_deny_matrix() {
        assert_eq!(
            parse("/approve ap-1234abcd"),
            Some(Command::Approve {
                id: "ap-1234abcd".to_string()
            })
        );
        assert_eq!(
            parse("/deny ap-1234abcd"),
            Some(Command::Deny {
                id: "ap-1234abcd".to_string()
            })
        );
        assert_eq!(parse("/approve"), None);
        assert_eq!(parse("/deny"), None);
        assert_eq!(parse("/approve ap-1 ap-2"), None);
        assert_eq!(parse("/deny x y z"), None);
    }

    #[test]
    fn audit_matrix_defaults_caps_and_rejects_non_numeric() {
        assert_eq!(parse("/audit"), Some(Command::Audit { n: 10 }));
        assert_eq!(parse("/audit 1"), Some(Command::Audit { n: 1 }));
        assert_eq!(parse("/audit 10"), Some(Command::Audit { n: 10 }));
        assert_eq!(parse("/audit 50"), Some(Command::Audit { n: 50 }));
        assert_eq!(parse("/audit 51"), Some(Command::Audit { n: 50 }));
        assert_eq!(parse("/audit 9999"), Some(Command::Audit { n: 50 }));
        // The spec names only a default and a cap; a numeric 0 passes through.
        assert_eq!(parse("/audit 0"), Some(Command::Audit { n: 0 }));
        assert_eq!(parse("/audit abc"), None);
        assert_eq!(parse("/audit -1"), None);
        assert_eq!(parse("/audit 2.5"), None);
        assert_eq!(parse("/audit 5 extra"), None);
    }

    #[test]
    fn no_argument_commands_reject_extra_tokens() {
        for text in [
            "/start x",
            "/help x",
            "/status x",
            "/policy x",
            "/pause x",
            "/resume x",
            "/spend x",
            "/mode x",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn unknown_and_malformed_text_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("/"), None);
        assert_eq!(parse("//status"), None);
        assert_eq!(parse("///risk 32"), None);
        assert_eq!(parse("status"), None);
        assert_eq!(parse("/status@sentinelbot"), None);
        assert_eq!(parse("/estadó"), None);
        assert_eq!(parse("/ｓｔａｔｕｓ"), None);
        assert_eq!(parse("/unknown"), None);
        assert_eq!(parse("hola"), None);
    }

    #[test]
    fn plain_text_never_parses_so_typed_confirmations_reach_handle_text() {
        // The /pause flow: the confirmation arrives as a plain message and
        // must stay text (handlers::handle_text owns it) — never a command.
        assert_eq!(parse("PAUSE"), None);
        assert_eq!(parse("pause"), None);
        assert_eq!(parse("Pause"), None);
    }
}
