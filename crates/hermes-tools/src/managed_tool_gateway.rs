#![allow(dead_code)]
//! Managed tool gateway helpers for Nous-hosted vendor passthroughs.
//!
//! Resolves shared managed-tool gateway config for vendors, reading the
//! Nous access token from auth state or environment overrides.

use hermes_core::hermes_home::get_hermes_home;
use std::path::PathBuf;

/// Default gateway domain.
const DEFAULT_TOOL_GATEWAY_DOMAIN: &str = "nousresearch.com";
/// Default gateway scheme.
const DEFAULT_TOOL_GATEWAY_SCHEME: &str = "https";

/// Resolved managed tool gateway configuration.
#[derive(Debug, Clone)]
pub struct ManagedToolGatewayConfig {
    pub vendor: String,
    pub gateway_origin: String,
    pub nous_user_token: String,
    pub managed_mode: bool,
}

/// Return the Hermes auth store path, respecting HERMES_HOME overrides.
fn auth_json_path() -> PathBuf {
    get_hermes_home().join("auth.json")
}

/// Read the Nous provider state from auth.json.
fn read_nous_provider_state() -> Option<serde_json::Value> {
    let path = auth_json_path();
    if !path.is_file() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    json.get("providers")?.get("nous").cloned()
}

/// Read a Nous Subscriber OAuth access token from auth store or env override.
pub fn read_nous_access_token() -> Option<String> {
    // Check explicit env override first
    if let Ok(token) = std::env::var("TOOL_GATEWAY_USER_TOKEN") {
        let trimmed = token.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    // Try auth.json
    if let Some(nous) = read_nous_provider_state() {
        if let Some(token) = nous.get("access_token").and_then(|v| v.as_str()) {
            let trimmed = token.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }

    None
}

/// Return the gateway origin for a specific vendor.
pub fn build_vendor_gateway_url(vendor: &str) -> String {
    // Check for vendor-specific override
    let vendor_key = format!(
        "{}_GATEWAY_URL",
        vendor.to_uppercase().replace('-', "_")
    );
    if let Ok(url) = std::env::var(&vendor_key) {
        let trimmed = url.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    // Use shared scheme and domain
    let scheme = std::env::var("TOOL_GATEWAY_SCHEME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| DEFAULT_TOOL_GATEWAY_SCHEME.to_string());

    let domain = std::env::var("TOOL_GATEWAY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(|d| d.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_TOOL_GATEWAY_DOMAIN.to_string());

    format!("{scheme}://{vendor}-gateway.{domain}")
}

/// Check if managed Nous tools are enabled.
pub fn managed_nous_tools_enabled() -> bool {
    std::env::var("MANAGED_NOUS_TOOLS")
        .ok()
        .map(|v| v.trim().eq_ignore_ascii_case("true") || v.trim() == "1")
        .unwrap_or(false)
}

/// Resolve shared managed-tool gateway config for a vendor.
pub fn resolve_managed_tool_gateway(vendor: &str) -> Option<ManagedToolGatewayConfig> {
    if !managed_nous_tools_enabled() {
        return None;
    }

    let gateway_origin = build_vendor_gateway_url(vendor);
    let nous_user_token = read_nous_access_token()?;

    if gateway_origin.is_empty() || nous_user_token.is_empty() {
        return None;
    }

    Some(ManagedToolGatewayConfig {
        vendor: vendor.to_string(),
        gateway_origin,
        nous_user_token,
        managed_mode: true,
    })
}

/// Return `true` when gateway URL and Nous access token are available.
pub fn is_managed_tool_gateway_ready(vendor: &str) -> bool {
    resolve_managed_tool_gateway(vendor).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_vendor_gateway_url_default() {
        // With no env overrides, should use defaults
        let url = build_vendor_gateway_url("openai-audio");
        assert!(url.contains("openai-audio-gateway."));
        assert!(url.starts_with("https://"));
    }

    #[test]
    fn test_managed_disabled_by_default() {
        assert!(!managed_nous_tools_enabled());
    }

    #[test]
    fn test_resolve_returns_none_when_disabled() {
        assert!(resolve_managed_tool_gateway("test").is_none());
    }
}
