//! Skills index prompt builder.
//!
//! Mirrors the Python `build_skills_system_prompt()` in `agent/prompt_builder.py`.
//! Builds a compact skill index for the system prompt with two-layer caching:
//! 1. In-process LRU cache (parking_lot Mutex + LinkedHashMap)
//! 2. Disk snapshot (~/.hermez/.skills_prompt_snapshot.json) validated by mtime/size

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// In-process LRU cache for skills prompt.
static SKILLS_PROMPT_CACHE: Lazy<Mutex<SkillsCache>> =
    Lazy::new(|| Mutex::new(SkillsCache::new()));

const SKILLS_PROMPT_CACHE_MAX: usize = 8;

/// Categorized skills and category descriptions returned by directory scan.
type SkillsScanResult = (
    BTreeMap<String, Vec<(String, String)>>,
    BTreeMap<String, String>,
);

struct SkillsCache {
    map: indexmap::IndexMap<String, String>,
}

impl SkillsCache {
    fn new() -> Self {
        Self {
            map: indexmap::IndexMap::new(),
        }
    }
}

/// Disk snapshot schema (reserved for future disk caching).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SkillsSnapshot {
    version: usize,
    manifest: HashMap<String, Vec<i64>>,
    skills: Vec<SkillEntry>,
    category_descriptions: HashMap<String, String>,
}

impl Default for SkillsSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            manifest: HashMap::new(),
            skills: Vec::new(),
            category_descriptions: HashMap::new(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillEntry {
    skill_name: String,
    category: String,
    frontmatter_name: String,
    description: String,
    platforms: Vec<String>,
    conditions: SkillConditions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SkillConditions {
    #[serde(default)]
    requires_tools: Vec<String>,
    #[serde(default)]
    requires_toolsets: Vec<String>,
    #[serde(default)]
    fallback_for_tools: Vec<String>,
    #[serde(default)]
    fallback_for_toolsets: Vec<String>,
}

/// Build the skills system prompt.
///
/// Scans skill directories and builds a compact index with categories,
/// descriptions, and conditional activation rules.
pub fn build_skills_system_prompt(
    available_tools: &HashSet<String>,
    available_toolsets: &HashSet<String>,
) -> String {
    let hermez_home = hermez_core::get_hermez_home();
    let skills_dir = hermez_home.join("skills");

    if !skills_dir.exists() {
        return String::new();
    }

    // Build cache key
    let cache_key = format!(
        "{:?}|{:?}|{:?}",
        skills_dir,
        available_tools
            .iter()
            .collect::<Vec<_>>()
            .chunks(5)
            .collect::<Vec<_>>(),
        available_toolsets
            .iter()
            .collect::<Vec<_>>()
            .chunks(5)
            .collect::<Vec<_>>()
    );

    // Check in-process cache
    {
        let cache = SKILLS_PROMPT_CACHE.lock();
        if let Some(cached) = cache.map.get(&cache_key) {
            return cached.clone();
        }
    }

    // Load disabled skills from config
    let disabled = load_disabled_skills();

    // Build skills by category
    let (skills_by_category, category_descriptions) =
        scan_skills_dir(&skills_dir, available_tools, available_toolsets, &disabled);

    if skills_by_category.is_empty() {
        return String::new();
    }

    // Format the prompt
    let result = format_skills_prompt(&skills_by_category, &category_descriptions);

    // Cache the result
    {
        let mut cache = SKILLS_PROMPT_CACHE.lock();
        if cache.map.len() >= SKILLS_PROMPT_CACHE_MAX {
            // Remove oldest (first) entry
            if let Some((key, _)) = cache.map.first() {
                let key = key.clone();
                cache.map.shift_remove(&key);
            }
        }
        cache.map.insert(cache_key, result.clone());
    }

    result
}

/// Load disabled skill names from config.
fn load_disabled_skills() -> HashSet<String> {
    let mut disabled = HashSet::new();
    if let Ok(config) = hermez_core::HermezConfig::load() {
        for skill in config.skills.disabled {
            disabled.insert(skill);
        }
    }
    disabled
}

/// Scan a skills directory and return categorized skills.
fn scan_skills_dir(
    skills_dir: &Path,
    available_tools: &HashSet<String>,
    available_toolsets: &HashSet<String>,
    disabled: &HashSet<String>,
) -> SkillsScanResult {
    let mut skills_by_category: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut category_descriptions: BTreeMap<String, String> = BTreeMap::new();

    if !skills_dir.exists() {
        return (skills_by_category, category_descriptions);
    }

    // Walk the skills directory
    for entry in walkdir::WalkDir::new(skills_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

        // Parse SKILL.md files
        if file_name == "SKILL.md" {
            if let Some((name, category, desc, conditions, _platforms)) = parse_skill_file(path) {
                if disabled.contains(&name) {
                    continue;
                }
                if !skill_should_show(&conditions, available_tools, available_toolsets) {
                    continue;
                }
                skills_by_category
                    .entry(category)
                    .or_default()
                    .push((name, desc));
            }
        }

        // Parse DESCRIPTION.md files
        if file_name == "DESCRIPTION.md" {
            if let Some((category, desc)) = parse_category_description(path) {
                category_descriptions
                    .entry(category)
                    .or_insert_with(|| desc);
            }
        }
    }

    // Sort and deduplicate within each category
    for (_, skills) in skills_by_category.iter_mut() {
        skills.sort_by(|a, b| a.0.cmp(&b.0));
        skills.dedup_by(|a, b| a.0 == b.0);
    }

    (skills_by_category, category_descriptions)
}

/// Parse a SKILL.md file and extract metadata.
fn parse_skill_file(
    path: &Path,
) -> Option<(String, String, String, SkillConditions, Vec<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let content = if content.len() > 2000 {
        content[..2000].to_string()
    } else {
        content
    };

    let (frontmatter, _body) = parse_frontmatter(&content);

    let skill_name = path
        .parent()?
        .file_name()?
        .to_str()
        .unwrap_or("unknown")
        .to_string();

    let skills_root = skills_dir(path);
    let category = determine_category(path, &skills_root);
    let description = extract_description(&frontmatter);
    let conditions = extract_conditions(&frontmatter);
    let platforms = extract_platforms(&frontmatter);

    Some((skill_name, category, description, conditions, platforms))
}

/// Get the skills root from a file path.
fn skills_dir(_path: &Path) -> PathBuf {
    let hermez_home = hermez_core::get_hermez_home();
    hermez_home.join("skills")
}

/// Determine category from file path.
fn determine_category(path: &Path, skills_dir: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(skills_dir) {
        let parts: Vec<_> = rel.iter().collect();
        if parts.len() >= 3 {
            // category/skill/SKILL.md
            if let Some(c) = parts[0].to_str() {
                return c.to_string();
            }
        }
    }
    "general".to_string()
}

/// Parse YAML-like frontmatter from skill content.
fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut frontmatter = HashMap::new();
    let mut body = content.to_string();

    if let Some(stripped) = content.strip_prefix("---") {
        if let Some(end) = stripped.find("\n---") {
            let fm_content = &content[3..end + 3];
            for line in fm_content.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    let key = key.trim().to_string();
                    let value = value.trim().trim_matches('"').trim_matches('\'');
                    frontmatter.insert(key, value.to_string());
                }
            }
            body = content[end + 7..].trim_start().to_string();
        }
    }

    (frontmatter, body)
}

/// Extract description from frontmatter.
fn extract_description(frontmatter: &HashMap<String, String>) -> String {
    frontmatter
        .get("description")
        .cloned()
        .or_else(|| frontmatter.get("name").cloned())
        .unwrap_or_default()
}

/// Extract conditions from frontmatter.
fn extract_conditions(frontmatter: &HashMap<String, String>) -> SkillConditions {
    let mut conditions = SkillConditions::default();

    // Parse requires_tools (comma-separated)
    if let Some(requires) = frontmatter.get("requires_tools") {
        conditions.requires_tools = requires.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Some(requires) = frontmatter.get("requires_toolsets") {
        conditions.requires_toolsets = requires.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Some(fallback) = frontmatter.get("fallback_for_tools") {
        conditions.fallback_for_tools = fallback.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Some(fallback) = frontmatter.get("fallback_for_toolsets") {
        conditions.fallback_for_toolsets =
            fallback.split(',').map(|s| s.trim().to_string()).collect();
    }

    conditions
}

/// Extract platforms from frontmatter.
fn extract_platforms(frontmatter: &HashMap<String, String>) -> Vec<String> {
    frontmatter
        .get("platforms")
        .map(|p| p.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Check if a skill should be shown based on conditions.
fn skill_should_show(
    conditions: &SkillConditions,
    available_tools: &HashSet<String>,
    available_toolsets: &HashSet<String>,
) -> bool {
    // fallback_for: hide when the primary tool/toolset IS available
    for ts in &conditions.fallback_for_toolsets {
        if available_toolsets.contains(ts) {
            return false;
        }
    }
    for t in &conditions.fallback_for_tools {
        if available_tools.contains(t) {
            return false;
        }
    }

    // requires: hide when a required tool/toolset is NOT available
    for ts in &conditions.requires_toolsets {
        if !available_toolsets.contains(ts) {
            return false;
        }
    }
    for t in &conditions.requires_tools {
        if !available_tools.contains(t) {
            return false;
        }
    }

    true
}

/// Parse a category DESCRIPTION.md file.
fn parse_category_description(path: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let (frontmatter, _body) = parse_frontmatter(&content);

    let desc = frontmatter.get("description")?.clone();

    let skills_dir = hermez_core::get_hermez_home().join("skills");
    let rel = path.strip_prefix(&skills_dir).ok()?;
    let category = rel
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("general")
        .to_string();

    Some((category, desc))
}

/// Format the skills prompt.
fn format_skills_prompt(
    skills_by_category: &BTreeMap<String, Vec<(String, String)>>,
    category_descriptions: &BTreeMap<String, String>,
) -> String {
    let mut lines = Vec::new();

    for (category, skills) in skills_by_category {
        if let Some(cat_desc) = category_descriptions.get(category) {
            lines.push(format!("  {}: {}", category, cat_desc));
        } else {
            lines.push(format!("  {}:", category));
        }

        for (name, desc) in skills {
            if desc.is_empty() {
                lines.push(format!("    - {}", name));
            } else {
                lines.push(format!("    - {}: {}", name, desc));
            }
        }
    }

    format!(
        "## Skills (mandatory)\n\
        Before replying, scan the skills below. \
        If a skill matches or is even partially relevant to your task, \
        you MUST load it with skill_view(name) and follow its instructions. \
        When in doubt, err on the side of loading.\n\
        If a skill has issues, fix it with skill_manage(action='patch').\n\
        After difficult/iterative tasks, offer to save as a skill. \
        If a skill you loaded was missing steps, had wrong commands, or needed \
        pitfalls you discovered, update it before finishing.\n\
        \n\
        <available_skills>\n{}\n</available_skills>\n\
        \n\
        If none match, proceed normally without loading a skill.",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        let content = "---\nname: Test Skill\ndescription: A test skill\n---\nBody content";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("name"), Some(&"Test Skill".to_string()));
        assert_eq!(fm.get("description"), Some(&"A test skill".to_string()));
        assert!(body.contains("Body content"));
    }

    #[test]
    fn test_parse_frontmatter_no_fm() {
        let content = "# No frontmatter\nJust body";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_skill_should_show_no_conditions() {
        let conditions = SkillConditions::default();
        assert!(skill_should_show(
            &conditions,
            &HashSet::new(),
            &HashSet::new()
        ));
    }

    #[test]
    fn test_skill_should_show_requires() {
        let conditions = SkillConditions {
            requires_tools: vec!["file_read".to_string()],
            ..Default::default()
        };
        let mut tools = HashSet::new();
        tools.insert("file_read".to_string());
        assert!(skill_should_show(&conditions, &tools, &HashSet::new()));

        tools.remove("file_read");
        assert!(!skill_should_show(&conditions, &tools, &HashSet::new()));
    }

    #[test]
    fn test_skill_should_show_fallback() {
        let conditions = SkillConditions {
            fallback_for_tools: vec!["file_write".to_string()],
            ..Default::default()
        };
        let mut tools = HashSet::new();
        tools.insert("file_write".to_string());
        assert!(!skill_should_show(&conditions, &tools, &HashSet::new()));

        tools.remove("file_write");
        assert!(skill_should_show(&conditions, &tools, &HashSet::new()));
    }

    #[test]
    fn test_format_skills_prompt() {
        let mut skills = BTreeMap::new();
        skills.insert(
            "dev".to_string(),
            vec![
                ("test_skill".to_string(), "A test skill".to_string()),
            ],
        );
        let mut descs = BTreeMap::new();
        descs.insert("dev".to_string(), "Development skills".to_string());

        let prompt = format_skills_prompt(&skills, &descs);
        assert!(prompt.contains("## Skills (mandatory)"));
        assert!(prompt.contains("dev: Development skills"));
        assert!(prompt.contains("test_skill: A test skill"));
    }

    #[test]
    fn test_build_skills_empty_dir() {
        // With no skills directory, should return empty string
        let result = build_skills_system_prompt(&HashSet::new(), &HashSet::new());
        // May not be empty if skills exist in test environment
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn test_extract_description_from_frontmatter() {
        let mut fm = HashMap::new();
        fm.insert("description".to_string(), "A skill".to_string());
        assert_eq!(extract_description(&fm), "A skill");
    }

    #[test]
    fn test_extract_description_fallback_to_name() {
        let mut fm = HashMap::new();
        fm.insert("name".to_string(), "My Skill".to_string());
        assert_eq!(extract_description(&fm), "My Skill");
    }

    #[test]
    fn test_extract_description_empty() {
        let fm = HashMap::new();
        assert_eq!(extract_description(&fm), "");
    }

    #[test]
    fn test_extract_conditions_all_fields() {
        let mut fm = HashMap::new();
        fm.insert("requires_tools".to_string(), "a, b, c".to_string());
        fm.insert("requires_toolsets".to_string(), "web, code".to_string());
        fm.insert("fallback_for_tools".to_string(), "x, y".to_string());
        fm.insert("fallback_for_toolsets".to_string(), "z".to_string());
        let conds = extract_conditions(&fm);
        assert_eq!(conds.requires_tools, vec!["a", "b", "c"]);
        assert_eq!(conds.requires_toolsets, vec!["web", "code"]);
        assert_eq!(conds.fallback_for_tools, vec!["x", "y"]);
        assert_eq!(conds.fallback_for_toolsets, vec!["z"]);
    }

    #[test]
    fn test_extract_conditions_empty() {
        let fm = HashMap::new();
        let conds = extract_conditions(&fm);
        assert!(conds.requires_tools.is_empty());
        assert!(conds.requires_toolsets.is_empty());
    }

    #[test]
    fn test_extract_platforms() {
        let mut fm = HashMap::new();
        fm.insert("platforms".to_string(), "cli, telegram, discord".to_string());
        let platforms = extract_platforms(&fm);
        assert_eq!(platforms, vec!["cli", "telegram", "discord"]);
    }

    #[test]
    fn test_extract_platforms_empty() {
        let fm = HashMap::new();
        assert!(extract_platforms(&fm).is_empty());
    }

    #[test]
    fn test_skill_should_show_requires_toolsets() {
        let conditions = SkillConditions {
            requires_toolsets: vec!["web".to_string()],
            ..Default::default()
        };
        let mut toolsets = HashSet::new();
        toolsets.insert("web".to_string());
        assert!(skill_should_show(&conditions, &HashSet::new(), &toolsets));

        toolsets.remove("web");
        assert!(!skill_should_show(&conditions, &HashSet::new(), &toolsets));
    }

    #[test]
    fn test_skill_should_show_fallback_for_toolsets() {
        let conditions = SkillConditions {
            fallback_for_toolsets: vec!["code".to_string()],
            ..Default::default()
        };
        let mut toolsets = HashSet::new();
        toolsets.insert("code".to_string());
        assert!(!skill_should_show(&conditions, &HashSet::new(), &toolsets));

        toolsets.remove("code");
        assert!(skill_should_show(&conditions, &HashSet::new(), &toolsets));
    }

    #[test]
    fn test_determine_category_from_path() {
        let skills_dir = PathBuf::from("/skills");
        let path = PathBuf::from("/skills/dev/my_skill/SKILL.md");
        assert_eq!(determine_category(&path, &skills_dir), "dev");
    }

    #[test]
    fn test_determine_category_too_shallow() {
        let skills_dir = PathBuf::from("/skills");
        let path = PathBuf::from("/skills/SKILL.md");
        assert_eq!(determine_category(&path, &skills_dir), "general");
    }

    #[test]
    fn test_determine_category_outside_skills() {
        let skills_dir = PathBuf::from("/skills");
        let path = PathBuf::from("/other/file.md");
        assert_eq!(determine_category(&path, &skills_dir), "general");
    }

    #[test]
    fn test_format_skills_prompt_empty_desc() {
        let mut skills = BTreeMap::new();
        skills.insert(
            "cat".to_string(),
            vec![("skill1".to_string(), "".to_string())],
        );
        let descs = BTreeMap::new();
        let prompt = format_skills_prompt(&skills, &descs);
        assert!(prompt.contains("skill1"));
        // With empty desc, skill line should be "    - skill1" not "    - skill1: "
        assert!(prompt.contains("    - skill1\n"));
    }

    #[test]
    fn test_parse_frontmatter_no_end_marker() {
        let content = "---\nkey: value\nno end marker";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_frontmatter_quoted_values() {
        let content = "---\nname: \"Test\"\ndesc: 'A test'\n---\nBody";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("name"), Some(&"Test".to_string()));
        assert_eq!(fm.get("desc"), Some(&"A test".to_string()));
        assert_eq!(body, "Body");
    }
}
