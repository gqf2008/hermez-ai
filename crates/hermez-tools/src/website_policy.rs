#![allow(dead_code)]
//! Website access policy / blocklist.
//!
//! Lightweight URL policy checker for web/browser tools.
//! Mirrors the Python `tools/website_policy.py`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use hermez_core::hermez_home::get_hermez_home;
use once_cell::sync::Lazy;
use regex::Regex;

/// Cache duration in seconds.
const CACHE_TTL_SECS: u64 = 30;

/// A single blocklist rule.
#[derive(Debug, Clone)]
pub struct BlocklistRule {
    /// Normalized pattern (lowercase, no scheme, no www.).
    pub pattern: String,
    /// Source of the rule (e.g., "config", "blocklist.txt").
    pub source: String,
}

/// Cached policy with TTL.
struct CachedPolicy {
    policy: WebsitePolicy,
    loaded_at: Instant,
}

/// Website access policy.
#[derive(Debug, Clone)]
pub struct WebsitePolicy {
    /// Whether the blocklist is enabled.
    pub enabled: bool,
    /// Blocklist rules.
    pub rules: Vec<BlocklistRule>,
}

static POLICY_CACHE: Lazy<Mutex<Option<CachedPolicy>>> =
    Lazy::new(|| Mutex::new(None));

/// Regex for extracting host from a URL-ish string.
static HOST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:https?://)?([^/:?#]+)").unwrap()
});

impl WebsitePolicy {
    /// Load the website blocklist from config.
    ///
    /// Cached with a 30-second TTL. Thread-safe.
    pub fn load() -> Self {
        {
            let cache = POLICY_CACHE.lock().unwrap();
            if let Some(cached) = &*cache {
                if cached.loaded_at.elapsed().as_secs() < CACHE_TTL_SECS {
                    return cached.policy.clone();
                }
            }
        }

        let policy = Self::load_from_config();

        let mut cache = POLICY_CACHE.lock().unwrap();
        *cache = Some(CachedPolicy {
            policy: policy.clone(),
            loaded_at: Instant::now(),
        });

        policy
    }

    /// Force cache invalidation.
    pub fn invalidate_cache() {
        let mut cache = POLICY_CACHE.lock().unwrap();
        *cache = None;
    }

    /// Load the policy from config.yaml.
    ///
    /// Fail-open: returns a disabled policy if config can't be read.
    fn load_from_config() -> Self {
        let config_path = get_hermez_home().join("config.yaml");
        if !config_path.exists() {
            return Self::disabled();
        }

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => return Self::disabled(),
        };

        let config: serde_yaml::Value = match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Self::disabled(),
        };

        let security = config
            .get("security")
            .and_then(|v| v.get("website_blocklist"));

        let Some(security) = security else {
            return Self::disabled();
        };

        let enabled = security
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !enabled {
            return Self::disabled();
        }

        let mut rules = Vec::new();

        // Load domain rules from config
        if let Some(domains) = security.get("domains").and_then(|v| v.as_sequence()) {
            for domain in domains {
                if let Some(domain_str) = domain.as_str() {
                    let normalized = normalize_rule(domain_str);
                    rules.push(BlocklistRule {
                        pattern: normalized,
                        source: "config".to_string(),
                    });
                }
            }
        }

        // Load rules from shared blocklist files
        if let Some(files) = security.get("shared_files").and_then(|v| v.as_sequence()) {
            for file in files {
                if let Some(file_path) = file.as_str() {
                    let path = shellexpand::tilde(file_path).into_owned();
                    let path = std::path::Path::new(&path);
                    if path.exists() {
                        if let Ok(content) = std::fs::read_to_string(path) {
                            for line in content.lines() {
                                let line = line.trim();
                                if line.is_empty() || line.starts_with('#') {
                                    continue;
                                }
                                let normalized = normalize_rule(line);
                                rules.push(BlocklistRule {
                                    pattern: normalized,
                                    source: path.display().to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        Self { enabled, rules }
    }

    /// Create a disabled policy.
    fn disabled() -> Self {
        Self {
            enabled: false,
            rules: Vec::new(),
        }
    }

    /// Check if access to a URL is allowed.
    ///
    /// Returns `None` if allowed, or block metadata if blocked.
    /// Never panics — fail-open on unexpected errors.
    pub fn check_access(&self, url: &str) -> Option<HashMap<String, String>> {
        if !self.enabled {
            return None;
        }

        let host = extract_host(url);
        if host.is_empty() {
            return None; // Fail-open on malformed URL
        }

        let normalized_host = normalize_host(&host);

        for rule in &self.rules {
            if match_host(&normalized_host, &rule.pattern) {
                return Some(HashMap::from([
                    ("blocked".to_string(), "true".to_string()),
                    ("host".to_string(), host),
                    ("rule".to_string(), rule.pattern.clone()),
                    ("source".to_string(), rule.source.clone()),
                ]));
            }
        }

        None
    }
}

/// Normalize a rule: strip, lowercase, remove scheme/path, strip www.
fn normalize_rule(rule: &str) -> String {
    let rule = rule.trim().to_lowercase();
    let rule = rule
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let rule = rule.split('/').next().unwrap_or(rule);
    let rule = rule.trim_start_matches("www.");
    rule.to_string()
}

/// Normalize a host: strip, lowercase, remove trailing dot.
fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase().trim_end_matches('.').to_string()
}

/// Extract host from a URL-ish string.
fn extract_host(url: &str) -> String {
    HOST_RE
        .captures(url)
        .map(|cap| cap[1].to_string())
        .unwrap_or_else(|| url.to_string())
}

/// Match a host against a rule pattern.
///
/// Supports wildcard patterns (`*.example.com`) and suffix matching
/// (`example.com` matches `sub.example.com`).
fn match_host(host: &str, pattern: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // Wildcard: must be a subdomain
        host.ends_with(suffix) && host != suffix
    } else {
        // Exact or suffix match: `example.com` matches `example.com` and `sub.example.com`
        host == pattern || host.ends_with(&format!(".{pattern}"))
    }
}

/// Check if a URL is blocked by website policy.
///
/// Returns `None` if access is allowed, or a `HashMap` with block metadata
/// (`host`, `rule`, `source`, `message`) if blocked.
/// Mirrors Python `tools.website_policy.check_website_access`.
pub fn check_website_access(url: &str) -> Option<HashMap<String, String>> {
    let policy = WebsitePolicy::load();
    let blocked = policy.check_access(url)?;
    let mut result = blocked;
    let host = result.get("host").cloned().unwrap_or_default();
    let rule = result.get("rule").cloned().unwrap_or_default();
    result.insert(
        "message".to_string(),
        format!("Blocked by website policy: {host} matched rule '{rule}'"),
    );
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_rule() {
        assert_eq!(normalize_rule("  *.Evil.COM  "), "*.evil.com");
        assert_eq!(normalize_rule("https://www.bad.com/path"), "bad.com");
        assert_eq!(normalize_rule("http://example.com"), "example.com");
    }

    #[test]
    fn test_normalize_host() {
        assert_eq!(normalize_host("Example.COM."), "example.com");
        assert_eq!(normalize_host("  test.org  "), "test.org");
    }

    #[test]
    fn test_match_host_wildcard() {
        assert!(match_host("sub.evil.com", "*.evil.com"));
        assert!(match_host("deep.sub.evil.com", "*.evil.com"));
        assert!(!match_host("evil.com", "*.evil.com"));
        assert!(!match_host("good.com", "*.evil.com"));
    }

    #[test]
    fn test_match_host_suffix() {
        assert!(match_host("example.com", "example.com"));
        assert!(match_host("sub.example.com", "example.com"));
        assert!(!match_host("notexample.com", "example.com"));
        assert!(!match_host("example.com.evil", "example.com"));
    }

    #[test]
    fn test_disabled_policy_allows_all() {
        let policy = WebsitePolicy::disabled();
        assert!(policy.check_access("http://blocked.example.com").is_none());
    }
}
