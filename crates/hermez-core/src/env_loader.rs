#![allow(dead_code)]
//! .env file loader.
//!
//! Loads `.env` files with the same precedence logic as the Python version:
//! - `~/.hermez/.env` overrides stale shell-exported values
//! - project `.env` acts as a dev fallback (only fills missing values)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::hermez_home::get_hermez_home;

/// Parse a .env file and return key-value pairs.
fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Parse KEY=VALUE
            if let Some(eq_idx) = line.find('=') {
                let key = line[..eq_idx].trim().to_string();
                let mut value = line[eq_idx + 1..].trim().to_string();
                // Strip surrounding quotes
                if (value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\''))
                {
                    value = value[1..value.len() - 1].to_string();
                }
                if !key.is_empty() {
                    result.insert(key, value);
                }
            }
        }
    }
    result
}

/// Env var name suffixes that indicate credential values.
/// These are sanitized on load — we must not silently alter arbitrary user
/// env vars, but credentials are known to require pure ASCII (they become
/// HTTP header values).
const CREDENTIAL_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET", "_KEY"];

/// Strip non-ASCII characters from credential env vars.
///
/// Only sanitizes vars that were just loaded from a `.env` file, not the
/// entire process environment. This avoids O(n) overhead and prevents
/// mutating unrelated env vars set by the parent process.
///
/// Mirrors Python `_sanitize_loaded_credentials` in `hermez_cli/env_loader.py:18`.
pub fn sanitize_loaded_credentials(loaded_vars: &HashMap<String, String>) {
    for (key, value) in loaded_vars {
        if !CREDENTIAL_SUFFIXES.iter().any(|suffix| key.ends_with(suffix)) {
            continue;
        }
        if value.is_ascii() {
            continue;
        }
        // Strip non-ASCII chars (e.g. Unicode lookalikes from PDF copy-paste)
        let cleaned: String = value.chars().filter(|c| c.is_ascii()).collect();
        if cleaned != *value {
            tracing::warn!(
                "Sanitized non-ASCII chars from {} ({} -> {} chars)",
                key,
                value.len(),
                cleaned.len()
            );
            std::env::set_var(key, &cleaned);
        }
    }
}

/// Load the Hermez .env files.
///
/// Returns the list of loaded file paths.
///
/// Behavior:
/// - `HERMEZ_HOME/.env` overrides existing env vars
/// - project `.env` only fills missing vars (when user .env exists)
///   or overrides all (when no user .env exists)
pub fn load_hermez_dotenv(project_env: Option<&Path>) -> Vec<PathBuf> {
    let mut loaded = Vec::new();

    let hermez_home = get_hermez_home();
    let user_env = hermez_home.join(".env");

    let has_user_env = user_env.exists();

    let mut all_loaded_vars = HashMap::new();

    if has_user_env {
        let vars = parse_env_file(&user_env);
        for (key, value) in &vars {
            std::env::set_var(key, value);
        }
        all_loaded_vars.extend(vars);
        loaded.push(user_env);
    }

    if let Some(project_path) = project_env {
        if project_path.exists() {
            let vars = parse_env_file(project_path);
            for (key, value) in &vars {
                // Only set if not already set (user env takes precedence)
                if !has_user_env || std::env::var(key).is_err() {
                    std::env::set_var(key, value);
                    all_loaded_vars.insert(key.clone(), value.clone());
                }
            }
            loaded.push(project_path.to_path_buf());
        }
    }

    // Strip non-ASCII characters from credential env vars that were just
    // loaded. API keys must be pure ASCII since they're sent as HTTP headers.
    sanitize_loaded_credentials(&all_loaded_vars);

    loaded
}

/// Load a single .env file and set all variables (override mode).
pub fn load_dotenv_override(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let vars = parse_env_file(path);
    for (key, value) in &vars {
        std::env::set_var(key, value);
    }
    sanitize_loaded_credentials(&vars);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_env_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_env_loader.env");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "# comment").unwrap();
        writeln!(file, "KEY1=value1").unwrap();
        writeln!(file, "KEY2=\"quoted value\"").unwrap();
        writeln!(file, "KEY3='single quoted'").unwrap();
        drop(file);

        let vars = parse_env_file(&path);
        assert_eq!(vars.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(vars.get("KEY2"), Some(&"quoted value".to_string()));
        assert_eq!(vars.get("KEY3"), Some(&"single quoted".to_string()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_parse_skips_comments() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_env_comments.env");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "# this is a comment").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, "VALID=yes").unwrap();
        drop(file);

        let vars = parse_env_file(&path);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("VALID"), Some(&"yes".to_string()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sanitize_credentials_strips_non_ascii() {
        let mut vars = HashMap::new();
        vars.insert("SANITIZE_TEST_API_KEY".to_string(), "key_with_ʋ_lookalike".to_string());
        sanitize_loaded_credentials(&vars);
        let cleaned = std::env::var("SANITIZE_TEST_API_KEY").unwrap();
        assert_eq!(cleaned, "key_with__lookalike");
        std::env::remove_var("SANITIZE_TEST_API_KEY");
    }

    #[test]
    fn test_sanitize_credentials_leaves_ascii_intact() {
        let mut vars = HashMap::new();
        vars.insert("SANITIZE_TEST_TOKEN".to_string(), "sk-abc123_xyz".to_string());
        // The var must already be in the process env (as if load_hermez_dotenv set it)
        std::env::set_var("SANITIZE_TEST_TOKEN", "sk-abc123_xyz");
        sanitize_loaded_credentials(&vars);
        let value = std::env::var("SANITIZE_TEST_TOKEN").unwrap();
        assert_eq!(value, "sk-abc123_xyz");
        std::env::remove_var("SANITIZE_TEST_TOKEN");
    }

    #[test]
    fn test_sanitize_credentials_skips_non_credentials() {
        let mut vars = HashMap::new();
        vars.insert("SANITIZE_USER_NAME".to_string(), "José".to_string());
        sanitize_loaded_credentials(&vars);
        // Since the var is not in the process env, we verify it was NOT set
        // (non-credential vars are not touched even when passed in)
        assert!(std::env::var("SANITIZE_USER_NAME").is_err());
    }
}
