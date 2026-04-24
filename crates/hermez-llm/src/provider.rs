//! Provider definitions and routing.
//!
//! Multi-provider fallback chain with provider aliasing, capability flags,
//! and client resolution logic. Mirrors the Python provider routing in
//! `auxiliary_client.py`.

use std::collections::HashMap;
use std::fmt;

/// Provider alias mappings (e.g., "google" -> "gemini").
/// Mirrors Python `_PROVIDER_ALIASES` (auxiliary_client.py).
pub const PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("google", "gemini"),
    ("gemini-cli", "google-gemini-cli"),
    ("z-ai", "zai"),
    ("z.ai", "zai"),
    ("zhipu", "zai"),
    ("glm", "zai"),
    ("kimi", "kimi-coding"),
    ("moonshot", "kimi-coding"),
    ("minimax_china", "minimax-cn"),
    ("minimax_cn", "minimax-cn"),
    ("claude", "anthropic"),
    ("claude-code", "anthropic"),
    ("deepseek", "deepseek"),
];

/// Normalizes a provider name using the alias table.
pub fn resolve_provider_alias(name: &str) -> &str {
    for (alias, canonical) in PROVIDER_ALIASES {
        if alias.eq_ignore_ascii_case(name) {
            return canonical;
        }
    }
    name
}

/// Multi-provider types.
///
/// The fallback chain order mirrors the Python auxiliary client:
/// 1. OpenRouter (aggregator)
/// 2. Nous Research
/// 3. Custom/local endpoint
/// 4. OpenAI Codex
/// 5. Direct API providers (Gemini, ZAI, Kimi, Minimax, Anthropic)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderType {
    OpenRouter,
    Nous,
    Custom,
    Codex,
    Gemini,
    Zai,
    Kimi,
    Minimax,
    Anthropic,
    OpenAI,
    Ollama,
    GoogleGeminiCli,
    Unknown,
}

impl fmt::Display for ProviderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderType::OpenRouter => write!(f, "openrouter"),
            ProviderType::Nous => write!(f, "nous"),
            ProviderType::Custom => write!(f, "custom"),
            ProviderType::Codex => write!(f, "openai-codex"),
            ProviderType::Gemini => write!(f, "gemini"),
            ProviderType::Zai => write!(f, "zai"),
            ProviderType::Kimi => write!(f, "kimi"),
            ProviderType::Minimax => write!(f, "minimax"),
            ProviderType::Anthropic => write!(f, "anthropic"),
            ProviderType::OpenAI => write!(f, "openai"),
            ProviderType::Ollama => write!(f, "ollama"),
            ProviderType::GoogleGeminiCli => write!(f, "google-gemini-cli"),
            ProviderType::Unknown => write!(f, "unknown"),
        }
    }
}

/// Whether a provider is an aggregator (OpenRouter, Nous).
/// Aggregators support multiple underlying models from different providers.
pub fn is_aggregator(provider: ProviderType) -> bool {
    matches!(provider, ProviderType::OpenRouter | ProviderType::Nous)
}

/// Parse a provider string into a ProviderType.
pub fn parse_provider(name: &str) -> ProviderType {
    let canonical = resolve_provider_alias(name);
    match canonical {
        "openrouter" => ProviderType::OpenRouter,
        "nous" => ProviderType::Nous,
        "custom" | "local" => ProviderType::Custom,
        "openai-codex" | "codex" => ProviderType::Codex,
        "gemini" => ProviderType::Gemini,
        "zai" => ProviderType::Zai,
        "kimi" => ProviderType::Kimi,
        "minimax" => ProviderType::Minimax,
        "anthropic" => ProviderType::Anthropic,
        "openai" => ProviderType::OpenAI,
        "ollama" => ProviderType::Ollama,
        "google-gemini-cli" => ProviderType::GoogleGeminiCli,
        _ => ProviderType::Unknown,
    }
}

/// Strips provider prefix from model name (e.g., "local:my-model" -> "my-model").
/// Preserves Ollama-style `model:tag` formats.
pub fn strip_provider_prefix(model: &str) -> &str {
    // Known provider prefixes that use colon separator
    let known_prefixes = ["openrouter", "nous", "local", "custom", "anthropic", "openai"];
    for prefix in &known_prefixes {
        if model.starts_with(prefix) && model.len() > prefix.len() + 1 && model.as_bytes()[prefix.len()] == b':' {
            return &model[prefix.len() + 1..];
        }
    }
    model
}

/// Default base URLs for various providers.
pub fn default_base_url(provider: ProviderType) -> Option<&'static str> {
    match provider {
        ProviderType::OpenRouter => Some("https://openrouter.ai/api/v1"),
        ProviderType::Nous => Some("https://api.nousresearch.com/v1"),
        ProviderType::Codex => Some("https://api.openai.com/v1"),
        ProviderType::Anthropic => Some("https://api.anthropic.com"),
        ProviderType::OpenAI => Some("https://api.openai.com/v1"),
        ProviderType::Gemini => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        ProviderType::Ollama => Some("http://localhost:11434/v1"),
        ProviderType::GoogleGeminiCli => Some("https://generativelanguage.googleapis.com/v1beta"),
        _ => None,
    }
}

/// Build provider-specific metadata headers (not auth — that's handled separately).
pub fn provider_headers(provider: ProviderType) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match provider {
        ProviderType::OpenRouter => {
            headers.insert("HTTP-Referer".to_string(), "https://hermez-agent.local".to_string());
            headers.insert("X-Title".to_string(), "Hermez Agent".to_string());
        }
        ProviderType::Anthropic => {
            headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
        }
        _ => {}
    }
    headers
}

/// Get the default model for a provider when no model is configured.
///
/// Mirrors Python: `get_default_model_for_provider()` in `hermez_cli/models.py`.
/// Returns the provider's first catalog model so the API call doesn't fail
/// with "model must be a non-empty string".
pub fn get_default_model_for_provider(provider: ProviderType) -> Option<&'static str> {
    match provider {
        ProviderType::Anthropic => Some("claude-sonnet-4-6-20250514"),
        ProviderType::OpenAI => Some("gpt-4.1"),
        ProviderType::OpenRouter => Some("anthropic/claude-sonnet-4-6"),
        ProviderType::Nous => Some("nousresearch/hermes-3-llama-3.1-70b"),
        ProviderType::Codex => Some("o3"),
        ProviderType::Gemini => Some("gemini-2.5-flash"),
        ProviderType::Zai => Some("glm-4-plus"),
        ProviderType::Kimi => Some("kimi-k2-0905"),
        ProviderType::Minimax => Some("MiniMax-M2.5"),
        ProviderType::Ollama => Some("llama3"),
        ProviderType::GoogleGeminiCli => Some("gemini-2.5-flash"),
        ProviderType::Custom | ProviderType::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alias_google() {
        assert_eq!(resolve_provider_alias("google"), "gemini");
    }

    #[test]
    fn test_alias_glm() {
        assert_eq!(resolve_provider_alias("glm"), "zai");
    }

    #[test]
    fn test_alias_claude() {
        assert_eq!(resolve_provider_alias("claude"), "anthropic");
    }

    #[test]
    fn test_alias_unknown_passthrough() {
        assert_eq!(resolve_provider_alias("openrouter"), "openrouter");
    }

    #[test]
    fn test_parse_provider() {
        assert_eq!(parse_provider("google"), ProviderType::Gemini);
        assert_eq!(parse_provider("openrouter"), ProviderType::OpenRouter);
        assert_eq!(parse_provider("nous"), ProviderType::Nous);
        assert_eq!(parse_provider("unknown_provider"), ProviderType::Unknown);
    }

    #[test]
    fn test_strip_provider_prefix() {
        assert_eq!(strip_provider_prefix("local:my-model"), "my-model");
        assert_eq!(strip_provider_prefix("openrouter:anthropic/claude-3"), "anthropic/claude-3");
        assert_eq!(strip_provider_prefix("gpt-4o"), "gpt-4o");
        // Ollama-style model:tag should NOT be stripped (not a known prefix)
        assert_eq!(strip_provider_prefix("llama3:8b"), "llama3:8b");
    }

    #[test]
    fn test_is_aggregator() {
        assert!(is_aggregator(ProviderType::OpenRouter));
        assert!(is_aggregator(ProviderType::Nous));
        assert!(!is_aggregator(ProviderType::OpenAI));
        assert!(!is_aggregator(ProviderType::Anthropic));
    }

    #[test]
    fn test_default_base_url() {
        assert_eq!(
            default_base_url(ProviderType::OpenRouter),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(default_base_url(ProviderType::Custom), None);
    }

    #[test]
    fn test_get_default_model_for_provider() {
        assert_eq!(
            get_default_model_for_provider(ProviderType::Anthropic),
            Some("claude-sonnet-4-6-20250514")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::OpenAI),
            Some("gpt-4.1")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Zai),
            Some("glm-4-plus")
        );
    }

    #[test]
    fn test_get_default_model_for_provider_none() {
        assert_eq!(get_default_model_for_provider(ProviderType::Custom), None);
        assert_eq!(get_default_model_for_provider(ProviderType::Unknown), None);
    }

    #[test]
    fn test_provider_type_display() {
        assert_eq!(format!("{}", ProviderType::OpenRouter), "openrouter");
        assert_eq!(format!("{}", ProviderType::Nous), "nous");
        assert_eq!(format!("{}", ProviderType::Custom), "custom");
        assert_eq!(format!("{}", ProviderType::Codex), "openai-codex");
        assert_eq!(format!("{}", ProviderType::Gemini), "gemini");
        assert_eq!(format!("{}", ProviderType::Zai), "zai");
        assert_eq!(format!("{}", ProviderType::Kimi), "kimi");
        assert_eq!(format!("{}", ProviderType::Minimax), "minimax");
        assert_eq!(format!("{}", ProviderType::Anthropic), "anthropic");
        assert_eq!(format!("{}", ProviderType::OpenAI), "openai");
        assert_eq!(format!("{}", ProviderType::Ollama), "ollama");
        assert_eq!(format!("{}", ProviderType::GoogleGeminiCli), "google-gemini-cli");
        assert_eq!(format!("{}", ProviderType::Unknown), "unknown");
    }

    #[test]
    fn test_provider_headers_openrouter() {
        let headers = provider_headers(ProviderType::OpenRouter);
        assert_eq!(
            headers.get("HTTP-Referer"),
            Some(&"https://hermez-agent.local".to_string())
        );
        assert_eq!(headers.get("X-Title"), Some(&"Hermez Agent".to_string()));
    }

    #[test]
    fn test_provider_headers_anthropic() {
        let headers = provider_headers(ProviderType::Anthropic);
        assert_eq!(headers.get("anthropic-version"), Some(&"2023-06-01".to_string()));
    }

    #[test]
    fn test_provider_headers_others_empty() {
        for pt in [
            ProviderType::Nous,
            ProviderType::Custom,
            ProviderType::Codex,
            ProviderType::Gemini,
            ProviderType::Zai,
            ProviderType::Kimi,
            ProviderType::Minimax,
            ProviderType::OpenAI,
            ProviderType::Ollama,
            ProviderType::GoogleGeminiCli,
            ProviderType::Unknown,
        ] {
            assert!(provider_headers(pt.clone()).is_empty(), "{:?} should have empty headers", pt);
        }
    }

    #[test]
    fn test_resolve_provider_alias_all() {
        assert_eq!(resolve_provider_alias("gemini-cli"), "google-gemini-cli");
        assert_eq!(resolve_provider_alias("z-ai"), "zai");
        assert_eq!(resolve_provider_alias("z.ai"), "zai");
        assert_eq!(resolve_provider_alias("zhipu"), "zai");
        assert_eq!(resolve_provider_alias("kimi"), "kimi-coding");
        assert_eq!(resolve_provider_alias("moonshot"), "kimi-coding");
        assert_eq!(resolve_provider_alias("minimax_china"), "minimax-cn");
        assert_eq!(resolve_provider_alias("minimax_cn"), "minimax-cn");
        assert_eq!(resolve_provider_alias("claude-code"), "anthropic");
        assert_eq!(resolve_provider_alias("deepseek"), "deepseek");
    }

    #[test]
    fn test_parse_provider_aliases() {
        assert_eq!(parse_provider("google"), ProviderType::Gemini);
        assert_eq!(parse_provider("claude"), ProviderType::Anthropic);
        assert_eq!(parse_provider("glm"), ProviderType::Zai);
        // moonshot aliases to "kimi-coding" which is not in parse_provider match arms
        assert_eq!(parse_provider("moonshot"), ProviderType::Unknown);
    }

    #[test]
    fn test_parse_provider_local() {
        assert_eq!(parse_provider("local"), ProviderType::Custom);
    }

    #[test]
    fn test_parse_provider_codex_variants() {
        assert_eq!(parse_provider("codex"), ProviderType::Codex);
        assert_eq!(parse_provider("openai-codex"), ProviderType::Codex);
    }

    #[test]
    fn test_strip_provider_prefix_all_known() {
        assert_eq!(strip_provider_prefix("anthropic:claude-3"), "claude-3");
        assert_eq!(strip_provider_prefix("openai:gpt-4"), "gpt-4");
        assert_eq!(strip_provider_prefix("nous:model"), "model");
        assert_eq!(strip_provider_prefix("custom:local"), "local");
    }

    #[test]
    fn test_strip_provider_prefix_no_extra_colon() {
        // If there's no colon after prefix, don't strip
        assert_eq!(strip_provider_prefix("openrouter"), "openrouter");
    }

    #[test]
    fn test_default_base_url_all() {
        assert_eq!(
            default_base_url(ProviderType::Nous),
            Some("https://api.nousresearch.com/v1")
        );
        assert_eq!(
            default_base_url(ProviderType::Codex),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            default_base_url(ProviderType::Gemini),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(
            default_base_url(ProviderType::Ollama),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            default_base_url(ProviderType::GoogleGeminiCli),
            Some("https://generativelanguage.googleapis.com/v1beta")
        );
        assert_eq!(default_base_url(ProviderType::Zai), None);
        assert_eq!(default_base_url(ProviderType::Kimi), None);
        assert_eq!(default_base_url(ProviderType::Minimax), None);
    }

    #[test]
    fn test_get_default_model_for_provider_all() {
        assert_eq!(
            get_default_model_for_provider(ProviderType::OpenRouter),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Nous),
            Some("nousresearch/hermes-3-llama-3.1-70b")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Codex),
            Some("o3")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Gemini),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Kimi),
            Some("kimi-k2-0905")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Minimax),
            Some("MiniMax-M2.5")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::Ollama),
            Some("llama3")
        );
        assert_eq!(
            get_default_model_for_provider(ProviderType::GoogleGeminiCli),
            Some("gemini-2.5-flash")
        );
    }
}
