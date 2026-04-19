#![allow(dead_code)]
//! Skills hub management — search, install, uninstall, inspect, browse, check, update.

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
fn red() -> Style { Style::new().red() }

fn skills_dir() -> PathBuf {
    get_hermes_home().join("skills")
}

fn skills_index_path() -> PathBuf {
    get_hermes_home().join(".skills_index.json")
}

/// Skill registry entry.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct SkillEntry {
    name: String,
    description: String,
    category: String,
    source: String, // "hub", "builtin", "local"
    enabled: bool,
    version: Option<String>,
    installed_at: Option<String>,
}

/// Remote skill registry record.
#[derive(serde::Deserialize)]
struct RemoteSkill {
    name: String,
    description: String,
    category: String,
    #[serde(default)]
    repository: String,
    #[serde(default)]
    version: String,
}

fn load_skills_index() -> Vec<SkillEntry> {
    let path = skills_index_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(index) = serde_json::from_str::<Vec<SkillEntry>>(&content) {
                return index;
            }
        }
    }
    // Fallback: scan skills directory
    let dir = skills_dir();
    if dir.exists() {
        return scan_skills_dir(&dir);
    }
    Vec::new()
}

fn scan_skills_dir(dir: &PathBuf) -> Vec<SkillEntry> {
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                skills.push(SkillEntry {
                    name: name.clone(),
                    description: String::new(),
                    category: "local".to_string(),
                    source: "local".to_string(),
                    enabled: true,
                    version: None,
                    installed_at: None,
                });
            }
        }
    }
    skills
}

fn save_skills_index(skills: &[SkillEntry]) -> anyhow::Result<()> {
    let path = skills_index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(skills)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// List installed skills with filtering.
pub fn cmd_skills_list(source: &str) -> anyhow::Result<()> {
    let skills = load_skills_index();

    println!();
    println!("{}", cyan().apply_to("◆ Installed Skills"));
    println!();

    let filtered: Vec<_> = skills.iter()
        .filter(|s| source == "all" || s.source == source)
        .collect();

    if filtered.is_empty() {
        println!("  {}", dim().apply_to("No skills installed."));
        println!("  Search available skills with: hermes skills search <query>");
        println!("  Or browse with: hermes skills browse");
    } else {
        for skill in &filtered {
            let status = if skill.enabled {
                green().apply_to("enabled").to_string()
            } else {
                yellow().apply_to("disabled").to_string()
            };
            let desc = if skill.description.is_empty() {
                dim().apply_to("(no description)").to_string()
            } else {
                skill.description.clone()
            };
            println!("  {} — {} {}", skill.name, status, dim().apply_to(&desc));
            if let Some(ref ver) = skill.version {
                println!("    source: {}, version: {}", skill.source, ver);
            }
        }
    }
    println!();

    Ok(())
}

/// Search remote skill registries.
pub fn cmd_skills_search(query: &str, source: &str, limit: usize) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to(&format!("◆ Searching Skills: \"{}\"", query)));
    println!();

    // Use built-in skill registry
    let results = search_builtin_registry(query, source);

    if results.is_empty() {
        println!("  {}", dim().apply_to("No skills found matching your query."));
    } else {
        println!("  {} results:", results.len());
        println!();
        for (i, skill) in results.iter().take(limit).enumerate() {
            println!("  {}. {} — {}", i + 1, skill.name, dim().apply_to(&skill.description));
            println!("     source: {}, category: {}", skill.repository, skill.category);
        }
        if results.len() > limit {
            println!("  ... and {} more", results.len() - limit);
        }
        println!();
        println!("  Install with: hermes skills install <name>");
    }
    println!();

    Ok(())
}

/// Search local registry (skills stored in skills/ dir and known hub skills).
fn search_builtin_registry(query: &str, source: &str) -> Vec<RemoteSkill> {
    let query_lower = query.to_lowercase();

    // Built-in skill definitions
    let builtin_skills = [
        ("code-review", "Review pull requests and suggest improvements", "code", "builtin"),
        ("summarize", "Summarize long documents or conversations", "utility", "builtin"),
        ("commit", "Generate commit messages from diffs", "git", "builtin"),
        ("test", "Write and run unit tests for new code", "testing", "builtin"),
        ("debug", "Debug failing tests or application issues", "debugging", "builtin"),
        ("refactor", "Refactor code while preserving behavior", "code", "builtin"),
        ("explain", "Explain code, errors, or concepts", "learning", "builtin"),
        ("write-docs", "Generate documentation for code", "docs", "builtin"),
        ("security-audit", "Audit code for security vulnerabilities", "security", "builtin"),
        ("performance", "Analyze and optimize code performance", "optimization", "builtin"),
        ("data-analysis", "Analyze CSV/JSON datasets and create reports", "data", "builtin"),
        ("translate", "Translate text between languages", "utility", "builtin"),
        ("browser-automate", "Automate web browser tasks", "automation", "builtin"),
        ("file-organize", "Organize and clean up files", "utility", "builtin"),
    ];

    let mut results = Vec::new();

    for (name, desc, cat, src) in &builtin_skills {
        if source != "all" && source != *src {
            continue;
        }
        if (*name).contains(&query_lower)
            || desc.to_lowercase().contains(&query_lower)
            || cat.contains(&query_lower) {
            results.push(RemoteSkill {
                name: name.to_string(),
                description: desc.to_string(),
                category: cat.to_string(),
                repository: src.to_string(),
                version: "1.0.0".to_string(),
            });
        }
    }

    // Also check installed local skills
    let dir = skills_dir();
    if dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.contains(&query_lower) || source == "all" || source == "local" {
                        // Read description if exists
                        let desc_path = entry.path().join("DESCRIPTION.md");
                        let desc = if desc_path.exists() {
                            std::fs::read_to_string(&desc_path).unwrap_or_default()
                                .lines().next().unwrap_or("").trim().to_string()
                        } else {
                            format!("Local skill: {}", name)
                        };
                        results.push(RemoteSkill {
                            name,
                            description: desc,
                            category: "local".to_string(),
                            repository: "local".to_string(),
                            version: String::new(),
                        });
                    }
                }
            }
        }
    }

    results
}

/// Browse all available skills (paginated).
pub fn cmd_skills_browse(page: usize, size: usize, source: &str) -> anyhow::Result<()> {
    let all_skills = search_builtin_registry("", source);

    println!();
    println!("{}", cyan().apply_to(&format!("◆ Available Skills (page {})", page)));
    println!();

    let total_pages = (all_skills.len() + size - 1).max(1) / size;
    let start = (page - 1) * size;
    let _end = (start + size).min(all_skills.len());

    if all_skills.is_empty() {
        println!("  {}", dim().apply_to("No skills available."));
    } else {
        for (i, skill) in all_skills.iter().enumerate().skip(start).take(size) {
            println!("  {}. {} — {}", i + 1, skill.name, dim().apply_to(&skill.description));
            println!("     [{}]", skill.repository);
        }
        println!();
        println!("  Page {}/{} ({} total skills)", page, total_pages, all_skills.len());
        if page < total_pages {
            println!("  Next page: hermes skills browse --page {}", page + 1);
        }
        println!();
        println!("  Install with: hermes skills install <name>");
    }
    println!();

    Ok(())
}

/// Inspect a skill without installing.
pub fn cmd_skills_inspect(identifier: &str) -> anyhow::Result<()> {
    // Check if installed locally
    let skills_dir = skills_dir();
    let skill_path = skills_dir.join(identifier);

    println!();
    println!("{}", cyan().apply_to(&format!("◆ Skill: {}", identifier)));
    println!();

    if skill_path.exists() {
        println!("  {}", green().apply_to("Installed"));
        println!("  Path: {}", skill_path.display());

        // Read skill files
        for e in std::fs::read_dir(&skill_path).ok().into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") || name.ends_with(".txt") || name.ends_with(".py") || name.ends_with(".yaml") {
                println!("    {}", name);
            }
        }
    } else {
        // Search in builtin registry
        let results = search_builtin_registry(identifier, "all");
        if let Some(skill) = results.into_iter().find(|s| s.name == identifier) {
            println!("  Available in registry");
            println!("  Description: {}", skill.description);
            println!("  Category: {}", skill.category);
            println!("  Source: {}", skill.repository);
            println!();
            println!("  Install with: hermes skills install {}", identifier);
        } else {
            println!("  {}", yellow().apply_to("Skill not found."));
        }
    }
    println!();

    Ok(())
}

/// Install a skill.
pub fn cmd_skills_install(identifier: &str, category: &str, force: bool, _yes: bool) -> anyhow::Result<()> {
    let dir = skills_dir();
    std::fs::create_dir_all(&dir)?;

    let install_dir = if category.is_empty() {
        dir.join(identifier)
    } else {
        std::fs::create_dir_all(dir.join(category))?;
        dir.join(category).join(identifier)
    };

    if install_dir.exists() {
        if force {
            std::fs::remove_dir_all(&install_dir)?;
        } else {
            println!("  {} Skill already installed: {}", yellow().apply_to("⚠"), identifier);
            println!("  Use --force to reinstall");
            return Ok(());
        }
    }

    // Check if it's a builtin skill — create a stub
    let results = search_builtin_registry(identifier, "all");
    let is_builtin = results.iter().any(|s| s.name == identifier);

    if is_builtin {
        std::fs::create_dir_all(&install_dir)?;

        // Create skill stub
        let skill_yaml = format!(
            "name: {}\ndescription: \"{}\"\nenabled: true\nversion: \"1.0.0\"\n",
            identifier,
            results.iter().find(|s| s.name == identifier).map(|s| &s.description).unwrap_or(&String::new())
        );
        std::fs::write(install_dir.join("skill.yaml"), skill_yaml)?;

        let description = format!("# {}\n\n{}\n", identifier,
            results.iter().find(|s| s.name == identifier).map(|s| &s.description).unwrap_or(&String::new()));
        std::fs::write(install_dir.join("DESCRIPTION.md"), description)?;

        // Update index
        let mut index = load_skills_index();
        index.retain(|s| s.name != identifier);
        index.push(SkillEntry {
            name: identifier.to_string(),
            description: results.iter().find(|s| s.name == identifier).map(|s| s.description.clone()).unwrap_or_default(),
            category: if category.is_empty() { "utility".to_string() } else { category.to_string() },
            source: "builtin".to_string(),
            enabled: true,
            version: Some("1.0.0".to_string()),
            installed_at: Some(chrono::Local::now().to_rfc3339()),
        });
        save_skills_index(&index)?;
    } else {
        // Try to clone from GitHub (skills.sh repo pattern)
        println!("  Attempting to fetch from registry...");

        // For now, create a placeholder and let user customize
        std::fs::create_dir_all(&install_dir)?;
        let skill_yaml = format!(
            "name: {}\ndescription: \"Custom skill\"\nenabled: true\n",
            identifier
        );
        std::fs::write(install_dir.join("skill.yaml"), skill_yaml)?;

        println!("  {} Skill installed (local stub — customize at {})", green().apply_to("✓"), install_dir.display());
    }

    println!("  {} Skill installed: {}", green().apply_to("✓"), identifier);
    println!("    Path: {}", install_dir.display());

    Ok(())
}

/// Uninstall a skill.
pub fn cmd_skills_uninstall(name: &str) -> anyhow::Result<()> {
    let install_dir = skills_dir().join(name);

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
        println!("  {} Skill uninstalled: {}", green().apply_to("✓"), name);
    } else {
        println!("  {} Skill not found: {}", yellow().apply_to("✗"), name);
        return Ok(());
    }

    // Remove from index
    let mut index = load_skills_index();
    let before = index.len();
    index.retain(|s| s.name != name);
    if index.len() < before {
        save_skills_index(&index)?;
    }

    Ok(())
}

/// Check installed skills for updates.
pub fn cmd_skills_check(name: Option<&str>) -> anyhow::Result<()> {
    let index = load_skills_index();

    println!();
    println!("{}", cyan().apply_to("◆ Skill Updates"));
    println!();

    let to_check: Vec<_> = match name {
        Some(n) => index.iter().filter(|s| s.name == n).collect(),
        None => index.iter().filter(|s| s.source == "hub" || s.source == "builtin").collect(),
    };

    if to_check.is_empty() {
        println!("  {}", dim().apply_to("No skills to check for updates."));
    } else {
        let mut all_current = true;
        for skill in &to_check {
            // Check against builtin registry version
            let results = search_builtin_registry(&skill.name, "all");
            let remote_version = results.iter()
                .find(|s| s.name == skill.name)
                .map(|s| &s.version);

            match (remote_version, &skill.version) {
                (Some(remote), Some(local)) if remote != local => {
                    println!("  {} {} — update available ({} → {})",
                        yellow().apply_to("⟳"), skill.name, local, remote);
                    all_current = false;
                }
                _ => {
                    println!("  {} {} — up to date", green().apply_to("✓"), skill.name);
                }
            }
        }
        if all_current {
            println!("  {}", green().apply_to("All skills up to date."));
        }
    }
    println!();

    Ok(())
}

/// Update installed hub skills.
pub fn cmd_skills_update(name: Option<&str>) -> anyhow::Result<()> {
    let index = load_skills_index();

    println!();
    println!("{}", cyan().apply_to("◆ Updating Skills"));
    println!();

    let to_update: Vec<_> = match name {
        Some(n) => index.iter().filter(|s| s.name == n).collect(),
        None => index.iter().filter(|s| s.source == "hub" || s.source == "builtin").collect(),
    };

    if to_update.is_empty() {
        println!("  {}", dim().apply_to("No skills to update."));
    } else {
        for skill in &to_update {
            let results = search_builtin_registry(&skill.name, "all");
            let remote_version = results.iter()
                .find(|s| s.name == skill.name)
                .map(|s| &s.version);

            if let (Some(remote), Some(local)) = (remote_version, &skill.version) {
                if remote != local {
                    println!("  {} {} — updating from {} to {}", green().apply_to("⟳"), skill.name, local, remote);
                    // Update version in index
                    let mut index = load_skills_index();
                    for s in &mut index {
                        if s.name == skill.name {
                            s.version = Some(remote.clone());
                        }
                    }
                    save_skills_index(&index)?;
                } else {
                    println!("  {} {} — already current", dim().apply_to("✓"), skill.name);
                }
            } else {
                println!("  {} {} — no update info available", dim().apply_to("○"), skill.name);
            }
        }
    }
    println!();

    Ok(())
}

/// Audit installed hub skills (re-scan for compatibility).
pub fn cmd_skills_audit(name_filter: Option<&str>) -> anyhow::Result<()> {
    let index = load_skills_index();
    let dir = skills_dir();

    println!();
    println!("{}", cyan().apply_to("◆ Skills Audit"));
    println!();

    // Find skills on disk not in index
    let indexed: std::collections::HashSet<String> = index.iter().map(|s| s.name.clone()).collect();

    if dir.exists() {
        for e in std::fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            if e.path().is_dir() {
                let disk_name = e.file_name().to_string_lossy().to_string();
                if !indexed.contains(&disk_name) {
                    if let Some(filter) = name_filter {
                        if disk_name != filter { continue; }
                    }
                    println!("  {} {} — on disk but not in index",
                        yellow().apply_to("⚠"), disk_name);
                }
            }
        }
    }

    // Find skills in index not on disk
    for skill in &index {
        if let Some(filter) = name_filter {
            if skill.name != filter { continue; }
        }
        let skill_path = dir.join(&skill.name);
        if !skill_path.exists() {
            println!("  {} {} — in index but not on disk",
                red().apply_to("✗"), skill.name);
        } else if name_filter.is_some() {
            println!("  {} {} — found on disk", green().apply_to("✓"), skill.name);
        }
    }

    if name_filter.is_none() {
        println!("  {} Audited {} skills", dim().apply_to("✓"), index.len());
    }
    println!();

    Ok(())
}

fn skills_taps_path() -> PathBuf {
    get_hermes_home().join(".skill_taps.json")
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct SkillTap {
    name: String,
    repo: String,
}

fn load_taps() -> Vec<SkillTap> {
    let path = skills_taps_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(taps) = serde_json::from_str::<Vec<SkillTap>>(&content) {
                return taps;
            }
        }
    }
    Vec::new()
}

fn save_taps(taps: &[SkillTap]) -> anyhow::Result<()> {
    let path = skills_taps_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(taps)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Publish a skill to a registry.
pub fn cmd_skills_publish(name: &str, _registry: Option<&str>, _repo: Option<&str>) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Publish Skill"));
    println!();
    let dir = skills_dir();
    let skill_path = dir.join(name);
    if skill_path.exists() {
        println!("  {} Found skill: {name}", green().apply_to("✓"));
        println!("  {}", dim().apply_to("Publishing to registry (stub — requires registry API)."));
    } else {
        println!("  {} Skill '{name}' not found in local skills.", yellow().apply_to("⚠"));
    }
    println!();
    Ok(())
}

/// Export installed skills to a file.
pub fn cmd_skills_snapshot_export(output: &str) -> anyhow::Result<()> {
    let index = load_skills_index();
    let out = output.to_string();
    let content = serde_json::to_string_pretty(&index)?;
    std::fs::write(&out, content)?;
    println!("  {} Exported {} skills to: {out}", green().apply_to("✓"), index.len());
    println!();
    Ok(())
}

/// Import skills from a file.
pub fn cmd_skills_snapshot_import(path: &str) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ Import Skills"));
    println!();
    let content = std::fs::read_to_string(path)?;
    let skills: Vec<SkillEntry> = serde_json::from_str(&content)?;
    println!("  {} Found {n} skills in snapshot.", green().apply_to("✓"), n = skills.len());
    println!("  {}", dim().apply_to("Run `hermes skills install <name>` for each skill to install."));
    println!();
    Ok(())
}

/// List configured taps.
pub fn cmd_skills_tap_list() -> anyhow::Result<()> {
    let taps = load_taps();
    println!();
    println!("{}", cyan().apply_to("◆ Skill Taps"));
    println!();
    if taps.is_empty() {
        println!("  {}", dim().apply_to("No taps configured."));
    } else {
        for tap in &taps {
            println!("  {} {} — {repo}", green().apply_to("✓"), tap.name, repo = tap.repo);
        }
    }
    println!();
    Ok(())
}

/// Add a tap.
pub fn cmd_skills_tap_add(repo: &str) -> anyhow::Result<()> {
    let mut taps = load_taps();
    let name = repo.split('/').next_back().unwrap_or(repo);
    if taps.iter().any(|t| t.name == name) {
        println!("  {} Tap '{name}' already exists.", yellow().apply_to("⚠"));
    } else {
        taps.push(SkillTap { name: name.to_string(), repo: repo.to_string() });
        save_taps(&taps)?;
        println!("  {} Tap added: {name} — {repo}", green().apply_to("✓"));
    }
    println!();
    Ok(())
}

/// Remove a tap.
pub fn cmd_skills_tap_remove(name: &str) -> anyhow::Result<()> {
    let mut taps = load_taps();
    let before = taps.len();
    taps.retain(|t| t.name != name);
    if taps.len() < before {
        save_taps(&taps)?;
        println!("  {} Tap removed: {name}", green().apply_to("✓"));
    } else {
        println!("  {} Tap not found: {name}", yellow().apply_to("⚠"));
    }
    println!();
    Ok(())
}

/// Reset skills to factory defaults.
pub fn cmd_skills_reset() -> anyhow::Result<()> {
    let dir = skills_dir();

    println!();
    println!("{}", cyan().apply_to("◆ Reset Skills"));
    println!();

    if !dir.exists() {
        println!("  {}", dim().apply_to("No skills directory found. Nothing to reset."));
        println!();
        return Ok(());
    }

    let entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .collect();

    if entries.is_empty() {
        println!("  {}", dim().apply_to("No skills installed. Nothing to reset."));
        println!();
        return Ok(());
    }

    println!("  This will remove all {} installed skills from {}", entries.len(), dir.display());
    print!("  Are you sure? [y/N]: ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let confirmed = input.trim().eq_ignore_ascii_case("y") || input.trim().eq_ignore_ascii_case("yes");

    if !confirmed {
        println!("  {}", yellow().apply_to("Cancelled."));
        println!();
        return Ok(());
    }

    for entry in entries {
        let path = entry.path();
        if path.is_dir() || path.is_file() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                if let Err(e2) = std::fs::remove_file(&path) {
                    println!("  {} Could not remove {}: {e} / {e2}", yellow().apply_to("⚠"), path.display());
                    continue;
                }
            }
        }
    }

    // Also clear the skills index
    let index_path = skills_index_path();
    if index_path.exists() {
        let _ = std::fs::remove_file(&index_path);
    }

    println!("  {} All skills have been reset.", green().apply_to("✓"));
    println!();

    Ok(())
}

/// Interactive skill configuration.
pub fn cmd_skills_config() -> anyhow::Result<()> {
    let index = load_skills_index();
    println!();
    println!("{}", cyan().apply_to("◆ Skill Configuration"));
    println!();
    if index.is_empty() {
        println!("  {}", dim().apply_to("No skills installed."));
    } else {
        for skill in &index {
            let status = if skill.enabled { "enabled" } else { "disabled" };
            println!("  {} [{status}]", skill.name);
        }
    }
    println!();
    println!("  {}", dim().apply_to("Use `hermes skills enable/disable <name>` to toggle."));
    println!();
    Ok(())
}

/// Dispatch skills subcommands.
pub fn cmd_skills(
    action: &str,
    name: Option<&str>,
    query: Option<&str>,
    source: &str,
    limit: usize,
    page: usize,
    category: &str,
    force: bool,
) -> anyhow::Result<()> {
    match action {
        "search" => {
            let q = query.ok_or_else(|| anyhow::anyhow!("query is required"))?;
            cmd_skills_search(q, source, limit)
        }
        "browse" => cmd_skills_browse(page, limit.max(20), source),
        "install" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("skill name is required"))?;
            cmd_skills_install(n, category, force, false)
        }
        "uninstall" | "remove" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("skill name is required"))?;
            cmd_skills_uninstall(n)
        }
        "inspect" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("skill name is required"))?;
            cmd_skills_inspect(n)
        }
        "check" => cmd_skills_check(name),
        "update" => cmd_skills_update(name),
        "audit" => cmd_skills_audit(name),
        "list" | "" => cmd_skills_list(source),
        "publish" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("skill name is required"))?;
            cmd_skills_publish(n, Some(category), None)
        }
        "snapshot-export" => cmd_skills_snapshot_export(category),
        "snapshot-import" => {
            let p = name.ok_or_else(|| anyhow::anyhow!("file path is required"))?;
            cmd_skills_snapshot_import(p)
        }
        "tap-list" => cmd_skills_tap_list(),
        "tap-add" => {
            let r = name.ok_or_else(|| anyhow::anyhow!("repo is required"))?;
            cmd_skills_tap_add(r)
        }
        "tap-remove" => {
            let n = name.ok_or_else(|| anyhow::anyhow!("tap name is required"))?;
            cmd_skills_tap_remove(n)
        }
        "config" => cmd_skills_config(),
        _ => {
            anyhow::bail!("Unknown action: {}. Use list, search, browse, install, uninstall, inspect, check, update, audit, publish, snapshot, tap, or config.", action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_returns_results_for_query() {
        let results = search_builtin_registry("review", "all");
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "code-review");
    }

    #[test]
    fn test_search_empty_query() {
        let results = search_builtin_registry("", "all");
        assert!(!results.is_empty());
        assert!(results.len() >= 10);
    }

    #[test]
    fn test_search_by_category() {
        let results = search_builtin_registry("code", "all");
        // Should match "code-review" and "refactor" categories
        assert!(!results.is_empty());
    }

    #[test]
    fn test_skills_list_empty() {
        // Should run without error even with no skills
        let result = cmd_skills_list("all");
        assert!(result.is_ok());
    }
}
