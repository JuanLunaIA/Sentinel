//! Bot commands — parsing with EN primary + ES aliases (`SPEC-P11.md` §3).
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

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

/// Parse one message text into a command (`None` for anything unrecognized
/// or malformed). EN primary plus ES aliases (ASCII, per `SPEC-P11.md` §3).
pub fn parse(_text: &str) -> Option<Command> {
    todo!("P11 agent bot-frame")
}
