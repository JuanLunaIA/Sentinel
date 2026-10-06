//! Nansen x402 client — cache-first, budget-capped, ledger-audited.
//!
//! Frozen by `SPEC-P09.md` §3.5: the self-paying agent. Cache hits cost
//! nothing; the spend ledger is the budget source of truth; failures degrade
//! to `SmartMoneyContext::unavailable` and never stall the caller.
//!
//! **P09 status:** implemented (`orchestrator` agent); interfaces frozen.

pub mod cache;
pub mod client;
pub mod spend;
pub mod x402;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rust_decimal::Decimal;

use crate::brain::prompts::SmartMoneyContext;
use crate::config::NansenConfig;
use crate::error::{NansenError, Result, SentinelError};

use cache::TtlCache;
use spend::{SpendEntry, SpendLedger};
use x402::PayerSigner;

/// Default ledger location (gitignored `data/`).
pub const SPEND_LEDGER_PATH: &str = "data/nansen-spend.jsonl";

/// Sliding budget window for `NANSEN_MAX_CALLS_PER_HOUR` (one hour, ms).
pub const BUDGET_WINDOW_MS: u64 = 3_600_000;

/// Per-call HTTP timeout (connect + response), seconds.
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Honesty note attached to every successful smart-money context: Nansen
/// coverage is cross-venue, so the netflow is a directional proxy only
/// (`SPEC-P09.md` §3.5).
pub const SMART_MONEY_PROXY_NOTE: &str =
    "cross-venue smart-money signal as directional proxy (Nansen coverage is not Perpl-specific)";

/// `POST` endpoint: smart-money netflow (`$0.05` rail at the recorded challenge).
pub const ENDPOINT_NETFLOW: &str = "/api/v1/smart-money/netflow";

/// `POST` endpoint: smart-money holdings (`$0.05` rail at the recorded challenge).
pub const ENDPOINT_HOLDINGS: &str = "/api/v1/smart-money/holdings";

/// `POST` endpoint: perp leaderboard (`$0.05` rail at the recorded challenge).
pub const ENDPOINT_LEADERBOARD: &str = "/api/v1/perp-leaderboard";

/// `POST` endpoint: perp positions by address (`$0.01` rail at the recorded
/// challenge).
pub const ENDPOINT_PERP_POSITIONS: &str = "/api/v1/profiler/perp-positions";

/// Per-call metadata returned alongside the raw payload.
#[derive(Debug, Clone, PartialEq)]
pub struct CallMeta {
    /// Endpoint path called.
    pub endpoint: String,
    /// Cost of this call, USD (`0` when cached).
    pub cost_usd: Decimal,
    /// Settlement tx hash when the facilitator reported one.
    pub tx_hash: Option<String>,
    /// True when served from the cache.
    pub cached: bool,
}

/// The paying client.
pub struct NansenClient {
    http: reqwest::Client,
    signer: PayerSigner,
    base_url: String,
    network: String,
    cache: Mutex<TtlCache>,
    ledger: SpendLedger,
    max_calls_per_hour: u32,
}

impl NansenClient {
    /// Build from configuration (`NANSEN_PAYER_KEY` must be a real 0x key at
    /// call time; construction validates its shape only).
    ///
    /// # Errors
    /// `NansenError::Sign` on a malformed payer key; `SentinelError::Internal`
    /// when the HTTP client cannot be built.
    pub fn new(cfg: &NansenConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| SentinelError::Internal(format!("nansen http client: {e}")))?;
        let signer = PayerSigner::from_hex(cfg.payer_key.expose())?;
        Ok(Self {
            http,
            signer,
            base_url: cfg.base_url.clone(),
            network: cfg.payment_network.clone(),
            cache: Mutex::new(TtlCache::new(cfg.cache_ttl_secs.saturating_mul(1000))),
            ledger: SpendLedger::new(SPEND_LEDGER_PATH),
            max_calls_per_hour: cfg.max_calls_per_hour,
        })
    }

    /// Override the ledger path (tests).
    pub fn with_ledger_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.ledger = SpendLedger::new(path);
        self
    }

    /// Payer (x402 wallet) address, `0x…`.
    pub fn address(&self) -> String {
        self.signer.address()
    }

    /// Cache-first paid call to `endpoint` with `body`.
    ///
    /// Order (`SPEC-P09.md` §3.5): cache hit ⇒ free; else the sliding-hour
    /// budget is checked against the ledger **before** any HTTP; else the x402
    /// flow runs, the settled entry is appended and the response cached.
    ///
    /// # Errors
    /// `NansenError::Budget` before any HTTP when the sliding hour is full;
    /// `{Challenge,Sign,Retry}` from the x402 flow.
    pub async fn call_endpoint(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
        now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        // 1. Cache hit: zero cost, no budget left, no network.
        let key = cache::key(endpoint, body);
        if let Some(value) = self.lock_cache().get(&key, now_ms) {
            return Ok((
                value,
                CallMeta {
                    endpoint: endpoint.to_string(),
                    cost_usd: Decimal::ZERO,
                    tx_hash: None,
                    cached: true,
                },
            ));
        }

        // 2. Budget: the ledger is the single source of truth.
        let spent = self.ledger.calls_since(now_ms, BUDGET_WINDOW_MS);
        if spent >= self.max_calls_per_hour {
            return Err(NansenError::Budget {
                spent,
                limit: self.max_calls_per_hour,
            }
            .into());
        }

        // 3. Pay via the x402 v2 flow (unpaid POST → 402 → sign → retry).
        let url = format!("{}{endpoint}", self.base_url.trim_end_matches('/'));
        let paid =
            x402::fetch_with_payment(&self.http, &url, body, &self.signer, &self.network, now_ms)
                .await?;

        // 4. Audit the settled purchase (tx hash only when reported).
        let tx_hash = paid.settlement.as_ref().map(|s| s.transaction.clone());
        self.ledger.append(&SpendEntry {
            ts_ms: now_ms,
            endpoint: endpoint.to_string(),
            cost_usd: paid.cost_usd.to_string(),
            tx_hash: tx_hash.clone(),
            payer: Some(self.signer.address()),
            network: Some(paid.rail_network),
        })?;

        // 5. Cache the payload for the TTL window.
        self.lock_cache().put(&key, paid.body.clone(), now_ms);

        // 6. Hand back the payload plus the spend metadata.
        Ok((
            paid.body,
            CallMeta {
                endpoint: endpoint.to_string(),
                cost_usd: paid.cost_usd,
                tx_hash,
                cached: false,
            },
        ))
    }

    /// Paid `sm_netflow` wrapper.
    ///
    /// # Errors
    /// Same as [`NansenClient::call_endpoint`].
    pub async fn sm_netflow(
        &self,
        chain: &str,
        now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        let body = client::netflow_body(chain);
        self.call_endpoint(ENDPOINT_NETFLOW, &body, now_ms).await
    }

    /// Paid `perp_positions` wrapper.
    ///
    /// # Errors
    /// Same as [`NansenClient::call_endpoint`].
    pub async fn perp_positions(
        &self,
        addresses: &[String],
        now_ms: u64,
    ) -> Result<(serde_json::Value, CallMeta)> {
        let body = client::perp_positions_body(addresses);
        self.call_endpoint(ENDPOINT_PERP_POSITIONS, &body, now_ms)
            .await
    }

    /// Best-effort smart-money context for `asset` (**never fails**).
    ///
    /// Fills `netflow_24h`/`fetched_at_ms`/`total_cost_usd` and always sets
    /// the cross-venue proxy `note` (SPEC-P09 §3.5 honesty requirement).
    /// On any error the caller gets [`SmartMoneyContext::unavailable`] and a
    /// `warn!` — the reasoning loop must never stall on paid data.
    pub async fn smart_money_context(
        &self,
        asset: &str,
        chain: &str,
        now_ms: u64,
    ) -> SmartMoneyContext {
        match self.sm_netflow(chain, now_ms).await {
            Ok((resp, meta)) => {
                let mut sm = SmartMoneyContext::unavailable(asset);
                sm.netflow_24h = client::extract_decimal(&resp, client::NETFLOW_KEYS);
                sm.fetched_at_ms = Some(now_ms);
                sm.total_cost_usd = Some(meta.cost_usd);
                sm.note = Some(SMART_MONEY_PROXY_NOTE.to_string());
                sm
            }
            Err(err) => {
                tracing::warn!(
                    asset = %asset,
                    chain = %chain,
                    error = %err,
                    "nansen smart-money context unavailable; degrading to placeholder"
                );
                SmartMoneyContext::unavailable(asset)
            }
        }
    }

    /// Lock the response cache, recovering a poisoned mutex: a cache must
    /// never take the client down.
    fn lock_cache(&self) -> MutexGuard<'_, TtlCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use base64::Engine as _;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::Config;

    /// A syntactically valid (non-placeholder) test payer key: 32×`0x07`.
    fn test_key() -> String {
        format!("0x{}", "07".repeat(32))
    }

    /// Synthetic settlement transaction hash: 32×`0xab`.
    fn tx_hash() -> String {
        format!("0x{}", "ab".repeat(32))
    }

    const NOW_MS: u64 = 1_762_000_000_000;
    const LATER_MS: u64 = NOW_MS + 60_000;
    const QUERY_ADDRESS: &str = "0x1111111111111111111111111111111111111111";

    /// Recorded 402 challenge for the netflow endpoint (Monad rail $0.05).
    const NETFLOW_402_FIXTURE: &str = include_str!("../../../../docs/evidence/p01-nansen-402.json");
    /// Recorded 402 challenge for the profiler endpoint (Monad rail $0.01).
    const PROFILER_402_FIXTURE: &str =
        include_str!("../../../../docs/evidence/p01-nansen-402-profiler-perp-positions.json");

    fn fixture_value(fixture: &str) -> Value {
        serde_json::from_str(fixture).expect("recorded challenge parses")
    }

    fn b64_of(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    }

    fn settlement_b64(payer: &str) -> String {
        b64_of(
            &serde_json::to_string(&json!({
                "success": true,
                "transaction": tx_hash(),
                "network": "eip155:143",
                "payer": payer,
            }))
            .expect("settlement serialises"),
        )
    }

    /// `NansenConfig` built through the real loader (var map → validation).
    fn test_nansen_config(server_uri: &str, max_calls_per_hour: u32) -> NansenConfig {
        let vars: HashMap<String, String> = [
            ("PERPL_ENV", "testnet".to_string()),
            ("PERPL_API_KEY", "perpl-test-token".to_string()),
            ("PERPL_API_KEY_SECRET", format!("0x{}", "00".repeat(32))),
            ("QWEN_API_KEY", "qwen-test-key".to_string()),
            ("KIMI_API_KEY", "kimi-test-key".to_string()),
            ("TELOXIDE_TOKEN", "123456:test-token".to_string()),
            ("TELEGRAM_ALLOWED_USER_IDS", "1,2".to_string()),
            ("NANSEN_BASE_URL", server_uri.to_string()),
            ("NANSEN_PAYER_KEY", test_key()),
            ("NANSEN_MAX_CALLS_PER_HOUR", max_calls_per_hour.to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
        Config::from_vars(vars).expect("test config").nansen
    }

    fn test_client(server: &MockServer, max_calls_per_hour: u32, ledger: &Path) -> NansenClient {
        NansenClient::new(&test_nansen_config(&server.uri(), max_calls_per_hour))
            .expect("client builds")
            .with_ledger_path(ledger)
    }

    /// Initial unpaid request ⇒ the recorded 402 (body + header).
    async fn mount_challenge(server: &MockServer, endpoint: &str, fixture: &str) {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(402)
                    .set_body_json(fixture_value(fixture))
                    .append_header(x402::HEADER_PAYMENT_REQUIRED, b64_of(fixture)),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    /// Signed retry (has `PAYMENT-SIGNATURE`) ⇒ 200 + settlement header.
    async fn mount_paid_call(
        server: &MockServer,
        endpoint: &str,
        response_body: Value,
        settlement: String,
    ) {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .and(header_exists("PAYMENT-SIGNATURE"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(response_body)
                    .append_header(x402::HEADER_PAYMENT_RESPONSE, settlement),
            )
            .with_priority(1)
            .expect(1)
            .mount(server)
            .await;
    }

    async fn request_count(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .map_or(0, |requests| requests.len())
    }

    #[tokio::test]
    async fn paid_then_cached_then_budget_breach() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let ledger_file = dir.path().join("nansen-spend.jsonl");
        let payer = PayerSigner::from_hex(&test_key())
            .expect("test key builds a signer")
            .address();

        mount_challenge(&server, ENDPOINT_PERP_POSITIONS, PROFILER_402_FIXTURE).await;
        mount_paid_call(
            &server,
            ENDPOINT_PERP_POSITIONS,
            json!({ "positions": [ { "market": "MON-ETH" } ] }),
            settlement_b64(&payer),
        )
        .await;

        let client = test_client(&server, 1, &ledger_file);
        let addresses = vec![QUERY_ADDRESS.to_string()];

        // 1st call: cache miss ⇒ pays the recorded $0.01 profiler rail and
        // lands exactly one ledger row.
        let (first, meta) = client
            .perp_positions(&addresses, NOW_MS)
            .await
            .expect("first call pays");
        assert!(!meta.cached);
        assert_eq!(meta.endpoint, ENDPOINT_PERP_POSITIONS);
        assert_eq!(meta.cost_usd, Decimal::new(1, 2));
        assert_eq!(meta.tx_hash.as_deref(), Some(tx_hash().as_str()));
        assert_eq!(first, json!({ "positions": [ { "market": "MON-ETH" } ] }));

        let entries = SpendLedger::new(&ledger_file).load();
        assert_eq!(entries.len(), 1, "one settled call ⇒ one ledger entry");
        let entry = &entries[0];
        assert_eq!(entry.ts_ms, NOW_MS);
        assert_eq!(entry.endpoint, ENDPOINT_PERP_POSITIONS);
        assert_eq!(entry.cost_usd, "0.01");
        assert_eq!(entry.tx_hash.as_deref(), Some(tx_hash().as_str()));
        assert_eq!(entry.payer.as_deref(), Some(payer.as_str()));
        assert_eq!(entry.network.as_deref(), Some("eip155:143"));

        // 2nd call within TTL: served from the cache — free, no ledger row,
        // no HTTP request, even though the 1-call budget is already spent.
        let (second, meta) = client
            .perp_positions(&addresses, LATER_MS)
            .await
            .expect("second call is cached");
        assert!(meta.cached);
        assert_eq!(meta.cost_usd, Decimal::ZERO);
        assert_eq!(meta.tx_hash, None, "cached meta carries no settlement");
        assert_eq!(second, first);
        assert_eq!(
            SpendLedger::new(&ledger_file).load().len(),
            1,
            "a cache hit must not append a ledger row"
        );
        assert_eq!(
            request_count(&server).await,
            2,
            "a cache hit must not touch the network"
        );

        // 3rd call, different cache key ⇒ miss; the exhausted budget refuses
        // it BEFORE any HTTP request goes out.
        let err = client
            .sm_netflow("ethereum", LATER_MS)
            .await
            .expect_err("exhausted budget must refuse the call");
        match err {
            SentinelError::Nansen(NansenError::Budget { spent, limit }) => {
                assert_eq!(spent, 1);
                assert_eq!(limit, 1);
            }
            other => panic!("expected NansenError::Budget, got {other:?}"),
        }
        assert_eq!(
            request_count(&server).await,
            2,
            "the budget refusal must be pre-HTTP"
        );

        // The wire-level shape: unpaid first, signed retry second.
        let requests = server.received_requests().await.expect("requests recorded");
        assert!(
            requests[0].headers.get("payment-signature").is_none(),
            "the first attempt is unpaid"
        );
        assert!(
            requests[1].headers.get("payment-signature").is_some(),
            "the retry carries the signed payment"
        );
        server.verify().await; // exactly one unpaid + one paid request
    }

    #[tokio::test]
    async fn smart_money_context_fills_fields_and_proxy_note() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let ledger_file = dir.path().join("nansen-spend.jsonl");
        let payer = PayerSigner::from_hex(&test_key())
            .expect("test key builds a signer")
            .address();

        mount_challenge(&server, ENDPOINT_NETFLOW, NETFLOW_402_FIXTURE).await;
        mount_paid_call(
            &server,
            ENDPOINT_NETFLOW,
            json!({ "net_flow_usd": "-123.45" }),
            settlement_b64(&payer),
        )
        .await;

        let client = test_client(&server, 40, &ledger_file);
        let sm = client.smart_money_context("ETH", "ethereum", NOW_MS).await;

        assert_eq!(sm.asset, "ETH");
        assert_eq!(sm.netflow_24h, Some(Decimal::new(-12345, 2)));
        assert_eq!(sm.fetched_at_ms, Some(NOW_MS));
        assert_eq!(
            sm.total_cost_usd,
            Some(Decimal::new(5, 2)),
            "the recorded netflow rail is 50000 ⇒ $0.05"
        );
        assert_eq!(sm.note.as_deref(), Some(SMART_MONEY_PROXY_NOTE));
        assert_eq!(sm.holdings_delta, None);
        assert_eq!(sm.long_short_ratio, None);
        assert_eq!(sm.top_traders_net_bias, None);
        assert_eq!(SpendLedger::new(&ledger_file).load().len(), 1);
        assert_eq!(request_count(&server).await, 2);
    }

    #[tokio::test]
    async fn smart_money_context_degrades_without_a_challenge() {
        // No mocks at all: every request 404s ⇒ the context degrades.
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let client = test_client(&server, 40, &dir.path().join("nansen-spend.jsonl"));

        let sm = client
            .smart_money_context("AVAX", "avalanche", NOW_MS)
            .await;

        assert_eq!(sm, SmartMoneyContext::unavailable("AVAX"));
        assert!(
            request_count(&server).await >= 1,
            "the attempt still reaches the endpoint"
        );
        assert_eq!(
            SpendLedger::new(dir.path().join("nansen-spend.jsonl"))
                .load()
                .len(),
            0,
            "a failed call must not append a ledger row"
        );
    }

    #[tokio::test]
    async fn smart_money_context_degrades_on_http_403() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let client = test_client(&server, 40, &dir.path().join("nansen-spend.jsonl"));

        let sm = client.smart_money_context("ETH", "ethereum", NOW_MS).await;

        assert_eq!(sm, SmartMoneyContext::unavailable("ETH"));
    }
}
