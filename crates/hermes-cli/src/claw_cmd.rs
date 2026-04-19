#![allow(dead_code)]
//! Claw migration command — migrate from OpenClaw (or Claude Code) to Hermes.
//!
//! Mirrors Python: `hermes claw` in `hermes_cli/claw.py`.
//!
//! For OpenClaw migration, the Python script `openclaw_to_hermes.py` is the
//! canonical implementation.  This Rust command finds that script and invokes
//! it as a subprocess, handling the preview → confirm → execute flow.

use console::Style;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn red() -> Style { Style::new().red() }
fn dim() -> Style { Style::new().dim() }

/// Known OpenClaw directory names (current + legacy).
const OPENCLAW_DIR_NAMES: &[&str] = &[".openclaw", ".clawdbot", ".moltbot"];

/// Known paths where the Python migration script may live.
fn migration_script_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // Installed alongside the binary (bundled skills directory).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // hermes is in target/release/ or bundled/some/path/
            // Try sibling optional-skills/ directory.
            candidates.push(parent.join("optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
            // Try parent of parent.
            if let Some(pp) = parent.parent() {
                candidates.push(pp.join("optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
                // Try going up more levels (common in dev setups: target/release/hermes).
                if let Some(ppp) = pp.parent() {
                    candidates.push(ppp.join("optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
                    // Project root may be another level up (e.g. hermes-rs/ under project root).
                    if let Some(pppp) = ppp.parent() {
                        candidates.push(pppp.join("optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
                    }
                }
            }
        }
    }

    // HERMES_HOME skills directory (user installed from Skills Hub).
    if let Ok(home) = std::env::var("HERMES_HOME") {
        candidates.push(PathBuf::from(&home).join("skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
    } else if let Some(dir) = dirs::home_dir() {
        candidates.push(dir.join(".hermes/skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
    }

    // Current working directory (dev setup).
    candidates.push(PathBuf::from("optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));
    candidates.push(PathBuf::from("../optional-skills/migration/openclaw-migration/scripts/openclaw_to_hermes.py"));

    candidates
}

fn find_migration_script() -> Option<PathBuf> {
    for candidate in migration_script_candidates() {
        if candidate.is_file() {
            return Some(candidate.canonicalize().ok().unwrap_or(candidate));
        }
    }
    None
}

fn find_openclaw_dir(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(src) = explicit {
        let p = PathBuf::from(src);
        if p.is_dir() { return Some(p); }
        return None;
    }
    for name in OPENCLAW_DIR_NAMES {
        if let Some(home) = dirs::home_dir() {
            let candidate = home.join(name);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Cached Python binary — Python location doesn't change at runtime.
static CACHED_PYTHON: OnceLock<Option<String>> = OnceLock::new();

fn find_python() -> Option<String> {
    CACHED_PYTHON
        .get_or_init(|| {
            for name in ["python3", "python", "py"] {
                if Command::new(name).arg("--version").output().is_ok() {
                    return Some(name.to_string());
                }
            }
            None
        })
        .clone()
}

/// Migrate from OpenClaw by invoking the Python migration script.
fn migrate_openclaw(
    source: Option<&str>,
    dry_run: bool,
    preset: &str,
    overwrite: bool,
    migrate_secrets: bool,
    yes: bool,
    workspace_target: Option<&str>,
    skill_conflict: &str,
) -> anyhow::Result<()> {
    // Find OpenClaw source directory.
    let Some(source_dir) = find_openclaw_dir(source) else {
        println!();
        println!("{}", yellow().apply_to("OpenClaw directory not found."));
        if let Some(home) = dirs::home_dir() {
            println!("  {}", dim().apply_to(format!(
                "Searched: {}",
                OPENCLAW_DIR_NAMES.iter().map(|n| home.join(n).display().to_string()).collect::<Vec<_>>().join(", ")
            )));
        }
        println!("  {}", dim().apply_to("Specify a custom path: hermes claw migrate --source /path/to/.openclaw"));
        return Ok(());
    };

    // Find migration script.
    let Some(script_path) = find_migration_script() else {
        println!();
        println!("{}", yellow().apply_to("Migration script not found."));
        println!("  {}", dim().apply_to("Expected at one of:"));
        for c in migration_script_candidates() {
            println!("    {}", c.display());
        }
        println!("  {}", dim().apply_to("Install the openclaw-migration skill: hermes skills install openclaw-migration"));
        return Ok(());
    };

    let Some(python) = find_python() else {
        println!();
        println!("{}", yellow().apply_to("Python not found."));
        println!("  {}", dim().apply_to("The OpenClaw migration requires Python 3.11+."));
        println!("  {}", dim().apply_to("Install Python and re-run, or use the Python Hermes CLI."));
        return Ok(());
    };

    let hermes_home = get_hermes_home();

    // Show migration settings.
    println!();
    println!("{}", cyan().apply_to("◆ Hermes — OpenClaw Migration"));
    println!();
    println!("  Source:      {}", source_dir.display());
    println!("  Target:      {}", hermes_home.display());
    println!("  Preset:      {preset}");
    println!("  Overwrite:   {}", if overwrite { "yes" } else { "no (skip conflicts)" });
    println!("  Secrets:     {}", if migrate_secrets || preset == "full" { "yes (allowlisted only)" } else { "no" });
    if skill_conflict != "skip" {
        println!("  Skill conflicts: {skill_conflict}");
    }
    if let Some(ws) = workspace_target {
        println!("  Workspace:   {ws}");
    }
    println!();

    // ── Phase 1: Always run dry-run preview first ──
    println!("  Running migration preview...");
    let preview_status = run_python_script(
        &python, &script_path, &source_dir, &hermes_home,
        false,  // execute = false → dry run
        overwrite, migrate_secrets, workspace_target, skill_conflict, preset,
    )?;

    if !preview_status.success() {
        println!();
        println!("{}", red().apply_to("Migration preview failed."));
        if let Some(code) = preview_status.code() {
            println!("  Python script exited with code {code}");
        }
        return Ok(());
    }

    // If --dry-run, stop here.
    if dry_run {
        println!();
        println!("{}", dim().apply_to("Dry run complete. No files were modified."));
        println!("  {}", dim().apply_to("Run without --dry-run to execute the migration."));
        return Ok(());
    }

    // ── Phase 2: Confirm and execute ──
    println!();
    if !yes && !crate::confirm("Proceed with migration?")? {
        println!("  Migration cancelled.");
        return Ok(());
    }

    println!("  Executing migration...");
    let exec_status = run_python_script(
        &python, &script_path, &source_dir, &hermes_home,
        true,   // execute = true
        overwrite, migrate_secrets, workspace_target, skill_conflict, preset,
    )?;

    if !exec_status.success() {
        println!();
        println!("{}", red().apply_to("Migration failed."));
        if let Some(code) = exec_status.code() {
            println!("  Python script exited with code {code}");
        }
        return Ok(());
    }

    println!();
    println!("{}", green().apply_to("Migration complete!"));
    println!("  {}", dim().apply_to("Imported skills and memories take effect in a new session."));
    println!("  {}", dim().apply_to("Run `hermes claw cleanup` to archive leftover OpenClaw directories."));
    println!();

    Ok(())
}

fn run_python_script(
    python: &str,
    script: &Path,
    source_dir: &Path,
    hermes_home: &Path,
    execute: bool,
    overwrite: bool,
    migrate_secrets: bool,
    workspace_target: Option<&str>,
    skill_conflict: &str,
    preset: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let mut cmd = Command::new(python);
    cmd.arg(script);
    cmd.arg("--source").arg(source_dir);
    cmd.arg("--target").arg(hermes_home);
    cmd.arg("--preset").arg(preset);
    if overwrite {
        cmd.arg("--overwrite");
    }
    if migrate_secrets || preset == "full" {
        cmd.arg("--migrate-secrets");
    }
    cmd.arg("--skill-conflict").arg(skill_conflict);
    if let Some(ws) = workspace_target {
        cmd.arg("--workspace-target").arg(ws);
    }
    if execute {
        cmd.arg("--execute");
    }

    let status = cmd.status()?;
    Ok(status)
}

/// Migrate from Claude Code (stub — basic config/memory migration).
fn migrate_claude_code(force: bool) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Claw Migration — Claude Code"));
    println!();

    let claude_home = match dirs::home_dir() {
        Some(h) => h.join(".claude"),
        None => {
            println!("  {} Could not find home directory.", yellow().apply_to("⚠"));
            return Ok(());
        }
    };

    if !claude_home.exists() {
        println!("  {} Claude Code config not found at ~/.claude", yellow().apply_to("⚠"));
        println!("  {}", dim().apply_to("Make sure Claude Code is installed and configured."));
        return Ok(());
    }

    println!("  Found Claude Code config at: {}", claude_home.display());

    let hermes_home = get_hermes_home();
    std::fs::create_dir_all(&hermes_home)?;

    let mut migrated = 0;
    let mut skipped = 0;

    // Migrate settings.
    let claude_settings = claude_home.join("settings.json");
    if claude_settings.exists() {
        if let Ok(content) = std::fs::read_to_string(&claude_settings) {
            if let Ok(settings) = serde_json::from_str::<serde_json::Value>(&content) {
                let config_file = hermes_home.join("config.yaml");
                if config_file.exists() && !force {
                    println!("  {} config.yaml already exists (use --force to overwrite).", yellow().apply_to("⚠"));
                    skipped += 1;
                } else {
                    let mut config: serde_yaml::Value = serde_yaml::Value::Mapping(Default::default());
                    if let Some(model) = settings.get("model").and_then(|v| v.as_str()) {
                        if let Some(map) = config.as_mapping_mut() {
                            map.insert(
                                serde_yaml::Value::String("model".to_string()),
                                serde_yaml::Value::String(model.to_string()),
                            );
                        }
                    }
                    let yaml = serde_yaml::to_string(&config)?;
                    std::fs::write(&config_file, yaml)?;
                    println!("  {} Settings migrated.", green().apply_to("✓"));
                    migrated += 1;
                }
            }
        }
    } else {
        println!("  {} No settings.json found.", dim().apply_to("─"));
        skipped += 1;
    }

    // Migrate memories.
    let claude_memories = claude_home.join("projects");
    if claude_memories.exists() {
        println!("  Migrating memories...");
        let hermes_memories = hermes_home.join("memories");
        std::fs::create_dir_all(&hermes_memories)?;

        match std::fs::read_dir(&claude_memories) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let src = entry.path();
                    let dst = hermes_memories.join(entry.file_name());
                    if src.is_file() {
                        if dst.exists() && !force {
                            skipped += 1;
                        } else if std::fs::copy(&src, &dst).is_ok() {
                            migrated += 1;
                        }
                    }
                }
            }
            Err(e) => {
                println!("  {} Failed to read projects dir: {}", yellow().apply_to("⚠"), e);
            }
        }
        println!("  {} Memories: {migrated} migrated, {skipped} skipped.", green().apply_to("✓"));
    }

    println!();
    println!("  {} Migration complete.", green().apply_to("✓"));
    println!("  {}", dim().apply_to("Run `hermes claw cleanup` to remove migration artifacts."));
    println!();

    Ok(())
}

/// Clean up migration artifacts.
fn cmd_cleanup(source: Option<&str>) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Claw Cleanup"));
    println!();

    // Remove migration temp files.
    let hermes_home = get_hermes_home();
    let temp_dir = hermes_home.join(".migration");
    if temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        println!("  {} Migration temp files removed.", green().apply_to("✓"));
    }

    // Check for OpenClaw directories that can be archived.
    let dirs_to_check: Vec<PathBuf> = if let Some(src) = source {
        vec![PathBuf::from(src)]
    } else {
        dirs::home_dir()
            .into_iter()
            .flat_map(|home| {
                OPENCLAW_DIR_NAMES.iter().map(move |name| home.join(name))
            })
            .filter(|p| p.is_dir())
            .collect()
    };

    if dirs_to_check.is_empty() {
        println!("  {} No OpenClaw directories found.", green().apply_to("✓"));
        println!("  {}", dim().apply_to("Nothing to clean up."));
        return Ok(());
    }

    for dir in &dirs_to_check {
        let timestamp = chrono::Local::now().format("%Y%m%d");
        let archive_name = format!("{}.pre-migration-{}", dir.file_name().unwrap().to_string_lossy(), timestamp);
        let archive_path = dir.parent().unwrap_or(dir).join(&archive_name);

        // If archive already exists, add counter.
        let mut counter = 2;
        let mut final_path = archive_path.clone();
        while final_path.exists() {
            final_path = dir.parent().unwrap_or(dir).join(format!("{}-{counter}", archive_name));
            counter += 1;
        }

        print!("  Archive {} → {}? ", dir.display(), final_path.display());
        if crate::confirm("Confirm")? {
            match std::fs::rename(dir, &final_path) {
                Ok(()) => {
                    println!("  {} Archived.", green().apply_to("✓"));
                }
                Err(e) => {
                    println!();
                    println!("  {} Failed: {}", red().apply_to("✗"), e);
                    println!("  {}", dim().apply_to(format!("Try manually: mv {} {}", dir.display(), final_path.display())));
                }
            }
        } else {
            println!("  {} Skipped.", dim().apply_to("─"));
        }
    }

    println!();
    Ok(())
}

fn get_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        PathBuf::from(home)
    } else if let Some(dir) = dirs::home_dir() {
        dir.join(".hermes")
    } else {
        PathBuf::from(".hermes")
    }
}

/// Claw subcommands: migrate, cleanup.
pub fn cmd_claw(
    action: &str,
    source: Option<&str>,
    force: bool,
    dry_run: bool,
    preset: &str,
    overwrite: bool,
    migrate_secrets: bool,
    yes: bool,
    workspace_target: Option<&str>,
    skill_conflict: &str,
) -> anyhow::Result<()> {
    match action {
        "migrate" => {
            match source {
                None
                | Some("~/.openclaw" | ".openclaw" | ".clawdbot" | ".moltbot" | "openclaw") => {
                    let src = source.filter(|s| !matches!(*s, "openclaw" | "~/.openclaw"));
                    migrate_openclaw(
                        src, dry_run, preset, overwrite, migrate_secrets, yes,
                        workspace_target, skill_conflict,
                    )
                }
                Some("claude-code" | "claude") => migrate_claude_code(force),
                Some("chatgpt" | "openai") => {
                    println!();
                    println!("{}", cyan().apply_to("◆ Claw Migration"));
                    println!();
                    println!("  {}", dim().apply_to("OpenAI ChatGPT migration not yet implemented."));
                    Ok(())
                }
                Some(src) => {
                    if PathBuf::from(src).is_dir() {
                        migrate_openclaw(
                            source, dry_run, preset, overwrite, migrate_secrets, yes,
                            workspace_target, skill_conflict,
                        )
                    } else {
                        println!();
                        println!("{}", yellow().apply_to("Unknown source: {src}"));
                        println!("  {}", dim().apply_to("Supported: openclaw, claude-code, chatgpt, or a filesystem path"));
                        Ok(())
                    }
                }
            }
        }
        "cleanup" | "clean" => cmd_cleanup(source),
        _ => {
            println!("Usage: hermes claw <command> [options]");
            println!();
            println!("Commands:");
            println!("  migrate          Migrate settings from OpenClaw or Claude Code to Hermes");
            println!("  cleanup          Archive leftover OpenClaw directories after migration");
            println!();
            println!("Run 'hermes claw <command> --help' for options.");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_script_candidates_non_empty() {
        let candidates = migration_script_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates.iter().any(|p| p.to_string_lossy().contains("openclaw_to_hermes.py")));
    }

    #[test]
    fn test_find_openclaw_dir_explicit_nonexistent() {
        assert!(find_openclaw_dir(Some("/nonexistent/path")).is_none());
    }

    #[test]
    fn test_openclaw_dir_names() {
        assert_eq!(OPENCLAW_DIR_NAMES, &[".openclaw", ".clawdbot", ".moltbot"]);
    }
}
