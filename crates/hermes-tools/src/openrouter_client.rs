#![allow(dead_code)]
//! OpenRouter API client helper for Hermes tools.
//!
//! Provides a single lazy-initialized HTTP client that all tool modules can
//! share for OpenRouter API calls.
//!
//! Mirrors the Python `tools/openrouter_client.py`.

use std::sync::LazyLock;

use reqwest::Client;

/// Shared HTTP client for OpenRouter API calls.
///
/// Created lazily on first use and reused across all tool calls.
static OPENROUTER_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

/// Get the shared OpenRouter HTTP client.
pub fn get_openrouter_client() -> &'static Client {
    &OPENROUTER_CLIENT
}

/// Check whether the OpenRouter API key is present.
pub fn has_openrouter_key() -> bool {
    std::env::var("OPENROUTER_API_KEY").is_ok()
}

/// Get the OpenRouter API key, or an error message.
pub fn openrouter_api_key() -> Result<String, String> {
    std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY environment variable not set".to_string())
}

/// OpenRouter base URL.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_client_returns_same_instance() {
        let a = get_openrouter_client();
        let b = get_openrouter_client();
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn test_has_openrouter_key_reflects_env() {
        std::env::remove_var("OPENROUTER_API_KEY");
        assert!(!has_openrouter_key());
        std::env::set_var("OPENROUTER_API_KEY", "sk-test");
        assert!(has_openrouter_key());
        std::env::remove_var("OPENROUTER_API_KEY");
    }

    #[test]
    fn test_openrouter_api_key_error() {
        std::env::remove_var("OPENROUTER_API_KEY");
        assert!(openrouter_api_key().is_err());
    }

    #[test]
    fn test_openrouter_api_key_success() {
        std::env::set_var("OPENROUTER_API_KEY", "sk-test-key");
        assert_eq!(openrouter_api_key().unwrap(), "sk-test-key");
        std::env::remove_var("OPENROUTER_API_KEY");
    }
}
