//! # Hermes State
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! SQLite session store with WAL mode and FTS5 full-text search.
//! Mirrors the Python `hermes_state.py` SessionDB class.

pub(crate) mod insights;
pub(crate) mod models;
pub(crate) mod schema;
pub mod session_db;

pub use insights::InsightsEngine;
pub use session_db::{now_epoch, SessionDB, StateError};
