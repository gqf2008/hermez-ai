//! Runtime provider resolution.
//!
//! Mirrors Python `hermez_cli/runtime_provider.py`.
//! Determines which provider, API key, base URL, and api_mode to use
//! at chat time by evaluating config, credential pools, auth stores,
//! OAuth state, and environment variables in priority order.

use std::collections::HashMap;

use serde_json::Value;

use crate::credential_pool::{load_from_env, CredentialPool};
use crate::provider::{default_base_url, parse_provider, ProviderType};

/// Resolved runtime credentials for a single provider.
#[derive(Debug, Clone, Default)]
pub struct RuntimeProvider {
    /// Canonical provider id (e.g. "openrouter", "nous", "anthropic").
    pub provider: String,
    /// API mode: "chat_completions", "codex_responses", "anthropic_messages", "bedrock_converse".
    pub api_mode: String,
    /// Base URL for the API endpoint.
    pub base_url: String,
    /// API key or access token.
    pub api_key: String,
    /// Where these credentials came from.
    pub source: String,
    /// The provider originally requested by the user.
    pub requested_provider: String,
    /// Optional: credential pool if one was used.
    pub credential_pool: Option<CredentialPool>,
    /// Optional: model override from custom provider config.
    pub model: Option<String>,
    /// Optional: token expiry.
    pub expires_at: Option<String>,
    /// Optional: last refresh timestamp.
    pub last_refresh: Option<String>,
    /// Optional: Bedrock region.
    pub region: Option<String>,
    /// Optional: Bedrock guardrail config.
    pub guardrail_config: Option<Value>,
    /// Optional: command for external-process providers.
    pub command: Option<String>,
    /// Optional: args for external-process providers.
    pub args: Vec<String>,
}

const VALID_API_MODES: &[&str] = &[
    "chat_completions",
    "codex_responses",
    "anthropic_messages",
    "bedrock_converse",
];

/// Main entry: resolve runtime provider credentials for agent execution.
///
/// Mirrors Python `resolve_runtime_provider()` (runtime_provider.py:649).
pub fn resolve_runtime_provider(
    requested: Option<&str>,
    explicit_api_key: Option<&str>,
    explicit_base_url: Option<&str>,
    model_cfg: Option<&HashMap<String, Value>>,
) -> Option<RuntimeProvider> {
    let requested_provider = resolve_requested_provider(requested, model_cfg);

    // 1. Named custom provider resolution
    if let Some(runtime) =
        _resolve_named_custom_runtime(&requested_provider, explicit_api_key, explicit_base_url, model_cfg)
    {
        return Some(runtime);
    }

    // 2. Try to resolve from explicit provider
    let provider = resolve_provider(&requested_provider, explicit_api_key, explicit_base_url);

    let model_cfg = model_cfg.cloned().unwrap_or_default();

    // 3. Explicit runtime overrides
    if let Some(runtime) = _resolve_explicit_runtime(
        &provider,
        &requested_provider,
        &model_cfg,
        explicit_api_key,
        explicit_base_url,
    ) {
        return Some(runtime);
    }

    // 4. Credential pool resolution
    let should_use_pool = provider != "openrouter";
    if should_use_pool {
        if let Some(runtime) =
            _try_resolve_from_pool(&provider, &requested_provider, &model_cfg, explicit_api_key, explicit_base_url)
        {
            return Some(runtime);
        }
    }

    // 5. OAuth provider resolution (Nous, Codex, Qwen)
    if let Some(runtime) = _resolve_oauth_provider(&provider, &requested_provider, &model_cfg) {
        return Some(runtime);
    }

    // 6. Anthropic native
    if provider == "anthropic" {
        return _resolve_anthropic_runtime(&requested_provider, &model_cfg);
    }

    // 7. API-key providers (zai, kimi, minimax, copilot, etc.)
    if let Some(runtime) = _resolve_api_key_provider(&provider, &requested_provider, &model_cfg) {
        return Some(runtime);
    }

    // 8. Default to OpenRouter
    let runtime = _resolve_openrouter_runtime(&requested_provider, explicit_api_key, explicit_base_url, &model_cfg);
    Some(runtime)
}

/// Resolve the provider request from explicit arg, config, then env.
///
/// Mirrors Python `resolve_requested_provider()` (runtime_provider.py:209).
fn resolve_requested_provider(
    requested: Option<&str>,
    model_cfg: Option<&HashMap<String, Value>>,
) -> String {
    if let Some(r) = requested {
        let trimmed = r.trim();
        if !trimmed.is_empty() {
            return trimmed.to_lowercase();
        }
    }

    if let Some(cfg) = model_cfg {
        if let Some(provider) = cfg.get("provider").and_then(|v| v.as_str()) {
            let trimmed = provider.trim();
            if !trimmed.is_empty() {
                return trimmed.to_lowercase();
            }
        }
    }

    if let Ok(env_provider) = std::env::var("HERMEZ_INFERENCE_PROVIDER") {
        let trimmed = env_provider.trim();
        if !trimmed.is_empty() {
            return trimmed.to_lowercase();
        }
    }

    "auto".to_string()
}

/// Simple provider resolution: maps "auto" and aliases to canonical names.
///
/// Mirrors Python `resolve_provider()` in `auth.py`.
fn resolve_provider(
    requested_provider: &str,
    explicit_api_key: Option<&str>,
    explicit_base_url: Option<&str>,
) -> String {
    let norm = requested_provider.trim().to_lowercase();

    // Explicit overrides force openrouter unless clearly another provider
    if explicit_api_key.is_some() || explicit_base_url.is_some() {
        if norm == "auto" || norm.is_empty() {
            return "openrouter".to_string();
        }
        return norm;
    }

    if norm == "auto" || norm.is_empty() {
        // Check env vars for known providers
        if std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("ANTHROPIC_TOKEN").is_ok() {
            return "anthropic".to_string();
        }
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return "openai".to_string();
        }
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            return "openrouter".to_string();
        }
        if std::env::var("NOUS_API_KEY").is_ok() {
            return "nous".to_string();
        }
        if std::env::var("GEMINI_API_KEY").is_ok() {
            return "gemini".to_string();
        }
        if std::env::var("OLLAMA_HOST").is_ok()
            || std::env::var("OLLAMA_BASE_URL").is_ok()
        {
            return "ollama".to_string();
        }
        if std::env::var("GEMINI_CLI_API_KEY").is_ok()
            || std::env::var("GOOGLE_API_KEY").is_ok()
        {
            return "google-gemini-cli".to_string();
        }
        return "openrouter".to_string();
    }

    // Resolve alias
    let canonical = crate::provider::resolve_provider_alias(&norm);
    canonical.to_string()
}

/// Resolve runtime from a credential pool entry.
///
/// Mirrors Python `_resolve_runtime_from_pool_entry()` (runtime_provider.py:139).
fn _resolve_runtime_from_pool_entry(
    provider: &str,
    pool: &CredentialPool,
    requested_provider: &str,
    _model_cfg: &HashMap<String, Value>,
) -> Option<RuntimeProvider> {
    let entry = pool.select()?;
    let api_key = entry.runtime_api_key().to_string();
    if api_key.is_empty() {
        return None;
    }

    let base_url = entry
        .runtime_base_url()
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_base_url(parse_provider(provider)).unwrap_or("").to_string());

    let api_mode = match provider {
        "openai-codex" => "codex_responses",
        "qwen-oauth" => "chat_completions",
        "anthropic" => "anthropic_messages",
        "nous" => "chat_completions",
        _ => "chat_completions",
    }
    .to_string();

    Some(RuntimeProvider {
        provider: provider.to_string(),
        api_mode,
        base_url,
        api_key,
        source: format!("pool:{}", provider),
        requested_provider: requested_provider.to_string(),
        credential_pool: Some(pool.clone()),
        ..Default::default()
    })
}

/// Try to resolve credentials from the credential pool for a provider.
fn _try_resolve_from_pool(
    provider: &str,
    requested_provider: &str,
    model_cfg: &HashMap<String, Value>,
    _explicit_api_key: Option<&str>,
    _explicit_base_url: Option<&str>,
) -> Option<RuntimeProvider> {
    let pool = load_from_env(provider)?;
    if !pool.has_credentials() {
        return None;
    }
    _resolve_runtime_from_pool_entry(provider, &pool, requested_provider, model_cfg)
}

/// Resolve a named custom provider from config.
///
/// Mirrors Python `_resolve_named_custom_runtime()` (runtime_provider.py:368).
fn _resolve_named_custom_runtime(
    requested_provider: &str,
    explicit_api_key: Option<&str>,
    explicit_base_url: Option<&str>,
    _model_cfg: Option<&HashMap<String, Value>>,
) -> Option<RuntimeProvider> {
    let norm = normalize_custom_provider_name(requested_provider);
    if norm.is_empty() || norm == "custom" || norm == "auto" {
        return None;
    }

    // If it's a known built-in provider, don't treat as custom
    let parsed = parse_provider(&norm);
    if !matches!(parsed, ProviderType::Unknown | ProviderType::Custom) {
        return None;
    }

    // Load config and look for custom_providers
    let config = hermez_core::HermezConfig::load().ok()?;

    // Check providers: dict (new-style)
    for (ep_name, entry) in &config.providers {
            let name_norm = normalize_custom_provider_name(ep_name);
            let display_name = entry.name.as_deref().unwrap_or(ep_name);
            let display_norm = normalize_custom_provider_name(display_name);

            let matches = norm == *ep_name
                || norm == name_norm
                || norm == format!("custom:{}", name_norm)
                || norm == *display_name
                || norm == display_norm
                || norm == format!("custom:{}", display_norm);

            if matches {
                let base_url = entry
                    .api
                    .as_deref()
                    .or(entry.url.as_deref())
                    .or(entry.base_url.as_deref())?;

                let resolved_api_key = if let Some(ref key_env) = entry.key_env {
                    std::env::var(key_env).unwrap_or_default()
                } else {
                    entry.api_key.clone().unwrap_or_default()
                };

                let base_url = explicit_base_url
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| base_url.to_string());

                let api_key = explicit_api_key
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(resolved_api_key);

                return Some(RuntimeProvider {
                    provider: "custom".to_string(),
                    api_mode: _detect_api_mode_for_url(&base_url)
                        .unwrap_or_else(|| "chat_completions".to_string()),
                    base_url: base_url.trim().to_string(),
                    api_key: if api_key.is_empty() {
                        "no-key-required".to_string()
                    } else {
                        api_key
                    },
                    source: format!("custom_provider:{}", display_name),
                    requested_provider: requested_provider.to_string(),
                    model: entry.default_model.clone(),
                    ..Default::default()
                });
            }
        }

    None
}

/// Resolve explicit provider runtime.
///
/// Mirrors Python `_resolve_explicit_runtime()` (runtime_provider.py:517).
fn _resolve_explicit_runtime(
    provider: &str,
    requested_provider: &str,
    model_cfg: &HashMap<String, Value>,
    explicit_api_key: Option<&str>,
    explicit_base_url: Option<&str>,
) -> Option<RuntimeProvider> {
    let has_explicit = explicit_api_key.is_some() || explicit_base_url.is_some();
    if !has_explicit {
        return None;
    }

    let api_key = explicit_api_key.unwrap_or("").to_string();
    let base_url = explicit_base_url.unwrap_or("").trim().trim_end_matches('/').to_string();

    match provider {
        "anthropic" => {
            let cfg_provider = model_cfg
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let cfg_base_url = if cfg_provider == "anthropic" {
                model_cfg
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches('/')
                    .to_string()
            } else {
                String::new()
            };
            let base_url = if !base_url.is_empty() {
                base_url
            } else if !cfg_base_url.is_empty() {
                cfg_base_url
            } else {
                "https://api.anthropic.com".to_string()
            };
            Some(RuntimeProvider {
                provider: "anthropic".to_string(),
                api_mode: "anthropic_messages".to_string(),
                base_url,
                api_key,
                source: "explicit".to_string(),
                requested_provider: requested_provider.to_string(),
                ..Default::default()
            })
        }
        "openai-codex" => {
            let base_url = if !base_url.is_empty() {
                base_url
            } else {
                "https://api.openai.com/v1".to_string()
            };
            Some(RuntimeProvider {
                provider: "openai-codex".to_string(),
                api_mode: "codex_responses".to_string(),
                base_url,
                api_key,
                source: "explicit".to_string(),
                requested_provider: requested_provider.to_string(),
                ..Default::default()
            })
        }
        "nous" => {
            let base_url = if !base_url.is_empty() {
                base_url
            } else {
                "https://api.nousresearch.com/v1".to_string()
            };
            Some(RuntimeProvider {
                provider: "nous".to_string(),
                api_mode: "chat_completions".to_string(),
                base_url,
                api_key,
                source: "explicit".to_string(),
                requested_provider: requested_provider.to_string(),
                ..Default::default()
            })
        }
        _ => {
            // Generic API-key provider with explicit override
            let base_url = if !base_url.is_empty() {
                base_url
            } else {
                default_base_url(parse_provider(provider))
                    .unwrap_or("")
                    .to_string()
            };
            let api_mode = _parse_api_mode(model_cfg.get("api_mode"))
                .or_else(|| _detect_api_mode_for_url(&base_url))
                .unwrap_or_else(|| "chat_completions".to_string());
            Some(RuntimeProvider {
                provider: provider.to_string(),
                api_mode,
                base_url,
                api_key,
                source: "explicit".to_string(),
                requested_provider: requested_provider.to_string(),
                ..Default::default()
            })
        }
    }
}

/// Resolve OAuth providers (Nous, Codex, Qwen) at runtime.
fn _resolve_oauth_provider(
    provider: &str,
    requested_provider: &str,
    _model_cfg: &HashMap<String, Value>,
) -> Option<RuntimeProvider> {
    match provider {
        "nous" => {
            // Try auth.json for Nous state
            let auth_store = _load_auth_store_json();
            let state = auth_store
                .get("providers")
                .and_then(|p| p.get("nous"))
                .cloned()
                .unwrap_or_default();
            let agent_key = state
                .get("agent_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let base_url = state
                .get("inference_base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://api.nousresearch.com/v1")
                .to_string();
            if !agent_key.is_empty() {
                return Some(RuntimeProvider {
                    provider: "nous".to_string(),
                    api_mode: "chat_completions".to_string(),
                    base_url,
                    api_key: agent_key,
                    source: "auth.json".to_string(),
                    requested_provider: requested_provider.to_string(),
                    ..Default::default()
                });
            }
            None
        }
        "openai-codex" => {
            let auth_store = _load_auth_store_json();
            let active = auth_store.get("active_provider").and_then(|v| v.as_str());
            if active != Some("openai-codex") {
                return None;
            }
            let tokens = auth_store
                .get("providers")
                .and_then(|p| p.get("openai-codex"))
                .and_then(|p| p.get("tokens"));
            let access_token = tokens
                .and_then(|t| t.get("access_token"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !access_token.is_empty() {
                return Some(RuntimeProvider {
                    provider: "openai-codex".to_string(),
                    api_mode: "codex_responses".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    api_key: access_token,
                    source: "auth.json".to_string(),
                    requested_provider: requested_provider.to_string(),
                    ..Default::default()
                });
            }
            None
        }
        _ => None,
    }
}

/// Resolve Anthropic runtime from env/config/auth.
fn _resolve_anthropic_runtime(
    requested_provider: &str,
    model_cfg: &HashMap<String, Value>,
) -> Option<RuntimeProvider> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("ANTHROPIC_TOKEN").ok().filter(|s| !s.is_empty()))?;

    let cfg_provider = model_cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let cfg_base_url = if cfg_provider == "anthropic" {
        model_cfg
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .trim_end_matches('/')
            .to_string()
    } else {
        String::new()
    };

    let base_url = if !cfg_base_url.is_empty() {
        cfg_base_url
    } else {
        "https://api.anthropic.com".to_string()
    };

    Some(RuntimeProvider {
        provider: "anthropic".to_string(),
        api_mode: "anthropic_messages".to_string(),
        base_url,
        api_key,
        source: "env".to_string(),
        requested_provider: requested_provider.to_string(),
        ..Default::default()
    })
}

/// Resolve API-key providers (zai, kimi, minimax, etc.) from env.
fn _resolve_api_key_provider(
    provider: &str,
    requested_provider: &str,
    model_cfg: &HashMap<String, Value>,
) -> Option<RuntimeProvider> {
    let env_var = match provider {
        "gemini" => "GEMINI_API_KEY",
        "google-gemini-cli" => "GEMINI_CLI_API_KEY",
        "zai" => "ZAI_API_KEY",
        "kimi" | "kimi-coding" => "KIMI_API_KEY",
        "minimax" | "minimax-cn" => "MINIMAX_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "ollama" => "OLLAMA_API_KEY",
        _ => return None,
    };

    let mut api_key = std::env::var(env_var).ok().filter(|s| !s.is_empty()).unwrap_or_default();

    let mut base_url = default_base_url(parse_provider(provider))
        .unwrap_or("")
        .to_string();

    // Ollama: allow OLLAMA_HOST / OLLAMA_BASE_URL override
    if provider == "ollama" {
        if let Ok(host) = std::env::var("OLLAMA_HOST") {
            base_url = host.trim_end_matches('/').to_string() + "/v1";
        } else if let Ok(base) = std::env::var("OLLAMA_BASE_URL") {
            base_url = base.trim_end_matches('/').to_string() + "/v1";
        }
    }

    // Google Gemini CLI: fallback to GOOGLE_API_KEY if GEMINI_CLI_API_KEY is empty
    if provider == "google-gemini-cli" && api_key.is_empty() {
        if let Ok(key) = std::env::var("GOOGLE_API_KEY") {
            if !key.is_empty() {
                api_key = key;
            }
        }
    }

    if api_key.is_empty() && provider != "ollama" {
        return None;
    }

    let api_mode = _parse_api_mode(model_cfg.get("api_mode"))
        .or_else(|| _detect_api_mode_for_url(&base_url))
        .unwrap_or_else(|| "chat_completions".to_string());

    Some(RuntimeProvider {
        provider: provider.to_string(),
        api_mode,
        base_url,
        api_key: if provider == "ollama" && api_key.is_empty() {
            "no-key-required".to_string()
        } else {
            api_key
        },
        source: "env".to_string(),
        requested_provider: requested_provider.to_string(),
        ..Default::default()
    })
}

/// Resolve OpenRouter runtime (default fallback).
///
/// Mirrors Python `_resolve_openrouter_runtime()` (runtime_provider.py:420).
fn _resolve_openrouter_runtime(
    requested_provider: &str,
    explicit_api_key: Option<&str>,
    explicit_base_url: Option<&str>,
    model_cfg: &HashMap<String, Value>,
) -> RuntimeProvider {
    let cfg_base_url = model_cfg
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_string();
    let cfg_provider = model_cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let env_openrouter_base_url = std::env::var("OPENROUTER_BASE_URL")
        .ok()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();

    let use_config_base_url = !cfg_base_url.is_empty()
        && explicit_base_url.is_none()
        && (requested_provider == "auto" || (requested_provider == "custom" && cfg_provider == "custom"));

    let base_url = explicit_base_url
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .or({
            if use_config_base_url {
                Some(cfg_base_url)
            } else {
                None
            }
        })
        .or({
            if !env_openrouter_base_url.is_empty() {
                Some(env_openrouter_base_url)
            } else {
                None
            }
        })
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());

    let is_openrouter_url = base_url.contains("openrouter.ai");

    let api_key = if is_openrouter_url {
        explicit_api_key
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()))
            .unwrap_or_default()
    } else {
        let cfg_api_key = if use_config_base_url {
            model_cfg
                .get("api_key")
                .and_then(|v| v.as_str())
                .or_else(|| model_cfg.get("api").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        explicit_api_key
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or({
                if !cfg_api_key.is_empty() {
                    Some(cfg_api_key)
                } else {
                    None
                }
            })
            .or_else(|| std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok().filter(|s| !s.is_empty()))
            .unwrap_or_default()
    };

    let effective_provider = if requested_provider == "custom" {
        "custom"
    } else {
        "openrouter"
    };

    let api_key = if effective_provider == "custom" && api_key.is_empty() && !is_openrouter_url {
        "no-key-required".to_string()
    } else {
        api_key
    };

    let api_mode = _parse_api_mode(model_cfg.get("api_mode"))
        .or_else(|| _detect_api_mode_for_url(&base_url))
        .unwrap_or_else(|| "chat_completions".to_string());

    RuntimeProvider {
        provider: effective_provider.to_string(),
        api_mode,
        base_url,
        api_key,
        source: if explicit_api_key.is_some() || explicit_base_url.is_some() {
            "explicit".to_string()
        } else {
            "env/config".to_string()
        },
        requested_provider: requested_provider.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Auto-detect api_mode from a base URL.
///
/// Mirrors Python `_detect_api_mode_for_url()` (runtime_provider.py:37).
fn _detect_api_mode_for_url(base_url: &str) -> Option<String> {
    let normalized = base_url.trim().to_lowercase().trim_end_matches('/').to_string();
    if normalized.contains("api.openai.com") && !normalized.contains("openrouter") {
        Some("codex_responses".to_string())
    } else {
        None
    }
}

/// Validate an api_mode value from config.
///
/// Mirrors Python `_parse_api_mode()` (runtime_provider.py:130).
fn _parse_api_mode(raw: Option<&Value>) -> Option<String> {
    let s = raw?.as_str()?;
    let normalized = s.trim().to_lowercase();
    if VALID_API_MODES.contains(&normalized.as_str()) {
        Some(normalized)
    } else {
        None
    }
}

/// Normalize a custom provider name.
fn normalize_custom_provider_name(value: &str) -> String {
    value.trim().to_lowercase().replace(" ", "-")
}

/// Load auth.json as a serde_json::Value.
fn _load_auth_store_json() -> Value {
    let hermez_home = hermez_core::get_hermez_home();
    let path = hermez_home.join("auth.json");
    if !path.exists() {
        return serde_json::json!({"version": 1, "providers": {}});
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| {
            serde_json::json!({"version": 1, "providers": {}})
        }),
        Err(_) => serde_json::json!({"version": 1, "providers": {}}),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_alias() {
        assert_eq!(resolve_provider("google", None, None), "gemini");
    }

    #[test]
    fn test_resolve_requested_provider_explicit() {
        assert_eq!(
            resolve_requested_provider(Some("anthropic"), None),
            "anthropic"
        );
    }

    #[test]
    fn test_resolve_requested_provider_auto() {
        assert_eq!(resolve_requested_provider(None, None), "auto");
    }

    #[test]
    fn test_detect_api_mode_for_url_openai() {
        assert_eq!(
            _detect_api_mode_for_url("https://api.openai.com/v1"),
            Some("codex_responses".to_string())
        );
    }

    #[test]
    fn test_detect_api_mode_for_url_other() {
        assert_eq!(_detect_api_mode_for_url("https://openrouter.ai/api/v1"), None);
    }

    #[test]
    fn test_parse_api_mode_valid() {
        assert_eq!(
            _parse_api_mode(Some(&serde_json::json!("codex_responses"))),
            Some("codex_responses".to_string())
        );
    }

    #[test]
    fn test_parse_api_mode_invalid() {
        assert_eq!(
            _parse_api_mode(Some(&serde_json::json!("invalid_mode"))),
            None
        );
    }

    #[test]
    fn test_normalize_custom_provider_name() {
        assert_eq!(
            normalize_custom_provider_name("My Provider"),
            "my-provider"
        );
    }
}
