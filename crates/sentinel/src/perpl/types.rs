//! Raw venue JSON → domain type mapping. Nothing raw leaks past this module.
//!
//! Implements the frozen interface from `SPEC.md` §4 (`types`) following the
//! §3.3 mapping rules exactly, pinned by the normative examples of §3.5.
//!
//! Scale rules (from `docs/FACTS.md`, page-1 verified constants):
//! price = raw / 10^price_decimals · size = raw / 10^size_decimals ·
//! amounts = raw / 10^6 (collateral AUSD carries 6 decimals on both networks) ·
//! leverage = `lv` / 100 · margin fraction = `100 / raw` (Perpl
//! leverage-hundredths encoding: `1200 -> 12x -> 1/12`, `2000 -> 20x -> 5%`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sentinel_core::types::{AccountState, Market, MarketId, Position};
use serde_json::Value;

use crate::error::{PerplError, Result, SentinelError};

/// Collateral (AUSD) decimals on both networks — `docs/FACTS.md` §1.
const COLLATERAL_DECIMALS: u32 = 6;

/// Upper bound on venue-reported decimal exponents (well above the 0–8 range in
/// use) so that `10^e` and `Decimal::new(1, e)` cannot overflow or panic.
const MAX_DECIMALS: u32 = 18;

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
/// `symbol` falls back to `name` when the venue sends it empty (the BTC/MON
/// mainnet quirk); `base` is always the raw `name`. `min_size` is always
/// [`Decimal::ZERO`]: the venue minimum is not exposed in context
/// (`min_posting_amount` is collateral-denominated and currently `0`).
///
/// # Errors
/// `PerplError::Rest` on structurally invalid payloads.
pub fn parse_context(v: &Value) -> Result<Vec<Market>> {
    let markets = require(v, "markets", "context")?
        .as_array()
        .ok_or_else(|| rest_error("context: `markets` is not an array"))?;
    markets.iter().map(market_from_raw).collect()
}

/// Parse `GET /v1/trading/positions` into positions.
///
/// `marks` maps market id → mark price in **price units** (build it with
/// [`marks_from_ticker`] for REST, or from live `market-state` messages).
/// Only entries with `st == 1` (Open) are returned; `liq_price` and
/// `margin_ratio` stay `None` because the gateway does not expose them.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads or unknown market ids.
pub fn parse_positions(
    v: &Value,
    markets: &[Market],
    marks: &HashMap<u32, Decimal>,
) -> Result<Vec<Position>> {
    let entries = require(v, "d", "positions")?
        .as_array()
        .ok_or_else(|| rest_error("positions: `d` is not an array"))?;
    entries
        .iter()
        .filter(|raw| raw.get("st").and_then(Value::as_u64) == Some(1))
        .map(|raw| position_from_raw(raw, markets, marks))
        .collect()
}

/// Extract mark prices from a raw `/v1/market-data/ticker` payload.
///
/// The payload's `d` is keyed by market id (string keys); each `mrk` is scaled
/// with that market's `price_decimals`. Unknown market ids are rejected.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads or unknown market ids.
pub fn marks_from_ticker(v: &Value, markets: &[Market]) -> Result<HashMap<u32, Decimal>> {
    let states = require(v, "d", "ticker")?
        .as_object()
        .ok_or_else(|| rest_error("ticker: `d` is not an object keyed by market id"))?;
    let mut marks = HashMap::with_capacity(states.len());
    for (key, state) in states {
        let id: u32 = key
            .parse()
            .map_err(|_| rest_error(format!("ticker: invalid market id key `{key}`")))?;
        let market = market_by_id(markets, id)?;
        let raw = require(state, "mrk", "ticker market state")?;
        marks.insert(id, scaled(raw, market.price_decimals, "ticker.mrk")?);
    }
    Ok(marks)
}

/// Parse `GET /v1/trading/wallet` into the primary account snapshot fields.
///
/// Returns `(account_id, free_balance, balance, fee_tier, forward_enabled,
/// last_forwarded_request_id)` for the first account in `as[]`, with
/// `free_balance = b - lb`.
///
/// # Errors
/// `PerplError::Rest` when the wallet holds no exchange account: `as[]` missing
/// or empty (the documented "wallet holds no exchange account" case).
pub fn parse_wallet(v: &Value) -> Result<(u64, Decimal, Decimal, u32, bool, u64)> {
    let accounts = require(v, "as", "wallet")?
        .as_array()
        .ok_or_else(|| rest_error("wallet: `as` is not an array"))?;
    let account = accounts
        .first()
        .ok_or_else(|| rest_error("wallet holds no exchange account: empty `as[]`"))?;
    let account_id = as_u64(
        require(account, "id", "wallet account")?,
        "wallet account.id",
    )?;
    let balance = amount(require(account, "b", "wallet account")?, "wallet account.b")?;
    let locked = amount(
        require(account, "lb", "wallet account")?,
        "wallet account.lb",
    )?;
    let fee_tier = as_u32(
        require(account, "ft", "wallet account")?,
        "wallet account.ft",
    )?;
    let forward_enabled = account
        .get("fw")
        .and_then(Value::as_bool)
        .ok_or_else(|| rest_error("wallet account: missing boolean `fw`"))?;
    let lfr = as_u64(
        require(account, "lfr", "wallet account")?,
        "wallet account.lfr",
    )?;
    Ok((
        account_id,
        balance - locked,
        balance,
        fee_tier,
        forward_enabled,
        lfr,
    ))
}

/// Compose `AccountState` from wallet + positions (+ mark map).
///
/// `equity = balance + Σ unrealized_pnl` (SPEC §4). `snapshot_ts` is taken
/// from the wallet's `at` timestamp, falling back to the positions payload's
/// `at`.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn account_state(
    wallet: &Value,
    positions: &Value,
    marks: &HashMap<u32, Decimal>,
    markets: &[Market],
) -> Result<AccountState> {
    let (_, free_balance, balance, fee_tier, _, _) = parse_wallet(wallet)?;
    let snapshot_ts = timestamp(wallet.get("at"))
        .or_else(|| timestamp(positions.get("at")))
        .ok_or_else(|| rest_error("account snapshot: missing `at` timestamp"))?;
    let positions = parse_positions(positions, markets, marks)?;
    let mut equity = balance;
    for p in &positions {
        equity = equity
            .checked_add(p.unrealized_pnl)
            .ok_or_else(|| rest_error("account snapshot: equity out of range"))?;
    }
    Ok(AccountState {
        positions,
        free_balance,
        equity,
        fee_tier,
        snapshot_ts,
    })
}

/// Parse a `mt:21` account update.
///
/// `free_balance` is `b - lb`; `ts` is taken from `at` when the update carries
/// one (some updates do not).
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn parse_account_update(v: &Value) -> Result<AccountUpdate> {
    let account_id = as_u64(require(v, "id", "account update")?, "account update.id")?;
    let balance = amount(require(v, "b", "account update")?, "account update.b")?;
    let locked = amount(require(v, "lb", "account update")?, "account update.lb")?;
    let fee_tier = as_u32(require(v, "ft", "account update")?, "account update.ft")?;
    let forward_enabled = v
        .get("fw")
        .and_then(Value::as_bool)
        .ok_or_else(|| rest_error("account update: missing boolean `fw`"))?;
    let last_forwarded_request_id =
        as_u64(require(v, "lfr", "account update")?, "account update.lfr")?;
    Ok(AccountUpdate {
        account_id,
        free_balance: balance - locked,
        fee_tier,
        forward_enabled,
        last_forwarded_request_id,
        ts: timestamp(v.get("at")),
    })
}

/// Parse `GET /v1/trading/fills` (`d[]`) into fill records.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads or unknown market ids.
pub fn parse_fills(v: &Value, markets: &[Market]) -> Result<Vec<FillRecord>> {
    let entries = require(v, "d", "fills")?
        .as_array()
        .ok_or_else(|| rest_error("fills: `d` is not an array"))?;
    entries
        .iter()
        .map(|raw| fill_from_raw(raw, markets))
        .collect()
}

/// Parse `GET /v1/trading/account-history` (`d[]`) into event records.
///
/// # Errors
/// `PerplError::Rest` on invalid payloads.
pub fn parse_account_history(v: &Value) -> Result<Vec<AccountEventRecord>> {
    let entries = require(v, "d", "account history")?
        .as_array()
        .ok_or_else(|| rest_error("account history: `d` is not an array"))?;
    entries.iter().map(event_from_raw).collect()
}

// ---- private mapping helpers -------------------------------------------------

/// Wrap a message in `PerplError::Rest` — the parsing error for this module.
fn rest_error(msg: impl Into<String>) -> SentinelError {
    SentinelError::Perpl(PerplError::Rest(msg.into()))
}

/// `Err(PerplError::Rest(msg))` in the crate result type.
fn rest_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(rest_error(msg))
}

/// Fetch a required field from an object.
fn require<'a>(v: &'a Value, key: &str, ctx: &str) -> Result<&'a Value> {
    v.get(key)
        .ok_or_else(|| rest_error(format!("{ctx}: missing `{key}`")))
}

/// Fetch a required string field from an object.
fn require_str<'a>(v: &'a Value, key: &str, ctx: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| rest_error(format!("{ctx}: missing string `{key}`")))
}

/// Unsigned integer from a JSON number or a decimal digit string.
fn as_u64(v: &Value, ctx: &str) -> Result<u64> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str()
        && let Ok(n) = s.parse::<u64>()
    {
        return Ok(n);
    }
    rest_err(format!("{ctx}: expected an unsigned integer"))
}

/// `u32` via [`as_u64`], range-checked.
fn as_u32(v: &Value, ctx: &str) -> Result<u32> {
    let n = as_u64(v, ctx)?;
    u32::try_from(n).map_err(|_| rest_error(format!("{ctx}: value {n} out of range for u32")))
}

/// Raw venue numeric (JSON integer or decimal string) without scaling.
fn raw_decimal(v: &Value, ctx: &str) -> Result<Decimal> {
    if let Some(s) = v.as_str() {
        return s
            .parse::<Decimal>()
            .map_err(|_| rest_error(format!("{ctx}: `{s}` is not a number")));
    }
    if let Some(n) = v.as_i64() {
        return Ok(Decimal::from(n));
    }
    if let Some(n) = v.as_u64() {
        return Ok(Decimal::from(n));
    }
    rest_err(format!("{ctx}: expected a JSON integer or decimal string"))
}

/// `10^decimals` as `Decimal`, rejecting absurd exponents.
fn pow10(decimals: u32) -> Result<Decimal> {
    if decimals > MAX_DECIMALS {
        return rest_err(format!(
            "decimals {decimals} out of supported range (max {MAX_DECIMALS})"
        ));
    }
    Ok(Decimal::from(10u64.pow(decimals)))
}

/// `raw / 10^decimals` — the price/size scaling rule (SPEC §3.3).
fn scaled(v: &Value, decimals: u32, ctx: &str) -> Result<Decimal> {
    let raw = raw_decimal(v, ctx)?;
    raw.checked_div(pow10(decimals)?)
        .ok_or_else(|| rest_error(format!("{ctx}: value out of range")))
}

/// `raw / 10^6` — the amount scaling rule (collateral decimals, SPEC §3.3).
fn amount(v: &Value, ctx: &str) -> Result<Decimal> {
    scaled(v, COLLATERAL_DECIMALS, ctx)
}

/// Market decimals field, range-checked against the supported exponent set.
fn decimals(v: &Value, ctx: &str) -> Result<u32> {
    let d = as_u32(v, ctx)?;
    if d > MAX_DECIMALS {
        return rest_err(format!(
            "{ctx}: {d} out of supported range (max {MAX_DECIMALS})"
        ));
    }
    Ok(d)
}

/// `t`-like millisecond timestamp: `{"b":..,"t":ms}`, a number, or a digit
/// string; absent or unparseable ⇒ `None` (SPEC §3.3).
fn timestamp(v: Option<&Value>) -> Option<DateTime<Utc>> {
    millis(v?).and_then(DateTime::from_timestamp_millis)
}

/// Milliseconds from the timestamp shapes above.
fn millis(v: &Value) -> Option<i64> {
    if let Some(obj) = v.as_object() {
        return obj.get("t").and_then(millis);
    }
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(n) = v.as_u64() {
        return i64::try_from(n).ok();
    }
    v.as_str().and_then(|s| s.parse::<i64>().ok())
}

/// Look up a market by raw id; unknown ⇒ `Err` (SPEC §3.3).
fn market_by_id(markets: &[Market], id: u32) -> Result<&Market> {
    markets
        .iter()
        .find(|m| m.id.0 == id)
        .ok_or_else(|| rest_error(format!("unknown market id {id}")))
}

/// Map one `Market` entry of `/v1/pub/context`.
fn market_from_raw(raw: &Value) -> Result<Market> {
    let id = as_u32(require(raw, "id", "market")?, "market.id")?;
    let name = require_str(raw, "name", "market")?;
    let symbol = match raw.get("symbol").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        // Empty (or missing) `symbol` → fall back to `name` (BTC/MON quirk).
        _ => name.to_string(),
    };
    let config = require(raw, "config", "market")?;
    let price_decimals = decimals(
        require(config, "price_decimals", "market.config")?,
        "market.config.price_decimals",
    )?;
    let size_decimals = decimals(
        require(config, "size_decimals", "market.config")?,
        "market.config.size_decimals",
    )?;
    let initial_margin = as_u64(
        require(config, "initial_margin", "market.config")?,
        "market.config.initial_margin",
    )?;
    let maintenance_margin = as_u64(
        require(config, "maintenance_margin", "market.config")?,
        "market.config.maintenance_margin",
    )?;
    if initial_margin == 0 || maintenance_margin == 0 {
        return rest_err(format!("market {id}: margin fractions must be non-zero"));
    }
    Ok(Market {
        id: MarketId(id),
        symbol,
        base: name.to_string(),
        price_decimals,
        size_decimals,
        // Perpl encodes margins as leverage-hundredths: fraction = 100 / raw.
        initial_margin_fraction: Decimal::from(100u64) / Decimal::from(initial_margin),
        maintenance_margin_fraction: Decimal::from(100u64) / Decimal::from(maintenance_margin),
        max_leverage: Decimal::from(initial_margin) / Decimal::from(100u64),
        // Venue minimum is not exposed in context: `min_posting_amount` is
        // collateral-denominated and currently `0` (SPEC §3.3).
        min_size: Decimal::ZERO,
        tick_size: Decimal::new(1, price_decimals),
        maker_fee_micros: as_u64(
            require(config, "maker_fee", "market.config")?,
            "market.config.maker_fee",
        )?,
        taker_fee_micros: as_u64(
            require(config, "taker_fee", "market.config")?,
            "market.config.taker_fee",
        )?,
        order_ttl_blocks: as_u64(
            require(raw, "order_ttl_blocks", "market")?,
            "market.order_ttl_blocks",
        )?,
    })
}

/// Map one `Position` entry (already filtered to Open) of the positions page.
fn position_from_raw(
    raw: &Value,
    markets: &[Market],
    marks: &HashMap<u32, Decimal>,
) -> Result<Position> {
    let market_id = as_u32(require(raw, "mkt", "position")?, "position.mkt")?;
    let market = market_by_id(markets, market_id)?;
    let sd = as_u32(require(raw, "sd", "position")?, "position.sd")?;
    let sign = match sd {
        1 => Decimal::ONE,
        2 => -Decimal::ONE,
        other => return rest_err(format!("position: unsupported side `sd` {other}")),
    };
    let size = scaled(
        require(raw, "s", "position")?,
        market.size_decimals,
        "position.s",
    )? * sign;
    let entry_price = scaled(
        require(raw, "ep", "position")?,
        market.price_decimals,
        "position.ep",
    )?;
    let collateral = amount(require(raw, "c", "position")?, "position.c")?;
    // `lv` is leverage in hundredths: 500 ⇒ 5x.
    let leverage = scaled(require(raw, "lv", "position")?, 2, "position.lv")?;
    let mark_price = marks.get(&market_id).copied();
    let unrealized_pnl = match mark_price {
        Some(mark) => {
            let delta = mark
                .checked_sub(entry_price)
                .ok_or_else(|| rest_error("position: entry/mark price out of range"))?;
            delta
                .checked_mul(size)
                .ok_or_else(|| rest_error("position: unrealized pnl out of range"))?
        }
        // No mark ⇒ PnL is unknown; the spec fixes it to zero.
        None => Decimal::ZERO,
    };
    Ok(Position {
        market_id: MarketId(market_id),
        symbol: market.symbol.clone(),
        size,
        entry_price,
        mark_price,
        // The gateway does not expose a liquidation price (P04 derives it).
        liq_price: None,
        collateral,
        unrealized_pnl,
        // The gateway does not expose a margin ratio.
        margin_ratio: None,
        leverage,
        opened_at: timestamp(raw.get("ots")),
    })
}

/// Map one `Fill` entry of `FillHistoryPage.d[]`.
fn fill_from_raw(raw: &Value, markets: &[Market]) -> Result<FillRecord> {
    let market_id = as_u32(require(raw, "mkt", "fill")?, "fill.mkt")?;
    let market = market_by_id(markets, market_id)?;
    let order_id = as_u64(require(raw, "oid", "fill")?, "fill.oid")?;
    let liquidity = as_u64(require(raw, "l", "fill")?, "fill.l")?;
    let price = match raw.get("p") {
        // `p` is optional: market orders may report no fill price.
        None | Some(Value::Null) => None,
        Some(p) => Some(scaled(p, market.price_decimals, "fill.p")?),
    };
    Ok(FillRecord {
        market_id: MarketId(market_id),
        order_id,
        // LiquiditySide: 1 = Maker, 2 = Taker.
        is_maker: liquidity == 1,
        price,
        size: scaled(require(raw, "s", "fill")?, market.size_decimals, "fill.s")?,
        fee: amount(require(raw, "f", "fill")?, "fill.f")?,
        ts: timestamp(raw.get("at")),
    })
}

/// Map one `AccountEvent` entry of `AccountHistoryPage.d[]`.
fn event_from_raw(raw: &Value) -> Result<AccountEventRecord> {
    let kind = as_u32(require(raw, "et", "account event")?, "account event.et")?;
    let market_id = match raw.get("m") {
        None | Some(Value::Null) => None,
        Some(m) => Some(MarketId(as_u32(m, "account event.m")?)),
    };
    Ok(AccountEventRecord {
        kind,
        market_id,
        amount: amount(require(raw, "a", "account event")?, "account event.a")?,
        balance: amount(require(raw, "b", "account event")?, "account event.b")?,
        ts: timestamp(raw.get("at")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    /// SPEC §3.5 context example (trimmed real testnet payload).
    fn context_payload() -> Value {
        json!({"markets":[{"ver":270,"id":32,"instance_id":12,"perpetual_id":32,"symbol":"ETH","name":"ETH Perp",
            "funding_interval_sec":2580,"order_ttl_blocks":20,"order_max_market_slippage_bps":1000,
            "config":{"price_decimals":2,"size_decimals":3,"min_posting_amount":"0","initial_margin":1200,
            "maintenance_margin":2000,"maker_fee":45,"taker_fee":345,"contract_version":[1,7,5]}}]})
    }

    /// SPEC §3.5 positions example; `sd` selects Long (1) / Short (2).
    fn positions_payload(sd: u64) -> Value {
        json!({"mt":26,"sn":68507460,"at":{"b":68507460,"t":1791235369000i64},"d":[
            {"at":{"b":1,"t":1791235000000i64},"mkt":32,"acc":7,"pid":1001,"rq":41,"oid":555,"st":1,"sr":21,
             "sd":sd,"c":"150000000","ep":271200,"s":50000,"fee":"1000","cfee":"0","efs":0,"lv":500,
             "dpnl":"0","fnd":"0","ots":{"b":1,"t":1791234000000i64}}]})
    }

    /// SPEC §3.5 wallet example (mt:19).
    fn wallet_payload() -> Value {
        json!({"mt":19,"sn":68507460,"at":{"b":68507460,"t":1791235369000i64},"addr":"0xabc","n":12,"fl":0,
            "as":[{"mt":19,"in":12,"id":7,"fr":false,"fw":true,"ft":0,"lfr":41,"b":"1000000000","lb":"0"}],"sts":[]})
    }

    /// Mark map from SPEC §3.5: `{32: 2713.70}`.
    fn marks_271370() -> HashMap<u32, Decimal> {
        HashMap::from([(32u32, Decimal::new(271370, 2))])
    }

    /// Markets slice built from the SPEC §3.5 context.
    fn eth_markets() -> Vec<Market> {
        parse_context(&context_payload()).expect("SPEC §3.5 context must parse")
    }

    /// Require a `PerplError::Rest` failure and return its message.
    fn rest_err_msg<T: std::fmt::Debug>(r: crate::error::Result<T>) -> String {
        match r {
            Err(SentinelError::Perpl(PerplError::Rest(msg))) => msg,
            other => panic!("expected Err(PerplError::Rest), got {other:?}"),
        }
    }

    #[test]
    fn context_maps_eth_market_exactly() {
        let markets = eth_markets();
        assert_eq!(markets.len(), 1);
        let eth = &markets[0];
        assert_eq!(eth.id, MarketId(32));
        assert_eq!(eth.symbol, "ETH");
        assert_eq!(eth.base, "ETH Perp");
        assert_eq!(eth.price_decimals, 2);
        assert_eq!(eth.size_decimals, 3);
        assert_eq!(
            eth.initial_margin_fraction,
            Decimal::from(100) / Decimal::from(1200)
        );
        assert_eq!(eth.maintenance_margin_fraction, Decimal::new(5, 2)); // 0.05
        assert_eq!(eth.max_leverage, Decimal::from(12)); // 1200 hundredths = 12x
        assert_eq!(eth.min_size, Decimal::ZERO);
        assert_eq!(eth.tick_size, Decimal::new(1, 2)); // 0.01
        assert_eq!(eth.maker_fee_micros, 45);
        assert_eq!(eth.taker_fee_micros, 345);
        assert_eq!(eth.order_ttl_blocks, 20);
    }

    #[test]
    fn context_falls_back_to_name_for_empty_symbol() {
        // BTC/MON mainnet quirk: empty `symbol` ⇒ symbol = `name`; base = `name`.
        let v = json!({"markets":[{"id":1,"symbol":"","name":"BTC Perp","order_ttl_blocks":20,
            "config":{"price_decimals":1,"size_decimals":4,"initial_margin":1000,"maintenance_margin":2000,
                      "maker_fee":10,"taker_fee":60}}]});
        let markets = parse_context(&v).expect("BTC context must parse");
        assert_eq!(markets[0].symbol, "BTC Perp");
        assert_eq!(markets[0].base, "BTC Perp");
        assert_eq!(markets[0].tick_size, Decimal::new(1, 1));
    }

    #[test]
    fn positions_long_maps_sign_and_pnl() {
        let positions = parse_positions(&positions_payload(1), &eth_markets(), &marks_271370())
            .expect("SPEC §3.5 long position must parse");
        assert_eq!(positions.len(), 1);
        let p = &positions[0];
        assert_eq!(p.market_id, MarketId(32));
        assert_eq!(p.symbol, "ETH");
        assert_eq!(p.size, Decimal::new(50, 0)); // +50.000
        assert_eq!(p.entry_price, Decimal::new(271200, 2)); // 2712.00
        assert_eq!(p.collateral, Decimal::new(150_000_000, 6)); // 150.000000
        assert_eq!(p.leverage, Decimal::new(5, 0)); // lv 500 ⇒ 5x
        assert_eq!(p.mark_price, Some(Decimal::new(271370, 2))); // 2713.70
        // (2713.70 − 2712.00) × 50 = 85.000000
        assert_eq!(p.unrealized_pnl, Decimal::new(85, 0));
        assert_eq!(p.liq_price, None);
        assert_eq!(p.margin_ratio, None);
        assert_eq!(
            p.opened_at,
            DateTime::from_timestamp_millis(1791234000000i64)
        );
    }

    #[test]
    fn positions_short_maps_sign_and_pnl() {
        let positions = parse_positions(&positions_payload(2), &eth_markets(), &marks_271370())
            .expect("SPEC §3.5 short position must parse");
        assert_eq!(positions.len(), 1);
        let p = &positions[0];
        assert_eq!(p.size, Decimal::new(-50, 0)); // −50.000
        assert_eq!(p.entry_price, Decimal::new(271200, 2));
        assert_eq!(p.collateral, Decimal::new(150_000_000, 6));
        assert_eq!(p.leverage, Decimal::new(5, 0));
        // (2713.70 − 2712.00) × −50 = −85.000000
        assert_eq!(p.unrealized_pnl, Decimal::new(-85, 0));
    }

    #[test]
    fn positions_without_mark_have_no_mark_and_zero_pnl() {
        let positions = parse_positions(&positions_payload(1), &eth_markets(), &HashMap::new())
            .expect("position without marks must parse");
        assert_eq!(positions[0].mark_price, None);
        assert_eq!(positions[0].unrealized_pnl, Decimal::ZERO);
    }

    #[test]
    fn positions_keep_only_open_status() {
        let mut v = positions_payload(1);
        let mut closed = v["d"][0].clone();
        closed["st"] = json!(3); // Liquidated — must be ignored
        v["d"].as_array_mut().expect("`d` is an array").push(closed);
        let positions = parse_positions(&v, &eth_markets(), &HashMap::new())
            .expect("mixed-status positions must parse");
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, Decimal::new(50, 0));
    }

    #[test]
    fn ticker_maps_mark_price() {
        let v = json!({"mt":9,"sn":1,"d":{"32":{"at":{"t":1791235369000i64},"orl":271373,"mrk":271370,"lst":271438,
            "mid":271401,"bid":271386,"ask":271416,"prv":270329,"dv":3849321,"oi":238883,"tvl":"132421635870"}}});
        let marks = marks_from_ticker(&v, &eth_markets()).expect("SPEC §3.5 ticker must parse");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks.get(&32).copied(), Some(Decimal::new(271370, 2))); // 2713.70
    }

    #[test]
    fn wallet_returns_primary_account_tuple() {
        let (account_id, free, balance, fee_tier, forward_enabled, lfr) =
            parse_wallet(&wallet_payload()).expect("SPEC §3.5 wallet must parse");
        assert_eq!(account_id, 7);
        assert_eq!(free, Decimal::from(1000)); // 1000.0
        assert_eq!(balance, Decimal::from(1000)); // 1000.0
        assert_eq!(fee_tier, 0);
        assert!(forward_enabled);
        assert_eq!(lfr, 41);

        // Locked balance (`lb`) reduces the free balance: b − lb.
        let mut locked = wallet_payload();
        locked["as"][0]["lb"] = json!("400000000"); // 400.0
        let (_, free, balance, _, _, _) =
            parse_wallet(&locked).expect("wallet with locked balance must parse");
        assert_eq!(free, Decimal::from(600));
        assert_eq!(balance, Decimal::from(1000));
    }

    #[test]
    fn wallet_without_exchange_account_errors() {
        let msg = rest_err_msg(parse_wallet(&json!({"mt":19,"sts":[]})));
        assert!(msg.contains("holds no exchange account") || msg.contains("missing"));
        let msg = rest_err_msg(parse_wallet(&json!({"mt":19,"as":[],"sts":[]})));
        assert!(msg.contains("wallet holds no exchange account"));
    }

    #[test]
    fn account_update_mt21_exact() {
        let v = json!({"mt":21,"in":12,"id":7,"fr":false,"fw":true,"ft":0,"lfr":42,"b":"900000000","lb":"100000000"});
        let update = parse_account_update(&v).expect("SPEC §3.5 mt:21 must parse");
        assert_eq!(
            update,
            AccountUpdate {
                account_id: 7,
                free_balance: Decimal::from(800), // 800.0 = 900 − 100
                fee_tier: 0,
                forward_enabled: true,
                last_forwarded_request_id: 42,
                ts: None,
            }
        );
    }

    #[test]
    fn fills_record_exact() {
        let v = json!({"d":[{"at":{"t":1791235369000i64},"mkt":32,"oid":555,"t":1,"l":2,"p":271300,"s":50000,"f":"9450"}]});
        let fills = parse_fills(&v, &eth_markets()).expect("SPEC §3.5 fills must parse");
        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.market_id, MarketId(32));
        assert_eq!(f.order_id, 555);
        assert!(!f.is_maker); // l == 2 ⇒ taker
        assert_eq!(f.price, Some(Decimal::new(271300, 2))); // 2713.00
        assert_eq!(f.size, Decimal::new(50, 0)); // 50.000
        // SPEC §3.5 prints `fee: 9.450000` for raw `"f": "9450"`, but §3.3
        // (amounts = raw / 10^6, `f` included) and the §3.5 account-history
        // example (same raw 9450 → −0.009450) both give 0.009450; the rule is
        // applied. Reported as a spec disagreement.
        assert_eq!(f.fee, Decimal::new(9450, 6)); // 0.009450
        assert_eq!(f.ts, DateTime::from_timestamp_millis(1791235369000i64));
    }

    #[test]
    fn account_history_record_exact() {
        let v = json!({"d":[{"at":{"t":1791235369000i64},"in":12,"id":7,"et":4,"m":32,"a":"-9450","b":"999990550","f":"9450"}]});
        let events = parse_account_history(&v).expect("SPEC §3.5 account history must parse");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, 4);
        assert_eq!(e.market_id, Some(MarketId(32)));
        assert_eq!(e.amount, Decimal::new(-9450, 6)); // −0.009450
        assert_eq!(e.balance, Decimal::new(999_990_550, 6)); // 999.990550
        assert_eq!(e.ts, DateTime::from_timestamp_millis(1791235369000i64));
    }

    #[test]
    fn account_state_sums_equity_from_balance_and_unrealized() {
        let state = account_state(
            &wallet_payload(),
            &positions_payload(1),
            &marks_271370(),
            &eth_markets(),
        )
        .expect("account state must build");
        assert_eq!(state.positions.len(), 1);
        assert_eq!(state.free_balance, Decimal::from(1000));
        assert_eq!(state.equity, Decimal::from(1085)); // 1000 + 85
        assert_eq!(state.fee_tier, 0);
        assert_eq!(
            state.snapshot_ts,
            DateTime::from_timestamp_millis(1791235369000i64).expect("valid ts")
        );
    }

    #[test]
    fn unknown_market_id_errors() {
        let markets = eth_markets();

        let mut v = positions_payload(1);
        v["d"][0]["mkt"] = json!(99);
        let msg = rest_err_msg(parse_positions(&v, &markets, &HashMap::new()));
        assert_eq!(msg, "unknown market id 99");

        let msg = rest_err_msg(parse_fills(
            &json!({"d":[{"at":{"t":1},"mkt":99,"oid":1,"l":2,"p":1,"s":1,"f":"0"}]}),
            &markets,
        ));
        assert_eq!(msg, "unknown market id 99");

        let msg = rest_err_msg(marks_from_ticker(
            &json!({"mt":9,"d":{"99":{"mrk":1}}}),
            &markets,
        ));
        assert_eq!(msg, "unknown market id 99");
    }

    #[test]
    fn malformed_payloads_error() {
        let markets = eth_markets();
        let marks = marks_271370();

        assert!(parse_context(&json!({})).is_err());
        assert!(parse_context(&json!({"markets": "nope"})).is_err());

        assert!(parse_positions(&json!({"mt":26}), &markets, &marks).is_err());
        // `st` says Open but required fields (`ep`, `c`, `lv`) are missing.
        assert!(
            parse_positions(
                &json!({"d":[{"mkt":32,"st":1,"sd":1,"s":1}]}),
                &markets,
                &marks
            )
            .is_err()
        );

        assert!(parse_fills(&json!({"d":[{"mkt":32,"oid":1}]}), &markets).is_err());
        assert!(marks_from_ticker(&json!({"mt":9,"d":"nope"}), &markets).is_err());

        assert!(parse_account_update(&json!({"mt":21,"id":7})).is_err());
        assert!(parse_account_history(&json!({})).is_err());
        assert!(parse_account_history(&json!({"d":[{"et":4}]})).is_err());
    }

    #[test]
    fn unparseable_timestamps_map_to_none() {
        let mut v = positions_payload(1);
        v["d"][0]["ots"] = json!("not-a-timestamp");
        let positions = parse_positions(&v, &eth_markets(), &HashMap::new())
            .expect("position with unparseable `ots` still parses");
        assert_eq!(positions[0].opened_at, None);

        let v =
            json!({"mt":21,"id":7,"fw":false,"ft":1,"lfr":1,"b":"1","lb":"0","at":{"t":"garbage"}});
        let update = parse_account_update(&v).expect("update with unparseable `at` still parses");
        assert_eq!(update.ts, None);
    }
}
