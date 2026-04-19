#![allow(dead_code)]
//! Plugin management command.

use console::Style;
use std::path::PathBuf;

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

fn green() -> Style { Style::new().green() }
fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }
fn yellow() -> Style { Style::new().yellow() }

/// Plugin metadata.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct PluginInfo {
    name: String,
    source: String,
    enabled: bool,
    installed_at: String,
    version: Option<String>,
}

fn plugins_dir() -> PathBuf {
    get_hermes_home().join("plugins")
}

fn plugin_registry() -> PathBuf {
    get_hermes_home().join(".plugin_registry.json")
}

fn load_registry() -> Vec<PluginInfo> {
    let path = plugin_registry();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(reg) = serde_json::from_str::<Vec<PluginInfo>>(&content) {
                return reg;
            }
        }
    }
    Vec::new()
}

fn save_registry(reg: &[PluginInfo]) -> anyhow::Result<()> {
    let path = plugin_registry();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(reg)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Install a plugin from a Git URL or owner/repo shorthand.
pub fn cmd_plugins_install(identifier: &str, force: bool) -> anyhow::Result<()> {
    let dir = plugins_dir();
    std::fs::create_dir_all(&dir)?;

    // Extract plugin name from identifier
    let name = identifier
        .split('/')
        .next_back()
        .unwrap_or(identifier)
        .trim_end_matches(".git");

    let plugin_path = dir.join(name);

    // Check if already installed
    if plugin_path.exists() {
        if force {
            // Remove and reinstall
            std::fs::remove_dir_all(&plugin_path)?;
            println!("  {} Removed existing plugin: {}", yellow().apply_to("○"), name);
        } else {
            println!("  {} Plugin already installed: {}", yellow().apply_to("⚠"), name);
            println!("  Use --force to reinstall");
            return Ok(());
        }
    }

    // Build git URL
    let git_url = if identifier.starts_with("http") || identifier.starts_with("git@") {
        identifier.to_string()
    } else {
        format!("https://github.com/{}.git", identifier)
    };

    println!("  Installing plugin from {}...", git_url);

    // Clone the repository
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", &git_url])
        .arg(&plugin_path)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // Add to registry
            let mut reg = load_registry();
            reg.retain(|p| p.name != name);
            reg.push(PluginInfo {
                name: name.to_string(),
                source: git_url.clone(),
                enabled: true,
                installed_at: chrono::Local::now().to_rfc3339(),
                version: None,
            });
            save_registry(&reg)?;

            println!("  {} Plugin installed: {}", green().apply_to("✓"), name);
            println!("    Source: {}", git_url);
            println!("    Path: {}", plugin_path.display());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            println!("  {} Failed to clone: {}", yellow().apply_to("✗"), stderr.trim());
            println!("  If the plugin exists locally, copy it to: {}", plugin_path.display());
        }
        Err(e) => {
            println!("  {} Git not available: {}", yellow().apply_to("✗"), e);
            println!("  Create the plugin directory manually at: {}", plugin_path.display());
        }
    }

    Ok(())
}

/// Update a plugin.
pub fn cmd_plugins_update(name: &str) -> anyhow::Result<()> {
    let plugin_path = plugins_dir().join(name);
    if !plugin_path.exists() {
        println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
        return Ok(());
    }

    let output = std::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&plugin_path)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            println!("  {} Plugin updated: {}", green().apply_to("✓"), name);
        }
        Ok(_) => {
            println!("  {} Update failed (not a git repo or conflicts)", yellow().apply_to("✗"));
        }
        Err(_) => {
            println!("  {} Git not available", yellow().apply_to("✗"));
        }
    }

    Ok(())
}

/// Remove a plugin.
pub fn cmd_plugins_remove(name: &str) -> anyhow::Result<()> {
    let plugin_path = plugins_dir().join(name);
    if plugin_path.exists() {
        std::fs::remove_dir_all(&plugin_path)?;
        println!("  {} Plugin removed: {}", green().apply_to("✓"), name);
    } else {
        println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
    }

    // Remove from registry
    let mut reg = load_registry();
    let before = reg.len();
    reg.retain(|p| p.name != name);
    if reg.len() < before {
        save_registry(&reg)?;
    }

    Ok(())
}

/// List installed plugins.
pub fn cmd_plugins_list() -> anyhow::Result<()> {
    let reg = load_registry();

    // Also check for plugins in the directory
    let dir = plugins_dir();
    let dir_plugins: Vec<String> = if dir.exists() {
        std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    } else {
        Vec::new()
    };

    println!();
    println!("{}", cyan().apply_to("◆ Installed Plugins"));
    println!();

    if reg.is_empty() && dir_plugins.is_empty() {
        println!("  {}", dim().apply_to("No plugins installed."));
        println!("  Install one with: hermes plugins install <owner/repo>");
    } else {
        // Merge registry and directory info
        let mut seen = std::collections::HashSet::new();
        for plugin in &reg {
            seen.insert(&plugin.name);
            let status = if plugin.enabled {
                green().apply_to("enabled").to_string()
            } else {
                yellow().apply_to("disabled").to_string()
            };
            println!("  {} — {} ({})", plugin.name, status, plugin.source);
        }
        // Show directory plugins not in registry
        for name in &dir_plugins {
            if !seen.contains(name) {
                println!("  {} — {} (untracked)", name, dim().apply_to("local"));
            }
        }
    }
    println!();

    Ok(())
}

/// Enable a disabled plugin.
pub fn cmd_plugins_enable(name: &str) -> anyhow::Result<()> {
    let mut reg = load_registry();
    for plugin in &mut reg {
        if plugin.name == name {
            plugin.enabled = true;
            save_registry(&reg)?;
            println!("  {} Plugin enabled: {}", green().apply_to("✓"), name);
            return Ok(());
        }
    }

    // Check if exists in directory
    if plugins_dir().join(name).exists() {
        reg.push(PluginInfo {
            name: name.to_string(),
            source: "local".to_string(),
            enabled: true,
            installed_at: chrono::Local::now().to_rfc3339(),
            version: None,
        });
        save_registry(&reg)?;
        println!("  {} Plugin enabled: {}", green().apply_to("✓"), name);
    } else {
        println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
    }

    Ok(())
}

/// Disable a plugin.
pub fn cmd_plugins_disable(name: &str) -> anyhow::Result<()> {
    let mut reg = load_registry();
    for plugin in &mut reg {
        if plugin.name == name {
            plugin.enabled = false;
            save_registry(&reg)?;
            println!("  {} Plugin disabled: {}", green().apply_to("✓"), name);
            return Ok(());
        }
    }
    println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
    Ok(())
}

/// Dispatch plugin subcommands.
pub fn cmd_plugins(
    action: &str,
    identifier: Option<&str>,
    name: Option<&str>,
    force: bool,
) -> anyhow::Result<()> {
    match action {
        "install" => {
            let id = identifier.ok_or_else(|| anyhow::anyhow!("identifier is required"))?;
            cmd_plugins_install(id, force)
        }
        "update" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_update(n)
        }
        "remove" | "rm" | "uninstall" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_remove(n)
        }
        "list" | "ls" | "" => cmd_plugins_list(),
        "enable" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_enable(n)
        }
        "disable" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_disable(n)
        }
        _ => {
            anyhow::bail!("Unknown action: {}. Use install, update, remove, list, enable, or disable.", action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_empty_registry() {
        let reg = load_registry();
        assert!(reg.is_empty());
    }

    #[test]
    fn test_plugin_enable_disable() {
        // Test registry operations directly
        let mut reg = Vec::new();
        reg.push(PluginInfo {
            name: "test_plugin".to_string(),
            source: "local".to_string(),
            enabled: true,
            installed_at: "".to_string(),
            version: None,
        });

        // Disable
        for plugin in &mut reg {
            if plugin.name == "test_plugin" {
                plugin.enabled = false;
                break;
            }
        }
        assert!(!reg.iter().find(|p| p.name == "test_plugin").unwrap().enabled);

        // Re-enable
        for plugin in &mut reg {
            if plugin.name == "test_plugin" {
                plugin.enabled = true;
                break;
            }
        }
        assert!(reg.iter().find(|p| p.name == "test_plugin").unwrap().enabled);
    }
}
