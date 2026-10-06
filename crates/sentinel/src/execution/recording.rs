//! Recording executor — pass-through that persists request/report pairs.
//!
//! Used by tests and demos to capture exactly what the pipeline decided.
//!
//! **P05 status:** implemented; interfaces frozen per `SPEC-P05.md` §5.

use std::path::PathBuf;

use sentinel_core::order::OrderRequest;
use serde::Serialize;

use super::{
    ExecutionReport, ExecutionStatus, Executor, SeqCounter, append_jsonl, client_order_id, unix_ms,
};
use crate::error::Result;

/// One JSONL entry: the request as submitted plus the report it produced.
#[derive(Debug, Serialize)]
struct RecordedEntry<'a> {
    /// Order request that was recorded.
    request: &'a OrderRequest,
    /// Report synthesized for it.
    report: &'a ExecutionReport,
}

/// Persists `{request, report}` JSONL lines; never touches the network.
///
/// The synthesized report is always
/// [`ExecutionStatus::Simulated`] with a
/// full fill and no price (`avg_price = None`); the point is the persisted
/// record of what was submitted, not a venue outcome.
pub struct RecordingExecutor {
    path: PathBuf,
    seq: SeqCounter,
}

impl RecordingExecutor {
    /// Build the recorder writing to `path`.
    ///
    /// Every recorded submission appends one JSON line (parent directories
    /// are created on demand). `seq_seed` seeds the client-order-id counter,
    /// so the first id is `sentinel-<market>-<seed+1>`.
    pub fn new(path: PathBuf, seq_seed: u64) -> Self {
        Self {
            path,
            seq: SeqCounter::new(seq_seed),
        }
    }
}

impl Executor for RecordingExecutor {
    async fn submit(&self, order: &OrderRequest) -> Result<ExecutionReport> {
        let report = ExecutionReport {
            order: order.clone(),
            status: ExecutionStatus::Simulated,
            filled_size: order.size,
            avg_price: None,
            tx_hash: None,
            client_order_id: client_order_id(order, self.seq.next()),
            detail: "recorded".to_string(),
            ts_ms: unix_ms(),
        };
        append_jsonl(
            &self.path,
            &RecordedEntry {
                request: order,
                report: &report,
            },
        )?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use sentinel_core::order::{CloseSide, OrderType};
    use sentinel_core::types::MarketId;

    use super::*;
    use rust_decimal::Decimal;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).expect("valid decimal literal")
    }

    fn order() -> OrderRequest {
        OrderRequest {
            market_id: MarketId(32),
            close: CloseSide::CloseLong,
            size: d("0.5"),
            order_type: OrderType::Market,
            max_slippage_bps: 10,
            size_decimals: 3,
        }
    }

    fn decimal_at(value: &serde_json::Value, pointer: &str) -> Decimal {
        serde_json::from_value(value.pointer(pointer).cloned().expect("field present"))
            .expect("decimal field")
    }

    #[tokio::test]
    async fn records_request_and_report_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recording.jsonl");
        let executor = RecordingExecutor::new(path.clone(), 7);

        let order = order();
        let report = executor.submit(&order).await.expect("recorded submit");
        assert_eq!(report.status, ExecutionStatus::Simulated);
        assert_eq!(report.filled_size, order.size);
        assert_eq!(report.avg_price, None);
        assert_eq!(report.tx_hash, None);
        assert_eq!(
            report.client_order_id, "sentinel-32-8",
            "seed 7 → first id 8"
        );
        assert_eq!(report.detail, "recorded");

        let text = std::fs::read_to_string(&path).expect("recording written");
        assert_eq!(text.lines().count(), 1);
        let line: serde_json::Value = serde_json::from_str(text.trim()).expect("entry JSON");

        // Request half.
        assert_eq!(line["request"]["market_id"], 32);
        assert_eq!(line["request"]["close"], "CloseLong");
        assert_eq!(line["request"]["order_type"], "Market");
        assert_eq!(line["request"]["max_slippage_bps"], 10);
        assert_eq!(line["request"]["size_decimals"], 3);
        assert_eq!(decimal_at(&line, "/request/size"), order.size);

        // Report half.
        assert_eq!(line["report"]["status"], "Simulated");
        assert_eq!(decimal_at(&line, "/report/filled_size"), order.size);
        assert!(line["report"]["avg_price"].is_null());
        assert!(line["report"]["tx_hash"].is_null());
        assert_eq!(line["report"]["detail"], "recorded");
        assert_eq!(line["report"]["client_order_id"], "sentinel-32-8");
        assert!(line["report"]["ts_ms"].as_u64().is_some_and(|ts| ts > 0));
    }

    #[tokio::test]
    async fn appends_one_line_per_submission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recording.jsonl");
        let executor = RecordingExecutor::new(path.clone(), 7);

        let first = executor.submit(&order()).await.expect("first");
        let second = executor.submit(&order()).await.expect("second");
        assert_eq!(first.client_order_id, "sentinel-32-8");
        assert_eq!(second.client_order_id, "sentinel-32-9");

        let text = std::fs::read_to_string(&path).expect("recording written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "append, never overwrite");
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("entry JSON");
            assert_eq!(value["report"]["status"], "Simulated");
        }
    }
}
