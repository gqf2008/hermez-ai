#![allow(dead_code)]
//! Doctor command — diagnose common configuration issues.
//!
//! Mirrors the Python `hermes_cli/doctor.py`.
//! Runs a battery of diagnostic checks across the entire Hermes setup.

use std::path::PathBuf;

use console::Style;
use hermes_core::hermes_home::{display_hermes_home, get_hermes_home};

// ---------------------------------------------------------------------------
// Styling helpers
// ---------------------------------------------------------------------------

fn green() -> Style { Style::new().green() }
fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }
fn red() -> Style { Style::new().red() }
fn bold() -> Style { Style::new().bold() }

fn check_ok(text: &str, detail: &str) {
    let detail_str = if detail.is_empty() {
        String::new()
    } else {
        format!(" {}", dim().apply_to(detail))
    };
    println!("  {} {}{}", green().apply_to("\u{2713}"), text, detail_str);
}

fn check_warn(text: &str, detail: &str) {
    let detail_str = if detail.is_empty() {
        String::new()
    } else {
        format!(" {}", dim().apply_to(detail))
    };
    println!("  {} {}{}", yellow().apply_to("\u{26a0}"), text, detail_str);
}

fn check_fail(text: &str, detail: &str) {
    let detail_str = if detail.is_empty() {
        String::new()
    } else {
        format!(" {}", dim().apply_to(detail))
    };
    println!("  {} {}{}", red().apply_to("\u{2717}"), text, detail_str);
}

fn check_info(text: &str) {
    println!("    {} {}", cyan().apply_to("\u{2192}"), text);
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn config_path() -> PathBuf {
    get_hermes_home().join("config.yaml")
}

fn env_path() -> PathBuf {
    get_hermes_home().join(".env")
}

fn load_config_yaml() -> Option<serde_yaml::Value> {
    let path = config_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_yaml::from_str(&content).ok()
}

/// API key environment variables that indicate a provider is configured.
const PROVIDER_ENV_KEYS: &[&str] = &[
    "OPENROUTER_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_TOKEN",
    "OPENAI_BASE_URL",
    "NOUS_API_KEY",
    "GLM_API_KEY",
    "ZAI_API_KEY",
    "Z_AI_API_KEY",
    "KIMI_API_KEY",
    "KIMI_CN_API_KEY",
    "MINIMAX_API_KEY",
    "MINIMAX_CN_API_KEY",
    "DEEPSEEK_API_KEY",
    "DASHSCOPE_API_KEY",
    "HF_TOKEN",
    "AI_GATEWAY_API_KEY",
    "OPENCODE_ZEN_API_KEY",
    "OPENCODE_GO_API_KEY",
];

/// Check if the .env file contains any provider auth/base URL settings.
fn has_provider_env_config(content: &str) -> bool {
    PROVIDER_ENV_KEYS.iter().any(|key| content.contains(key))
}

/// Check if a given env var is set or present in .env file.
fn env_key_configured(env_var: &str) -> bool {
    if std::env::var(env_var).is_ok() {
        return true;
    }
    let path = env_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content.contains(env_var);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Diagnostic sections
// ---------------------------------------------------------------------------

/// Check system information (OS, Rust version).
fn check_system_info(issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ System Information"));

    // OS info
    let os_info = format!(
        "{} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    check_ok(&os_info, "");

    // Rust version (from compile-time env vars, same as version_cmd.rs)
    let rust_version = env!("CARGO_PKG_RUST_VERSION");
    let rustc_version = option_env!("RUSTC_VERSION").unwrap_or("stable");
    check_ok(&format!("MSRV: {rust_version}"), "");
    check_ok(&format!("Rustc: {rustc_version}"), "");

    // Hermes CLI version
    let hermes_version = env!("CARGO_PKG_VERSION");
    check_ok(&format!("Hermes: {hermes_version}"), "");

    // Working directory permissions
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.exists() {
            check_ok("Working directory accessible", &cwd.display().to_string());
        } else {
            check_warn("Working directory inaccessible", &cwd.display().to_string());
            issues.push("Current working directory is not accessible".to_string());
        }
    }
}

/// Check configuration files (config.yaml, .env).
fn check_configuration(issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Configuration Files"));

    let dhh = display_hermes_home();

    // Check .env file
    let env = env_path();
    if env.exists() {
        check_ok(&format!("{dhh}/.env exists"), "");

        if let Ok(content) = std::fs::read_to_string(&env) {
            if has_provider_env_config(&content) {
                check_ok("API key or custom endpoint configured", "");
            } else {
                check_warn(&format!("No API key found in {dhh}/.env"), "");
                issues.push("Run 'hermes setup' to configure API keys".to_string());
            }
        }
    } else {
        check_fail(&format!("{dhh}/.env file missing"), "");
        check_info("Run 'hermes setup' to create one");
        issues.push("Run 'hermes setup' to create .env".to_string());
    }

    // Check config.yaml
    let cfg = config_path();
    if cfg.exists() {
        check_ok(&format!("{dhh}/config.yaml exists"), "");

        // Validate YAML structure
        if let Some(config) = load_config_yaml() {
            // Check for stale root-level keys (provider, base_url should be under model:)
            let stale_keys: Vec<&str> = ["provider", "base_url"]
                .iter()
                .filter(|&&k| {
                    config.get(k).and_then(|v| v.as_str()).is_some()
                })
                .copied()
                .collect();

            if !stale_keys.is_empty() {
                check_warn(
                    &format!("Stale root-level config keys: {}", stale_keys.join(", ")),
                    "(should be under 'model:' section)",
                );
                issues.push("Stale root-level provider/base_url in config.yaml — run 'hermes doctor --fix'".to_string());
            }

            // Check for required model keys
            let has_provider = config.get("model")
                .and_then(|m| m.get("provider"))
                .is_some();
            let has_model = config.get("model")
                .and_then(|m| m.get("name"))
                .is_some();

            if has_provider {
                check_ok("model.provider is set", "");
            } else {
                check_warn("model.provider not set", "(using default provider)");
            }

            if has_model {
                check_ok("model.name is set", "");
            } else {
                check_warn("model.name not set", "(using default model)");
            }
        } else {
            check_fail("config.yaml is not valid YAML", "");
            issues.push("config.yaml contains invalid YAML syntax".to_string());
        }
    } else {
        check_warn("config.yaml not found", "(using defaults)");
    }
}

/// Check environment variables for API keys.
fn check_environment(_issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Environment Variables"));

    let key_providers = [
        ("OPENROUTER_API_KEY", "OpenRouter"),
        ("OPENAI_API_KEY", "OpenAI"),
        ("ANTHROPIC_API_KEY", "Anthropic"),
        ("NOUS_API_KEY", "Nous Research"),
        ("DEEPSEEK_API_KEY", "DeepSeek"),
        ("GOOGLE_API_KEY", "Google"),
    ];

    let mut any_key_found = false;
    for (env_var, label) in key_providers {
        if env_key_configured(env_var) {
            let source = if std::env::var(env_var).is_ok() { "env" } else { ".env" };
            check_ok(label, &format!("configured ({source})"));
            any_key_found = true;
        }
    }

    // Also check extended providers
    let extended_providers = [
        ("GLM_API_KEY", "Z.AI / GLM"),
        ("ZAI_API_KEY", "Z.AI"),
        ("Z_AI_API_KEY", "Z.AI (alt)"),
        ("KIMI_API_KEY", "Kimi / Moonshot"),
        ("KIMI_CN_API_KEY", "Kimi (China)"),
        ("MINIMAX_API_KEY", "MiniMax"),
        ("DASHSCOPE_API_KEY", "Alibaba / DashScope"),
        ("HF_TOKEN", "Hugging Face"),
        ("AI_GATEWAY_API_KEY", "Vercel AI Gateway"),
        ("KILOCODE_API_KEY", "Kilo Code"),
        ("OPENCODE_ZEN_API_KEY", "OpenCode Zen"),
        ("OPENCODE_GO_API_KEY", "OpenCode Go"),
    ];

    for (env_var, label) in extended_providers {
        if env_key_configured(env_var) {
            check_ok(label, "configured");
            any_key_found = true;
        }
    }

    if !any_key_found {
        check_warn("No API keys configured", "");
        check_info("Run 'hermes setup' to configure your AI provider");
    }
}

/// Check directory structure.
fn check_directory_structure(_issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Directory Structure"));

    let hermes_home = get_hermes_home();
    let dhh = display_hermes_home();

    // Check HERMES_HOME
    if hermes_home.exists() {
        check_ok(&format!("{dhh} directory exists"), "");

        // Check writability
        let test_file = hermes_home.join(".doctor_test");
        if std::fs::write(&test_file, "").is_ok() {
            let _ = std::fs::remove_file(&test_file);
            check_ok(&format!("{dhh} is writable"), "");
        } else {
            check_fail(&format!("{dhh} is not writable"), "");
        }
    } else {
        check_warn(&format!("{dhh} not found"), "(will be created on first use)");
    }

    // Check expected subdirectories
    let expected_subdirs = ["cron", "sessions", "logs", "skills", "memories"];
    for subdir_name in &expected_subdirs {
        let subdir_path = hermes_home.join(subdir_name);
        if subdir_path.exists() {
            check_ok(&format!("{dhh}/{subdir_name}/ exists"), "");
        } else {
            check_warn(
                &format!("{dhh}/{subdir_name}/ not found"),
                "(will be created on first use)",
            );
        }
    }

    // Check SOUL.md
    let soul_path = hermes_home.join("SOUL.md");
    if soul_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&soul_path) {
            let has_content = content.lines().any(|l| {
                let trimmed = l.trim();
                !trimmed.is_empty()
                    && !trimmed.starts_with("<!--")
                    && !trimmed.starts_with("-->")
                    && !trimmed.starts_with("#")
            });
            if has_content {
                check_ok(&format!("{dhh}/SOUL.md exists"), "(persona configured)");
            } else {
                check_info(&format!("{dhh}/SOUL.md exists but is empty — edit it to customize personality"));
            }
        }
    } else {
        check_warn(
            &format!("{dhh}/SOUL.md not found"),
            "(create it to give Hermes a custom personality)",
        );
    }

    // Check memories directory contents
    let memories_dir = hermes_home.join("memories");
    if memories_dir.exists() {
        check_ok(&format!("{dhh}/memories/ directory exists"), "");
        let memory_file = memories_dir.join("MEMORY.md");
        let user_file = memories_dir.join("USER.md");

        if memory_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&memory_file) {
                let chars = content.chars().count();
                if chars > 0 {
                    check_ok(&format!("{dhh}/memories/MEMORY.md"), &format!("({chars} chars)"));
                }
            }
        } else {
            check_info("MEMORY.md not created yet (will be created on first memory write)");
        }

        if user_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&user_file) {
                let chars = content.chars().count();
                if chars > 0 {
                    check_ok(&format!("{dhh}/memories/USER.md"), &format!("({chars} chars)"));
                }
            }
        } else {
            check_info("USER.md not created yet (will be created on first memory write)");
        }
    }
}

/// Check session database integrity.
fn check_session_database(issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Session Database"));

    let hermes_home = get_hermes_home();
    let dhh = display_hermes_home();
    let db_path = hermes_home.join("sessions.db");

    if db_path.exists() {
        match std::fs::metadata(&db_path) {
            Ok(metadata) => {
                check_ok(&format!("{dhh}/sessions.db exists"), &format!("({} bytes)", metadata.len()));
            }
            Err(e) => {
                check_fail(&format!("{dhh}/sessions.db inaccessible"), &e.to_string());
                issues.push("Session database file is inaccessible".to_string());
                return;
            }
        }

        // Try to open and validate
        match hermes_state::SessionDB::open(&db_path) {
            Ok(db) => {
                if let Ok(count) = db.session_count(None) {
                    check_ok(&format!("{count} session(s) recorded"), "");
                }

                // FTS5 search check
                match db.search_messages("test", None, None, None, 1, 0) {
                    Ok(_) => check_ok("FTS5 search available", "(full-text search enabled)"),
                    Err(_) => check_warn("FTS5 search unavailable", "(search will use fallback)"),
                }
            }
            Err(e) => {
                check_fail("Failed to open session database", &e.to_string());
                issues.push(format!("Session database error: {e}"));
            }
        }
    } else {
        check_info(&format!("{dhh}/sessions.db not created yet (will be created on first session)"));
    }

    // Check WAL file size
    let wal_path = hermes_home.join("sessions.db-wal");
    if wal_path.exists() {
        if let Ok(metadata) = std::fs::metadata(&wal_path) {
            let size_mb = metadata.len() as f64 / (1024.0 * 1024.0);
            if size_mb > 50.0 {
                check_warn(
                    &format!("WAL file is large ({size_mb:.0} MB)"),
                    "(may indicate missed checkpoints)",
                );
                issues.push("Large WAL file — run 'hermes doctor --fix' to checkpoint".to_string());
            } else if size_mb > 10.0 {
                check_info(&format!("WAL file is {size_mb:.0} MB (normal for active sessions)"));
            }
        }
    }
}

/// Check external tool availability.
fn check_external_tools(issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ External Tools"));

    // git
    if which::which("git").is_ok() {
        check_ok("git", "");
    } else {
        check_warn("git not found", "(optional)");
    }

    // ripgrep (optional, for faster file search)
    if which::which("rg").is_ok() {
        check_ok("ripgrep (rg)", "(faster file search)");
    } else {
        check_warn("ripgrep (rg) not found", "(file search uses grep fallback)");
        let install_hint = if cfg!(target_os = "macos") {
            "brew install ripgrep"
        } else if cfg!(unix) {
            "sudo apt install ripgrep"
        } else {
            "Install from https://github.com/BurntSushi/ripgrep"
        };
        check_info(install_hint);
    }

    // Docker (optional)
    if which::which("docker").is_ok() {
        // Check if docker daemon is running
        let output = std::process::Command::new("docker")
            .arg("info")
            .output();
        match output {
            Ok(out) if out.status.success() => {
                check_ok("docker", "(daemon running)");
            }
            Ok(_) => {
                check_warn("docker found", "(daemon not running)");
            }
            Err(_) => {
                check_ok("docker", "(CLI available)");
            }
        }
    } else {
        check_warn("docker not found", "(optional)");
    }

    // bash
    if which::which("bash").is_ok() {
        check_ok("bash", "");
    } else {
        check_fail("bash not found", "(required for command execution)");
        issues.push("bash is required but not found".to_string());
    }

    // Node.js (optional, for browser tools)
    if which::which("node").is_ok() {
        check_ok("Node.js", "");
    } else {
        check_warn("Node.js not found", "(optional, needed for browser tools)");
    }
}

/// Check tool registry.
fn check_tools(_issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Tool Registry"));

    let mut registry = hermes_tools::registry::ToolRegistry::new();
    hermes_tools::register_all_tools(&mut registry);
    let total = registry.len();
    let available = registry.get_available_tools();

    check_ok(&format!("{total} tools registered"), "");
    check_ok(&format!("{} available", available.len()), "(prerequisites met)");

    let unavailable_count = total - available.len();
    if unavailable_count > 0 {
        check_warn(&format!("{unavailable_count} tool(s) unavailable"), "(missing prerequisites or API keys)");
    }
}

/// Check Skills Hub.
fn check_skills_hub(_issues: &mut Vec<String>) {
    println!();
    println!("{}", bold().apply_to("◆ Skills Hub"));

    let hermes_home = get_hermes_home();
    let dhh = display_hermes_home();
    let hub_dir = hermes_home.join("skills").join(".hub");

    if hub_dir.exists() {
        check_ok(&format!("{dhh}/skills/.hub exists"), "");

        let lock_file = hub_dir.join("lock.json");
        if lock_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&lock_file) {
                if let Ok(lock_data) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(installed) = lock_data.get("installed") {
                        if let Some(obj) = installed.as_object() {
                            check_ok(&format!("Lock file OK ({} hub-installed skill(s))", obj.len()), "");
                        } else {
                            check_ok("Lock file OK", "");
                        }
                    } else {
                        check_ok("Lock file OK", "");
                    }
                } else {
                    check_warn("Lock file", "(corrupted or unreadable)");
                }
            }
        } else {
            check_warn("Lock file not found", "");
        }

        // Check quarantine
        let quarantine = hub_dir.join("quarantine");
        if quarantine.exists() {
            let q_count = std::fs::read_dir(&quarantine)
                .map(|entries| entries.filter(|e| e.as_ref().map(|e| e.path().is_dir()).unwrap_or(false)).count())
                .unwrap_or(0);
            if q_count > 0 {
                check_warn(&format!("{q_count} skill(s) in quarantine"), "(pending review)");
            }
        }
    } else {
        check_warn("Skills Hub directory not initialized", "(run: hermes skills list)");
    }

    // Check GitHub token
    let github_token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok();
    if github_token.is_some() {
        check_ok("GitHub token configured", "(authenticated API access)");
    } else {
        check_warn("No GITHUB_TOKEN", &format!("(60 req/hr rate limit — set in {dhh}/.env for better rates)"));
    }
}

/// Check cron jobs configuration.
fn check_cron_jobs() {
    println!();
    println!("{}", bold().apply_to("◆ Cron Jobs"));

    let hermes_home = get_hermes_home();
    let cron_path = hermes_home.join("cron_jobs.json");

    if cron_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cron_path) {
            if let Ok(jobs) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                let enabled = jobs.iter()
                    .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                    .count();
                if enabled > 0 {
                    check_ok(&format!("{enabled} active job(s)"), "");
                } else {
                    check_warn(&format!("{} total job(s), 0 active", jobs.len()), "");
                }
            } else {
                check_fail("cron_jobs.json", "(parse error)");
            }
        } else {
            check_fail("cron_jobs.json", "(unreadable)");
        }
    } else {
        check_info("No cron jobs configured");
    }
}

/// Check gateway configuration.
fn check_gateway() {
    println!();
    println!("{}", bold().apply_to("◆ Gateway"));

    let dhh = display_hermes_home();
    let cfg_path = config_path();

    if cfg_path.exists() {
        if let Some(config) = load_config_yaml() {
            if let Some(gateway) = config.get("gateway") {
                if let Some(enabled) = gateway.get("enabled").and_then(|v| v.as_bool()) {
                    if enabled {
                        check_ok("Gateway enabled", "");
                    } else {
                        check_warn("Gateway disabled", "(set gateway.enabled: true in config.yaml)");
                    }
                } else {
                    check_info("Gateway section found but 'enabled' flag not set");
                }

                // Check platforms
                if let Some(platforms) = gateway.get("platforms") {
                    if let Some(arr) = platforms.as_sequence() {
                        let platform_names: Vec<&str> = arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect();
                        if !platform_names.is_empty() {
                            check_ok(&format!("Platforms: {}", platform_names.join(", ")), "");
                        }
                    }
                }
            } else {
                check_info("No gateway section in config.yaml");
            }
        }
    } else {
        check_info(&format!("No config.yaml at {dhh}"));
    }
}

// ---------------------------------------------------------------------------
// Auto-fix mode
// ---------------------------------------------------------------------------

/// Run doctor in auto-fix mode — attempt to resolve detected issues.
pub fn cmd_doctor_fix() -> anyhow::Result<()> {
    let mut fixed_count = 0;
    let mut failed: Vec<String> = Vec::new();

    println!();
    println!("{}", bold().apply_to("┌─────────────────────────────────────────────────────────┐"));
    println!("{}", bold().apply_to("│              Hermes Doctor — Auto-Fix                  │"));
    println!("{}", bold().apply_to("└─────────────────────────────────────────────────────────┘"));

    let hermes_home = get_hermes_home();

    // Ensure HERMES_HOME exists
    if !hermes_home.exists() {
        print!("  {} Creating HERMES_HOME... ", yellow().apply_to("→"));
        match std::fs::create_dir_all(&hermes_home) {
            Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
            Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push("create HERMES_HOME".to_string()); }
        }
    }

    // Create missing subdirectories
    let expected_subdirs = ["cron", "sessions", "logs", "skills", "memories"];
    for subdir_name in &expected_subdirs {
        let subdir_path = hermes_home.join(subdir_name);
        if !subdir_path.exists() {
            print!("  {} Creating {subdir_name}/... ", yellow().apply_to("→"));
            match std::fs::create_dir_all(&subdir_path) {
                Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
                Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push(format!("create {subdir_name}/")); }
            }
        }
    }

    // Create default .env if missing
    let env = env_path();
    if !env.exists() {
        print!("  {} Creating .env template... ", yellow().apply_to("→"));
        let default_content = "# API keys for Hermes Agent\n# Get yours at openrouter.ai or nousresearch.com\n";
        match std::fs::write(&env, default_content) {
            Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
            Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push("create .env".to_string()); }
        }
    }

    // Create default config.yaml if missing
    let cfg = config_path();
    if !cfg.exists() {
        print!("  {} Creating config.yaml... ", yellow().apply_to("→"));
        let default_config = "# Hermes Agent configuration\nmodel:\n  name: anthropic/claude-opus-4.6\n";
        match std::fs::write(&cfg, default_config) {
            Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
            Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push("create config.yaml".to_string()); }
        }
    }

    // Create default SOUL.md if missing
    let soul = hermes_home.join("SOUL.md");
    if !soul.exists() {
        print!("  {} Creating SOUL.md template... ", yellow().apply_to("→"));
        let default_soul = "# SOUL.md — Custom personality for Hermes Agent\n# Edit this file to customize your agent's behavior\n\nYou are Hermes, a helpful AI assistant.\n";
        match std::fs::write(&soul, default_soul) {
            Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
            Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push("create SOUL.md".to_string()); }
        }
    }

    // Fix stale root-level config keys
    if let Some(mut config) = load_config_yaml() {
        if let Some(map) = config.as_mapping_mut() {
            let stale_keys: Vec<String> = ["provider", "base_url"]
                .iter()
                .filter(|&&k| {
                    map.get(serde_yaml::Value::String(k.to_string()))
                        .and_then(|v| v.as_str())
                        .is_some()
                })
                .map(|&k| k.to_string())
                .collect();

            if !stale_keys.is_empty() {
                print!("  {} Migrating stale root-level keys... ", yellow().apply_to("→"));
                // Collect values first, then remove from root, then add to model
                let mut kv_pairs: Vec<(serde_yaml::Value, serde_yaml::Value)> = Vec::new();
                for key in &stale_keys {
                    let key_val = serde_yaml::Value::String(key.clone());
                    if let Some(value) = map.remove(&key_val) {
                        kv_pairs.push((key_val, value));
                    }
                }
                // Ensure model section exists
                let model_key = serde_yaml::Value::String("model".to_string());
                if !map.contains_key(&model_key) {
                    map.insert(model_key.clone(), serde_yaml::Value::Mapping(Default::default()));
                }
                let model_map = map.get_mut(&model_key).unwrap();
                if let Some(model_mapping) = model_map.as_mapping_mut() {
                    for (key_val, value) in kv_pairs {
                        if !model_mapping.contains_key(&key_val) {
                            model_mapping.insert(key_val, value);
                        }
                    }
                }
                // Write back
                let yaml = serde_yaml::to_string(&config).unwrap_or_default();
                match std::fs::write(config_path(), yaml) {
                    Ok(()) => { println!("{}", green().apply_to("✓")); fixed_count += 1; }
                    Err(e) => { println!("{} {e}", red().apply_to("✗")); failed.push("migrate stale config keys".to_string()); }
                }
            }
        }
    }

    println!();
    println!("  {} {fixed_count} issue(s) auto-fixed", green().apply_to("✓"));
    if !failed.is_empty() {
        println!("  {} {} issue(s) require manual action:", red().apply_to("✗"), failed.len());
        for f in &failed {
            println!("    - {f}");
        }
    }
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the full doctor diagnostic check.
pub fn cmd_doctor() -> anyhow::Result<()> {
    let mut issues: Vec<String> = Vec::new();

    println!();
    println!("{}", bold().apply_to("┌─────────────────────────────────────────────────────────┐"));
    println!("{}", bold().apply_to("│                 Hermes Doctor                          │"));
    println!("{}", bold().apply_to("└─────────────────────────────────────────────────────────┘"));

    // Run all synchronous checks
    check_system_info(&mut issues);
    check_configuration(&mut issues);
    check_environment(&mut issues);
    check_directory_structure(&mut issues);
    check_session_database(&mut issues);
    check_external_tools(&mut issues);
    check_tools(&mut issues);
    check_skills_hub(&mut issues);
    check_cron_jobs();
    check_gateway();

    // Summary
    println!();
    if issues.is_empty() {
        println!("{}", cyan().apply_to("────────────────────────────────────────────────────"));
        println!("  {} All checks passed!", green().apply_to("✓"));
    } else {
        println!("{}", cyan().apply_to("────────────────────────────────────────────────────"));
        println!("  {} Found {} issue(s) to address:", yellow().apply_to("⚠"), issues.len());
        println!();
        for (i, issue) in issues.iter().enumerate() {
            println!("  {}. {issue}", i + 1);
        }
        println!();
        println!("  {}", dim().apply_to("Tip: run 'hermes doctor --fix' to auto-fix what's possible."));
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_provider_env_config_positive() {
        assert!(has_provider_env_config("OPENROUTER_API_KEY=sk-123"));
        assert!(has_provider_env_config("OPENAI_API_KEY=sk-123"));
        assert!(has_provider_env_config("ANTHROPIC_API_KEY=sk-123"));
        assert!(has_provider_env_config("OPENAI_BASE_URL=https://example.com"));
    }

    #[test]
    fn test_has_provider_env_config_negative() {
        assert!(!has_provider_env_config("# Just a comment\nHERMES_MODEL=opus"));
        assert!(!has_provider_env_config(""));
    }

    #[test]
    fn test_doctor_runs_without_panic() {
        // Should not panic even with no config
        let result = cmd_doctor();
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_hermes_home_from_env() {
        // Verify the path helper works
        let path = get_hermes_home();
        assert!(path.ends_with(".hermes") || path.to_string_lossy().contains(".hermes"));
    }
}
