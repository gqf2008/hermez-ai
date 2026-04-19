#![allow(dead_code)]
//! Helper utilities shared across tool backend implementations.
//!
//! Provides common patterns like managed Nous tools detection and OpenAI
//! key resolution.

/// Check if managed Nous tools are enabled.
///
/// Reads the `MANAGED_NOUS_TOOLS` environment variable.
pub fn managed_nous_tools_enabled() -> bool {
    std::env::var("MANAGED_NOUS_TOOLS")
        .ok()
        .map(|v| v.trim().eq_ignore_ascii_case("true") || v.trim() == "1")
        .unwrap_or(false)
}

/// Resolve the OpenAI audio API key from config or environment.
///
/// Priority:
/// 1. `VOICE_TOOLS_OPENAI_KEY` env var (STT-specific key)
/// 2. `OPENAI_API_KEY` env var (general key)
///
/// Returns `(api_key, base_url)`.
pub fn resolve_openai_audio_api_key() -> Option<(String, String)> {
    // STT-specific key
    if let Ok(key) = std::env::var("VOICE_TOOLS_OPENAI_KEY") {
        let trimmed = key.trim().to_string();
        if !trimmed.is_empty() {
            return Some((trimmed, "https://api.openai.com/v1".to_string()));
        }
    }
    // General OpenAI key
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let trimmed = key.trim().to_string();
        if !trimmed.is_empty() {
            return Some((trimmed, "https://api.openai.com/v1".to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_managed_disabled_by_default() {
        assert!(!managed_nous_tools_enabled());
    }
}
