#![allow(dead_code)]
//! Log viewer command — view, filter, and follow log files.

use console::Style;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }
fn red() -> Style { Style::new().red() }

/// Log file names recognized by Hermes.
const LOG_FILES: &[(&str, &str)] = &[
    ("agent.log", "Main agent log"),
    ("errors.log", "Error-only log"),
    ("gateway.log", "Gateway platform log"),
];

fn log_path(name: &str) -> PathBuf {
    get_hermes_home().join(name)
}

/// Parse a duration string like "1h", "30m", "2d".
fn parse_since(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let (num_str, unit) = if let Some(s) = s.strip_suffix('h') {
        (s, 3600u64)
    } else if let Some(s) = s.strip_suffix('m') {
        (s, 60u64)
    } else if let Some(s) = s.strip_suffix('d') {
        (s, 86400u64)
    } else {
        return None;
    };
    let n: u64 = num_str.parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let cutoff = now.as_secs().saturating_sub(n * unit);
    Some(UNIX_EPOCH + std::time::Duration::from_secs(cutoff))
}

/// Level filter values in priority order.
fn level_priority(level: &str) -> Option<usize> {
    match level.to_uppercase().as_str() {
        "DEBUG" => Some(0),
        "INFO" => Some(1),
        "WARNING" | "WARN" => Some(2),
        "ERROR" => Some(3),
        _ => None,
    }
}

/// Extract log level from a line (matches "INFO", "WARNING", "ERROR", "DEBUG").
fn line_level(line: &str) -> Option<&str> {
    ["ERROR", "WARNING", "WARN", "INFO", "DEBUG"].iter().find(|&level| line.contains(level)).map(|v| v as _)
}

/// List available log files with sizes.
pub fn cmd_logs_list() -> anyhow::Result<()> {
    let home = get_hermes_home();

    println!();
    println!("{}", cyan().apply_to("◆ Log Files"));
    println!();

    let mut found = false;
    for (file, desc) in LOG_FILES {
        let path = home.join(file);
        if path.exists() {
            let metadata = std::fs::metadata(&path)?;
            let size = metadata.len();
            let modified = metadata.modified().ok()
                .and_then(|t| t.elapsed().ok())
                .map(|d| {
                    let secs = d.as_secs();
                    if secs < 60 {
                        format!("{}s ago", secs)
                    } else if secs < 3600 {
                        format!("{}m ago", secs / 60)
                    } else if secs < 86400 {
                        format!("{}h ago", secs / 3600)
                    } else {
                        format!("{}d ago", secs / 86400)
                    }
                })
                .unwrap_or_else(|| "unknown".to_string());
            println!("  {} — {} ({}, modified {})", file, format_size(size), desc, modified);
            found = true;
        }
    }

    if !found {
        println!("  {}", dim().apply_to("No log files found. Logs are created when Hermes runs."));
    }
    println!();

    Ok(())
}

/// View log lines with optional filtering.
pub fn cmd_logs_view(
    log_name: &str,
    lines: usize,
    follow: bool,
    level_filter: Option<&str>,
    session_filter: Option<&str>,
    component_filter: Option<&str>,
    since: Option<&str>,
) -> anyhow::Result<()> {
    let log_file = format!("{}.log", log_name);
    let path = log_path(&log_file);

    if !path.exists() {
        println!("  {} Log file not found: {}", yellow().apply_to("✗"), path.display());
        return Ok(());
    }

    let min_priority = level_filter.and_then(level_priority);
    let cutoff = since.and_then(parse_since);

    let content = std::fs::read_to_string(&path)?;
    let all_lines: Vec<&str> = content.lines().collect();

    // Filter lines
    let filtered: Vec<(&str, usize)> = all_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            // Level filter
            if let Some(min_p) = min_priority {
                if let Some(line_level) = line_level(line) {
                    if level_priority(line_level).unwrap_or(0) < min_p {
                        return false;
                    }
                }
            }
            // Session filter
            if let Some(session_id) = session_filter {
                if !line.contains(session_id) {
                    return false;
                }
            }
            // Component filter
            if let Some(component) = component_filter {
                if !line.to_lowercase().contains(&component.to_lowercase()) {
                    return false;
                }
            }
            // Time filter (check for ISO timestamps in line)
            if let Some(cutoff_time) = cutoff {
                if let Some(line_time) = extract_timestamp(line) {
                    if line_time < cutoff_time {
                        return false;
                    }
                }
            }
            true
        })
        .map(|(i, line)| (*line, i))
        .collect();

    if follow {
        // Follow mode: print current tail then watch
        println!("  Following {} (Ctrl+C to stop)", path.display());
        println!();
        for (line, _) in filtered.iter().take(lines) {
            println!("{}", colorize_line(line));
        }
        // Simple follow: just note that it would require inotify
        println!();
        println!("  {}", dim().apply_to("Note: Full follow mode requires file watcher. Showing current tail."));
    } else {
        // Take last N lines
        let start = filtered.len().saturating_sub(lines);
        let display: &[(&str, usize)] = &filtered[start..];

        println!();
        println!("{}", cyan().apply_to(&format!("◆ {} (last {} of {} lines)", log_file, display.len(), all_lines.len())));
        println!();

        if display.is_empty() {
            println!("  {}", dim().apply_to("No lines match the current filters."));
        } else {
            for (line, _idx) in display {
                println!("{}", colorize_line(line));
            }
        }
        println!();
    }

    Ok(())
}

/// Colorize a log line based on level.
fn colorize_line(line: &str) -> String {
    if let Some(level) = line_level(line) {
        match level {
            "ERROR" => format!("  {}", red().apply_to(line)),
            "WARNING" | "WARN" => format!("  {}", yellow().apply_to(line)),
            "INFO" => format!("  {}", line),
            "DEBUG" => format!("  {}", dim().apply_to(line)),
            _ => format!("  {}", line),
        }
    } else {
        format!("  {}", line)
    }
}

/// Extract ISO timestamp from a log line.
fn extract_timestamp(line: &str) -> Option<SystemTime> {
    // Look for patterns like 2026-04-14T10:30:00 or 2026-04-14 10:30:00
    for i in 0..line.len().saturating_sub(19) {
        let slice = &line[i..i + 19];
        if slice.len() == 19 && slice.chars().nth(4) == Some('-') && slice.chars().nth(7) == Some('-') {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(slice, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(slice, "%Y-%m-%d %H:%M:%S")) {
                let secs = dt.and_utc().timestamp() as u64;
                return Some(UNIX_EPOCH + std::time::Duration::from_secs(secs));
            }
        }
    }
    None
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1}GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1}MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1}KB", bytes as f64 / 1_000.0)
    } else {
        format!("{}B", bytes)
    }
}

/// Dispatch log subcommands.
pub fn cmd_logs(
    log_name: &str,
    lines: usize,
    follow: bool,
    level: Option<&str>,
    session: Option<&str>,
    component: Option<&str>,
    since: Option<&str>,
) -> anyhow::Result<()> {
    if log_name == "list" {
        return cmd_logs_list();
    }
    cmd_logs_view(log_name, lines, follow, level, session, component, since)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1500), "1.5KB");
        assert_eq!(format_size(2_500_000), "2.5MB");
        assert_eq!(format_size(1_500_000_000), "1.5GB");
    }

    #[test]
    fn test_parse_since_hours() {
        let cutoff = parse_since("1h").unwrap();
        let now = SystemTime::now();
        let diff = now.duration_since(cutoff).unwrap();
        assert!(diff.as_secs() >= 3500 && diff.as_secs() <= 3700);
    }

    #[test]
    fn test_parse_since_minutes() {
        let cutoff = parse_since("30m").unwrap();
        let now = SystemTime::now();
        let diff = now.duration_since(cutoff).unwrap();
        assert!(diff.as_secs() >= 1700 && diff.as_secs() <= 1900);
    }

    #[test]
    fn test_parse_since_invalid() {
        assert!(parse_since("invalid").is_none());
        assert!(parse_since("").is_none());
    }

    #[test]
    fn test_level_priority() {
        assert_eq!(level_priority("DEBUG"), Some(0));
        assert_eq!(level_priority("INFO"), Some(1));
        assert_eq!(level_priority("WARNING"), Some(2));
        assert_eq!(level_priority("WARN"), Some(2));
        assert_eq!(level_priority("ERROR"), Some(3));
        assert!(level_priority("UNKNOWN").is_none());
    }

    #[test]
    fn test_line_level() {
        assert_eq!(line_level("2026-04-14 ERROR something failed"), Some("ERROR"));
        assert_eq!(line_level("2026-04-14 WARNING low memory"), Some("WARNING"));
        assert_eq!(line_level("2026-04-14 INFO starting up"), Some("INFO"));
        assert_eq!(line_level("2026-04-14 DEBUG details"), Some("DEBUG"));
        assert!(line_level("no level here").is_none());
    }

    #[test]
    fn test_logs_list_runs() {
        let result = cmd_logs_list();
        assert!(result.is_ok());
    }
}
