//! # Hermes Gateway
#![recursion_limit = "256"]
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Gateway session management, platform configuration, and messaging adapters.
//! Mirrors the Python `gateway/` directory.

pub mod config;
pub(crate) mod dedup;
pub mod runner;
pub(crate) mod session;
pub(crate) mod platforms;
pub(crate) mod stream_consumer;
pub(crate) mod mcp_config;
