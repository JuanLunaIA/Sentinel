//! Raw venue JSON → domain type mapping. Nothing raw leaks past this module.
//!
//! SKELETON STUB — frozen interface (see `SPEC.md` §4.2). Replaced by agent
//! `types`; do not change public signatures.
//!
//! Scale rules (from `docs/FACTS.md`, page-1 verified constants):
//! price = raw / 10^price_decimals · size = raw / 10^size_decimals ·
//! amounts = raw / 10^6 · leverage = `lv` / 100 ·
//! margin fraction = `100 / raw` (Perpl leverage-hundredths encoding).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Market, MarketId, Position};
use serde_json::Value;

use crate::error::Result;

/// One fill from `GET /v1/trading/fills` (`FillHistoryPage.d[]`).
#[derive(Debug, Clone, PartialEq)]
pub struct FillRecord {
    /// Market the fill happened on.
    pub market_id: MarketId,
    /// Exchange order id.
    pub order_id: u64,
    /// `true` when the fill was maker liquidity (`l == 1`).
    pub is_maker: bool,
    /// Fill price; absent for market orders that report none.
    pub price: Option<Decimal>,
    /// Filled size (absolute, base units).
    pub size: Decimal,
    /// Fee paid (negative = rebate).
    pub fee: Decimal,
    /// Source timestamp when present.
    pub ts: Option<DateTime<Utc>>,
}

/// One account event from `GET /v1/trading/account-history`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountEventRecord {
    /// Event type (`et`; e.g. 1 Deposit, 4 Settlement, 5 Liquidation).
    pub kind: u32,
    /// Market id when the event is market-scoped.
    pub market_id: Option<MarketId>,
    /// Amount change (`a`), collateral units.
    pub amount: Decimal,
    /// Balance after the event (`b`), collateral units.
    pub balance: Decimal,
    /// Source timestamp when present.
    pub ts: Option<DateTime<Utc>>,
}

/// Account fields carried by a `mt:21` account update (used by the WS layer).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUpdate {
    /// Exchange account id.
    pub account_id: u64,
    /// Unlocked balance (`b - lb`), collateral units.
    pub free_balance: Decimal,
    /// Fee tier index (`ft`).
    pub fee_tier: u32,
    /// Order forwarding enabled (`fw`) — orders are rejected while false.
    pub forward_enabled: bool,
    /// Last forwarded request id (`lfr`) — seeds `rq` generation.
    pub last_forwarded_request_id: u64,
    /// Source timestamp when present.
    pub ts: Option<DateTime<Utc>>,
}

/// Parse `/v1/pub/context` into the market list.
///
/// # Errors
/// `PerplError::Rest` on structurally invalid payloads.
pub fn parse_context(_v: &Value) -> Result<Vec<Market>> {
    todo!("P03 agent types")
}

/// Parse `GET /v1/trading/positions` into positions.
///
/// `marks` maps market id → mark price in **price units** (build it with
/// [`marks_from_ticker`] for REST, or from live `market-state` messages).
///
/// # Errors
/// `PerplError::Rest` on invalid payloads or unknown market ids.
pub fn parse_positions(
    _v: &Value,
    _markets: &[Market],
    _marks: &std::collections::HashMap<u32, Decimal>,
) -> Result<Vec<Position>> {
    todo!("P03 agent types")
}

/// Extract mark prices from a raw `/v1/market-data/ticker` payload.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn marks_from_ticker(
    _v: &Value,
    _markets: &[Market],
) -> Result<std::collections::HashMap<u32, Decimal>> {
    todo!("P03 agent types")
}

/// Parse `GET /v1/trading/wallet` into the primary account snapshot fields.
///
/// # Errors
/// `PerplError::Rest` when the wallet holds no exchange account.
pub fn parse_wallet(_v: &Value) -> Result<(u64, Decimal, Decimal, u32, bool, u64)> {
    todo!("P03 agent types")
}

/// Compose `AccountState` from wallet + positions (+ mark map).
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn account_state(
    _wallet: &Value,
    _positions: &Value,
    _marks: &std::collections::HashMap<u32, Decimal>,
    _markets: &[Market],
) -> Result<AccountState> {
    todo!("P03 agent types")
}

/// Parse a `mt:21` account update.
///
/// # Errors
/// `PerplError::Rest`/`Ws` on invalid payloads.
pub fn parse_account_update(_v: &Value) -> Result<AccountUpdate> {
    todo!("P03 agent types")
}

/// Parse `GET /v1/trading/fills` (`d[]`) into fill records.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads or unknown market ids.
pub fn parse_fills(_v: &Value, _markets: &[Market]) -> Result<Vec<FillRecord>> {
    todo!("P03 agent types")
}

/// Parse `GET /v1/trading/account-history` (`d[]`) into event records.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn parse_account_history(_v: &Value) -> Result<Vec<AccountEventRecord>> {
    todo!("P03 agent types")
}
