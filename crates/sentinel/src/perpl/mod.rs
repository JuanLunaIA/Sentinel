//! Perpl perception layer: authentication, REST snapshots and WebSocket
//! feeds — the sensory system of Sentinel.
//!
//! **Skeleton status (P03):** interfaces are frozen in `SPEC.md`;
//! [`LivePerpl`], [`MockPerpl`] and the socket loop are integrated by the
//! parent after the module wave lands. Submodules: [`auth`] (Ed25519 request
//! signing), [`types`] (raw-JSON to domain mapping), [`rest`] (HTTP
//! snapshots), [`ws`] (streams, events, staleness detection).

pub mod auth;
pub mod rest;
pub mod types;
pub mod ws;

use sentinel_core::types::{AccountState, Market};
use tokio::sync::mpsc::Receiver;

pub use types::AccountUpdate;
pub use ws::{AccountEvent, FeedEvent, MarketEvent, StaleDetector};

/// Perception feed: streams live events and serves point-in-time snapshots.
///
/// Implementations never expose raw venue JSON; everything is mapped into
/// `sentinel_core` types at this boundary (P00 invariant #3).
///
/// `async_fn_in_trait` is allowed deliberately: this is an internal,
/// non-generic trait used behind concrete types (`LivePerpl`/`MockPerpl`); no
/// external implementor needs `Send` bounds on the returned futures.
#[allow(async_fn_in_trait)]
pub trait PerplFeed {
    /// Subscribe to the live event stream (market + account + feed health).
    async fn stream(&self) -> Receiver<FeedEvent>;

    /// Point-in-time account snapshot (positions + balances).
    async fn snapshot(&self) -> crate::error::Result<AccountState>;

    /// Static market context (the markets the venue currently lists).
    async fn context(&self) -> crate::error::Result<Vec<Market>>;
}

/// Live implementation over the Perpl gateway (REST + WebSocket).
pub struct LivePerpl;

impl LivePerpl {
    /// Construct from configuration (builds the signer and HTTP client).
    pub fn new(_cfg: &crate::config::Config) -> crate::error::Result<Self> {
        todo!("P03 integration (parent)")
    }
}

impl PerplFeed for LivePerpl {
    async fn stream(&self) -> Receiver<FeedEvent> {
        todo!("P03 integration (parent)")
    }

    async fn snapshot(&self) -> crate::error::Result<AccountState> {
        todo!("P03 integration (parent)")
    }

    async fn context(&self) -> crate::error::Result<Vec<Market>> {
        todo!("P03 integration (parent)")
    }
}

/// Fixture-backed implementation for offline dev, tests and demo replay.
pub struct MockPerpl;

impl MockPerpl {
    /// Load a session fixture (JSONL; format frozen in `SPEC.md` §3.4).
    pub fn from_fixture(_path: &std::path::Path) -> crate::error::Result<Self> {
        todo!("P03 integration (parent)")
    }
}

impl PerplFeed for MockPerpl {
    async fn stream(&self) -> Receiver<FeedEvent> {
        todo!("P03 integration (parent)")
    }

    async fn snapshot(&self) -> crate::error::Result<AccountState> {
        todo!("P03 integration (parent)")
    }

    async fn context(&self) -> crate::error::Result<Vec<Market>> {
        todo!("P03 integration (parent)")
    }
}
