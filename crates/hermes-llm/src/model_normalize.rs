#![allow(dead_code)]
//! Per-provider model name normalization.
//!
//! Mirrors Python `hermes_cli/model_normalize.py`.
//!
//! Different LLM providers expect model identifiers in different formats:
//! - **Aggregators** (OpenRouter, Nous, AI Gateway, Kilo Code) need
//!   `vendor/model` slugs like `anthropic/claude-sonnet-4.6`.
//! - **Anthropic** native API expects bare names with dots replaced by
//!   hyphens: `claude-sonnet-4-6`.
//! - **Copilot** expects bare names *with* dots preserved:
//!   `claude-sonnet-4.6`.
//! - **DeepSeek** only accepts two model identifiers:
//!   `deepseek-chat` and `deepseek-reasoner`.
//! - **Custom** and remaining providers pass the name through as-is.

use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Vendor prefix mapping
// ---------------------------------------------------------------------------

/// Maps the first hyphen-delimited token of a bare model name to the vendor
/// slug used by aggregator APIs (OpenRouter, Nous, etc.).
const VENDOR_PREFIXES: &[(&str, &str)] = &[
    ("claude", "anthropic"),
    ("gpt", "openai"),
    ("o1", "openai"),
    ("o3", "openai"),
    ("o4", "openai"),
    ("gemini", "google"),
    ("gemma", "google"),
    ("deepseek", "deepseek"),
    ("glm", "z-ai"),
    ("kimi", "moonshotai"),
    ("minimax", "minimax"),
    ("grok", "x-ai"),
    ("qwen", "qwen"),
    ("mimo", "xiaomi"),
    ("trinity", "arcee-ai"),
    ("nemotron", "nvidia"),
    ("llama", "meta-llama"),
    ("step", "stepfun"),
];

fn aggregator_providers() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&str>> = OnceLock::new();
    SET.get_or_init(|| {
        ["openrouter", "nous", "ai-gateway", "kilocode"]
            .iter()
            .copied()
            .collect()
    })
}

fn dot_to_hyphen_providers() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&str>> = OnceLock::new();
    SET.get_or_init(|| ["anthropic"].iter().copied().collect())
}

fn strip_vendor_only_providers() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&str>> = OnceLock::new();
    SET.get_or_init(|| {
        ["copilot", "copilot-acp", "openai-codex"]
            .iter()
            .copied()
            .collect()
    })
}

fn authoritative_native_providers() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&str>> = OnceLock::new();
    SET.get_or_init(|| ["gemini", "huggingface"].iter().copied().collect())
}

fn matching_prefix_strip_providers() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "zai",
            "kimi-coding",
            "kimi-coding-cn",
            "minimax",
            "minimax-cn",
            "alibaba",
            "qwen-oauth",
            "xiaomi",
            "arcee",
            "custom",
        ]
        .iter()
        .copied()
        .collect()
    })
}

// ---------------------------------------------------------------------------
// DeepSeek special handling
// ---------------------------------------------------------------------------

const DEEPSEEK_REASONER_KEYWORDS: &[&str] = &[
    "reasoner", "r1", "think", "reasoning", "cot",
];

const DEEPSEEK_CANONICAL_MODELS: &[&str] = &["deepseek-chat", "deepseek-reasoner"];

/// Map any model input to one of DeepSeek's two accepted identifiers.
fn normalize_for_deepseek(model_name: &str) -> String {
    let bare = strip_vendor_prefix(model_name).to_lowercase();

    if DEEPSEEK_CANONICAL_MODELS.contains(&bare.as_str()) {
        return bare;
    }

    for keyword in DEEPSEEK_REASONER_KEYWORDS {
        if bare.contains(keyword) {
            return "deepseek-reasoner".to_string();
        }
    }

    "deepseek-chat".to_string()
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Remove a `vendor/` prefix if present.
///
/// Examples:
/// - `anthropic/claude-sonnet-4.6` → `claude-sonnet-4.6`
/// - `claude-sonnet-4.6` → `claude-sonnet-4.6`
pub fn strip_vendor_prefix(model_name: &str) -> &str {
    match model_name.find('/') {
        Some(pos) => &model_name[pos + 1..],
        None => model_name,
    }
}

/// Replace dots with hyphens in a model name.
///
/// Anthropic's native API uses hyphens where marketing names use dots:
/// `claude-sonnet-4.6` → `claude-sonnet-4-6`.
pub fn dots_to_hyphens(model_name: &str) -> String {
    model_name.replace('.', "-")
}

/// Resolve provider aliases to Hermes' canonical ids.
fn normalize_provider_alias(provider_name: &str) -> String {
    let raw = provider_name.trim().to_lowercase();
    if raw.is_empty() {
        return raw;
    }
    // In Rust we don't have the full normalize_provider mapping;
    // common aliases are handled here.
    match raw.as_str() {
        "openrouter" | "or" => "openrouter".to_string(),
        "anthropic" | "claude" => "anthropic".to_string(),
        "openai" | "gpt" => "openai".to_string(),
        "copilot" | "github" | "github-copilot" => "copilot".to_string(),
        "deepseek" | "ds" => "deepseek".to_string(),
        "google" | "gemini" => "gemini".to_string(),
        _ => raw,
    }
}

/// Strip `provider/` only when the prefix matches the target provider.
fn strip_matching_provider_prefix(model_name: &str, target_provider: &str) -> String {
    let Some(pos) = model_name.find('/') else {
        return model_name.to_string();
    };

    let prefix = model_name[..pos].trim();
    let remainder = model_name[pos + 1..].trim();

    if prefix.is_empty() || remainder.is_empty() {
        return model_name.to_string();
    }

    let normalized_prefix = normalize_provider_alias(prefix);
    let normalized_target = normalize_provider_alias(target_provider);

    if !normalized_prefix.is_empty() && normalized_prefix == normalized_target {
        return remainder.to_string();
    }

    model_name.to_string()
}

/// Detect the vendor slug from a bare model name.
///
/// Uses the first hyphen-delimited token of the model name to look up
/// the corresponding vendor in `VENDOR_PREFIXES`. Also handles
/// case-insensitive matching and special patterns.
///
/// Returns the vendor slug (e.g. `"anthropic"`, `"openai"`) or `None`
/// if no vendor can be confidently detected.
pub fn detect_vendor(model_name: &str) -> Option<&'static str> {
    let name = model_name.trim();
    if name.is_empty() {
        return None;
    }

    // If there's already a vendor/ prefix, extract and normalize it
    if let Some(pos) = name.find('/') {
        let prefix = name[..pos].trim().to_lowercase();
        if prefix.is_empty() {
            return None;
        }
        // Map known aliases to canonical vendor slugs (avoids String::leak)
        return match prefix.as_str() {
            "anthropic" | "claude" => Some("anthropic"),
            "openai" | "gpt" | "o1" | "o3" | "o4" => Some("openai"),
            "deepseek" | "ds" => Some("deepseek"),
            "google" | "gemini" | "gemma" => Some("google"),
            "copilot" | "github" | "github-copilot" => Some("copilot"),
            "openrouter" | "or" => Some("openrouter"),
            "nous" => Some("nous"),
            "ai-gateway" | "aigateway" => Some("ai-gateway"),
            "kilocode" => Some("kilocode"),
            "z-ai" | "zai" => Some("z-ai"),
            "moonshotai" | "kimi" => Some("moonshotai"),
            "minimax" => Some("minimax"),
            "x-ai" | "grok" => Some("x-ai"),
            "qwen" => Some("qwen"),
            "xiaomi" | "mimo" => Some("xiaomi"),
            "arcee-ai" | "arcee" | "trinity" => Some("arcee-ai"),
            "nvidia" | "nemotron" => Some("nvidia"),
            "meta-llama" | "llama" | "meta" => Some("meta-llama"),
            "stepfun" | "step" => Some("stepfun"),
            "huggingface" => Some("huggingface"),
            _ => None,
        };
    }

    let name_lower = name.to_lowercase();

    // Try first hyphen-delimited token (exact match)
    let first_token = name_lower.split('-').next().unwrap_or("");
    for (prefix, vendor) in VENDOR_PREFIXES {
        if *prefix == first_token {
            return Some(*vendor);
        }
    }

    // Handle patterns where the first token includes version digits,
    // e.g. "qwen3.5-plus" -> first token "qwen3.5", but prefix is "qwen"
    for (prefix, vendor) in VENDOR_PREFIXES {
        if name_lower.starts_with(prefix) {
            return Some(*vendor);
        }
    }

    None
}

/// Prepend the detected `vendor/` prefix if missing.
///
/// Used for aggregator providers that require `vendor/model` format.
/// If the name already contains a `/`, it is returned as-is.
/// If no vendor can be detected, the name is returned unchanged.
pub fn prepend_vendor(model_name: &str) -> String {
    if model_name.contains('/') {
        return model_name.to_string();
    }

    if let Some(vendor) = detect_vendor(model_name) {
        return format!("{vendor}/{model_name}");
    }

    model_name.to_string()
}

// ---------------------------------------------------------------------------
// Main normalisation entry point
// ---------------------------------------------------------------------------

/// Translate a model name into the format the target provider's API expects.
///
/// This is the primary entry point for model name normalisation. It accepts
/// any user-facing model identifier and transforms it for the specific
/// provider that will receive the API call.
///
/// # Examples
///
/// ```
/// use hermes_llm::model_normalize::normalize_model_for_provider;
///
/// assert_eq!(
///     normalize_model_for_provider("claude-sonnet-4.6", "openrouter"),
///     "anthropic/claude-sonnet-4.6"
/// );
/// assert_eq!(
///     normalize_model_for_provider("anthropic/claude-sonnet-4.6", "anthropic"),
///     "claude-sonnet-4-6"
/// );
/// assert_eq!(
///     normalize_model_for_provider("deepseek-r1", "deepseek"),
///     "deepseek-reasoner"
/// );
/// ```
pub fn normalize_model_for_provider(model_input: &str, target_provider: &str) -> String {
    let name = model_input.trim();
    if name.is_empty() {
        return name.to_string();
    }

    let provider = normalize_provider_alias(target_provider);
    let provider_ref = provider.as_str();

    // --- Aggregators: need vendor/model format ---
    if aggregator_providers().contains(provider_ref) {
        return prepend_vendor(name);
    }

    // --- OpenCode Zen: Claude stays hyphenated; other models keep dots ---
    if provider_ref == "opencode-zen" {
        let bare = strip_matching_provider_prefix(name, target_provider);
        if bare.contains('/') {
            return bare;
        }
        if bare.to_lowercase().starts_with("claude-") {
            return dots_to_hyphens(&bare);
        }
        return bare;
    }

    // --- Anthropic: strip matching provider prefix, dots -> hyphens ---
    if dot_to_hyphen_providers().contains(provider_ref) {
        let bare = strip_matching_provider_prefix(name, target_provider);
        if bare.contains('/') {
            return bare;
        }
        return dots_to_hyphens(&bare);
    }

    // --- Copilot: strip matching provider prefix, keep dots ---
    if strip_vendor_only_providers().contains(provider_ref) {
        let stripped = strip_matching_provider_prefix(name, target_provider);
        if stripped == name && name.starts_with("openai/") {
            // openai-codex maps openai/gpt-5.4 -> gpt-5.4
            return strip_vendor_prefix(name).to_string();
        }
        return stripped;
    }

    // --- DeepSeek: map to one of two canonical names ---
    if provider_ref == "deepseek" {
        let bare = strip_matching_provider_prefix(name, target_provider);
        if bare.contains('/') {
            return bare;
        }
        return normalize_for_deepseek(&bare);
    }

    // --- Direct providers: repair matching provider prefixes only ---
    if matching_prefix_strip_providers().contains(provider_ref) {
        return strip_matching_provider_prefix(name, target_provider);
    }

    // --- Authoritative native providers: preserve user-facing slugs as-is ---
    if authoritative_native_providers().contains(provider_ref) {
        return name.to_string();
    }

    // --- Custom & all others: pass through as-is ---
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_vendor_prefix() {
        assert_eq!(strip_vendor_prefix("anthropic/claude-sonnet-4.6"), "claude-sonnet-4.6");
        assert_eq!(strip_vendor_prefix("claude-sonnet-4.6"), "claude-sonnet-4.6");
        assert_eq!(strip_vendor_prefix("meta-llama/llama-4-scout"), "llama-4-scout");
    }

    #[test]
    fn test_detect_vendor() {
        assert_eq!(detect_vendor("claude-sonnet-4.6"), Some("anthropic"));
        assert_eq!(detect_vendor("gpt-5.4-mini"), Some("openai"));
        assert_eq!(detect_vendor("anthropic/claude-sonnet-4.6"), Some("anthropic"));
        assert_eq!(detect_vendor("my-custom-model"), None);
        assert_eq!(detect_vendor("qwen3.5-plus"), Some("qwen"));
    }

    #[test]
    fn test_prepend_vendor() {
        assert_eq!(
            prepend_vendor("claude-sonnet-4.6"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            prepend_vendor("anthropic/claude-sonnet-4.6"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(prepend_vendor("my-custom-thing"), "my-custom-thing");
    }

    #[test]
    fn test_normalize_model_openrouter() {
        assert_eq!(
            normalize_model_for_provider("claude-sonnet-4.6", "openrouter"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            normalize_model_for_provider("gpt-5.4", "openrouter"),
            "openai/gpt-5.4"
        );
    }

    #[test]
    fn test_normalize_model_anthropic() {
        assert_eq!(
            normalize_model_for_provider("anthropic/claude-sonnet-4.6", "anthropic"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_for_provider("claude-sonnet-4.6", "anthropic"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn test_normalize_model_copilot() {
        // Copilot strips matching provider prefix; non-matching prefixes
        // are kept as-is (unlike the docstring example in Python which
        // appears outdated).
        assert_eq!(
            normalize_model_for_provider("anthropic/claude-sonnet-4.6", "copilot"),
            "anthropic/claude-sonnet-4.6"
        );
        // openai/ prefix gets special handling for codex compatibility
        assert_eq!(
            normalize_model_for_provider("openai/gpt-5.4", "copilot"),
            "gpt-5.4"
        );
        // Bare name with dots preserved
        assert_eq!(
            normalize_model_for_provider("claude-sonnet-4.6", "copilot"),
            "claude-sonnet-4.6"
        );
    }

    #[test]
    fn test_normalize_model_deepseek() {
        assert_eq!(
            normalize_model_for_provider("deepseek-v3", "deepseek"),
            "deepseek-chat"
        );
        assert_eq!(
            normalize_model_for_provider("deepseek-r1", "deepseek"),
            "deepseek-reasoner"
        );
        assert_eq!(
            normalize_model_for_provider("deepseek-chat", "deepseek"),
            "deepseek-chat"
        );
    }

    #[test]
    fn test_normalize_model_opencode_zen() {
        assert_eq!(
            normalize_model_for_provider("claude-sonnet-4.6", "opencode-zen"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_for_provider("minimax-m2.5-free", "opencode-zen"),
            "minimax-m2.5-free"
        );
    }

    #[test]
    fn test_normalize_model_custom() {
        assert_eq!(
            normalize_model_for_provider("my-model", "custom"),
            "my-model"
        );
    }

    #[test]
    fn test_normalize_model_zai() {
        assert_eq!(
            normalize_model_for_provider("zai/glm-5.1", "zai"),
            "glm-5.1"
        );
        assert_eq!(
            normalize_model_for_provider("claude-sonnet-4.6", "zai"),
            "claude-sonnet-4.6"
        );
    }

    #[test]
    fn test_dots_to_hyphens() {
        assert_eq!(dots_to_hyphens("claude-sonnet-4.6"), "claude-sonnet-4-6");
        assert_eq!(dots_to_hyphens("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn test_normalize_provider_alias() {
        assert_eq!(normalize_provider_alias("openrouter"), "openrouter");
        assert_eq!(normalize_provider_alias("OR"), "openrouter");
        assert_eq!(normalize_provider_alias("claude"), "anthropic");
    }
}
