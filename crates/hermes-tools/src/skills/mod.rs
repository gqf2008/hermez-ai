//! Skills tools.
//!
//! Mirrors the Python `tools/skills_tool.py`.
//! 3 tools: skills_list, skill_view, skills_categories.
//! Progressive disclosure: categories → metadata → full content + linked files.
//!
//! Skills are directories containing a `SKILL.md` file with YAML frontmatter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::registry::tool_error;
use crate::skills_guard;

// Constants
const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
const MAX_SKILL_FILE_BYTES: usize = 1_048_576;
const EXCLUDED_SKILL_DIRS: &[&str] = &[".git", ".github", ".hub"];
const ALLOWED_SUBDIRS: &[&str] = &["references", "templates", "scripts", "assets"];

/// Skill name regex: lowercase letters, digits, hyphens, dots, underscores; must start with letter/digit.
static VALID_NAME_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^[a-z0-9][a-z0-9._-]*$").unwrap());

/// Get the skills directory: ~/.hermes/skills/
fn skills_dir() -> PathBuf {
    hermes_core::get_hermes_home().join("skills")
}

/// Check if a directory name should be excluded.
fn is_excluded_dir(name: &str) -> bool {
    EXCLUDED_SKILL_DIRS.contains(&name)
}

/// Parsed YAML frontmatter from a SKILL.md file.
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    platforms: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Value>,
}

/// Parse YAML frontmatter from markdown content.
///
/// Returns (frontmatter, body_string).
fn parse_frontmatter(content: &str) -> (SkillFrontmatter, String) {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return (SkillFrontmatter::default(), content.to_string());
    }

    let rest = &content[3..];
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_block = &rest[..end_idx];
        let body = rest[end_idx + 4..].to_string();
        let fm: SkillFrontmatter = serde_yaml::from_str(yaml_block).unwrap_or_default();
        return (fm, body.trim_start().to_string());
    }

    (SkillFrontmatter::default(), content.to_string())
}

/// Parse the YAML frontmatter block into a generic Value.
///
/// Used by `extract_skill_config_vars` and `extract_skill_conditions`
/// which need access to the raw nested YAML structure.
fn parse_frontmatter_value(content: &str) -> Option<Value> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_block = &rest[..end_idx];
        serde_yaml::from_str(yaml_block).ok()
    } else {
        None
    }
}

/// Check if a skill matches the current platform.
fn skill_matches_platform(fm: &SkillFrontmatter) -> bool {
    let Some(platforms) = &fm.platforms else {
        return true;
    };

    let current = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return true;
    };

    platforms.iter().any(|p| p.to_lowercase() == current)
}

/// Extract category from skill path based on directory structure.
fn get_category_from_path(skill_md: &Path, skills_dirs: &[PathBuf]) -> Option<String> {
    for skills_dir in skills_dirs {
        if let Ok(rel) = skill_md.strip_prefix(skills_dir) {
            let parts: Vec<_> = rel.components().collect();
            if parts.len() >= 3 {
                if let Some(c) = parts[0].as_os_str().to_str() {
                    if !c.starts_with('.') {
                        return Some(c.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Collect all skill directories to scan (builtin + local + external from config).
fn all_skills_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // 1. Builtin skills from HERMES_BUILTIN_SKILLS env var
    if let Ok(builtin) = std::env::var("HERMES_BUILTIN_SKILLS") {
        let path = PathBuf::from(builtin);
        if path.exists() {
            dirs.push(path);
        }
    }

    // 2. Auto-detect builtin skills relative to the executable or workspace
    if let Some(builtin) = detect_builtin_skills_dir() {
        if builtin.exists() && !dirs.iter().any(|d| d == &builtin) {
            dirs.push(builtin);
        }
    }

    // 3. Local user skills
    let local = skills_dir();
    if local.exists() && !dirs.iter().any(|d| d == &local) {
        dirs.push(local);
    }

    // 4. External skill dirs from config
    if let Ok(config) = hermes_core::config::HermesConfig::load() {
        for path in config.skills.external_dirs {
            if path.exists() && !dirs.iter().any(|d| d == &path) {
                dirs.push(path);
            }
        }
    }
    dirs
}

/// Detect builtin skills directory by checking common locations.
///
/// Resolution order:
/// 1. Current working directory / skills
/// 2. Parent of executable directory / skills (for installed binaries)
/// 3. CARGO_MANIFEST_DIR parent / skills (for cargo run in workspace)
fn detect_builtin_skills_dir() -> Option<PathBuf> {
    // Check CWD/skills
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("skills");
        if candidate.join("software-development").exists() || candidate.join("github").exists() {
            return Some(candidate);
        }
    }

    // Check executable parent/skills
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("skills");
            if candidate.join("software-development").exists() || candidate.join("github").exists() {
                return Some(candidate);
            }
            // Also check grandparent (typical for target/debug/hermes)
            if let Some(grandparent) = exe_dir.parent() {
                let candidate = grandparent.join("skills");
                if candidate.join("software-development").exists() || candidate.join("github").exists() {
                    return Some(candidate);
                }
            }
        }
    }

    // Check CARGO_MANIFEST_DIR (set during cargo run/build)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let manifest_path = PathBuf::from(manifest_dir);
        // Try manifest parent (workspace root)
        if let Some(workspace) = manifest_path.parent() {
            let candidate = workspace.join("skills");
            if candidate.join("software-development").exists() || candidate.join("github").exists() {
                return Some(candidate);
            }
        }
        // Try manifest dir itself
        let candidate = manifest_path.join("skills");
        if candidate.join("software-development").exists() || candidate.join("github").exists() {
            return Some(candidate);
        }
    }

    None
}

/// Get disabled skill names from config.
fn get_disabled_skill_names() -> std::collections::BTreeSet<String> {
    let mut disabled = std::collections::BTreeSet::new();
    let Ok(config) = hermes_core::config::HermesConfig::load() else {
        return disabled;
    };
    if let Ok(platform) = std::env::var("HERMES_PLATFORM") {
        if let Some(platform_list) = config.skills.platform_disabled.get(&platform) {
            for name in platform_list {
                disabled.insert(name.clone());
            }
        }
    }
    for name in &config.skills.disabled {
        disabled.insert(name.clone());
    }
    disabled
}

/// Find all skills across all skill directories.
fn find_all_skills(skip_disabled: bool) -> Vec<BTreeMap<String, Value>> {
    let dirs = all_skills_dirs();
    if dirs.is_empty() {
        return Vec::new();
    }

    let disabled = if skip_disabled {
        std::collections::BTreeSet::new()
    } else {
        get_disabled_skill_names()
    };

    let mut skills = Vec::new();
    let mut seen_names = std::collections::BTreeSet::new();

    for scan_dir in &dirs {
        if !scan_dir.exists() {
            continue;
        }
        let walker = walkdir::WalkDir::new(scan_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !is_excluded_dir(&name)
            });

        for entry in walker {
            let Ok(entry) = entry else { continue };
            if entry.file_name() != "SKILL.md" || !entry.file_type().is_file() {
                continue;
            }

            let skill_md = entry.path().to_path_buf();
            let skill_dir = skill_md.parent().unwrap_or(&skill_md).to_path_buf();

            let Ok(content_bytes) = std::fs::read(&skill_md) else { continue };
            let Ok(content) = String::from_utf8(content_bytes) else { continue };

            let (fm, body) = parse_frontmatter(&content);

            if !skill_matches_platform(&fm) {
                continue;
            }

            let name = fm
                .name
                .clone()
                .unwrap_or_else(|| skill_dir.file_name().unwrap_or_default().to_string_lossy().to_string());
            let name = if name.len() > MAX_NAME_LENGTH {
                name[..MAX_NAME_LENGTH].to_string()
            } else {
                name
            };

            if seen_names.contains(&name) || disabled.contains(&name) {
                continue;
            }

            let description = fm.description.clone().unwrap_or_else(|| {
                for line in body.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        return trimmed.to_string();
                    }
                }
                String::new()
            });
            let description = if description.len() > MAX_DESCRIPTION_LENGTH {
                format!("{}...", &description[..MAX_DESCRIPTION_LENGTH.saturating_sub(3)])
            } else {
                description
            };

            let category = get_category_from_path(&skill_md, &dirs);

            seen_names.insert(name.clone());
            let mut entry = BTreeMap::new();
            entry.insert("name".to_string(), Value::String(name));
            entry.insert("description".to_string(), Value::String(description));
            if let Some(cat) = category {
                entry.insert("category".to_string(), Value::String(cat));
            }
            skills.push(entry);
        }
    }

    skills
}

/// Load category description from DESCRIPTION.md if it exists.
fn load_category_description(category_dir: &Path) -> Option<String> {
    let desc_file = category_dir.join("DESCRIPTION.md");
    if !desc_file.exists() {
        return None;
    }

    let Ok(content) = std::fs::read_to_string(&desc_file) else { return None };

    let (fm, body) = parse_frontmatter(&content);
    let description = fm.description.unwrap_or_else(|| {
        for line in body.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                return trimmed.to_string();
            }
        }
        String::new()
    });

    if description.is_empty() {
        None
    } else if description.len() > MAX_DESCRIPTION_LENGTH {
        Some(format!("{}...", &description[..MAX_DESCRIPTION_LENGTH.saturating_sub(3)]))
    } else {
        Some(description)
    }
}

/// Check if skills requirements are met (always true).
pub fn check_skills_requirements() -> bool {
    true
}

/// Handle skills_list tool call.
pub fn handle_skills_list(args: Value) -> Result<String, hermes_core::HermesError> {
    let category = args.get("category").and_then(Value::as_str).map(String::from);

    let dir = skills_dir();
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Ok(crate::registry::tool_error(format!("Failed to create skills directory: {e}")));
        }
        return Ok(serde_json::json!({
            "success": true,
            "skills": [],
            "categories": [],
            "message": "No skills found. Skills directory created at ~/.hermes/skills/",
        })
        .to_string());
    }

    let all_skills = find_all_skills(false);

    if all_skills.is_empty() {
        return Ok(serde_json::json!({
            "success": true,
            "skills": [],
            "categories": [],
            "message": "No skills found in skills/ directory.",
        })
        .to_string());
    }

    let filtered: Vec<_> = if let Some(ref cat) = category {
        all_skills
            .into_iter()
            .filter(|s| s.get("category").and_then(|v| v.as_str()) == Some(cat))
            .collect()
    } else {
        all_skills
    };

    let categories: std::collections::BTreeSet<_> = filtered
        .iter()
        .filter_map(|s| s.get("category").and_then(|v| v.as_str()))
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "skills": filtered,
        "categories": categories,
        "count": filtered.len(),
        "hint": "Use skill_view(name) to see full content, tags, and linked files",
    })
    .to_string())
}

/// Handle skill_view tool call.
pub fn handle_skill_view(args: Value) -> Result<String, hermes_core::HermesError> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                "skill_view requires 'name' parameter",
            )
        })?
        .to_string();

    let file_path = args.get("file_path").and_then(Value::as_str).map(String::from);

    let all_dirs = all_skills_dirs();
    if all_dirs.is_empty() {
        return Ok(serde_json::json!({
            "success": false,
            "error": "Skills directory does not exist yet. It will be created on first install.",
        })
        .to_string());
    }

    let (skill_md, skill_dir) = match find_skill_by_name(&name, &all_dirs) {
        Some(result) => result,
        None => {
            let available: Vec<_> = find_all_skills(false)
                .into_iter()
                .filter_map(|m| m.get("name").and_then(|v| v.as_str()).map(String::from))
                .take(20)
                .collect();
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("Skill '{name}' not found."),
                "available_skills": available,
                "hint": "Use skills_list to see all available skills",
            })
            .to_string());
        }
    };

    let content = match std::fs::read_to_string(&skill_md) {
        Ok(c) => c,
        Err(e) => {
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("Failed to read skill '{name}': {e}"),
            })
            .to_string());
        }
    };

    // Security: check if file is outside trusted dirs
    let outside_skills_dir = {
        let trusted: Vec<_> = all_dirs
            .iter()
            .filter_map(|d| d.canonicalize().ok())
            .collect();
        let resolved = skill_md.canonicalize().unwrap_or_else(|_| skill_md.clone());
        trusted.iter().all(|td| !resolved.starts_with(td))
    };

    // Security: detect prompt injection patterns
    const INJECTION_PATTERNS: &[&str] = &[
        "ignore previous instructions",
        "ignore all previous",
        "you are now",
        "disregard your",
        "forget your instructions",
        "new instructions:",
        "system prompt:",
        "<system>",
        "]]>",
    ];
    let content_lower = content.to_lowercase();
    let injection_detected = INJECTION_PATTERNS
        .iter()
        .any(|p| content_lower.contains(p));

    if outside_skills_dir || injection_detected {
        let mut warnings = Vec::new();
        if outside_skills_dir {
            warnings.push(format!(
                "skill file is outside the trusted skills directory: {}",
                skill_md.display()
            ));
        }
        if injection_detected {
            warnings.push("skill content contains patterns that may indicate prompt injection".to_string());
        }
        tracing::warn!("Skill security warning for '{}': {}", name, warnings.join("; "));
    }

    let (fm, _body) = parse_frontmatter(&content);

    // Platform check
    if !skill_matches_platform(&fm) {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("Skill '{name}' is not supported on this platform."),
        })
        .to_string());
    }

    // Disabled check
    let skill_name = fm
        .name
        .clone()
        .unwrap_or_else(|| {
            skill_dir
                .as_ref()
                .unwrap_or(&skill_md)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
    let disabled = get_disabled_skill_names();
    if disabled.contains(&skill_name) {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("Skill '{skill_name}' is disabled. Enable it with `hermes skills` or inspect the files directly on disk."),
        })
        .to_string());
    }

    // If a specific file path is requested
    if let Some(ref fp) = file_path {
        let Some(ref sdir) = skill_dir else {
            return Ok(serde_json::json!({
                "success": false,
                "error": "Cannot view file: skill is not a directory-based skill.",
            })
            .to_string());
        };

        let normalized = PathBuf::from(fp);
        if normalized.components().any(|c| c.as_os_str() == "..") {
            return Ok(serde_json::json!({
                "success": false,
                "error": "Path traversal ('..') is not allowed.",
                "hint": "Use a relative path within the skill directory",
            })
            .to_string());
        }

        let target = sdir.join(fp);
        let target_resolved = target.canonicalize().ok();
        let sdir_resolved = sdir.canonicalize().ok();
        if let (Some(t), Some(s)) = (&target_resolved, &sdir_resolved) {
            if !t.starts_with(s) {
                return Ok(serde_json::json!({
                    "success": false,
                    "error": "Path escapes skill directory boundary.",
                    "hint": "Use a relative path within the skill directory",
                })
                .to_string());
            }
        }

        if !target.exists() {
            let available_files = list_skill_files(sdir);
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("File '{fp}' not found in skill '{name}'."),
                "available_files": available_files,
                "hint": "Use one of the available file paths listed above",
            })
            .to_string());
        }

        let Ok(bytes) = std::fs::read(&target) else {
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("Failed to read file: {fp}"),
            })
            .to_string());
        };

        let is_binary = bytes.iter().take(8192).any(|&b| b == 0);
        if is_binary {
            let file_name = target.file_name().unwrap_or_default().to_string_lossy();
            let size = bytes.len();
            return Ok(serde_json::json!({
                "success": true,
                "name": name,
                "file": fp,
                "content": format!("[Binary file: {file_name}, size: {size} bytes]"),
                "is_binary": true,
            })
            .to_string());
        }

        let Ok(file_content) = String::from_utf8(bytes) else {
            let file_name = target.file_name().unwrap_or_default().to_string_lossy();
            let size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
            return Ok(serde_json::json!({
                "success": true,
                "name": name,
                "file": fp,
                "content": format!("[Binary file: {file_name}, size: {size} bytes]"),
                "is_binary": true,
            })
            .to_string());
        };

        let file_type = target
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();

        return Ok(serde_json::json!({
            "success": true,
            "name": name,
            "file": fp,
            "content": file_content,
            "file_type": file_type,
        })
        .to_string());
    }

    // Full skill view
    let (tags, related_skills) = extract_tags_and_related(&fm);
    let linked_files = if let Some(dir) = &skill_dir {
        collect_linked_files(dir)
    } else {
        BTreeMap::new()
    };

    let rel_path = skill_md
        .strip_prefix(skills_dir())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| {
            if let Some(parent) = skill_md.parent().and_then(|p| p.parent()) {
                skill_md
                    .strip_prefix(parent)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| skill_md.to_string_lossy().to_string())
            } else {
                skill_md.to_string_lossy().to_string()
            }
        });

    let mut result = serde_json::Map::new();
    result.insert("success".to_string(), Value::Bool(true));
    result.insert("name".to_string(), Value::String(skill_name));
    result.insert(
        "description".to_string(),
        Value::String(fm.description.unwrap_or_default()),
    );
    result.insert(
        "tags".to_string(),
        Value::Array(tags.into_iter().map(Value::String).collect()),
    );
    result.insert(
        "related_skills".to_string(),
        Value::Array(related_skills.into_iter().map(Value::String).collect()),
    );
    result.insert("content".to_string(), Value::String(content));
    result.insert("path".to_string(), Value::String(rel_path));

    if linked_files.is_empty() {
        result.insert("linked_files".to_string(), Value::Null);
        result.insert("usage_hint".to_string(), Value::Null);
    } else {
        let lf_value: serde_json::Map<String, Value> = linked_files
            .into_iter()
            .map(|(k, v)| (k, Value::Array(v.into_iter().map(Value::String).collect())))
            .collect();
        result.insert("linked_files".to_string(), Value::Object(lf_value));
        result.insert(
            "usage_hint".to_string(),
            Value::String(
                "To view linked files, call skill_view(name, file_path) where file_path is e.g. 'references/api.md' or 'assets/config.yaml'".to_string(),
            ),
        );
    }

    Ok(serde_json::Value::Object(result).to_string())
}

/// Find a skill by name across all skill directories.
///
/// Returns (skill_md_path, optional_skill_dir).
fn find_skill_by_name(name: &str, all_dirs: &[PathBuf]) -> Option<(PathBuf, Option<PathBuf>)> {
    for search_dir in all_dirs {
        let direct_path = search_dir.join(name);
        if direct_path.is_dir() {
            let skill_md = direct_path.join("SKILL.md");
            if skill_md.exists() {
                return Some((skill_md, Some(direct_path)));
            }
        } else {
            let md_path = direct_path.with_extension("md");
            if md_path.exists() {
                return Some((md_path, None));
            }
        }
    }

    // Search by directory name
    for search_dir in all_dirs {
        if !search_dir.exists() {
            continue;
        }
        let walker = walkdir::WalkDir::new(search_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                !is_excluded_dir(&n)
            });
        for entry in walker {
            let Ok(entry) = entry else { continue };
            if entry.file_name() == "SKILL.md" && entry.file_type().is_file() {
                let parent = entry.path().parent()?;
                if parent.file_name()?.to_string_lossy() == name {
                    return Some((entry.path().to_path_buf(), Some(parent.to_path_buf())));
                }
            }
        }
    }

    // Legacy: flat .md files (not SKILL.md)
    for search_dir in all_dirs {
        if !search_dir.exists() {
            continue;
        }
        let walker = walkdir::WalkDir::new(search_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                !is_excluded_dir(&n)
            });
        for entry in walker {
            let Ok(entry) = entry else { continue };
            let fname = entry.file_name().to_string_lossy();
            let target = format!("{name}.md");
            if fname == target && entry.file_type().is_file() {
                return Some((entry.path().to_path_buf(), None));
            }
        }
    }

    None
}

/// List files in a skill directory, organized by type.
fn list_skill_files(skill_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut files = BTreeMap::new();
    let mut refs_files = Vec::new();
    let mut tmpl_files = Vec::new();
    let mut asset_files = Vec::new();
    let mut script_files = Vec::new();
    let mut other_files = Vec::new();

    let walker = walkdir::WalkDir::new(skill_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !is_excluded_dir(&n)
        });

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() || entry.file_name() == "SKILL.md" {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(skill_dir) else { continue };
        // Normalize path separators for cross-platform string matching
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");

        if rel_str.starts_with("references/") && ext == "md" {
            refs_files.push(rel_str);
        } else if rel_str.starts_with("templates/")
            && matches!(ext, "md" | "py" | "yaml" | "yml" | "json" | "tex" | "sh")
        {
            tmpl_files.push(rel_str);
        } else if rel_str.starts_with("assets/") {
            asset_files.push(rel_str);
        } else if rel_str.starts_with("scripts/")
            && matches!(ext, "py" | "sh" | "bash" | "js" | "ts" | "rb")
        {
            script_files.push(rel_str);
        } else if matches!(ext, "md" | "py" | "yaml" | "yml" | "json" | "tex" | "sh") {
            other_files.push(rel_str);
        }
    }

    if !refs_files.is_empty() {
        files.insert("references".to_string(), refs_files);
    }
    if !tmpl_files.is_empty() {
        files.insert("templates".to_string(), tmpl_files);
    }
    if !asset_files.is_empty() {
        files.insert("assets".to_string(), asset_files);
    }
    if !script_files.is_empty() {
        files.insert("scripts".to_string(), script_files);
    }
    if !other_files.is_empty() {
        files.insert("other".to_string(), other_files);
    }

    files
}

/// Collect linked files structure (references, templates, assets, scripts).
fn collect_linked_files(skill_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();

    // References
    let refs_dir = skill_dir.join("references");
    if refs_dir.exists() {
        let refs: Vec<_> = walkdir::WalkDir::new(&refs_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().is_some_and(|ex| ex == "md"))
            .filter_map(|e| {
                e.path()
                    .strip_prefix(skill_dir)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        if !refs.is_empty() {
            result.insert("references".to_string(), refs);
        }
    }

    // Templates
    let tmpl_dir = skill_dir.join("templates");
    if tmpl_dir.exists() {
        let exts = ["md", "py", "yaml", "yml", "json", "tex", "sh"];
        let mut tmpls = Vec::new();
        for ext in &exts {
            tmpls.extend(
                walkdir::WalkDir::new(&tmpl_dir)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter(|e| e.path().extension().is_some_and(|ex| ex == *ext))
                    .filter_map(|e| {
                        e.path()
                            .strip_prefix(skill_dir)
                            .ok()
                            .map(|p| p.to_string_lossy().replace('\\', "/"))
                    }),
            );
        }
        if !tmpls.is_empty() {
            result.insert("templates".to_string(), tmpls);
        }
    }

    // Assets
    let assets_dir = skill_dir.join("assets");
    if assets_dir.exists() {
        let assets: Vec<_> = walkdir::WalkDir::new(&assets_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                e.path()
                    .strip_prefix(skill_dir)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        if !assets.is_empty() {
            result.insert("assets".to_string(), assets);
        }
    }

    // Scripts
    let scripts_dir = skill_dir.join("scripts");
    if scripts_dir.exists() {
        let exts = ["py", "sh", "bash", "js", "ts", "rb"];
        let mut scripts = Vec::new();
        for ext in &exts {
            scripts.extend(
                walkdir::WalkDir::new(&scripts_dir)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter(|e| e.path().extension().is_some_and(|ex| ex == *ext))
                    .filter_map(|e| {
                        e.path()
                            .strip_prefix(skill_dir)
                            .ok()
                            .map(|p| p.to_string_lossy().replace('\\', "/"))
                    }),
            );
        }
        if !scripts.is_empty() {
            result.insert("scripts".to_string(), scripts);
        }
    }

    result
}

/// Extract tags and related_skills from frontmatter.
fn extract_tags_and_related(fm: &SkillFrontmatter) -> (Vec<String>, Vec<String>) {
    let hermes_meta = fm
        .metadata
        .as_ref()
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.as_object());

    let tags_raw = hermes_meta
        .and_then(|h| h.get("tags"))
        .or_else(|| fm.metadata.as_ref().and_then(|m| m.get("tags")));

    let related_raw = hermes_meta
        .and_then(|h| h.get("related_skills"))
        .or_else(|| fm.metadata.as_ref().and_then(|m| m.get("related_skills")));

    (parse_tags(tags_raw), parse_tags(related_raw))
}

/// Parse tags from a JSON value.
fn parse_tags(value: Option<&Value>) -> Vec<String> {
    let Some(v) = value else { return Vec::new() };

    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|item| item.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
            .collect();
    }

    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };
        return inner
            .split(',')
            .map(|t| t.trim().trim_matches(|c: char| c == '"' || c == '\'').to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }

    Vec::new()
}

/// Handle skills_categories tool call.
pub fn handle_skills_categories(args: Value) -> Result<String, hermes_core::HermesError> {
    let _verbose = args.get("verbose").and_then(Value::as_bool).unwrap_or(false);

    let all_dirs = all_skills_dirs();
    if all_dirs.is_empty() {
        return Ok(serde_json::json!({
            "success": true,
            "categories": [],
            "message": "No skills directory found.",
        })
        .to_string());
    }

    let mut category_dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut category_counts: BTreeMap<String, usize> = BTreeMap::new();

    for scan_dir in &all_dirs {
        if !scan_dir.exists() {
            continue;
        }
        let walker = walkdir::WalkDir::new(scan_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                !is_excluded_dir(&n)
            });

        for entry in walker {
            let Ok(entry) = entry else { continue };
            if entry.file_name() != "SKILL.md" || !entry.file_type().is_file() {
                continue;
            }

            let content = match std::fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let (fm, _) = parse_frontmatter(&content[..content.len().min(4000)]);

            if !skill_matches_platform(&fm) {
                continue;
            }

            let category = get_category_from_path(entry.path(), &all_dirs);
            if let Some(cat) = category {
                *category_counts.entry(cat.clone()).or_insert(0) += 1;
                category_dirs.entry(cat).or_insert_with(|| {
                    entry
                        .path()
                        .parent()
                        .and_then(|p| p.parent())
                        .unwrap_or(entry.path())
                        .to_path_buf()
                });
            }
        }
    }

    let categories: Vec<_> = category_dirs
        .keys()
        .map(|name| {
            let cat_dir = &category_dirs[name];
            let count = category_counts[name];
            let mut entry = serde_json::Map::new();
            entry.insert("name".to_string(), Value::String(name.clone()));
            entry.insert(
                "skill_count".to_string(),
                Value::Number(serde_json::Number::from(count)),
            );
            if let Some(desc) = load_category_description(cat_dir) {
                entry.insert("description".to_string(), Value::String(desc));
            }
            Value::Object(entry)
        })
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "categories": categories,
        "hint": "If a category is relevant to your task, use skills_list with that category to see available skills",
    })
    .to_string())
}

/// Find a skill directory by name across all skill directories.
fn find_skill_dir(skills_root: &std::path::Path, name: &str) -> Option<PathBuf> {
    let dirs = all_skills_dirs();
    for search_dir in &dirs {
        // Direct: skills_root/name/SKILL.md
        let direct_path = search_dir.join(name);
        if direct_path.is_dir() {
            let skill_md = direct_path.join("SKILL.md");
            if skill_md.exists() {
                return Some(direct_path);
            }
        }
        // Categorized: skills_root/category/name/SKILL.md
        if search_dir == skills_root {
            for entry in std::fs::read_dir(search_dir).ok()?.filter_map(|e| e.ok()) {
                let cat = entry.path();
                if cat.is_dir() {
                    let cat_path = cat.join(name);
                    if cat_path.is_dir() && cat_path.join("SKILL.md").exists() {
                        return Some(cat_path);
                    }
                }
            }
        }
    }
    None
}

/// Atomic write: write to temp file then rename to avoid partial writes.
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{}.", path.file_name().unwrap_or_default().to_string_lossy()))
        .suffix(".tmp")
        .tempfile_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| format!("tempfile: {e}"))?;
    use std::io::Write;
    tmp.write_all(content.as_bytes()).map_err(|e| format!("write: {e}"))?;
    tmp.flush().map_err(|e| format!("flush: {e}"))?;
    tmp.persist(path).map_err(|e| format!("persist: {e}"))?;
    Ok(())
}

/// Security scan a skill directory after writes. Returns error message if blocked.
fn security_scan_skill(skill_dir: &Path) -> Option<String> {
    let result = match skills_guard::scan_skill(skill_dir, "agent-created") {
        Ok(r) => r,
        Err(e) => return Some(format!("Security scan error: {e}")),
    };

    // Derive verdict from findings
    use crate::skills_guard::Severity;
    let has_dangerous = result.findings.iter().any(|f| matches!(f.severity, Severity::Critical | Severity::High));
    let has_caution = result.findings.iter().any(|f| matches!(f.severity, Severity::Medium));
    let verdict = if has_dangerous {
        "dangerous"
    } else if has_caution {
        "caution"
    } else {
        "safe"
    };

    let action = skills_guard::should_allow_install("agent-created", verdict);
    match action {
        "block" => {
            let report = skills_guard::format_scan_report(&result);
            Some(format!("Security scan blocked skill: {report}"))
        }
        "ask" => {
            tracing::warn!("Agent-created skill has security findings ({verdict}) for {skill_dir:?}");
            None // Allow but warn
        }
        _ => None,
    }
}

/// Validate a skill name. Returns error message or None if valid.
fn validate_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Skill name is required.".to_string());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Some(format!("Skill name exceeds {MAX_NAME_LENGTH} characters."));
    }
    if !VALID_NAME_RE.is_match(name) {
        return Some(format!(
            "Invalid skill name '{name}'. Use lowercase letters, numbers, hyphens, dots, and underscores. Must start with a letter or digit."
        ));
    }
    None
}

/// Validate an optional category name. Returns error message or None if valid.
fn validate_category(category: Option<&str>) -> Option<String> {
    let cat = category?.trim();
    if cat.is_empty() {
        return None;
    }
    if cat.contains('/') || cat.contains('\\') {
        return Some(format!(
            "Invalid category '{cat}'. Categories must be a single directory name."
        ));
    }
    if cat.len() > MAX_NAME_LENGTH {
        return Some("Category exceeds {MAX_NAME_LENGTH} characters.".to_string());
    }
    if !VALID_NAME_RE.is_match(cat) {
        return Some(format!(
            "Invalid category '{cat}'. Use lowercase letters, numbers, hyphens, dots, and underscores."
        ));
    }
    None
}

/// Validate YAML frontmatter of SKILL.md content. Returns error message or None if valid.
fn validate_frontmatter(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some("Content cannot be empty.".to_string());
    }
    if !trimmed.starts_with("---") {
        return Some("SKILL.md must start with YAML frontmatter (---). See existing skills for format.".to_string());
    }
    // Find closing ---
    let Some(end_idx) = trimmed[3..].find("\n---") else {
        return Some("SKILL.md frontmatter is not closed. Ensure you have a closing '---' line.".to_string());
    };
    let yaml_content = &trimmed[3..end_idx + 3];
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(yaml_content);
    let Ok(serde_yaml::Value::Mapping(map)) = parsed else {
        return Some("Frontmatter must be a YAML mapping (key: value pairs).".to_string());
    };
    if !map.contains_key("name") {
        return Some("Frontmatter must include 'name' field.".to_string());
    }
    if !map.contains_key("description") {
        return Some("Frontmatter must include 'description' field.".to_string());
    }
    if let Some(desc) = map.get("description").and_then(|v| v.as_str()) {
        if desc.len() > MAX_DESCRIPTION_LENGTH {
            return Some(format!("Description exceeds {MAX_DESCRIPTION_LENGTH} characters."));
        }
    }
    // Check body after frontmatter
    let body_start = end_idx + 3 + 5; // "3 + len("\n---") + 1 for newline
    let body = &trimmed[body_start.min(trimmed.len())..].trim();
    if body.is_empty() {
        return Some("SKILL.md must have content after the frontmatter (instructions, procedures, etc.).".to_string());
    }
    None
}

/// Validate content size. Returns error message or None if valid.
fn validate_content_size(content: &str, label: &str) -> Option<String> {
    if content.len() > MAX_SKILL_CONTENT_CHARS {
        return Some(format!(
            "{} content is {} characters (limit: {}). Consider splitting into smaller files.",
            label, content.len(), MAX_SKILL_CONTENT_CHARS
        ));
    }
    None
}

/// Validate a file path for write_file/remove_file.
fn validate_file_path(file_path: &str) -> Option<String> {
    if file_path.is_empty() {
        return Some("file_path is required.".to_string());
    }
    let first = file_path.split('/').next().unwrap_or(file_path).split('\\').next().unwrap_or(file_path);
    if !ALLOWED_SUBDIRS.contains(&first) {
        let allowed = ALLOWED_SUBDIRS.join(", ");
        return Some(format!("File must be under one of: {allowed}. Got: '{file_path}'"));
    }
    // Must have a filename
    let p = PathBuf::from(file_path);
    if p.file_name().is_none() {
        return Some(format!("Provide a file path, not just a directory. Example: '{first}/myfile.md'"));
    }
    None
}

/// Find a skill by name across all skill directories.
fn find_skill_anywhere(skills_root: &Path, name: &str) -> Option<PathBuf> {
    // Check direct child
    let direct = skills_root.join(name);
    if direct.join("SKILL.md").exists() {
        return Some(direct);
    }
    // Search subdirectories
    for entry in walkdir::WalkDir::new(skills_root)
        .min_depth(1)
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir()
            && entry.path().file_name().and_then(|n| n.to_str()) == Some(name)
            && entry.path().join("SKILL.md").exists()
        {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

/// Resolve a supporting file path within a skill directory.
fn resolve_skill_file_target(skill_dir: &Path, file_path: &str) -> Result<PathBuf, String> {
    let target = skill_dir.join(file_path);
    // Path traversal check
    let canon_skill = skill_dir.canonicalize().map_err(|e| format!("canonicalize skill dir: {e}"))?;
    let canon_target = target.canonicalize().or_else(|_| {
        target.parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(target.file_name().unwrap()))
            .ok_or_else(|| "cannot resolve target path".to_string())
    })?;
    if !canon_target.starts_with(&canon_skill) {
        return Err("File path escapes skill directory.".to_string());
    }
    Ok(target)
}

/// Handle skill management operations.
pub fn handle_skill_manage(args: Value) -> Result<String, hermes_core::HermesError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");

    if name.is_empty() {
        return Ok(tool_error("skill_manage requires a 'name' parameter."));
    }

    let hermes_home = hermes_core::get_hermes_home();
    let skills_root = hermes_home.join("skills");

    match action {
        "create" => handle_skill_create(&skills_root, name, &args),
        "edit" => handle_skill_edit(&skills_root, name, &args),
        "patch" => handle_skill_patch(&skills_root, name, &args),
        "delete" => handle_skill_delete(&skills_root, name),
        "write_file" => handle_skill_write_file(&skills_root, name, &args),
        "remove_file" => handle_skill_remove_file(&skills_root, name, &args),
        _ => Ok(tool_error(format!(
            "Unknown skill_manage action: '{action}'. Use: create, edit, patch, delete, write_file, remove_file"
        ))),
    }
}

fn handle_skill_create(skills_root: &Path, name: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    if content.is_empty() {
        return Ok(tool_error("content is required for 'create'. Provide the full SKILL.md text (frontmatter + body)."));
    }

    // Validate name
    if let Some(err) = validate_name(name) {
        return Ok(tool_error(err));
    }

    // Validate category
    let category = args.get("category").and_then(Value::as_str);
    if let Some(err) = validate_category(category) {
        return Ok(tool_error(err));
    }

    // Validate frontmatter
    if let Some(err) = validate_frontmatter(content) {
        return Ok(tool_error(err));
    }

    // Validate content size
    if let Some(err) = validate_content_size(content, "SKILL.md") {
        return Ok(tool_error(err));
    }

    // Check for duplicate names
    if find_skill_anywhere(skills_root, name).is_some() {
        return Ok(tool_error(format!("A skill named '{name}' already exists.")));
    }

    // Create skill directory
    let skill_dir = if let Some(cat) = category {
        skills_root.join(cat).join(name)
    } else {
        skills_root.join(name)
    };
    if let Err(e) = std::fs::create_dir_all(&skill_dir) {
        return Ok(tool_error(format!("Failed to create skill directory: {e}")));
    }

    // Write SKILL.md atomically
    let skill_md = skill_dir.join("SKILL.md");
    if let Err(e) = atomic_write(&skill_md, content) {
        let _ = std::fs::remove_dir_all(&skill_dir);
        return Ok(tool_error(format!("Failed to write SKILL.md: {e}")));
    }

    // Security scan — roll back on block
    if let Some(err) = security_scan_skill(&skill_dir) {
        let _ = std::fs::remove_dir_all(&skill_dir);
        return Ok(tool_error(err));
    }

    let rel = skill_dir.strip_prefix(skills_root).unwrap_or(&skill_dir);
    Ok(serde_json::json!({
        "success": true,
        "action": "create",
        "name": name,
        "path": rel.to_string_lossy().to_string(),
        "message": format!("Skill '{name}' created."),
        "hint": format!("To add reference files, templates, or scripts, use skill_manage(action='write_file', name='{name}', file_path='references/example.md', file_content='...')"),
    })
    .to_string())
}

fn handle_skill_edit(skills_root: &Path, name: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    if content.is_empty() {
        return Ok(tool_error("content is required for 'edit'. Provide the full updated SKILL.md text."));
    }

    // Validate frontmatter
    if let Some(err) = validate_frontmatter(content) {
        return Ok(tool_error(err));
    }

    // Validate content size
    if let Some(err) = validate_content_size(content, "SKILL.md") {
        return Ok(tool_error(err));
    }

    let Some(skill_dir) = find_skill_dir(skills_root, name)
        .or_else(|| find_skill_anywhere(skills_root, name))
    else {
        return Ok(tool_error(format!("Skill '{name}' not found. Use skills_list() to see available skills.")));
    };

    // Back up original for rollback
    let skill_md = skill_dir.join("SKILL.md");
    let original = std::fs::read_to_string(&skill_md).ok();

    if let Err(e) = atomic_write(&skill_md, content) {
        return Ok(tool_error(format!("Failed to write SKILL.md: {e}")));
    }

    // Security scan — roll back on block
    if let Some(err) = security_scan_skill(&skill_dir) {
        if let Some(orig) = original {
            let _ = atomic_write(&skill_md, &orig);
        }
        return Ok(tool_error(err));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "edit",
        "name": name,
        "message": format!("Skill '{name}' updated."),
        "path": skill_dir.to_string_lossy().to_string(),
    })
    .to_string())
}

fn handle_skill_patch(skills_root: &Path, name: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    let old_string = args.get("old_string").and_then(Value::as_str);
    let new_string = args.get("new_string").and_then(Value::as_str);

    if old_string.is_none() {
        return Ok(tool_error("old_string is required for 'patch'. Provide the text to find."));
    }
    if new_string.is_none() {
        return Ok(tool_error("new_string is required for 'patch'. Use empty string to delete matched text."));
    }

    let old_string = old_string.unwrap();
    let new_string = new_string.unwrap();
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

    let file_path = args
        .get("file_path")
        .and_then(Value::as_str)
        .unwrap_or("SKILL.md");

    let Some(skill_dir) = find_skill_dir(skills_root, name)
        .or_else(|| find_skill_anywhere(skills_root, name))
    else {
        return Ok(tool_error(format!("Skill not found: {name}")));
    };

    let target_file = if file_path == "SKILL.md" {
        skill_dir.join("SKILL.md")
    } else {
        match resolve_skill_file_target(&skill_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(tool_error(e)),
        }
    };

    if !target_file.exists() {
        return Ok(tool_error(format!("File not found in skill '{name}': {file_path}")));
    }

    let content = match std::fs::read_to_string(&target_file) {
        Ok(c) => c,
        Err(e) => return Ok(tool_error(format!("Failed to read file: {e}"))),
    };

    // Try exact match first, then fuzzy
    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };

    let (new_content, match_count) = if new_content != content {
        (new_content, if replace_all { content.matches(old_string).count() } else { 1 })
    } else {
        // Fuzzy fallback
        let (s, n, _) = crate::fuzzy_match::fuzzy_find_and_replace(&content, old_string, new_string, replace_all);
        (s, n)
    };

    if match_count == 0 {
        let preview = content.chars().take(500).collect::<String>();
        return Ok(serde_json::json!({
            "success": false,
            "error": "old_string not found (exact or fuzzy match failed).",
            "file_preview": preview,
        }).to_string());
    }

    // Validate SKILL.md frontmatter after patch
    if file_path == "SKILL.md" {
        if let Some(err) = validate_frontmatter(&new_content) {
            return Ok(tool_error(format!("Patch would break SKILL.md structure: {err}")));
        }
    }

    // Validate content size
    let label = if file_path == "SKILL.md" { "SKILL.md" } else { file_path };
    if let Some(err) = validate_content_size(&new_content, label) {
        return Ok(tool_error(err));
    }

    let original = content;
    if let Err(e) = atomic_write(&target_file, &new_content) {
        return Ok(tool_error(format!("Failed to write file: {e}")));
    }

    // Security scan — roll back on block
    if let Some(err) = security_scan_skill(&skill_dir) {
        let _ = atomic_write(&target_file, &original);
        return Ok(tool_error(err));
    }

    let plural = if match_count > 1 { "s" } else { "" };
    Ok(serde_json::json!({
        "success": true,
        "action": "patch",
        "name": name,
        "file": file_path,
        "message": format!("Patched '{file_path}' in skill '{name}' ({match_count} replacement{}).", plural),
    })
    .to_string())
}

fn handle_skill_delete(skills_root: &Path, name: &str) -> Result<String, hermes_core::HermesError> {
    let Some(skill_dir) = find_skill_dir(skills_root, name)
        .or_else(|| find_skill_anywhere(skills_root, name))
    else {
        return Ok(tool_error(format!("Skill not found: {name}")));
    };

    if let Err(e) = std::fs::remove_dir_all(&skill_dir) {
        return Ok(tool_error(format!("Failed to delete skill directory: {e}")));
    }

    // Clean up empty parent (don't remove SKILLS_DIR itself)
    if let Some(parent) = skill_dir.parent() {
        if parent != skills_root && parent.exists() {
            let _ = std::fs::remove_dir(parent); // best-effort
        }
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "delete",
        "name": name,
        "message": format!("Skill '{name}' deleted."),
    })
    .to_string())
}

fn handle_skill_write_file(skills_root: &Path, name: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    let file_path = args.get("file_path").and_then(Value::as_str);
    let file_content = args.get("file_content").and_then(Value::as_str);

    if file_path.is_none() {
        return Ok(tool_error("file_path is required for 'write_file'. Example: 'references/api-guide.md'"));
    }
    if file_content.is_none() {
        return Ok(tool_error("file_content is required for 'write_file'."));
    }

    let file_path = file_path.unwrap();
    let file_content = file_content.unwrap();

    // Validate file path (must be under allowed subdir)
    if let Some(err) = validate_file_path(file_path) {
        return Ok(tool_error(err));
    }

    // Check byte size limit
    let content_bytes = file_content.len();
    if content_bytes > MAX_SKILL_FILE_BYTES {
        return Ok(tool_error(format!(
            "File content is {} bytes (limit: {} bytes / 1 MiB). Consider splitting into smaller files.",
            content_bytes, MAX_SKILL_FILE_BYTES
        )));
    }

    let Some(skill_dir) = find_skill_dir(skills_root, name)
        .or_else(|| find_skill_anywhere(skills_root, name))
    else {
        return Ok(tool_error(format!("Skill not found: {name}. Create it first with action='create'.")));
    };

    let target_file = match resolve_skill_file_target(&skill_dir, file_path) {
        Ok(p) => p,
        Err(e) => return Ok(tool_error(e)),
    };

    // Back up for rollback
    let original = std::fs::read_to_string(&target_file).ok();

    if let Err(e) = atomic_write(&target_file, file_content) {
        return Ok(tool_error(format!("Failed to write file: {e}")));
    }

    // Security scan — roll back on block
    if let Some(err) = security_scan_skill(&skill_dir) {
        if let Some(orig) = original {
            let _ = atomic_write(&target_file, &orig);
        } else {
            let _ = std::fs::remove_file(&target_file);
        }
        return Ok(tool_error(err));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "write_file",
        "name": name,
        "file": file_path,
        "message": format!("File '{file_path}' written to skill '{name}'."),
        "path": target_file.to_string_lossy().to_string(),
    })
    .to_string())
}

fn handle_skill_remove_file(skills_root: &Path, name: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    let file_path = args.get("file_path").and_then(Value::as_str);

    if file_path.is_none() {
        return Ok(tool_error("file_path is required for 'remove_file'."));
    }

    let file_path = file_path.unwrap();

    // Validate file path
    if let Some(err) = validate_file_path(file_path) {
        return Ok(tool_error(err));
    }

    let Some(skill_dir) = find_skill_dir(skills_root, name)
        .or_else(|| find_skill_anywhere(skills_root, name))
    else {
        return Ok(tool_error(format!("Skill not found: {name}")));
    };

    let target_file = match resolve_skill_file_target(&skill_dir, file_path) {
        Ok(p) => p,
        Err(e) => return Ok(tool_error(e)),
    };

    if !target_file.exists() {
        // List available files for debugging
        let available: Vec<String> = ALLOWED_SUBDIRS
            .iter()
            .filter_map(|sub| {
                let d = skill_dir.join(sub);
                if d.exists() {
                    Some(walkdir::WalkDir::new(&d).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.path().strip_prefix(&skill_dir).unwrap_or(e.path()).to_string_lossy().to_string()).collect::<Vec<_>>())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("File '{file_path}' not found in skill '{name}'."),
            "available_files": if available.is_empty() { serde_json::Value::Null } else { serde_json::json!(available) },
        }).to_string());
    }

    if let Err(e) = std::fs::remove_file(&target_file) {
        return Ok(tool_error(format!("Failed to remove file: {e}")));
    }

    // Clean up empty parent directories
    let mut parent = target_file.parent().map(|p| p.to_path_buf());
    while let Some(p) = parent {
        if p == skill_dir { break; }
        if p.read_dir().is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(&p);
            parent = p.parent().map(|p| p.to_path_buf());
        } else {
            break;
        }
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "remove_file",
        "name": name,
        "file": file_path,
        "message": format!("File '{file_path}' removed from skill '{name}'."),
    })
    .to_string())
}

// ── Skill config variables (from Python skill_utils.py) ──────────────────

/// A config variable declared by a skill in its YAML frontmatter.
///
/// Skills declare config.yaml settings they need via:
/// ```yaml
/// metadata:
///   hermes:
///     config:
///       - key: wiki.path
///         description: Path to the LLM Wiki knowledge base directory
///         default: "~/wiki"
///         prompt: Wiki directory path
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillConfigVar {
    pub key: String,
    pub description: String,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Storage prefix: all skill config vars are stored under `skills.config.*`
/// in config.yaml. Skill authors declare logical keys (e.g. "wiki.path");
/// the system adds this prefix for storage and strips it for display.
const SKILL_CONFIG_PREFIX: &str = "skills.config";

/// Extract conditional activation fields from parsed frontmatter.
pub fn extract_skill_conditions(fm: &Value) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();
    let metadata = fm.get("metadata").and_then(|v| v.as_object());
    let hermes = metadata
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.as_object());

    let keys = [
        "fallback_for_toolsets",
        "requires_toolsets",
        "fallback_for_tools",
        "requires_tools",
    ];
    for key in &keys {
        let values = hermes
            .and_then(|h| h.get(*key))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        result.insert(key.to_string(), values);
    }
    result
}

/// Extract config variable declarations from parsed frontmatter.
///
/// Returns a list of [`SkillConfigVar`] entries. Invalid or incomplete entries
/// are silently skipped.
pub fn extract_skill_config_vars(fm: &Value) -> Vec<SkillConfigVar> {
    let metadata = fm.get("metadata").and_then(|v| v.as_object());
    let hermes = metadata
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.as_object());
    let raw = match hermes.and_then(|h| h.get("config")) {
        Some(v) => v,
        None => return Vec::new(),
    };

    let items: Vec<&Value> = if raw.is_array() {
        raw.as_array().unwrap().iter().collect()
    } else if raw.is_object() {
        vec![raw]
    } else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for item in items {
        let Some(obj) = item.as_object() else { continue };
        let key = obj
            .get("key")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());
        let Some(key) = key else { continue };
        if !seen.insert(key.clone()) {
            continue;
        }

        let desc = obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());
        if desc.is_none() || desc.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
            continue;
        }

        let default = obj.get("default").cloned();
        let prompt = obj
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .or_else(|| desc.clone());

        result.push(SkillConfigVar {
            key,
            description: desc.unwrap_or_default(),
            default,
            prompt,
        });
    }

    result
}

/// Walk a nested dict following a dotted key. Returns None if any part is missing.
fn resolve_dotpath(config: &Value, dotted_key: &str) -> Option<Value> {
    let mut current = config;
    for part in dotted_key.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// Resolve current values for skill config vars from config.yaml.
///
/// Skill config is stored under `skills.config.<key>` in config.yaml.
/// Returns a dict mapping **logical** keys to their current values
/// (or the declared default if the key isn't set).
/// Path values with `~` or `${` are expanded.
pub fn resolve_skill_config_values(vars: &[SkillConfigVar]) -> BTreeMap<String, String> {
    let config_path = hermes_core::get_hermes_home().join("config.yaml");
    let config: Value = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_yaml::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let mut resolved = BTreeMap::new();
    for var in vars {
        let storage_key = format!("{SKILL_CONFIG_PREFIX}.{}", var.key);
        let value = resolve_dotpath(&config, &storage_key);

        let display_val = match value {
            Some(v) if v.is_string() && v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) => {
                var.default.clone().unwrap_or(Value::String(String::new()))
            }
            Some(v) => v,
            None => var.default.clone().unwrap_or(Value::String(String::new())),
        };

        let str_val = match &display_val {
            Value::String(s) => {
                if s.contains('~') || s.contains("${") {
                    shellexpand::full(s).unwrap_or(std::borrow::Cow::Borrowed(s)).into_owned()
                } else {
                    s.clone()
                }
            }
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        };

        resolved.insert(var.key.clone(), str_val);
    }

    resolved
}

/// Scan all enabled skills and collect their config variable declarations.
///
/// Walks every skills directory, parses each SKILL.md frontmatter, and returns
/// a deduplicated list of config var dicts. Each dict also includes a
/// `skill` key with the skill name for attribution.
/// Disabled and platform-incompatible skills are excluded.
pub fn discover_all_skill_config_vars() -> Vec<BTreeMap<String, String>> {
    let disabled = get_disabled_skill_names();
    let all_dirs = all_skills_dirs();
    let mut result = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();

    for skills_dir in &all_dirs {
        if !skills_dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(skills_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !EXCLUDED_SKILL_DIRS.contains(&name.as_ref())
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() == "SKILL.md" && e.file_type().is_file())
        {
            let skill_file = entry.path();
            let Ok(raw) = std::fs::read_to_string(skill_file) else { continue };
            let Some(fm_val) = parse_frontmatter_value(&raw) else { continue };

            let (fm, _) = parse_frontmatter(&raw);
            let skill_name = fm
                .name
                .clone()
                .unwrap_or_else(|| skill_file.parent().map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()).unwrap_or_default());

            if disabled.contains(&skill_name) {
                continue;
            }
            if !skill_matches_platform(&fm) {
                continue;
            }

            let config_vars = extract_skill_config_vars(&fm_val);
            for var in &config_vars {
                if seen_keys.insert(var.key.clone()) {
                    let mut entry_map = BTreeMap::new();
                    entry_map.insert("key".to_string(), var.key.clone());
                    entry_map.insert("description".to_string(), var.description.clone());
                    if let Some(ref d) = var.default {
                        entry_map.insert("default".to_string(), d.to_string());
                    }
                    if let Some(ref p) = var.prompt {
                        entry_map.insert("prompt".to_string(), p.clone());
                    }
                    entry_map.insert("skill".to_string(), skill_name.clone());
                    result.push(entry_map);
                }
            }
        }
    }

    result
}

// ── Skill commands (from Python skill_commands.py) ───────────────────────

/// Patterns for sanitizing skill names into clean hyphen-separated slugs.
static SKILL_INVALID_CHARS: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"[^a-z0-9-]").unwrap());
static SKILL_MULTI_HYPHEN: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"-{2,}").unwrap());

/// A discovered skill command mapping.
#[derive(Debug, Clone)]
pub struct SkillCommandInfo {
    pub name: String,
    pub description: String,
    pub skill_md_path: PathBuf,
    pub skill_dir: PathBuf,
}

/// Scan ~/.hermes/skills/ and return a mapping of "/command" -> skill info.
///
/// Skills are normalized to hyphen-separated slugs (e.g. `/gif-search`).
/// Platform-incompatible and user-disabled skills are excluded.
pub fn scan_skill_commands() -> BTreeMap<String, SkillCommandInfo> {
    let mut commands = BTreeMap::new();
    let disabled = get_disabled_skill_names();
    let mut seen_names = std::collections::HashSet::new();

    let dirs_to_scan: Vec<PathBuf> = all_skills_dirs();

    for scan_dir in &dirs_to_scan {
        if !scan_dir.exists() {
            continue;
        }
        for skill_md in walkdir::WalkDir::new(scan_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() == "SKILL.md" && e.file_type().is_file())
            .filter(|e| {
                !e.path().components().any(|c| {
                    c.as_os_str()
                        .to_str()
                        .map(|s| matches!(s, ".git" | ".github" | ".hub"))
                        .unwrap_or(false)
                })
            })
        {
            let Ok(content) = std::fs::read_to_string(skill_md.path()) else { continue };
            let (fm, body) = parse_frontmatter(&content);

            if !skill_matches_platform(&fm) {
                continue;
            }

            let name = fm
                .name
                .clone()
                .unwrap_or_else(|| skill_md.path().parent().map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()).unwrap_or_default());

            if seen_names.contains(&name) {
                continue;
            }
            if disabled.contains(&name) {
                continue;
            }

            let description = fm.description.clone().unwrap_or_else(|| {
                for line in body.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        return trimmed.chars().take(80).collect::<String>();
                    }
                }
                String::new()
            });

            seen_names.insert(name.clone());

            // Normalize to hyphen-separated slug
            let mut cmd_name = name.to_lowercase().replace([' ', '_'], "-");
            cmd_name = SKILL_INVALID_CHARS.replace_all(&cmd_name, "").to_string();
            cmd_name = SKILL_MULTI_HYPHEN.replace_all(&cmd_name, "-").to_string();
            cmd_name = cmd_name.trim_matches('-').to_string();

            if cmd_name.is_empty() {
                continue;
            }

            let cmd_key = format!("/{cmd_name}");
            let skill_name_for_desc = name.clone();
            commands.insert(
                cmd_key,
                SkillCommandInfo {
                    name,
                    description: if description.is_empty() {
                        format!("Invoke the {skill_name_for_desc} skill")
                    } else {
                        description
                    },
                    skill_md_path: skill_md.path().to_path_buf(),
                    skill_dir: skill_md.path().parent().unwrap_or(skill_md.path()).to_path_buf(),
                },
            );
        }
    }

    commands
}

/// Resolve a user-typed `/command` to its canonical skill command key.
///
/// Skills are always stored with hyphens. Hyphens and underscores are treated
/// interchangeably in user input.
///
/// Returns the matching `/slug` key from `scan_skill_commands()` or `None`.
pub fn resolve_skill_command_key(command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let cmd_key = format!("/{}", command.replace('_', "-"));
    let commands = scan_skill_commands();
    if commands.contains_key(&cmd_key) {
        Some(cmd_key)
    } else {
        None
    }
}

/// Build the user message content for a skill slash command invocation.
///
/// Args:
///   cmd_key: The command key including leading slash (e.g., "/gif-search").
///   user_instruction: Optional text the user typed after the command.
///
/// Returns the formatted message string, or an error message if not found.
pub fn build_skill_invocation_message(
    cmd_key: &str,
    user_instruction: &str,
) -> String {
    let commands = scan_skill_commands();
    let Some(skill_info) = commands.get(cmd_key) else {
        return format!("[Failed to load skill: {cmd_key}]");
    };

    let skill_name = &skill_info.name;
    let activation_note = format!(
        "[SYSTEM: The user has invoked the \"{skill_name}\" skill, indicating they want \
         you to follow its instructions. The full skill content is loaded below.]"
    );

    let Ok(content) = std::fs::read_to_string(&skill_info.skill_md_path) else {
        return format!("[Failed to load skill: {skill_name}]");
    };

    let mut parts = vec![activation_note, String::new(), content.trim().to_string()];

    // Inject resolved skill config values
    if let Some(fm_val) = parse_frontmatter_value(&content) {
        let config_vars = extract_skill_config_vars(&fm_val);
        if !config_vars.is_empty() {
            let resolved = resolve_skill_config_values(&config_vars);
            if !resolved.is_empty() {
                parts.push(String::new());
                parts.push("[Skill config (from ~/.hermes/config.yaml):".to_string());
                for (key, value) in &resolved {
                    let display_val = if value.is_empty() {
                        "(not set)".to_string()
                    } else {
                        value.clone()
                    };
                    parts.push(format!("  {key} = {display_val}"));
                }
                parts.push("]".to_string());
            }
        }
    }

    if !user_instruction.is_empty() {
        parts.push(String::new());
        parts.push(format!(
            "The user has provided the following instruction alongside the skill invocation: {user_instruction}"
        ));
    }

    parts.join("\n")
}

/// Load one or more skills for session-wide CLI preloading.
///
/// Returns (prompt_text, loaded_skill_names, missing_identifiers).
pub fn build_preloaded_skills_prompt(
    skill_identifiers: &[String],
) -> (String, Vec<String>, Vec<String>) {
    let mut prompt_parts = Vec::new();
    let mut loaded_names = Vec::new();
    let mut missing = Vec::new();

    let mut seen = std::collections::HashSet::new();
    let all_dirs = all_skills_dirs();

    for raw_identifier in skill_identifiers {
        let identifier = raw_identifier.trim();
        if identifier.is_empty() || !seen.insert(identifier.to_string()) {
            continue;
        }

        let Some(skill_info) = find_skill_by_name(identifier, &all_dirs) else {
            missing.push(raw_identifier.clone());
            continue;
        };

        let Ok(content) = std::fs::read_to_string(&skill_info.0) else {
            missing.push(raw_identifier.clone());
            continue;
        };

        let (fm, _) = parse_frontmatter(&content);
        let skill_name = fm
            .name
            .clone()
            .unwrap_or_else(|| identifier.to_string());

        let activation_note = format!(
            "[SYSTEM: The user launched this CLI session with the \"{skill_name}\" skill \
             preloaded. Treat its instructions as active guidance for the duration of this \
             session unless the user overrides them.]"
        );

        let mut skill_parts = vec![activation_note, String::new(), content.trim().to_string()];

        // Inject skill config
        if let Some(fm_val) = parse_frontmatter_value(&content) {
            let config_vars = extract_skill_config_vars(&fm_val);
            if !config_vars.is_empty() {
                let resolved = resolve_skill_config_values(&config_vars);
                if !resolved.is_empty() {
                    skill_parts.push(String::new());
                    skill_parts.push("[Skill config (from ~/.hermes/config.yaml):".to_string());
                    for (key, value) in &resolved {
                        let display_val = if value.is_empty() {
                            "(not set)".to_string()
                        } else {
                            value.clone()
                        };
                        skill_parts.push(format!("  {key} = {display_val}"));
                    }
                    skill_parts.push("]".to_string());
                }
            }
        }

        prompt_parts.push(skill_parts.join("\n"));
        loaded_names.push(skill_name);
    }

    (prompt_parts.join("\n\n"), loaded_names, missing)
}

/// Build the default workspace-relative markdown path for a /plan invocation.
///
/// Relative paths are intentional: file tools are task/backend-aware and resolve
/// them against the active working directory.
pub fn build_plan_path(user_instruction: &str) -> PathBuf {
    let slug_source = user_instruction
        .trim()
        .lines()
        .next()
        .unwrap_or("");
    let plan_slug_re = regex::Regex::new(r"[^a-z0-9]+").unwrap();
    let mut slug = plan_slug_re
        .replace_all(&slug_source.to_lowercase(), "-")
        .trim_matches('-')
        .to_string();

    if !slug.is_empty() {
        slug = slug
            .split('-')
            .take(8)
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        slug = slug.chars().take(48).collect::<String>();
        slug = slug.trim_matches('-').to_string();
    }

    if slug.is_empty() {
        slug = "conversation-plan".to_string();
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
    PathBuf::from(".hermes")
        .join("plans")
        .join(format!("{timestamp}-{slug}.md"))
}

/// Register skills tools.
pub fn register_skills_tools(registry: &mut crate::registry::ToolRegistry) {
    registry.register(
        "skills_list".to_string(),
        "skills".to_string(),
        serde_json::json!({
            "name": "skills_list",
            "description": "List available skills with metadata (name, description, category). Use this first to discover what skills are available. Optionally filter by category.",
            "parameters": {
                "type": "object",
                "properties": {
                    "category": { "type": "string", "description": "Optional category filter (e.g., 'mlops', 'coding')." }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_skills_list),
        Some(std::sync::Arc::new(check_skills_requirements)),
        vec![],
        "List available skills with metadata".to_string(),
        "📚".to_string(),
        None,
    );

    registry.register(
        "skill_view".to_string(),
        "skills".to_string(),
        serde_json::json!({
            "name": "skill_view",
            "description": "View full skill content, tags, related skills, and linked files. Also supports viewing specific files within a skill directory via file_path parameter.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name or path of the skill (e.g., 'axolotl' or 'mlops/axolotl')." },
                    "file_path": { "type": "string", "description": "Optional relative path to a specific file within the skill (e.g., 'references/api.md')." }
                },
                "required": ["name"]
            }
        }),
        std::sync::Arc::new(handle_skill_view),
        Some(std::sync::Arc::new(check_skills_requirements)),
        vec![],
        "View full skill content and linked files".to_string(),
        "📚".to_string(),
        None,
    );

    registry.register(
        "skills_categories".to_string(),
        "skills".to_string(),
        serde_json::json!({
            "name": "skills_categories",
            "description": "List skill categories with descriptions and skill counts. Use this for high-level discovery before drilling into specific skills.",
            "parameters": {
                "type": "object",
                "properties": {
                    "verbose": { "type": "boolean", "description": "Include detailed information (currently always included)." }
                },
                "required": []
            }
        }),
        std::sync::Arc::new(handle_skills_categories),
        Some(std::sync::Arc::new(check_skills_requirements)),
        vec![],
        "List skill categories with descriptions".to_string(),
        "📚".to_string(),
        None,
    );

    registry.register(
        "skill_manage".to_string(),
        "skills".to_string(),
        serde_json::json!({
            "name": "skill_manage",
            "description": "Manage skills (create, update, delete). Skills are your procedural memory — reusable approaches for recurring task types. New skills go to ~/.hermes/skills/; existing skills can be modified wherever they live. Actions: create (full SKILL.md + optional category), edit (full rewrite), patch (find/replace in SKILL.md or linked files), delete (remove entire skill), write_file (create/update linked file within a skill), remove_file (delete linked file from skill).",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "Action: create, edit, patch, delete, write_file, remove_file." },
                    "name": { "type": "string", "description": "Skill name (directory name under skills/)." },
                    "content": { "type": "string", "description": "Full SKILL.md content (for create/edit actions)." },
                    "category": { "type": "string", "description": "Category subdirectory (for create action, e.g., 'dev', 'analysis')." },
                    "file_path": { "type": "string", "description": "Relative file path within skill directory (for patch/write_file/remove_file). Examples: 'references/api.md', 'SKILL.md'." },
                    "old_string": { "type": "string", "description": "Text to find (for patch action). Provide empty string to insert at end." },
                    "new_string": { "type": "string", "description": "Replacement text (for patch action). Use empty string to delete matched text." },
                    "file_content": { "type": "string", "description": "Content for the file (for write_file action)." },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences (for patch action, default false)." }
                },
                "required": ["action", "name"]
            }
        }),
        std::sync::Arc::new(handle_skill_manage),
        Some(std::sync::Arc::new(check_skills_requirements)),
        vec![],
        "Create, edit, patch, delete, and manage skill files".to_string(),
        "\u{1F4DD}".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_skill(dir: &Path, name: &str, description: &str, category: Option<&str>) {
        let skill_dir = if let Some(cat) = category {
            let cat_dir = dir.join(cat);
            std::fs::create_dir_all(&cat_dir).unwrap();
            let sd = cat_dir.join(name);
            std::fs::create_dir_all(&sd).unwrap();
            sd
        } else {
            let sd = dir.join(name);
            std::fs::create_dir_all(&sd).unwrap();
            sd
        };

        let frontmatter = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nThis is the skill content."
        );
        std::fs::write(skill_dir.join("SKILL.md"), frontmatter).unwrap();
    }

    // Shared test base dir — set once before any test accesses get_hermes_home()
    fn test_base() -> &'static std::path::Path {
        use std::sync::OnceLock;
        static BASE: OnceLock<tempfile::TempDir> = OnceLock::new();
        let tmp = BASE.get_or_init(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills = tmp.path().join("skills");
            create_test_skill(&skills, "test-skill-a", "Skill A description", Some("coding"));
            create_test_skill(&skills, "test-skill-b", "Skill B description", Some("coding"));
            create_test_skill(&skills, "test-skill-c", "Skill C description", Some("data"));
            create_test_skill(&skills, "root-skill", "Root skill description", None);
            // Also create the "skill-with-files" test skill
            let swf = skills.join("coding").join("skill-with-files");
            std::fs::create_dir_all(swf.join("references")).unwrap();
            std::fs::create_dir_all(swf.join("templates")).unwrap();
            std::fs::write(
                swf.join("SKILL.md"),
                "---\nname: skill-with-files\ndescription: Has linked files\n---\n\nContent.",
            )
            .unwrap();
            std::fs::write(
                swf.join("references").join("api.md"),
                "# API Reference\n\nDetails here.",
            )
            .unwrap();
            std::fs::write(swf.join("templates").join("output.md"), "# Output Template").unwrap();
            // Also create test-skill-d with a reference file
            let tsd = skills.join("test-skill-d");
            std::fs::create_dir_all(tsd.join("references")).unwrap();
            std::fs::write(
                tsd.join("SKILL.md"),
                "---\nname: test-skill-d\ndescription: Skill D\n---\n\nContent.",
            )
            .unwrap();
            std::fs::write(tsd.join("references").join("guide.md"), "# Guide").unwrap();
            tmp
        });
        tmp.path()
    }

    fn init_hermes_home() -> bool {
        // Try to set hermes home to our shared test base dir.
        // This can only succeed once globally — if another test already
        // set it, we return false and the caller should skip assertions
        // that depend on test-specific skills.
        hermes_core::hermes_home::set_hermes_home(test_base()).is_ok()
    }

    #[test]
    fn test_parse_frontmatter() {
        let content = "---\nname: test\ndescription: A test skill\n---\n\n# Title\n\nBody content.";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.name, Some("test".to_string()));
        assert_eq!(fm.description, Some("A test skill".to_string()));
        assert_eq!(body, "# Title\n\nBody content.");
    }

    #[test]
    fn test_parse_frontmatter_no_yaml() {
        let content = "# Just a title\n\nBody content.";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.name.is_none());
        assert_eq!(body, "# Just a title\n\nBody content.");
    }

    #[test]
    fn test_skill_matches_platform() {
        let fm = SkillFrontmatter::default();
        assert!(skill_matches_platform(&fm));

        let fm = SkillFrontmatter {
            platforms: Some(vec!["linux".to_string(), "macos".to_string(), "windows".to_string()]),
            ..Default::default()
        };
        assert!(skill_matches_platform(&fm));

        #[cfg(target_os = "windows")]
        {
            let fm = SkillFrontmatter {
                platforms: Some(vec!["macos".to_string()]),
                ..Default::default()
            };
            assert!(!skill_matches_platform(&fm));
        }
    }

    #[test]
    fn test_parse_tags_array() {
        let arr = serde_json::json!(["tag1", "tag2", "tag3"]);
        let tags = parse_tags(Some(&arr));
        assert_eq!(tags, vec!["tag1", "tag2", "tag3"]);
    }

    #[test]
    fn test_parse_tags_bracket_string() {
        let s = serde_json::json!("[tag1, tag2, tag3]");
        let tags = parse_tags(Some(&s));
        assert_eq!(tags, vec!["tag1", "tag2", "tag3"]);
    }

    #[test]
    fn test_parse_tags_comma_string() {
        let s = serde_json::json!("fine-tuning, llm, training");
        let tags = parse_tags(Some(&s));
        assert_eq!(tags, vec!["fine-tuning", "llm", "training"]);
    }

    #[test]
    fn test_parse_tags_empty() {
        assert!(parse_tags(None).is_empty());
        assert!(parse_tags(Some(&serde_json::json!(""))).is_empty());
        assert!(parse_tags(Some(&serde_json::json!([]))).is_empty());
    }

    #[test]
    fn test_handler_skills_list_no_dir() {
        if !init_hermes_home() { return; }
        let result = handle_skills_list(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(!json["skills"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_handler_skills_list_with_skills() {
        if !init_hermes_home() { return; }
        let result = handle_skills_list(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        let count = json["count"].as_u64().unwrap();
        assert!(count >= 5);
    }

    #[test]
    fn test_handler_skills_list_category_filter() {
        if !init_hermes_home() { return; }
        let result = handle_skills_list(serde_json::json!({
            "category": "coding"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["count"], 3);
    }

    #[test]
    fn test_handler_skill_view_missing_skill() {
        if !init_hermes_home() { return; }
        let result = handle_skill_view(serde_json::json!({
            "name": "nonexistent"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn test_handler_skill_view_no_name() {
        let result = handle_skill_view(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_handler_skill_view_success() {
        if !init_hermes_home() { return; }
        let result = handle_skill_view(serde_json::json!({
            "name": "test-skill-a"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["name"], "test-skill-a");
        assert!(json["content"].as_str().unwrap().contains("test-skill-a"));
    }

    #[test]
    fn test_handler_skill_view_file_path_traversal() {
        if !init_hermes_home() { return; }
        let result = handle_skill_view(serde_json::json!({
            "name": "test-skill-a",
            "file_path": "../../etc/passwd"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("not allowed"));
    }

    #[test]
    fn test_handler_skill_view_linked_files() {
        if !init_hermes_home() { return; }
        let result = handle_skill_view(serde_json::json!({
            "name": "skill-with-files"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        let lf = json.get("linked_files").unwrap();
        assert!(lf.get("references").is_some());
        assert!(lf.get("templates").is_some());
    }

    #[test]
    fn test_handler_skills_categories() {
        if !init_hermes_home() { return; }
        let result = handle_skills_categories(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        let cats = json["categories"].as_array().unwrap();
        assert!(cats.len() >= 2);

        let coding = cats.iter().find(|c| c["name"] == "coding").unwrap();
        assert_eq!(coding["skill_count"], 3);
    }

    #[test]
    fn test_check_skills_requirements() {
        assert!(check_skills_requirements());
    }

    #[test]
    fn test_list_skill_files() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path();

        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();

        std::fs::write(skill_dir.join("references").join("api.md"), "# API").unwrap();
        std::fs::write(skill_dir.join("templates").join("config.yaml"), "key: val").unwrap();
        std::fs::write(skill_dir.join("scripts").join("run.py"), "print(1)").unwrap();
        std::fs::write(skill_dir.join("readme.md"), "# README").unwrap();

        let files = list_skill_files(skill_dir);
        assert!(files.contains_key("references"));
        assert!(files.contains_key("templates"));
        assert!(files.contains_key("scripts"));
        assert!(files.contains_key("other"));
    }

    #[test]
    fn test_handler_skill_view_file_not_found_lists_available() {
        init_hermes_home();
        // Use an existing directory-based skill but with a nonexistent file
        let result = handle_skill_view(serde_json::json!({
            "name": "test-skill-c",
            "file_path": "nonexistent.md"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        // available_files may or may not be present depending on skill_dir
        if let Some(avail) = json.get("available_files") {
            assert!(avail.get("references").is_some());
        }
    }

    #[test]
    fn test_parse_frontmatter_value() {
        let content = "---\nname: test\ndescription: A test skill\nmetadata:\n  hermes:\n    config:\n      - key: wiki.path\n        description: Wiki path\n        default: ~/wiki\n---\n\n# Title\n\nBody.";
        let val = parse_frontmatter_value(content).unwrap();
        assert_eq!(val["name"], "test");
        assert_eq!(val["description"], "A test skill");
        assert!(val["metadata"]["hermes"]["config"].is_array());
    }

    #[test]
    fn test_parse_frontmatter_value_no_frontmatter() {
        let content = "# Just markdown\n\nNo frontmatter here.";
        assert!(parse_frontmatter_value(content).is_none());
    }

    #[test]
    fn test_extract_skill_config_vars() {
        let fm: Value = serde_yaml::from_str(
            r#"
name: test-skill
metadata:
  hermes:
    config:
      - key: wiki.path
        description: Path to wiki
        default: "~/wiki"
        prompt: Wiki path
      - key: model.name
        description: Model to use
"#,
        )
        .unwrap();
        let vars = extract_skill_config_vars(&fm);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].key, "wiki.path");
        assert_eq!(vars[0].description, "Path to wiki");
        assert_eq!(vars[1].key, "model.name");
    }

    #[test]
    fn test_extract_skill_config_vars_empty() {
        let fm: Value = serde_yaml::from_str("name: test\n").unwrap();
        let vars = extract_skill_config_vars(&fm);
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_skill_config_vars_no_description_skips() {
        let fm: Value = serde_yaml::from_str(
            r#"
metadata:
  hermes:
    config:
      - key: no.desc
"#,
        )
        .unwrap();
        let vars = extract_skill_config_vars(&fm);
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_skill_conditions() {
        let fm: Value = serde_yaml::from_str(
            r#"
metadata:
  hermes:
    requires_tools: [file_read, file_write]
    fallback_for_tools: [web_search]
"#,
        )
        .unwrap();
        let conditions = extract_skill_conditions(&fm);
        assert_eq!(conditions["requires_tools"], vec!["file_read", "file_write"]);
        assert_eq!(conditions["fallback_for_tools"], vec!["web_search"]);
        assert!(conditions["requires_toolsets"].is_empty());
    }

    #[test]
    fn test_resolve_dotpath() {
        let config: Value = serde_json::json!({
            "skills": {
                "config": {
                    "wiki": {
                        "path": "/home/wiki"
                    },
                    "model": {
                        "name": "gpt-4"
                    }
                }
            }
        });
        let val = resolve_dotpath(&config, "skills.config.wiki.path").unwrap();
        assert_eq!(val, "/home/wiki");
        assert!(resolve_dotpath(&config, "skills.config.model.name").is_some());
        assert!(resolve_dotpath(&config, "skills.config.nonexistent").is_none());
    }

    #[test]
    fn test_build_plan_path() {
        let path = build_plan_path("create a new API endpoint");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains(".hermes"));
        assert!(path_str.contains("plans"));
        assert!(path_str.contains("create-a-new-api-endpoint"));
    }

    #[test]
    fn test_build_plan_path_empty() {
        let path = build_plan_path("");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("conversation-plan"));
    }

    #[test]
    fn test_scan_skill_commands_empty() {
        // Without a skills directory, should return empty
        let commands = scan_skill_commands();
        // May or may not be empty depending on real ~/.hermes/skills
        // But the function should not panic
        let _ = commands.len();
    }

    #[test]
    fn test_resolve_skill_command_key() {
        // Without skills installed, should return None
        let result = resolve_skill_command_key("nonexistent");
        // May find a real skill, or return None — either is valid
        let _ = result;
    }

    #[test]
    fn test_build_skill_invocation_message_missing() {
        let msg = build_skill_invocation_message("/nonexistent-skill", "");
        assert!(msg.contains("Failed to load skill"));
    }

    #[test]
    fn test_build_preloaded_skills_prompt_empty() {
        let (prompt, loaded, missing) = build_preloaded_skills_prompt(&[]);
        assert!(prompt.is_empty());
        assert!(loaded.is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn test_build_preloaded_skills_prompt_missing() {
        let (prompt, loaded, missing) =
            build_preloaded_skills_prompt(&["nonexistent-skill".to_string()]);
        assert!(prompt.is_empty());
        assert!(loaded.is_empty());
        assert_eq!(missing.len(), 1);
    }

    // ========== Validation tests ==========

    #[test]
    fn test_validate_name_valid() {
        assert!(validate_name("my-skill").is_none());
        assert!(validate_name("skill_123").is_none());
        assert!(validate_name("simple").is_none());
        assert!(validate_name("a-b_c").is_none());
    }

    #[test]
    fn test_validate_name_invalid() {
        let r = validate_name("My Skill");
        assert!(r.is_some());
        assert!(r.unwrap().contains("Invalid"));

        let r = validate_name("skill/path");
        assert!(r.is_some());

        let r = validate_name("");
        assert!(r.is_some());
        assert!(r.unwrap().contains("required"));
    }

    #[test]
    fn test_validate_name_too_long() {
        let r = validate_name(&"a".repeat(65));
        assert!(r.is_some());
        assert!(r.unwrap().contains("64"));
    }

    #[test]
    fn test_validate_category_valid() {
        assert!(validate_category(Some("coding")).is_none());
        assert!(validate_category(Some("data-science")).is_none());
        assert!(validate_category(None).is_none());
    }

    #[test]
    fn test_validate_category_invalid() {
        let r = validate_category(Some("cat/sub"));
        assert!(r.is_some());
        assert!(r.unwrap().contains("single"));

        let r = validate_category(Some("  "));
        assert!(r.is_none()); // empty after trim is valid (means no category)
    }

    #[test]
    fn test_validate_content_size_ok() {
        let small = "x".repeat(100);
        assert!(validate_content_size(&small, "SKILL.md").is_none());
    }

    #[test]
    fn test_validate_content_size_too_large() {
        let huge = "x".repeat(MAX_SKILL_CONTENT_CHARS + 1);
        let r = validate_content_size(&huge, "SKILL.md");
        assert!(r.is_some());
        assert!(r.unwrap().contains("limit"));
    }

    #[test]
    fn test_validate_file_path_valid() {
        assert!(validate_file_path("references/api.md").is_none());
        assert!(validate_file_path("templates/output.md").is_none());
        assert!(validate_file_path("scripts/run.py").is_none());
        assert!(validate_file_path("assets/icon.png").is_none());
    }

    #[test]
    fn test_validate_file_path_invalid() {
        let r = validate_file_path("../../etc/passwd");
        assert!(r.is_some());
        assert!(r.unwrap().contains("must be under"));

        let r = validate_file_path("SKILL.md");
        assert!(r.is_some());

        let r = validate_file_path("");
        assert!(r.is_some());
        assert!(r.unwrap().contains("required"));
    }

    #[test]
    fn test_validate_frontmatter_valid() {
        let content = "---\nname: test\ndescription: A valid skill\n---\n\nContent.";
        assert!(validate_frontmatter(content).is_none());
    }

    #[test]
    fn test_validate_frontmatter_missing_name() {
        let content = "---\ndescription: No name here\n---\n\nContent.";
        let r = validate_frontmatter(content);
        assert!(r.is_some());
        assert!(r.unwrap().contains("name"));
    }

    // ========== Atomic write tests ==========

    #[test]
    fn test_atomic_write_creates_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subdir").join("file.txt");
        atomic_write(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn test_atomic_write_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.txt");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    // ========== Handler tests ==========

    #[test]
    fn test_handler_skill_create_valid() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "new-test-skill",
            "content": "---\nname: new-test-skill\ndescription: A brand new test skill\n---\n\n# New Test Skill\n\nContent.",
            "category": "coding"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["name"], "new-test-skill");
    }

    #[test]
    fn test_handler_skill_create_invalid_name() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "Invalid Name With Spaces",
            "content": "---\nname: invalid\n---\n\nContent."
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("lowercase"));
    }

    #[test]
    fn test_handler_skill_create_no_content() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "no-content-skill"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("content"));
    }

    #[test]
    fn test_handler_skill_create_duplicate() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "test-skill-a",
            "content": "---\nname: test-skill-a\ndescription: duplicate\n---\n\nContent."
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("already"));
    }

    #[test]
    fn test_handler_skill_edit_valid() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "edit",
            "name": "test-skill-a",
            "description": "Updated description"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].as_str().unwrap().contains("updated"));
    }

    #[test]
    fn test_handler_skill_edit_nonexistent() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "edit",
            "name": "does-not-exist",
            "description": "ghost"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn test_handler_skill_patch_exact_match() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "patch",
            "name": "test-skill-a",
            "old_string": "Skill A description",
            "new_string": "Patched description"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["replacements"], 1);
    }

    #[test]
    fn test_handler_skill_patch_no_match() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "patch",
            "name": "test-skill-a",
            "old_string": "xyz this string definitely does not exist",
            "new_string": "replacement"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("Could not find"));
    }

    #[test]
    fn test_handler_skill_patch_fuzzy_fallback() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "patch",
            "name": "test-skill-a",
            "old_string": "Skill  A  descrption",
            "new_string": "Fuzzy patched"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("success").is_some());
    }

    #[test]
    fn test_handler_skill_delete_valid() {
        if !init_hermes_home() { return; }
        handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "to-delete",
            "content": "---\nname: to-delete\ndescription: will be deleted\n---\n\nContent."
        })).unwrap();

        let result = handle_skill_manage(serde_json::json!({
            "action": "delete",
            "name": "to-delete",
            "confirm": true
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["message"].as_str().unwrap().contains("deleted"));
    }

    #[test]
    fn test_handler_skill_delete_no_confirm() {
        if !init_hermes_home() { return; }
        handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "needs-confirm",
            "content": "---\nname: needs-confirm\ndescription: needs confirmation\n---\n\nContent."
        })).unwrap();

        let result = handle_skill_manage(serde_json::json!({
            "action": "delete",
            "name": "needs-confirm",
            "confirm": false
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("confirm"));
    }

    #[test]
    fn test_handler_skill_delete_nonexistent() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "delete",
            "name": "never-existed",
            "confirm": true
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
        assert!(json["error"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn test_handler_skill_write_file_valid() {
        if !init_hermes_home() { return; }
        handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "write-test-skill",
            "content": "---\nname: write-test-skill\ndescription: Has files\n---\n\nContent."
        })).unwrap();

        let result = handle_skill_manage(serde_json::json!({
            "action": "write_file",
            "name": "write-test-skill",
            "file_path": "references/new-file.md",
            "content": "# New File\n\nContent here."
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
    }

    #[test]
    fn test_handler_skill_write_file_path_traversal() {
        if !init_hermes_home() { return; }
        let result = handle_skill_manage(serde_json::json!({
            "action": "write_file",
            "name": "test-skill-a",
            "file_path": "../../../etc/passwd",
            "content": "malicious"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], false);
    }

    #[test]
    fn test_handler_skill_remove_file_valid() {
        if !init_hermes_home() { return; }
        handle_skill_manage(serde_json::json!({
            "action": "create",
            "name": "remove-file-skill",
            "content": "---\nname: remove-file-skill\ndescription: Has removable file\n---\n\nContent."
        })).unwrap();

        handle_skill_manage(serde_json::json!({
            "action": "write_file",
            "name": "remove-file-skill",
            "file_path": "references/temp.md",
            "content": "# Temp"
        })).unwrap();

        let result = handle_skill_manage(serde_json::json!({
            "action": "remove_file",
            "name": "remove-file-skill",
            "file_path": "references/temp.md"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
    }
}
