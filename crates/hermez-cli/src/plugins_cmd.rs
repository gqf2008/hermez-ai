#![allow(dead_code)]
//! Plugin management command.
//!
//! Enhanced with manifest parsing, hook inspection, and runtime info.

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
    get_hermez_home().join("plugins")
}

fn plugin_registry() -> PathBuf {
    get_hermez_home().join(".plugin_registry.json")
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

// ---------------------------------------------------------------------------
// Manifest parsing
// ---------------------------------------------------------------------------

/// Read plugin.yaml from a plugin directory.
fn read_manifest(plugin_dir: &std::path::Path) -> Option<hermez_agent_engine::plugin_system::PluginManifest> {
    hermez_agent_engine::plugin_system::PluginManifest::from_dir(plugin_dir)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

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
            // Parse manifest
            let manifest = read_manifest(&plugin_path);
            let version = manifest.as_ref().map(|m| m.version.clone()).filter(|v| !v.is_empty());

            // Add to registry
            let mut reg = load_registry();
            reg.retain(|p| p.name != name);
            reg.push(PluginInfo {
                name: name.to_string(),
                source: git_url.clone(),
                enabled: true,
                installed_at: chrono::Local::now().to_rfc3339(),
                version,
            });
            save_registry(&reg)?;

            println!("  {} Plugin installed: {}", green().apply_to("✓"), name);
            println!("    Source: {}", git_url);
            println!("    Path: {}", plugin_path.display());

            // Show manifest info if available
            if let Some(manifest) = manifest {
                if !manifest.description.is_empty() {
                    println!("    Description: {}", manifest.description);
                }
                if !manifest.author.is_empty() {
                    println!("    Author: {}", manifest.author);
                }
                if !manifest.provides_hooks.is_empty() {
                    println!("    Hooks: {}", manifest.provides_hooks.join(", "));
                }
                if !manifest.provides_tools.is_empty() {
                    println!("    Tools: {}", manifest.provides_tools.join(", "));
                }
                if !manifest.pip_dependencies.is_empty() {
                    println!("    Pip deps: {}", manifest.pip_dependencies.join(", "));
                }
            }
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

    let mut reg = load_registry();
    let before = reg.len();
    reg.retain(|p| p.name != name);
    if reg.len() < before {
        save_registry(&reg)?;
    }

    Ok(())
}

/// List installed plugins with manifest details.
pub fn cmd_plugins_list() -> anyhow::Result<()> {
    let reg = load_registry();
    let dir = plugins_dir();

    println!();
    println!("{}", cyan().apply_to("◆ Installed Plugins"));
    println!();

    if reg.is_empty() && !dir.exists() {
        println!("  {}", dim().apply_to("No plugins installed."));
        println!("  Install one with: hermez plugins install <owner/repo>");
        println!();
        return Ok(());
    }

    let mut seen = std::collections::HashSet::new();
    for plugin in &reg {
        seen.insert(plugin.name.as_str());
        let status = if plugin.enabled {
            green().apply_to("enabled").to_string()
        } else {
            yellow().apply_to("disabled").to_string()
        };

        // Try to read manifest for extra info
        let manifest = read_manifest(&dir.join(&plugin.name));
        let hooks = manifest.as_ref().map(|m| m.provides_hooks.clone()).unwrap_or_default();
        let tools = manifest.as_ref().map(|m| m.provides_tools.clone()).unwrap_or_default();

        println!("  {} — {} ({})", plugin.name, status, plugin.source);
        if let Some(ref v) = plugin.version {
            println!("    Version: {}", v);
        }
        if !hooks.is_empty() {
            println!("    Hooks: {}", hooks.join(", "));
        }
        if !tools.is_empty() {
            println!("    Tools: {}", tools.join(", "));
        }
    }

    // Show directory plugins not in registry
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !seen.contains(name) {
                    println!("  {} — {} (untracked)", name, dim().apply_to("local"));
                }
            }
        }
    }

    println!();
    Ok(())
}

/// Show detailed info for a plugin.
pub fn cmd_plugins_info(name: &str) -> anyhow::Result<()> {
    let plugin_path = plugins_dir().join(name);
    if !plugin_path.exists() {
        println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
        return Ok(());
    }

    println!();
    println!("{}", cyan().apply_to(format!("◆ Plugin: {}", name)));
    println!();

    let manifest = read_manifest(&plugin_path);
    match manifest {
        Some(m) => {
            println!("  Name:        {}", m.name);
            println!("  Version:     {}", m.version);
            println!("  Description: {}", m.description);
            println!("  Author:      {}", m.author);
            println!("  Manifest:    v{}", m.manifest_version);
            println!("  Path:        {}", plugin_path.display());

            if !m.provides_hooks.is_empty() {
                println!();
                println!("  Hooks:");
                for hook in &m.provides_hooks {
                    println!("    • {}", hook);
                }
            }

            if !m.provides_tools.is_empty() {
                println!();
                println!("  Tools:");
                for tool in &m.provides_tools {
                    println!("    • {}", tool);
                }
            }

            if !m.pip_dependencies.is_empty() {
                println!();
                println!("  Pip dependencies:");
                for dep in &m.pip_dependencies {
                    println!("    • {}", dep);
                }
            }

            if !m.requires_env.is_empty() {
                println!();
                println!("  Required env vars:");
                for env in &m.requires_env {
                    if let Some(s) = env.as_str() {
                        println!("    • {}", s);
                    } else if let Some(obj) = env.as_mapping() {
                        let name = obj.get(&serde_yaml::Value::String("name".into()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        println!("    • {}", name);
                    }
                }
            }
        }
        None => {
            println!("  {}", dim().apply_to("No plugin.yaml manifest found."));
            println!("  Path: {}", plugin_path.display());
        }
    }

    // Show registry status
    let reg = load_registry();
    if let Some(info) = reg.iter().find(|p| p.name == name) {
        println!();
        println!("  Registry status: {}", if info.enabled { green().apply_to("enabled") } else { yellow().apply_to("disabled") });
        println!("  Source: {}", info.source);
        println!("  Installed: {}", info.installed_at);
    }

    println!();
    Ok(())
}

/// Run a plugin's entry point (if defined).
pub fn cmd_plugins_run(name: &str, args: &[String]) -> anyhow::Result<()> {
    let plugin_path = plugins_dir().join(name);
    if !plugin_path.exists() {
        println!("  {} Plugin not found: {}", yellow().apply_to("✗"), name);
        return Ok(());
    }

    let manifest = read_manifest(&plugin_path);
    if manifest.is_none() {
        println!("  {} Plugin has no manifest: {}", yellow().apply_to("✗"), name);
        return Ok(());
    }

    println!();
    println!("{}", cyan().apply_to(format!("◆ Running plugin: {}", name)));
    if !args.is_empty() {
        println!("  Args: {}", args.join(" "));
    }
    println!();

    // Check for a run script
    let run_sh = plugin_path.join("run.sh");
    let run_py = plugin_path.join("run.py");
    let main_py = plugin_path.join("__init__.py");

    if run_sh.exists() {
        let output = std::process::Command::new("sh")
            .arg(&run_sh)
            .args(args)
            .current_dir(&plugin_path)
            .output()?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    } else if run_py.exists() {
        let output = std::process::Command::new("python3")
            .arg(&run_py)
            .args(args)
            .current_dir(&plugin_path)
            .output()?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    } else if main_py.exists() {
        println!("  {} Plugin has __init__.py but no run script.", dim().apply_to("→"));
        println!("  To run manually: cd {} && python3 -c 'import {}'", plugin_path.display(), name);
    } else {
        println!("  {} No runnable entry point found.", yellow().apply_to("→"));
        println!("  Expected: run.sh, run.py, or __init__.py");
    }

    println!();
    Ok(())
}

/// Show active hooks from the global hook registry.
pub fn cmd_plugins_hooks() -> anyhow::Result<()> {
    use hermez_agent_engine::plugin_system::global_hooks;

    println!();
    println!("{}", cyan().apply_to("◆ Active Plugin Hooks"));
    println!();

    let hooks = global_hooks().list();
    if hooks.is_empty() {
        println!("  {}", dim().apply_to("No hooks registered."));
    } else {
        for (name, count) in hooks {
            println!("  {} — {} callback{}", name, count, if count == 1 { "" } else { "s" });
        }
    }

    println!();
    println!("{}", dim().apply_to("Valid hooks:"));
    for hook in hermez_agent_engine::plugin_system::VALID_HOOKS {
        println!("  {}", hook);
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
    args: &[String],
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
        "info" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_info(n)
        }
        "run" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_run(n, args)
        }
        "hooks" => cmd_plugins_hooks(),
        "enable" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_enable(n)
        }
        "disable" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("name is required"))?;
            cmd_plugins_disable(n)
        }
        _ => {
            anyhow::bail!("Unknown action: {}. Use install, update, remove, list, info, run, hooks, enable, or disable.", action);
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
        let mut reg = Vec::new();
        reg.push(PluginInfo {
            name: "test_plugin".to_string(),
            source: "local".to_string(),
            enabled: true,
            installed_at: "".to_string(),
            version: None,
        });

        for plugin in &mut reg {
            if plugin.name == "test_plugin" {
                plugin.enabled = false;
                break;
            }
        }
        assert!(!reg.iter().find(|p| p.name == "test_plugin").unwrap().enabled);

        for plugin in &mut reg {
            if plugin.name == "test_plugin" {
                plugin.enabled = true;
                break;
            }
        }
        assert!(reg.iter().find(|p| p.name == "test_plugin").unwrap().enabled);
    }
}
