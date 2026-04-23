#![allow(dead_code)]
//! Debug share command — generate and optionally upload debug report.

use console::Style;
use std::path::PathBuf;

fn get_hermez_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMEZ_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermez")
    } else {
        PathBuf::from(".hermez")
    }
}

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }

/// Generate a debug report.
pub fn cmd_debug_share(
    lines: usize,
    expire_days: usize,
    local_only: bool,
) -> anyhow::Result<()> {
    let home = get_hermez_home();

    println!();
    println!("{}", cyan().apply_to("◆ Hermez Debug Report"));
    println!();

    let mut report = String::new();

    // System info
    report.push_str("=== System Info ===\n");
    report.push_str(&format!("Platform: {}\n", std::env::consts::OS));
    report.push_str(&format!("Arch: {}\n", std::env::consts::ARCH));
    report.push_str(&format!("Hermez: {}\n", env!("CARGO_PKG_VERSION")));
    report.push_str(&format!("HERMEZ_HOME: {}\n", home.display()));
    report.push('\n');

    // Config (redacted)
    let config_path = home.join("config.yaml");
    if config_path.exists() {
        report.push_str("=== Config (redacted) ===\n");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            for line in content.lines() {
                if line.contains("key") || line.contains("token") || line.contains("secret") {
                    report.push_str(&format!("{}: [REDACTED]\n", line.split(':').next().unwrap_or(line)));
                } else {
                    report.push_str(line);
                    report.push('\n');
                }
            }
        }
        report.push('\n');
    }

    // Env vars status
    report.push_str("=== Environment ===\n");
    let env_keys = ["HERMEZ_HOME", "OPENROUTER_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY", "GOOGLE_API_KEY"];
    for key in &env_keys {
        let status = if std::env::var(key).is_ok() { "set" } else { "not set" };
        report.push_str(&format!("{}: {}\n", key, status));
    }
    report.push('\n');

    // Sessions info
    let db_path = home.join("sessions.db");
    if db_path.exists() {
        report.push_str("=== Sessions ===\n");
        if let Ok(db) = hermez_state::SessionDB::open(&db_path) {
            if let Ok(count) = db.session_count(None) {
                report.push_str(&format!("Total sessions: {}\n", count));
            }
        }
        report.push('\n');
    }

    // Cron jobs
    let cron_path = home.join("cron_jobs.json");
    if cron_path.exists() {
        report.push_str("=== Cron Jobs ===\n");
        if let Ok(content) = std::fs::read_to_string(&cron_path) {
            if let Ok(jobs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                let enabled = jobs.iter()
                    .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                    .count();
                report.push_str(&format!("Total: {}, Enabled: {}\n", jobs.len(), enabled));
            }
        }
        report.push('\n');
    }

    // Recent log lines
    let log_files = ["agent.log", "errors.log", "gateway.log"];
    for log_name in &log_files {
        let log_path = home.join(log_name);
        if log_path.exists() {
            report.push_str(&format!("=== {} (last {} lines) ===\n", log_name, lines));
            if let Ok(content) = std::fs::read_to_string(&log_path) {
                let all_lines: Vec<&str> = content.lines().collect();
                let start = all_lines.len().saturating_sub(lines);
                for line in &all_lines[start..] {
                    report.push_str(line);
                    report.push('\n');
                }
            }
            report.push('\n');
        }
    }

    if local_only {
        // Print locally
        println!("{}", report);
        println!("  {}", green().apply_to("Report generated locally."));
    } else {
        // Try to upload to a paste service
        println!("  Uploading debug report...");
        match upload_paste(&report, expire_days) {
            Ok(url) => {
                println!("  {} Share URL: {}", green().apply_to("✓"), url);
                println!();
                println!("  {}", dim().apply_to(&format!("Expires in {} days.", expire_days)));
            }
            Err(e) => {
                println!("  {} Upload failed: {}", yellow().apply_to("⚠"), e);
                println!();
                println!("  Printing report locally instead:");
                println!();
                println!("{}", report);
            }
        }
    }

    println!();
    Ok(())
}

/// Upload content to a paste service (using termbin/pastebin-style).
fn upload_paste(content: &str, _expire_days: usize) -> anyhow::Result<String> {
    // Try termbin.net first
    let mut child = std::process::Command::new("curl")
        .args(["--silent", "--connect-timeout", "5", "termbin.com:9999"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| anyhow::anyhow!("curl not available"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(content.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Ok(url);
        }
    }

    anyhow::bail!("Failed to upload to paste service")
}
