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
        let confidence = self.confidence;
        if confidence < Decimal::ZERO || confidence > Decimal::ONE {
            return Err(format!(
                "confidence {confidence} is outside the allowed range [0, 1]"
            ));
        }

        let market_id = self.market_id;
        if !allowed_markets.iter().any(|market| market.0 == market_id) {
            return Err(format!(
                "market_id {market_id} is not in the allowed market list"
            ));
        }

        if matches!(
            self.action,
            DecisionAction::Reduce | DecisionAction::AddCollateral
        ) {
            let action = if self.action == DecisionAction::Reduce {
                "REDUCE"
            } else {
                "ADD_COLLATERAL"
            };
            match self.amount {
                Some(amount) if amount > Decimal::ZERO => {}
                Some(amount) => {
                    return Err(format!(
                        "amount {amount} must be greater than 0 for {action}"
                    ));
                }
                None => {
                    return Err(format!(
                        "amount is required for {action} and must be greater than 0"
                    ));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod decisions {
    use rust_decimal_macros::dec;
    use serde_json::{Value, json};

    use super::*;

    /// Markets a consultation may target (Perpl testnet 32 + mainnet 20).
    const ALLOWED: [MarketId; 2] = [MarketId(20), MarketId(32)];

    /// Baseline schema-v3 decision on the happy path (HOLD, amount `null`).
    fn sample(action: DecisionAction, amount: Option<Decimal>) -> Decision {
        Decision {
            action,
            market_id: 32,
            amount,
            confidence: dec!(0.72),
            urgency: Urgency::Routine,
            reason: "grounded in the provided snapshot".to_string(),
        }
    }

    /// Baseline wire JSON; tweak fields per test before parsing.
    fn base_json() -> Value {
        json!({
            "action": "HOLD",
            "market_id": 32,
            "amount": null,
            "confidence": 0.72,
            "urgency": "ROUTINE",
            "reason": "grounded"
        })
    }

    #[test]
    fn serde_action_variants_round_trip() {
        let cases = [
            (DecisionAction::Hold, "HOLD"),
            (DecisionAction::Reduce, "REDUCE"),
            (DecisionAction::Close, "CLOSE"),
            (DecisionAction::AddCollateral, "ADD_COLLATERAL"),
            (DecisionAction::Escalate, "ESCALATE"),
        ];
        for (variant, wire) in cases {
            let decoded: DecisionAction = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(decoded, variant);

            let decision = sample(variant, None);
            let value = serde_json::to_value(&decision).unwrap();
            assert_eq!(value["action"].as_str(), Some(wire));
            let round: Decision = serde_json::from_value(value).unwrap();
            assert_eq!(round, decision);
        }
    }

    #[test]
    fn serde_urgency_variants_round_trip() {
        let cases = [
            (Urgency::Routine, "ROUTINE"),
            (Urgency::Elevated, "ELEVATED"),
            (Urgency::Critical, "CRITICAL"),
        ];
        for (variant, wire) in cases {
            let decoded: Urgency = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(decoded, variant);

            let mut decision = sample(DecisionAction::Hold, None);
            decision.urgency = variant;
            let value = serde_json::to_value(&decision).unwrap();
            assert_eq!(value["urgency"].as_str(), Some(wire));
            let round: Decision = serde_json::from_value(value).unwrap();
            assert_eq!(round, decision);
        }
    }

    #[test]
    fn serde_amount_as_string_parses() {
        let mut raw = base_json();
        raw["action"] = json!("ADD_COLLATERAL");
        raw["amount"] = json!("1.25");
        let decision: Decision = serde_json::from_value(raw).unwrap();
        assert_eq!(decision.amount, Some(dec!(1.25)));
        assert_eq!(decision.action, DecisionAction::AddCollateral);

        // The decision re-serializes the amount as a string.
        let value = serde_json::to_value(&decision).unwrap();
        assert_eq!(value["amount"].as_str(), Some("1.25"));
    }

    #[test]
    fn serde_amount_as_json_number_parses() {
        // SPEC-P07 §2: string on the wire, but the tolerant parse also accepts
        // a JSON number.
        let mut raw = base_json();
        raw["action"] = json!("REDUCE");
        raw["amount"] = json!(1.25);
        let decision: Decision = serde_json::from_value(raw).unwrap();
        assert_eq!(decision.amount, Some(dec!(1.25)));
    }

    #[test]
    fn serde_amount_null_or_missing_ok_for_hold() {
        let with_null: Decision = serde_json::from_value(base_json()).unwrap();
        assert_eq!(with_null.amount, None);

        let mut missing = base_json();
        missing.as_object_mut().unwrap().remove("amount");
        let without_amount: Decision = serde_json::from_value(missing).unwrap();
        assert_eq!(without_amount.amount, None);

        // `None` serializes back to JSON null.
        let value = serde_json::to_value(&without_amount).unwrap();
        assert_eq!(value["amount"], Value::Null);
    }

    #[test]
    fn serde_unknown_fields_are_ignored() {
        let mut raw = base_json();
        raw["future_field"] = json!({"nested": [1, 2, 3]});
        raw["llm_notes"] = json!("whatever the model added");
        let decision: Decision = serde_json::from_value(raw).unwrap();
        assert_eq!(decision.action, DecisionAction::Hold);
        assert_eq!(decision.market_id, 32);
        assert_eq!(decision.confidence, dec!(0.72));
        assert_eq!(decision.urgency, Urgency::Routine);
    }

    #[test]
    fn serde_missing_confidence_is_an_error() {
        let mut raw = base_json();
        raw.as_object_mut().unwrap().remove("confidence");
        let error = serde_json::from_value::<Decision>(raw).expect_err("confidence is required");
        assert!(
            error.to_string().contains("confidence"),
            "unexpected message: {error}"
        );
    }

    #[test]
    fn validate_confidence_must_be_unit_interval() {
        let mut low = sample(DecisionAction::Hold, None);
        low.confidence = dec!(0);
        assert!(low.validate(&ALLOWED).is_ok(), "confidence 0 is inclusive");

        let mut high = sample(DecisionAction::Hold, None);
        high.confidence = dec!(1);
        assert!(high.validate(&ALLOWED).is_ok(), "confidence 1 is inclusive");

        high.confidence = dec!(1.01);
        let error = high
            .validate(&ALLOWED)
            .expect_err("1.01 is above the range");
        assert!(error.contains("confidence"), "unexpected message: {error}");

        low.confidence = dec!(-0.01);
        let error = low
            .validate(&ALLOWED)
            .expect_err("-0.01 is below the range");
        assert!(error.contains("confidence"), "unexpected message: {error}");
    }

    #[test]
    fn validate_reduce_requires_positive_amount() {
        let error = sample(DecisionAction::Reduce, None)
            .validate(&ALLOWED)
            .expect_err("REDUCE requires an amount");
        assert!(error.contains("amount"), "unexpected message: {error}");

        let error = sample(DecisionAction::Reduce, Some(dec!(0)))
            .validate(&ALLOWED)
            .expect_err("zero is not a positive amount");
        assert!(error.contains("amount"), "unexpected message: {error}");

        let error = sample(DecisionAction::Reduce, Some(dec!(-1)))
            .validate(&ALLOWED)
            .expect_err("negative amounts are rejected");
        assert!(error.contains("amount"), "unexpected message: {error}");

        assert!(
            sample(DecisionAction::Reduce, Some(dec!(0.001)))
                .validate(&ALLOWED)
                .is_ok(),
            "smallest positive amount passes"
        );
    }

    #[test]
    fn validate_add_collateral_requires_positive_amount() {
        let error = sample(DecisionAction::AddCollateral, None)
            .validate(&ALLOWED)
            .expect_err("ADD_COLLATERAL requires an amount");
        assert!(error.contains("amount"), "unexpected message: {error}");

        let error = sample(DecisionAction::AddCollateral, Some(dec!(0)))
            .validate(&ALLOWED)
            .expect_err("zero is not a positive amount");
        assert!(error.contains("amount"), "unexpected message: {error}");

        let error = sample(DecisionAction::AddCollateral, Some(dec!(-1)))
            .validate(&ALLOWED)
            .expect_err("negative amounts are rejected");
        assert!(error.contains("amount"), "unexpected message: {error}");

        assert!(
            sample(DecisionAction::AddCollateral, Some(dec!(0.001)))
                .validate(&ALLOWED)
                .is_ok(),
            "smallest positive amount passes"
        );
    }

    #[test]
    fn validate_amount_ignored_for_hold_close_escalate() {
        // The amount rule applies only to REDUCE and ADD_COLLATERAL.
        assert!(
            sample(DecisionAction::Hold, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::Close, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::Escalate, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        // Values present on those actions are ignored entirely.
        assert!(
            sample(DecisionAction::Hold, Some(dec!(-1)))
                .validate(&ALLOWED)
                .is_ok()
        );
    }

    #[test]
    fn validate_market_must_be_allowed() {
        assert!(
            sample(DecisionAction::Hold, None)
                .validate(&ALLOWED)
                .is_ok(),
            "market 32 is in the allowlist"
        );

        let mut other = sample(DecisionAction::Hold, None);
        other.market_id = 20;
        assert!(
            other.validate(&ALLOWED).is_ok(),
            "market 20 is in the allowlist"
        );

        other.market_id = 999;
        let error = other
            .validate(&ALLOWED)
            .expect_err("market 999 is not in the allowlist");
        assert!(error.contains("market_id"), "unexpected message: {error}");
        assert!(error.contains("999"), "unexpected message: {error}");

        // An empty allowlist rejects every market.
        assert!(sample(DecisionAction::Hold, None).validate(&[]).is_err());
    }

    #[test]
    fn validate_reports_the_first_violated_rule() {
        // Out-of-range confidence wins over an unknown market and a bad amount.
        let mut broken = sample(DecisionAction::Reduce, None);
        broken.confidence = dec!(2);
        broken.market_id = 777;
        let error = broken.validate(&ALLOWED).expect_err("must fail");
        assert!(error.contains("confidence"), "unexpected message: {error}");

        // An unknown market wins over the amount rule.
        let mut broken = sample(DecisionAction::Reduce, None);
        broken.market_id = 777;
        let error = broken.validate(&ALLOWED).expect_err("must fail");
        assert!(error.contains("market_id"), "unexpected message: {error}");
    }

    #[test]
    fn validate_accepts_valid_decisions() {
        assert!(
            sample(DecisionAction::Hold, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::Close, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::Escalate, None)
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::Reduce, Some(dec!(1.25)))
                .validate(&ALLOWED)
                .is_ok()
        );
        assert!(
            sample(DecisionAction::AddCollateral, Some(dec!(500)))
                .validate(&ALLOWED)
                .is_ok()
        );
    }
}
