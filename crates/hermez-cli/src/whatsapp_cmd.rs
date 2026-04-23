#![allow(dead_code)]
//! WhatsApp integration command.
//!
//! Mirrors Python: hermez whatsapp (configure WhatsApp Cloud API)

use console::Style;
use std::path::PathBuf;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

fn get_hermez_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMEZ_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermez")
    } else {
        PathBuf::from(".hermez")
    }
}

/// Configure WhatsApp Cloud API integration.
pub fn cmd_whatsapp(action: &str, token: Option<&str>, phone_id: Option<&str>) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ WhatsApp Integration"));
    println!();

    match action {
        "setup" => {
            println!("  {}", dim().apply_to("WhatsApp Cloud API Setup:"));
            println!();
            println!("  1. Create a Meta Developer account");
            println!("  2. Create a WhatsApp Business app");
            println!("  3. Get your Phone Number ID and Access Token");
            println!();
            println!("  {}", dim().apply_to("Then run: hermez whatsapp connect --token <token> --phone-id <id>"));
        }
        "connect" => {
            let token_val = token.ok_or_else(|| anyhow::anyhow!("--token is required"))?;
            let phone_id_val = phone_id.ok_or_else(|| anyhow::anyhow!("--phone-id is required"))?;

            let config_path = get_hermez_home().join("gateway_config.yaml");
            let mut config: serde_yaml::Value = if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                serde_yaml::from_str(&content).unwrap_or(serde_yaml::Value::Mapping(Default::default()))
            } else {
                serde_yaml::Value::Mapping(Default::default())
            };

            if let Some(map) = config.as_mapping_mut() {
                let mut whatsapp_map = serde_yaml::Mapping::new();
                whatsapp_map.insert(
                    serde_yaml::Value::String("access_token".to_string()),
                    serde_yaml::Value::String(token_val.to_string()),
                );
                whatsapp_map.insert(
                    serde_yaml::Value::String("phone_number_id".to_string()),
                    serde_yaml::Value::String(phone_id_val.to_string()),
                );
                whatsapp_map.insert(
                    serde_yaml::Value::String("enabled".to_string()),
                    serde_yaml::Value::Bool(true),
                );
                map.insert(
                    serde_yaml::Value::String("whatsapp".to_string()),
                    serde_yaml::Value::Mapping(whatsapp_map),
                );
            }

            std::fs::write(&config_path, serde_yaml::to_string(&config)?)?;
            println!("  {} Token configured for phone: {}", green().apply_to("✓"), phone_id_val);
            println!("  {}", dim().apply_to("Restart gateway to apply: hermez gateway restart"));
        }
        "status" => {
            let config_path = get_hermez_home().join("gateway_config.yaml");
            if config_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    if content.contains("whatsapp") {
                        println!("  {} WhatsApp integration: configured", green().apply_to("✓"));
                    } else {
                        println!("  {} WhatsApp integration: not configured", yellow().apply_to("⚠"));
                    }
                }
            } else {
                println!("  {} No gateway config found", yellow().apply_to("⚠"));
            }
        }
        _ => {
            println!("  {}", dim().apply_to("Usage: hermez whatsapp <setup|connect|status>"));
        }
    }
    println!();

    Ok(())
}
