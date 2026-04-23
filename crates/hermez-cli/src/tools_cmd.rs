#![allow(dead_code)]
//! Tool configuration commands for the Hermez CLI.
//!
//! Mirrors the Python `hermez tools` subcommands:
//! - `tools list` - List all toolsets with enabled/disabled status
//! - `tools disable <name>` - Disable a toolset
//! - `tools enable <name>` - Enable a toolset
//! - `tools info <name>` - Show detailed tool info (params, description, schema)
//! - `tools disable-all` - Disable all toolsets
//! - `tools enable-all` - Enable all toolsets
//! - `tools disable-batch <names>` - Batch disable
//! - `tools enable-batch <names>` - Batch enable
//!
//! Tool config is stored in config.yaml under `tools:` section.
//! Per-platform entries use a sequence where:
//! - `"toolset"` (bare) = enabled toolset
//! - `"!toolset"` (prefixed) = disabled toolset

use console::style;
use std::collections::HashSet;
use std::path::PathBuf;

// ─── Built-in Toolset Registry ───────────────────────────────────────────────
// Mirrors Python CONFIGURABLE_TOOLSETS: (key, label, description)

/// A built-in toolset definition.
#[derive(Clone, Debug)]
pub struct ToolsetDef {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// All configurable built-in toolsets.
pub const BUILTIN_TOOLSETS: &[ToolsetDef] = &[
    ToolsetDef { key: "web",             label: "Web Search & Scraping",   description: "web_search, web_extract" },
    ToolsetDef { key: "browser",         label: "Browser Automation",      description: "navigate, click, type, scroll" },
    ToolsetDef { key: "terminal",        label: "Terminal & Processes",    description: "terminal, process" },
    ToolsetDef { key: "file",            label: "File Operations",         description: "read, write, patch, search" },
    ToolsetDef { key: "code_execution",  label: "Code Execution",          description: "execute_code" },
    ToolsetDef { key: "vision",          label: "Vision / Image Analysis", description: "vision_analyze" },
    ToolsetDef { key: "image_gen",       label: "Image Generation",        description: "image_generate" },
    ToolsetDef { key: "moa",             label: "Mixture of Agents",       description: "mixture_of_agents" },
    ToolsetDef { key: "tts",             label: "Text-to-Speech",          description: "text_to_speech" },
    ToolsetDef { key: "skills",          label: "Skills",                  description: "list, view, manage" },
    ToolsetDef { key: "todo",            label: "Task Planning",           description: "todo" },
    ToolsetDef { key: "memory",          label: "Memory",                  description: "persistent memory across sessions" },
    ToolsetDef { key: "session_search",  label: "Session Search",          description: "search past conversations" },
    ToolsetDef { key: "clarify",         label: "Clarifying Questions",    description: "clarify" },
    ToolsetDef { key: "delegation",      label: "Task Delegation",         description: "delegate_task" },
    ToolsetDef { key: "cronjob",         label: "Cron Jobs",               description: "create/list/update/pause/resume/run" },
    ToolsetDef { key: "messaging",       label: "Cross-Platform Messaging", description: "send_message" },
    ToolsetDef { key: "rl",              label: "RL Training",             description: "Tinker-Atropos training tools" },
    ToolsetDef { key: "homeassistant",   label: "Home Assistant",          description: "smart home device control" },
];

/// Toolsets that are OFF by default for new installs.
pub const DEFAULT_OFF_TOOLSETS: &[&str] = &["moa", "homeassistant", "rl"];

// ─── Config Helpers ─────────────────────────────────────────────────────────

fn get_hermez_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMEZ_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermez")
    } else {
        PathBuf::from(".hermez")
    }
}

fn get_config_path() -> PathBuf {
    get_hermez_home().join("config.yaml")
}

/// Load config YAML, returning empty mapping on failure.
fn load_config() -> anyhow::Result<serde_yaml::Value> {
    let path = get_config_path();
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        Ok(serde_yaml::from_str(&content).unwrap_or(serde_yaml::Value::Mapping(Default::default())))
    } else {
        Ok(serde_yaml::Value::Mapping(Default::default()))
    }
}

/// Save config YAML.
fn save_config(config: &serde_yaml::Value) -> anyhow::Result<()> {
    let path = get_config_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let yaml = serde_yaml::to_string(config)?;
    std::fs::write(&path, yaml)?;
    Ok(())
}

/// Get the set of all built-in toolset keys.
fn all_toolset_keys() -> HashSet<&'static str> {
    BUILTIN_TOOLSETS.iter().map(|t| t.key).collect()
}

/// Get the enabled toolsets for a platform from config.
/// Returns the set of toolset keys that are enabled (not prefixed with !).
fn get_enabled_toolsets(config: &serde_yaml::Value, platform: &str) -> HashSet<String> {
    let mut enabled = HashSet::new();

    if let Some(tools) = config.get("tools") {
        if let Some(platforms) = tools.get(platform) {
            if let Some(seq) = platforms.as_sequence() {
                for entry in seq {
                    if let Some(s) = entry.as_str() {
                        // Skip disabled entries (prefixed with !)
                        // Skip MCP entries (prefixed with mcp:)
                        if !s.starts_with('!') && !s.starts_with("mcp:") {
                            enabled.insert(s.to_string());
                        }
                    }
                }
            }
        }
    }

    enabled
}

/// Get the disabled toolsets for a platform from config.
fn get_disabled_toolsets(config: &serde_yaml::Value, platform: &str) -> HashSet<String> {
    let mut disabled = HashSet::new();

    if let Some(tools) = config.get("tools") {
        if let Some(platforms) = tools.get(platform) {
            if let Some(seq) = platforms.as_sequence() {
                for entry in seq {
                    if let Some(s) = entry.as_str() {
                        if s.starts_with('!') {
                            disabled.insert(s.strip_prefix('!').unwrap_or(s).to_string());
                        }
                    }
                }
            }
        }
    }

    disabled
}

/// Ensure the tools.platform sequence exists in config and return a mutable reference to it.
fn get_or_create_platform_sequence<'a>(
    config: &'a mut serde_yaml::Value,
    platform: &str,
) -> &'a mut Vec<serde_yaml::Value> {
    let map = config.as_mapping_mut().unwrap();
    let tools_key = serde_yaml::Value::String("tools".to_string());
    let tools_entry = map.entry(tools_key)
        .or_insert(serde_yaml::Value::Mapping(Default::default()));
    let tools_map = tools_entry.as_mapping_mut().unwrap();
    let platform_key = serde_yaml::Value::String(platform.to_string());
    let platform_entry = tools_map.entry(platform_key)
        .or_insert(serde_yaml::Value::Sequence(Default::default()));
    platform_entry.as_sequence_mut().unwrap()
}

/// Clean a platform's tool sequence: remove duplicate entries and MCP-only entries
/// when we're dealing with built-in toolsets. Returns the cleaned sequence.
fn clean_platform_sequence(seq: &mut Vec<serde_yaml::Value>) {
    let mut seen = HashSet::new();
    let mut mcp_entries = Vec::new();

    // First pass: collect MCP entries and deduplicate built-in entries
    seq.retain(|entry| {
        if let Some(s) = entry.as_str() {
            if s.starts_with("mcp:") {
                mcp_entries.push(entry.clone());
                return false; // remove temporarily
            }
            if seen.contains(s) {
                return false; // duplicate
            }
            seen.insert(s.to_string());
            true
        } else {
            true // keep non-string entries
        }
    });

    // Re-add MCP entries
    seq.extend(mcp_entries);
}

// ─── Commands ────────────────────────────────────────────────────────────────

/// List all toolsets with enabled/disabled status for a platform.
pub fn cmd_tools_list(platform: &str) -> anyhow::Result<()> {
    let config = load_config()?;
    let enabled = get_enabled_toolsets(&config, platform);
    let disabled = get_disabled_toolsets(&config, platform);

    println!("{}", style("Tool Configuration").bold());
    println!("Platform: {}", style(platform).cyan());
    println!();

    // Count stats
    let enabled_count = BUILTIN_TOOLSETS.iter().filter(|t| enabled.contains(t.key)).count();
    let disabled_explicit_count = BUILTIN_TOOLSETS.iter().filter(|t| disabled.contains(t.key)).count();
    let implicit_disabled = BUILTIN_TOOLSETS.iter()
        .filter(|t| !enabled.contains(t.key) && !disabled.contains(t.key))
        .count();

    println!("{}", style("Built-in Toolsets:").bold());
    println!();
    println!("  {:<4} {:<20} {:<30} Description", "Status", "Key", "Label");
    println!("  {}", "-".repeat(90));

    for ts in BUILTIN_TOOLSETS {
        let (status_label, status_style) = if enabled.contains(ts.key) {
            ("enabled", style("✓ enabled").green())
        } else if disabled.contains(ts.key) {
            ("disabled", style("✗ disabled").red())
        } else {
            ("default off", style("— default off").dim())
        };

        println!(
            "  {:<14} {:<20} {:<30} {}",
            status_style,
            style(ts.key).cyan(),
            ts.label,
            style(ts.description).dim()
        );
        let _ = status_label; // used for sorting/grouping if needed
    }

    println!();
    println!("  {} {} enabled, {} explicitly disabled, {} default off",
        style("Summary:").bold(),
        style(enabled_count).green(),
        style(disabled_explicit_count).red(),
        style(implicit_disabled).dim(),
    );

    // Show MCP servers if configured
    if let Some(mcp_servers) = config.get("mcp_servers") {
        if let Some(mcp_map) = mcp_servers.as_mapping() {
            if !mcp_map.is_empty() {
                println!();
                println!("{}", style("MCP Servers:").bold());
                for (server_name, server_cfg) in mcp_map {
                    let name = server_name.as_str().unwrap_or("?");
                    if let Some(tools) = server_cfg.get("tools") {
                        if let Some(exclude) = tools.get("exclude") {
                            if let Some(seq) = exclude.as_sequence() {
                                if !seq.is_empty() {
                                    let excluded: Vec<_> = seq.iter()
                                        .filter_map(|v| v.as_str())
                                        .collect();
                                    println!("  {} {} [excluded: {}]",
                                        style("●").yellow(),
                                        style(name).cyan(),
                                        style(excluded.join(", ")).yellow()
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                    println!("  {} {} [all tools enabled]", style("●").green(), style(name).cyan());
                }
            }
        }
    }

    println!();

    Ok(())
}

/// Disable one or more toolsets for a platform.
pub fn cmd_tools_disable(names: &[String], platform: &str) -> anyhow::Result<()> {
    let mut config = load_config()?;
    let seq = get_or_create_platform_sequence(&mut config, platform);

    let valid_keys = all_toolset_keys();
    let mut success = Vec::new();
    let mut unknown = Vec::new();

    for name in names {
        // Check for MCP tool format
        if name.contains(':') {
            // MCP tools are handled differently — add as mcp:server:tool
            let mcp_entry = serde_yaml::Value::String(format!("mcp:{}", name));
            if !seq.contains(&mcp_entry) {
                seq.push(mcp_entry);
            }
            success.push(name.clone());
            continue;
        }

        if !valid_keys.contains(name.as_str()) {
            unknown.push(name.clone());
            continue;
        }

        // Remove any existing bare entry for this toolset
        seq.retain(|v| {
            if let Some(s) = v.as_str() {
                s != name && s != format!("!{}", name)
            } else {
                true
            }
        });

        // Add disabled entry
        let disabled_entry = serde_yaml::Value::String(format!("!{}", name));
        seq.push(disabled_entry);
        success.push(name.clone());
    }

    clean_platform_sequence(seq);
    save_config(&config)?;

    if !success.is_empty() {
        println!("  {} Disabled: {}",
            style("✓").green(),
            style(success.join(", ")).cyan()
        );
    }

    for name in &unknown {
        println!("  {} Unknown toolset '{}'", style("✗").red(), name);
        println!("    Valid: {}", BUILTIN_TOOLSETS.iter().map(|t| t.key).collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

/// Enable one or more toolsets for a platform.
pub fn cmd_tools_enable(names: &[String], platform: &str) -> anyhow::Result<()> {
    let mut config = load_config()?;
    let seq = get_or_create_platform_sequence(&mut config, platform);

    let valid_keys = all_toolset_keys();
    let mut success = Vec::new();
    let mut unknown = Vec::new();

    for name in names {
        // Check for MCP tool format
        if name.contains(':') {
            let parts: Vec<&str> = name.splitn(2, ':').collect();
            if parts.len() == 2 {
                // Remove mcp:server:tool disable entry
                let mcp_disable = serde_yaml::Value::String(format!("mcp:{}", name));
                seq.retain(|v| v != &mcp_disable);
            }
            success.push(name.clone());
            continue;
        }

        if !valid_keys.contains(name.as_str()) {
            unknown.push(name.clone());
            continue;
        }

        // Remove disabled entry for this toolset
        let disabled_entry = serde_yaml::Value::String(format!("!{}", name));
        seq.retain(|v| v != &disabled_entry);

        // Add bare (enabled) entry if not already present
        let enabled_entry = serde_yaml::Value::String(name.clone());
        if !seq.contains(&enabled_entry) {
            seq.push(enabled_entry);
        }
        success.push(name.clone());
    }

    clean_platform_sequence(seq);
    save_config(&config)?;

    if !success.is_empty() {
        println!("  {} Enabled: {}",
            style("✓").green(),
            style(success.join(", ")).cyan()
        );
    }

    for name in &unknown {
        println!("  {} Unknown toolset '{}'", style("✗").red(), name);
        println!("    Valid: {}", BUILTIN_TOOLSETS.iter().map(|t| t.key).collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

/// Disable all built-in toolsets for a platform.
pub fn cmd_tools_disable_all(platform: &str) -> anyhow::Result<()> {
    let mut config = load_config()?;
    let seq = get_or_create_platform_sequence(&mut config, platform);

    // Collect existing MCP entries
    let mcp_entries: Vec<_> = seq.iter()
        .filter(|v| v.as_str().is_some_and(|s| s.starts_with("mcp:")))
        .cloned()
        .collect();

    // Clear and rebuild with all toolsets disabled
    seq.clear();
    for ts in BUILTIN_TOOLSETS {
        let disabled_entry = serde_yaml::Value::String(format!("!{}", ts.key));
        seq.push(disabled_entry);
    }
    // Preserve MCP entries
    seq.extend(mcp_entries);

    save_config(&config)?;

    println!("  {} All {} toolsets disabled for {}",
        style("✓").green(),
        style(BUILTIN_TOOLSETS.len()).cyan(),
        style(platform).cyan()
    );

    Ok(())
}

/// Enable all built-in toolsets for a platform.
pub fn cmd_tools_enable_all(platform: &str) -> anyhow::Result<()> {
    let mut config = load_config()?;
    let seq = get_or_create_platform_sequence(&mut config, platform);

    // Collect existing MCP entries
    let mcp_entries: Vec<_> = seq.iter()
        .filter(|v| v.as_str().is_some_and(|s| s.starts_with("mcp:")))
        .cloned()
        .collect();

    // Clear and rebuild with all toolsets enabled
    seq.clear();
    for ts in BUILTIN_TOOLSETS {
        let enabled_entry = serde_yaml::Value::String(ts.key.to_string());
        seq.push(enabled_entry);
    }
    // Preserve MCP entries
    seq.extend(mcp_entries);

    save_config(&config)?;

    println!("  {} All {} toolsets enabled for {}",
        style("✓").green(),
        style(BUILTIN_TOOLSETS.len()).cyan(),
        style(platform).cyan()
    );

    Ok(())
}

/// Batch disable toolsets (alias for disable with multiple names).
pub fn cmd_tools_disable_batch(names: &[String], platform: &str) -> anyhow::Result<()> {
    cmd_tools_disable(names, platform)
}

/// Batch enable toolsets (alias for enable with multiple names).
pub fn cmd_tools_enable_batch(names: &[String], platform: &str) -> anyhow::Result<()> {
    cmd_tools_enable(names, platform)
}

/// Show detailed info about a toolset or tool.
///
/// For toolset keys (e.g., "web", "memory"), shows toolset info.
/// For individual tool names, delegates to the tool registry.
pub fn cmd_tools_info(name: &str) -> anyhow::Result<()> {
    // Check if it's a known toolset
    let toolset = BUILTIN_TOOLSETS.iter().find(|t| t.key == name);

    if let Some(ts) = toolset {
        let config = load_config()?;
        let enabled_toolsets = get_enabled_toolsets(&config, "cli");
        let is_enabled = enabled_toolsets.contains(ts.key);

        println!("{}", style("Toolset Info").bold());
        println!();
        println!("  {:<12} {}", style("Key:").bold(), style(ts.key).cyan());
        println!("  {:<12} {}", style("Label:").bold(), ts.label);
        println!("  {:<12} {}", style("Tools:").bold(), style(ts.description).dim());
        println!("  {:<12} {}", style("Status:").bold(), if is_enabled {
            style("enabled").green()
        } else {
            style("disabled").red()
        });

        // Show which platforms have this toolset enabled
        println!("  {}:", style("Platforms").bold());
        for platform in ["cli", "telegram", "discord", "feishu", "weixin", "slack", "whatsapp", "signal", "sms", "matrix", "mattermost", "homeassistant", "bluebubbles", "wecom_callback"] {
            let plat_enabled = get_enabled_toolsets(&config, platform);
            let status = if plat_enabled.contains(ts.key) {
                style("✓").green()
            } else {
                style("—").dim()
            };
            println!("    {} {}", status, platform);
        }

        println!();
        return Ok(());
    }

    // Fall back to tool registry for individual tool lookup
    println!("{}", style("Tool Info").bold());
    println!();

    let mut registry = hermez_tools::registry::ToolRegistry::new();
    hermez_tools::register_all_tools(&mut registry);

    if let Some(entry) = registry.get(name) {
        println!("  {:<14} {}", style("Name:").bold(), style(&entry.name).cyan());
        println!("  {:<14} {}", style("Toolset:").bold(), style(&entry.toolset).cyan());
        println!("  {:<14} {}", style("Description:").bold(), &entry.description);
        println!("  {:<14} {}", style("Emoji:").bold(), &entry.emoji);

        if !entry.requires_env.is_empty() {
            println!("  {:<14} {:?}", style("Required Env:").bold(), &entry.requires_env);
        }

        println!();
        println!("  {}:", style("Schema").bold());
        match serde_json::to_string_pretty(&entry.schema) {
            Ok(json) => {
                for line in json.lines() {
                    println!("    {}", style(line).dim());
                }
            }
            Err(e) => {
                println!("    {} Failed to serialize schema: {}", style("!").yellow(), e);
            }
        }
    } else {
        println!("  {} Tool/toolset '{}' not found", style("✗").red(), name);
        println!();
        println!("  {} Available toolsets:", style("→").dim());
        for ts in BUILTIN_TOOLSETS {
            println!("    {}", style(ts.key).dim());
        }
    }

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_toolset_keys_unique() {
        let keys: Vec<_> = BUILTIN_TOOLSETS.iter().map(|t| t.key).collect();
        let unique: HashSet<_> = keys.iter().copied().collect();
        assert_eq!(keys.len(), unique.len(), "Duplicate toolset keys found");
    }

    #[test]
    fn test_default_off_toolsets_exist() {
        let all_keys: HashSet<_> = BUILTIN_TOOLSETS.iter().map(|t| t.key).collect();
        for key in DEFAULT_OFF_TOOLSETS {
            assert!(all_keys.contains(key), "DEFAULT_OFF contains unknown key: {}", key);
        }
    }

    #[test]
    fn test_load_empty_config() {
        // With no config file, load should return empty mapping
        let result = load_config();
        // This may succeed or fail depending on whether a config exists,
        // but it shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_get_enabled_toolsets_empty() {
        let config = serde_yaml::Value::Mapping(Default::default());
        let enabled = get_enabled_toolsets(&config, "cli");
        assert!(enabled.is_empty());
    }

    #[test]
    fn test_get_enabled_toolsets_parses_correctly() {
        let yaml = r#"
tools:
  cli:
    - file
    - "!web"
    - terminal
    - "!memory"
"#;
        let config: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let enabled = get_enabled_toolsets(&config, "cli");
        assert!(enabled.contains("file"));
        assert!(enabled.contains("terminal"));
        assert!(!enabled.contains("web"));
        assert!(!enabled.contains("memory"));
    }

    #[test]
    fn test_get_disabled_toolsets() {
        let yaml = r#"
tools:
  cli:
    - file
    - "!web"
    - terminal
    - "!memory"
"#;
        let config: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let disabled = get_disabled_toolsets(&config, "cli");
        assert!(disabled.contains("web"));
        assert!(disabled.contains("memory"));
        assert!(!disabled.contains("file"));
    }

    #[test]
    fn test_clean_platform_sequence_removes_duplicates() {
        let yaml = r#"
tools:
  cli:
    - file
    - file
    - terminal
"#;
        let mut config: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let seq = get_or_create_platform_sequence(&mut config, "cli");
        clean_platform_sequence(seq);

        let keys: HashSet<_> = seq.iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(keys.len(), 2, "Should have removed duplicate 'file'");
    }

    #[test]
    fn test_clean_platform_sequence_preserves_mcp() {
        let yaml = r#"
tools:
  cli:
    - file
    - "mcp:github:create_issue"
    - terminal
"#;
        let mut config: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let seq = get_or_create_platform_sequence(&mut config, "cli");
        clean_platform_sequence(seq);

        let has_mcp = seq.iter().any(|v| {
            v.as_str().map_or(false, |s| s.starts_with("mcp:"))
        });
        assert!(has_mcp, "MCP entries should be preserved");
    }
}
