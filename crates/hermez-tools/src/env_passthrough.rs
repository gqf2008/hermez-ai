#![allow(dead_code)]
//! Environment variable passthrough registry.
//!
//! Skills that declare `required_environment_variables` in their frontmatter
//! need those vars available in sandboxed execution environments. This module
//! provides a session-scoped allowlist so skill-declared vars (and
//! user-configured overrides) pass through.
//!
//! Both `code_execution_tool` and terminal environments consult
//! [`is_env_passthrough`] before stripping a variable.

use parking_lot::Mutex;
use std::collections::HashSet;

/// Thread-safe passthrough registry.
#[derive(Default)]
pub struct EnvPassthrough {
    allowed: Mutex<HashSet<String>>,
}

impl EnvPassthrough {
    /// Register environment variable names as allowed in sandboxed environments.
    pub fn register(&self, var_names: &[&str]) {
        let mut set = self.allowed.lock();
        for name in var_names {
            let name = name.trim();
            if !name.is_empty() {
                set.insert(name.to_string());
            }
        }
    }

    /// Check whether `var_name` is allowed to pass through to sandboxes.
    pub fn is_passthrough(&self, var_name: &str) -> bool {
        let set = self.allowed.lock();
        set.contains(var_name)
    }

    /// Return the set of all passthrough var names.
    pub fn get_all(&self) -> HashSet<String> {
        self.allowed.lock().clone()
    }

    /// Reset the allowlist (e.g. on session reset).
    pub fn clear(&self) {
        self.allowed.lock().clear();
    }
}

/// Global singleton instance.
pub static ENV_PASSTHROUGH: std::sync::LazyLock<EnvPassthrough> =
    std::sync::LazyLock::new(EnvPassthrough::default);

/// Convenience wrappers that delegate to the global instance.
/// Register environment variable names as allowed.
pub fn register_env_passthrough(var_names: &[&str]) {
    ENV_PASSTHROUGH.register(var_names);
}

/// Check whether `var_name` is allowed to pass through.
pub fn is_env_passthrough(var_name: &str) -> bool {
    ENV_PASSTHROUGH.is_passthrough(var_name)
}

/// Return the set of all passthrough var names.
pub fn get_all_passthrough() -> HashSet<String> {
    ENV_PASSTHROUGH.get_all()
}

/// Reset the allowlist.
pub fn clear_env_passthrough() {
    ENV_PASSTHROUGH.clear()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_check() {
        let pt = EnvPassthrough::default();
        pt.register(&["API_KEY", "DB_URL"]);
        assert!(pt.is_passthrough("API_KEY"));
        assert!(pt.is_passthrough("DB_URL"));
        assert!(!pt.is_passthrough("SECRET"));
    }

    #[test]
    fn test_clear() {
        let pt = EnvPassthrough::default();
        pt.register(&["API_KEY"]);
        assert!(pt.is_passthrough("API_KEY"));
        pt.clear();
        assert!(!pt.is_passthrough("API_KEY"));
    }

    #[test]
    fn test_get_all() {
        let pt = EnvPassthrough::default();
        pt.register(&["A", "B"]);
        let all = pt.get_all();
        assert!(all.contains("A"));
        assert!(all.contains("B"));
    }
}
