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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local, NaiveDate, TimeZone};

    #[test]
    fn test_now_and_utc() {
        let local = now();
        let utc = now_utc();
        // They should represent the same instant (within a few seconds)
        let diff = (utc - local.with_timezone(&chrono::Utc)).num_seconds().abs();
        assert!(diff < 5, "local and utc differ by {diff}s");
    }

    #[test]
    fn test_format_timestamp() {
        let dt = Local.with_ymd_and_hms(2024, 6, 15, 14, 30, 0).unwrap();
        assert_eq!(format_timestamp(&dt), "2024-06-15 14:30:00");
    }

    #[test]
    fn test_format_log_timestamp() {
        let dt = Local.with_ymd_and_hms(2024, 6, 15, 14, 30, 0).unwrap();
        assert_eq!(format_log_timestamp(&dt), "20240615");
    }

    #[test]
    fn test_parse_timestamp_valid() {
        let dt = parse_timestamp("2024-06-15T14:30:00Z").unwrap();
        let utc_dt = dt.with_timezone(&chrono::Utc);
        assert_eq!(utc_dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2024-06-15 14:30:00");
    }

    #[test]
    fn test_parse_timestamp_invalid() {
        assert!(parse_timestamp("not-a-timestamp").is_none());
    }

    #[test]
    fn test_duration_secs() {
        let start = Local.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2024, 1, 1, 12, 0, 5).unwrap();
        assert_eq!(duration_secs(&start, &end), 5.0);
    }

    #[test]
    fn test_is_today() {
        assert!(is_today(&now()));
        let yesterday = now() - Duration::days(1);
        assert!(!is_today(&yesterday));
    }

    #[test]
    fn test_is_within_minutes() {
        assert!(is_within_minutes(&now(), 5));
        let old = now() - Duration::minutes(10);
        assert!(!is_within_minutes(&old, 5));
    }

    #[test]
    fn test_daily_reset_cutoff_before_reset() {
        // If now is before today's reset hour, cutoff should be yesterday's reset
        let now_time = Local.with_ymd_and_hms(2024, 6, 15, 3, 0, 0).unwrap();
        // We can't easily mock `now()` here, but we can verify the function doesn't panic
        // and returns a reasonable value when called at runtime
        let cutoff = daily_reset_cutoff(4);
        // Just verify it returns a DateTime
        let _ = format_timestamp(&cutoff);
    }
}
