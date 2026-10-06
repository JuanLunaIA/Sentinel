//! Heartbeat watcher over the anchor contract (`SPEC-P14.md` §3).
//!
//! Polls `eth_getLogs` every [`POLL_INTERVAL`] (15 s) over a rolling lookback
//! window (default [`DEFAULT_LOOKBACK_BLOCKS`] = 7200 blocks) for
//! `Heartbeat(address,bytes32,uint32,uint8)` events emitted by
//! `SentinelAuditAnchor.beat` and tracks the latest event per guardian.
//!
//! The ABI event-signature hashes are frozen Keccak-256 values computed with
//! Foundry 1.8.5 and re-checked at test time against the runtime Keccak
//! implementation:
//!
//! ```text
//! $ cast keccak "Heartbeat(address,bytes32,uint32,uint8)"
//! 0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee
//! $ cast keccak "DecisionAnchored(uint64,bytes32,bytes32,address)"
//! 0x8e91e279fa0ecd8e63f2e02c10a6ed25ee8ba052a972045c37b8c38f1bc0d555
//! ```
//!
//! Timestamps: `eth_getLogs` responses carry no block timestamp; the watcher
//! uses the `blockTimestamp` field when the node provides it (alloy's `Log`
//! carries the execution-apis proposal field) and otherwise fetches the block
//! header once per block (cached). An event whose timestamp cannot be resolved
//! is skipped with a warning — without a time there is no staleness math.
//!
//! The tier mapping matches the contract: `Green=0, Yellow=1, Orange=2,
//! Red=3`; values above 3 are kept verbatim (never clamped) and log as
//! `unknown`.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{BlockNumberOrTag, Filter, Log};
use tokio::sync::{RwLock, watch};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

/// Watched event signature: `Heartbeat(address guardian, bytes32 riskStateHash, uint32 openPositions, uint8 maxTier)`.
pub const HEARTBEAT_SIGNATURE: &str = "Heartbeat(address,bytes32,uint32,uint8)";

/// Keccak-256 of [`HEARTBEAT_SIGNATURE`] (`cast keccak`, Foundry 1.8.5):
/// `0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee`.
pub const HEARTBEAT_TOPIC0_HEX: &str =
    "0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee";

/// `DecisionAnchored` event signature (P10 anchor task; watcher-side filter
/// constant so a future liveness leg can reuse the decoded stream).
pub const DECISION_ANCHORED_SIGNATURE: &str = "DecisionAnchored(uint64,bytes32,bytes32,address)";

/// Keccak-256 of [`DECISION_ANCHORED_SIGNATURE`] (`cast keccak`, Foundry
/// 1.8.5): `0x8e91e279fa0ecd8e63f2e02c10a6ed25ee8ba052a972045c37b8c38f1bc0d555`.
pub const DECISION_ANCHORED_TOPIC0_HEX: &str =
    "0x8e91e279fa0ecd8e63f2e02c10a6ed25ee8ba052a972045c37b8c38f1bc0d555";

/// Default rolling lookback window, blocks (frozen: 7200).
pub const DEFAULT_LOOKBACK_BLOCKS: u64 = 7200;

/// Poll cadence (frozen: every 15 s).
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// ABI word size.
const WORD_SIZE: usize = 32;

/// Exact ABI data length of a `Heartbeat` event: four static words.
const HEARTBEAT_DATA_LEN: usize = 4 * WORD_SIZE;

/// The `Heartbeat` topic0 as a hash.
pub fn heartbeat_topic0() -> B256 {
    HEARTBEAT_TOPIC0_HEX
        .parse()
        .expect("frozen topic0 constant is valid hex")
}

/// The `DecisionAnchored` topic0 as a hash.
pub fn decision_anchored_topic0() -> B256 {
    DECISION_ANCHORED_TOPIC0_HEX
        .parse()
        .expect("frozen topic0 constant is valid hex")
}

/// Contract tier name for a raw `maxTier` byte (`Green=0 … Red=3`).
pub fn tier_name(max_tier: u8) -> &'static str {
    match max_tier {
        0 => "Green",
        1 => "Yellow",
        2 => "Orange",
        3 => "Red",
        _ => "unknown",
    }
}

/// Watcher failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    /// RPC/provider transport failure.
    Rpc(String),
    /// A log did not decode as the frozen `Heartbeat` event.
    Decode(String),
    /// The block timestamp for an event could not be resolved.
    Timestamp(String),
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WatchError::Rpc(detail) => write!(f, "rpc failure: {detail}"),
            WatchError::Decode(detail) => write!(f, "heartbeat decode failure: {detail}"),
            WatchError::Timestamp(detail) => write!(f, "timestamp resolution failure: {detail}"),
        }
    }
}

impl std::error::Error for WatchError {}

/// One decoded `Heartbeat` log, with whatever block metadata the node gave us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedHeartbeat {
    /// `msg.sender` of the `beat` call — the guardian the daemon signs as.
    pub guardian: Address,
    /// Hash of the canonical risk-state summary at beat time.
    pub risk_state_hash: B256,
    /// Open positions at beat time.
    pub open_positions: u32,
    /// Highest risk tier in effect (raw contract byte; `2` = Orange, `3` = Red).
    pub max_tier: u8,
    /// Block the event was mined in (`0` when the node omitted it).
    pub block_number: u64,
    /// Log index within the block (`0` when the node omitted it).
    pub log_index: u64,
    /// Block timestamp in seconds when the node included `blockTimestamp`.
    pub block_timestamp_secs: Option<u64>,
}

/// Latest heartbeat tracked for one guardian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianHeartbeat {
    /// The guardian this heartbeat belongs to.
    pub guardian: Address,
    /// Block timestamp of the beat, milliseconds since the Unix epoch.
    pub last_ts_ms: u64,
    /// Raw `maxTier` byte from the beat.
    pub max_tier: u8,
    /// `openPositions` from the beat.
    pub open_positions: u32,
    /// `riskStateHash` from the beat.
    pub risk_state_hash: B256,
    /// Block the beat was mined in.
    pub block_number: u64,
    /// Log index within that block.
    pub log_index: u64,
}

/// Shared per-guardian heartbeat table, updated by the watcher and read by
/// the trigger/armed surfaces.
#[derive(Debug, Default)]
pub struct WatcherState {
    guardians: HashMap<Address, GuardianHeartbeat>,
}

impl WatcherState {
    /// New empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `guardian` has any recorded heartbeat.
    pub fn contains(&self, guardian: &Address) -> bool {
        self.guardians.contains_key(guardian)
    }

    /// Number of tracked guardians.
    pub fn len(&self) -> usize {
        self.guardians.len()
    }

    /// Whether no guardian has ever been seen.
    pub fn is_empty(&self) -> bool {
        self.guardians.is_empty()
    }

    /// Record `event`; returns `true` when it was newer than what was stored.
    ///
    /// Ordering key is `(block_number, log_index)` — replayed polls are
    /// idempotent, same-block events apply in log order, and an older event
    /// never overwrites a newer one.
    pub fn record(&mut self, event: GuardianHeartbeat) -> bool {
        match self.guardians.get(&event.guardian) {
            Some(previous)
                if (event.block_number, event.log_index)
                    <= (previous.block_number, previous.log_index) =>
            {
                false
            }
            _ => {
                self.guardians.insert(event.guardian, event);
                true
            }
        }
    }

    /// Latest heartbeat for `guardian`, if any.
    pub fn latest(&self, guardian: &Address) -> Option<&GuardianHeartbeat> {
        self.guardians.get(guardian)
    }
}

/// Decode one `Heartbeat` log into its payload + block metadata.
///
/// # Errors
/// [`WatchError::Decode`] when the topic does not match
/// [`HEARTBEAT_TOPIC0_HEX`] or the data is not exactly four ABI words.
pub fn decode_heartbeat(log: &Log) -> Result<DecodedHeartbeat, WatchError> {
    let topics = log.topics();
    let topic0 = topics
        .first()
        .ok_or_else(|| WatchError::Decode("log carries no topics".to_string()))?;
    if *topic0 != heartbeat_topic0() {
        return Err(WatchError::Decode(format!(
            "topic0 {topic0:#x} is not the Heartbeat signature"
        )));
    }

    let data = log.data().data.as_ref();
    if data.len() != HEARTBEAT_DATA_LEN {
        return Err(WatchError::Decode(format!(
            "Heartbeat data must be {HEARTBEAT_DATA_LEN} bytes, got {}",
            data.len()
        )));
    }

    let word = |index: usize| -> &[u8] {
        let start = index * WORD_SIZE;
        &data[start..start + WORD_SIZE]
    };

    let guardian = Address::from_slice(&word(0)[12..WORD_SIZE]);
    let risk_state_hash = B256::from_slice(word(1));
    let open_positions = u32::from_be_bytes(word(2)[28..WORD_SIZE].try_into().expect("4 bytes"));
    let max_tier = word(3)[WORD_SIZE - 1];

    Ok(DecodedHeartbeat {
        guardian,
        risk_state_hash,
        open_positions,
        max_tier,
        block_number: log.block_number.unwrap_or(0),
        log_index: log.log_index.unwrap_or(0),
        block_timestamp_secs: log.block_timestamp,
    })
}

/// alloy-backed heartbeat watcher.
pub struct HeartbeatWatcher {
    provider: DynProvider,
    contract: Address,
    lookback_blocks: u64,
    poll_interval: Duration,
    block_timestamp_cache: tokio::sync::Mutex<HashMap<u64, u64>>,
}

impl HeartbeatWatcher {
    /// Build the watcher for `contract` at `rpc_url`.
    ///
    /// # Errors
    /// [`WatchError::Rpc`] when the RPC URL does not parse.
    pub fn new(rpc_url: &str, contract: Address, lookback_blocks: u64) -> Result<Self, WatchError> {
        let url = alloy::transports::http::reqwest::Url::parse(rpc_url.trim())
            .map_err(|err| WatchError::Rpc(format!("BREAKER_RPC_URL is not a valid URL: {err}")))?;
        let provider = ProviderBuilder::new().connect_http(url).erased();
        Ok(Self {
            provider,
            contract,
            lookback_blocks,
            poll_interval: POLL_INTERVAL,
            block_timestamp_cache: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Override the poll cadence (tests and tight demos; default
    /// [`POLL_INTERVAL`]).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Watched anchor contract.
    pub fn contract(&self) -> Address {
        self.contract
    }

    /// One poll pass: fetch the window, decode, resolve timestamps, record.
    ///
    /// Returns the number of events that advanced the tracked state.
    ///
    /// # Errors
    /// [`WatchError::Rpc`] when `eth_blockNumber` / `eth_getLogs` fails.
    pub async fn poll_once(&self, state: &RwLock<WatcherState>) -> Result<usize, WatchError> {
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(|err| WatchError::Rpc(format!("eth_blockNumber failed: {err}")))?;
        let from_block = latest.saturating_sub(self.lookback_blocks);
        let filter = Filter::new()
            .address(self.contract)
            .event_signature(heartbeat_topic0())
            .from_block(from_block)
            .to_block(latest);
        let logs = self
            .provider
            .get_logs(&filter)
            .await
            .map_err(|err| WatchError::Rpc(format!("eth_getLogs failed: {err}")))?;

        let mut decoded = Vec::new();
        for log in &logs {
            match decode_heartbeat(log) {
                Ok(event) => decoded.push(event),
                Err(err) => {
                    warn!(error = %err, "breaker watcher: foreign log in Heartbeat filter; skipped")
                }
            }
        }
        // Deterministic application order regardless of node ordering.
        decoded.sort_by_key(|event| (event.block_number, event.log_index));

        let mut events = Vec::with_capacity(decoded.len());
        for event in decoded {
            let ts_ms = match event.block_timestamp_secs {
                Some(secs) => secs.saturating_mul(1000),
                None => match self.block_timestamp_ms(event.block_number).await {
                    Ok(ms) => ms,
                    Err(err) => {
                        warn!(
                            error = %err,
                            block_number = event.block_number,
                            guardian = %event.guardian,
                            "breaker watcher: heartbeat timestamp unresolved; event skipped"
                        );
                        continue;
                    }
                },
            };
            events.push(GuardianHeartbeat {
                guardian: event.guardian,
                last_ts_ms: ts_ms,
                max_tier: event.max_tier,
                open_positions: event.open_positions,
                risk_state_hash: event.risk_state_hash,
                block_number: event.block_number,
                log_index: event.log_index,
            });
        }

        let mut guard = state.write().await;
        let mut applied = 0;
        for event in events {
            if guard.record(event) {
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Run the poll loop until `shutdown` flips, logging poll failures.
    pub async fn run(&self, state: Arc<RwLock<WatcherState>>, mut shutdown: watch::Receiver<bool>) {
        info!(
            contract = %self.contract,
            lookback_blocks = self.lookback_blocks,
            poll_secs = self.poll_interval.as_secs(),
            "breaker watcher: started"
        );
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    match self.poll_once(&state).await {
                        Ok(applied) if applied > 0 => {
                            // Read the guard BEFORE the macro: an `.await`
                            // inside tracing fields makes the future !Send.
                            let trackers = state.read().await.len();
                            info!(applied, trackers, "breaker watcher: heartbeats recorded");
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!(error = %err, "breaker watcher: poll failed; retrying next tick");
                        }
                    }
                }
            }
        }
        info!("breaker watcher: stopped");
    }

    /// Block timestamp in ms, cached per block number.
    async fn block_timestamp_ms(&self, block_number: u64) -> Result<u64, WatchError> {
        if let Some(secs) = self.block_timestamp_cache.lock().await.get(&block_number) {
            return Ok(secs.saturating_mul(1000));
        }
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await
            .map_err(|err| {
                WatchError::Timestamp(format!(
                    "eth_getBlockByNumber({block_number}) failed: {err}"
                ))
            })?
            .ok_or_else(|| WatchError::Timestamp(format!("block {block_number} not found")))?;
        let secs = block.header.timestamp;
        self.block_timestamp_cache
            .lock()
            .await
            .insert(block_number, secs);
        Ok(secs.saturating_mul(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Bytes, Log as PrimitiveLog, LogData};

    const GUARDIAN: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    /// The topic0 constants must equal `cast keccak` output AND the runtime
    /// Keccak-256 of the signatures (an independent recomputation).
    #[test]
    fn topic0_consts_match_cast_keccak_output() {
        // cast keccak "Heartbeat(address,bytes32,uint32,uint8)"
        // → 0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee
        assert_eq!(
            HEARTBEAT_TOPIC0_HEX,
            "0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee"
        );
        // cast keccak "DecisionAnchored(uint64,bytes32,bytes32,address)"
        // → 0x8e91e279fa0ecd8e63f2e02c10a6ed25ee8ba052a972045c37b8c38f1bc0d555
        assert_eq!(
            DECISION_ANCHORED_TOPIC0_HEX,
            "0x8e91e279fa0ecd8e63f2e02c10a6ed25ee8ba052a972045c37b8c38f1bc0d555"
        );

        assert_eq!(
            heartbeat_topic0(),
            alloy::primitives::keccak256(HEARTBEAT_SIGNATURE),
            "const must equal keccak256 of the signature"
        );
        assert_eq!(
            decision_anchored_topic0(),
            alloy::primitives::keccak256(DECISION_ANCHORED_SIGNATURE)
        );
    }

    #[test]
    fn tier_names_follow_the_contract_mapping() {
        assert_eq!(tier_name(0), "Green");
        assert_eq!(tier_name(1), "Yellow");
        assert_eq!(tier_name(2), "Orange");
        assert_eq!(tier_name(3), "Red");
        assert_eq!(tier_name(4), "unknown");
    }

    /// Build an ABI-exact Heartbeat log for decoding tests.
    fn heartbeat_log(
        guardian: Address,
        risk_state_hash: B256,
        open_positions: u32,
        max_tier: u8,
        block_number: u64,
        block_timestamp: Option<u64>,
        log_index: u64,
    ) -> Log {
        let mut data = vec![0u8; HEARTBEAT_DATA_LEN];
        data[12..32].copy_from_slice(guardian.as_slice());
        data[32..64].copy_from_slice(risk_state_hash.as_slice());
        data[92..96].copy_from_slice(&open_positions.to_be_bytes());
        data[WORD_SIZE - 1 + 3 * WORD_SIZE] = max_tier;
        Log {
            inner: PrimitiveLog {
                address: "0x5FbDB2315678afecb367f032d93F642f64180aa3"
                    .parse()
                    .expect("address"),
                data: LogData::new_unchecked(vec![heartbeat_topic0()], Bytes::from(data)),
            },
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp,
            transaction_hash: None,
            transaction_index: None,
            log_index: Some(log_index),
            removed: false,
        }
    }

    #[test]
    fn decode_reads_all_four_fields() {
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let hash = B256::repeat_byte(0xAB);
        for tier in 0..=3u8 {
            let log = heartbeat_log(guardian, hash, 7, tier, 21, Some(1_700_000_000), 5);
            let event = decode_heartbeat(&log).expect("decodes");
            assert_eq!(event.guardian, guardian);
            assert_eq!(event.risk_state_hash, hash);
            assert_eq!(event.open_positions, 7);
            assert_eq!(event.max_tier, tier);
            assert_eq!(event.block_number, 21);
            assert_eq!(event.log_index, 5);
            assert_eq!(event.block_timestamp_secs, Some(1_700_000_000));
        }
    }

    #[test]
    fn decode_rejects_wrong_topic_and_bad_length() {
        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let mut log = heartbeat_log(guardian, B256::ZERO, 0, 0, 1, None, 0);
        log.inner.data = LogData::new_unchecked(
            vec![decision_anchored_topic0()],
            log.inner.data.data.clone(),
        );
        let err = decode_heartbeat(&log).expect_err("wrong topic must fail");
        assert!(matches!(err, WatchError::Decode(_)), "{err:?}");

        // Truncated payload (one word short).
        let mut short = heartbeat_log(guardian, B256::ZERO, 0, 0, 1, None, 0);
        let mut bytes = short.inner.data.data.to_vec();
        bytes.truncate(HEARTBEAT_DATA_LEN - WORD_SIZE);
        short.inner.data = LogData::new_unchecked(vec![heartbeat_topic0()], Bytes::from(bytes));
        let err = decode_heartbeat(&short).expect_err("short data must fail");
        assert!(matches!(err, WatchError::Decode(_)), "{err:?}");
    }

    fn heartbeat(
        guardian: Address,
        block_number: u64,
        log_index: u64,
        tier: u8,
        ts_ms: u64,
    ) -> GuardianHeartbeat {
        GuardianHeartbeat {
            guardian,
            last_ts_ms: ts_ms,
            max_tier: tier,
            open_positions: 1,
            risk_state_hash: B256::ZERO,
            block_number,
            log_index,
        }
    }

    #[test]
    fn state_tracks_latest_per_guardian_with_block_order() {
        let mut state = WatcherState::new();
        assert!(state.is_empty());

        let guardian = GUARDIAN.parse::<Address>().expect("guardian");
        let first = heartbeat(guardian, 10, 0, 1, 1_000);
        assert!(state.record(first.clone()));
        assert!(state.contains(&guardian));
        assert_eq!(state.len(), 1);

        // Replaying the same event is a no-op.
        assert!(!state.record(first.clone()));
        assert_eq!(state.latest(&guardian), Some(&first));

        // An older block never overwrites.
        assert!(!state.record(heartbeat(guardian, 9, 99, 3, 500)));
        assert_eq!(state.latest(&guardian), Some(&first));

        // Same block: higher log index wins, lower is ignored.
        let same_block_later = heartbeat(guardian, 10, 1, 2, 1_100);
        assert!(state.record(same_block_later.clone()));
        assert!(!state.record(heartbeat(guardian, 10, 0, 0, 900)));
        assert_eq!(state.latest(&guardian), Some(&same_block_later));

        // A newer block wins.
        let newer = heartbeat(guardian, 11, 0, 3, 2_000);
        assert!(state.record(newer.clone()));
        assert_eq!(state.latest(&guardian), Some(&newer));

        // A second guardian is tracked independently.
        let other = "0x00000000000000000000000000000000000000aa"
            .parse::<Address>()
            .expect("address");
        assert!(state.record(heartbeat(other, 5, 0, 0, 250)));
        assert_eq!(state.len(), 2, "both guardians tracked");
        assert_eq!(
            state.latest(&guardian),
            Some(&newer),
            "first guardian untouched"
        );
        assert_eq!(
            state.latest(&other),
            Some(&heartbeat(other, 5, 0, 0, 250)),
            "second guardian tracked separately"
        );
    }
}
