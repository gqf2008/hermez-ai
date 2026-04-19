#![allow(dead_code)]
//! Backup and restore commands for Hermes Agent.
//!
//! Mirrors the Python `hermes_cli/backup.py`.
//! Features: backup config, sessions, skills, and full state.

use console::Style;
use std::path::{Path, PathBuf};

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
fn red() -> Style { Style::new().red() }

/// Create a backup of Hermes state.
pub fn cmd_backup(output: Option<&str>, include_sessions: bool) -> anyhow::Result<()> {
    cmd_backup_extended(output, include_sessions, false, None)
}

/// Extended backup with quick mode and custom label.
pub fn cmd_backup_extended(output: Option<&str>, include_sessions: bool, _quick: bool, _label: Option<&str>) -> anyhow::Result<()> {
    let home = get_hermes_home();
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("hermes_backup_{timestamp}");

    let backup_dir = if let Some(out) = output {
        PathBuf::from(out)
    } else {
        std::env::current_dir()?.join(&backup_name)
    };

    println!();
    println!("{}", cyan().apply_to("◆ Backup"));
    println!();
    println!("  Source: {}", home.display());
    println!("  Destination: {}", backup_dir.display());
    println!();

    // Create backup directory
    std::fs::create_dir_all(&backup_dir)?;

    let mut items_backed_up = 0;

    // Always backup config
    let config_src = home.join("config.yaml");
    if config_src.exists() {
        let config_dst = backup_dir.join("config.yaml");
        std::fs::copy(&config_src, &config_dst)?;
        println!("  {} config.yaml", green().apply_to("✓"));
        items_backed_up += 1;
    }

    // Always backup .env
    let env_src = home.join(".env");
    if env_src.exists() {
        let env_dst = backup_dir.join(".env");
        std::fs::copy(&env_src, &env_dst)?;
        println!("  {} .env (credentials)", green().apply_to("✓"));
        items_backed_up += 1;
    }

    // Backup skills directory
    let skills_src = home.join("skills");
    if skills_src.exists() {
        let skills_dst = backup_dir.join("skills");
        copy_dir_all(&skills_src, &skills_dst)?;
        println!("  {} skills/", green().apply_to("✓"));
        items_backed_up += 1;
    }

    // Backup sessions if requested
    if include_sessions {
        let sessions_src = home.join("sessions.db");
        if sessions_src.exists() {
            let sessions_dst = backup_dir.join("sessions.db");
            std::fs::copy(&sessions_src, &sessions_dst)?;
            println!("  {} sessions.db", green().apply_to("✓"));
            items_backed_up += 1;
        }

        // Backup session exports
        let exports_src = home.join("exports");
        if exports_src.exists() {
            let exports_dst = backup_dir.join("exports");
            copy_dir_all(&exports_src, &exports_dst)?;
            println!("  {} exports/", green().apply_to("✓"));
            items_backed_up += 1;
        }
    } else {
        println!("  {} sessions skipped (use --include-sessions)", dim().apply_to("○"));
    }

    // Backup cron jobs
    let cron_src = home.join("cron_jobs.json");
    if cron_src.exists() {
        let cron_dst = backup_dir.join("cron_jobs.json");
        std::fs::copy(&cron_src, &cron_dst)?;
        println!("  {} cron_jobs.json", green().apply_to("✓"));
        items_backed_up += 1;
    }

    // Backup approval allowlist
    let approval_src = home.join(".approval_allowlist.json");
    if approval_src.exists() {
        let approval_dst = backup_dir.join(".approval_allowlist.json");
        std::fs::copy(&approval_src, &approval_dst)?;
        println!("  {} .approval_allowlist.json", green().apply_to("✓"));
        items_backed_up += 1;
    }

    println!();
    println!("  {} {items_backed_up} item(s) backed up.", green().apply_to("✓"));

    // Create backup manifest
    let manifest = format!(
        "Hermes Agent Backup\n\
         Timestamp: {}\n\
         Source: {}\n\
         Items: {}\n\
         Include Sessions: {}\n",
        timestamp,
        home.display(),
        items_backed_up,
        include_sessions,
    );
    std::fs::write(backup_dir.join("BACKUP_MANIFEST.txt"), manifest)?;

    println!("  Manifest: {}", backup_dir.join("BACKUP_MANIFEST.txt").display());
    println!();

    Ok(())
}

/// Restore from a backup.
pub fn cmd_restore(backup_path: &str, force: bool) -> anyhow::Result<()> {
    let backup_dir = PathBuf::from(backup_path);
    let home = get_hermes_home();

    if !backup_dir.exists() {
        println!("  {} Backup not found: {}", yellow().apply_to("✗"), backup_path);
        return Ok(());
    }

    // Check for manifest
    let manifest = backup_dir.join("BACKUP_MANIFEST.txt");
    if manifest.exists() {
        let content = std::fs::read_to_string(&manifest)?;
        println!();
        println!("{}", cyan().apply_to("◆ Restore from Backup"));
        println!();
        println!("{}", dim().apply_to(&content));
    }

    if !force
        && !super::confirm(&format!("Restore to {}? This may overwrite existing files.", home.display()))? {
            println!("  {} Restore cancelled.", dim().apply_to("○"));
            return Ok(());
        }

    std::fs::create_dir_all(&home)?;

    let mut items_restored = 0;

    // Restore config
    let config_src = backup_dir.join("config.yaml");
    if config_src.exists() {
        let config_dst = home.join("config.yaml");
        std::fs::copy(&config_src, &config_dst)?;
        println!("  {} config.yaml", green().apply_to("✓"));
        items_restored += 1;
    }

    // Restore .env
    let env_src = backup_dir.join(".env");
    if env_src.exists() {
        let env_dst = home.join(".env");
        std::fs::copy(&env_src, &env_dst)?;
        println!("  {} .env", green().apply_to("✓"));
        items_restored += 1;
    }

    // Restore skills
    let skills_src = backup_dir.join("skills");
    if skills_src.exists() {
        let skills_dst = home.join("skills");
        copy_dir_all(&skills_src, &skills_dst)?;
        println!("  {} skills/", green().apply_to("✓"));
        items_restored += 1;
    }

    // Restore sessions
    let sessions_src = backup_dir.join("sessions.db");
    if sessions_src.exists() {
        let sessions_dst = home.join("sessions.db");
        std::fs::copy(&sessions_src, &sessions_dst)?;
        println!("  {} sessions.db", green().apply_to("✓"));
        items_restored += 1;
    }

    // Restore cron jobs
    let cron_src = backup_dir.join("cron_jobs.json");
    if cron_src.exists() {
        let cron_dst = home.join("cron_jobs.json");
        std::fs::copy(&cron_src, &cron_dst)?;
        println!("  {} cron_jobs.json", green().apply_to("✓"));
        items_restored += 1;
    }

    // Restore approval allowlist
    let approval_src = backup_dir.join(".approval_allowlist.json");
    if approval_src.exists() {
        let approval_dst = home.join(".approval_allowlist.json");
        std::fs::copy(&approval_src, &approval_dst)?;
        println!("  {} .approval_allowlist.json", green().apply_to("✓"));
        items_restored += 1;
    }

    println!();
    println!("  {} {items_restored} item(s) restored.", green().apply_to("✓"));
    println!();

    Ok(())
}

/// List available backups.
pub fn cmd_backup_list() -> anyhow::Result<()> {
    let current_dir = std::env::current_dir()?;
    let backups: Vec<_> = std::fs::read_dir(&current_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("hermes_backup_")
        })
        .collect();

    println!();
    println!("{}", cyan().apply_to("◆ Available Backups"));
    println!();

    if backups.is_empty() {
        println!("  {}", dim().apply_to("No backups found in current directory."));
        println!("  Create one with: hermes backup");
    } else {
        for entry in &backups {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let manifest = entry.path().join("BACKUP_MANIFEST.txt");
            let items = if manifest.exists() {
                let content = std::fs::read_to_string(&manifest).unwrap_or_default();
                content.lines()
                    .find(|l| l.starts_with("Items:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("?")
                    .to_string()
            } else {
                "?".to_string()
            };
            println!("  {} — {} items", name, items);
        }
    }
    println!();

    Ok(())
}

/// Import a backup from a zip archive.
pub fn cmd_import(archive_path: &str, force: bool) -> anyhow::Result<()> {
    let archive = std::path::Path::new(archive_path);
    if !archive.exists() {
        println!("  {} Archive not found: {}", red().apply_to("✗"), archive_path);
        return Ok(());
    }

    if !force
        && !super::confirm(&format!("Restore backup from {}?", archive_path))? {
            println!("  {}", dim().apply_to("Import cancelled."));
            return Ok(());
        }

    println!("  {} Extracting backup...", cyan().apply_to("→"));

    // Try to extract as zip
    let home = get_hermes_home();
    std::fs::create_dir_all(&home)?;

    // Simple zip extraction using zip command or manual
    let output = std::process::Command::new("tar")
        .args(["-xf", archive_path, "-C", &home.to_string_lossy()])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            println!("  {} Backup imported to {}", green().apply_to("✓"), home.display());
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            println!("  {} Import failed: {}", yellow().apply_to("⚠"), err.trim());
            println!("  Try manually extracting to {}", home.display());
        }
        Err(e) => {
            println!("  {} Failed to extract: {}", yellow().apply_to("⚠"), e);
            println!("  Try manually extracting to {}", home.display());
        }
    }

    println!();
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// confirm() is provided by hermes_cli::confirm()
