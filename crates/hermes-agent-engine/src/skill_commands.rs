//! Skill slash-command discovery and invocation.
//!
//! Mirrors the Python `agent/skill_commands.py` so that both the CLI and
//! gateway surfaces can invoke skills via `/skill-name` commands.
//!
//! ## How it works
//!
//! 1. `scan_skill_commands()` walks `~/.hermes/skills/` (plus external skill
//!    directories) looking for `SKILL.md` files.
//! 2. Each `SKILL.md` is parsed for YAML frontmatter between `---` delimiters.
//! 3. Skills are filtered by platform and disabled-list, then stored in a
//!    module-level cache keyed by normalized slug (e.g. `/gif-search`).
//! 4. `build_skill_invocation_message()` loads the skill payload and formats
//!    the complete prompt that is sent to the model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde::Deserialize;

use hermes_core::get_hermes_home;

// ---------------------------------------------------------------------------
// Module-level cache
// ---------------------------------------------------------------------------

static SKILL_COMMANDS: Lazy<Mutex<HashMap<String, SkillCommand>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// Regex patterns for slug normalization.
static SKILL_INVALID_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9-]").unwrap());
static SKILL_MULTI_HYPHEN: Lazy<Regex> = Lazy::new(|| Regex::new(r"-{2,}").unwrap());

// ---------------------------------------------------------------------------
// Public structs
// ---------------------------------------------------------------------------

/// Metadata about a discovered skill slash-command.
#[derive(Debug, Clone)]
pub struct SkillCommand {
    /// Display name from frontmatter (e.g. "GIF Search").
    pub name: String,
    /// Normalized slug with leading slash (e.g. "/gif-search").
    pub slug: String,
    /// Short description for help text.
    pub description: String,
    /// Absolute path to the SKILL.md file.
    pub skill_md_path: PathBuf,
    /// Directory containing the skill.
    pub skill_dir: PathBuf,
    /// Platform restriction from frontmatter (e.g. "gateway", "cli").
    pub platform: Option<String>,
    /// Embedded Hermes config overrides from frontmatter.
    pub hermes_config: Option<serde_json::Value>,
}

/// The loaded content of a skill, ready to inject into a prompt.
#[derive(Debug, Clone)]
pub struct SkillPayload {
    /// Skill name from frontmatter.
    pub name: String,
    /// The body content of the skill (after frontmatter).
    pub content: String,
    /// Config overrides declared in frontmatter, if any.
    pub config: Option<serde_json::Value>,
    /// Directory containing the skill.
    pub skill_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Frontmatter data structures
// ---------------------------------------------------------------------------

/// Parsed YAML frontmatter from a SKILL.md file.
///
/// We use `serde_yaml::Value` for the raw map so we can handle arbitrary
/// keys, then extract the fields we care about.
#[derive(Debug, Default, Deserialize)]
struct FrontmatterRaw {
    name: Option<String>,
    description: Option<String>,
    platform: Option<String>,
    /// `hermes` is a catch-all for nested `hermes.config` etc.
    hermes: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate a plan-file path for a `/plan` invocation.
///
/// Returns `~/.hermes/plans/<timestamp>-<slug>.md`.
///
/// The slug is derived from the first line of `user_instruction`,
/// sanitized to alphanumeric + hyphens.
pub fn build_plan_path(user_instruction: &str, now: chrono::DateTime<chrono::Local>) -> PathBuf {
    let slug_source = user_instruction
        .lines()
        .next()
        .unwrap_or_default();
    let slug = normalize_slug_inner(slug_source);
    let slug = if slug.is_empty() {
        "conversation-plan".to_string()
    } else {
        // Limit slug length: take up to 8 parts, cap at 48 chars.
        let mut parts: Vec<&str> = slug.split('-').filter(|p| !p.is_empty()).collect();
        parts.truncate(8);
        let mut s = parts.join("-");
        if s.len() > 48 {
            // Truncate to 48 bytes, then strip trailing hyphen.
            s.truncate(48);
            s = s.trim_end_matches('-').to_string();
        }
        s
    };
    let slug = if slug.is_empty() {
        "conversation-plan".to_string()
    } else {
        slug
    };
    let timestamp = now.format("%Y-%m-%d_%H%M%S");
    get_hermes_home()
        .join("plans")
        .join(format!("{timestamp}-{slug}.md"))
}

/// Scan `~/.hermes/skills/` for SKILL.md files and populate the cache.
///
/// Walks the local skills directory plus any external skill directories.
/// For each `SKILL.md`, parses YAML frontmatter, checks platform match,
/// skips disabled skills, and normalizes the name into a command slug.
///
/// Returns a snapshot of the discovered commands.
pub fn scan_skill_commands() -> Result<HashMap<String, SkillCommand>> {
    let mut commands = HashMap::new();
    let skills_dir = get_hermes_home().join("skills");

    if !skills_dir.exists() {
        return Ok(commands);
    }

    let disabled = load_disabled_skills();
    let mut seen_names: HashMap<String, ()> = HashMap::new();

    for entry in walkdir::WalkDir::new(&skills_dir)
        .into_iter()
        .filter_entry(|e| !is_hidden_dir(e))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Only care about SKILL.md files.
        if entry.file_name() != "SKILL.md" {
            continue;
        }

        let skill_md_path = entry.path().to_path_buf();
        let skill_dir = skill_md_path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

        // Skip files inside .git, .github, .hub directories.
        if is_in_vcs_or_hub(&skill_md_path, &skills_dir) {
            continue;
        }

        let content = match std::fs::read_to_string(&skill_md_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", skill_md_path.display(), e);
                continue;
            }
        };

    let (frontmatter, body) = match parse_frontmatter(&content) {
            Ok((fm, b)) => (fm, b),
            Err(e) => {
                tracing::warn!("Failed to parse frontmatter in {}: {}", skill_md_path.display(), e);
                continue;
            }
        };

        // Platform filter.
        if let Some(platform) = &frontmatter.platform {
            if !platform_matches(platform) {
                continue;
            }
        }

        let name = frontmatter
            .name
            .clone()
            .unwrap_or_else(|| skill_dir.file_name().unwrap_or_default().to_string_lossy().to_string());

        // Deduplicate by name (first one wins).
        if seen_names.contains_key(&name) {
            continue;
        }

        // Disabled filter.
        if disabled.contains(&name) {
            continue;
        }

        seen_names.insert(name.clone(), ());

        // Fallback description from body if frontmatter has none.
        let description = frontmatter
            .description
            .clone()
            .filter(|d: &String| !d.is_empty())
            .unwrap_or_else(|| first_meaningful_line(&body).unwrap_or_default());

        let description = if description.is_empty() {
            format!("Invoke the {name} skill")
        } else {
            description
        };

        // Normalize slug.
        let cmd_name = normalize_slug_inner(&name);
        if cmd_name.is_empty() {
            continue;
        }

        // Extract hermes.config if present.
        let hermes_config = frontmatter
            .hermes
            .as_ref()
            .and_then(|v: &serde_json::Value| v.get("config"))
            .cloned();

        let slug = format!("/{}", cmd_name);
        commands.insert(
            slug.clone(),
            SkillCommand {
                name,
                slug,
                description,
                skill_md_path,
                skill_dir,
                platform: frontmatter.platform,
                hermes_config,
            },
        );
    }

    // Update the global cache.
    {
        let mut cache = SKILL_COMMANDS.lock();
        *cache = commands.clone();
    }

    Ok(commands)
}

/// Lazy accessor: returns the current skill-commands map, scanning first if
/// the cache is empty.
pub fn get_skill_commands() -> HashMap<String, SkillCommand> {
    let snapshot = {
        let cache = SKILL_COMMANDS.lock();
        cache.clone()
    };
    if snapshot.is_empty() {
        // Best-effort scan; ignore errors so caller still gets empty map.
        let _ = scan_skill_commands();
    }
    SKILL_COMMANDS.lock().clone()
}

/// Resolve a user-typed command (with or without `/`) to the canonical key.
///
/// Skills are always stored with hyphens. Hyphens and underscores are treated
/// interchangeably in user input to accommodate Telegram bot-command names
/// (which disallow hyphens, so `/claude_code` arrives in underscored form).
///
/// Returns the matching `/slug` key, or `None`.
pub fn resolve_skill_command_key(command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    // Normalize: strip leading `/`, replace underscores with hyphens.
    let cleaned = command.strip_prefix('/').unwrap_or(command);
    let cmd_key = format!("/{}", cleaned.replace('_', "-"));

    let cache = SKILL_COMMANDS.lock();
    if cache.contains_key(&cmd_key) {
        return Some(cmd_key);
    }
    None
}

/// Load a skill's JSON payload by slug.
///
/// Returns `Ok(None)` if the skill isn't found or can't be loaded.
/// The `SkillPayload` contains the name, body content, optional config, and
/// directory path.
pub fn load_skill_payload(slug: &str) -> Result<Option<SkillPayload>> {
    let commands = get_skill_commands();
    let skill_info = commands
        .get(slug)
        .ok_or_else(|| anyhow::anyhow!("Skill not found: {slug}"))?;

    let skill_md_path = &skill_info.skill_md_path;
    let content = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("Failed to read {}", skill_md_path.display()))?;

    let (_frontmatter, body) = parse_frontmatter(&content)
        .context("Failed to parse frontmatter")?;

    // Extract name and config from frontmatter again for the payload.
    let (fm, _body) = parse_frontmatter(&content).unwrap_or_default();
    let name = fm.name.clone().unwrap_or_else(|| skill_info.name.clone());

    Ok(Some(SkillPayload {
        name,
        content: body,
        config: skill_info.hermes_config.clone(),
        skill_dir: skill_info.skill_dir.clone(),
    }))
}

/// Build the complete user/system message for a skill invocation.
///
/// # Arguments
/// * `slug` — canonical command key with leading slash (e.g. `/gif-search`).
/// * `user_instruction` — text the user typed after the command.
/// * `task_id` — optional task identifier for downstream tracing.
/// * `runtime_note` — optional runtime note to append.
///
/// # Returns
/// `Ok(Some(message))` on success, `Ok(None)` if the skill wasn't found,
/// or `Err` on file/parsing failures.
pub fn build_skill_invocation_message(
    slug: &str,
    user_instruction: &str,
    task_id: &str,
    runtime_note: Option<&str>,
) -> Result<Option<String>> {
    let commands = get_skill_commands();
    let skill_info = match commands.get(slug) {
        Some(info) => info,
        None => return Ok(None),
    };

    let skill_md_path = &skill_info.skill_md_path;
    let content = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("Failed to read {}", skill_md_path.display()))?;

    let (_frontmatter, body) = parse_frontmatter(&content)
        .context("Failed to parse frontmatter")?;

    let skill_name = &skill_info.name;
    let skill_dir = &skill_info.skill_dir;

    // Build the activation note.
    let activation_note = format!(
        "[SYSTEM: The user has invoked the \"{skill_name}\" skill, indicating they want \
         you to follow its instructions. The full skill content is loaded below.]"
    );

    // Assemble the message parts.
    let mut parts: Vec<String> = vec![
        activation_note,
        String::new(),
        body.trim().to_string(),
    ];

    // Inject skill config values if present.
    if let Some(config) = &skill_info.hermes_config {
        let config_block = format_skill_config_block(config);
        parts.push(config_block);
    }

    // User instruction.
    if !user_instruction.is_empty() {
        parts.push(String::new());
        parts.push(format!(
            "The user has provided the following instruction alongside the skill invocation: {user_instruction}"
        ));
    }

    // Runtime note.
    if let Some(note) = runtime_note {
        if !note.is_empty() {
            parts.push(String::new());
            parts.push(format!("[Runtime note: {note}]"));
        }
    }

    // Mention supporting files.
    if let Some(supporting) = list_supporting_files(skill_dir) {
        parts.push(String::new());
        parts.push(
            "[This skill has supporting files you can load with the skill_view tool:]".to_string(),
        );
        for sf in &supporting {
            parts.push(format!("- {sf}"));
        }
        // Build a relative skill-view target.
        let skills_dir = get_hermes_home().join("skills");
        let view_target = if let Ok(rel) = skill_dir.strip_prefix(&skills_dir) {
            rel.to_string_lossy().to_string()
        } else {
            skill_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| skill_name.clone())
        };
        parts.push(format!(
            "\nTo view any of these, use: skill_view(name=\"{view_target}\", file_path=\"<path>\")"
        ));
    }

    // Task id note.
    if !task_id.is_empty() {
        parts.push(String::new());
        parts.push(format!("[Task ID: {task_id}]"));
    }

    Ok(Some(parts.join("\n")))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Normalize a name into a hyphen-separated slug (no leading `/`).
///
/// - Lowercase
/// - Spaces and underscores become hyphens
/// - Strip non-alphanumeric characters except hyphens
/// - Collapse consecutive hyphens
/// - Trim leading/trailing hyphens
fn normalize_slug_inner(name: &str) -> String {
    let mut slug = name.to_lowercase();
    slug = slug.replace([' ', '_'], "-");
    // Keep only lowercase alphanumerics and hyphens.
    slug = SKILL_INVALID_CHARS
        .replace_all(&slug, "")
        .to_string();
    // Collapse multiple hyphens.
    slug = SKILL_MULTI_HYPHEN
        .replace_all(&slug, "-")
        .to_string();
    slug.trim_matches('-').to_string()
}

/// Parse YAML frontmatter from a SKILL.md string.
///
/// Looks for content between the first pair of `---` delimiters at the start
/// of the file. Returns `(frontmatter_struct, body)` where body is everything
/// after the closing `---`.
fn parse_frontmatter(content: &str) -> Result<(FrontmatterRaw, String)> {
    let trimmed = content.trim_start();

    // Must start with ---
    if !trimmed.starts_with("---") {
        return Ok((FrontmatterRaw::default(), content.to_string()));
    }

    // Find the second ---
    let rest = &trimmed[3..]; // skip first "---"
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_section = &rest[..end_idx];
        let body = &rest[end_idx + 4..]; // skip "\n---"

        let fm: FrontmatterRaw =
            serde_yaml::from_str(yaml_section).unwrap_or_default();
        return Ok((fm, body.to_string()));
    }

    // No closing --- found — treat entire content as body.
    Ok((FrontmatterRaw::default(), content.to_string()))
}

/// Check whether a path is inside a VCS or `.hub` directory.
fn is_in_vcs_or_hub(path: &Path, base: &Path) -> bool {
    path.strip_prefix(base)
        .ok()
        .map(|rel| {
            rel.components().any(|c| {
                let name = c.as_os_str().to_string_lossy();
                matches!(name.as_ref(), ".git" | ".github" | ".hub")
            })
        })
        .unwrap_or(false)
}

/// Skip hidden directories during walkdir traversal.
fn is_hidden_dir(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Check if a platform string matches the current runtime.
///
/// Supports comma-separated values like "cli, gateway" or a single value.
fn platform_matches(platform: &str) -> bool {
    if platform.is_empty() {
        return true;
    }
    // For simplicity, accept all — platform filtering is typically handled
    // at a higher level (gateway vs CLI dispatch).  A more sophisticated
    // implementation could check env vars or feature flags.
    let _ = platform;
    true
}

/// Load the list of disabled skill names from config.
///
/// Returns an empty set if the config file doesn't exist or can't be parsed.
fn load_disabled_skills() -> std::collections::HashSet<String> {
    let config_path = get_hermes_home().join("config.yaml");
    let mut disabled = std::collections::HashSet::new();

    if let Ok(content) = std::fs::read_to_string(&config_path) {
        // Try to parse disabled skills from the YAML config.
        // Expected shape: `{ skills: { disabled: ["name1", "name2"] } }`
        if let Ok(config) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            if let Some(list) = config
                .get("skills")
                .and_then(|s| s.get("disabled"))
                .and_then(|d| d.as_sequence())
            {
                for item in list {
                    if let Some(name) = item.as_str() {
                        disabled.insert(name.to_string());
                    }
                }
            }
        }
    }

    disabled
}

/// Extract the first non-empty, non-heading line from the skill body.
fn first_meaningful_line(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return Some(trimmed.chars().take(80).collect());
    }
    None
}

/// Format skill config values into a readable block for the model.
fn format_skill_config_block(config: &serde_json::Value) -> String {
    let mut lines = vec![
        String::new(),
        "[Skill config (from ~/.hermes/config.yaml):".to_string(),
    ];

    if let Some(obj) = config.as_object() {
        for (key, value) in obj {
            let display_val = match value {
                serde_json::Value::Null => "(not set)".to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push(format!("  {key} = {display_val}"));
        }
    } else {
        lines.push(format!("  {}", config));
    }

    lines.push("]".to_string());
    lines.join("\n")
}

/// List supporting files under standard subdirectories.
///
/// Looks in `references/`, `templates/`, `scripts/`, `assets/` under
/// `skill_dir`.
fn list_supporting_files(skill_dir: &Path) -> Option<Vec<String>> {
    let mut files = Vec::new();

    for subdir in &["references", "templates", "scripts", "assets"] {
        let subdir_path = skill_dir.join(subdir);
        if subdir_path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&subdir_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        if let Ok(rel) = path.strip_prefix(skill_dir) {
                            files.push(rel.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }

    if files.is_empty() {
        None
    } else {
        files.sort();
        Some(files)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_slug_basic() {
        assert_eq!(normalize_slug_inner("GIF Search"), "gif-search");
        assert_eq!(normalize_slug_inner("gif_search"), "gif-search");
        assert_eq!(normalize_slug_inner("gif  search"), "gif-search");
        assert_eq!(normalize_slug_inner("GIF--Search"), "gif-search");
    }

    #[test]
    fn test_normalize_slug_strips_invalid() {
        assert_eq!(normalize_slug_inner("My+Skill"), "myskill");
        assert_eq!(normalize_slug_inner("a/b/c"), "abc");
        assert_eq!(normalize_slug_inner("test!@#$%skill"), "testskill");
    }

    #[test]
    fn test_normalize_slug_edge_cases() {
        assert_eq!(normalize_slug_inner(""), "");
        assert_eq!(normalize_slug_inner("   "), "");
        assert_eq!(normalize_slug_inner("---"), "");
        assert_eq!(normalize_slug_inner("a"), "a");
    }

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = r#"---
name: Test Skill
description: A test skill
---

# Test Skill

Content here.
"#;
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name.as_deref(), Some("Test Skill"));
        assert_eq!(fm.description.as_deref(), Some("A test skill"));
        assert!(body.contains("# Test Skill"));
    }

    #[test]
    fn test_parse_frontmatter_no_delimiters() {
        let content = "# Just a markdown file";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert!(fm.name.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_frontmatter_no_closing() {
        let content = "---\nname: orphan\n\nbody text";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert!(fm.name.is_none()); // no closing --- so entire thing is body
        assert_eq!(body, content);
    }

    #[test]
    fn test_build_plan_path_basic() {
        let now = chrono::Local::now();
        let path = build_plan_path("build a todo app", now);
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(filename.contains("todo-app"));
        assert!(filename.ends_with(".md"));
    }

    #[test]
    fn test_build_plan_path_empty_instruction() {
        let now = chrono::Local::now();
        let path = build_plan_path("", now);
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(filename.contains("conversation-plan"));
    }

    #[test]
    fn test_build_plan_path_long_instruction() {
        let now = chrono::Local::now();
        let instruction = "this is a very long instruction that should be truncated to fit within reasonable filename limits";
        let path = build_plan_path(instruction, now);
        let filename = path.file_name().unwrap().to_string_lossy();
        // Slug should be limited to 48 chars + timestamp prefix.
        assert!(filename.len() < 80);
    }
}
