#![allow(dead_code)]
//! Logging setup using the `tracing` crate.
//!
//! Creates rotating file handlers for `agent.log` (INFO+) and
//! `errors.log` (WARNING+) under the Hermez logs directory.
//! Mirrors the Python `hermez_logging.py` setup.

use std::fs;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::hermez_home::get_hermez_home;
use crate::constants::{LOG_DIR_NAME, LOG_FILE, ERROR_LOG_FILE};

/// Initialize logging for the Hermez application.
///
/// Creates:
/// - `~/.hermez/logs/agent.log` — INFO level and above
/// - `~/.hermez/logs/errors.log` — WARNING level and above
///
/// Returns `WorkerGuard` handles that must be kept alive for the duration
/// of the application to ensure buffered logs are flushed.
pub fn setup_logging() -> anyhow::Result<Vec<WorkerGuard>> {
    let hermez_home = get_hermez_home();
    let log_dir = hermez_home.join(LOG_DIR_NAME);
    fs::create_dir_all(&log_dir)?;

    let mut guards = Vec::new();

    // INFO+ log file (rotating daily)
    let info_appender = RollingFileAppender::new(
        Rotation::DAILY,
        &log_dir,
        LOG_FILE,
    );
    let (info_non_blocking, info_guard) = tracing_appender::non_blocking(info_appender);
    guards.push(info_guard);

    let info_layer = tracing_subscriber::fmt::layer()
        .with_writer(info_non_blocking)
        .with_target(true)
        .with_thread_ids(true)
        .with_span_events(FmtSpan::NONE)
        .with_filter(EnvFilter::new("info"));

    // WARNING+ error log file
    let error_appender = RollingFileAppender::new(
        Rotation::DAILY,
        &log_dir,
        ERROR_LOG_FILE,
    );
    let (error_non_blocking, error_guard) = tracing_appender::non_blocking(error_appender);
    guards.push(error_guard);

    let error_layer = tracing_subscriber::fmt::layer()
        .with_writer(error_non_blocking)
        .with_target(true)
        .with_thread_ids(true)
        .with_filter(EnvFilter::new("warn"));

    // Console layer — reads RUST_LOG env var
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(EnvFilter::from_default_env());

    tracing_subscriber::registry()
        .with(console_layer)
        .with(info_layer)
        .with(error_layer)
        .init();

    Ok(guards)
}

/// Initialize verbose (DEBUG-level) logging for development / `-v` mode.
///
/// Adds a console handler at DEBUG level in addition to file handlers.
pub fn setup_verbose_logging() -> anyhow::Result<Vec<WorkerGuard>> {
    let hermez_home = get_hermez_home();
    let log_dir = hermez_home.join(LOG_DIR_NAME);
    fs::create_dir_all(&log_dir)?;

    let mut guards = Vec::new();

    let info_appender = RollingFileAppender::new(
        Rotation::DAILY,
        &log_dir,
        LOG_FILE,
    );
    let (info_non_blocking, info_guard) = tracing_appender::non_blocking(info_appender);
    guards.push(info_guard);

    let info_layer = tracing_subscriber::fmt::layer()
        .with_writer(info_non_blocking)
        .with_target(true)
        .with_filter(EnvFilter::new("debug"));

    // Verbose console
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(EnvFilter::new("debug"));

    tracing_subscriber::registry()
        .with(console_layer)
        .with(info_layer)
        .init();

    Ok(guards)
}

/// Check if a path is under the Hermez logs directory.
pub fn is_log_path(path: &Path) -> bool {
    let log_dir = get_hermez_home().join(LOG_DIR_NAME);
    path.starts_with(&log_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_is_log_path_positive() {
        let log_dir = get_hermez_home().join(LOG_DIR_NAME);
        let log_file = log_dir.join("agent.log");
        assert!(is_log_path(&log_file));
    }

    #[test]
    fn test_is_log_path_nested() {
        let log_dir = get_hermez_home().join(LOG_DIR_NAME);
        let nested = log_dir.join("sub").join("error.log");
        assert!(is_log_path(&nested));
    }

    #[test]
    fn test_is_log_path_negative() {
        let not_log = PathBuf::from("/tmp/other.txt");
        assert!(!is_log_path(&not_log));
    }

    #[test]
    fn test_is_log_path_config_not_log() {
        let config = get_hermez_home().join("config.yaml");
        assert!(!is_log_path(&config));
    }
}
