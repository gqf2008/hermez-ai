#![allow(dead_code)]
//! Device pairing management.
//!
//! Mirrors Python: hermez pairing (list/approve/revoke/clear-pending)

use console::Style;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingEntry {
    code: String,
    device_name: String,
    approved: bool,
    timestamp: String,
}

fn pairing_path() -> PathBuf {
    let home = get_hermez_home();
    home.join("pairings.json")
}

fn get_hermez_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMEZ_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermez")
    } else {
        PathBuf::from(".hermez")
    }
}

fn load_pairings() -> Vec<PairingEntry> {
    let path = pairing_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(entries) = serde_json::from_str::<Vec<PairingEntry>>(&content) {
                return entries;
            }
        }
    }
    Vec::new()
}

fn save_pairings(entries: &[PairingEntry]) -> anyhow::Result<()> {
    let path = pairing_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(entries)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// List pending and approved pairings.
pub fn cmd_pairing_list() -> anyhow::Result<()> {
    let entries = load_pairings();

    println!();
    println!("{}", cyan().apply_to("◆ Device Pairings"));
    println!();

    if entries.is_empty() {
        println!("  {}", dim().apply_to("No device pairings configured."));
        println!();
        return Ok(());
    }

    let (approved, pending): (Vec<_>, Vec<_>) = entries.iter().partition(|e| e.approved);

    if !approved.is_empty() {
        println!("  {}", green().apply_to("Approved:"));
        for entry in &approved {
            println!("    {} {} — {}", entry.device_name, entry.code, entry.timestamp);
        }
        println!();
    }

    if !pending.is_empty() {
        println!("  {}", yellow().apply_to("Pending:"));
        for entry in &pending {
            println!("    {} {} — {}", entry.device_name, entry.code, entry.timestamp);
        }
        println!();
    }

    Ok(())
}

/// Approve a pairing code.
pub fn cmd_pairing_approve(_platform: &str, code: &str) -> anyhow::Result<()> {
    let mut entries = load_pairings();

    for entry in &mut entries {
        if entry.code == code {
            entry.approved = true;
            save_pairings(&entries)?;
            println!("  {} Pairing approved: {}", green().apply_to("✓"), code);
            println!();
            return Ok(());
        }
    }

    println!("  {} Pairing code not found: {}", yellow().apply_to("⚠"), code);
    println!();
    Ok(())
}

/// Revoke a device pairing.
pub fn cmd_pairing_revoke(_platform: &str, code: &str) -> anyhow::Result<()> {
    let mut entries = load_pairings();
    let before = entries.len();
    entries.retain(|e| e.code != code);

    if entries.len() < before {
        save_pairings(&entries)?;
        println!("  {} Pairing revoked: {}", green().apply_to("✓"), code);
    } else {
        println!("  {} Pairing code not found: {}", yellow().apply_to("⚠"), code);
    }
    println!();
    Ok(())
}

/// Clear all pending pairings.
pub fn cmd_pairing_clear_pending() -> anyhow::Result<()> {
    let mut entries = load_pairings();
    let before = entries.len();
    entries.retain(|e| e.approved);

    let removed = before - entries.len();
    if removed > 0 {
        save_pairings(&entries)?;
        println!("  {} Cleared {removed} pending pairing(s).", green().apply_to("✓"));
    } else {
        println!("  {}", dim().apply_to("No pending pairings to clear."));
    }
    println!();
    Ok(())
}
