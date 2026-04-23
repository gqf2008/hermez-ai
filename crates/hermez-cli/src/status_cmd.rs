#![allow(dead_code)]
//! Status command — show status of all Hermez components.

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

fn green() -> Style { Style::new().green() }
fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }
fn red() -> Style { Style::new().red() }

/// Show status of all Hermez components.
pub fn cmd_status(_all: bool, _deep: bool) -> anyhow::Result<()> {
    let home = get_hermez_home();

    println!();
    println!("{}", cyan().apply_to("◆ Hermez Status"));
    println!();

    let mut warnings = Vec::new();

    // HERMEZ_HOME
    let status = if home.exists() {
        green().apply_to("OK").to_string()
    } else {
        warnings.push("HERMEZ_HOME not found");
        yellow().apply_to("not found").to_string()
    };
    println!("  {:20} {}  {}", "HERMEZ_HOME", status, dim().apply_to(home.display().to_string()));

    // Config
    let config_path = home.join("config.yaml");
    let config_status = if config_path.exists() {
        green().apply_to("configured").to_string()
    } else {
        warnings.push("config.yaml not found");
        yellow().apply_to("not configured").to_string()
    };
    println!("  {:20} {}  {}", "Config", config_status, dim().apply_to(config_path.display().to_string()));

    // .env / API keys
    let env_path = home.join(".env");
    let api_keys = [
        ("OPENAI_API_KEY", "OpenAI"),
        ("ANTHROPIC_API_KEY", "Anthropic"),
        ("DEEPSEEK_API_KEY", "DeepSeek"),
        ("GOOGLE_API_KEY", "Google"),
    ];
    let mut key_count = 0;
    for (env_var, label) in &api_keys {
        if std::env::var(env_var).is_ok() {
            key_count += 1;
        } else if env_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&env_path) {
                if content.contains(env_var) {
                    key_count += 1;
                }
            }
        }
        let _ = label; // used for future display
    }
    let keys_status = if key_count > 0 {
        green().apply_to(format!("{key_count} set")).to_string()
    } else {
        warnings.push("No API keys configured");
        yellow().apply_to("none").to_string()
    };
    println!("  {:20} {}", "API Keys", keys_status);

    // Nous Subscription Features
    if hermez_tools::tool_backend_helpers::managed_nous_tools_enabled() {
        let features = crate::nous_subscription::get_nous_subscription_features(None);
        println!();
        println!("{}", cyan().apply_to("◆ Nous Subscription Features"));
        let (portal_marker, portal_state) = if features.nous_auth_present {
            (green().apply_to("✓"), "managed tools available")
        } else {
            (red().apply_to("✗"), "not logged in")
        };
        println!("  {:<15} {} {}", "Nous Portal", portal_marker, portal_state);
        for feature in features.items() {
            let state = if feature.managed_by_nous {
                "active via Nous subscription"
            } else if feature.active {
                let current = if feature.current_provider.is_empty() {
                    "configured provider"
                } else {
                    &feature.current_provider
                };
                &format!("active via {current}")
            } else if feature.included_by_default && features.nous_auth_present {
                "included by subscription, not currently selected"
            } else if feature.key == "modal" && features.nous_auth_present {
                "available via subscription (optional)"
            } else {
                "not configured"
            };
            let available = feature.available || feature.active || feature.managed_by_nous;
            let marker = if available { green().apply_to("✓") } else { red().apply_to("✗") };
            println!("  {:<15} {} {}", feature.label, marker, state);
        }
    }

    // Sessions DB
    let db_path = home.join("sessions.db");
    let db_status = if db_path.exists() {
        match std::fs::metadata(&db_path) {
            Ok(metadata) => green().apply_to(format!("{} bytes", metadata.len())).to_string(),
            Err(e) => yellow().apply_to(format!("inaccessible: {e}")).to_string(),
        }
    } else {
        dim().apply_to("empty").to_string()
    };
    println!("  {:20} {}", "Sessions DB", db_status);

    // Gateway
    let gateway_config = if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if content.contains("gateway") || content.contains("platforms") {
                green().apply_to("configured").to_string()
            } else {
                dim().apply_to("not configured").to_string()
            }
        } else {
            dim().apply_to("unreadable").to_string()
        }
    } else {
        dim().apply_to("not configured").to_string()
    };
    println!("  {:20} {}", "Gateway", gateway_config);

    // Cron jobs
    let cron_path = home.join("cron_jobs.json");
    let cron_status = if cron_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cron_path) {
            if let Ok(jobs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                let enabled = jobs.iter()
                    .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                    .count();
                if enabled > 0 {
                    green().apply_to(format!("{enabled} active")).to_string()
                } else {
                    dim().apply_to(format!("{} total, 0 active", jobs.len())).to_string()
                }
            } else {
                red().apply_to("parse error").to_string()
            }
        } else {
            red().apply_to("unreadable").to_string()
        }
    } else {
        dim().apply_to("no jobs").to_string()
    };
    println!("  {:20} {}", "Cron Jobs", cron_status);

    // Skills
    let skills_dir = home.join("skills");
    let skills_status = if skills_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            let count = entries.count();
            green().apply_to(format!("{count} installed")).to_string()
        } else {
            dim().apply_to("unreadable").to_string()
        }
    } else {
        dim().apply_to("none").to_string()
    };
    println!("  {:20} {}", "Skills", skills_status);

    // Terminal backend
    let terminal_backend = std::env::var("HERMEZ_TERMINAL_BACKEND")
        .ok()
        .or_else(|| {
            if config_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    if content.contains("terminal:") {
                        Some("configured".to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| "local (default)".to_string());
    println!("  {:20} {}", "Terminal", terminal_backend);

    println!();
    if !warnings.is_empty() {
        println!("  {}", yellow().apply_to(&format!("⚠ {} warning(s):", warnings.len())));
        for w in &warnings {
            println!("    - {w}");
        }
        println!("  {}", yellow().apply_to("Run `hermez setup` to configure missing components."));
        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_hermez_home_from_env() {
        std::env::set_var("HERMEZ_HOME", "/tmp/test_hermez_home");
        let path = get_hermez_home();
        assert_eq!(path, PathBuf::from("/tmp/test_hermez_home"));
        std::env::remove_var("HERMEZ_HOME");
    }

    #[test]
    fn test_status_runs_without_errors() {
        // Should not panic even without HERMEZ_HOME
        let result = cmd_status(false, false);
        assert!(result.is_ok());
    }
}
