//! # Hermez Core
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Shared types, constants, error definitions, and configuration for the Hermez Agent system.
//!
//! This crate provides:
//! - `HermezError` — unified error type with rich context
//! - `get_hermez_home()` — profile-aware path resolution
//! - `HermezConfig` — typed configuration from YAML + env vars
//! - Logging setup via `tracing`
//! - Time utilities

pub(crate) mod auth_lock;
pub mod config;
pub(crate) mod constants;
pub(crate) mod env_loader;
pub mod errors;
pub mod hermez_home;
pub(crate) mod logging;
pub(crate) mod platforms;
pub(crate) mod proxy_validation;
pub(crate) mod redact;
pub(crate) mod time;
pub mod text_utils;

pub use auth_lock::{with_auth_json_read_lock, with_auth_json_write_lock};
pub use config::{coerce_bool, HermezConfig, ProviderPreferencesConfig};
pub use env_loader::{load_dotenv_override, load_hermez_dotenv};
pub use errors::{ApiErrorDetails, ErrorCategory, HermezError, Result};
pub use hermez_home::{display_hermez_home, get_hermez_home, get_hermez_dir, get_default_hermez_root};
pub use proxy_validation::{validate_base_url, validate_proxy_env_urls};
pub use redact::redact_sensitive_text;
pub use text_utils::strip_think_blocks;
