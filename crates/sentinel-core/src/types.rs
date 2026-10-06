//! Domain types shared across Sentinel.
//!
//! These types are the contract between the perception layer
//! (`sentinel::perpl`), the reflex engine ([`crate::risk`]), the policy gate
//! ([`crate::policy`]) and the audit journal ([`crate::audit`]).
//!
//! Implementations never leak raw exchange JSON past the perception module —
//! everything is converted into these types at the boundary, in **real units**
//! (no scaled integers, no venue-specific encodings). Decimal values carry the
//! precision of the source; scale conversions are documented per field.

use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Market identifier as used by Perpl (`market_id` in the REST/WS APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MarketId(pub u32);

/// Exchange account identifier (on-chain `accountId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub u64);

/// Static market description resolved from `GET /v1/pub/context`.
///
/// Margin fields are stored as **fractions** (not Perpl's leverage-hundredths
/// encoding): the raw API values `initial_margin` / `maintenance_margin` are
/// in hundredths of a leverage multiple, so `1200 -> 12x -> 0.08333…` and
/// `2000 -> 20x -> 0.05` (see `docs/FACTS.md` §1.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Market {
    /// Perpl market id (e.g. `32` = ETH on testnet, `20` = ETH on mainnet).
    pub id: MarketId,
    /// Human-readable symbol (e.g. `ETH`). Note: BTC/MON on mainnet carry an
    /// empty `symbol` field in the venue payload; the mapping falls back to
    /// `base` there.
    pub symbol: String,
    /// Base asset name from the venue payload (e.g. `ETH`, `ETH Perp`).
    pub base: String,
    /// Price scaling exponent: `price = raw / 10^price_decimals`.
    pub price_decimals: u32,
    /// Size scaling exponent: `size = raw / 10^size_decimals`.
    pub size_decimals: u32,
    /// Initial-margin fraction (e.g. `0.08333…` for Perpl ETH `1200`).
    pub initial_margin_fraction: Decimal,
    /// Maintenance-margin fraction (e.g. `0.05` for Perpl ETH `2000`).
    pub maintenance_margin_fraction: Decimal,
    /// Maximum leverage allowed by the venue (`initial_margin / 100` in Perpl's
    /// leverage-hundredths encoding; e.g. `12` for ETH).
    pub max_leverage: Decimal,
    /// Minimum order size in base units (0 when the venue reports no minimum).
    pub min_size: Decimal,
    /// Price tick in price units (`10^-price_decimals`).
    pub tick_size: Decimal,
    /// Base-tier maker fee in micros (`1e-6` of notional; per-market schedule).
    pub maker_fee_micros: u64,
    /// Base-tier taker fee in micros (`1e-6` of notional; per-market schedule).
    pub taker_fee_micros: u64,
    /// Order time-to-live in blocks: `lb` ceiling offset from head.
    pub order_ttl_blocks: u64,
}

/// A live position on one market (Perpl uses **isolated margin**: each
/// position carries its own collateral; free account balance does not protect
/// it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Market this position belongs to.
    pub market_id: MarketId,
    /// Display symbol copied from the market.
    pub symbol: String,
    /// Signed size in base units: positive = long, negative = short.
    pub size: Decimal,
    /// Volume-weighted entry price (collateral per base unit).
    pub entry_price: Decimal,
    /// Last observed mark price for the market, if known.
    pub mark_price: Option<Decimal>,
    /// Exchange-provided liquidation price when the venue exposes one.
    /// Perpl's gateway API does **not** expose it (`None` there); the risk
    /// engine derives it from first principles (see `docs/FACTS.md` §1.7).
    pub liq_price: Option<Decimal>,
    /// Collateral locked in this position (isolated margin), collateral units.
    pub collateral: Decimal,
    /// Unrealized PnL as reported or derived, collateral units.
    pub unrealized_pnl: Decimal,
    /// Margin ratio (position equity / notional) when derivable.
    pub margin_ratio: Option<Decimal>,
    /// Leverage in effect (e.g. `5` = 5x).
    pub leverage: Decimal,
    /// When the position was opened, if known.
    pub opened_at: Option<DateTime<Utc>>,
}

impl Position {
    /// Notional value of the position using mark price when available,
    /// falling back to entry price. Always non-negative.
    pub fn notional(&self) -> Decimal {
        let price = self.mark_price.unwrap_or(self.entry_price);
        (self.size * price).abs()
    }

    /// Whether this is a long position.
    pub fn is_long(&self) -> bool {
        self.size > Decimal::ZERO
    }
}

/// Full account snapshot: positions plus balances, as of `snapshot_ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    /// Open positions. Per-position collateral lives on each [`Position`].
    pub positions: Vec<Position>,
    /// Free (unlocked) balance, collateral units.
    pub free_balance: Decimal,
    /// Account equity, collateral units.
    pub equity: Decimal,
    /// Fee tier index (`Account.ft`), used to pick the right fee-schedule entry.
    pub fee_tier: u32,
    /// Source timestamp of the snapshot (when the venue emitted it).
    pub snapshot_ts: DateTime<Utc>,
}

/// Risk tier produced by the deterministic reflex engine.
///
/// Ordered from safest to most severe so comparisons like `tier >= RiskTier::Orange`
/// express escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskTier {
    /// Comfortable distance to liquidation.
    Green,
    /// Worth watching; strategy consult may be scheduled.
    Yellow,
    /// De-risk soon.
    Orange,
    /// Immediate deterministic action required.
    Red,
}

/// Freshness of the data a decision is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataQuality {
    /// Feed is fresh within configured bounds.
    Fresh,
    /// Feed is stale by `secs` seconds.
    Stale {
        /// Seconds since the last accepted update.
        secs: u64,
    },
    /// No data at all.
    Missing,
}

/// A defensive action the system may take. Intents **never increase
/// exposure**: they reduce, close, add collateral, or alert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Intent {
    /// Reduce the position by a fraction of its current size.
    Reduce {
        /// Fraction of current size to remove, in `(0, 1]`.
        fraction: Decimal,
        /// Human-readable trigger that produced this intent.
        reason: String,
    },
    /// Close the position entirely (reduce-only close order).
    Close {
        /// Human-readable trigger that produced this intent.
        reason: String,
    },
    /// Add collateral to the position (isolated margin).
    AddCollateral {
        /// Amount in collateral units.
        amount: Decimal,
        /// Human-readable trigger that produced this intent.
        reason: String,
    },
    /// Informational alert; no automatic action attached.
    Alert {
        /// Human-readable alert text.
        message: String,
    },
}

/// Policy gate verdict for an [`Intent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PolicyVerdict {
    /// Allowed to execute as requested.
    Allow,
    /// Denied; execution must not proceed.
    Deny {
        /// Why the intent was denied.
        reason: String,
    },
    /// Requires human approval (e.g. Telegram inline approval) before execution.
    NeedsApproval {
        /// Why approval is required.
        reason: String,
    },
}

/// Execution mode of the daemon (see `.env.example`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    /// Simulate fills locally; no orders leave the process. Default mode.
    DryRun,
    /// Live orders on Monad **testnet**.
    Testnet,
    /// Live orders on Monad **mainnet**. Gated by an explicit acknowledgement
    /// variable at configuration load (see `sentinel::config`).
    Mainnet,
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ExecutionMode::DryRun => "DRY_RUN",
            ExecutionMode::Testnet => "TESTNET",
            ExecutionMode::Mainnet => "MAINNET",
        })
    }
}

/// Action classes of the strategy-brain decision schema v3 (`SPEC-P07.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionAction {
    /// Keep the position as-is.
    Hold,
    /// Reduce exposure by `amount` (base units).
    Reduce,
    /// Close the position entirely.
    Close,
    /// Add `amount` collateral to the isolated position.
    AddCollateral,
    /// Needs a human (below confidence floor, ambiguous, or above policy).
    Escalate,
}

/// Operational urgency of a strategy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Urgency {
    /// No time pressure.
    Routine,
    /// Act soon.
    Elevated,
    /// Act now.
    Critical,
}

/// Strategy-brain decision (schema v3). `amount` is a decimal **string** on
/// the wire (`"1.25"` or `null`) and is required (`> 0`) for `Reduce`/
/// `ADD_COLLATERAL`; unknown JSON fields are tolerated, missing required
/// fields are a typed error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// What the model wants to do.
    pub action: DecisionAction,
    /// Market the decision concerns.
    pub market_id: u32,
    /// Amount in base units (required for `Reduce`/`AddCollateral`).
    #[serde(default)]
    pub amount: Option<Decimal>,
    /// Model confidence in `[0, 1]`.
    pub confidence: Decimal,
    /// How urgent the action is.
    pub urgency: Urgency,
    /// Short rationale grounded in the provided data.
    pub reason: String,
}

impl Decision {
    /// Range/business validation against the consulted snapshot's markets.
    ///
    /// # Errors
    /// Static description of the first violated rule (`SPEC-P07.md` §2).
    pub fn validate(&self, allowed_markets: &[MarketId]) -> Result<(), String> {
        let _ = allowed_markets;
        todo!("P07 agent decision-types: schema v3 validation")
    }
}
