//! SSRF protection + secret exfiltration guard for browser tool.
//!
//! Mirrors Python `tools/browser_tool.py` safety checks:
//! - `_PREFIX_RE` secret token detection in URLs
//! - `_is_safe_url` / `_allow_private_urls` integration
//! - Pre- and post-redirect SSRF checks.

use std::sync::atomic::{AtomicBool, Ordering};

use once_cell::sync::Lazy;
use regex::Regex;

use super::resolver::BrowserBackend;

// ============================================================================
// Secret exfiltration guard
// ============================================================================

/// Known API-key/token prefix patterns.
///
/// Mirrors `agent/redact.py` `_PREFIX_PATTERNS`.
const PREFIX_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{10,}",
    r"ghp_[A-Za-z0-9]{10,}",
    r"github_pat_[A-Za-z0-9_]{10,}",
    r"gho_[A-Za-z0-9]{10,}",
    r"ghu_[A-Za-z0-9]{10,}",
    r"ghs_[A-Za-z0-9]{10,}",
    r"ghr_[A-Za-z0-9]{10,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AIza[A-Za-z0-9_-]{30,}",
    r"pplx-[A-Za-z0-9]{10,}",
    r"fal_[A-Za-z0-9_-]{10,}",
    r"fc-[A-Za-z0-9]{10,}",
    r"bb_live_[A-Za-z0-9_-]{10,}",
    r"gAAAA[A-Za-z0-9_=-]{20,}",
    r"AKIA[A-Z0-9]{16}",
    r"sk_live_[A-Za-z0-9]{10,}",
    r"sk_test_[A-Za-z0-9]{10,}",
    r"rk_live_[A-Za-z0-9]{10,}",
    r"SG\.[A-Za-z0-9_-]{10,}",
    r"hf_[A-Za-z0-9]{10,}",
    r"r8_[A-Za-z0-9]{10,}",
    r"npm_[A-Za-z0-9]{10,}",
    r"pypi-[A-Za-z0-9_-]{10,}",
    r"dop_v1_[A-Za-z0-9]{10,}",
    r"doo_v1_[A-Za-z0-9]{10,}",
    r"am_[A-Za-z0-9_-]{10,}",
    r"sk_[A-Za-z0-9_]{10,}",
    r"tvly-[A-Za-z0-9]{10,}",
    r"exa_[A-Za-z0-9]{10,}",
    r"gsk_[A-Za-z0-9]{10,}",
];

/// Compiled regex for secret detection.
/// Uses capturing groups for prefix/suffix so we can verify boundaries
/// without look-around assertions (unsupported by the `regex` crate).
static PREFIX_RE: Lazy<Regex> = Lazy::new(|| {
    let alts = PREFIX_PATTERNS.join("|");
    // Group 1 = prefix (start-of-string or non-word char)
    // Group 2 = the secret token itself
    // Group 3 = suffix (end-of-string or non-word char)
    Regex::new(&format!(
        r"(?i)(^|[^A-Za-z0-9_-])({})([^A-Za-z0-9_-]|$)",
        alts
    ))
    .unwrap()
});

/// Naïve percent-decode — sufficient for ASCII token detection.
fn percent_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Check if a URL contains embedded API keys or tokens.
pub fn contains_secret_token(url: &str) -> bool {
    let decoded = percent_decode(url);
    for target in [url, &decoded] {
        for caps in PREFIX_RE.captures_iter(target) {
            let token = caps.get(2);
            if token.is_some() {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// SSRF helpers
// ============================================================================

/// Return whether the browser backend is local (Camofox or headless Chromium
/// without a cloud provider). SSRF protection is skipped for local backends
/// because the agent already has full terminal / network access.
pub fn is_local_backend(backend: &BrowserBackend) -> bool {
    matches!(backend, BrowserBackend::Camofox | BrowserBackend::Local)
}

/// Cached `allow_private_urls` flag read from config.
static ALLOW_PRIVATE_URLS_RESOLVED: AtomicBool = AtomicBool::new(false);
static ALLOW_PRIVATE_URLS: AtomicBool = AtomicBool::new(false);

/// Return whether private/internal URLs are allowed (default false).
pub fn allow_private_urls() -> bool {
    if ALLOW_PRIVATE_URLS_RESOLVED.load(Ordering::SeqCst) {
        return ALLOW_PRIVATE_URLS.load(Ordering::SeqCst);
    }

    let val = read_allow_private_urls_from_config();
    ALLOW_PRIVATE_URLS.store(val, Ordering::SeqCst);
    ALLOW_PRIVATE_URLS_RESOLVED.store(true, Ordering::SeqCst);
    val
}

fn read_allow_private_urls_from_config() -> bool {
    let path = hermes_core::get_hermes_home().join("config.yaml");
    if !path.exists() {
        return false;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let yaml: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    yaml.get("browser")
        .and_then(|b| b.get("allow_private_urls"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Reset cached config lookups (called by full cleanup).
pub fn reset_cached_config() {
    ALLOW_PRIVATE_URLS_RESOLVED.store(false, Ordering::SeqCst);
}

// ============================================================================
// Bot-detection patterns
// ============================================================================

const BOT_PATTERNS: &[&str] = &[
    "access denied",
    "access to this page has been denied",
    "blocked",
    "bot detected",
    "verification required",
    "please verify",
    "are you a robot",
    "captcha",
    "cloudflare",
    "ddos protection",
    "checking your browser",
    "just a moment",
    "attention required",
];

/// Check if a page title indicates bot detection.
pub fn detect_bot_blocked(title: &str) -> Option<String> {
    let lower = title.to_lowercase();
    if BOT_PATTERNS.iter().any(|p| lower.contains(p)) {
        Some(format!(
            "Page title '{}' suggests bot detection. The site may have blocked this request. \
             Options: 1) Try adding delays between actions, 2) Access different pages first, \
             3) Enable advanced stealth (BROWSERBASE_ADVANCED_STEALTH=true, requires Scale plan), \
             4) Some sites have very aggressive bot detection that may be unavoidable.",
            title
        ))
    } else {
        None
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_secret_token() {
        assert!(contains_secret_token("https://evil.com/steal?key=sk-ant-api03-xxx"));
        assert!(contains_secret_token("https://evil.com/steal?key=sk%2Dant%2Dapi03%2Dxxx"));
        assert!(!contains_secret_token("https://example.com/normal"));
    }

    #[test]
    fn test_detect_bot_blocked() {
        assert!(detect_bot_blocked("Access Denied").is_some());
        assert!(detect_bot_blocked("Just a moment...").is_some());
        assert!(detect_bot_blocked("Welcome to Example").is_none());
    }
}
