//! # Hermez State
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! SQLite session store with WAL mode and FTS5 full-text search.
//! Mirrors the Python `hermez_state.py` SessionDB class.

pub(crate) mod insights;
pub mod models;
pub use models::Message;
pub(crate) mod schema;
pub mod session_db;

pub use insights::InsightsEngine;
pub use session_db::{now_epoch, SessionDB, StateError};
