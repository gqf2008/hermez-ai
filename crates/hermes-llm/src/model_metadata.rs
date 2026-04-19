//! Model metadata: context length discovery, capability detection, and token estimation.
//!
//! Multi-source context length resolution:
//! 0. Explicit config override
//! 1. Persistent cache (YAML)
//! 2. Active endpoint metadata (/models for custom endpoints)
//! 3. Local server query (Ollama, LM Studio, vLLM, llama.cpp)
//! 4. Anthropic /v1/models API
//! 5. Provider-aware lookups (models.dev via models_dev crate)
//! 6. OpenRouter live API metadata (cached 1 hour)
//! 7. Hardcoded DEFAULT_CONTEXT_LENGTHS (substring matching, longest-first)
//! 8. Default fallback (128K)
//!
//! Mirrors the Python `agent/model_metadata.py`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::pricing::UsagePricing;

// =========================================================================
// Constants
// =========================================================================

/// Minimum context length required to run Hermes Agent.
/// Models with fewer tokens cannot maintain enough working memory
/// for tool-calling workflows.
pub const MINIMUM_CONTEXT_LENGTH: usize = 64_000;

/// Default context length when no detection method succeeds.
pub const DEFAULT_FALLBACK_CONTEXT: usize = 128_000;

/// Container-local DNS suffixes that should be treated as local endpoints.
/// Used to skip network-related fallbacks and proxying for Docker-served models.
const CONTAINER_LOCAL_SUFFIXES: &[&str] = &[
    "host.docker.internal",
    "host.containers.internal",
    "gateway.docker.internal",
    "host.lima.internal",
];

/// Local server hostnames / address patterns.
const LOCAL_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "0.0.0.0"];

/// Docker / Podman / Lima DNS suffixes for host resolution.
const CONTAINER_DNS_SUFFIXES: &[&str] = &[
    ".docker.internal",
    ".containers.internal",
    ".lima.internal",
];

/// Provider names that can appear as a "provider:" prefix before a model ID.
/// Only these are stripped — Ollama-style "model:tag" colons (e.g. "qwen3.5:27b")
/// are preserved so the full model name reaches cache lookups and server queries.
const PROVIDER_PREFIXES: &[&str] = &[
    "openrouter",
    "nous",
    "openai-codex",
    "copilot",
    "copilot-acp",
    "gemini",
    "zai",
    "kimi-coding",
    "kimi-coding-cn",
    "minimax",
    "minimax-cn",
    "anthropic",
    "deepseek",
    "opencode-zen",
    "opencode-go",
    "ai-gateway",
    "kilocode",
    "alibaba",
    "qwen-oauth",
    "xiaomi",
    "arcee",
    "custom",
    "local",
    // Common aliases
    "google",
    "google-gemini",
    "google-ai-studio",
    "glm",
    "z-ai",
    "z.ai",
    "zhipu",
    "github",
    "github-copilot",
    "github-models",
    "kimi",
    "moonshot",
    "kimi-cn",
    "moonshot-cn",
    "claude",
    "deep-seek",
    "opencode",
    "zen",
    "go",
    "vercel",
    "kilo",
    "dashscope",
    "aliyun",
    "qwen",
    "mimo",
    "xiaomi-mimo",
    "arcee-ai",
    "arceeai",
    "xai",
    "x-ai",
    "x.ai",
    "grok",
    "qwen-portal",
];

/// URL-to-provider mapping for inferring provider from base_url.
const URL_TO_PROVIDER: &[(&str, &str)] = &[
    ("api.openai.com", "openai"),
    ("chatgpt.com", "openai"),
    ("api.anthropic.com", "anthropic"),
    ("api.z.ai", "zai"),
    ("api.moonshot.ai", "kimi-coding"),
    ("api.moonshot.cn", "kimi-coding-cn"),
    ("api.kimi.com", "kimi-coding"),
    ("api.arcee.ai", "arcee"),
    ("api.minimax", "minimax"),
    ("dashscope.aliyuncs.com", "alibaba"),
    ("dashscope-intl.aliyuncs.com", "alibaba"),
    ("portal.qwen.ai", "qwen-oauth"),
    ("openrouter.ai", "openrouter"),
    ("generativelanguage.googleapis.com", "gemini"),
    ("inference-api.nousresearch.com", "nous"),
    ("api.deepseek.com", "deepseek"),
    ("api.githubcopilot.com", "copilot"),
    ("models.github.ai", "copilot"),
    ("api.fireworks.ai", "fireworks"),
    ("opencode.ai", "opencode-go"),
    ("api.x.ai", "xai"),
    ("api.xiaomimimo.com", "xiaomi"),
    ("xiaomimimo.com", "xiaomi"),
];

/// Context length probe tiers (descending).
pub const CONTEXT_PROBE_TIERS: &[usize] = &[128_000, 64_000, 32_000, 16_000, 8_000];

/// Keys used to find context length in JSON payloads.
const CONTEXT_LENGTH_KEYS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "max_position_embeddings",
    "max_model_len",
    "max_input_tokens",
    "max_sequence_length",
    "max_seq_len",
    "n_ctx_train",
    "n_ctx",
];

/// Keys used to find max completion tokens in JSON payloads.
const MAX_COMPLETION_KEYS: &[&str] = &[
    "max_completion_tokens",
    "max_output_tokens",
    "max_tokens",
];

// =========================================================================
// Default context lengths (hardcoded fallback)
// =========================================================================

/// Default context lengths for known model families (fallback).
/// Sorted by specificity — the resolution code uses longest-key-first matching.
/// Keys are substring-matched against the model name.
const DEFAULT_CONTEXT_LENGTHS: &[(&str, usize)] = &[
    // Anthropic Claude 4.6 (1M context) — bare IDs only to avoid
    // fuzzy-match collisions (e.g. "anthropic/claude-sonnet-4" is a
    // substring of "anthropic/claude-sonnet-4.6").
    ("claude-opus-4-6", 1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-opus-4.6", 1_000_000),
    ("claude-sonnet-4.6", 1_000_000),
    // OpenAI — GPT-5 family (most have 400k; specific overrides first)
    ("gpt-5.4-nano", 400_000),
    ("gpt-5.4-mini", 400_000),
    ("gpt-5.4", 1_050_000),
    ("gpt-5.3-codex-spark", 128_000),
    ("gpt-5.1-chat", 128_000),
    ("gpt-5", 400_000),
    // OpenAI — GPT-4 family
    ("gpt-4.1", 1_047_576),
    ("gpt-4-turbo", 128_000),
    ("gpt-4o", 128_000),
    ("gpt-4", 128_000),
    // OpenAI — GPT-3.5
    ("gpt-3.5-turbo", 16_385),
    // Google Gemini
    ("gemini", 1_048_576),
    // Gemma (open models served via AI Studio)
    ("gemma-4-31b", 256_000),
    ("gemma-4-26b", 256_000),
    ("gemma-3", 131_072),
    ("gemma", 8_192),
    // Anthropic Claude (catch-all for older models)
    ("claude-3-5-sonnet", 200_000),
    ("claude-3-sonnet", 200_000),
    ("claude-3-opus", 200_000),
    ("claude-opus", 200_000),
    ("claude-3-haiku", 200_000),
    ("claude-haiku", 200_000),
    ("claude", 200_000),
    // DeepSeek
    ("deepseek", 128_000),
    // Meta Llama
    ("llama-3", 128_000),
    ("llama3", 128_000),
    ("llama", 131_072),
    // Mistral
    ("mistral", 32_768),
    // Qwen — specific model families before the catch-all
    ("qwen3-coder-plus", 1_000_000),
    ("qwen3-coder", 262_144),
    ("qwen", 131_072),
    // MiniMax
    ("minimax", 204_800),
    // GLM
    ("glm", 202_752),
    // xAI Grok — values sourced from models.dev (2026-04)
    ("grok-code-fast", 256_000),
    ("grok-4-1-fast", 2_000_000),
    ("grok-2-vision", 8_192),
    ("grok-4-fast", 2_000_000),
    ("grok-4.20", 2_000_000),
    ("grok-4", 256_000),
    ("grok-3", 131_072),
    ("grok-2", 131_072),
    ("grok", 131_072),
    // Kimi
    ("kimi", 262_144),
    // Arcee
    ("trinity", 262_144),
    // OpenRouter
    ("elephant", 262_144),
    // Hugging Face Inference Providers
    ("Qwen/Qwen3.5-397B-A17B", 131_072),
    ("Qwen/Qwen3.5-35B-A3B", 131_072),
    ("deepseek-ai/DeepSeek-V3.2", 65_536),
    ("moonshotai/Kimi-K2.5", 262_144),
    ("moonshotai/Kimi-K2-Thinking", 262_144),
    ("MiniMaxAI/MiniMax-M2.5", 204_800),
    ("XiaomiMiMo/MiMo-V2-Flash", 256_000),
    // MiMo variants (bare IDs)
    ("mimo-v2-pro", 1_000_000),
    ("mimo-v2-omni", 256_000),
    ("mimo-v2-flash", 256_000),
    // Z.AI GLM-5
    ("zai-org/GLM-5", 202_752),
];

// =========================================================================
// Ollama tag pattern
// =========================================================================

/// Pattern that matches Ollama-style tags (e.g. "7b", "latest", "q4_0", "fp16").
fn ollama_tag_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(\d+\.?\d*b|latest|stable|q\d|fp?\d|instruct|chat|coder|vision|text)")
            .unwrap()
    })
}

// =========================================================================
// Provider prefix stripping
// =========================================================================

/// Strip a recognised provider prefix from a model string.
///
/// `"local:my-model"` → `"my-model"`
/// `"qwen3.5:27b"`   → `"qwen3.5:27b"`  (unchanged — not a provider prefix)
/// `"qwen:0.5b"`     → `"qwen:0.5b"`    (unchanged — Ollama model:tag)
/// `"deepseek:latest"`→ `"deepseek:latest"` (unchanged — Ollama model:tag)
pub fn strip_provider_prefix(model: &str) -> &str {
    if !model.contains(':') || model.starts_with("http") {
        return model;
    }
    let mut parts = model.splitn(2, ':');
    let prefix = parts.next().unwrap_or("");
    let suffix = parts.next().unwrap_or("");

    let prefix_lower = prefix.trim().to_lowercase();
    let is_provider = PROVIDER_PREFIXES
        .iter()
        .any(|p| p.eq_ignore_ascii_case(&prefix_lower));

    if is_provider {
        // Don't strip if suffix looks like an Ollama tag
        if ollama_tag_pattern().is_match(suffix.trim()) {
            return model;
        }
        return suffix.trim();
    }
    model
}

/// Normalize a model slug for cache lookup: strip OpenRouter-format prefixes
/// (containing "/") when the cache-hit model doesn't match the target provider.
///
/// Mirrors Python `_compat_model()` helper.
pub fn compat_model_slug(model: &str) -> String {
    if let Some((_, rest)) = model.split_once('/') {
        rest.to_string()
    } else {
        model.to_string()
    }
}

/// Normalize model version separators for matching.
///
/// Nous uses dashes: claude-opus-4-6, claude-sonnet-4-5
/// OpenRouter uses dots: claude-opus-4.6, claude-sonnet-4.5
/// Normalize both to dashes for comparison.
pub fn normalize_model_version(model: &str) -> String {
    model.replace('.', "-")
}

// =========================================================================
// URL / provider utilities
// =========================================================================

/// Normalize a base URL: strip trailing slashes.
pub fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// Check if base_url points to OpenRouter.
pub fn is_openrouter_base_url(base_url: &str) -> bool {
    normalize_base_url(base_url)
        .to_lowercase()
        .contains("openrouter.ai")
}

/// Check if base_url is a custom endpoint (non-empty and non-OpenRouter).
pub fn is_custom_endpoint(base_url: &str) -> bool {
    let normalized = normalize_base_url(base_url);
    !normalized.is_empty() && !is_openrouter_base_url(&normalized)
}

/// Infer the models.dev provider name from a base URL.
pub fn infer_provider_from_url(base_url: &str) -> Option<&'static str> {
    let normalized = normalize_base_url(base_url).to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    for (url_part, provider) in URL_TO_PROVIDER {
        if normalized.contains(*url_part) {
            return Some(provider);
        }
    }
    None
}

/// Check if base_url corresponds to a known provider.
pub fn is_known_provider_base_url(base_url: &str) -> bool {
    infer_provider_from_url(base_url).is_some()
}

// =========================================================================
// Local endpoint detection
// =========================================================================

/// Return true if base_url points to a local machine (localhost / RFC-1918 / WSL).
pub fn is_local_endpoint(base_url: &str) -> bool {
    let normalized = normalize_base_url(base_url);
    if normalized.is_empty() {
        return false;
    }

    // Quick string checks
    for host in LOCAL_HOSTS {
        if normalized.contains(host) {
            return true;
        }
    }
    for suffix in CONTAINER_LOCAL_SUFFIXES {
        if normalized.contains(suffix) {
            return true;
        }
    }
    if normalized.contains("http://host") {
        return true;
    }

    // Parse the host
    let url_with_scheme = if normalized.contains("://") {
        normalized.clone()
    } else {
        format!("http://{}", normalized)
    };

    // Extract hostname
    let host = match extract_hostname(&url_with_scheme) {
        Some(h) => h,
        None => return false,
    };

    // Check container DNS suffixes
    if CONTAINER_DNS_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }

    // Check RFC-1918 private ranges
    is_private_ip(&host)
}

fn extract_hostname(url: &str) -> Option<String> {
    // Simple URL parsing
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = after_scheme.split('/').next()?;
    let host = host_port.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn is_private_ip(host: &str) -> bool {
    // Try parsing as IP address
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 {
        if let (Ok(first), Ok(second), _, _) = (
            parts[0].parse::<u32>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
            parts[3].parse::<u32>(),
        ) {
            // 10.0.0.0/8
            if first == 10 {
                return true;
            }
            // 172.16.0.0/12
            if first == 172 && (16..=31).contains(&second) {
                return true;
            }
            // 192.168.0.0/16
            if first == 192 && second == 168 {
                return true;
            }
            // 127.0.0.0/8 (loopback)
            if first == 127 {
                return true;
            }
        }
    }
    // IPv6 loopback
    if host == "::1" {
        return true;
    }
    false
}

// =========================================================================
// Payload extraction utilities
// =========================================================================

/// Check if a value is a "reasonable" integer context length.
fn coerce_reasonable_int(value: &serde_json::Value, min: i64, max: i64) -> Option<usize> {
    let n = match value {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => {
            let cleaned = s.replace(',', "");
            cleaned.parse::<i64>().ok()?
        }
        _ => return None,
    };
    if n >= min && n <= max {
        Some(n as usize)
    } else {
        None
    }
}

/// Iterate over all nested objects in a JSON value.
fn iter_nested_dicts<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Map<String, serde_json::Value>>) {
    match value {
        serde_json::Value::Object(map) => {
            out.push(map);
            for v in map.values() {
                iter_nested_dicts(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                iter_nested_dicts(v, out);
            }
        }
        _ => {}
    }
}

/// Extract the first integer matching any of the candidate keys from nested dicts.
fn extract_first_int(payload: &serde_json::Value, keys: &[&str]) -> Option<usize> {
    let keyset: std::collections::HashSet<&str> = keys.iter().copied().collect();
    let mut maps = Vec::new();
    iter_nested_dicts(payload, &mut maps);

    for map in maps {
        for (key, value) in map {
            if keyset.contains(key.to_lowercase().as_str()) {
                if let Some(n) = coerce_reasonable_int(value, 1024, 10_000_000) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extract context length from a JSON payload (e.g. /models response).
pub fn extract_context_length(payload: &serde_json::Value) -> Option<usize> {
    extract_first_int(payload, CONTEXT_LENGTH_KEYS)
}

/// Extract max completion tokens from a JSON payload.
pub fn extract_max_completion_tokens(payload: &serde_json::Value) -> Option<usize> {
    extract_first_int(payload, MAX_COMPLETION_KEYS)
}

/// Extract pricing from a JSON payload.
pub fn extract_pricing_from_payload(payload: &serde_json::Value) -> Option<UsagePricing> {
    let alias_map: &[(&str, &[&str])] = &[
        ("prompt", &["prompt", "input", "input_cost_per_token", "prompt_token_cost"]),
        ("completion", &["completion", "output", "output_cost_per_token", "completion_token_cost"]),
        ("request", &["request", "request_cost"]),
        ("cache_read", &["cache_read", "cached_prompt", "input_cache_read", "cache_read_cost_per_token"]),
        ("cache_write", &["cache_write", "cache_creation", "input_cache_write", "cache_write_cost_per_token"]),
    ];

    let mut maps = Vec::new();
    iter_nested_dicts(payload, &mut maps);

    for map in maps {
        let normalized: HashMap<String, &serde_json::Value> = map
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();

        // Check if any pricing alias exists
        let has_any = alias_map.iter().any(|(_, aliases)| {
            aliases.iter().any(|alias| normalized.contains_key(*alias))
        });
        if !has_any {
            continue;
        }

        let mut pricing = UsagePricing::default();

        for (target, aliases) in alias_map {
            for alias in *aliases {
                if let Some(val) = normalized.get(*alias) {
                    if let Some(n) = val.as_f64() {
                        match *target {
                            "prompt" => pricing.prompt_per_million = n * 1_000_000.0,
                            "completion" => pricing.completion_per_million = n * 1_000_000.0,
                            "request" => pricing.per_request = Some(n),
                            "cache_read" => pricing.cache_read_per_million = Some(n * 1_000_000.0),
                            "cache_write" => pricing.cache_write_per_million = Some(n * 1_000_000.0),
                            _ => {}
                        }
                        break;
                    }
                }
            }
        }

        if pricing.is_set() {
            return Some(pricing);
        }
    }
    None
}

/// Parse context length from an API response body.
pub fn parse_context_length_from_response(response: &serde_json::Value) -> Option<usize> {
    response
        .get("context_length")
        .or_else(|| response.get("context_window"))
        .or_else(|| response.get("max_tokens"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .or_else(|| {
            response
                .get("data")
                .and_then(|d| d.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("max_model_len"))
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
        })
        .or_else(|| extract_context_length(response))
}

// =========================================================================
// Model ID matching
// =========================================================================

/// Return true if candidate_id (from server) matches lookup_model (configured).
///
/// Supports:
/// - Exact match
/// - Slug match: "publisher/slug" matches "slug"
pub fn model_id_matches(candidate_id: &str, lookup_model: &str) -> bool {
    if candidate_id == lookup_model {
        return true;
    }
    if candidate_id.contains('/') && candidate_id.rsplit_once('/').unwrap().1 == lookup_model {
        return true;
    }
    false
}

// =========================================================================
// Error parsing from API responses
// =========================================================================

/// Try to extract the actual context limit from an API error message.
///
/// Many providers include the limit in their error text, e.g.:
///   - "maximum context length is 32768 tokens"
///   - "context_length_exceeded: 131072"
///   - "Maximum context size 32768 exceeded"
pub fn parse_context_limit_from_error(error_msg: &str) -> Option<usize> {
    let error_lower = error_msg.to_lowercase();

    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"(?:max(?:imum)?|limit)\s*(?:context\s*)?(?:length|size|window)?\s*(?:is|of|:)?\s*(\d{4,})").unwrap(),
            Regex::new(r"context\s*(?:length|size|window)\s*(?:is|of|:)?\s*(\d{4,})").unwrap(),
            Regex::new(r"context[_-]length[_-]\w+[:\s]+(\d{4,})").unwrap(),
            Regex::new(r"(\d{4,})\s*(?:token)?\s*(?:context|limit)").unwrap(),
            Regex::new(r">\s*(\d{4,})\s*(?:max|limit|token)").unwrap(),
            Regex::new(r"(\d{4,})\s*(?:max(?:imum)?)\b").unwrap(),
        ]
    });

    for pattern in patterns.iter() {
        if let Some(caps) = pattern.captures(&error_lower) {
            if let Some(m) = caps.get(1) {
                if let Ok(limit) = m.as_str().parse::<usize>() {
                    if (1024..=10_000_000).contains(&limit) {
                        return Some(limit);
                    }
                }
            }
        }
    }
    None
}

/// Detect an "output cap too large" error and return available output tokens.
///
/// Anthropic format:
///   "max_tokens: 32768 > context_window: 200000 - input_tokens: 190000 = available_tokens: 10000"
pub fn parse_available_output_tokens_from_error(error_msg: &str) -> Option<usize> {
    let error_lower = error_msg.to_lowercase();

    let is_output_cap = error_lower.contains("max_tokens")
        && (error_lower.contains("available_tokens") || error_lower.contains("available tokens"));
    if !is_output_cap {
        return None;
    }

    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"available_tokens[:\s]+(\d+)").unwrap(),
            Regex::new(r"available\s+tokens[:\s]+(\d+)").unwrap(),
            Regex::new(r"=\s*(\d+)\s*$").unwrap(),
        ]
    });

    for pattern in patterns.iter() {
        if let Some(caps) = pattern.captures(&error_lower) {
            if let Some(m) = caps.get(1) {
                if let Ok(tokens) = m.as_str().parse::<usize>() {
                    if tokens >= 1 {
                        return Some(tokens);
                    }
                }
            }
        }
    }
    None
}

// =========================================================================
// Context cache (persistent YAML)
// =========================================================================

fn get_context_cache_path() -> Option<PathBuf> {
    Some(hermes_core::hermes_home::get_hermes_home().join("context_length_cache.yaml"))
}

fn load_context_cache() -> HashMap<String, usize> {
    let path = match get_context_cache_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    if !path.exists() {
        return HashMap::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| parse_yaml_simple(&content))
        .unwrap_or_default()
}

/// Minimal YAML parser for the simple flat dict format we use.
/// Handles: `key: value` lines under `context_lengths:` top-level key.
/// Keys may contain colons (URLs), so we find the last `: <digits>` pattern.
fn parse_yaml_simple(content: &str) -> Option<HashMap<String, usize>> {
    let mut result = HashMap::new();
    let mut in_context_lengths = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "context_lengths:" {
            in_context_lengths = true;
            continue;
        }
        if in_context_lengths {
            // Non-indented, non-empty line ends the section
            if trimmed.is_empty() || !line.starts_with(char::is_whitespace) {
                if trimmed.is_empty() {
                    break;
                }
                if !line.starts_with(char::is_whitespace) && !line.starts_with('"') {
                    break;
                }
            }
            // Find last colon followed by a number
            if let Some(pos) = trimmed.rfind(':') {
                let val_part = trimmed[pos + 1..].trim();
                if let Ok(val) = val_part.parse::<usize>() {
                    let key_part = trimmed[..pos].trim().trim_matches('"').trim_matches('\'');
                    if !key_part.is_empty() {
                        result.insert(key_part.to_string(), val);
                    }
                }
            }
        }
    }
    Some(result)
}

fn save_context_cache(cache: &HashMap<String, usize>) {
    let Some(path) = get_context_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut lines = String::from("context_lengths:\n");
    // Sort for stable output
    let mut entries: Vec<_> = cache.iter().collect();
    entries.sort_by_key(|(k, _)| (*k).clone());
    for (key, val) in entries {
        lines.push_str(&format!("  {}: {}\n", key, val));
    }

    let tmp = path.with_extension("yaml.tmp");
    if std::fs::write(&tmp, lines).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Persist a discovered context length for a model+provider combo.
pub fn save_context_length(model: &str, base_url: &str, length: usize) {
    let key = format!("{}@{}", model, base_url);
    let mut cache = load_context_cache();
    if cache.get(&key) == Some(&length) {
        return;
    }
    cache.insert(key, length);
    save_context_cache(&cache);
}

/// Look up a previously discovered context length for model+provider.
pub fn get_cached_context_length_persistent(model: &str, base_url: &str) -> Option<usize> {
    let key = format!("{}@{}", model, base_url);
    load_context_cache().remove(&key)
}

// =========================================================================
// In-memory cache
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub model: String,
    pub context_length: Option<usize>,
    pub max_completion_tokens: Option<usize>,
    pub pricing: Option<UsagePricing>,
    pub last_fetched: Option<String>,
}

struct CachedEntry {
    metadata: ModelMetadata,
    fetched_at: Instant,
    ttl: Duration,
}

impl CachedEntry {
    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > self.ttl
    }
}

fn cache() -> &'static Mutex<HashMap<String, CachedEntry>> {
    static C: OnceLock<Mutex<HashMap<String, CachedEntry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

const OPENROUTER_TTL: Duration = Duration::from_secs(3600);
const CUSTOM_TTL: Duration = Duration::from_secs(300);

/// Get context length for a model, using the multi-source resolution chain.
pub fn get_context_length(model: &str, base_url: &str) -> Option<usize> {
    let cache_key = format!("{}@{}", model, base_url);
    {
        let c = cache().lock();
        if let Some(entry) = c.get(&cache_key) {
            if !entry.is_expired() {
                return entry.metadata.context_length;
            }
        }
    }

    // Hardcoded fallback (substring matching)
    lookup_default_context_length(model)
}

/// Store metadata in the cache.
pub fn cache_metadata(model: &str, base_url: &str, metadata: ModelMetadata, is_openrouter: bool) {
    let cache_key = format!("{}@{}", model, base_url);
    let ttl = if is_openrouter { OPENROUTER_TTL } else { CUSTOM_TTL };
    let entry = CachedEntry {
        metadata,
        fetched_at: Instant::now(),
        ttl,
    };
    cache().lock().insert(cache_key, entry);
}

/// Get cached metadata if present and not expired.
pub fn get_cached_metadata(model: &str, base_url: &str) -> Option<ModelMetadata> {
    let cache_key = format!("{}@{}", model, base_url);
    let c = cache().lock();
    c.get(&cache_key).and_then(|e| {
        if e.is_expired() {
            None
        } else {
            Some(e.metadata.clone())
        }
    })
}

/// Lookup context length from hardcoded defaults using substring matching.
pub fn lookup_default_context_length(model: &str) -> Option<usize> {
    let model_lower = model.to_lowercase();
    // Sort by key length descending (longest first) for most specific match
    let mut entries: Vec<_> = DEFAULT_CONTEXT_LENGTHS.iter().collect();
    entries.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

    for (substr, ctx_len) in entries {
        if model_lower.contains(*substr) {
            return Some(*ctx_len);
        }
    }
    None
}

/// Return the next lower probe tier, or None if already at minimum.
pub fn get_next_probe_tier(current_length: usize) -> Option<usize> {
    CONTEXT_PROBE_TIERS.iter().find(|&&tier| tier < current_length).copied()
}

/// Check if a model requires the Responses API (OpenAI GPT-5.x family).
pub fn model_requires_responses_api(model: &str) -> bool {
    let model_lower = model.to_lowercase();
    let bare = model_lower
        .split('/')
        .next_back()
        .unwrap_or(&model_lower);
    bare.starts_with("gpt-5")
}

/// Add model aliases to a cache: both full ID and bare ID (after stripping provider prefix).
pub fn add_model_aliases(
    cache: &mut HashMap<String, serde_json::Value>,
    model_id: &str,
    entry: serde_json::Value,
) {
    cache.insert(model_id.to_string(), entry.clone());
    if model_id.contains('/') {
        let bare = model_id.split_once('/').map(|x| x.1).unwrap_or(model_id);
        cache.entry(bare.to_string()).or_insert_with(|| entry.clone());
    }
}

// =========================================================================
// Full context length resolution (mirrors Python get_model_context_length)
// =========================================================================

/// Get the context length for a model using the full resolution chain.
///
/// This is the synchronous version. It checks:
/// 0. Explicit config override
/// 1. Persistent cache (previously discovered via probing)
/// 2. Endpoint metadata (for custom endpoints)
/// 3. In-memory OpenRouter cache
/// 4. Hardcoded defaults (fuzzy match, longest-first)
/// 5. Default fallback (128K)
///
/// For the async version with full models.dev lookup, use
/// `get_model_context_length_async`.
pub fn get_model_context_length(
    model: &str,
    base_url: &str,
    config_context_length: Option<usize>,
    _provider: &str,
    endpoint_metadata: Option<&HashMap<String, serde_json::Value>>,
) -> usize {
    // 0. Explicit config override
    if let Some(length) = config_context_length {
        if length > 0 {
            return length;
        }
    }

    // Normalize: strip provider prefix
    let bare_model = strip_provider_prefix(model);

    // 1. Persistent cache
    if !base_url.is_empty() {
        if let Some(cached) = get_cached_context_length_persistent(bare_model, base_url) {
            return cached;
        }
    }

    // 2. Endpoint metadata (for custom endpoints)
    if is_custom_endpoint(base_url) && !is_known_provider_base_url(base_url) {
        if let Some(meta) = endpoint_metadata {
            let mut matched: Option<&serde_json::Value> = meta.get(bare_model);

            // Single-model server
            if matched.is_none() && meta.len() == 1 {
                matched = meta.values().next();
            }

            // Fuzzy match: substring in either direction
            if matched.is_none() {
                for (key, entry) in meta {
                    if bare_model.contains(key) || key.contains(bare_model) {
                        matched = Some(entry);
                        break;
                    }
                }
            }

            if let Some(entry) = matched {
                if let Some(ctx) = entry.get("context_length").and_then(|v| v.as_u64()) {
                    return ctx as usize;
                }
                if let Some(ctx) = entry.get("context_window").and_then(|v| v.as_u64()) {
                    return ctx as usize;
                }
            }
        }
    }

    // 3. In-memory OpenRouter cache
    if let Some(metadata) = get_cached_metadata(bare_model, base_url) {
        if let Some(ctx) = metadata.context_length {
            return ctx;
        }
    }

    // 4. Hardcoded defaults
    if let Some(ctx) = lookup_default_context_length(bare_model) {
        return ctx;
    }

    // 5. Default fallback
    DEFAULT_FALLBACK_CONTEXT
}

/// Async version of `get_model_context_length` with full models.dev lookup.
///
/// Resolution order:
/// 0. Explicit config override
/// 1. Persistent cache
/// 2. Endpoint metadata
/// 3. Provider-aware lookup (models.dev)
/// 4. In-memory OpenRouter cache
/// 5. Hardcoded defaults
/// 6. Default fallback (128K)
pub async fn get_model_context_length_async(
    model: &str,
    base_url: &str,
    config_context_length: Option<usize>,
    provider: &str,
    endpoint_metadata: Option<&HashMap<String, serde_json::Value>>,
) -> usize {
    // 0. Explicit config override
    if let Some(length) = config_context_length {
        if length > 0 {
            return length;
        }
    }

    // Normalize: strip provider prefix
    let bare_model = strip_provider_prefix(model);

    // 1. Persistent cache
    if !base_url.is_empty() {
        if let Some(cached) = get_cached_context_length_persistent(bare_model, base_url) {
            return cached;
        }
    }

    // 2. Endpoint metadata (for custom endpoints)
    if is_custom_endpoint(base_url) && !is_known_provider_base_url(base_url) {
        if let Some(meta) = endpoint_metadata {
            let mut matched: Option<&serde_json::Value> = meta.get(bare_model);
            if matched.is_none() && meta.len() == 1 {
                matched = meta.values().next();
            }
            if matched.is_none() {
                for (key, entry) in meta {
                    if bare_model.contains(key) || key.contains(bare_model) {
                        matched = Some(entry);
                        break;
                    }
                }
            }
            if let Some(entry) = matched {
                if let Some(ctx) = entry.get("context_length").and_then(|v| v.as_u64()) {
                    return ctx as usize;
                }
                if let Some(ctx) = entry.get("context_window").and_then(|v| v.as_u64()) {
                    return ctx as usize;
                }
            }
        }
    }

    // 3. Provider-aware lookup via models.dev
    let effective_provider = if provider.is_empty()
        || provider.eq_ignore_ascii_case("openrouter")
        || provider.eq_ignore_ascii_case("custom")
    {
        infer_provider_from_url(base_url).unwrap_or(provider)
    } else {
        provider
    };

    if !effective_provider.is_empty() {
        if let Some(ctx) = crate::models_dev::lookup_context(effective_provider, bare_model).await {
            return ctx as usize;
        }
    }

    // 4. In-memory OpenRouter cache
    if let Some(metadata) = get_cached_metadata(bare_model, base_url) {
        if let Some(ctx) = metadata.context_length {
            return ctx;
        }
    }

    // 5. Hardcoded defaults
    if let Some(ctx) = lookup_default_context_length(bare_model) {
        return ctx;
    }

    // 6. Default fallback
    DEFAULT_FALLBACK_CONTEXT
}

// =========================================================================
// Token estimation (rough character-based heuristic)
// =========================================================================

/// Rough token estimate (~4 chars/token) for pre-flight checks.
///
/// Uses ceiling division so short texts (1-3 chars) never estimate as
/// 0 tokens, which would cause the compressor and pre-flight checks to
/// systematically undercount.
pub fn estimate_tokens_rough(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.len().div_ceil(4)
}

/// Rough token estimate for a message list.
pub fn estimate_messages_tokens_rough(messages: &[serde_json::Value]) -> usize {
    let total_chars: usize = messages.iter().map(|msg| msg.to_string().len()).sum();
    total_chars.div_ceil(4)
}

/// Rough token estimate for a full request (system + messages + tools).
pub fn estimate_request_tokens_rough(
    system_prompt: Option<&str>,
    messages: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
) -> usize {
    let mut total_chars = 0;
    if let Some(sys) = system_prompt {
        total_chars += sys.len();
    }
    total_chars += messages.iter().map(|m| m.to_string().len()).sum::<usize>();
    if let Some(tool_list) = tools {
        total_chars += tool_list.iter().map(|t| t.to_string().len()).sum::<usize>();
    }
    total_chars.div_ceil(4)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Provider prefix stripping
    // =========================================================================

    #[test]
    fn test_strip_provider_prefix_local() {
        assert_eq!(strip_provider_prefix("local:my-model"), "my-model");
    }

    #[test]
    fn test_strip_provider_prefix_ollama_tag_preserved() {
        // Ollama-style tags should NOT be stripped
        assert_eq!(strip_provider_prefix("qwen3.5:27b"), "qwen3.5:27b");
        assert_eq!(strip_provider_prefix("qwen:0.5b"), "qwen:0.5b");
        assert_eq!(strip_provider_prefix("deepseek:latest"), "deepseek:latest");
        assert_eq!(strip_provider_prefix("llama3:8b"), "llama3:8b");
        assert_eq!(strip_provider_prefix("model:q4_0"), "model:q4_0");
        assert_eq!(strip_provider_prefix("model:fp16"), "model:fp16");
        assert_eq!(strip_provider_prefix("model:instruct"), "model:instruct");
        assert_eq!(strip_provider_prefix("model:coder"), "model:coder");
        assert_eq!(strip_provider_prefix("model:vision"), "model:vision");
    }

    #[test]
    fn test_strip_provider_prefix_anthropic() {
        assert_eq!(strip_provider_prefix("anthropic:claude-sonnet-4-6"), "claude-sonnet-4-6");
    }

    #[test]
    fn test_strip_provider_prefix_openai_codex() {
        assert_eq!(strip_provider_prefix("openai-codex:gpt-5"), "gpt-5");
    }

    #[test]
    fn test_strip_provider_prefix_no_match() {
        assert_eq!(strip_provider_prefix("my-custom:model-name"), "my-custom:model-name");
    }

    #[test]
    fn test_strip_provider_prefix_http() {
        assert_eq!(strip_provider_prefix("http://example.com:8080"), "http://example.com:8080");
    }

    // =========================================================================
    // Model slug compatibility
    // =========================================================================

    #[test]
    fn test_compat_model_slug() {
        assert_eq!(compat_model_slug("openrouter/gpt-4o"), "gpt-4o");
        assert_eq!(compat_model_slug("anthropic/claude-opus-4-6"), "claude-opus-4-6");
        assert_eq!(compat_model_slug("gpt-4o"), "gpt-4o");
        assert_eq!(compat_model_slug("claude-sonnet-4-6"), "claude-sonnet-4-6");
    }

    #[test]
    fn test_normalize_model_version() {
        assert_eq!(normalize_model_version("claude-opus-4.6"), "claude-opus-4-6");
        assert_eq!(normalize_model_version("claude-sonnet-4.5"), "claude-sonnet-4-5");
        assert_eq!(normalize_model_version("gpt-4o"), "gpt-4o");
    }

    // =========================================================================
    // URL / provider utilities
    // =========================================================================

    #[test]
    fn test_normalize_base_url() {
        assert_eq!(normalize_base_url("https://api.example.com/v1/"), "https://api.example.com/v1");
        assert_eq!(normalize_base_url("https://api.example.com"), "https://api.example.com");
        assert_eq!(normalize_base_url(""), "");
    }

    #[test]
    fn test_is_openrouter_base_url() {
        assert!(is_openrouter_base_url("https://openrouter.ai/api/v1"));
        assert!(!is_openrouter_base_url("https://api.openai.com/v1"));
    }

    #[test]
    fn test_is_custom_endpoint() {
        assert!(is_custom_endpoint("https://api.example.com/v1"));
        assert!(!is_custom_endpoint(""));
        assert!(!is_custom_endpoint("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn test_infer_provider_from_url() {
        assert_eq!(infer_provider_from_url("https://api.openai.com/v1"), Some("openai"));
        assert_eq!(infer_provider_from_url("https://api.anthropic.com/v1"), Some("anthropic"));
        assert_eq!(infer_provider_from_url("https://api.deepseek.com/v1"), Some("deepseek"));
        assert_eq!(infer_provider_from_url("https://api.x.ai/v1"), Some("xai"));
        assert_eq!(infer_provider_from_url("https://dashscope.aliyuncs.com/v1"), Some("alibaba"));
        assert_eq!(infer_provider_from_url("https://api.moonshot.ai/v1"), Some("kimi-coding"));
        assert_eq!(infer_provider_from_url(""), None);
    }

    #[test]
    fn test_is_known_provider_base_url() {
        assert!(is_known_provider_base_url("https://api.openai.com/v1"));
        assert!(is_known_provider_base_url("https://api.anthropic.com/v1"));
        assert!(!is_known_provider_base_url("https://api.example.com/v1"));
    }

    // =========================================================================
    // Local endpoint detection
    // =========================================================================

    #[test]
    fn test_is_local_endpoint() {
        assert!(is_local_endpoint("http://localhost:8080"));
        assert!(is_local_endpoint("http://127.0.0.1:8000"));
        assert!(is_local_endpoint("http://host.docker.internal:8000"));
        assert!(is_local_endpoint("http://host.containers.internal:8000"));
        assert!(is_local_endpoint("http://gateway.docker.internal:8000"));
        assert!(is_local_endpoint("http://host.lima.internal:8000"));
        assert!(!is_local_endpoint("https://api.openai.com"));
        assert!(!is_local_endpoint("https://api.openrouter.ai"));
        assert!(!is_local_endpoint(""));
    }

    #[test]
    fn test_is_local_endpoint_private_ip() {
        assert!(is_local_endpoint("http://10.0.0.1:8080"));
        assert!(is_local_endpoint("http://172.16.0.1:8080"));
        assert!(is_local_endpoint("http://192.168.1.1:8080"));
        assert!(!is_local_endpoint("http://8.8.8.8:8080"));
    }

    // =========================================================================
    // Payload extraction
    // =========================================================================

    #[test]
    fn test_extract_context_length_from_payload() {
        let payload = serde_json::json!({
            "context_length": 200000,
            "max_tokens": 8192,
        });
        assert_eq!(extract_context_length(&payload), Some(200_000));
    }

    #[test]
    fn test_extract_context_length_nested() {
        let payload = serde_json::json!({
            "model": {
                "context_window": 128000,
            }
        });
        assert_eq!(extract_context_length(&payload), Some(128_000));
    }

    #[test]
    fn test_extract_context_length_max_model_len() {
        let payload = serde_json::json!({
            "max_model_len": 32768,
        });
        assert_eq!(extract_context_length(&payload), Some(32_768));
    }

    #[test]
    fn test_extract_context_length_llamacpp() {
        let payload = serde_json::json!({
            "default_generation_settings": {
                "n_ctx": 16384,
            }
        });
        assert_eq!(extract_context_length(&payload), Some(16_384));
    }

    #[test]
    fn test_extract_context_length_none() {
        let payload = serde_json::json!({"name": "test-model"});
        assert_eq!(extract_context_length(&payload), None);
    }

    #[test]
    fn test_extract_max_completion_tokens() {
        let payload = serde_json::json!({
            "max_completion_tokens": 8192,
        });
        assert_eq!(extract_max_completion_tokens(&payload), Some(8_192));
    }

    #[test]
    fn test_extract_max_completion_tokens_max_tokens() {
        let payload = serde_json::json!({
            "max_tokens": 4096,
        });
        assert_eq!(extract_max_completion_tokens(&payload), Some(4_096));
    }

    #[test]
    fn test_extract_pricing_from_payload_openrouter() {
        let payload = serde_json::json!({
            "pricing": {
                "input_cost_per_token": 0.0000025,
                "output_cost_per_token": 0.000010,
                "cache_read_cost_per_token": 0.0000003,
                "cache_write_cost_per_token": 0.00000375,
            }
        });
        let pricing = extract_pricing_from_payload(&payload).unwrap();
        assert!((pricing.prompt_per_million - 2.5).abs() < 0.01);
        assert!((pricing.completion_per_million - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_extract_pricing_from_payload_none() {
        let payload = serde_json::json!({"name": "test"});
        assert!(extract_pricing_from_payload(&payload).is_none());
    }

    #[test]
    fn test_parse_context_length_from_response() {
        let r = serde_json::json!({"context_length": 200000, "max_completion_tokens": 8192});
        assert_eq!(parse_context_length_from_response(&r), Some(200_000));
    }

    #[test]
    fn test_parse_context_length_from_data_array() {
        let r = serde_json::json!({
            "data": [
                {"id": "gpt-4o", "max_model_len": 128000}
            ]
        });
        assert_eq!(parse_context_length_from_response(&r), Some(128_000));
    }

    // =========================================================================
    // Model ID matching
    // =========================================================================

    #[test]
    fn test_model_id_matches_exact() {
        assert!(model_id_matches("my-model", "my-model"));
    }

    #[test]
    fn test_model_id_matches_slug() {
        assert!(model_id_matches("publisher/my-model", "my-model"));
        assert!(model_id_matches("org/publisher/my-model", "my-model"));
    }

    #[test]
    fn test_model_id_matches_no_match() {
        assert!(!model_id_matches("model-a", "model-b"));
        assert!(!model_id_matches("publisher/model-a", "model-b"));
    }

    // =========================================================================
    // Error parsing
    // =========================================================================

    #[test]
    fn test_parse_context_limit_from_error_max_length() {
        assert_eq!(
            parse_context_limit_from_error("maximum context length is 32768 tokens"),
            Some(32_768)
        );
    }

    #[test]
    fn test_parse_context_limit_from_error_exceeded() {
        assert_eq!(
            parse_context_limit_from_error("context_length_exceeded: 131072"),
            Some(131_072)
        );
    }

    #[test]
    fn test_parse_context_limit_from_error_size() {
        assert_eq!(
            parse_context_limit_from_error("Maximum context size 32768 exceeded"),
            Some(32_768)
        );
    }

    #[test]
    fn test_parse_context_limit_from_error_model_max() {
        assert_eq!(
            parse_context_limit_from_error("model's max context length is 65536"),
            Some(65_536)
        );
    }

    #[test]
    fn test_parse_context_limit_from_error_no_match() {
        assert_eq!(
            parse_context_limit_from_error("rate limit exceeded"),
            None
        );
    }

    #[test]
    fn test_parse_available_output_tokens() {
        assert_eq!(
            parse_available_output_tokens_from_error(
                "max_tokens: 32768 > context_window: 200000 - input_tokens: 190000 = available_tokens: 10000"
            ),
            Some(10_000)
        );
    }

    #[test]
    fn test_parse_available_output_tokens_not_matching() {
        assert_eq!(
            parse_available_output_tokens_from_error("maximum context length is 32768"),
            None
        );
    }

    #[test]
    fn test_parse_available_output_tokens_space_format() {
        assert_eq!(
            parse_available_output_tokens_from_error(
                "max_tokens: 8192, available tokens: 4096"
            ),
            Some(4_096)
        );
    }

    // =========================================================================
    // Default context length lookup
    // =========================================================================

    #[test]
    fn test_lookup_claude_4_6() {
        assert_eq!(lookup_default_context_length("claude-opus-4-6"), Some(1_000_000));
        assert_eq!(lookup_default_context_length("claude-sonnet-4-6"), Some(1_000_000));
    }

    #[test]
    fn test_lookup_claude_4_6_dotted() {
        assert_eq!(lookup_default_context_length("claude-opus-4.6"), Some(1_000_000));
        assert_eq!(lookup_default_context_length("claude-sonnet-4.6"), Some(1_000_000));
    }

    #[test]
    fn test_lookup_claude() {
        assert_eq!(lookup_default_context_length("claude-3-5-sonnet-20241022"), Some(200_000));
        assert_eq!(lookup_default_context_length("claude-3-sonnet-20240229"), Some(200_000));
        assert_eq!(lookup_default_context_length("claude-3-opus-20240229"), Some(200_000));
        assert_eq!(lookup_default_context_length("claude-3-haiku-20240307"), Some(200_000));
    }

    #[test]
    fn test_lookup_gpt5_family() {
        assert_eq!(lookup_default_context_length("gpt-5"), Some(400_000));
        assert_eq!(lookup_default_context_length("gpt-5-mini"), Some(400_000));
        assert_eq!(lookup_default_context_length("gpt-5-nano"), Some(400_000));
        assert_eq!(lookup_default_context_length("gpt-5.4"), Some(1_050_000));
        assert_eq!(lookup_default_context_length("gpt-5.4-mini"), Some(400_000));
        assert_eq!(lookup_default_context_length("gpt-5.4-nano"), Some(400_000));
        assert_eq!(lookup_default_context_length("gpt-5.3-codex-spark"), Some(128_000));
        assert_eq!(lookup_default_context_length("gpt-5.1-chat"), Some(128_000));
    }

    #[test]
    fn test_lookup_gpt4o() {
        assert_eq!(lookup_default_context_length("gpt-4o-mini"), Some(128_000));
        assert_eq!(lookup_default_context_length("gpt-4o"), Some(128_000));
        assert_eq!(lookup_default_context_length("gpt-4-turbo"), Some(128_000));
        assert_eq!(lookup_default_context_length("gpt-4.1"), Some(1_047_576));
    }

    #[test]
    fn test_lookup_gpt35() {
        assert_eq!(lookup_default_context_length("gpt-3.5-turbo"), Some(16_385));
    }

    #[test]
    fn test_lookup_gemini() {
        assert_eq!(lookup_default_context_length("gemini-1.5-pro"), Some(1_048_576));
        assert_eq!(lookup_default_context_length("gemini-2.0-flash"), Some(1_048_576));
    }

    #[test]
    fn test_lookup_gemma() {
        assert_eq!(lookup_default_context_length("gemma-4-31b-it"), Some(256_000));
        assert_eq!(lookup_default_context_length("gemma-4-26b"), Some(256_000));
        assert_eq!(lookup_default_context_length("gemma-3-27b"), Some(131_072));
        assert_eq!(lookup_default_context_length("gemma-2-9b"), Some(8_192));
    }

    #[test]
    fn test_lookup_deepseek() {
        assert_eq!(lookup_default_context_length("deepseek-chat"), Some(128_000));
        assert_eq!(lookup_default_context_length("deepseek-coder"), Some(128_000));
        // "deepseek-ai/deepseek-v3.2" contains "deepseek" as substring
        // so it matches the "deepseek" → 128000 entry
        assert_eq!(lookup_default_context_length("deepseek-ai/DeepSeek-V3.2"), Some(128_000));
    }

    #[test]
    fn test_lookup_llama() {
        assert_eq!(lookup_default_context_length("llama-3-70b"), Some(128_000));
        // "llama3.1-405b" contains "llama3" → 128000 (not "llama" → 131072)
        assert_eq!(lookup_default_context_length("llama3.1-405b"), Some(128_000));
    }

    #[test]
    fn test_lookup_mistral() {
        assert_eq!(lookup_default_context_length("mistral-large-2"), Some(32_768));
        assert_eq!(lookup_default_context_length("mistral-small"), Some(32_768));
    }

    #[test]
    fn test_lookup_qwen() {
        assert_eq!(lookup_default_context_length("qwen3-coder-plus"), Some(1_000_000));
        assert_eq!(lookup_default_context_length("qwen3-coder-235b"), Some(262_144));
        assert_eq!(lookup_default_context_length("qwen-max"), Some(131_072));
    }

    #[test]
    fn test_lookup_minimax() {
        assert_eq!(lookup_default_context_length("minimax-m2.5"), Some(204_800));
    }

    #[test]
    fn test_lookup_glm() {
        assert_eq!(lookup_default_context_length("glm-4-plus"), Some(202_752));
        assert_eq!(lookup_default_context_length("zai-org/GLM-5"), Some(202_752));
    }

    #[test]
    fn test_lookup_grok() {
        assert_eq!(lookup_default_context_length("grok-code-fast-1"), Some(256_000));
        assert_eq!(lookup_default_context_length("grok-4-1-fast-reasoning"), Some(2_000_000));
        assert_eq!(lookup_default_context_length("grok-4-fast"), Some(2_000_000));
        assert_eq!(lookup_default_context_length("grok-4.20-0309-reasoning"), Some(2_000_000));
        assert_eq!(lookup_default_context_length("grok-4"), Some(256_000));
        assert_eq!(lookup_default_context_length("grok-3-mini"), Some(131_072));
        assert_eq!(lookup_default_context_length("grok-2"), Some(131_072));
        assert_eq!(lookup_default_context_length("grok-beta"), Some(131_072));
        assert_eq!(lookup_default_context_length("grok-2-vision"), Some(8_192));
    }

    #[test]
    fn test_lookup_kimi() {
        assert_eq!(lookup_default_context_length("kimi-k2.5"), Some(262_144));
        assert_eq!(lookup_default_context_length("moonshotai/Kimi-K2-Thinking"), Some(262_144));
    }

    #[test]
    fn test_lookup_mimo() {
        assert_eq!(lookup_default_context_length("mimo-v2-pro"), Some(1_000_000));
        assert_eq!(lookup_default_context_length("mimo-v2-omni"), Some(256_000));
        assert_eq!(lookup_default_context_length("mimo-v2-flash"), Some(256_000));
        assert_eq!(lookup_default_context_length("XiaomiMiMo/MiMo-V2-Flash"), Some(256_000));
    }

    #[test]
    fn test_lookup_trinity() {
        assert_eq!(lookup_default_context_length("trinity-r1"), Some(262_144));
    }

    #[test]
    fn test_lookup_unknown() {
        assert_eq!(lookup_default_context_length("some-random-model"), None);
    }

    // =========================================================================
    // get_context_length with cache
    // =========================================================================

    #[test]
    fn test_get_context_length_defaults() {
        assert_eq!(get_context_length("gpt-4o", ""), Some(128_000));
    }

    #[test]
    fn test_cache_roundtrip() {
        let m = ModelMetadata {
            model: "test-model".to_string(),
            context_length: Some(4096),
            max_completion_tokens: None,
            pricing: None,
            last_fetched: None,
        };
        cache_metadata("test-model", "https://test.com", m.clone(), false);
        let c = get_cached_metadata("test-model", "https://test.com").unwrap();
        assert_eq!(c.context_length, Some(4096));
    }

    #[test]
    fn test_minimum_context_length_constant() {
        assert_eq!(MINIMUM_CONTEXT_LENGTH, 64_000);
    }

    #[test]
    fn test_default_fallback_context() {
        assert_eq!(DEFAULT_FALLBACK_CONTEXT, 128_000);
    }

    // =========================================================================
    // Probe tiers
    // =========================================================================

    #[test]
    fn test_probe_tiers() {
        assert_eq!(CONTEXT_PROBE_TIERS, &[128_000, 64_000, 32_000, 16_000, 8_000]);
    }

    #[test]
    fn test_get_next_probe_tier() {
        assert_eq!(get_next_probe_tier(128_000), Some(64_000));
        assert_eq!(get_next_probe_tier(64_000), Some(32_000));
        assert_eq!(get_next_probe_tier(32_000), Some(16_000));
        assert_eq!(get_next_probe_tier(16_000), Some(8_000));
        assert_eq!(get_next_probe_tier(8_000), None);
        assert_eq!(get_next_probe_tier(5_000), None);
        assert_eq!(get_next_probe_tier(200_000), Some(128_000));
    }

    // =========================================================================
    // Responses API
    // =========================================================================

    #[test]
    fn test_model_requires_responses_api() {
        assert!(model_requires_responses_api("gpt-5"));
        assert!(model_requires_responses_api("gpt-5-mini"));
        assert!(model_requires_responses_api("gpt-5-nano"));
        assert!(model_requires_responses_api("openai/gpt-5"));
        assert!(!model_requires_responses_api("gpt-4o"));
        assert!(!model_requires_responses_api("gpt-4-turbo"));
        assert!(!model_requires_responses_api("claude-opus-4-6"));
    }

    // =========================================================================
    // Model aliases
    // =========================================================================

    #[test]
    fn test_add_model_aliases() {
        let mut cache = HashMap::new();
        let entry = serde_json::json!({"context_length": 200000});
        add_model_aliases(&mut cache, "anthropic/claude-sonnet-4-6", entry.clone());
        assert!(cache.contains_key("anthropic/claude-sonnet-4-6"));
        assert!(cache.contains_key("claude-sonnet-4-6"));
        assert_eq!(cache["claude-sonnet-4-6"]["context_length"], 200_000);
    }

    #[test]
    fn test_add_model_aliases_no_slash() {
        let mut cache = HashMap::new();
        let entry = serde_json::json!({"context_length": 128000});
        add_model_aliases(&mut cache, "gpt-4o", entry.clone());
        assert!(cache.contains_key("gpt-4o"));
        assert_eq!(cache.len(), 1);
    }

    // =========================================================================
    // YAML cache parsing
    // =========================================================================

    #[test]
    fn test_parse_yaml_simple() {
        let content = "context_lengths:\n  model1@https://api.com: 128000\n  model2@https://api.com: 200000\n";
        let cache = parse_yaml_simple(content).unwrap();
        assert_eq!(cache.get("model1@https://api.com"), Some(&128_000));
        assert_eq!(cache.get("model2@https://api.com"), Some(&200_000));
    }

    #[test]
    fn test_parse_yaml_simple_quoted_keys() {
        let content = "context_lengths:\n  \"gpt-4o@https://api.openai.com\": 128000\n";
        let cache = parse_yaml_simple(content).unwrap();
        assert_eq!(cache.get("gpt-4o@https://api.openai.com"), Some(&128_000));
    }

    #[test]
    fn test_parse_yaml_empty() {
        let content = "context_lengths:\n";
        let cache = parse_yaml_simple(content).unwrap();
        assert!(cache.is_empty());
    }

    // =========================================================================
    // Token estimation
    // =========================================================================

    #[test]
    fn test_estimate_tokens_rough() {
        assert_eq!(estimate_tokens_rough("hello world"), 3);  // 11 chars -> (11+3)/4 = 3
    }

    #[test]
    fn test_estimate_tokens_rough_empty() {
        assert_eq!(estimate_tokens_rough(""), 0);
    }

    #[test]
    fn test_estimate_tokens_rough_ceiling() {
        // Ceiling division: (len + 3) / 4
        assert_eq!(estimate_tokens_rough("a"), 1);   // (1+3)/4 = 1
        assert_eq!(estimate_tokens_rough("abcd"), 1); // (4+3)/4 = 1
        assert_eq!(estimate_tokens_rough("abcde"), 2); // (5+3)/4 = 2
    }

    #[test]
    fn test_estimate_messages_tokens() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi there"}),
        ];
        let tokens = estimate_messages_tokens_rough(&messages);
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_request_tokens() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let total = estimate_request_tokens_rough(
            Some("You are a helpful assistant"),
            &messages,
            None,
        );
        assert!(total > 0);
    }

    #[test]
    fn test_estimate_request_with_tools() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let tools = vec![
            serde_json::json!({"name": "weather", "parameters": {"type": "object"}}),
        ];
        let total = estimate_request_tokens_rough(
            Some("You are a helpful assistant"),
            &messages,
            Some(&tools),
        );
        assert!(total > 0);
    }

    // =========================================================================
    // Full resolution chain
    // =========================================================================

    #[test]
    fn test_get_model_context_length_config_override() {
        let result = get_model_context_length("unknown-model", "", Some(50_000), "", None);
        assert_eq!(result, 50_000);
    }

    #[test]
    fn test_get_model_context_length_hardcoded_default() {
        let result = get_model_context_length("gpt-4o", "", None, "", None);
        assert_eq!(result, 128_000);
    }

    #[test]
    fn test_get_model_context_length_fallback() {
        let result = get_model_context_length("totally-unknown-model", "", None, "", None);
        assert_eq!(result, DEFAULT_FALLBACK_CONTEXT);
    }

    #[test]
    fn test_get_model_context_length_provider_strip() {
        // "local:gpt-4o" should strip to "gpt-4o" and match hardcoded default
        let result = get_model_context_length("local:gpt-4o", "", None, "", None);
        assert_eq!(result, 128_000);
    }

    // =========================================================================
    // Coerce reasonable int
    // =========================================================================

    #[test]
    fn test_coerce_reasonable_int_number() {
        let v = serde_json::json!(128000);
        assert_eq!(coerce_reasonable_int(&v, 1024, 10_000_000), Some(128_000));
    }

    #[test]
    fn test_coerce_reasonable_int_string() {
        let v = serde_json::json!("128,000");
        assert_eq!(coerce_reasonable_int(&v, 1024, 10_000_000), Some(128_000));
    }

    #[test]
    fn test_coerce_reasonable_int_too_small() {
        let v = serde_json::json!(512);
        assert_eq!(coerce_reasonable_int(&v, 1024, 10_000_000), None);
    }

    #[test]
    fn test_coerce_reasonable_int_too_large() {
        let v = serde_json::json!(50_000_000);
        assert_eq!(coerce_reasonable_int(&v, 1024, 10_000_000), None);
    }

    #[test]
    fn test_coerce_reasonable_int_bool() {
        let v = serde_json::json!(true);
        assert_eq!(coerce_reasonable_int(&v, 1024, 10_000_000), None);
    }

    // =========================================================================
    // DEFAULT_CONTEXT_LENGTHS coverage verification
    // =========================================================================

    #[test]
    fn test_all_default_context_lengths_positive() {
        for (key, value) in DEFAULT_CONTEXT_LENGTHS {
            assert!(*value >= 1024, "context length for '{}' is too small: {}", key, value);
            assert!(*value <= 10_000_000, "context length for '{}' is too large: {}", key, value);
        }
    }

    #[test]
    fn test_default_context_lengths_no_empty_keys() {
        for (key, _) in DEFAULT_CONTEXT_LENGTHS {
            assert!(!key.is_empty(), "empty key in DEFAULT_CONTEXT_LENGTHS");
        }
    }

    #[test]
    fn test_claude_4_6_no_collision_with_4_5() {
        // "claude-sonnet-4-5" matches "claude" catch-all → 200000
        // (not "claude-sonnet-4-6" because we only check key-in-model)
        assert_eq!(lookup_default_context_length("claude-sonnet-4-5"), Some(200_000));
        assert_eq!(lookup_default_context_length("claude-sonnet-4-6"), Some(1_000_000));
    }

    // =========================================================================
    // iter_nested_dicts
    // =========================================================================

    #[test]
    fn test_iter_nested_dicts_flat() {
        let v = serde_json::json!({"a": 1, "b": 2});
        let mut maps = Vec::new();
        iter_nested_dicts(&v, &mut maps);
        assert_eq!(maps.len(), 1);
    }

    #[test]
    fn test_iter_nested_dicts_nested() {
        let v = serde_json::json!({"a": 1, "nested": {"b": 2}});
        let mut maps = Vec::new();
        iter_nested_dicts(&v, &mut maps);
        assert_eq!(maps.len(), 2);
    }

    #[test]
    fn test_iter_nested_dicts_array() {
        let v = serde_json::json!({"data": [{"x": 1}, {"y": 2}]});
        let mut maps = Vec::new();
        iter_nested_dicts(&v, &mut maps);
        assert_eq!(maps.len(), 3);
    }
}
