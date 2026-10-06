//! MarkdownV2 escaping + message builders (`SPEC-P11.md` §4).
//!
//! **Skeleton status (P11):** interfaces frozen; implemented by the wave.

/// Escape every Telegram MarkdownV2 reserved character with a backslash.
pub fn escape_md2(_input: &str) -> String {
    todo!("P11 agent bot-frame")
}

/// Rows for the status card (one per position).
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRow {
    /// Symbol.
    pub symbol: String,
    /// Market id.
    pub market_id: u32,
    /// Signed size (base units).
    pub size: String,
    /// Entry price.
    pub entry: String,
    /// Mark price (or `n/a`).
    pub mark: String,
    /// Distance to liquidation, rendered %.
    pub distance_pct: String,
    /// Tier emoji.
    pub tier_emoji: String,
    /// Collateral.
    pub collateral: String,
    /// Unrealized PnL.
    pub upnl: String,
}

/// `/status` card.
pub fn status_card(
    _rows: &[PositionRow],
    _free_balance: &str,
    _feed_age_s: Option<u64>,
    _mode: &str,
) -> String {
    todo!("P11 agent bot-frame")
}
