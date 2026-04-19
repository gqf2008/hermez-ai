#![allow(dead_code)]
//! Lightweight skill metadata utilities shared by prompt_builder and skills_tool.
//!
//! Mirrors the Python `agent/skill_utils.py`. This module avoids importing
//! the tool registry or any heavy dependency chain, so it is safe to import
//! at module level without triggering tool registration or provider resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_yaml::Value as YamlValue;
use walkdir::WalkDir;

use hermes_core::hermes_home::{get_hermes_home, display_hermes_home};

// ── Excluded directories ──────────────────────────────────────────────────

const EXCLUDED_SKILL_DIRS: &[&str] = &[".git", ".github", ".hub"];

/// Platform suffixes mapped to sys.platform equivalents.
const PLATFORM_MAP: &[(&str, &str)] = &[
    ("macos", "darwin"),
    ("linux", "linux"),
    ("windows", "win32"),
];

// ── Config path helpers ───────────────────────────────────────────────────

/// Returns the path to `~/.hermes/config.yaml`.
pub fn get_config_path() -> PathBuf {
    get_hermes_home().join("config.yaml")
}

/// Returns the path to `~/.hermes/skills/`.
pub fn get_skills_dir() -> PathBuf {
    get_hermes_home().join("skills")
}

// ── Frontmatter parsing ──────────────────────────────────────────────────

/// Parse YAML frontmatter from a markdown string.
///
/// Uses `serde_yaml` for full YAML support (nested metadata, lists) with a
/// fallback to simple key:value splitting for robustness.
///
/// Returns `(frontmatter_map, remaining_body)`.
pub fn parse_frontmatter(content: &str) -> (serde_json::Map<String, serde_json::Value>, String) {
    let mut frontmatter = serde_json::Map::new();
    let body = content.to_string();

    if !content.starts_with("---") {
        return (frontmatter, body);
    }

    // Use a regex to find the closing `---` on its own line after the opening `---`
    static FRONT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = FRONT_RE.get_or_init(|| {
        regex::Regex::new(r"^---\s*\n(?P<yaml>(?s:.*?))\n---\s*\n").unwrap()
    });

    let Some(caps) = re.captures(content) else {
        return (frontmatter, body);
    };

    let yaml_content = caps.name("yaml").map(|m| m.as_str()).unwrap_or("");
    let body_start = caps.get(0).map(|m| m.end()).unwrap_or(0);
    let body = content[body_start..].to_string();

    // Try serde_yaml first
    if let Ok(yaml_value) = serde_yaml::from_str::<YamlValue>(yaml_content) {
        if let Some(map) = yaml_value_to_json_map(yaml_value) {
            return (map, body);
        }
    }

    // Fallback: simple key:value parsing for malformed YAML
    for line in yaml_content.lines() {
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim();
            let value = line[colon_pos + 1..].trim();
            if !key.is_empty() {
                frontmatter.insert(key.to_string(), serde_json::Value::String(value.to_string()));
            }
        }
    }

    (frontmatter, body)
}

/// Convert a `serde_yaml::Value` to a `serde_json::Map<String, serde_json::Value>`.
fn yaml_value_to_json_map(value: YamlValue) -> Option<serde_json::Map<String, serde_json::Value>> {
    let json = serde_json::to_value(&value).ok()?;
    match json {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

// ── Platform matching ─────────────────────────────────────────────────────

/// Return `true` when the skill is compatible with the current OS.
///
/// Skills declare platform requirements via a top-level `platforms` list
/// in their YAML frontmatter. If the field is absent or empty, the skill
/// is compatible with all platforms.
pub fn skill_matches_platform(frontmatter: &serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(platforms_value) = frontmatter.get("platforms") else {
        return true;
    };

    let platforms = match platforms_value {
        serde_json::Value::Array(arr) => arr.clone(),
        serde_json::Value::String(s) => {
            vec![serde_json::Value::String(s.clone())]
        }
        _ => return true,
    };

    if platforms.is_empty() {
        return true;
    }

    let current = std::env::consts::OS;

    for platform in &platforms {
        let normalized = platform.as_str().unwrap_or("").trim().to_lowercase();
        let mapped = PLATFORM_MAP
            .iter()
            .find(|(k, _)| *k == normalized)
            .map(|(_, v)| *v)
            .unwrap_or(&normalized);

        if current.starts_with(mapped) {
            return true;
        }
    }

    false
}

// ── Disabled skills ───────────────────────────────────────────────────────

/// Read disabled skill names from config.yaml.
///
/// Args:
///   platform: Explicit platform name (e.g. "telegram"). When `None`,
///   resolves from `HERMES_PLATFORM` or `HERMES_SESSION_PLATFORM` env vars.
///   Falls back to the global disabled list when no platform is determined.
pub fn get_disabled_skill_names(platform: Option<&str>) -> HashSet<String> {
    let config_path = get_config_path();
    if !config_path.exists() {
        return HashSet::new();
    }

    let Ok(content) = std::fs::read_to_string(&config_path) else {
        tracing::debug!("Could not read skill config {}", display_hermes_home());
        return HashSet::new();
    };

    let Ok(yaml_value) = serde_yaml::from_str::<YamlValue>(&content) else {
        return HashSet::new();
    };

    let Ok(json) = serde_json::to_value(yaml_value) else {
        return HashSet::new();
    };

    let Some(obj) = json.as_object() else {
        return HashSet::new();
    };

    let Some(skills_cfg) = obj.get("skills").and_then(|v| v.as_object()) else {
        return HashSet::new();
    };

    // Try platform-specific disabled list
    let hermes_platform = std::env::var("HERMES_PLATFORM").ok();
    let session_platform = std::env::var("HERMES_SESSION_PLATFORM").ok();
    let resolved_platform = platform
        .or(hermes_platform.as_deref())
        .or(session_platform.as_deref());

    if let Some(plat) = resolved_platform {
        if let Some(platform_disabled) = skills_cfg
            .get("platform_disabled")
            .and_then(|v| v.get(plat))
            .and_then(|v| v.as_array())
        {
            return normalize_string_set(platform_disabled);
        }
    }

    // Fall back to global disabled list
    if let Some(disabled) = skills_cfg.get("disabled").and_then(|v| v.as_array()) {
        return normalize_string_set(disabled);
    }

    HashSet::new()
}

fn normalize_string_set(values: &[serde_json::Value]) -> HashSet<String> {
    values
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── External skills directories ──────────────────────────────────────────

/// Read `skills.external_dirs` from config.yaml and return validated paths.
///
/// Each entry is expanded (`~` and `${VAR}`) and resolved to an absolute
/// path. Only directories that actually exist are returned. Duplicates and
/// paths that resolve to the local skills dir are silently skipped.
pub fn get_external_skills_dirs() -> Vec<PathBuf> {
    let config_path = get_config_path();
    if !config_path.exists() {
        return vec![];
    }

    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return vec![];
    };

    let Ok(yaml_value) = serde_yaml::from_str::<YamlValue>(&content) else {
        return vec![];
    };

    let Ok(json) = serde_json::to_value(yaml_value) else {
        return vec![];
    };

    let Some(obj) = json.as_object() else {
        return vec![];
    };

    let Some(skills_cfg) = obj.get("skills").and_then(|v| v.as_object()) else {
        return vec![];
    };

    let raw_dirs = match skills_cfg.get("external_dirs") {
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        Some(serde_json::Value::String(s)) => vec![serde_json::Value::String(s.clone())],
        _ => return vec![],
    };

    let local_skills = get_skills_dir();
    let local_resolved = local_skills.canonicalize().unwrap_or(local_skills);

    let mut seen = HashSet::new();
    let mut result = vec![];

    for entry in &raw_dirs {
        let Some(entry_str) = entry.as_str() else { continue };
        let entry_str = entry_str.trim();
        if entry_str.is_empty() {
            continue;
        }

        let expanded = expand_path(entry_str);
        let Ok(p) = expanded.canonicalize() else {
            tracing::debug!("External skills dir does not exist, skipping: {}", expanded.display());
            continue;
        };

        if p == local_resolved {
            continue;
        }
        if seen.contains(&p) {
            continue;
        }
        if p.is_dir() {
            seen.insert(p.clone());
            result.push(p);
        } else {
            tracing::debug!("External skills dir does not exist, skipping: {}", p.display());
        }
    }

    result
}

/// Expand `~` and `${VAR}` references in a path string.
fn expand_path(s: &str) -> PathBuf {
    // Expand ~ and environment variables using shellexpand
    let expanded = shellexpand::full(s).unwrap_or(std::borrow::Cow::Borrowed(s));
    PathBuf::from(expanded.as_ref())
}

/// Return all skill directories: local skills dir first, then external.
///
/// The local dir is always first (and always included even if it doesn't
/// exist yet — callers handle that). External dirs follow in config order.
pub fn get_all_skills_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![get_skills_dir()];
    dirs.extend(get_external_skills_dirs());
    dirs
}

// ── Condition extraction ──────────────────────────────────────────────────

/// Extract conditional activation fields from parsed frontmatter.
pub fn extract_skill_conditions(
    frontmatter: &serde_json::Map<String, serde_json::Value>,
) -> SkillConditions {
    let metadata = frontmatter
        .get("metadata")
        .and_then(|v| v.as_object())
        .cloned();

    let hermes = metadata
        .as_ref()
        .and_then(|m| m.get("hermes"))
        .and_then(|v| v.as_object());

    SkillConditions {
        fallback_for_toolsets: hermes
            .and_then(|h| h.get("fallback_for_toolsets"))
            .and_then(|v| v.as_array())
            .map(|a| string_array_from_json(a))
            .unwrap_or_default(),
        requires_toolsets: hermes
            .and_then(|h| h.get("requires_toolsets"))
            .and_then(|v| v.as_array())
            .map(|a| string_array_from_json(a))
            .unwrap_or_default(),
        fallback_for_tools: hermes
            .and_then(|h| h.get("fallback_for_tools"))
            .and_then(|v| v.as_array())
            .map(|a| string_array_from_json(a))
            .unwrap_or_default(),
        requires_tools: hermes
            .and_then(|h| h.get("requires_tools"))
            .and_then(|v| v.as_array())
            .map(|a| string_array_from_json(a))
            .unwrap_or_default(),
    }
}

fn string_array_from_json(arr: &[serde_json::Value]) -> Vec<String> {
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_string())
        .collect()
}

/// Conditional activation fields extracted from skill frontmatter.
#[derive(Debug, Clone, Default)]
pub struct SkillConditions {
    pub fallback_for_toolsets: Vec<String>,
    pub requires_toolsets: Vec<String>,
    pub fallback_for_tools: Vec<String>,
    pub requires_tools: Vec<String>,
}

// ── Skill config extraction ───────────────────────────────────────────────

/// A config variable declaration from skill frontmatter.
#[derive(Debug, Clone)]
pub struct SkillConfigVar {
    pub key: String,
    pub description: String,
    pub default: Option<serde_json::Value>,
    pub prompt: String,
    /// Attribution: which skill declared this variable.
    pub skill: Option<String>,
}

/// Extract config variable declarations from parsed frontmatter.
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
pub fn extract_skill_config_vars(
    frontmatter: &serde_json::Map<String, serde_json::Value>,
) -> Vec<SkillConfigVar> {
    let Some(metadata) = frontmatter.get("metadata").and_then(|v| v.as_object()) else {
        return vec![];
    };

    let Some(hermes) = metadata.get("hermes").and_then(|v| v.as_object()) else {
        return vec![];
    };

    let Some(raw) = hermes.get("config") else {
        return vec![];
    };

    let items: Vec<&serde_json::Value> = match raw {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        serde_json::Value::Object(_) => vec![raw],
        _ => return vec![],
    };

    let mut result = vec![];
    let mut seen = HashSet::new();

    for item in items {
        let Some(obj) = item.as_object() else { continue };

        let Some(key_value) = obj.get("key") else { continue };
        let key = key_value.as_str().map(|s| s.trim().to_string());
        let Some(key) = key else { continue };
        if key.is_empty() || seen.contains(&key) {
            continue;
        }

        let Some(desc_value) = obj.get("description") else { continue };
        let desc = desc_value.as_str().map(|s| s.trim().to_string());
        let Some(desc) = desc else { continue };
        if desc.is_empty() {
            continue;
        }

        let default = obj.get("default").cloned();

        let prompt = obj
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| desc.clone());

        seen.insert(key.clone());
        result.push(SkillConfigVar {
            key,
            description: desc,
            default,
            prompt,
            skill: None,
        });
    }

    result
}

/// Storage prefix: all skill config vars are stored under `skills.config.*`
/// in config.yaml. Skill authors declare logical keys (e.g. "wiki.path");
/// the system adds this prefix for storage and strips it for display.
pub const SKILL_CONFIG_PREFIX: &str = "skills.config";

/// Walk a nested JSON object following a dotted key. Returns `None` if any part is missing.
fn resolve_dotpath(config: &serde_json::Value, dotted_key: &str) -> Option<serde_json::Value> {
    let mut current = config;
    for part in dotted_key.split('.') {
        current = current.as_object()?.get(part)?;
    }
    Some(current.clone())
}

/// Resolve current values for skill config vars from config.yaml.
///
/// Skill config is stored under `skills.config.<key>` in config.yaml.
/// Returns a map mapping logical keys (as declared by skills) to their
/// current values (or the declared default if the key isn't set).
/// Path values are expanded.
pub fn resolve_skill_config_values(
    config_vars: &[SkillConfigVar],
) -> std::collections::HashMap<String, serde_json::Value> {
    let config_path = get_config_path();
    let mut config = serde_json::Map::new();

    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(yaml_value) = serde_yaml::from_str::<YamlValue>(&content) {
                if let Ok(json) = serde_json::to_value(yaml_value) {
                    if let Some(map) = json.as_object().cloned() {
                        config = map;
                    }
                }
            }
        }
    }

    let mut resolved = std::collections::HashMap::new();

    for var in config_vars {
        let logical_key = &var.key;
        let storage_key = format!("{SKILL_CONFIG_PREFIX}.{logical_key}");
        let value = resolve_dotpath(&serde_json::Value::Object(config.clone()), &storage_key);

        let value = match value {
            Some(v) if is_meaningful_value(&v) => v,
            _ => var.default.clone().unwrap_or(serde_json::Value::String(String::new())),
        };

        // Expand ~ in path-like string values
        let value = if let Some(s) = value.as_str() {
            if s.contains('~') || s.contains("${") {
                let expanded = expand_path(s);
                serde_json::Value::String(expanded.to_string_lossy().into_owned())
            } else {
                value
            }
        } else {
            value
        };

        resolved.insert(logical_key.clone(), value);
    }

    resolved
}

/// Check if a JSON value is "meaningful" (not null, not empty string).
fn is_meaningful_value(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        _ => true,
    }
}

/// Scan all enabled skills and collect their config variable declarations.
///
/// Walks every skills directory, parses each SKILL.md frontmatter, and returns
/// a deduplicated list of config var dicts. Each entry includes a `skill` field
/// with the skill name for attribution.
///
/// Disabled and platform-incompatible skills are excluded.
pub fn discover_all_skill_config_vars() -> Vec<SkillConfigVar> {
    let disabled = get_disabled_skill_names(None);
    let mut all_vars: Vec<SkillConfigVar> = vec![];
    let mut seen_keys: HashSet<String> = HashSet::new();

    for skills_dir in get_all_skills_dirs() {
        if !skills_dir.is_dir() {
            continue;
        }

        for skill_file in iter_skill_index_files(&skills_dir, "SKILL.md") {
            let Ok(raw) = std::fs::read_to_string(&skill_file) else {
                continue;
            };

            let (frontmatter, _body) = parse_frontmatter(&raw);

            let skill_name = frontmatter
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    skill_file
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string()
                });

            if disabled.contains(&skill_name) {
                continue;
            }
            if !skill_matches_platform(&frontmatter) {
                continue;
            }

            let config_vars = extract_skill_config_vars(&frontmatter);
            for mut var in config_vars {
                if !seen_keys.contains(&var.key) {
                    seen_keys.insert(var.key.clone());
                    var.skill = Some(skill_name.clone());
                    all_vars.push(var);
                }
            }
        }
    }

    all_vars
}

// ── Description extraction ────────────────────────────────────────────────

/// Extract a truncated description from parsed frontmatter.
pub fn extract_skill_description(
    frontmatter: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let Some(raw_desc) = frontmatter.get("description") else {
        return String::new();
    };

    let Some(desc_str) = raw_desc.as_str() else {
        return String::new();
    };

    let desc = desc_str.trim().trim_matches(|c| c == '\'' || c == '"');

    if desc.len() > 60 {
        format!("{}...", &desc[..57])
    } else {
        desc.to_string()
    }
}

// ── File iteration ────────────────────────────────────────────────────────

/// Walk `skills_dir` yielding sorted paths matching `filename` (e.g. "SKILL.md").
///
/// Excludes `.git`, `.github`, `.hub` directories.
pub fn iter_skill_index_files(skills_dir: &Path, filename: &str) -> impl Iterator<Item = PathBuf> {
    let skills_dir = skills_dir.to_path_buf();
    let filename = filename.to_string();

    let mut matches = Vec::new();

    for entry in WalkDir::new(&skills_dir)
        .into_iter()
        .filter_entry(|e| {
            e.file_name()
                .to_str()
                .map(|n| !EXCLUDED_SKILL_DIRS.contains(&n))
                .unwrap_or(false)
        })
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some(&filename)
        {
            matches.push(entry.path().to_path_buf());
        }
    }

    // Sort by relative path for deterministic ordering
    matches.sort_by_key(|p| {
        p.strip_prefix(&skills_dir)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string_lossy().into_owned())
    });

    matches.into_iter()
}

// ── Namespace helpers for plugin-provided skills ───────────────────────────

/// Regex for valid namespace: `[a-zA-Z0-9_-]+`
static NAMESPACE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

fn namespace_re() -> &'static regex::Regex {
    NAMESPACE_RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap())
}

/// Split `'namespace:skill-name'` into `(namespace, bare_name)`.
///
/// Returns `(None, name)` when there is no `':'`.
pub fn parse_qualified_name(name: &str) -> (Option<String>, String) {
    if let Some(pos) = name.find(':') {
        let ns = name[..pos].to_string();
        let bare = name[pos + 1..].to_string();
        (Some(ns), bare)
    } else {
        (None, name.to_string())
    }
}

/// Check whether candidate is a valid namespace (`[a-zA-Z0-9_-]+`).
pub fn is_valid_namespace(candidate: Option<&str>) -> bool {
    match candidate {
        Some(s) => namespace_re().is_match(s),
        None => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_with_yaml() {
        let content = r#"---
name: test-skill
description: A test skill
platforms:
  - linux
  - macos
---

# Skill body content
Some markdown here."#;

        let (frontmatter, body) = parse_frontmatter(content);
        assert_eq!(frontmatter.get("name").and_then(|v| v.as_str()), Some("test-skill"));
        assert_eq!(
            frontmatter.get("description").and_then(|v| v.as_str()),
            Some("A test skill")
        );
        assert!(body.contains("# Skill body content"));
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "# Just a markdown file\nNo frontmatter here.";
        let (frontmatter, body) = parse_frontmatter(content);
        assert!(frontmatter.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_frontmatter_malformed_yaml_fallback() {
        let content = "---\nname: test\ndescription: test desc\n---\nbody";
        let (frontmatter, body) = parse_frontmatter(content);
        // serde_yaml should handle this fine
        assert_eq!(frontmatter.get("name").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(body, "body");
    }

    #[test]
    fn test_skill_matches_platform_all() {
        let frontmatter = serde_json::Map::new();
        assert!(skill_matches_platform(&frontmatter));
    }

    #[test]
    fn test_skill_matches_platform_specific() {
        let frontmatter = serde_json::json!({
            "platforms": ["linux"]
        })
        .as_object()
        .unwrap()
        .clone();

        let result = skill_matches_platform(&frontmatter);
        // Should match if running on linux, not match on other platforms
        let is_linux = std::env::consts::OS == "linux";
        assert_eq!(result, is_linux);
    }

    #[test]
    fn test_skill_matches_platform_string() {
        let frontmatter = serde_json::json!({
            "platforms": "linux"
        })
        .as_object()
        .unwrap()
        .clone();

        let result = skill_matches_platform(&frontmatter);
        let is_linux = std::env::consts::OS == "linux";
        assert_eq!(result, is_linux);
    }

    #[test]
    fn test_normalize_string_set() {
        let values = vec![
            serde_json::Value::String("  foo  ".to_string()),
            serde_json::Value::String("bar".to_string()),
            serde_json::Value::String("  ".to_string()),
            serde_json::Value::String("".to_string()),
        ];
        let result = normalize_string_set(&values);
        assert_eq!(result.len(), 2);
        assert!(result.contains("foo"));
        assert!(result.contains("bar"));
    }

    #[test]
    fn test_extract_skill_conditions() {
        let frontmatter = serde_json::json!({
            "metadata": {
                "hermes": {
                    "fallback_for_toolsets": ["web"],
                    "requires_tools": ["python"]
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let conditions = extract_skill_conditions(&frontmatter);
        assert_eq!(conditions.fallback_for_toolsets, vec!["web"]);
        assert_eq!(conditions.requires_tools, vec!["python"]);
        assert!(conditions.requires_toolsets.is_empty());
        assert!(conditions.fallback_for_tools.is_empty());
    }

    #[test]
    fn test_extract_skill_conditions_missing_metadata() {
        let frontmatter = serde_json::json!({
            "name": "test"
        })
        .as_object()
        .unwrap()
        .clone();

        let conditions = extract_skill_conditions(&frontmatter);
        assert!(conditions.fallback_for_toolsets.is_empty());
    }

    #[test]
    fn test_extract_skill_config_vars() {
        let frontmatter = serde_json::json!({
            "metadata": {
                "hermes": {
                    "config": [
                        {
                            "key": "wiki.path",
                            "description": "Path to wiki",
                            "default": "~/wiki",
                            "prompt": "Wiki path"
                        },
                        {
                            "key": "wiki.enabled",
                            "description": "Enable wiki",
                            "default": true
                        }
                    ]
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let vars = extract_skill_config_vars(&frontmatter);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].key, "wiki.path");
        assert_eq!(vars[0].description, "Path to wiki");
        assert_eq!(vars[0].prompt, "Wiki path");
        assert_eq!(vars[1].key, "wiki.enabled");
        assert_eq!(vars[1].prompt, "Enable wiki"); // defaults to description
    }

    #[test]
    fn test_extract_skill_config_vars_skips_invalid() {
        let frontmatter = serde_json::json!({
            "metadata": {
                "hermes": {
                    "config": [
                        {
                            "key": "",
                            "description": "Missing key"
                        },
                        {
                            "key": "valid.key",
                            "description": ""
                        },
                        "not a dict"
                    ]
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let vars = extract_skill_config_vars(&frontmatter);
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_skill_config_vars_single_object() {
        let frontmatter = serde_json::json!({
            "metadata": {
                "hermes": {
                    "config": {
                        "key": "single.key",
                        "description": "Single var"
                    }
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let vars = extract_skill_config_vars(&frontmatter);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].key, "single.key");
    }

    #[test]
    fn test_extract_skill_description() {
        let frontmatter = serde_json::json!({
            "description": "A short description"
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(extract_skill_description(&frontmatter), "A short description");
    }

    #[test]
    fn test_extract_skill_description_truncated() {
        let frontmatter = serde_json::json!({
            "description": "This is a very long description that should be truncated at exactly sixty characters"
        })
        .as_object()
        .unwrap()
        .clone();

        let desc = extract_skill_description(&frontmatter);
        assert_eq!(desc.len(), 60);
        assert!(desc.ends_with("..."));
    }

    #[test]
    fn test_extract_skill_description_empty() {
        let frontmatter = serde_json::Map::new();
        assert_eq!(extract_skill_description(&frontmatter), "");
    }

    #[test]
    fn test_extract_skill_description_stripped_quotes() {
        let frontmatter = serde_json::json!({
            "description": "'Quoted description'"
        })
        .as_object()
        .unwrap()
        .clone();

        assert_eq!(extract_skill_description(&frontmatter), "Quoted description");
    }

    #[test]
    fn test_parse_qualified_name() {
        let (ns, name) = parse_qualified_name("my-plugin:skill-name");
        assert_eq!(ns, Some("my-plugin".to_string()));
        assert_eq!(name, "skill-name");

        let (ns, name) = parse_qualified_name("just-skill");
        assert_eq!(ns, None);
        assert_eq!(name, "just-skill");
    }

    #[test]
    fn test_is_valid_namespace() {
        assert!(is_valid_namespace(Some("valid-ns_123")));
        assert!(is_valid_namespace(Some("abc")));
        assert!(!is_valid_namespace(Some("invalid ns")));
        assert!(!is_valid_namespace(Some("invalid.ns")));
        assert!(!is_valid_namespace(None));
        assert!(!is_valid_namespace(Some("")));
    }

    #[test]
    fn test_resolve_dotpath() {
        let config = serde_json::json!({
            "skills": {
                "config": {
                    "wiki": {
                        "path": "/some/path",
                        "enabled": true
                    }
                }
            }
        });

        assert_eq!(
            resolve_dotpath(&config, "skills.config.wiki.path"),
            Some(serde_json::Value::String("/some/path".to_string()))
        );
        assert_eq!(
            resolve_dotpath(&config, "skills.config.wiki.enabled"),
            Some(serde_json::Value::Bool(true))
        );
        assert!(resolve_dotpath(&config, "skills.config.missing.key").is_none());
    }

    #[test]
    fn test_resolve_dotpath_missing_parts() {
        let config = serde_json::json!({
            "a": {
                "b": "value"
            }
        });

        assert!(resolve_dotpath(&config, "a.b.c").is_none());
        assert!(resolve_dotpath(&config, "x.y.z").is_none());
    }

    #[test]
    fn test_expand_path_home() {
        let p = expand_path("~/some/path");
        assert!(!p.to_string_lossy().starts_with('~'));
        assert!(p.to_string_lossy().contains("some"));
    }

    #[test]
    fn test_expand_path_env_var() {
        std::env::set_var("TEST_SKILL_DIR", "/tmp/test-skills");
        let p = expand_path("${TEST_SKILL_DIR}/subdir");
        assert!(p.to_string_lossy().starts_with("/tmp/test-skills"));
        std::env::remove_var("TEST_SKILL_DIR");
    }

    #[test]
    fn test_expand_path_unresolved_env() {
        let p = expand_path("${UNLIKELY_VAR_XYZ}/path");
        // Unresolved vars are kept verbatim
        assert!(p.to_string_lossy().contains("${UNLIKELY_VAR_XYZ}"));
    }

    #[test]
    fn test_iter_skill_index_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let files: Vec<_> = iter_skill_index_files(tmp.path(), "SKILL.md").collect();
        assert!(files.is_empty());
    }

    #[test]
    fn test_iter_skill_index_files_with_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("skill-a");
        let dir_b = tmp.path().join("skill-b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join("SKILL.md"), "---\nname: a\n---\nbody").unwrap();
        std::fs::write(dir_b.join("SKILL.md"), "---\nname: b\n---\nbody").unwrap();

        let files: Vec<_> = iter_skill_index_files(tmp.path(), "SKILL.md").collect();
        assert_eq!(files.len(), 2);
        assert!(files[0].to_string_lossy().contains("skill-a"));
        assert!(files[1].to_string_lossy().contains("skill-b"));
    }

    #[test]
    fn test_iter_skill_index_files_excludes_git() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("SKILL.md"), "---\nname: git\n---\n").unwrap();

        let files: Vec<_> = iter_skill_index_files(tmp.path(), "SKILL.md").collect();
        assert!(files.is_empty());
    }

    #[test]
    fn test_is_meaningful_value() {
        assert!(!is_meaningful_value(&serde_json::Value::Null));
        assert!(!is_meaningful_value(&serde_json::Value::String("".to_string())));
        assert!(!is_meaningful_value(&serde_json::Value::String("  ".to_string())));
        assert!(is_meaningful_value(&serde_json::Value::String("hello".to_string())));
        assert!(is_meaningful_value(&serde_json::Value::Bool(true)));
        assert!(is_meaningful_value(&serde_json::Value::Number(42.into())));
    }
}
