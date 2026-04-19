#![allow(dead_code)]
//! Uninstall command.
//!
//! Mirrors Python: hermes uninstall (remove Hermes and optionally all data)

use console::Style;
use std::path::PathBuf;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

/// Uninstall Hermes Agent.
pub fn cmd_uninstall(
    keep_data: bool,
    keep_config: bool,
    force: bool,
) -> anyhow::Result<()> {
    let home = get_hermes_home();

    println!();
    println!("{}", cyan().apply_to("◆ Hermes Uninstall"));
    println!();

    if !force
        && !crate::confirm("This will remove Hermes Agent from your system. Continue?")? {
            println!("  {}", dim().apply_to("Uninstall cancelled."));
            return Ok(());
        }

    // Remove binary (best-effort; may fail on Windows due to file locking)
    if let Ok(exe) = std::env::current_exe() {
        println!("  Removing binary: {}", exe.display());
        match std::fs::remove_file(&exe) {
            Ok(()) => {
                println!("  {} Binary removed.", green().apply_to("✓"));
            }
            Err(_) if cfg!(windows) => {
                println!("  {}", yellow().apply_to("⚠ Binary could not be deleted (file in use)."));
                println!("  {}", dim().apply_to(&format!(
                    "Delete manually: del \"{}\"",
                    exe.display()
                )));
            }
            Err(e) => {
                println!("  {} Binary deletion failed: {}", yellow().apply_to("⚠"), e);
            }
        }
    } else {
        println!("  {} Could not locate binary.", yellow().apply_to("⚠"));
    }
    println!();

    if keep_data {
        println!("  {}", dim().apply_to("Preserving data in ~/.hermes/"));
    } else {
        println!("  Removing data directory: {}", home.display());
        if home.exists() {
            match std::fs::remove_dir_all(&home) {
                Ok(()) => println!("  {} Data removed.", green().apply_to("✓")),
                Err(e) => println!("  {} Failed to remove data: {e}", yellow().apply_to("⚠")),
            }
        }
    }

    if keep_config {
        println!("  {}", dim().apply_to("Preserving config."));
    }

    // Remove gateway service if installed
    if has_systemd() {
        let _ = std::process::Command::new("systemctl")
            .args(["disable", "hermes-gateway"])
            .output();
        let _ = std::process::Command::new("systemctl")
            .args(["stop", "hermes-gateway"])
            .output();
    }

    println!();
    println!("  {} Hermes Agent uninstalled.", green().apply_to("✓"));
    println!();

    Ok(())
}

fn has_systemd() -> bool {
    cfg!(target_os = "linux") && std::path::Path::new("/run/systemd/system").exists()
}
