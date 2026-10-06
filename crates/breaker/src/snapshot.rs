//! Position snapshot sourcing, in the frozen order (`SPEC-P14.md` §4):
//!
//! 1. **live Perpl REST** when `PERPL_API_KEY`/`PERPL_API_URL` are present
//!    (signed `GET /v1/trading/positions` + public context/ticker for the
//!    market table and mark prices);
//! 2. **`BREAKER_SNAPSHOT_FILE`** — a JSON array of `Position` (the spec
//!    literal) or the documented extended object
//!    `{"positions":[…],"markets":[…]}` so an offline fixture can carry the
//!    market metadata the sizing step needs;
//! 3. **none** → the executor degrades to alert-only and submits NO order.
//!
//! A live fetch failure falls through to the next source with a loud warning;
//! a file failure degrades to alert-only. Decimals in snapshot files follow
//! the workspace convention (`rust_decimal` default serde): they serialize as
//! JSON strings and tolerate both strings and numbers on read.

use std::fmt;
use std::path::PathBuf;

use sentinel::perpl::auth::ApiKeySigner;
use sentinel::perpl::rest::PerplRest;
use sentinel::perpl::types;
use sentinel_core::types::{Market, MarketId, Position};
use serde::Deserialize;
use tracing::warn;

use crate::config::BreakerConfig;

/// Where a snapshot came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOrigin {
    /// Live Perpl gateway REST.
    LivePerpl,
    /// Local snapshot file.
    File(PathBuf),
    /// No source available (safe degrade: alert-only, no order).
    None,
}

impl SnapshotOrigin {
    /// Short wire label used in the journal detail.
    pub fn as_str(&self) -> String {
        match self {
            SnapshotOrigin::LivePerpl => "live_perpl".to_string(),
            SnapshotOrigin::File(path) => format!("file:{}", path.display()),
            SnapshotOrigin::None => "none".to_string(),
        }
    }
}

/// Positions plus the market metadata available alongside them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotBundle {
    /// Open positions at snapshot time.
    pub positions: Vec<Position>,
    /// Market table (empty when the source carries none).
    pub markets: Vec<Market>,
}

impl SnapshotBundle {
    /// Market metadata for `market_id`, when known.
    pub fn market_for(&self, market_id: MarketId) -> Option<&Market> {
        self.markets.iter().find(|market| market.id == market_id)
    }
}

/// A resolved snapshot together with its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcedSnapshot {
    /// Which source produced the bundle.
    pub origin: SnapshotOrigin,
    /// The positions (+ markets) themselves.
    pub bundle: SnapshotBundle,
}

/// Snapshot sourcing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// File read failure (path included; contents never are).
    Io {
        /// The offending path.
        path: PathBuf,
        /// OS error description.
        detail: String,
    },
    /// Invalid JSON or shape.
    Json(String),
    /// Live REST/auth failure.
    Rest(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io { path, detail } => {
                write!(f, "snapshot file {}: {detail}", path.display())
            }
            SnapshotError::Json(detail) => write!(f, "snapshot JSON invalid: {detail}"),
            SnapshotError::Rest(detail) => write!(f, "live Perpl snapshot failed: {detail}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Extended file shape (documented extension of the spec literal).
#[derive(Debug, Deserialize)]
struct SnapshotFileObject {
    positions: Vec<Position>,
    #[serde(default)]
    markets: Vec<Market>,
}

/// `BREAKER_SNAPSHOT_FILE` source: array of `Position` or extended object.
#[derive(Debug, Clone)]
pub struct FileSnapshotSource {
    path: PathBuf,
}

impl FileSnapshotSource {
    /// New file source for `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load and parse the snapshot file.
    ///
    /// # Errors
    /// [`SnapshotError`] on read, JSON or shape problems.
    pub fn load(&self) -> Result<SnapshotBundle, SnapshotError> {
        let text = std::fs::read_to_string(&self.path).map_err(|err| SnapshotError::Io {
            path: self.path.clone(),
            detail: err.to_string(),
        })?;
        parse_snapshot_file(&text)
    }
}

/// Parse a snapshot document: the spec-literal `Position[]` array, or the
/// extended `{"positions":…,"markets":…}` object.
///
/// # Errors
/// [`SnapshotError::Json`] when the document is neither shape.
pub fn parse_snapshot_file(text: &str) -> Result<SnapshotBundle, SnapshotError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| SnapshotError::Json(format!("not valid JSON: {err}")))?;
    if value.is_array() {
        let positions: Vec<Position> = serde_json::from_value(value)
            .map_err(|err| SnapshotError::Json(format!("not a Position array: {err}")))?;
        return Ok(SnapshotBundle {
            positions,
            markets: Vec::new(),
        });
    }
    let object: SnapshotFileObject = serde_json::from_value(value).map_err(|err| {
        SnapshotError::Json(format!(
            "not a Position array or {{positions, markets}} object: {err}"
        ))
    })?;
    Ok(SnapshotBundle {
        positions: object.positions,
        markets: object.markets,
    })
}

/// Live Perpl REST source (signed trading endpoints + public context).
pub struct LivePerplSource {
    base_url: String,
    api_key: String,
    api_key_secret: String,
    chain_id: u64,
}

impl LivePerplSource {
    /// Build from the breaker configuration.
    ///
    /// # Errors
    /// [`SnapshotError::Rest`] when the API key, secret or URL is missing.
    pub fn new(cfg: &BreakerConfig) -> Result<Self, SnapshotError> {
        let api_key = cfg
            .perpl_api_key
            .clone()
            .ok_or_else(|| SnapshotError::Rest("PERPL_API_KEY is not configured".to_string()))?;
        let api_key_secret = cfg.perpl_api_key_secret.clone().ok_or_else(|| {
            SnapshotError::Rest(
                "PERPL_API_KEY_SECRET is not configured (signing is required for \
                 /v1/trading/positions)"
                    .to_string(),
            )
        })?;
        let base_url = cfg
            .perpl_api_url
            .clone()
            .ok_or_else(|| SnapshotError::Rest("PERPL_API_URL is not configured".to_string()))?;
        Ok(Self {
            base_url,
            api_key,
            api_key_secret,
            chain_id: cfg.perpl_chain_id,
        })
    }

    /// Fetch positions + market table (context → ticker marks → positions).
    ///
    /// # Errors
    /// [`SnapshotError::Rest`] on auth, transport or parse failures.
    pub async fn load(&self) -> Result<SnapshotBundle, SnapshotError> {
        let signer = ApiKeySigner::from_parts(&self.api_key, &self.api_key_secret, self.chain_id)
            .map_err(|err| SnapshotError::Rest(format!("signer: {err}")))?;
        let rest = PerplRest::new(self.base_url.clone(), signer)
            .map_err(|err| SnapshotError::Rest(format!("REST client: {err}")))?;

        let context = rest
            .get_context()
            .await
            .map_err(|err| SnapshotError::Rest(format!("GET /v1/pub/context: {err}")))?;
        let markets = types::parse_context(&context)
            .map_err(|err| SnapshotError::Rest(format!("context parse: {err}")))?;
        let ticker = rest
            .get_ticker(None)
            .await
            .map_err(|err| SnapshotError::Rest(format!("GET /v1/market-data/ticker: {err}")))?;
        let marks = types::marks_from_ticker(&ticker, &markets)
            .map_err(|err| SnapshotError::Rest(format!("ticker parse: {err}")))?;
        let positions_raw = rest
            .get_positions()
            .await
            .map_err(|err| SnapshotError::Rest(format!("GET /v1/trading/positions: {err}")))?;
        let positions = types::parse_positions(&positions_raw, &markets, &marks)
            .map_err(|err| SnapshotError::Rest(format!("positions parse: {err}")))?;

        Ok(SnapshotBundle { positions, markets })
    }
}

/// Resolve the snapshot in the frozen source order, degrading loudly.
///
/// Never fails: a missing or broken source yields
/// [`SnapshotOrigin::None`] + an empty bundle, which the executor turns into
/// an alert-only action.
pub async fn resolve(cfg: &BreakerConfig) -> SourcedSnapshot {
    // 1. Live Perpl REST (key + URL present).
    if cfg.has_perpl_live() {
        match LivePerplSource::new(cfg) {
            Ok(source) => match source.load().await {
                Ok(bundle) => {
                    return SourcedSnapshot {
                        origin: SnapshotOrigin::LivePerpl,
                        bundle,
                    };
                }
                Err(err) => warn!(
                    error = %err,
                    "breaker snapshot: live Perpl fetch failed; falling back to the snapshot file"
                ),
            },
            Err(err) => warn!(
                error = %err,
                "breaker snapshot: live Perpl source unusable; falling back to the snapshot file"
            ),
        }
    }

    // 2. Snapshot file.
    if let Some(path) = &cfg.snapshot_file {
        let source = FileSnapshotSource::new(path.clone());
        match source.load() {
            Ok(bundle) => {
                return SourcedSnapshot {
                    origin: SnapshotOrigin::File(path.clone()),
                    bundle,
                };
            }
            Err(err) => warn!(
                error = %err,
                "breaker snapshot: snapshot file unusable; degrading to alert-only"
            ),
        }
    }

    // 3. None — safe degrade, no order.
    warn!("breaker snapshot: no snapshot source available; alert-only (no order)");
    SourcedSnapshot {
        origin: SnapshotOrigin::None,
        bundle: SnapshotBundle::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::str::FromStr;

    /// One position exactly as `serde_json::to_string(&Position)` writes it
    /// (decimals as strings — the workspace convention).
    const POSITION_ONE: &str = r#"{
        "market_id": 32,
        "symbol": "ETH",
        "size": "2",
        "entry_price": "3000",
        "mark_price": "2950",
        "liq_price": "2400",
        "collateral": "600",
        "unrealized_pnl": "-100",
        "margin_ratio": null,
        "leverage": "10",
        "opened_at": null
    }"#;

    /// One market exactly as `serde_json::to_string(&Market)` writes it.
    const MARKET_ONE: &str = r#"{
        "id": 32,
        "symbol": "ETH",
        "base": "ETH",
        "price_decimals": 2,
        "size_decimals": 3,
        "initial_margin_fraction": "0.083333",
        "maintenance_margin_fraction": "0.05",
        "max_leverage": "12",
        "min_size": "0",
        "tick_size": "0.01",
        "maker_fee_micros": 45,
        "taker_fee_micros": 345,
        "order_ttl_blocks": 20
    }"#;

    fn breakeven_cfg(vars: HashMap<String, String>) -> BreakerConfig {
        BreakerConfig::from_vars(vars).expect("test config")
    }

    fn test_cfg_no_snapshot() -> BreakerConfig {
        breakeven_cfg(HashMap::from([
            (
                "BREAKER_ANCHOR_ADDRESS".to_string(),
                "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
            ),
            (
                "BREAKER_GUARDIANS".to_string(),
                "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
            ),
            ("BREAKER_ARM_SECRET".to_string(), "s".to_string()),
        ]))
    }

    #[test]
    fn parses_the_spec_literal_position_array() {
        let text = format!("[{POSITION_ONE}]");
        let bundle = parse_snapshot_file(&text).expect("array parses");
        assert_eq!(bundle.positions.len(), 1);
        assert!(bundle.markets.is_empty());
        let position = &bundle.positions[0];
        assert_eq!(position.market_id, MarketId(32));
        assert_eq!(position.size, Decimal::from_str("2").unwrap());
        assert_eq!(
            position.mark_price,
            Some(Decimal::from_str("2950").unwrap())
        );
        assert_eq!(position.liq_price, Some(Decimal::from_str("2400").unwrap()));
    }

    #[test]
    fn parses_the_extended_object_with_markets() {
        let text = format!(r#"{{"positions":[{POSITION_ONE}],"markets":[{MARKET_ONE}]}}"#);
        let bundle = parse_snapshot_file(&text).expect("object parses");
        assert_eq!(bundle.positions.len(), 1);
        assert_eq!(bundle.markets.len(), 1);
        let market = bundle.market_for(MarketId(32)).expect("market present");
        assert_eq!(market.size_decimals, 3);
        assert_eq!(market.min_size, Decimal::ZERO);
    }

    #[test]
    fn rejects_malformed_documents() {
        for bad in ["{ not json", "42", r#"{"markets":[]}"#] {
            let err = parse_snapshot_file(bad).expect_err("must fail");
            assert!(matches!(err, SnapshotError::Json(_)), "{bad} → {err:?}");
        }
    }

    #[test]
    fn file_source_reads_and_reports_io_errors() {
        let dir = TempDir::new("snapshot");
        let path = dir.join("snap.json");
        std::fs::write(&path, format!("[{POSITION_ONE}]")).expect("write");
        let bundle = FileSnapshotSource::new(path.clone()).load().expect("loads");
        assert_eq!(bundle.positions.len(), 1);

        let missing = FileSnapshotSource::new(dir.join("nope.json"));
        let err = missing.load().expect_err("missing file fails");
        assert!(matches!(err, SnapshotError::Io { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_degrades_in_the_frozen_order() {
        // No live key, no file → None (alert-only).
        let cfg = test_cfg_no_snapshot();
        let resolved = resolve(&cfg).await;
        assert_eq!(resolved.origin, SnapshotOrigin::None);
        assert!(resolved.bundle.positions.is_empty());

        // No live key, file present → File.
        let dir = TempDir::new("resolve");
        let path = dir.join("snap.json");
        std::fs::write(&path, format!("[{POSITION_ONE}]")).expect("write");
        let mut vars = HashMap::new();
        vars.insert(
            "BREAKER_ANCHOR_ADDRESS".to_string(),
            "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
        );
        vars.insert(
            "BREAKER_GUARDIANS".to_string(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
        );
        vars.insert("BREAKER_ARM_SECRET".to_string(), "s".to_string());
        vars.insert(
            "BREAKER_SNAPSHOT_FILE".to_string(),
            path.display().to_string(),
        );
        let cfg = breakeven_cfg(vars);
        let resolved = resolve(&cfg).await;
        assert_eq!(resolved.origin, SnapshotOrigin::File(path.clone()));
        assert_eq!(resolved.bundle.positions.len(), 1);

        // File present but broken → None (never a panic, never stale reuse).
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "nope").expect("write");
        let mut vars = HashMap::new();
        vars.insert(
            "BREAKER_ANCHOR_ADDRESS".to_string(),
            "0x5FbDB2315678afecb367f032d93F642f64180aa3".to_string(),
        );
        vars.insert(
            "BREAKER_GUARDIANS".to_string(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
        );
        vars.insert("BREAKER_ARM_SECRET".to_string(), "s".to_string());
        vars.insert(
            "BREAKER_SNAPSHOT_FILE".to_string(),
            broken.display().to_string(),
        );
        let cfg = breakeven_cfg(vars);
        let resolved = resolve(&cfg).await;
        assert_eq!(resolved.origin, SnapshotOrigin::None);
    }

    #[test]
    fn origin_labels_are_stable() {
        assert_eq!(SnapshotOrigin::LivePerpl.as_str(), "live_perpl");
        assert_eq!(SnapshotOrigin::None.as_str(), "none");
        assert_eq!(
            SnapshotOrigin::File(PathBuf::from("x.json")).as_str(),
            "file:x.json"
        );
    }
}
