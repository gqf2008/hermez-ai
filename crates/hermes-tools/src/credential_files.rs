#![allow(dead_code)]
//! File passthrough registry for remote terminal backends.
//!
//! Remote backends (Docker, Modal, SSH) create sandboxes with no host files.
//! This module ensures that credential files, skill directories, and host-side
//! cache directories are mounted or synced into those sandboxes.
//!
//! Mirrors the Python `tools/credential_files.py`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;

use hermes_core::hermes_home::get_hermes_home;

/// Session-scoped registry of credential files.
/// Each entry: container_path -> host_path
static REGISTERED_FILES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Cache for config-based file list (loaded once).
static CONFIG_FILES: OnceLock<Vec<MountEntry>> = OnceLock::new();

/// A mount entry mapping container path to host path.
#[derive(Debug, Clone)]
pub struct MountEntry {
    pub host_path: String,
    pub container_path: String,
}

/// The default container base path for credential files.
const CONTAINER_BASE: &str = "/root/.hermes";

/// Cache subdirectories that should be mirrored into remote backends.
/// Each tuple is (new_subpath, old_name) for backward compatibility.
const CACHE_DIRS: &[(&str, &str)] = &[
    ("cache/documents", "document_cache"),
    ("cache/images", "image_cache"),
    ("cache/audio", "audio_cache"),
    ("cache/screenshots", "browser_screenshots"),
];

/// Register a credential file for mounting into remote sandboxes.
///
/// `relative_path` is relative to HERMES_HOME (e.g. "google_token.json").
/// Returns `true` if the file exists on the host and was registered.
///
/// Security: rejects absolute paths and path traversal sequences (`..`).
/// The resolved host path must remain inside HERMES_HOME so that a malicious
/// skill cannot declare `required_credential_files: ['../../.ssh/id_rsa']`
/// and exfiltrate sensitive host files into a container sandbox.
pub fn register_credential_file(relative_path: &str, container_base: &str) -> bool {
    let hermes_home = get_hermes_home();

    // Reject absolute paths
    if Path::new(relative_path).is_absolute() {
        tracing::warn!(
            "credential_files: rejected absolute path {:?} (must be relative to HERMES_HOME)",
            relative_path
        );
        return false;
    }

    let host_path = hermes_home.join(relative_path);

    // Resolve symlinks and normalise `..` before the containment check
    let resolved = match host_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            tracing::debug!("credential_files: skipping {} (not found)", host_path.display());
            return false;
        }
    };

    let hermes_home_resolved = match hermes_home.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!("credential_files: HERMES_HOME does not exist: {}", hermes_home.display());
            return false;
        }
    };

    if !resolved.starts_with(&hermes_home_resolved) {
        tracing::warn!(
            "credential_files: rejected path traversal {:?} (resolves to {}, outside HERMES_HOME {})",
            relative_path,
            resolved.display(),
            hermes_home_resolved.display()
        );
        return false;
    }

    if !resolved.is_file() {
        tracing::debug!("credential_files: skipping {} (not found)", resolved.display());
        return false;
    }

    let container_path = format!(
        "{}/{}",
        container_base.trim_end_matches('/'),
        relative_path
    );

    {
        let mut reg = REGISTERED_FILES.lock();
        reg.get_or_insert_with(HashMap::new)
            .insert(container_path.clone(), resolved.to_string_lossy().to_string());
    }

    tracing::debug!(
        "credential_files: registered {} -> {}",
        resolved.display(),
        container_path
    );
    true
}

/// Register multiple credential files from skill frontmatter entries.
///
/// Each entry is either a string (relative path) or a dict with a "path" key.
/// Returns the list of relative paths that were NOT found on the host.
pub fn register_credential_files(entries: &[serde_json::Value], container_base: &str) -> Vec<String> {
    let mut missing = Vec::new();
    for entry in entries {
        let rel_path = if let Some(s) = entry.as_str() {
            s.trim().to_string()
        } else if let Some(obj) = entry.as_object() {
            obj.get("path")
                .or_else(|| obj.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        } else {
            continue;
        };

        if rel_path.is_empty() {
            continue;
        }
        if !register_credential_file(&rel_path, container_base) {
            missing.push(rel_path);
        }
    }
    missing
}

/// Return all credential files that should be mounted into remote sandboxes.
///
/// Each item has `host_path` and `container_path` keys.
/// Combines skill-registered files and user config.
pub fn get_credential_file_mounts() -> Vec<MountEntry> {
    let mut mounts: HashMap<String, String> = HashMap::new();

    // Skill-registered files
    {
        let reg = REGISTERED_FILES.lock();
        if let Some(ref registered) = *reg {
            for (container_path, host_path) in registered {
                if Path::new(host_path).is_file() {
                    mounts.insert(container_path.clone(), host_path.clone());
                }
            }
        }
    }

    // Config-based files
    for entry in load_config_files() {
        let cp = &entry.container_path;
        if !mounts.contains_key(cp) && Path::new(&entry.host_path).is_file() {
            mounts.insert(cp.clone(), entry.host_path.clone());
        }
    }

    mounts
        .into_iter()
        .map(|(container_path, host_path)| MountEntry {
            host_path,
            container_path,
        })
        .collect()
}

/// Reset the skill-scoped registry (e.g. on session reset).
pub fn clear_credential_files() {
    *REGISTERED_FILES.lock() = None;
}

/// Load `terminal.credential_files` from config.yaml (cached).
fn load_config_files() -> Vec<MountEntry> {
    CONFIG_FILES
        .get_or_init(|| {
            let mut result = Vec::new();
            let hermes_home = get_hermes_home();

            let config_path = hermes_home.join("config.yaml");
            if !config_path.exists() {
                return result;
            }

            let content = match std::fs::read_to_string(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Could not read config.yaml: {}", e);
                    return result;
                }
            };

            let config: serde_yaml::Value = match serde_yaml::from_str(&content) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Could not parse config.yaml: {}", e);
                    return result;
                }
            };

            let hermes_home_resolved = hermes_home
                .canonicalize()
                .unwrap_or_else(|_| hermes_home.clone());

            if let Some(cred_files) = config
                .get("terminal")
                .and_then(|t| t.get("credential_files"))
                .and_then(|c| c.as_sequence())
            {
                for item in cred_files {
                    if let Some(rel) = item.as_str() {
                        let rel = rel.trim();
                        if rel.is_empty() {
                            continue;
                        }
                        if Path::new(rel).is_absolute() {
                            tracing::warn!(
                                "credential_files: rejected absolute config path {:?}",
                                rel
                            );
                            continue;
                        }
                        let host_path = hermes_home.join(rel);
                        let resolved = match host_path.canonicalize() {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        if !resolved.starts_with(&hermes_home_resolved) {
                            tracing::warn!(
                                "credential_files: rejected config path traversal {:?} (resolves to {})",
                                rel,
                                resolved.display()
                            );
                            continue;
                        }
                        if resolved.is_file() {
                            result.push(MountEntry {
                                host_path: resolved.to_string_lossy().to_string(),
                                container_path: format!("{}/{}", CONTAINER_BASE.trim_end_matches('/'), rel),
                            });
                        }
                    }
                }
            }

            result
        })
        .clone()
}

/// Get mount info for cache directories (documents, images, audio, screenshots).
pub fn get_cache_directory_mounts(container_base: &str) -> Vec<MountEntry> {
    let mut mounts = Vec::new();

    for (new_subpath, _old_name) in CACHE_DIRS {
        let host_dir = get_hermes_home().join(new_subpath);
        if host_dir.is_dir() {
            let container_path = format!("{}/{}", container_base.trim_end_matches('/'), new_subpath);
            mounts.push(MountEntry {
                host_path: host_dir.to_string_lossy().to_string(),
                container_path,
            });
        }
    }

    mounts
}

/// Get individual files within cache directories.
///
/// Used by Modal to upload files individually. Skips symlinks.
pub fn iter_cache_files(container_base: &str) -> Vec<MountEntry> {
    let mut result = Vec::new();

    for (new_subpath, _old_name) in CACHE_DIRS {
        let host_dir = get_hermes_home().join(new_subpath);
        if !host_dir.is_dir() {
            continue;
        }
        let container_root = format!("{}/{}", container_base.trim_end_matches('/'), new_subpath);

        for entry in walkdir::WalkDir::new(&host_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_symlink() || !path.is_file() {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(&host_dir) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                result.push(MountEntry {
                    host_path: path.to_string_lossy().to_string(),
                    container_path: format!("{}/{}", container_root, rel_str),
                });
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reject_absolute_path() {
        assert!(!register_credential_file("/etc/passwd", CONTAINER_BASE));
    }

    #[test]
    fn test_reject_path_traversal() {
        assert!(!register_credential_file("../../.ssh/id_rsa", CONTAINER_BASE));
    }

    #[test]
    fn test_clear_resets_registry() {
        clear_credential_files();
        let mounts = get_credential_file_mounts();
        assert!(mounts.len() < 10);
    }

    #[test]
    fn test_cache_directory_mounts_empty() {
        let mounts = get_cache_directory_mounts(CONTAINER_BASE);
        assert!(mounts.iter().all(|m| m.container_path.starts_with("/root/.hermes/cache")));
    }
}
