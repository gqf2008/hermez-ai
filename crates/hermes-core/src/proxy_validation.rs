#![allow(dead_code)]
//! Proxy and base URL validation.
//!
//! Fail-fast detection of malformed proxy environment variables and custom
//! base URLs before they reach the HTTP client. Mirrors Python
//! `_validate_proxy_env_urls()` and `_validate_base_url()` (commit f4724803).
//!
//! Without this, reqwest returns cryptic "Invalid port" or "relative URL
//! without a base" errors instead of a clear diagnostic.

/// Proxy-related environment variables that reqwest respects.
const PROXY_ENV_VARS: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"];

/// Validate proxy environment variable URLs.
///
/// Returns `Ok(())` if no proxy vars are set, or if all values are valid URLs.
/// Returns a descriptive `Err` if any proxy var contains a malformed URL.
pub fn validate_proxy_env_urls() -> Result<(), String> {
    for &var in PROXY_ENV_VARS {
        if let Ok(value) = std::env::var(var) {
            if !value.is_empty() && !looks_like_valid_url(&value) {
                return Err(format!(
                    "Malformed proxy URL in {var}: {value}\n\
                     (this looks like a broken shell config — check for missing quotes or typos)"
                ));
            }
        }
    }
    Ok(())
}

/// Validate a custom base URL before passing it to the HTTP client.
pub fn validate_base_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Ok(());
    }
    if !looks_like_valid_url(url) {
        return Err(format!(
            "Malformed base URL: {url}\n\
             (expected something like https://api.example.com/v1)"
        ));
    }
    Ok(())
}

/// Quick heuristic check — does this look like a valid HTTP(S) URL?
/// Catches common shell config mistakes like `http://127.0.0.1:6153export`
/// (broken env var interpolation).
fn looks_like_valid_url(value: &str) -> bool {
    // Must start with a known scheme
    if !(value.starts_with("http://") || value.starts_with("https://") || value.starts_with("socks://") || value.starts_with("socks5://") || value.starts_with("socks5h://")) {
        return false;
    }
    // After the scheme, there should be a host part
    let after_scheme = value.split_once("://").map(|x| x.1).unwrap_or("");
    if after_scheme.is_empty() {
        return false;
    }
    // The host shouldn't contain obvious shell artifacts
    if after_scheme.contains(' ') || after_scheme.contains('"') || after_scheme.contains('\'') || after_scheme.contains('$') {
        return false;
    }
    // If there's a port (host:port), the port part should be purely numeric
    // until the first `/` or `?`. Catches `http://127.0.0.1:6153export`.
    let host_part = after_scheme.split('/').next().unwrap_or(after_scheme);
    if let Some(colon_idx) = host_part.rfind(':') {
        let port_and_auth = &host_part[colon_idx + 1..];
        // Strip userinfo if present (user:pass@host)
        let port_str = if port_and_auth.contains('@') {
            // This is actually userinfo, not port — check the real port after @
            return true; // Complex URLs, let reqwest handle
        } else {
            port_and_auth
        };
        // Port must be all digits (empty port like `http://host:` is invalid)
        if !port_str.is_empty() && !port_str.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_valid_url() {
        assert!(looks_like_valid_url("http://127.0.0.1:8080"));
        assert!(looks_like_valid_url("https://proxy.example.com:3128"));
        assert!(looks_like_valid_url("socks5://localhost:1080"));
        assert!(!looks_like_valid_url("127.0.0.1:8080"));
        assert!(!looks_like_valid_url("http://127.0.0.1:6153export")); // malformed
        assert!(!looks_like_valid_url(""));
        assert!(!looks_like_valid_url("http://"));
        assert!(!looks_like_valid_url("http:// has space"));
    }

    #[test]
    fn test_validate_base_url() {
        assert!(validate_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_base_url("").is_ok());
        assert!(validate_base_url("not-a-url").is_err());
        assert!(validate_base_url("http:// has space").is_err());
    }

    #[test]
    fn test_validate_proxy_env_clean() {
        // No proxy vars set → clean
        assert!(validate_proxy_env_urls().is_ok());
    }
}
