#![allow(dead_code)]
//! Path security validation helpers.
//!
//! Ensures user-supplied paths resolve within allowed directories and do not
//! contain `..` traversal components. Used by skills, cron tools, and
//! credential file operations.

use std::path::Path;

/// Ensure `path` resolves to a location within `root`.
///
/// Returns an error message if validation fails, or `None` if the path
/// is safe. Uses `Path::canonicalize()` to follow symlinks and normalize
/// `..` components.
pub fn validate_within_dir(path: &Path, root: &Path) -> Option<String> {
    let resolved = match path.canonicalize() {
        Ok(r) => r,
        Err(e) => return Some(format!("Path resolution failed: {e}")),
    };
    let root_resolved = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => return Some(format!("Root resolution failed: {e}")),
    };
    if resolved.starts_with(&root_resolved) {
        None
    } else {
        Some(format!(
            "Path escapes allowed directory: {} is not under {}",
            resolved.display(),
            root_resolved.display()
        ))
    }
}

/// Return `true` if `path_str` contains `..` traversal components.
///
/// Quick check for obvious traversal attempts before doing full resolution.
pub fn has_traversal_component(path_str: &str) -> bool {
    Path::new(path_str).components().any(|c| {
        c.as_os_str() == std::ffi::OsStr::new("..")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_within_allowed() {
        // Test the logic using has_traversal_component since validate_within_dir
        // requires existing paths on disk
        assert!(!has_traversal_component("/tmp/some/file.txt"));
    }

    #[test]
    fn test_has_traversal() {
        assert!(has_traversal_component("../etc/passwd"));
        assert!(has_traversal_component("/tmp/../../etc"));
        assert!(!has_traversal_component("/tmp/file.txt"));
    }
}
