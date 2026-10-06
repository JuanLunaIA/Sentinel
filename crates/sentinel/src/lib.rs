//! # sentinel
//!
//! Application shell for Sentinel: strongly-typed configuration, the error
//! hierarchy and telemetry bootstrapping.
//!
//! Subsystems (perception, reasoning brain, execution, policy wiring, bot and
//! API) are added by later prompts; their failures already have a home in
//! [`error::SentinelError`], and all external I/O lives behind async traits so
//! it can be mocked and recorded (P00 invariant #3).

#![warn(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod anchor;
pub mod api;
pub mod args;
pub mod bot;
pub mod brain;
pub mod config;
pub mod error;
pub mod execution;
pub mod health;
pub mod indexer;
pub mod nansen;
pub mod notify;
pub mod perpl;
pub mod pipeline;
pub mod reflex;
pub mod sim;
pub mod supervisor;
pub mod telemetry;
