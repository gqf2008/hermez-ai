#![allow(dead_code)]
//! External memory provider configuration.

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

/// Available memory providers.
const PROVIDERS: &[(&str, &str)] = &[
    ("honcho", "Honcho AI memory - Nous Research"),
    ("openviking", "OpenViking - decentralized memory"),
    ("mem0", "Mem0 - AI memory layer"),
    ("hindsight", "Hindsight - conversation memory"),
    ("holographic", "Holographic memory"),
    ("retaindb", "RetainDB - persistent memory"),
    ("byterover", "ByteRover - semantic memory"),
];

fn config_path() -> PathBuf {
    get_hermez_home().join("config.yaml")
}

fn load_config() -> anyhow::Result<serde_yaml::Value> {
    let path = config_path();
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        Ok(serde_yaml::from_str(&content)?)
    } else {
        Ok(serde_yaml::Value::Mapping(Default::default()))
    }
}

fn save_config(config: &serde_yaml::Value) -> anyhow::Result<()> {
    let path = config_path();
    let content = serde_yaml::to_string(config)?;
    std::fs::write(&path, content)?;
    Ok(())
}

fn set_config_value(config: &mut serde_yaml::Mapping, key: &str, value: serde_yaml::Value) {
    config.insert(serde_yaml::Value::String(key.to_string()), value);
}

/// Interactive setup of external memory provider.
pub fn cmd_memory_setup() -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Memory Provider Setup"));
    println!();
    println!("Select a memory provider:");
    println!();
    for (i, (name, desc)) in PROVIDERS.iter().enumerate() {
        println!("  {}. {} — {}", i + 1, name, dim().apply_to(desc));
    }
    println!();
    println!("  0. None (built-in memory only)");
    println!();

    print!("Choice [0]: ");
    use std::io::{self, Write};
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().unwrap_or(0);

    let mut config = load_config()?;
    let mapping = config.as_mapping_mut().ok_or_else(|| anyhow::anyhow!("Config file is not a YAML mapping"))?;

    if choice == 0 {
        // Remove existing provider config
        mapping.remove(serde_yaml::Value::String("memory_provider".to_string()));
        save_config(&config)?;
        println!("  {} External memory disabled. Using built-in memory only.", green().apply_to("✓"));
    } else if choice <= PROVIDERS.len() {
        let provider = PROVIDERS[choice - 1].0;
        set_config_value(
            mapping,
            "memory_provider",
            serde_yaml::Value::String(provider.to_string()),
        );

        // Provider-specific setup prompts
        match provider {
            "honcho" => {
                println!("  See `hermez honcho setup` for detailed Honcho configuration.");
            }
            "mem0" => {
                print!("Mem0 API key: ");
                io::stdout().flush()?;
                let mut key = String::new();
                io::stdin().read_line(&mut key)?;
                let key = key.trim();
                if !key.is_empty() {
                    set_config_value(mapping, "mem0_api_key", serde_yaml::Value::String(key.to_string()));
                }
            }
            _ => {
                println!("  Configure additional settings in {}", config_path().display());
            }
        }

        save_config(&config)?;
        println!("  {} Memory provider set to: {}", green().apply_to("✓"), provider);
    } else {
        println!("  {} Invalid choice.", yellow().apply_to("✗"));
    }

    println!();
    Ok(())
}

/// Show current memory provider configuration.
pub fn cmd_memory_status() -> anyhow::Result<()> {
    let config = load_config()?;
    let provider = config.get("memory_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("built-in");

    println!();
    println!("{}", cyan().apply_to("◆ Memory Configuration"));
    println!();
    println!("  Provider: {}", provider);

    // Show provider-specific config
    if provider != "built-in" {
        println!();
        println!("  {}", cyan().apply_to("Provider Details:"));
        if let Some(mapping) = config.as_mapping() {
            for (key, value) in mapping {
                if let Some(k) = key.as_str() {
                    if k.starts_with("mem0") || k.starts_with("honcho") || k.starts_with("viking") {
                        let display = if k.contains("key") || k.contains("token") || k.contains("secret") {
                            "[REDACTED]".to_string()
                        } else {
                            serde_json::to_string(value).unwrap_or_else(|_| "?".to_string())
                        };
                        println!("    {}: {}", k, display);
                    }
                }
            }
        }
    }

    println!();
    println!("  {}", dim().apply_to("Built-in memory (MEMORY.md/USER.md) is always active."));
    println!();

    Ok(())
}

/// Disable external memory provider.
pub fn cmd_memory_off() -> anyhow::Result<()> {
    let mut config = load_config()?;
    let mapping = config.as_mapping_mut().ok_or_else(|| anyhow::anyhow!("Config file is not a YAML mapping"))?;
    mapping.remove(serde_yaml::Value::String("memory_provider".to_string()));
    save_config(&config)?;
    println!("  {} External memory disabled. Using built-in memory only.", green().apply_to("✓"));
    Ok(())
}

/// Dispatch memory subcommands.
pub fn cmd_memory(action: &str) -> anyhow::Result<()> {
    match action {
        "setup" => cmd_memory_setup(),
        "status" | "" => cmd_memory_status(),
        "off" => cmd_memory_off(),
        _ => {
            anyhow::bail!("Unknown action: {}. Use setup, status, or off.", action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_providers_not_empty() {
        assert!(!PROVIDERS.is_empty());
        assert!(PROVIDERS.len() >= 5);
    }

    #[test]
    fn test_provider_names_unique() {
        let names: Vec<&str> = PROVIDERS.iter().map(|(n, _)| *n).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "Duplicate provider names found");
    }
}
