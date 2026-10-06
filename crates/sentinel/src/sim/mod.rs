//! Deterministic backtesting (SPEC-P13): scripted price paths driven through
//! the real decision path — `sentinel-core` risk math, reflex rules and the
//! policy engine — with a synchronous tick loop. No wall clock, no network,
//! no LLM in replay mode.
pub mod baseline;
pub mod engine;
pub mod report;
pub mod scenario;
pub mod venue;
