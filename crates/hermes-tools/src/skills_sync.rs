#![allow(dead_code)]
//! Skills sync — manifest-based seeding of bundled skills.
//!
//! Copies bundled skills from the repo's `skills/` directory into
//! `~/.hermes/skills/` and uses a manifest to track which skills
//! have been synced and their origin hash.
//!
//! Mirrors the Python `tools/skills_sync.py`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;

/// Skills directory: ~/.hermes/skills/
fn skills_dir() -> &'static Path {
    static DIR: Lazy<PathBuf> = Lazy::new(|| hermes_core::get_hermes_home().join("skills"));
    DIR.as_path()
}

/// Manifest file: ~/.hermes/skills/.bundled_manifest
fn manifest_path() -> PathBuf {
    skills_dir().join(".bundled_manifest")
}

/// Result of sync_skills().
#[derive(Debug, Default)]
pub struct SyncResult {
    /// Newly copied skill names.
    pub copied: Vec<String>,
    /// Updated skill names.
    pub updated: Vec<String>,
    /// Count of skipped (unchanged or user-deleted).
    pub skipped: usize,
    /// User-modified skill names (kept, not overwritten).
    pub user_modified: Vec<String>,
    /// Skills cleaned from manifest (removed from bundled).
    pub cleaned: Vec<String>,
    /// Total bundled skills found in source.
    pub total_bundled: usize,
}

/// Locate the bundled skills directory.
///
/// Checks `HERMES_BUNDLED_SKILLS` env var first (set by Nix wrapper),
/// then falls back to the repo-relative `skills/` path.
pub fn get_bundled_dir() -> PathBuf {
    if let Ok(env) = std::env::var("HERMES_BUNDLED_SKILLS") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    // Fallback: relative to this crate's location in the workspace
    // hermes-rs/crates/hermes-tools/ -> hermes-rs/skills/
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Go up 3 levels: hermes-tools -> crates -> hermes-rs
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.join("skills"))
        .unwrap_or_else(|| PathBuf::from("skills"))
}

/// Read the manifest as {skill_name -> origin_hash}.
///
/// Handles both v1 (plain names) and v2 (name:hash) formats.
/// v1 entries get an empty hash string which triggers migration.
fn read_manifest() -> BTreeMap<String, String> {
    let path = manifest_path();
    if !path.exists() {
        return BTreeMap::new();
    }
    let Ok(content) = fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let mut result = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(pos) = line.find(':') {
            let name = line[..pos].trim();
            let hash = line[pos + 1..].trim();
            if !name.is_empty() {
                result.insert(name.to_string(), hash.to_string());
            }
        } else {
            // v1 format: plain name — empty hash triggers migration
            result.insert(line.to_string(), String::new());
        }
    }
    result
}

/// Write the manifest atomically (temp file + rename).
fn write_manifest(entries: &BTreeMap<String, String>) {
    let dir = skills_dir();
    let _ = fs::create_dir_all(dir);

    let data: String = entries
        .iter()
        .map(|(name, hash)| format!("{name}:{hash}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let tmp_path = dir.join(".bundled_manifest.tmp");
    let target = manifest_path();

    let result = (|| {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        Ok::< _, std::io::Error>(())
    })();

    if result.is_ok() {
        let _ = fs::rename(&tmp_path, &target);
    }
}

/// Read the skill name from SKILL.md frontmatter, falling back to the directory name.
fn read_skill_name(skill_md: &Path, fallback: &str) -> String {
    let Ok(content) = fs::read_to_string(skill_md) else {
        return fallback.to_string();
    };
    let truncated = if content.len() > 4000 {
        &content[..4000]
    } else {
        &content
    };

    let mut in_frontmatter = false;
    for line in truncated.lines() {
        let stripped = line.trim();
        if stripped == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter && stripped.starts_with("name:") {
            let value = stripped["name:".len()..].trim().trim_matches(|c: char| c == '"' || c == '\'');
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Discover all SKILL.md files in the bundled directory.
///
/// Returns (skill_name, skill_directory) tuples.
fn discover_bundled_skills(bundled_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut skills = Vec::new();
    if !bundled_dir.exists() {
        return skills;
    }

    let exclude_prefixes = &["/.git/", "/.github/", "/.hub/"];

    // Use walkdir to avoid following symlinks
    for entry in walkdir::WalkDir::new(bundled_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
            continue;
        }
        // Check for excluded paths
        let path_str = path.to_string_lossy();
        if exclude_prefixes.iter().any(|p| path_str.contains(p)) {
            continue;
        }
        let skill_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let skill_name = read_skill_name(path, skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown"));
        skills.push((skill_name, skill_dir.to_path_buf()));
    }
    skills
}

/// Compute the destination path preserving category structure.
fn compute_dest(skill_dir: &Path, bundled_dir: &Path) -> PathBuf {
    if let Ok(rel) = skill_dir.strip_prefix(bundled_dir) {
        skills_dir().join(rel)
    } else {
        skills_dir().join(skill_dir.file_name().unwrap_or("unknown".as_ref()))
    }
}

/// Compute MD5 hash of all file contents in a directory.
fn dir_hash(directory: &Path) -> String {
    let mut context = md5::Context::new();
    // Collect all files, sort by relative path
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(directory)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() {
            if let Ok(rel) = path.strip_prefix(directory) {
                files.push((rel.to_path_buf(), path.to_path_buf()));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (rel, abs) in &files {
        context.consume(rel.to_string_lossy().as_bytes());
        if let Ok(data) = fs::read(abs) {
            context.consume(&data);
        }
    }
    format!("{:x}", context.compute())
}

/// Sync bundled skills into ~/.hermes/skills/.
///
/// Returns a `SyncResult` with copied, updated, skipped, user_modified,
/// cleaned, and total_bundled counts.
pub fn sync_skills(quiet: bool) -> SyncResult {
    let bundled_dir = get_bundled_dir();
    if !bundled_dir.exists() {
        return SyncResult::default();
    }

    let _ = fs::create_dir_all(skills_dir());
    let mut manifest = read_manifest();
    let bundled_skills = discover_bundled_skills(&bundled_dir);
    let bundled_names: std::collections::HashSet<String> =
        bundled_skills.iter().map(|(name, _)| name.clone()).collect();

    let mut copied = Vec::new();
    let mut updated = Vec::new();
    let mut user_modified = Vec::new();
    let mut skipped = 0usize;

    for (skill_name, skill_src) in &bundled_skills {
        let dest = compute_dest(skill_src, &bundled_dir);
        let bundled_hash = dir_hash(skill_src);

        if !manifest.contains_key(skill_name) {
            // ── New skill — never offered before ──
            if dest.exists() {
                // User already has a skill with same name — don't overwrite
                skipped += 1;
                manifest.insert(skill_name.clone(), bundled_hash);
            } else {
                let _ = fs::create_dir_all(dest.parent().unwrap());
                if copy_dir_all(skill_src, &dest).is_ok() {
                    copied.push(skill_name.clone());
                    manifest.insert(skill_name.clone(), bundled_hash);
                    if !quiet {
                        tracing::info!("  + {skill_name}");
                    }
                } else if !quiet {
                    tracing::warn!("  ! Failed to copy {skill_name}");
                }
                // Do NOT add to manifest on failure — next sync should retry
            }
        } else if dest.exists() {
            // ── Existing skill — in manifest AND on disk ──
            let origin_hash = manifest.get(skill_name).cloned().unwrap_or_default();
            let user_hash = dir_hash(&dest);

            if origin_hash.is_empty() {
                // v1 migration: no origin hash recorded
                manifest.insert(skill_name.clone(), user_hash);
                skipped += 1; // baseline set, skip update
                continue;
            }

            if user_hash != origin_hash {
                // User modified — don't overwrite
                user_modified.push(skill_name.clone());
                if !quiet {
                    tracing::info!("  ~ {skill_name} (user-modified, skipping)");
                }
                continue;
            }

            // User copy matches origin — check if bundled changed
            if bundled_hash != origin_hash {
                let backup = dest.with_extension("bak");
                // Move old copy to backup
                if fs::rename(&dest, &backup).is_ok() {
                    if fs::create_dir_all(dest.parent().unwrap()).is_ok()
                        && copy_dir_all(skill_src, &dest).is_ok()
                    {
                        manifest.insert(skill_name.clone(), bundled_hash);
                        updated.push(skill_name.clone());
                        if !quiet {
                            tracing::info!("  ↑ {skill_name} (updated)");
                        }
                        let _ = fs::remove_dir_all(&backup);
                    } else {
                        // Restore from backup
                        let _ = fs::rename(&backup, &dest);
                        if !quiet {
                            tracing::warn!("  ! Failed to update {skill_name}");
                        }
                    }
                } else if !quiet {
                    tracing::warn!("  ! Failed to backup {skill_name}");
                }
            } else {
                skipped += 1; // both unchanged
            }
        } else {
            // ── In manifest but not on disk — user deleted it ──
            skipped += 1;
        }
    }

    // Clean stale manifest entries (skills removed from bundled dir)
    let cleaned: Vec<String> = manifest
        .keys()
        .filter(|name| !bundled_names.contains(*name))
        .cloned()
        .collect();
    for name in &cleaned {
        manifest.remove(name);
    }

    // Copy DESCRIPTION.md files for categories (if not already present)
    if bundled_dir.exists() {
        for entry in walkdir::WalkDir::new(&bundled_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some("DESCRIPTION.md") {
                if let Ok(rel) = path.strip_prefix(&bundled_dir) {
                    let dest_desc = skills_dir().join(rel);
                    if !dest_desc.exists() {
                        if let Some(parent) = dest_desc.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        let _ = fs::copy(path, &dest_desc);
                    }
                }
            }
        }
    }

    write_manifest(&manifest);

    SyncResult {
        copied,
        updated,
        skipped,
        user_modified,
        cleaned,
        total_bundled: bundled_skills.len(),
    }
}

/// Recursively copy a directory.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_hash_same_content() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        fs::write(tmp1.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp2.path().join("a.txt"), "hello").unwrap();
        assert_eq!(dir_hash(tmp1.path()), dir_hash(tmp2.path()));
    }

    #[test]
    fn test_dir_hash_different_content() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        fs::write(tmp1.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp2.path().join("a.txt"), "world").unwrap();
        assert_ne!(dir_hash(tmp1.path()), dir_hash(tmp2.path()));
    }

    #[test]
    fn test_read_skill_name_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("SKILL.md");
        fs::write(
            &md,
            "---\nname: my-test-skill\ndescription: A test\n---\n\n# Content",
        )
        .unwrap();
        assert_eq!(read_skill_name(&md, "fallback"), "my-test-skill");
    }

    #[test]
    fn test_read_skill_name_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("SKILL.md");
        fs::write(&md, "# No frontmatter").unwrap();
        assert_eq!(read_skill_name(&md, "fallback"), "fallback");
    }

    #[test]
    fn test_read_skill_name_fallback_on_missing_file() {
        assert_eq!(
            read_skill_name(Path::new("/nonexistent/SKILL.md"), "fallback"),
            "fallback"
        );
    }

    #[test]
    fn test_manifest_roundtrip() {
        // Use a temp dir for this test
        let _tmp = tempfile::tempdir().unwrap();
        let mut entries = BTreeMap::new();
        entries.insert("skill-a".to_string(), "abc123".to_string());
        entries.insert("skill-b".to_string(), "def456".to_string());

        let data: String = entries
            .iter()
            .map(|(n, h)| format!("{n}:{h}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";

        // Parse back
        let mut parsed = BTreeMap::new();
        for line in data.lines() {
            if let Some(pos) = line.find(':') {
                parsed.insert(line[..pos].to_string(), line[pos + 1..].to_string());
            }
        }
        assert_eq!(entries, parsed);
    }

    #[test]
    fn test_copy_dir_all() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), "hello").unwrap();
        fs::create_dir_all(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub/b.txt"), "world").unwrap();

        copy_dir_all(src.path(), dst.path()).unwrap();
        assert!(dst.path().join("a.txt").exists());
        assert!(dst.path().join("sub/b.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_sync_result_default() {
        let result = SyncResult::default();
        assert!(result.copied.is_empty());
        assert!(result.updated.is_empty());
        assert_eq!(result.skipped, 0);
        assert!(result.user_modified.is_empty());
        assert!(result.cleaned.is_empty());
        assert_eq!(result.total_bundled, 0);
    }

    #[test]
    fn test_sync_missing_bundled_dir() {
        // Point to a nonexistent directory
        std::env::set_var("HERMES_BUNDLED_SKILLS", "/nonexistent/path/xyz");
        let result = sync_skills(true);
        assert_eq!(result.total_bundled, 0);
        assert!(result.copied.is_empty());
        std::env::remove_var("HERMES_BUNDLED_SKILLS");
    }
}
