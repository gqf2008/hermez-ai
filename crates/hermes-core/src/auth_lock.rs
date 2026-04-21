#![allow(dead_code)]
//! Cross-process advisory lock for `auth.json`.
//!
//! Prevents concurrent writes from multiple Hermes processes (or threads)
//! corrupting the shared credential store.

use std::fs::File;
use std::path::PathBuf;

use crate::hermes_home::get_hermes_home;

/// Return the path to the auth lock file (`~/.hermes/auth.lock`).
fn auth_lock_path() -> PathBuf {
    get_hermes_home().join("auth.lock")
}

/// Open (or create) the lock file.
fn open_lock_file() -> std::io::Result<File> {
    let path = auth_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
}

/// Execute `f` while holding a **shared** advisory lock on `auth.json`.
///
/// Multiple readers can hold the lock concurrently.  Writers (see
/// `with_auth_json_write_lock`) block until all readers release.
///
/// The lock is released when the temporary `File` is dropped.
///
/// Returns `Err` if the lock file cannot be opened or locked (e.g. read-only
/// filesystem or permission denied).
pub fn with_auth_json_read_lock<T>(
    f: impl FnOnce() -> T,
) -> std::result::Result<T, std::io::Error> {
    let lock_file = open_lock_file()?;
    fs4::fs_std::FileExt::lock_shared(&lock_file)?;
    let result = f();
    drop(lock_file); // unlock on drop
    Ok(result)
}

/// Execute `f` while holding an **exclusive** advisory lock on `auth.json`.
///
/// Only one writer (or reader-writer pair) can hold the lock at a time.
///
/// The lock is released when the temporary `File` is dropped.
///
/// Returns `Err` if the lock file cannot be opened or locked (e.g. read-only
/// filesystem or permission denied).
pub fn with_auth_json_write_lock<T>(
    f: impl FnOnce() -> T,
) -> std::result::Result<T, std::io::Error> {
    let lock_file = open_lock_file()?;
    fs4::fs_std::FileExt::lock_exclusive(&lock_file)?;
    let result = f();
    drop(lock_file); // unlock on drop
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_lock_path() {
        let path = auth_lock_path();
        assert!(path.file_name().unwrap() == "auth.lock");
    }

    #[test]
    fn test_with_auth_json_read_lock() {
        let result = with_auth_json_read_lock(|| 42).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_with_auth_json_write_lock() {
        let result = with_auth_json_write_lock(|| "locked").unwrap();
        assert_eq!(result, "locked");
    }

    #[test]
    fn test_read_write_lock_compatibility() {
        // Multiple read locks should be compatible
        let r1 = with_auth_json_read_lock(|| 1).unwrap();
        let r2 = with_auth_json_read_lock(|| 2).unwrap();
        assert_eq!(r1 + r2, 3);

        // Write lock after read locks should work ( sequential in same thread)
        let w1 = with_auth_json_write_lock(|| 10).unwrap();
        assert_eq!(w1, 10);
    }
}
