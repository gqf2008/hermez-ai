#![allow(dead_code)]
//! Debug and diagnostics commands for Hermez Agent.
//!
//! Mirrors the Python `hermez_cli/debug.py` and `hermez_cli/dump.py`.

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

/// Print debug info about the current Hermez installation.
pub fn cmd_debug() -> anyhow::Result<()> {
    let home = get_hermez_home();

    println!();
    println!("{}", cyan().apply_to("◆ Hermez Debug Info"));
    println!();

    // HERMEZ_HOME
    println!("  HERMEZ_HOME: {}", home.display());
    println!("  Exists: {}", home.exists());

    if home.exists() {
        println!();
        println!("  {}", cyan().apply_to("Contents:"));

        for entry in std::fs::read_dir(&home)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let size = if path.is_file() {
                let metadata = std::fs::metadata(&path)?;
                format!("{} bytes", metadata.len())
            } else {
                "(directory)".to_string()
            };
            println!("    {} — {}", name, dim().apply_to(&size));
        }
    }

    // Config
    let config_path = home.join("config.yaml");
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        let lines = content.lines().count();
        println!();
        println!("  Config: {} ({lines} lines)", config_path.display());

        // Parse and show model info
        if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            if let Some(model) = config.get("model") {
                if let Some(provider) = model.get("provider").and_then(|p| p.as_str()) {
                    println!("  Provider: {provider}");
                }
                if let Some(name) = model.get("name").and_then(|n| n.as_str()) {
                    println!("  Model: {name}");
                }
                if let Some(base_url) = model.get("base_url").and_then(|u| u.as_str()) {
                    println!("  Base URL: {base_url}");
                }
            }
        }
    } else {
        println!();
        println!("  {}", yellow().apply_to("No config.yaml found. Run `hermez setup` to configure."));
    }

    // Environment variables
    println!();
    println!("  {}", cyan().apply_to("Environment Variables:"));
    let env_keys = [
        "HERMEZ_HOME",
        "OPENROUTER_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "GOOGLE_API_KEY",
        "CUSTOM_API_KEY",
    ];
    for key in &env_keys {
        let status = if std::env::var(key).is_ok() {
            green().apply_to("set").to_string()
        } else {
            dim().apply_to("not set").to_string()
        };
        println!("    {:30} {}", key, status);
    }

    // Sessions DB
    let db_path = home.join("sessions.db");
    if db_path.exists() {
        let metadata = std::fs::metadata(&db_path)?;
        println!();
        println!("  Sessions DB: {} ({} bytes)", db_path.display(), metadata.len());

        // Try to query session count
        match hermez_state::SessionDB::open(&db_path) {
            Ok(db) => {
                if let Ok(count) = db.session_count(None) {
                    println!("  Session count: {count}");
                }
            }
            Err(e) => {
                println!("  {} Could not open sessions DB: {}", yellow().apply_to("⚠"), e);
            }
        }
    }

    // Cron jobs
    let cron_path = home.join("cron_jobs.json");
    if cron_path.exists() {
        let content = std::fs::read_to_string(&cron_path)?;
        if let Ok(jobs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
            let enabled = jobs.iter().filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)).count();
            println!();
            println!("  Cron jobs: {} total, {} enabled", jobs.len(), enabled);
        }
    }

    // Process registry
    let proc_path = home.join("process_registry.json");
    if proc_path.exists() {
        let content = std::fs::read_to_string(&proc_path)?;
        if let Ok(procs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
            let running = procs.iter().filter(|p| p.get("running").and_then(|v| v.as_bool()).unwrap_or(false)).count();
            println!();
            println!("  Processes: {} tracked, {} running", procs.len(), running);
        }
    }

    println!();
    Ok(())
}

/// Delete a previously uploaded debug paste.
pub fn cmd_debug_delete(url: &str) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Delete Debug Paste"));
    println!();
    println!("  URL: {}", url);
    println!();
    println!("  {}", yellow().apply_to("Debug paste deletion not yet implemented."));
    println!();
    Ok(())
}

/// Dump session data for debugging.
pub fn cmd_dump_session(session_id: &str, _show_keys: bool) -> anyhow::Result<()> {
    let home = get_hermez_home();
    let db_path = home.join("sessions.db");

    if !db_path.exists() {
        println!("  {} No sessions database found.", yellow().apply_to("✗"));
        return Ok(());
    }

    let db = hermez_state::SessionDB::open(&db_path)?;

    // Find session by ID
    let session = db.resolve_session_id(session_id)?;
    if session.is_none() {
        println!("  {} Session not found: {}", yellow().apply_to("✗"), session_id);
        return Ok(());
    }

    let session_id_resolved = session.unwrap();

    // Get session details
    if let Ok(Some(s)) = db.get_session(&session_id_resolved) {
        println!();
        println!("{}", cyan().apply_to("◆ Session Dump"));
        println!();
        println!("  ID:         {}", s.id);
        println!("  Title:      {}", s.title.as_deref().unwrap_or("N/A"));
        println!("  Started:    {}", s.started_at);
        println!("  Updated:    {}", s.ended_at.map(|t| t.to_string()).as_deref().unwrap_or("N/A"));
        println!("  Source:     {}", s.source);
        println!("  Model:      {}", s.model.as_deref().unwrap_or("N/A"));

        if let Ok(msg_count) = db.message_count(Some(&session_id_resolved)) {
            println!("  Messages: {msg_count}");
        }

        // Get messages
        if let Ok(messages) = db.get_messages_as_conversation(&session_id_resolved) {
            let last = messages.iter().rev().take(5).rev().collect::<Vec<_>>();
            if !last.is_empty() {
                println!();
                println!("  {}", cyan().apply_to("Recent Messages (last 5):"));
                for msg in &last {
                    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                    let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let preview: String = content.chars().take(100).collect();
                    println!("    {role}: {preview}");
                }
            }
        }
    }

    println!();
    Ok(())
}

/// Dump all config and state for debugging.
pub fn cmd_dump_all(show_keys: bool) -> anyhow::Result<()> {
    let home = get_hermez_home();

    println!();
    println!("{}", cyan().apply_to("◆ Full Hermez Dump"));
    println!();
    println!("  HERMEZ_HOME: {}", home.display());
    println!();

    // Dump config
    let config_path = home.join("config.yaml");
    if config_path.exists() {
        println!("{}", cyan().apply_to("── config.yaml ──"));
        let content = std::fs::read_to_string(&config_path)?;
        println!("{content}");
    }

    // Dump .env (redacted or with key prefixes)
    let env_path = home.join(".env");
    if env_path.exists() {
        let content = std::fs::read_to_string(&env_path)?;
        if show_keys {
            println!("{}", cyan().apply_to("── .env (key prefixes) ──"));
            for line in content.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    let prefix = if value.len() <= 8 {
                        "[REDACTED]".to_string()
                    } else {
                        format!("{}..{}", &value[..4], &value[value.len() - 4..])
                    };
                    println!("{key}={prefix}");
                }
            }
        } else {
            println!("{}", cyan().apply_to("── .env (redacted) ──"));
            for line in content.lines() {
                if let Some((key, _)) = line.split_once('=') {
                    println!("{key}=[REDACTED]");
                }
            }
        }
    }

    // Dump cron jobs
    let cron_path = home.join("cron_jobs.json");
    if cron_path.exists() {
        println!("{}", cyan().apply_to("── cron_jobs.json ──"));
        let content = std::fs::read_to_string(&cron_path)?;
        println!("{content}");
    }

    // Dump skill commands
    let skills_dir = home.join("skills");
    if skills_dir.exists() {
        println!("{}", cyan().apply_to("── skills/ ──"));
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            println!("  {name}");
        }
    }

    println!();
    Ok(())
}
