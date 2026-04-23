//! # Hermes Core
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Shared types, constants, error definitions, and configuration for the Hermes Agent system.
//!
//! This crate provides:
//! - `HermesError` — unified error type with rich context
//! - `get_hermes_home()` — profile-aware path resolution
//! - `HermesConfig` — typed configuration from YAML + env vars
//! - Logging setup via `tracing`
//! - Time utilities

pub(crate) mod auth_lock;
pub mod config;
pub(crate) mod constants;
pub(crate) mod env_loader;
pub mod errors;
pub mod hermes_home;
pub(crate) mod logging;
pub(crate) mod platforms;
pub(crate) mod proxy_validation;
pub(crate) mod redact;
pub(crate) mod time;
pub mod text_utils;

pub use auth_lock::{with_auth_json_read_lock, with_auth_json_write_lock};
pub use config::{coerce_bool, HermesConfig, ProviderPreferencesConfig};
pub use env_loader::{load_dotenv_override, load_hermes_dotenv};
pub use errors::{ApiErrorDetails, ErrorCategory, HermesError, Result};
pub use hermes_home::{display_hermes_home, get_hermes_home, get_hermes_dir, get_default_hermes_root};
pub use proxy_validation::{validate_base_url, validate_proxy_env_urls};
pub use redact::redact_sensitive_text;
pub use text_utils::strip_think_blocks;
