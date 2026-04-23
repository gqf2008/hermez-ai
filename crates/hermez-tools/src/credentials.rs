#![allow(dead_code)]
//! Credential file registry.
//!
//! Mirrors the Python `tools/credential_files.py`.
//! Manages credential files that need to be mounted into remote terminal backends
//! (Docker, Modal, SSH, etc.). Supports both skill-declared and config-based mounts.
//!
//! No tools are registered — this module provides functions called by other modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use hermez_core::hermez_home::get_hermez_home;

/// Thread-safe registry of credential file mounts.
/// Maps container_path -> host_path.
static CREDENTIAL_REGISTRY: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Register a single credential file for mounting.
///
/// The `relative_path` is relative to the Hermez home directory.
/// It will be mounted at `{container_base}/{relative_path}` in the container.
pub fn register_credential_file(relative_path: &str, container_base: &str) -> Result<(), String> {
    // Reject absolute paths
    if relative_path.starts_with('/') {
        return Err(format!(
            "Credential path must be relative, got absolute path: {relative_path}"
        ));
    }

    // Reject path traversal
    if relative_path.contains("..") {
        return Err(format!(
            "Credential path must not contain '..': {relative_path}"
        ));
    }

    let hermez_home = get_hermez_home();
    let host_path = hermez_home.join(relative_path);

    // Resolve and verify it stays within HERMEZ_HOME
    let resolved = host_path
        .canonicalize()
        .map_err(|e| format!("Credential file not found: {}: {e}", host_path.display()))?;

    if !resolved.starts_with(&hermez_home) {
        return Err(format!(
            "Credential path resolves outside HERMEZ_HOME: {}",
            resolved.display()
        ));
    }

    let container_path = format!("{}/{}", container_base.trim_end_matches('/'), relative_path);

    let mut registry = CREDENTIAL_REGISTRY.lock().unwrap();
    registry.insert(container_path, resolved.to_string_lossy().to_string());

    Ok(())
}

/// Register multiple credential files from skill frontmatter.
pub fn register_credential_files(entries: &[serde_json::Value], container_base: &str) {
    for entry in entries {
        let path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| entry.get("name").and_then(|v| v.as_str()))
            .or_else(|| entry.as_str());

        if let Some(p) = path {
            if let Err(e) = register_credential_file(p, container_base) {
                tracing::warn!("Failed to register credential file: {e}");
            }
        }
    }
}

/// Get all registered credential file mounts.
///
/// Returns `[{host_path, container_path}]` for all registered files
/// that still exist on disk.
pub fn get_credential_file_mounts() -> Vec<HashMap<String, String>> {
    let registry = CREDENTIAL_REGISTRY.lock().unwrap();
    registry
        .iter()
        .filter(|(_, host_path)| Path::new(host_path).exists())
        .map(|(container_path, host_path)| {
            let mut m = HashMap::new();
            m.insert("host_path".to_string(), host_path.clone());
            m.insert("container_path".to_string(), container_path.clone());
            m
        })
        .collect()
}

/// Clear all registered credential files.
pub fn clear_credential_files() {
    let mut registry = CREDENTIAL_REGISTRY.lock().unwrap();
    registry.clear();
}

/// Register credential files from a skill's `required_credential_files` frontmatter.
pub fn register_skill_credentials(skill_dir: &Path, frontmatter_creds: Option<&serde_json::Value>) {
    let container_base = "/root/.hermez";

    if let Some(Value::Array(entries)) = frontmatter_creds {
        for entry in entries {
            // Skill credentials are relative to the skill directory
            if let Some(rel_path) = entry.as_str() {
                let cred_path = skill_dir.join(rel_path);
                if cred_path.exists() {
                    // Resolve to a path relative to HERMEZ_HOME
                    let hermez_home = get_hermez_home();
                    if let Ok(relative) = cred_path.strip_prefix(&hermez_home) {
                        let rel_str = relative.to_string_lossy().to_string();
                        let _ = register_credential_file(&rel_str, container_base);
                    }
                }
            } else if let Some(obj) = entry.as_object() {
                if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
                    let cred_path = skill_dir.join(path);
                    if cred_path.exists() {
                        let hermez_home = get_hermez_home();
                        if let Ok(relative) = cred_path.strip_prefix(&hermez_home) {
                            let rel_str = relative.to_string_lossy().to_string();
                            let _ = register_credential_file(&rel_str, container_base);
                        }
                    }
                }
            }
        }
    }
}

/// Get the skills directory mount.
///
/// Returns mount info for mounting the skills directory into containers.
pub fn get_skills_directory_mount() -> Option<HashMap<String, String>> {
    let hermez_home = get_hermez_home();
    let skills_dir = hermez_home.join("skills");

    if skills_dir.exists() {
        let mut m = HashMap::new();
        m.insert(
            "host_path".to_string(),
            skills_dir.to_string_lossy().to_string(),
        );
        m.insert("container_path".to_string(), "/root/.hermez/skills".to_string());
        Some(m)
    } else {
        None
    }
}

/// Get cache directory mounts.
///
/// Returns mounts for cache directories (documents, images, audio, screenshots).
pub fn get_cache_directory_mounts() -> Vec<HashMap<String, String>> {
    let hermez_home = get_hermez_home();
    let cache_dirs = [
        ("documents", "/root/.hermez/documents"),
        ("images", "/root/.hermez/images"),
        ("audio", "/root/.hermez/audio"),
        ("screenshots", "/root/.hermez/screenshots"),
    ];

    cache_dirs
        .iter()
        .filter_map(|(dir, container_path)| {
            let host_path = hermez_home.join(dir);
            if host_path.exists() {
                let mut m = HashMap::new();
                m.insert(
                    "host_path".to_string(),
                    host_path.to_string_lossy().to_string(),
                );
                m.insert("container_path".to_string(), container_path.to_string());
                Some(m)
            } else {
                None
            }
        })
        .collect()
}

/// Iterate skills files from a directory (skipping symlinks).
pub fn iter_skills_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip symlinks
            if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                continue;
            }
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                files.extend(iter_skills_files(&path));
            }
        }
    }
    files
}

/// Iterate cache files from a directory (skipping symlinks).
pub fn iter_cache_files(dir: &Path) -> Vec<PathBuf> {
    iter_skills_files(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_absolute_path_rejected() {
        let result = register_credential_file("/etc/passwd", "/root/.hermez");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("absolute"));
    }

    #[test]
    fn test_register_traversal_rejected() {
        let result = register_credential_file("../../.ssh/id_rsa", "/root/.hermez");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("'..'"));
    }

    #[test]
    fn test_register_nonexistent_file() {
        let result = register_credential_file("nonexistent/file.txt", "/root/.hermez");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_register_valid_credential_file() {
        // Create a temp file inside HERMEZ_HOME
        let tmp = std::env::temp_dir();
        let hermez_home = tmp.join("test_cred_home2");
        let _ = std::fs::create_dir_all(&hermez_home);

        let cred_file = hermez_home.join("test_cred.txt");
        std::fs::write(&cred_file, "secret").unwrap();

        // Try to set HERMEZ_HOME — may fail if already cached
        let set_ok = hermez_core::hermez_home::set_hermez_home(&hermez_home).is_ok();
        if !set_ok {
            // Already cached, skip this test
            return;
        }

        let result = register_credential_file("test_cred.txt", "/root/.hermez");
        assert!(result.is_ok());

        let mounts = get_credential_file_mounts();
        assert!(!mounts.is_empty());
        assert!(mounts[0].contains_key("host_path"));
        assert!(mounts[0].contains_key("container_path"));

        clear_credential_files();
        let mounts = get_credential_file_mounts();
        assert!(mounts.is_empty());
    }

    #[test]
    fn test_register_from_json_entries() {
        clear_credential_files();

        let entries = vec![
            serde_json::json!("config.json"),
            serde_json::json!({"path": "api_key.txt"}),
            serde_json::json!({"name": "secret.pem"}),
        ];

        register_credential_files(&entries, "/root/.hermez");
        // These files don't exist, so they won't appear in mounts
        let mounts = get_credential_file_mounts();
        assert!(mounts.is_empty());
    }

    #[test]
    fn test_clear_credential_files() {
        clear_credential_files();
        let mounts = get_credential_file_mounts();
        assert!(mounts.is_empty());
    }

    #[test]
    fn test_iter_skills_files() {
        let tmp = std::env::temp_dir().join("test_skills_iter");
        let _ = std::fs::create_dir_all(&tmp.join("subdir"));
        std::fs::write(tmp.join("file1.txt"), "a").unwrap();
        std::fs::write(tmp.join("subdir/file2.txt"), "b").unwrap();

        let files = iter_skills_files(&tmp);
        assert_eq!(files.len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_iter_cache_files_skips_symlinks() {
        let tmp = std::env::temp_dir().join("test_cache_iter");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("real.txt"), "a").unwrap();

        // Create a symlink
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(tmp.join("real.txt"), tmp.join("link.txt"));
        }

        let files = iter_cache_files(&tmp);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("real.txt"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_get_cache_directory_mounts_nonexistent() {
        // With default HERMEZ_HOME, cache dirs likely don't exist
        let mounts = get_cache_directory_mounts();
        // May or may not be empty depending on setup
        let _ = mounts;
    }

    #[test]
    fn test_get_skills_directory_mount_nonexistent() {
        // With default HERMEZ_HOME, skills dir may or may not exist
        let mount = get_skills_directory_mount();
        // Just verify it doesn't crash
        let _ = mount;
    }
}
