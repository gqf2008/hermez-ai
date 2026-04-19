#![allow(dead_code)]
//! Self-update command.
//!
//! Mirrors Python: hermes update (self-update from PyPI or Git)

use console::Style;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

/// Check for and apply updates.
pub fn cmd_update(preview: bool, force: bool, _gateway: bool) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Hermes Update"));
    println!();

    let current = env!("CARGO_PKG_VERSION");
    println!("  Current version: {current}");

    if preview {
        println!("  Channel: preview (pre-release)");
    } else {
        println!("  Channel: stable");
    }

    // Check for updates
    println!("  {}", dim().apply_to("Checking for updates..."));

    match check_for_updates(preview) {
        Ok(Some(latest)) => {
            if latest == current || force {
                println!("  {} Latest version: {latest}", green().apply_to("✓"));
                apply_update(&latest)?;
            } else {
                println!("  {} Update available: {latest}", yellow().apply_to("→"));
                println!("  Run with --force to upgrade now.");
            }
        }
        Ok(None) => {
            println!("  {} Already up to date.", green().apply_to("✓"));
        }
        Err(e) => {
            println!("  {} Update check failed: {e}", yellow().apply_to("⚠"));
            println!("  {}", dim().apply_to("Update manually via: cargo install hermes --locked"));
            println!("  {}", dim().apply_to("Or: pip install --upgrade hermes-agent"));
        }
    }
    println!();

    Ok(())
}

fn check_for_updates(_preview: bool) -> anyhow::Result<Option<String>> {
    // Try multiple HTTP backends for portability
    let urls = [
        ("GitHub", "https://api.github.com/repos/nousresearch/hermes-agent/releases/latest", "tag_name"),
        ("PyPI", "https://pypi.org/pypi/hermes-agent/json", "version"),
    ];

    for (_source, url, json_key) in urls {
        // Try curl first (common on Unix)
        if let Ok(out) = run_http_get(url) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&out) {
                let version = if json_key == "tag_name" {
                    json.get(json_key).and_then(|v| v.as_str())
                        .map(|s| s.trim_start_matches('v').to_string())
                } else {
                    json.get("info").and_then(|v| v.get(json_key)).and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                };
                if let Some(v) = version {
                    return Ok(Some(v));
                }
            }
        }

        // Fallback: try powershell on Windows
        if cfg!(windows) {
            if let Ok(out) = run_powershell_get(url) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&out) {
                    if let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) {
                        return Ok(Some(tag.trim_start_matches('v').to_string()));
                    }
                    if let Some(info) = json.get("info").and_then(|v| v.get("version")).and_then(|v| v.as_str()) {
                        return Ok(Some(info.to_string()));
                    }
                }
            }
        }
    }

    Err(anyhow::anyhow!("No HTTP client available (curl, wget, or powershell)"))
}

/// Run HTTP GET using curl or wget.
fn run_http_get(url: &str) -> anyhow::Result<String> {
    // Try curl
    let output = std::process::Command::new("curl")
        .args(["--silent", "--connect-timeout", "5", "--max-time", "10", url])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }

    // Try wget
    let output = std::process::Command::new("wget")
        .args(["-qO-", "--timeout=5", url])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }

    Err(anyhow::anyhow!("curl and wget not available"))
}

/// Run HTTP GET using PowerShell on Windows.
fn run_powershell_get(url: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("powershell")
        .args(["-Command", &format!("(Invoke-WebRequest -Uri '{url}' -TimeoutSec 5).Content")])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }

    Err(anyhow::anyhow!("PowerShell HTTP request failed"))
}

fn apply_update(_version: &str) -> anyhow::Result<()> {
    println!("  {}", dim().apply_to("Updating..."));

    // Try cargo install
    let output = std::process::Command::new("cargo")
        .args(["install", "hermes", "--locked"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            println!("  {} Update applied successfully.", green().apply_to("✓"));
            return Ok(());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            println!("  {} Cargo update failed: {}", yellow().apply_to("⚠"), stderr.lines().next().unwrap_or("unknown"));
        }
        Err(_) => {
            println!("  {} Cargo not available.", yellow().apply_to("⚠"));
        }
    }

    // Fallback: try pip
    println!("  {}", dim().apply_to("Trying pip install..."));
    let output = std::process::Command::new("pip")
        .args(["install", "--upgrade", "hermes-agent"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            println!("  {} Update applied via pip.", green().apply_to("✓"));
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("pip update failed: {}", stderr.lines().next().unwrap_or("unknown"));
        }
        Err(_) => {
            anyhow::bail!("pip not available. Update manually via cargo install or pip install.");
        }
    }

    Ok(())
}
