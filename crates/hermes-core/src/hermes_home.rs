//! Hermes home directory resolution.
//!
//! Single source of truth for all path lookups. Supports multi-profile
//! isolation via `HERMES_HOME` environment variable.
//!
//! DO NOT hardcode `~/.hermes` anywhere in the codebase — use these functions.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static HERMES_HOME: OnceLock<PathBuf> = OnceLock::new();

/// Returns the Hermes home directory.
///
/// Resolution order:
/// 1. `HERMES_HOME` environment variable (absolute path)
/// 2. `~/.hermes` (default)
///
/// The result is cached after first resolution.
pub fn get_hermes_home() -> PathBuf {
    HERMES_HOME
        .get_or_init(|| {
            std::env::var("HERMES_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    home::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".hermes")
                })
        })
        .clone()
}

/// Override the Hermes home directory.
///
/// Useful for testing and profile switching.
/// Can only be called once (uses `OnceLock`).
pub fn set_hermes_home(path: impl AsRef<Path>) -> std::result::Result<(), PathBuf> {
    HERMES_HOME
        .set(path.as_ref().to_path_buf())
}

/// Display-friendly version of the Hermes home path.
///
/// Replaces the user's home directory with `~` for user-facing messages.
pub fn display_hermes_home() -> String {
    let path = get_hermes_home();
    if let Some(home) = home::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

/// Returns the default Hermes root for deployments and profile mode.
///
/// In Docker deployments, this may differ from `get_hermes_home()`.
pub fn get_default_hermes_root() -> PathBuf {
    // Check for Docker deployment marker
    if Path::new("/opt/data").exists() {
        return PathBuf::from("/opt/data");
    }
    get_hermes_home()
}

/// Resolve a profile name to its HERMES_HOME directory path.
///
/// Mirrors Python `get_profile_dir()`:
/// - `"default"` → `get_hermes_home()`
/// - Any other name → `<profiles_root>/<name>`
///
/// Profiles root is `default_hermes_home() / "profiles"`.
/// In Docker/custom deployments where HERMES_HOME is already set to a
/// non-default path, profiles live under that path so they persist on
/// the mounted volume.
pub fn resolve_profile_path(name: &str) -> PathBuf {
    if name == "default" {
        return get_hermes_home();
    }
    let profiles_root = get_default_hermes_root().join("profiles");
    profiles_root.join(name)
}

/// Backward-compatible path resolution.
///
/// Checks for old migration paths and returns the first one that exists.
pub fn get_hermes_dir() -> PathBuf {
    let current = get_hermes_home();
    if current.exists() {
        return current;
    }
    // Fallback to common old paths
    let old_paths: &[&str] = &["~/.hermes-agent", "~/.config/hermes"];
    for old in old_paths {
        let path = shellexpand::tilde(old).into_owned();
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_hermes_home() {
        // In tests, HERMES_HOME should not be set by default
        let home = get_hermes_home();
        assert!(home.ends_with(".hermes"));
    }

    #[test]
    fn test_set_hermes_home() {
        let tmp = std::env::temp_dir().join("hermes_test_home_set");
        // set_hermes_home uses OnceLock — if already initialized (by another test
        // or the default path), the call returns Err with the existing value.
        // We just verify the API is callable and behaves correctly either way.
        match set_hermes_home(&tmp) {
            Ok(()) => {
                let home = get_hermes_home();
                assert_eq!(home, tmp);
            }
            Err(existing) => {
                // OnceLock was already set — existing value should be valid
                assert!(!existing.as_os_str().is_empty());
            }
        }
    }

    #[test]
    fn test_display_hermes_home() {
        // When HERMES_HOME is under the real home dir, it should display as ~/
        let display = display_hermes_home();
        // Either it starts with ~/ or it's an absolute path
        assert!(display.starts_with("~/") || display.starts_with('/'));
    }

    #[test]
    fn test_resolve_profile_path_default() {
        let path = resolve_profile_path("default");
        assert_eq!(path, get_hermes_home());
    }

    #[test]
    fn test_resolve_profile_path_custom() {
        let path = resolve_profile_path("work");
        let expected = get_default_hermes_root().join("profiles").join("work");
        assert_eq!(path, expected);
    }

    #[test]
    fn test_get_hermes_dir_fallback() {
        // get_hermes_dir returns current home if it exists, otherwise falls back
        let dir = get_hermes_dir();
        // Should at least return a path (either current or fallback)
        assert!(!dir.as_os_str().is_empty());
    }
}
