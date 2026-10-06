//! Scenario format and loader (SPEC-P13 §3, normative).
//!
//! A scenario is a self-contained, frozen replay input: full `sentinel_core`
//! market and position documents, a scripted price path, optional feed events,
//! optional reflex/policy overrides and an optional recorded decision trace.
//! [`load`] parses and validates; every SPEC-P13 §3 violation surfaces as a
//! typed [`ScenarioError`].

use std::path::Path;

use rust_decimal::Decimal;
use sentinel_core::policy::PolicyConfig;
use sentinel_core::risk::ReflexConfig;
use sentinel_core::types::{Decision, Market, MarketId, Position};
use serde::{Deserialize, Serialize};

/// Honesty marker rendered verbatim in the report (SPEC-P13 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioLabel {
    /// Hand-authored stress scenario.
    Synthetic,
    /// Built from real recorded fixtures (P03).
    Recorded,
    /// Rebuilt from recorded testnet data + a consistent synthesized path.
    Reconstructed,
}

impl ScenarioLabel {
    /// Wire/report string: `"synthetic"`, `"recorded"` or `"reconstructed"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::Recorded => "recorded",
            Self::Reconstructed => "reconstructed",
        }
    }
}

/// One price observation of the scenario path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    /// Logical timestamp, milliseconds (strictly increasing in the file).
    pub ts_ms: i64,
    /// Market the mark applies to.
    pub market_id: u32,
    /// Mark price at this tick.
    pub mark_price: Decimal,
}

/// Kinds accepted in `events` (v1.0 honors `feed_stale`; others are ignored
/// and counted in report notes — SPEC-P13 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Feed outage window (`until_ms` required).
    FeedStale,
    /// Accepted, ignored in v1.0.
    Funding,
    /// Accepted, ignored in v1.0.
    BigFill,
}

impl EventKind {
    /// Wire/report string (`"feed_stale"`, `"funding"`, `"big_fill"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FeedStale => "feed_stale",
            Self::Funding => "funding",
            Self::BigFill => "big_fill",
        }
    }
}

/// One scripted event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioEvent {
    /// Start timestamp, ms.
    pub ts_ms: i64,
    /// Event kind.
    pub kind: EventKind,
    /// End of the `feed_stale` window, ms.
    #[serde(default)]
    pub until_ms: Option<i64>,
    /// Free-form note surfaced in the report.
    #[serde(default)]
    pub note: Option<String>,
}

/// One recorded strategy decision replayed at a consult tick (SPEC-P13 §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionTraceEntry {
    /// Tick timestamp the decision applies to.
    pub at_ms: i64,
    /// Market the decision concerns.
    pub market_id: u32,
    /// Full Decision v3 document.
    pub decision: Decision,
}

/// A complete scenario (SPEC-P13 §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable id (file stem expected to match).
    pub id: String,
    /// Honesty label.
    pub label: ScenarioLabel,
    /// Human description.
    pub description: String,
    /// Free (unlocked) balance at scenario start, collateral units.
    pub start_free_balance: Decimal,
    /// Market definitions (full core `Market` documents).
    pub markets: Vec<Market>,
    /// Initial positions (full core `Position` documents).
    pub positions: Vec<Position>,
    /// Price path.
    pub price_path: Vec<PriceTick>,
    /// Scripted events.
    #[serde(default)]
    pub events: Vec<ScenarioEvent>,
    /// Reflex overrides (mapped onto `ReflexConfig`; defaults when absent).
    #[serde(default)]
    pub reflex: Option<serde_json::Value>,
    /// Policy overrides (mapped onto `PolicyConfig`; defaults when absent).
    #[serde(default)]
    pub policy: Option<serde_json::Value>,
    /// Optional recorded-decision trace.
    #[serde(default)]
    pub decision_trace: Option<Vec<DecisionTraceEntry>>,
}

/// Reflex override block (SPEC-P13 §3). Missing fields take the production
/// defaults: `0.5` / `0.25` / `600000` / `false`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ReflexOverrides {
    /// Fraction removed on a Red first-breach reduce, `(0, 1]`.
    reduce_fraction: Decimal,
    /// Fraction removed on an Orange (or gated-stale) reduce, `(0, 1]`.
    orange_fraction: Decimal,
    /// Per-market cooldown between reflexive actions, ms.
    cooldown_ms: u64,
    /// Whether stale data at Orange/Red may trigger a reduce.
    stale_reduce: bool,
}

impl Default for ReflexOverrides {
    fn default() -> Self {
        Self {
            reduce_fraction: Decimal::new(5, 1),
            orange_fraction: Decimal::new(25, 2),
            cooldown_ms: 600_000,
            stale_reduce: false,
        }
    }
}

/// Policy override block (SPEC-P13 §3). Missing fields take the SPEC-P13 §3
/// defaults: allowlist `[32]`, cap `$1000`, `10` daily actions, approval
/// threshold `$500`, kill switch off.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PolicyOverrides {
    /// Markets the engine may act on (Perpl market ids).
    market_allowlist: Vec<u32>,
    /// Per-action notional ceiling, USD.
    max_order_size_usd: Decimal,
    /// Daily action cap.
    max_daily_actions: u32,
    /// Notional above which human approval is required, USD.
    require_approval_above_usd: Decimal,
    /// Kill switch: when `true`, every intent is denied.
    kill_switch: bool,
}

impl Default for PolicyOverrides {
    fn default() -> Self {
        Self {
            market_allowlist: vec![32],
            max_order_size_usd: Decimal::new(1000, 0),
            max_daily_actions: 10,
            require_approval_above_usd: Decimal::new(500, 0),
            kill_switch: false,
        }
    }
}

/// Scenario loading/validation error (typed; SPEC-P13 §3).
#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    /// Filesystem error.
    #[error("scenario io: {0}")]
    Io(String),
    /// JSON/schema error.
    #[error("scenario json: {0}")]
    Json(String),
    /// Semantic validation error.
    #[error("scenario invalid: {0}")]
    Invalid(String),
}

impl Scenario {
    /// Load + validate a scenario from `path` (SPEC-P13 §3).
    ///
    /// # Errors
    /// [`ScenarioError`] on io/json/validation failure.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        load(path)
    }

    /// Parse + validate a scenario from a JSON string (SPEC-P13 §3).
    ///
    /// # Errors
    /// [`ScenarioError`] on json/validation failure.
    pub fn parse(text: &str) -> Result<Self, ScenarioError> {
        let scenario: Self =
            serde_json::from_str(text).map_err(|err| ScenarioError::Json(err.to_string()))?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Validate the SPEC-P13 §3 rules:
    ///
    /// - `price_path` ts_ms non-negative and strictly increasing;
    /// - every `market_id` used (path, positions, trace) exists in `markets`;
    /// - every `mark_price` > 0;
    /// - reflex fractions in `(0, 1]`;
    /// - at least one position and one tick;
    /// - `feed_stale` events carry a valid `until_ms` window.
    ///
    /// # Errors
    /// [`ScenarioError::Invalid`] describing the first violated rule.
    pub fn validate(&self) -> Result<(), ScenarioError> {
        if self.positions.is_empty() {
            return Err(ScenarioError::Invalid(
                "at least one position is required".to_string(),
            ));
        }
        if self.price_path.is_empty() {
            return Err(ScenarioError::Invalid(
                "at least one tick is required (price_path is empty)".to_string(),
            ));
        }
        self.validate_path()?;
        self.validate_market_refs()?;
        self.validate_events()?;
        self.reflex_config()?;
        self.policy_config()?;
        Ok(())
    }

    /// Reflex configuration: overrides mapped onto [`ReflexConfig`], or the
    /// production defaults when the block is absent (SPEC-P13 §3).
    ///
    /// # Errors
    /// [`ScenarioError::Invalid`] on unparseable overrides or fractions
    /// outside `(0, 1]`.
    pub fn reflex_config(&self) -> Result<ReflexConfig, ScenarioError> {
        let cfg = match &self.reflex {
            None => ReflexConfig::default(),
            Some(value) => {
                let overrides: ReflexOverrides = serde_json::from_value(value.clone())
                    .map_err(|err| ScenarioError::Invalid(format!("reflex overrides: {err}")))?;
                ReflexConfig {
                    reduce_fraction: overrides.reduce_fraction,
                    orange_fraction: overrides.orange_fraction,
                    cooldown_ms: overrides.cooldown_ms,
                    stale_reduce: overrides.stale_reduce,
                }
            }
        };
        cfg.validate()
            .map_err(|message| ScenarioError::Invalid(format!("reflex: {message}")))?;
        Ok(cfg)
    }

    /// Policy configuration: overrides mapped onto [`PolicyConfig`], or the
    /// SPEC-P13 §3 defaults when the block is absent.
    ///
    /// # Errors
    /// [`ScenarioError::Invalid`] on unparseable overrides.
    pub fn policy_config(&self) -> Result<PolicyConfig, ScenarioError> {
        let cfg = match &self.policy {
            None => PolicyConfig {
                market_allowlist: vec![MarketId(32)],
                max_order_size_usd: Decimal::new(1000, 0),
                max_daily_actions: 10,
                require_approval_above_usd: Decimal::new(500, 0),
                kill_switch: false,
            },
            Some(value) => {
                let overrides: PolicyOverrides = serde_json::from_value(value.clone())
                    .map_err(|err| ScenarioError::Invalid(format!("policy overrides: {err}")))?;
                PolicyConfig {
                    market_allowlist: overrides
                        .market_allowlist
                        .into_iter()
                        .map(MarketId)
                        .collect(),
                    max_order_size_usd: overrides.max_order_size_usd,
                    max_daily_actions: overrides.max_daily_actions,
                    require_approval_above_usd: overrides.require_approval_above_usd,
                    kill_switch: overrides.kill_switch,
                }
            }
        };
        Ok(cfg)
    }

    /// Validate the price path: non-negative, strictly increasing ts, marks > 0.
    fn validate_path(&self) -> Result<(), ScenarioError> {
        let mut previous: Option<i64> = None;
        for (index, tick) in self.price_path.iter().enumerate() {
            if tick.ts_ms < 0 {
                return Err(ScenarioError::Invalid(format!(
                    "price_path[{index}]: ts_ms {} must be non-negative",
                    tick.ts_ms
                )));
            }
            if let Some(previous) = previous
                && tick.ts_ms <= previous
            {
                return Err(ScenarioError::Invalid(format!(
                    "price_path must be strictly increasing in ts_ms \
                     (index {index}: {} <= {previous})",
                    tick.ts_ms
                )));
            }
            if tick.mark_price <= Decimal::ZERO {
                return Err(ScenarioError::Invalid(format!(
                    "price_path[{index}]: mark_price {} must be > 0",
                    tick.mark_price
                )));
            }
            previous = Some(tick.ts_ms);
        }
        Ok(())
    }

    /// Every market_id used must exist in `markets` (SPEC-P13 §3).
    fn validate_market_refs(&self) -> Result<(), ScenarioError> {
        let known: Vec<MarketId> = self.markets.iter().map(|market| market.id).collect();
        for (index, tick) in self.price_path.iter().enumerate() {
            if !known.contains(&MarketId(tick.market_id)) {
                return Err(ScenarioError::Invalid(format!(
                    "price_path[{index}]: market_id {} not in markets",
                    tick.market_id
                )));
            }
        }
        for (index, position) in self.positions.iter().enumerate() {
            if !known.contains(&position.market_id) {
                return Err(ScenarioError::Invalid(format!(
                    "positions[{index}]: market_id {} not in markets",
                    position.market_id.0
                )));
            }
        }
        if let Some(trace) = &self.decision_trace {
            for (index, entry) in trace.iter().enumerate() {
                if !known.contains(&MarketId(entry.market_id)) {
                    return Err(ScenarioError::Invalid(format!(
                        "decision_trace[{index}]: market_id {} not in markets",
                        entry.market_id
                    )));
                }
            }
        }
        Ok(())
    }

    /// `feed_stale` windows must be well-formed; other kinds are accepted
    /// as-is (ignored in v1.0, SPEC-P13 §3).
    fn validate_events(&self) -> Result<(), ScenarioError> {
        for (index, event) in self.events.iter().enumerate() {
            if event.kind == EventKind::FeedStale {
                match event.until_ms {
                    Some(until) if until > event.ts_ms => {}
                    Some(until) => {
                        return Err(ScenarioError::Invalid(format!(
                            "events[{index}]: feed_stale until_ms {until} must be > ts_ms {}",
                            event.ts_ms
                        )));
                    }
                    None => {
                        return Err(ScenarioError::Invalid(format!(
                            "events[{index}]: feed_stale requires until_ms"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Load + validate a scenario from disk (SPEC-P13 §3 validation rules).
///
/// # Errors
/// `ScenarioError` on io/json/validation failure.
pub fn load(path: &Path) -> Result<Scenario, ScenarioError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| ScenarioError::Io(format!("{}: {err}", path.display())))?;
    let scenario: Scenario = serde_json::from_str(&text)
        .map_err(|err| ScenarioError::Json(format!("{}: {err}", path.display())))?;
    scenario.validate()?;
    Ok(scenario)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("valid decimal literal")
    }

    /// Minimal ETH market document on Perpl testnet (market 32).
    fn market_json() -> Value {
        json!({
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
        })
    }

    /// Matching long position document.
    fn position_json() -> Value {
        json!({
            "market_id": 32,
            "symbol": "ETH",
            "size": "1",
            "entry_price": "3000",
            "mark_price": "3000",
            "liq_price": null,
            "collateral": "100",
            "unrealized_pnl": "0",
            "margin_ratio": null,
            "leverage": "10",
            "opened_at": null
        })
    }

    /// Baseline valid scenario document (SPEC-P13 §3 example shape).
    fn base_json() -> Value {
        json!({
            "id": "unit-test",
            "label": "synthetic",
            "description": "unit test scenario",
            "start_free_balance": "10000",
            "markets": [market_json()],
            "positions": [position_json()],
            "price_path": [
                {"ts_ms": 1000, "market_id": 32, "mark_price": "3000.00"}
            ]
        })
    }

    fn parse(value: Value) -> Result<Scenario, ScenarioError> {
        Scenario::parse(&value.to_string())
    }

    #[test]
    fn valid_scenario_parses_with_production_defaults() {
        let scenario = parse(base_json()).expect("valid scenario");
        assert_eq!(scenario.id, "unit-test");
        assert_eq!(scenario.label, ScenarioLabel::Synthetic);
        assert_eq!(scenario.label.as_str(), "synthetic");

        let reflex = scenario.reflex_config().expect("reflex defaults");
        assert_eq!(reflex.reduce_fraction, d("0.5"));
        assert_eq!(reflex.orange_fraction, d("0.25"));
        assert_eq!(reflex.cooldown_ms, 600_000);
        assert!(!reflex.stale_reduce);

        let policy = scenario.policy_config().expect("policy defaults");
        assert_eq!(policy.market_allowlist, vec![MarketId(32)]);
        assert_eq!(policy.max_order_size_usd, d("1000"));
        assert_eq!(policy.max_daily_actions, 10);
        assert_eq!(policy.require_approval_above_usd, d("500"));
        assert!(!policy.kill_switch);
    }

    #[test]
    fn overrides_replace_defaults_field_by_field() {
        let mut raw = base_json();
        raw["reflex"] = json!({"reduce_fraction": "1", "cooldown_ms": 0});
        raw["policy"] = json!({"market_allowlist": [32, 20], "max_daily_actions": 3});
        let scenario = parse(raw).expect("valid overrides");

        let reflex = scenario.reflex_config().expect("reflex");
        assert_eq!(reflex.reduce_fraction, d("1"), "fraction 1 is inclusive");
        assert_eq!(
            reflex.orange_fraction,
            d("0.25"),
            "missing field keeps default"
        );
        assert_eq!(reflex.cooldown_ms, 0);
        assert!(!reflex.stale_reduce);

        let policy = scenario.policy_config().expect("policy");
        assert_eq!(policy.market_allowlist, vec![MarketId(32), MarketId(20)]);
        assert_eq!(policy.max_daily_actions, 3);
        assert_eq!(
            policy.max_order_size_usd,
            d("1000"),
            "missing keeps default"
        );
    }

    #[test]
    fn price_path_must_be_strictly_increasing() {
        let mut raw = base_json();
        raw["price_path"] = json!([
            {"ts_ms": 1000, "market_id": 32, "mark_price": "3000"},
            {"ts_ms": 1000, "market_id": 32, "mark_price": "2999"}
        ]);
        let error = parse(raw).expect_err("duplicate ts must fail");
        assert!(
            error.to_string().contains("strictly increasing"),
            "unexpected: {error}"
        );

        let mut raw = base_json();
        raw["price_path"] = json!([
            {"ts_ms": 2000, "market_id": 32, "mark_price": "3000"},
            {"ts_ms": 1000, "market_id": 32, "mark_price": "2999"}
        ]);
        assert!(parse(raw).is_err(), "decreasing ts must fail");
    }

    #[test]
    fn every_market_id_must_exist_in_markets() {
        let mut raw = base_json();
        raw["price_path"] = json!([{"ts_ms": 1000, "market_id": 99, "mark_price": "3000"}]);
        let error = parse(raw).expect_err("unknown path market");
        assert!(
            error.to_string().contains("not in markets"),
            "unexpected: {error}"
        );

        let mut raw = base_json();
        let mut position = position_json();
        position["market_id"] = json!(99);
        raw["positions"] = json!([position]);
        let error = parse(raw).expect_err("unknown position market");
        assert!(
            error.to_string().contains("not in markets"),
            "unexpected: {error}"
        );

        let mut raw = base_json();
        raw["decision_trace"] = json!([{
            "at_ms": 1000,
            "market_id": 99,
            "decision": {
                "action": "HOLD", "market_id": 99, "amount": null,
                "confidence": "0.5", "urgency": "ROUTINE", "reason": "test"
            }
        }]);
        let error = parse(raw).expect_err("unknown trace market");
        assert!(
            error.to_string().contains("not in markets"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn mark_price_must_be_positive() {
        let mut raw = base_json();
        raw["price_path"] = json!([{"ts_ms": 1000, "market_id": 32, "mark_price": "0"}]);
        let error = parse(raw).expect_err("zero mark");
        assert!(error.to_string().contains("> 0"), "unexpected: {error}");
    }

    #[test]
    fn reflex_fractions_must_be_in_unit_interval() {
        for (fraction, valid) in [("0", false), ("-0.5", false), ("1.5", false), ("1", true)] {
            let mut raw = base_json();
            raw["reflex"] = json!({"reduce_fraction": fraction});
            let result = parse(raw);
            assert_eq!(
                result.is_ok(),
                valid,
                "reduce_fraction {fraction}: {result:?}"
            );
        }
        for (fraction, valid) in [("0", false), ("1.0001", false), ("0.25", true)] {
            let mut raw = base_json();
            raw["reflex"] = json!({"orange_fraction": fraction});
            let result = parse(raw);
            assert_eq!(
                result.is_ok(),
                valid,
                "orange_fraction {fraction}: {result:?}"
            );
        }
    }

    #[test]
    fn at_least_one_position_and_one_tick_required() {
        let mut raw = base_json();
        raw["positions"] = json!([]);
        let error = parse(raw).expect_err("no positions");
        assert!(
            error.to_string().contains("position"),
            "unexpected: {error}"
        );

        let mut raw = base_json();
        raw["price_path"] = json!([]);
        let error = parse(raw).expect_err("no ticks");
        assert!(error.to_string().contains("tick"), "unexpected: {error}");
    }

    #[test]
    fn feed_stale_window_is_validated() {
        let mut raw = base_json();
        raw["events"] = json!([{"ts_ms": 1000, "kind": "feed_stale"}]);
        let error = parse(raw).expect_err("missing until_ms");
        assert!(
            error.to_string().contains("until_ms"),
            "unexpected: {error}"
        );

        let mut raw = base_json();
        raw["events"] = json!([{"ts_ms": 1000, "kind": "feed_stale", "until_ms": 1000}]);
        assert!(parse(raw).is_err(), "empty window must fail");

        let mut raw = base_json();
        raw["events"] = json!([
            {"ts_ms": 0, "kind": "feed_stale", "until_ms": 5000},
            {"ts_ms": 0, "kind": "funding", "note": "accepted and ignored"}
        ]);
        assert!(parse(raw).is_ok(), "well-formed events must pass");
    }

    #[test]
    fn load_reads_and_validates_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scenario.json");
        let scenario = base_json();
        std::fs::write(&path, scenario.to_string()).expect("write scenario");

        let loaded = load(&path).expect("load");
        assert_eq!(loaded.id, "unit-test");
        assert_eq!(loaded.label, ScenarioLabel::Synthetic);
        // Scenario::load delegates to the free function.
        assert_eq!(Scenario::load(&path).expect("load"), loaded);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let error = load(Path::new("/nonexistent/p13/scenario.json")).expect_err("must fail");
        assert!(
            matches!(error, ScenarioError::Io(_)),
            "unexpected: {error:?}"
        );
    }

    #[test]
    fn malformed_json_is_a_json_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{ not json").expect("write");
        let error = load(&path).expect_err("must fail");
        assert!(
            matches!(error, ScenarioError::Json(_)),
            "unexpected: {error:?}"
        );
    }

    #[test]
    fn unknown_event_kind_is_a_json_error() {
        let mut raw = base_json();
        raw["events"] = json!([{"ts_ms": 0, "kind": "not_a_kind"}]);
        let error = parse(raw).expect_err("unknown kind");
        assert!(
            matches!(error, ScenarioError::Json(_)),
            "unexpected: {error:?}"
        );
    }
}
