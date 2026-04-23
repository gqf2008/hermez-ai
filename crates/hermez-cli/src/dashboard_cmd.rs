#![allow(dead_code)]
//! Dashboard TUI.
//!
//! Mirrors Python: hermez dashboard (interactive analytics dashboard)

use console::Style;
use std::path::PathBuf;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

fn get_hermez_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMEZ_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermez")
    } else {
        PathBuf::from(".hermez")
    }
}

/// Show interactive dashboard.
pub fn cmd_dashboard() -> anyhow::Result<()> {
    cmd_dashboard_with_opts("127.0.0.1", 8080, false, false)
}

/// Show interactive dashboard with custom options.
pub fn cmd_dashboard_with_opts(_host: &str, _port: u16, _no_open: bool, _insecure: bool) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Hermez Dashboard"));
    println!();

    let home = get_hermez_home();

    // Session stats
    let db_path = home.join("sessions.db");
    if db_path.exists() {
        if let Ok(db) = hermez_state::SessionDB::open(&db_path) {
            if let Ok(count) = db.session_count(None) {
                println!("  Sessions:      {}", green().apply_to(&count.to_string()));
            }

            // Token usage
            if let Ok(sessions) = db.list_sessions_rich(None, None, 100, 0, false) {
                let total_tokens: u64 = sessions.iter()
                    .map(|s| {
                        (s.session.input_tokens + s.session.output_tokens
                            + s.session.cache_read_tokens + s.session.cache_write_tokens) as u64
                    })
                    .sum();
                println!("  Total tokens:  {}", green().apply_to(&format_tokens(total_tokens)));
            }
        }
    } else {
        println!("  Sessions:      {}", dim().apply_to("no sessions"));
    }

    // Cron jobs
    let cron_path = home.join("cron_jobs.json");
    if cron_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cron_path) {
            if let Ok(jobs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                let enabled = jobs.iter()
                    .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                    .count();
                println!("  Cron jobs:     {} ({enabled} active)", green().apply_to(&jobs.len().to_string()));
            }
        }
    }

    // Config
    let config_path = home.join("config.yaml");
    if config_path.exists() {
        println!("  Config:        {}", green().apply_to("loaded"));
    } else {
        println!("  Config:        {}", yellow().apply_to("missing"));
    }

    // Disk usage
    if let Ok(size) = dir_size(&home) {
        println!("  Disk usage:    {}", green().apply_to(&format_size(size)));
    }

    println!();
    println!("  {}", dim().apply_to("For detailed analytics, use: hermez insights"));
    println!("  {}", dim().apply_to("For session details, use: hermez sessions list"));
    println!();

    Ok(())
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}

fn dir_size(path: &PathBuf) -> std::io::Result<u64> {
    let mut size = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                size += dir_size(&entry.path())?;
            } else {
                size += entry.metadata()?.len();
            }
        }
    }
    Ok(size)
}
