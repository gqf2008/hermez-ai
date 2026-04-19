#![allow(dead_code)]
//! Time utilities.
//!
//! Mirrors the Python `hermes_time.py` module.

use chrono::{DateTime, Local};
#[allow(unused_imports)]
use chrono::TimeZone;

/// Returns the current local date and time.
pub fn now() -> DateTime<Local> {
    Local::now()
}

/// Returns the current UTC date and time.
pub fn now_utc() -> DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Format a timestamp in a human-readable way.
pub fn format_timestamp(dt: &DateTime<Local>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Format a timestamp for log file naming.
pub fn format_log_timestamp(dt: &DateTime<Local>) -> String {
    dt.format("%Y%m%d").to_string()
}

/// Parse an ISO 8601 timestamp string.
pub fn parse_timestamp(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

/// Calculate the duration between two timestamps in seconds.
pub fn duration_secs(start: &DateTime<Local>, end: &DateTime<Local>) -> f64 {
    (end.signed_duration_since(*start)).num_milliseconds() as f64 / 1000.0
}

/// Check if a timestamp is from today (local time).
pub fn is_today(dt: &DateTime<Local>) -> bool {
    let now = now();
    dt.date_naive() == now.date_naive()
}

/// Check if a timestamp is within the last N minutes.
pub fn is_within_minutes(dt: &DateTime<Local>, minutes: i64) -> bool {
    let now = now();
    now.signed_duration_since(*dt).num_minutes() < minutes
}

/// Get the reset cutoff time based on a daily reset hour.
///
/// Used by the gateway session reset logic to determine when
/// a session should be rotated to a new day.
pub fn daily_reset_cutoff(reset_hour: u32) -> DateTime<Local> {
    let now = now();
    let today_reset = now
        .date_naive()
        .and_hms_opt(reset_hour.min(23), 0, 0)
        .unwrap_or_else(|| now.naive_local().date().and_hms_opt(0, 0, 0).unwrap())
        .and_local_timezone(Local)
        .earliest()
        .unwrap_or(now);

    if now < today_reset {
        // Yesterday's reset time
        today_reset - chrono::Duration::days(1)
    } else {
        today_reset
    }
}
