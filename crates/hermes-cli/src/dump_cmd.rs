#![allow(dead_code)]
//! Dump command for Hermes CLI.
//!
//! Outputs a compact, plain-text summary of the user's Hermes setup
//! that can be copy-pasted into Discord/GitHub/Telegram for support context.
//! No ANSI colors, no checkmarks — just data.
//!
//! Mirrors the Python `hermes_cli/dump.py`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use hermes_core::config::HermesConfig;
use hermes_core::hermes_home::{display_hermes_home, get_hermes_home};

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Return short git commit hash, or "(unknown)".
fn get_git_commit() -> String {
    option_env!("VERGEN_GIT_SHA")
        .map(|s| {
            // Take first 8 chars to match Python's `--short=8`
            let len = s.chars().count().min(8);
            s.chars().take(len).collect()
        })
        .unwrap_or_else(|| "(unknown)".to_string())
}

/// Redact all but first 4 and last 4 chars.
fn redact(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.len() < 12 {
        return "***".to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let first: String = chars[..4].iter().collect();
    let last: String = chars[chars.len() - 4..].iter().collect();
    format!("{first}...{last}")
}

/// Count installed skills by scanning for SKILL.md files.
fn count_skills(hermes_home: &Path) -> usize {
    let skills_dir = hermes_home.join("skills");
    if !skills_dir.is_dir() {
        return 0;
    }
    walk_skills(&skills_dir)
}

fn walk_skills(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == "SKILL.md") {
                count += 1;
            } else if path.is_dir() {
                count += walk_skills(&path);
            }
        }
    }
    count
}

/// Count configured MCP servers.
fn count_mcp_servers(config: &HermesConfig) -> usize {
    config.mcp_servers.len()
}

/// Return cron jobs summary from cron_jobs.json.
fn cron_summary(hermes_home: &Path) -> String {
    let jobs_file = hermes_home.join("cron_jobs.json");
    if !jobs_file.exists() {
        // Also check cron/jobs.json (newer path)
        let alt_file = hermes_home.join("cron").join("jobs.json");
        if alt_file.exists() {
            return read_cron_file(&alt_file);
        }
        return "0".to_string();
    }
    read_cron_file(&jobs_file)
}

fn read_cron_file(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(content) => {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                // New format: {"jobs": [...]}
                if let Some(jobs) = data.get("jobs").and_then(|v| v.as_array()) {
                    let total = jobs.len();
                    let active = jobs
                        .iter()
                        .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))
                        .count();
                    return format!("{active} active / {total} total");
                }
                // Legacy format: plain array
                if let Some(jobs) = data.as_array() {
                    let total = jobs.len();
                    let active = jobs
                        .iter()
                        .filter(|j| j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))
                        .count();
                    return format!("{active} active / {total} total");
                }
            }
            "(error parsing)".to_string()
        }
        Err(_) => "(error reading)".to_string(),
    }
}

/// Return list of configured messaging platform names by checking env vars.
fn configured_platforms() -> Vec<&'static str> {
    let checks: [(&str, &str); 16] = [
        ("telegram", "TELEGRAM_BOT_TOKEN"),
        ("discord", "DISCORD_BOT_TOKEN"),
        ("slack", "SLACK_BOT_TOKEN"),
        ("whatsapp", "WHATSAPP_ENABLED"),
        ("signal", "SIGNAL_HTTP_URL"),
        ("email", "EMAIL_ADDRESS"),
        ("sms", "TWILIO_ACCOUNT_SID"),
        ("matrix", "MATRIX_HOMESERVER_URL"),
        ("mattermost", "MATTERMOST_URL"),
        ("homeassistant", "HASS_TOKEN"),
        ("dingtalk", "DINGTALK_CLIENT_ID"),
        ("feishu", "FEISHU_APP_ID"),
        ("wecom", "WECOM_BOT_ID"),
        ("wecom_callback", "WECOM_CALLBACK_CORP_ID"),
        ("weixin", "WEIXIN_ACCOUNT_ID"),
        ("qqbot", "QQ_APP_ID"),
    ];
    checks
        .iter()
        .filter_map(|&(name, env)| std::env::var(env).ok().map(|_| name))
        .collect()
}

/// Return the active memory provider name.
fn memory_provider(config: &HermesConfig) -> &str {
    config.memory.backend.as_deref().unwrap_or("built-in")
}

/// Extract model and provider from config.
fn get_model_and_provider(config: &HermesConfig) -> (&str, &str) {
    let model = config.model.name.as_deref().unwrap_or("(not set)");
    let provider = config
        .model
        .provider
        .as_deref()
        .unwrap_or("(auto)");
    (model, provider)
}

/// Find non-default config values worth reporting.
///
/// Returns a flat HashMap of dotpath -> value for interesting overrides.
fn config_overrides(config: &HermesConfig) -> HashMap<String, String> {
    let defaults = HermesConfig::default();
    let mut overrides = HashMap::new();

    // Terminal overrides
    if config.terminal.backend != defaults.terminal.backend {
        overrides.insert("terminal.backend".to_string(), config.terminal.backend.clone());
    }
    if config.terminal.docker_image != defaults.terminal.docker_image {
        if let Some(v) = &config.terminal.docker_image {
            overrides.insert("terminal.docker_image".to_string(), v.clone());
        }
    }

    // Compression overrides
    if config.compression.enabled != defaults.compression.enabled {
        overrides.insert("compression.enabled".to_string(), config.compression.enabled.to_string());
    }
    if config.compression.target_tokens != defaults.compression.target_tokens {
        if let Some(v) = config.compression.target_tokens {
            overrides.insert("compression.threshold".to_string(), v.to_string());
        }
    }

    // Browser
    if config.browser.provider != defaults.browser.provider {
        if let Some(v) = &config.browser.provider {
            overrides.insert("browser.provider".to_string(), v.clone());
        }
    }

    // MCP servers
    if !config.mcp_servers.is_empty() {
        overrides.insert("mcp_servers".to_string(), format!("{} server(s)", config.mcp_servers.len()));
    }

    // Fallback providers
    if !config.fallback_providers.is_empty() {
        overrides.insert("fallback_providers".to_string(), config.fallback_providers.join(", "));
    }

    // Disabled tools
    if !config.disabled_tools.is_empty() {
        overrides.insert("disabled_tools".to_string(), config.disabled_tools.join(", "));
    }

    // Skills disabled
    if !config.skills.disabled.is_empty() {
        overrides.insert("skills.disabled".to_string(), config.skills.disabled.join(", "));
    }

    overrides
}

/// Get the active profile name.
fn get_active_profile_name() -> String {
    // Check HERMES_PROFILE env var or detect from HERMES_HOME path
    if let Ok(profile) = std::env::var("HERMES_PROFILE") {
        if !profile.is_empty() {
            return profile;
        }
    }
    // If HERMES_HOME contains "/profiles/", extract the profile name
    if let Ok(home) = std::env::var("HERMES_HOME") {
        if let Some(pos) = home.find("/profiles/") {
            let rest = &home[pos + 10..];
            if let Some(slash) = rest.find('/') {
                return rest[..slash].to_string();
            }
            return rest.to_string();
        }
    }
    "(default)".to_string()
}

/// Get OS info string.
fn os_info() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

// ---------------------------------------------------------------------------
// Main dump command
// ---------------------------------------------------------------------------

/// Output a compact, copy-pasteable setup summary.
pub fn cmd_dump(show_keys: bool) -> anyhow::Result<()> {
    let hermes_home = get_hermes_home();
    let commit = get_git_commit();

    // Load config (falls back to defaults)
    let config = HermesConfig::load().unwrap_or_default();

    let (model, provider) = get_model_and_provider(&config);
    let profile = get_active_profile_name();
    let backend = &config.terminal.backend;

    // Build version string
    let version = env!("CARGO_PKG_VERSION");
    let ver_str = format!("{version} [{commit}]");

    let mut lines: Vec<String> = Vec::new();
    lines.push("--- hermes dump ---".to_string());
    lines.push(format!("version:          {ver_str}"));
    lines.push(format!("os:               {}", os_info()));
    lines.push(format!("rust:             {}", option_env!("RUSTC_VERSION").unwrap_or("stable")));
    lines.push(format!("profile:          {profile}"));
    lines.push(format!("hermes_home:      {}", display_hermes_home()));
    lines.push(format!("model:            {model}"));
    lines.push(format!("provider:         {provider}"));
    lines.push(format!("terminal:         {backend}"));

    // API keys
    lines.push(String::new());
    lines.push("api_keys:".to_string());
    let api_keys: [(&str, &str); 20] = [
        ("OPENROUTER_API_KEY", "openrouter"),
        ("OPENAI_API_KEY", "openai"),
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("ANTHROPIC_TOKEN", "anthropic_token"),
        ("NOUS_API_KEY", "nous"),
        ("GLM_API_KEY", "glm/zai"),
        ("ZAI_API_KEY", "zai"),
        ("KIMI_API_KEY", "kimi"),
        ("MINIMAX_API_KEY", "minimax"),
        ("DEEPSEEK_API_KEY", "deepseek"),
        ("DASHSCOPE_API_KEY", "dashscope"),
        ("HF_TOKEN", "huggingface"),
        ("AI_GATEWAY_API_KEY", "ai_gateway"),
        ("FIRECRAWL_API_KEY", "firecrawl"),
        ("TAVILY_API_KEY", "tavily"),
        ("BROWSERBASE_API_KEY", "browserbase"),
        ("FAL_KEY", "fal"),
        ("ELEVENLABS_API_KEY", "elevenlabs"),
        ("GITHUB_TOKEN", "github"),
        ("OPENCODE_ZEN_API_KEY", "opencode_zen"),
    ];

    for (env_var, label) in &api_keys {
        let val = std::env::var(env_var).unwrap_or_default();
        let display = if !val.is_empty() {
            if show_keys {
                redact(&val)
            } else {
                "set".to_string()
            }
        } else {
            "not set".to_string()
        };
        lines.push(format!("  {label:<20} {display}"));
    }

    // Features summary
    lines.push(String::new());
    lines.push("features:".to_string());

    // Toolsets — not yet tracked in Rust config, show placeholder
    lines.push("  toolsets:           (default)".to_string());
    lines.push(format!("  mcp_servers:        {}", count_mcp_servers(&config)));
    lines.push(format!("  memory_provider:    {}", memory_provider(&config)));
    lines.push(format!("  platforms:          {}", {
        let platforms = configured_platforms();
        if platforms.is_empty() {
            "none".to_string()
        } else {
            platforms.join(", ")
        }
    }));
    lines.push(format!("  cron_jobs:          {}", cron_summary(&hermes_home)));
    lines.push(format!("  skills:             {}", count_skills(&hermes_home)));

    // Config overrides (non-default values)
    let overrides = config_overrides(&config);
    if !overrides.is_empty() {
        lines.push(String::new());
        lines.push("config_overrides:".to_string());
        let mut sorted: Vec<_> = overrides.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, val) in sorted {
            lines.push(format!("  {key}: {val}"));
        }
    }

    lines.push("--- end dump ---".to_string());

    // Output with no ANSI colors — just plain text
    let output = lines.join("\n");
    println!("{output}");

    Ok(())
}
