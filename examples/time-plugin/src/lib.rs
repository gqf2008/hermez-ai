//! Hermes Time Plugin — time utilities for agents.
//!
//! Tools:
//!   time_now          → current UTC time as ISO-8601 + unix timestamp
//!   time_format       → format a unix timestamp into human-readable string
//!   time_diff         → difference between two timestamps
//!   time_add          → add/subtract seconds from a timestamp
//!   timezone_convert  → convert between timezones by offset hours
//!   time_parse        → parse ISO-8601 string to unix timestamp

#[allow(warnings)]
mod bindings;

use bindings::exports::hermez::plugin::plugin::Guest;
use bindings::hermez::plugin::host;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TimePlugin;

impl Guest for TimePlugin {
    fn register() {
        host::log("info", "Time plugin registered — provides time utilities");
    }

    fn on_session_start(_ctx: String) {}
    fn on_session_end(_ctx: String) {}

    fn handle_tool(name: String, args: String) -> Result<String, String> {
        match name.as_str() {
            "time_now" => handle_time_now(&args),
            "time_format" => handle_time_format(&args),
            "time_diff" => handle_time_diff(&args),
            "time_add" => handle_time_add(&args),
            "timezone_convert" => handle_timezone_convert(&args),
            "time_parse" => handle_time_parse(&args),
            _ => Err(format!("Unknown tool: '{}'", name)),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn get_json_str(args: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let start = args.find(&pattern)? + pattern.len();
    let rest = &args[start..];
    // skip whitespace and colon
    let rest = rest.trim_start();
    let rest = if rest.starts_with(':') { &rest[1..] } else { rest };
    let rest = rest.trim_start();
    if rest.starts_with('"') {
        let rest = &rest[1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else {
        // number or boolean — read until comma or brace
        let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn get_json_i64(args: &str, key: &str) -> Option<i64> {
    get_json_str(args, key)?.parse().ok()
}

fn get_json_f64(args: &str, key: &str) -> Option<f64> {
    get_json_str(args, key)?.parse().ok()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// Simple ISO-8601 formatter: 2024-01-15T09:30:00Z
fn fmt_iso(ts: u64) -> String {
    let (y, m, d, hh, mm, ss) = ts_to_ymd_hms(ts);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

fn ts_to_ymd_hms(ts: u64) -> (u64, u64, u64, u64, u64, u64) {
    let mut days = ts / 86400;
    let rem = ts % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;

    // Days since 1970-01-01 to year/month/day
    let mut year = 1970u64;
    loop {
        let dpy = if is_leap_year(year) { 366 } else { 365 };
        if days < dpy {
            break;
        }
        days -= dpy;
        year += 1;
    }

    let month_days = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            month = (i + 1) as u64;
            break;
        }
        days -= md;
        month = (i + 2) as u64;
    }

    (year, month, days + 1, hh, mm, ss)
}

fn is_leap_year(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn parse_iso_to_ts(s: &str) -> Option<u64> {
    // Parse "2024-01-15T09:30:00Z" or "2024-01-15 09:30:00"
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let y: u64 = s[0..4].parse().ok()?;
    let m: u64 = s[5..7].parse().ok()?;
    let d: u64 = s[8..10].parse().ok()?;
    let hh: u64 = s[11..13].parse().ok()?;
    let mm: u64 = s[14..16].parse().ok()?;
    let ss: u64 = s[17..19].parse().ok()?;

    // Compute days since 1970-01-01
    let mut days = 0u64;
    for year in 1970..y {
        days += if is_leap_year(year) { 366 } else { 365 };
    }

    let month_days = if is_leap_year(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    for i in 0..(m as usize - 1) {
        days += month_days[i];
    }
    days += d - 1;

    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some(secs)
}

fn fmt_readable(ts: u64) -> String {
    let (y, m, d, hh, mm, ss) = ts_to_ymd_hms(ts);
    let wd = weekday(y, m, d);
    let mon = month_name(m);
    format!("{} {}, {:04} at {:02}:{:02}:{:02} UTC", wd, mon, y, hh, mm, ss)
}

fn weekday(y: u64, m: u64, d: u64) -> &'static str {
    // Zeller-like: compute day of week for Gregorian calendar
    let mut y = y;
    let mut m = m;
    if m < 3 {
        m += 12;
        y -= 1;
    }
    let k = y % 100;
    let j = y / 100;
    let f = d + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 + 5 * j;
    let w = f % 7;
    // 0=Sat, 1=Sun, ...
    match w {
        0 => "Saturday",
        1 => "Sunday",
        2 => "Monday",
        3 => "Tuesday",
        4 => "Wednesday",
        5 => "Thursday",
        6 => "Friday",
        _ => "Unknown",
    }
}

fn month_name(m: u64) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    }
}

// ── Tool Handlers ────────────────────────────────────────────────────────────

fn handle_time_now(_args: &str) -> Result<String, String> {
    let ts = unix_now();
    let iso = fmt_iso(ts);
    Ok(format!(
        "{{\"timestamp\":{},\"iso\":\"{}\",\"readable\":\"{}\"}}",
        ts, iso, fmt_readable(ts)
    ))
}

fn handle_time_format(args: &str) -> Result<String, String> {
    let ts = get_json_i64(args, "timestamp").ok_or("Missing 'timestamp'")? as u64;
    let format = get_json_str(args, "format").unwrap_or_else(|| "iso".to_string());
    let result = match format.as_str() {
        "iso" => fmt_iso(ts),
        "readable" => fmt_readable(ts),
        "date" => {
            let (y, m, d, _, _, _) = ts_to_ymd_hms(ts);
            format!("{:04}-{:02}-{:02}", y, m, d)
        }
        "time" => {
            let (_, _, _, hh, mm, ss) = ts_to_ymd_hms(ts);
            format!("{:02}:{:02}:{:02}", hh, mm, ss)
        }
        _ => fmt_iso(ts),
    };
    Ok(format!(
        "{{\"timestamp\":{},\"format\":\"{}\",\"result\":\"{}\"}}",
        ts, format, result
    ))
}

fn handle_time_diff(args: &str) -> Result<String, String> {
    let a = get_json_i64(args, "timestamp_a").ok_or("Missing 'timestamp_a'")? as u64;
    let b = get_json_i64(args, "timestamp_b").ok_or("Missing 'timestamp_b'")? as u64;
    let diff = if a > b { a - b } else { b - a };
    let sign = if a > b { "positive" } else { "negative" };
    Ok(format!(
        "{{\"diff_seconds\":{},\"diff_minutes\":{},\"diff_hours\":{},\"diff_days\":{},\"sign\":\"{}\"}}",
        diff,
        diff / 60,
        diff / 3600,
        diff / 86400,
        sign
    ))
}

fn handle_time_add(args: &str) -> Result<String, String> {
    let ts = get_json_i64(args, "timestamp").ok_or("Missing 'timestamp'")? as u64;
    let seconds = get_json_i64(args, "seconds").ok_or("Missing 'seconds'")?;
    let result = if seconds >= 0 {
        ts + seconds as u64
    } else {
        ts.saturating_sub((-seconds) as u64)
    };
    Ok(format!(
        "{{\"original\":{},\"seconds_added\":{},\"result\":{},\"iso\":\"{}\"}}",
        ts, seconds, result, fmt_iso(result)
    ))
}

fn handle_timezone_convert(args: &str) -> Result<String, String> {
    let ts = get_json_i64(args, "timestamp").ok_or("Missing 'timestamp'")? as u64;
    let offset = get_json_f64(args, "offset_hours").ok_or("Missing 'offset_hours'")?;
    let offset_secs = (offset * 3600.0) as i64;
    let local_ts = if offset_secs >= 0 {
        ts + offset_secs as u64
    } else {
        ts.saturating_sub((-offset_secs) as u64)
    };
    let (_y, _m, _d, _hh, _mm, _ss) = ts_to_ymd_hms(local_ts);
    let sign = if offset >= 0.0 { "+" } else { "-" };
    let abs_off = offset.abs();
    let off_h = abs_off as u64;
    let off_m = ((abs_off - abs_off.floor()) * 60.0) as u64;
    Ok(format!(
        "{{\"original\":{},\"offset_hours\":{},\"local_timestamp\":{},\"local_iso\":\"{}\",\"local_readable\":\"{}\",\"offset\":\"{}{}:{:02}\"}}",
        ts,
        offset,
        local_ts,
        fmt_iso(local_ts),
        fmt_readable(local_ts),
        sign,
        off_h,
        off_m
    ))
}

fn handle_time_parse(args: &str) -> Result<String, String> {
    let input = get_json_str(args, "input").ok_or("Missing 'input'")?;
    let ts = parse_iso_to_ts(&input).ok_or("Failed to parse input as ISO-8601")?;
    Ok(format!(
        "{{\"input\":\"{}\",\"timestamp\":{},\"iso\":\"{}\",\"readable\":\"{}\"}}",
        input, ts, fmt_iso(ts), fmt_readable(ts)
    ))
}

bindings::export!(TimePlugin with_types_in bindings);
