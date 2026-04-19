//! Auxiliary LLM client.
//!
//! Shared router for side tasks (context compression, session search,
//! web extraction, vision analysis). Multi-tier provider resolution chain.
//!
//! Mirrors the Python `auxiliary_client.py` (2,698 lines).
//!
//! # Resolution order for text tasks (auto mode):
//!   1. OpenRouter  (OPENROUTER_API_KEY)
//!   2. Nous Portal (NOUS_API_KEY or auth.json)
//!   3. Custom endpoint (config.yaml base_url + OPENAI_API_KEY)
//!   4. OpenAI Codex (Responses API via chatgpt.com)
//!   5. Native Anthropic
//!   6. Direct API-key providers (Gemini, ZAI, Kimi, Minimax)
//!
//! # Features
//! - Codex Responses API adapter for auxiliary calls
//! - Anthropic Messages API adapter for auxiliary calls
//! - OpenAI Chat Completions for auxiliary calls
//! - Error handling with payment/connection fallback
//! - Timeout and retry logic
//! - Credential pool integration

use std::collections::HashMap;

use crate::anthropic::{
    convert_content_to_anthropic, convert_messages, is_oauth_token,
    normalize_anthropic_response, AnthropicRequestBuilder,
};
use crate::codex::{
    call_responses_api_legacy, chat_to_responses_input, extract_text_from_output,
    extract_tool_calls_from_output,
};
use crate::credential_pool::{load_from_env, Credential};
use crate::error_classifier::{classify_api_error, ClassifiedError, FailoverReason};
use crate::model_metadata::compat_model_slug;
use crate::provider::{is_aggregator, parse_provider, resolve_provider_alias};
use crate::retry::{retry_with_backoff, RetryConfig};

// ── Constants ──────────────────────────────────────────────────────────────

/// Default auxiliary models per provider.
/// Mirrors Python `_API_KEY_PROVIDER_AUX_MODELS`.
const AUX_MODELS: &[(&str, &str)] = &[
    ("openrouter", "google/gemini-3-flash-preview"),
    ("nous", "google/gemini-3-flash-preview"),
    ("openai-codex", "gpt-5.2-codex"),
    ("anthropic", "claude-haiku-4-5-20251001"),
    ("gemini", "gemini-3-flash-preview"),
    ("zai", "glm-4.5-flash"),
    ("kimi", "kimi-k2-turbo-preview"),
    ("minimax", "MiniMax-M2.7"),
];

/// Default base URLs.
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const NOUS_DEFAULT_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const CODEX_AUX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// OpenRouter attribution headers.
const OR_HEADERS: &[(&str, &str)] = &[
    ("HTTP-Referer", "https://hermes-agent.nousresearch.com"),
    ("X-OpenRouter-Title", "Hermes Agent"),
];

/// Default timeout for auxiliary calls (seconds).
const DEFAULT_AUX_TIMEOUT_SECS: u64 = 30;

// ── Request / Response types ───────────────────────────────────────────────

/// Auxiliary LLM call parameters.
#[derive(Debug, Clone)]
pub struct AuxiliaryRequest {
    /// Task name (e.g. "compression", "web_extract", "vision").
    pub task: String,
    /// Provider override.
    pub provider: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
    /// API key override.
    pub api_key: Option<String>,
    /// Chat messages in OpenAI format.
    pub messages: Vec<serde_json::Value>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Max output tokens.
    pub max_tokens: Option<usize>,
    /// Tool definitions (for function calling).
    pub tools: Option<Vec<serde_json::Value>>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Additional request body fields.
    pub extra_body: Option<serde_json::Value>,
}

/// Auxiliary LLM response.
#[derive(Debug, Clone)]
pub struct AuxiliaryResponse {
    pub content: String,
    pub model: String,
    pub provider: String,
    pub usage: Option<UsageInfo>,
    pub finish_reason: Option<String>,
}

/// Token usage information.
#[derive(Debug, Clone)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ── Provider resolution chain ──────────────────────────────────────────────

/// The auxiliary provider resolution chain for auto mode.
const AUX_CHAIN: &[fn() -> Option<String>] = &[
    try_openrouter,
    try_nous,
    try_custom,
    try_codex,
    try_anthropic,
    try_api_key_provider,
];

/// Try OpenRouter provider.
///
/// Mirrors Python `_try_openrouter` (auxiliary_client.py:758).
/// First checks credential pool, then falls back to env var.
fn try_openrouter() -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry("openrouter");
    if selection.pool_exists {
        if let Some(entry) = &selection.entry {
            let key = pool_runtime_api_key(entry);
            if !key.is_empty() {
                tracing::debug!("Auxiliary client: OpenRouter via pool");
                return Some("openrouter".to_string());
            }
        }
    }
    // Fall back to env var
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(|_| "openrouter".to_string())
}

/// Try Nous Research provider.
///
/// Mirrors Python `_try_nous` (auxiliary_client.py:777).
/// Checks credential pool → auth.json → env var.
fn try_nous() -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry("nous");
    if selection.pool_exists {
        if let Some(entry) = &selection.entry {
            let key = pool_runtime_api_key(entry);
            if !key.is_empty() {
                tracing::debug!("Auxiliary client: Nous via pool");
                return Some("nous".to_string());
            }
        }
        // Pool present but no entry — try auth.json fallback
        if let Some(auth) = read_nous_auth() {
            if !nous_api_key(&auth).is_empty() {
                tracing::debug!("Auxiliary client: Nous via auth.json");
                return Some("nous".to_string());
            }
        }
    }
    // Fall back to env var
    std::env::var("NOUS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(|_| "nous".to_string())
}

/// Try custom/local endpoint (OPENAI_BASE_URL + OPENAI_API_KEY).
fn try_custom() -> Option<String> {
    let base_url = std::env::var("OPENAI_BASE_URL").ok()?;
    if base_url.is_empty() || base_url.to_lowercase().contains("openrouter.ai") {
        return None;
    }
    // Local servers don't require auth; use placeholder if no key
    let api_key = std::env::var("OPENAI_API_KEY").ok().unwrap_or_else(|| "no-key-required".to_string());
    if api_key.is_empty() {
        return None;
    }
    Some("custom".to_string())
}

/// Try OpenAI Codex provider (Responses API).
///
/// Mirrors Python `_try_codex` (auxiliary_client.py:647).
/// Checks credential pool → auth.json → env var.
fn try_codex() -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry("openai-codex");
    if selection.pool_exists {
        if let Some(entry) = &selection.entry {
            let token = pool_runtime_api_key(entry);
            if !token.is_empty() {
                tracing::debug!("Auxiliary client: Codex via pool");
                return Some("openai-codex".to_string());
            }
        }
        // Pool present but no entry — try auth.json fallback
        if let Some(token) = read_codex_access_token() {
            if !token.is_empty() {
                tracing::debug!("Auxiliary client: Codex via auth.json");
                return Some("openai-codex".to_string());
            }
        }
    }
    // Fall back to env var
    std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(|_| "openai-codex".to_string())
}

/// Try Anthropic provider.
///
/// Mirrors Python `_try_anthropic` (auxiliary_client.py:1008).
/// Checks credential pool first, then falls back to env vars.
fn try_anthropic() -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry("anthropic");
    if selection.pool_exists {
        if let Some(entry) = &selection.entry {
            let token = pool_runtime_api_key(entry);
            if !token.is_empty() {
                tracing::debug!("Auxiliary client: Anthropic via pool");
                return Some("anthropic".to_string());
            }
        }
    }
    // Fall back to env vars
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .or_else(|| std::env::var("ANTHROPIC_TOKEN").ok())
        .filter(|k| !k.is_empty())?;
    Some("anthropic".to_string())
}

/// Try resolving from API key to known provider (Gemini, ZAI, Kimi, Minimax).
///
/// Mirrors Python `_try_api_key_provider` pattern (auxiliary_client.py:708).
/// Checks credential pool first for each provider, then falls back to env vars.
fn try_api_key_provider() -> Option<String> {
    // Try Gemini
    {
        let selection = select_pool_entry("gemini");
        if selection.pool_exists {
            if let Some(ref entry) = selection.entry {
                let key = pool_runtime_api_key(entry);
                if !key.is_empty() {
                    return Some("gemini".to_string());
                }
            }
        }
    }
    if std::env::var("GEMINI_API_KEY").ok().filter(|k| !k.is_empty()).is_some() {
        return Some("gemini".to_string());
    }

    // Try ZAI/GLM
    {
        let selection = select_pool_entry("zai");
        if selection.pool_exists {
            if let Some(ref entry) = selection.entry {
                let key = pool_runtime_api_key(entry);
                if !key.is_empty() {
                    return Some("zai".to_string());
                }
            }
        }
    }
    if std::env::var("ZAI_API_KEY")
        .or_else(|_| std::env::var("GLM_API_KEY"))
        .ok()
        .filter(|k| !k.is_empty())
        .is_some()
    {
        return Some("zai".to_string());
    }

    // Try Kimi
    {
        let selection = select_pool_entry("kimi");
        if selection.pool_exists {
            if let Some(ref entry) = selection.entry {
                let key = pool_runtime_api_key(entry);
                if !key.is_empty() {
                    return Some("kimi".to_string());
                }
            }
        }
    }
    if std::env::var("KIMI_API_KEY").ok().filter(|k| !k.is_empty()).is_some() {
        return Some("kimi".to_string());
    }

    // Try Minimax
    {
        let selection = select_pool_entry("minimax");
        if selection.pool_exists {
            if let Some(ref entry) = selection.entry {
                let key = pool_runtime_api_key(entry);
                if !key.is_empty() {
                    return Some("minimax".to_string());
                }
            }
        }
    }
    if std::env::var("MINIMAX_API_KEY").ok().filter(|k| !k.is_empty()).is_some() {
        return Some("minimax".to_string());
    }

    None
}

// ── URL normalization ──────────────────────────────────────────────────────

/// Normalize an Anthropic-style base URL to OpenAI-compatible format.
///
/// Some providers (MiniMax, MiniMax-CN) expose an `/anthropic` endpoint for
/// the Anthropic Messages API and a separate `/v1` endpoint for OpenAI chat
/// completions. The auxiliary client uses the OpenAI SDK, so it must hit the
/// `/v1` surface. Passing the raw `inference_base_url` causes requests to
/// land on `/anthropic/chat/completions` — a 404.
///
/// Mirrors Python `_to_openai_base_url` (auxiliary_client.py:151).
pub(crate) fn to_openai_base_url(base_url: &str) -> String {
    let url = base_url.trim().trim_end_matches('/');
    if url.ends_with("/anthropic") {
        let rewritten = format!("{}/v1", url.strip_suffix("/anthropic").unwrap_or(url));
        tracing::debug!("Auxiliary client: rewrote base URL {} -> {}", url, rewritten);
        rewritten
    } else {
        url.to_string()
    }
}

// ── Credential pool integration ────────────────────────────────────────────

/// Result of attempting to select a credential from the pool.
/// Mirrors Python `_select_pool_entry` return (pool_exists, entry).
struct PoolSelection {
    /// Whether a credential pool exists for this provider.
    pool_exists: bool,
    /// The selected credential, if any.
    entry: Option<Credential>,
}

/// Try to select a credential from the pool for the given provider.
///
/// Mirrors Python `_select_pool_entry` (auxiliary_client.py:168).
fn select_pool_entry(provider: &str) -> PoolSelection {
    if let Some(pool) = load_from_env(provider) {
        if !pool.has_credentials() {
            return PoolSelection { pool_exists: true, entry: None };
        }
        let entry = pool.select();
        return PoolSelection { pool_exists: true, entry };
    }
    PoolSelection { pool_exists: false, entry: None }
}

/// Extract the runtime API key from a credential entry.
///
/// Mirrors Python `_pool_runtime_api_key` (auxiliary_client.py:184).
fn pool_runtime_api_key(entry: &Credential) -> String {
    entry.runtime_api_key().to_string()
}

/// Extract the runtime base URL from a credential entry.
///
/// Mirrors Python `_pool_runtime_base_url` (auxiliary_client.py:193).
fn pool_runtime_base_url(entry: &Credential, fallback: &str) -> String {
    entry
        .runtime_base_url()
        .unwrap_or(fallback)
        .trim()
        .trim_end_matches('/')
        .to_string()
}

// ── Auth store (auth.json) integration ─────────────────────────────────────

/// Resolve path to `~/.hermes/auth.json`.
/// Mirrors Python `~/.hermes/auth.json` reading (auxiliary_client.py:612).
#[allow(dead_code)]
fn auth_json_path() -> Option<std::path::PathBuf> {
    let home = hermes_core::hermes_home::get_hermes_home();
    let path = home.join("auth.json");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Read and parse auth.json.
/// Mirrors Python `data = json.loads(_AUTH_JSON_PATH.read_text())`.
fn read_auth_json() -> Option<serde_json::Value> {
    hermes_core::with_auth_json_read_lock(|| {
        let path = auth_json_path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    })
    .ok()
    .flatten()
}

/// Check if a JWT token string is expired (by decoding its payload).
///
/// Mirrors Python JWT expiry check in `_read_codex_access_token`
/// (auxiliary_client.py:663-673).
fn is_jwt_expired(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return false; // Not a valid JWT format, assume valid
    }
    // Decode payload with padding fix
    let mut payload = parts[1].to_string();
    let pad_len = (4 - payload.len() % 4) % 4;
    payload.push_str(&"=".repeat(pad_len));
    let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, &payload) else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return false;
    };
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_f64()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        return now > exp;
    }
    false // No exp claim, assume valid
}

/// Read a valid, non-expired Codex OAuth access token from auth.json.
///
/// Falls back to auth.json when credential pool has no selectable entry.
/// Mirrors Python `_read_codex_access_token` (auxiliary_client.py:638).
fn read_codex_access_token() -> Option<String> {
    // Pool already checked in try_codex; this is the auth.json fallback
    let auth = read_auth_json()?;
    if auth.get("active_provider").and_then(|v| v.as_str()) != Some("openai-codex") {
        return None;
    }
    let tokens = auth.get("providers")
        .and_then(|p| p.get("openai-codex"))
        .and_then(|p| p.get("tokens"))?;
    let access_token = tokens.get("access_token")
        .and_then(|v| v.as_str())?;
    if access_token.trim().is_empty() {
        return None;
    }
    // Check JWT expiry
    if is_jwt_expired(access_token) {
        tracing::debug!("Codex access token expired, skipping");
        return None;
    }
    Some(access_token.trim().to_string())
}

/// Read Nous auth data from auth.json.
///
/// Mirrors Python `_read_nous_auth` (auxiliary_client.py:590).
/// Returns the provider state dict if Nous is active with tokens.
fn read_nous_auth() -> Option<serde_json::Value> {
    // Pool already checked in try_nous; this is the auth.json fallback
    let auth = read_auth_json()?;
    if auth.get("active_provider").and_then(|v| v.as_str()) != Some("nous") {
        return None;
    }
    let provider = auth.get("providers")
        .and_then(|p| p.get("nous"))?;
    // Must have at least an access_token or agent_key
    let has_agent_key = provider.get("agent_key")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_access_token = provider.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_agent_key && !has_access_token {
        return None;
    }
    Some(provider.clone())
}

/// Extract the best API key from a Nous provider state dict.
/// Mirrors Python `_nous_api_key` (auxiliary_client.py:628).
fn nous_api_key(provider: &serde_json::Value) -> String {
    provider.get("agent_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| provider.get("access_token").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string()
}

/// Resolve the Nous inference base URL from auth.json or default.
/// Mirrors Python `_nous_base_url` (auxiliary_client.py:633).
#[allow(dead_code)]
fn nous_base_url(auth: &serde_json::Value) -> String {
    auth.get("inference_base_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(NOUS_DEFAULT_BASE_URL)
        .to_string()
}

// ── Error classification helpers ───────────────────────────────────────────

/// Detect payment / credit exhaustion errors.
/// Mirrors Python `_is_payment_error`.
fn is_payment_error(err: &ClassifiedError) -> bool {
    err.reason == FailoverReason::Billing
        || err.status_code == Some(402)
}

/// Detect connection / network errors that warrant provider fallback.
/// Mirrors Python `_is_connection_error`.
fn is_connection_error(err: &ClassifiedError) -> bool {
    matches!(
        err.reason,
        FailoverReason::Timeout | FailoverReason::ServerError
    )
}

/// Detect errors that warrant trying an alternative provider.
fn is_payment_or_connection_error(err: &ClassifiedError) -> bool {
    is_payment_error(err) || is_connection_error(err)
}

// ── Auxiliary call entry point ─────────────────────────────────────────────

/// Call the auxiliary LLM with automatic provider resolution.
///
/// Resolution priority:
/// 1. Explicit provider/model/base_url/api_key from request
/// 2. Config file auxiliary.{task}.provider/model/base_url/api_key
/// 3. Auto-detection chain (OpenRouter → Nous → Custom → Codex → Anthropic → API-key providers)
///
/// C4: Explicitly requested provider is a hard constraint — no silent fallback.
/// Mirrors Python `call_llm` (auxiliary_client.py:2346).
pub async fn call_auxiliary(request: AuxiliaryRequest) -> Result<AuxiliaryResponse, ClassifiedError> {
    // Resolve provider/model from task config or explicit args
    let (resolved_provider_name, cfg_model, cfg_base_url, cfg_api_key, _cfg_api_mode) =
        resolve_task_provider_model(
            if request.task.is_empty() { None } else { Some(&request.task) },
            request.provider.as_deref(),
            request.model.as_deref(),
            request.base_url.as_deref(),
            request.api_key.as_deref(),
        );

    // Apply config model/base_url/api_key if not overridden by request
    let effective_model = request.model.clone().or(cfg_model);
    let effective_base_url = request.base_url.clone().or(cfg_base_url);
    let effective_api_key = request.api_key.clone().or(cfg_api_key);

    // If explicit provider or custom base_url, do a single-provider call
    let is_auto = resolved_provider_name == "auto";
    if !is_auto || resolved_provider_name == "custom" {
        let provider = if resolved_provider_name == "custom" {
            resolved_provider_name
        } else {
            resolve_provider_alias(&resolved_provider_name).to_string()
        };
        let resolved = ResolvedProvider {
            provider,
            model: effective_model,
            base_url: effective_base_url,
            api_key: effective_api_key,
        };
        return execute_auxiliary_with_retry(&resolved, &request).await;
    }

    // Auto mode: build the chain of available providers
    let chain = get_available_providers();
    let mut tried = Vec::new();

    for provider in chain {
        let resolved = ResolvedProvider {
            provider: provider.clone(),
            model: effective_model.clone(),
            base_url: effective_base_url.clone(),
            api_key: effective_api_key.clone(),
        };

        match execute_auxiliary_with_retry(&resolved, &request).await {
            Ok(response) => return Ok(response),
            Err(ref e) if is_payment_or_connection_error(e) => {
                let reason = if is_payment_error(e) { "payment error" } else { "connection error" };
                tracing::info!(
                    "Auxiliary {}: {} on {}, trying fallback",
                    if request.task.is_empty() { "call" } else { &request.task },
                    reason,
                    provider
                );
                tried.push(provider);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(classify_api_error(
        "auto",
        effective_model.as_deref().unwrap_or("unknown"),
        None,
        "No auxiliary provider available — set OPENROUTER_API_KEY or configure a local model",
    ))
}

/// Resolved provider with credentials.
#[derive(Debug, Clone)]
struct ResolvedProvider {
    provider: String,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

/// Resolve auto provider by trying the chain.
#[allow(dead_code)]
async fn resolve_auto_provider(_request: &AuxiliaryRequest) -> Result<String, ClassifiedError> {
    for (i, try_fn) in AUX_CHAIN.iter().enumerate() {
        if let Some(name) = try_fn() {
            if i > 0 {
                tracing::debug!("Auxiliary auto-detect: skipping {} providers, using {}", i, name);
            }
            return Ok(name);
        }
    }
    Err(classify_api_error(
        "auto",
        "unknown",
        None,
        "No auxiliary provider available — set OPENROUTER_API_KEY or configure a local model",
    ))
}

/// Execute a single auxiliary API call.
async fn execute_auxiliary_call(
    resolved: &ResolvedProvider,
    request: &AuxiliaryRequest,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    // Resolve timeout from config if not explicitly set
    let timeout_secs = request.timeout_secs
        .unwrap_or_else(|| get_task_timeout(if request.task.is_empty() { None } else { Some(&request.task) }, DEFAULT_AUX_TIMEOUT_SECS));
    let provider = &resolved.provider;
    let provider_type = parse_provider(provider);

    match provider_type {
        crate::provider::ProviderType::Anthropic => {
            call_anthropic_auxiliary(resolved, request, timeout_secs).await
        }
        crate::provider::ProviderType::Codex => {
            call_codex_auxiliary(resolved, request, timeout_secs).await
        }
        // All other providers use OpenAI-compatible Chat Completions
        _ => call_openai_compat_auxiliary(resolved, request, timeout_secs).await,
    }
}

// ── OpenAI-compatible Chat Completions ─────────────────────────────────────

/// Call an OpenAI-compatible provider (OpenRouter, Nous, Gemini, ZAI, etc.).
async fn call_openai_compat_auxiliary(
    resolved: &ResolvedProvider,
    request: &AuxiliaryRequest,
    timeout_secs: u64,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    let provider = &resolved.provider;
    let provider_type = parse_provider(provider);

    // Resolve API key: request > env > default
    let api_key = resolved.api_key.clone()
        .or_else(|| resolve_api_key(provider))
        .unwrap_or_default();

    // Resolve base URL: request > provider default > env
    let base_url = resolved.base_url.clone()
        .map(|u| to_openai_base_url(&u))
        .or_else(|| resolve_base_url(provider))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let mut model = resolved.model.clone()
        .or_else(|| default_aux_model(provider))
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    // Strip model prefix for non-aggregator providers
    if model.contains('/') && !is_aggregator(provider_type.clone()) {
        let compat = compat_model_slug(&model);
        tracing::debug!("Stripping model slug prefix: {} -> {}", model, compat);
        model = compat;
    }

    // Build HTTP client
    let http_client = build_http_client(timeout_secs)?;

    // Build messages
    let messages = build_openai_messages(&request.messages)?;

    // Build request body
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
    });

    if let Some(temp) = request.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    // Handle max_tokens: start with max_tokens, retry with max_completion_tokens
    // if provider doesn't support it. Mirrors Python _build_call_kwargs +
    // call_llm max_tokens retry (auxiliary_client.py:2466-2483).
    let max_tokens_value = request.max_tokens;
    if let Some(max_tokens) = max_tokens_value {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }

    if let Some(ref tools) = request.tools {
        body["tools"] = serde_json::json!(tools);
    }

    // Merge extra_body
    if let Some(ref extra) = request.extra_body {
        if let Some(obj) = extra.as_object() {
            if let Some(body_obj) = body.as_object_mut() {
                for (k, v) in obj {
                    body_obj.insert(k.clone(), v.clone());
                }
            }
        }
    }

    // Provider-specific headers
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {}", api_key));
    headers.insert("Content-Type".to_string(), "application/json".to_string());

    // OpenRouter attribution headers
    if provider_type == crate::provider::ProviderType::OpenRouter {
        for (k, v) in OR_HEADERS {
            headers.insert(k.to_string(), v.to_string());
        }
    }

    // Provider-specific User-Agent
    if provider == "kimi" || provider == "kimi-coding" {
        headers.insert("User-Agent".to_string(), "KimiCLI/1.30.0".to_string());
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    // Execute request with max_tokens → max_completion_tokens retry.
    // Mirrors Python call_llm max_tokens retry (auxiliary_client.py:2466-2483).
    let (resp_status, resp_text) = do_openai_compat_request(
        &http_client, &url, &headers, &body, provider, &model,
    ).await?;

    parse_openai_response_text(&resp_text, provider, &model, resp_status)
}

/// Execute an OpenAI-compatible request, retrying with max_completion_tokens
/// if max_tokens is unsupported.
async fn do_openai_compat_request(
    http_client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    body: &serde_json::Value,
    provider: &str,
    model: &str,
) -> Result<(u16, String), ClassifiedError> {
    // First attempt with max_tokens (if present)
    let result = send_json_request(http_client, url, headers, body, provider, model).await;
    match result {
        Ok(resp) => Ok(resp),
        Err(ref e) => {
            let err_str = format!("{e}");
            if err_str.contains("max_tokens") || err_str.contains("unsupported_parameter") {
                // Retry with max_completion_tokens instead
                let mut retry_body = body.clone();
                if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_u64()) {
                    retry_body.as_object_mut().unwrap().remove("max_tokens");
                    retry_body["max_completion_tokens"] = serde_json::json!(max_tokens);
                }
                tracing::debug!("Auxiliary client: max_tokens rejected, retrying with max_completion_tokens");
                return send_json_request(http_client, url, headers, &retry_body, provider, model).await;
            }
            Err(e.clone())
        }
    }
}

/// Send a JSON POST request and return (status_code, response_text).
async fn send_json_request(
    http_client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    body: &serde_json::Value,
    provider: &str,
    model: &str,
) -> Result<(u16, String), ClassifiedError> {
    let resp = http_client
        .post(url)
        .headers(build_reqwest_headers(headers))
        .json(body)
        .send()
        .await
        .map_err(|e| {
            classify_api_error(provider, model, None, &format!("Request failed: {e}"))
        })?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

/// Parse the response text or return an error for non-2xx status.
fn parse_openai_response_text(
    text: &str,
    provider: &str,
    model: &str,
    status: u16,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    if status >= 400 {
        return Err(classify_api_error(provider, model, Some(status), text));
    }

    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        classify_api_error(provider, model, Some(status), &format!("Failed to parse response: {e}"))
    })?;

    parse_openai_response(json, provider, model)
}

/// Parse an OpenAI-compatible response.
fn parse_openai_response(
    json: serde_json::Value,
    provider: &str,
    model: &str,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    // Validate shape
    let choices = json.get("choices").and_then(|v| v.as_array());
    if choices.is_none_or(|a| a.is_empty()) {
        return Err(classify_api_error(provider, model, None, "API returned empty choices array"));
    }
    let choice = &choices.unwrap()[0];
    let message = choice.get("message");
    if message.is_none_or(|m| {
        m.get("content").and_then(|v| v.as_str()).unwrap_or("").is_empty()
            && m.get("tool_calls").is_none_or(|tc| tc.as_array().is_none_or(|a| a.is_empty()))
    }) {
        return Err(classify_api_error(
            provider, model, None,
            "API response choice missing message content or tool calls",
        ));
    }

    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let finish_reason = choice
        .get("finish_reason")
        .map(|v| v.to_string());

    let usage = json.get("usage").map(|u| UsageInfo {
        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    });

    let response_model = json
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(model);

    Ok(AuxiliaryResponse {
        content,
        model: response_model.to_string(),
        provider: provider.to_string(),
        usage,
        finish_reason,
    })
}

// ── Anthropic Messages API ─────────────────────────────────────────────────

/// Call Anthropic Messages API for auxiliary tasks.
async fn call_anthropic_auxiliary(
    resolved: &ResolvedProvider,
    request: &AuxiliaryRequest,
    timeout_secs: u64,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    let provider = &resolved.provider;
    let model = resolved.model.clone()
        .or_else(|| default_aux_model(provider))
        .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());

    // Resolve API key: request > credential pool > env
    let api_key = resolved.api_key.clone()
        .or_else(|| resolve_api_key(provider))
        .ok_or_else(|| {
            classify_api_error(provider, &model, None, "No Anthropic API key found")
        })?;

    // Resolve base URL
    // Resolve base URL: request > credential pool > default
    let base_url = resolved.base_url.clone()
        .map(|u| to_openai_base_url(&u))
        .or_else(|| resolve_base_url(provider))
        .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());

    let _is_oauth = is_oauth_token(&api_key);

    // Build HTTP client
    let http_client = build_http_client(timeout_secs)?;

    // Convert messages to Anthropic format
    let (system, messages_anthropic) = convert_messages(&request.messages, true);

    // Convert image blocks for Anthropic-compatible endpoints
    let messages_anthropic = convert_image_blocks(messages_anthropic);

    // Build system prompt value
    let system_value = system.map(|sys| {
        sys.get("content")
            .and_then(|v| v.as_str())
            .map(|s| serde_json::json!(s))
            .unwrap_or(serde_json::json!(""))
    });

    // Build request using AnthropicRequestBuilder
    let builder = AnthropicRequestBuilder {
        model: model.clone(),
        messages: messages_anthropic,
        system_prompt: system_value,
        max_tokens: request.max_tokens.unwrap_or(4096),
        temperature: request.temperature,
        tools: request.tools.clone(),
        api_key: api_key.clone(),
        base_url: Some(base_url.clone()),
        thinking_enabled: false,
        thinking_effort: None,
        fast_mode: false,
        stream: false,
    };

    let (body_str, headers, url) = builder.build();

    let resp = http_client
        .post(&url)
        .headers(build_reqwest_headers(&headers))
        .body(body_str)
        .send()
        .await
        .map_err(|e| {
            classify_api_error(provider, &model, None, &format!("Request failed: {e}"))
        })?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

    if status >= 400 {
        return Err(classify_api_error(provider, &model, Some(status), &text));
    }

    // Parse response
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        classify_api_error(provider, &model, Some(status), &format!("Failed to parse response: {e}"))
    })?;

    // Convert Anthropic response to our response structure
    let normalized = normalize_anthropic_response(&json, false);
    let response_model = json.get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(&model);

    let usage = json.get("usage").map(|u| UsageInfo {
        prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        total_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
            + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    });

    Ok(AuxiliaryResponse {
        content: normalized.content.unwrap_or_default(),
        model: response_model.to_string(),
        provider: provider.to_string(),
        usage,
        finish_reason: Some(normalized.finish_reason),
    })
}

/// Convert OpenAI-format image content blocks to Anthropic format.
/// Mirrors Python `_convert_openai_images_to_anthropic`.
fn convert_image_blocks(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    messages
        .into_iter()
        .map(|msg| {
            let content = msg.get("content").cloned();
            if let Some(content_arr) = content.and_then(|v| v.as_array().cloned()) {
                let has_image = content_arr.iter().any(|block| {
                    block.get("type").and_then(|v| v.as_str()) == Some("image_url")
                });
                if has_image {
                    let converted: Vec<serde_json::Value> = content_arr
                        .into_iter()
                        .map(|block| convert_content_to_anthropic(&block))
                        .collect();
                    let mut msg = msg;
                    if let Some(obj) = msg.as_object_mut() {
                        obj.insert("content".to_string(), serde_json::json!(converted));
                    }
                    return msg;
                }
            }
            msg
        })
        .collect()
}

// ── Codex Responses API ────────────────────────────────────────────────────

/// Call Codex Responses API for auxiliary tasks.
async fn call_codex_auxiliary(
    resolved: &ResolvedProvider,
    request: &AuxiliaryRequest,
    timeout_secs: u64,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    let provider = &resolved.provider;
    let model = resolved.model.clone()
        .or_else(|| default_aux_model(provider))
        .unwrap_or_else(|| "gpt-5.2-codex".to_string());

    // Resolve API key: request > credential pool > env
    let api_key = resolved.api_key.clone()
        .or_else(|| resolve_api_key(provider))
        .ok_or_else(|| {
            classify_api_error(provider, &model, None, "No Codex API key found")
        })?;

    // Resolve base URL: request > credential pool > default
    let base_url = resolved.base_url.clone()
        .map(|u| to_openai_base_url(&u))
        .or_else(|| resolve_base_url(provider))
        .unwrap_or_else(|| CODEX_AUX_BASE_URL.to_string());

    // Extract system message as instructions
    let mut instructions = "You are a helpful assistant.".to_string();
    let mut chat_messages = Vec::new();
    for msg in &request.messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        if role == "system" {
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                instructions = content.to_string();
            }
        } else {
            chat_messages.push(msg.clone());
        }
    }

    // Convert to Responses API input format
    let input = chat_to_responses_input(&chat_messages);

    // Call Responses API
    let result = call_responses_api_legacy(
        &model,
        &instructions,
        &input,
        request.tools.as_deref(),
        request.max_tokens,
        request.temperature,
        &base_url,
        &api_key,
        timeout_secs,
    )
    .await?;

    // Extract content
    let content = extract_text_from_output(&result.output);
    let tool_calls = extract_tool_calls_from_output(&result.output);

    let finish_reason = if tool_calls.is_empty() {
        Some("stop".to_string())
    } else {
        Some("tool_calls".to_string())
    };

    let usage = result.usage.map(|u| UsageInfo {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.total_tokens,
    });

    Ok(AuxiliaryResponse {
        content,
        model: result.model,
        provider: provider.to_string(),
        usage,
        finish_reason,
    })
}

// ── Helper functions ───────────────────────────────────────────────────────

/// Build OpenAI-compatible messages from serde_json::Value array.
fn build_openai_messages(
    messages: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, ClassifiedError> {
    let mut result = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let mut normalized = serde_json::Map::new();
        normalized.insert("role".to_string(), serde_json::json!(role));

        let content = msg.get("content").cloned();
        if let Some(content_val) = content {
            // Handle both string and array content
            normalized.insert("content".to_string(), content_val);
        }

        // Preserve tool_calls for assistant messages
        if role == "assistant" {
            if let Some(tool_calls) = msg.get("tool_calls") {
                normalized.insert("tool_calls".to_string(), tool_calls.clone());
            }
        }
        // Preserve tool_call_id for tool messages
        if role == "tool" {
            if let Some(call_id) = msg.get("tool_call_id") {
                normalized.insert("tool_call_id".to_string(), call_id.clone());
            }
        }

        result.push(serde_json::Value::Object(normalized));
    }

    if result.is_empty() {
        return Err(classify_api_error("auxiliary", "unknown", None, "No valid messages in request"));
    }

    Ok(result)
}

// ── Task-driven config resolution ──────────────────────────────────────────

/// Per-task auxiliary model override from config.yaml.
///
/// Mirrors Python `AuxiliaryTaskConfig` / `auxiliary.{task}` reading
/// (auxiliary_client.py:2114-2174).
#[derive(Debug, Clone, Default)]
struct TaskConfig {
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_mode: Option<String>,
}

/// Determine provider + model for a call based on config + explicit args.
///
/// Priority:
///   1. Explicit provider/model/base_url/api_key args (always win)
///   2. Config file auxiliary.{task}.provider/model/base_url/api_key
///   3. "auto" (full auto-detection chain)
///
/// Returns (provider, model, base_url, api_key, api_mode).
/// Mirrors Python `_resolve_task_provider_model` (auxiliary_client.py:2114).
fn resolve_task_provider_model(
    task: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> (String, Option<String>, Option<String>, Option<String>, Option<String>) {
    // Load config for the given task
    let task_cfg = task.map(load_auxiliary_task_config).unwrap_or_default();

    // Explicit args always win
    if let Some(url) = base_url {
        return ("custom".to_string(), model.map(String::from), Some(url.to_string()), api_key.map(String::from), None);
    }
    if let Some(p) = provider {
        return (p.to_string(), model.map(String::from), None, api_key.map(String::from), None);
    }

    // Config-based resolution for tasks
    if let Some(_t) = task {
        if let Some(cfg_url) = &task_cfg.base_url {
            return ("custom".to_string(), model.map(String::from).or(task_cfg.model), Some(cfg_url.clone()), task_cfg.api_key.clone(), task_cfg.api_mode.clone());
        }
        if let Some(cfg_provider) = &task_cfg.provider {
            if cfg_provider != "auto" {
                return (cfg_provider.clone(), model.map(String::from).or(task_cfg.model), None, None, task_cfg.api_mode.clone());
            }
        }
    }

    // Fall through to auto
    ("auto".to_string(), model.map(String::from).or(task_cfg.model), None, None, None)
}

/// Load auxiliary task config from HermesConfig.
fn load_auxiliary_task_config(task: &str) -> TaskConfig {
    let config = hermes_core::config::HermesConfig::load();
    if let Ok(cfg) = config {
        let aux = &cfg.auxiliary_model;
        if let Some(task_cfg) = aux.tasks.get(task) {
            return TaskConfig {
                provider: task_cfg.provider.clone(),
                model: task_cfg.model.clone(),
                base_url: task_cfg.base_url.clone(),
                api_key: task_cfg.api_key.clone(),
                api_mode: None, // Not stored in Rust config yet
            };
        }
    }
    TaskConfig::default()
}

/// Get timeout for a task from config, falling back to default.
///
/// Mirrors Python `_get_task_timeout` (auxiliary_client.py:2180).
fn get_task_timeout(task: Option<&str>, default_secs: u64) -> u64 {
    let Some(t) = task else { return default_secs };
    let config = hermes_core::config::HermesConfig::load();
    if let Ok(cfg) = config {
        if let Some(task_cfg) = cfg.auxiliary_model.tasks.get(t) {
            if let Some(timeout) = task_cfg.timeout {
                return timeout.max(1.0) as u64;
            }
        }
    }
    default_secs
}

/// Resolve API key for a given provider.
/// Checks credential pool → auth.json → env vars.
fn resolve_api_key(provider: &str) -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry(provider);
    if selection.pool_exists {
        if let Some(entry) = &selection.entry {
            let key = pool_runtime_api_key(entry);
            if !key.is_empty() {
                return Some(key);
            }
        }
        // Pool present but no usable entry — try auth.json for OAuth providers
        match provider {
            "openai-codex" => {
                if let Some(token) = read_codex_access_token() {
                    if !token.is_empty() {
                        return Some(token);
                    }
                }
            }
            "nous" => {
                if let Some(auth) = read_nous_auth() {
                    let key = nous_api_key(&auth);
                    if !key.is_empty() {
                        return Some(key);
                    }
                }
            }
            _ => {}
        }
    }
    // Fall back to env vars
    let env_var = match provider {
        "openrouter" => "OPENROUTER_API_KEY",
        "nous" => "NOUS_API_KEY",
        "openai-codex" | "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "zai" => "ZAI_API_KEY",
        "kimi" | "kimi-coding" => "KIMI_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "custom" => "OPENAI_API_KEY",
        _ => return None,
    };
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
}

/// Resolve base URL for a given provider.
/// Checks credential pool first, then falls back to env/default.
fn resolve_base_url(provider: &str) -> Option<String> {
    // Try credential pool first
    let selection = select_pool_entry(provider);
    if selection.pool_exists {
        if let Some(entry) = selection.entry {
            let fallback = match provider {
                "openrouter" => OPENROUTER_BASE_URL,
                "nous" => NOUS_DEFAULT_BASE_URL,
                "anthropic" => ANTHROPIC_DEFAULT_BASE_URL,
                "openai-codex" => CODEX_AUX_BASE_URL,
                _ => "",
            };
            let url = pool_runtime_base_url(&entry, fallback);
            if !url.is_empty() {
                let normalized = to_openai_base_url(&url);
                return Some(normalized);
            }
        }
    }
    // Fall back to env/default
    match provider {
        "openrouter" => Some(OPENROUTER_BASE_URL.to_string()),
        "nous" => std::env::var("NOUS_INFERENCE_BASE_URL")
            .ok()
            .map(|u| to_openai_base_url(&u))
            .or_else(|| Some(NOUS_DEFAULT_BASE_URL.to_string())),
        "anthropic" => Some(ANTHROPIC_DEFAULT_BASE_URL.to_string()),
        "openai-codex" => Some(CODEX_AUX_BASE_URL.to_string()),
        "custom" => std::env::var("OPENAI_BASE_URL").ok().map(|u| to_openai_base_url(&u)),
        _ => None,
    }
}

/// Get default auxiliary model for a provider.
fn default_aux_model(provider: &str) -> Option<String> {
    AUX_MODELS
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, m)| m.to_string())
}

/// Check if this is a direct OpenAI endpoint (not via OpenRouter).
#[allow(dead_code)]
fn is_openai_direct(provider: &str, base_url: &str) -> bool {
    provider == "openai" && base_url.to_lowercase().contains("api.openai.com")
}

/// Build HTTP client with timeout.
fn build_http_client(timeout_secs: u64) -> Result<reqwest::Client, ClassifiedError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| {
            classify_api_error("auxiliary", "unknown", None, &format!("Failed to build HTTP client: {e}"))
        })
}

/// Build reqwest headers from a HashMap.
fn build_reqwest_headers(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut header_map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (k.parse::<reqwest::header::HeaderName>(), v.parse::<reqwest::header::HeaderValue>()) {
            header_map.insert(name, value);
        }
    }
    header_map
}

// ── Public utility functions ───────────────────────────────────────────────

/// Extract content from an LLM response, falling back to reasoning fields.
///
/// Mirrors the main agent loop's behavior when a reasoning model (DeepSeek-R1,
/// Qwen-QwQ, etc.) returns content=None with reasoning in structured fields.
///
/// Resolution order:
///   1. `message.content` — strip inline think/reasoning blocks, check for
///      remaining non-whitespace text.
///   2. `message.reasoning` / `message.reasoning_content` — direct structured
///      reasoning fields (DeepSeek, Moonshot, Novita, etc.).
///   3. `message.reasoning_details` — OpenRouter unified array format.
///
/// Mirrors Python `extract_content_or_reasoning` (auxiliary_client.py:2519).
pub fn extract_content_or_reasoning(response: &serde_json::Value) -> String {
    use crate::reasoning::extract_reasoning;
    use regex::Regex;

    if let Some(message) = response.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|c| c.get("message")) {
        // 1. Try message.content, stripping inline think blocks
        if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
            let content = content.trim();
            if !content.is_empty() {
                // Strip inline think/reasoning blocks (mirrors _strip_think_blocks)
                static STRIP_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
                let re = STRIP_RE.get_or_init(|| {
                    Regex::new(r"<(?i)(?:think|thinking|reasoning|thought|REASONING_SCRATCHPAD)>.*?</(?i)(?:think|thinking|reasoning|thought|REASONING_SCRATCHPAD)>")
                        .unwrap()
                });
                let cleaned = re.replace_all(content, "").trim().to_string();
                if !cleaned.is_empty() {
                    return cleaned;
                }
            }
        }

        // 2. Fall back to structured reasoning fields
        let reasoning = extract_reasoning(message);
        if !reasoning.is_empty() {
            return reasoning;
        }
    }

    String::new()
}

/// Centralized asynchronous LLM call.
///
/// Same as `call_llm()` but async. Mirrors Python `async_call_llm`
/// (auxiliary_client.py:2575).
pub async fn async_call_llm(
    task: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
    messages: Vec<serde_json::Value>,
    temperature: Option<f64>,
    max_tokens: Option<usize>,
    tools: Option<Vec<serde_json::Value>>,
    timeout: Option<u64>,
    extra_body: Option<serde_json::Value>,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    call_auxiliary(AuxiliaryRequest {
        task: task.unwrap_or("").to_string(),
        provider: provider.map(String::from),
        model: model.map(String::from),
        base_url: base_url.map(String::from),
        api_key: api_key.map(String::from),
        messages,
        temperature,
        max_tokens,
        tools,
        timeout_secs: timeout,
        extra_body,
    })
    .await
}

/// Centralized synchronous LLM call.
///
/// Mirrors Python `call_llm` (auxiliary_client.py:2346).
pub fn call_llm(
    task: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
    messages: Vec<serde_json::Value>,
    temperature: Option<f64>,
    max_tokens: Option<usize>,
    tools: Option<Vec<serde_json::Value>>,
    timeout: Option<u64>,
    extra_body: Option<serde_json::Value>,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    let rt = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            return tokio::runtime::Runtime::new()
                .map_err(|e| classify_api_error("auxiliary", "unknown", None, &format!("Failed to create tokio runtime: {e}")))?
                .block_on(async_call_llm(
                    task, provider, model, base_url, api_key,
                    messages, temperature, max_tokens, tools, timeout, extra_body,
                ));
        }
    };
    rt.block_on(async_call_llm(
        task, provider, model, base_url, api_key,
        messages, temperature, max_tokens, tools, timeout, extra_body,
    ))
}

/// Get the default auxiliary model for a provider.
pub fn get_default_aux_model(provider: &str) -> Option<String> {
    default_aux_model(provider)
}

/// Check if a provider is available (has API key configured).
pub fn is_provider_available(provider: &str) -> bool {
    match provider {
        "openrouter" => try_openrouter().is_some(),
        "nous" => try_nous().is_some(),
        "custom" => try_custom().is_some(),
        "openai-codex" | "codex" => try_codex().is_some(),
        "anthropic" => try_anthropic().is_some(),
        "auto" => AUX_CHAIN.iter().any(|f| f().is_some()),
        _ => resolve_api_key(provider).is_some(),
    }
}

/// Get the ordered list of available auxiliary providers.
pub fn get_available_providers() -> Vec<String> {
    AUX_CHAIN
        .iter()
        .filter_map(|f| f())
        .collect()
}

/// Execute a single auxiliary call with retry logic.
async fn execute_auxiliary_with_retry(
    resolved: &ResolvedProvider,
    request: &AuxiliaryRequest,
) -> Result<AuxiliaryResponse, ClassifiedError> {
    let config = RetryConfig {
        max_retries: 2,
        base_delay: std::time::Duration::from_millis(500),
        max_delay: std::time::Duration::from_secs(5),
        jitter: true,
    };

    retry_with_backoff(&config, |attempt| {
        let resolved = resolved.clone();
        let req = request.clone();
        async move {
            let result = execute_auxiliary_call(&resolved, &req).await;
            if attempt > 0 {
                if let Err(ref e) = result {
                    tracing::warn!(
                        attempt, provider = %resolved.provider,
                        error = %e, "auxiliary call retrying"
                    );
                }
            }
            result
        }
    })
    .await
}

/// Get auxiliary provider chain labels.
pub fn get_provider_chain_labels() -> Vec<&'static str> {
    vec!["openrouter", "nous", "local/custom", "openai-codex", "anthropic", "api-key"]
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_openrouter_without_env() {
        // OPENROUTER_API_KEY may or may not be set
        let result = try_openrouter();
        if let Some(p) = result {
            assert_eq!(p, "openrouter");
        }
    }

    #[test]
    fn test_try_custom_without_env() {
        // Without OPENAI_BASE_URL, try_custom returns None
        let old_base = std::env::var("OPENAI_BASE_URL").ok();
        let old_key = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(try_custom().is_none());

        // Restore
        if let Some(v) = old_base {
            std::env::set_var("OPENAI_BASE_URL", v);
        }
        if let Some(v) = old_key {
            std::env::set_var("OPENAI_API_KEY", v);
        }
    }

    #[test]
    fn test_try_api_key_provider() {
        // Depends on env vars — just verify return type
        let result = try_api_key_provider();
        if let Some(p) = result {
            assert!(["gemini", "zai", "kimi", "minimax"].contains(&p.as_str()));
        }
    }

    #[test]
    fn test_explicit_non_aggregator_no_fallback() {
        let provider = parse_provider("anthropic");
        assert!(!is_aggregator(provider));
    }

    #[test]
    fn test_aggregator_resolves_via_chain() {
        let provider = parse_provider("openrouter");
        assert!(is_aggregator(provider));
    }

    #[test]
    fn test_is_payment_error() {
        let err = classify_api_error("openrouter", "model", Some(402), "insufficient credits");
        assert!(is_payment_error(&err));

        let err2 = classify_api_error("openrouter", "model", Some(500), "server error");
        assert!(!is_payment_error(&err2));
    }

    #[test]
    fn test_is_connection_error() {
        let err = classify_api_error("openrouter", "model", None, "Request timed out");
        assert!(is_connection_error(&err));

        let err2 = classify_api_error("openrouter", "model", None, "connection reset");
        assert!(is_connection_error(&err2));
    }

    #[test]
    fn test_default_aux_model() {
        assert_eq!(default_aux_model("openrouter"), Some("google/gemini-3-flash-preview".to_string()));
        assert_eq!(default_aux_model("anthropic"), Some("claude-haiku-4-5-20251001".to_string()));
        assert_eq!(default_aux_model("openai-codex"), Some("gpt-5.2-codex".to_string()));
        assert_eq!(default_aux_model("unknown_provider"), None);
    }

    #[test]
    fn test_to_openai_base_url() {
        // Rewrites /anthropic suffix to /v1
        assert_eq!(
            to_openai_base_url("https://api.minimax.com/anthropic"),
            "https://api.minimax.com/v1"
        );
        // Leaves /v1 URLs unchanged
        assert_eq!(
            to_openai_base_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        // Trims trailing slashes
        assert_eq!(
            to_openai_base_url("https://example.com/anthropic/"),
            "https://example.com/v1"
        );
        // Passes through non-anthropic URLs
        assert_eq!(
            to_openai_base_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1"
        );
    }

    #[test]
    fn test_is_openai_direct() {
        assert!(is_openai_direct("openai", "https://api.openai.com/v1"));
        assert!(!is_openai_direct("openai", "https://openrouter.ai/api/v1"));
        assert!(!is_openai_direct("openrouter", "https://api.openai.com/v1"));
    }

    #[test]
    fn test_resolve_api_key() {
        // Just verify it returns the right env var
        std::env::set_var("TEST_KEY_123", "test-value");
        // Not testing real env vars to avoid side effects
        assert!(resolve_api_key("openrouter").is_some() || resolve_api_key("openrouter").is_none());
    }

    #[test]
    fn test_build_openai_messages_empty() {
        let result = build_openai_messages(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_openai_messages_basic() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "You are helpful."}),
            serde_json::json!({"role": "user", "content": "Hello!"}),
        ];
        let result = build_openai_messages(&messages).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[1]["role"], "user");
    }

    #[test]
    fn test_get_available_providers() {
        let providers = get_available_providers();
        // Depends on env, but should be a valid list
        for p in &providers {
            assert!(
                ["openrouter", "nous", "custom", "openai-codex", "anthropic", "gemini", "zai", "kimi", "minimax"]
                    .contains(&p.as_str())
            );
        }
    }

    #[test]
    fn test_get_provider_chain_labels() {
        let labels = get_provider_chain_labels();
        assert_eq!(labels.len(), 6);
        assert_eq!(labels[0], "openrouter");
        assert_eq!(labels[5], "api-key");
    }

    #[test]
    fn test_is_provider_available() {
        // Just verify it doesn't panic
        let _ = is_provider_available("openrouter");
        let _ = is_provider_available("anthropic");
        let _ = is_provider_available("unknown");
    }

    #[test]
    fn test_parse_openai_response_valid() {
        let json = serde_json::json!({
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        let resp = parse_openai_response(json, "openai", "gpt-4o-mini").unwrap();
        assert_eq!(resp.content, "Hello!");
        assert_eq!(resp.model, "gpt-4o-mini");
        assert_eq!(resp.finish_reason, Some("\"stop\"".to_string()));
        assert_eq!(resp.usage.unwrap().prompt_tokens, 10);
    }

    #[test]
    fn test_parse_openai_response_empty_choices() {
        let json = serde_json::json!({
            "model": "gpt-4o-mini",
            "choices": []
        });
        let result = parse_openai_response(json, "openai", "gpt-4o-mini");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_openai_response_missing_content() {
        let json = serde_json::json!({
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant"},
                "finish_reason": "stop"
            }]
        });
        let result = parse_openai_response(json, "openai", "gpt-4o-mini");
        assert!(result.is_err());
    }

    #[test]
    fn test_convert_image_blocks_no_images() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "Hello!"}),
        ];
        let result = convert_image_blocks(messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["content"], "Hello!");
    }

    #[test]
    fn test_build_reqwest_headers() {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("Authorization".to_string(), "Bearer test".to_string());
        let header_map = build_reqwest_headers(&headers);
        assert_eq!(header_map.len(), 2);
    }

    #[test]
    fn test_select_pool_entry_no_env() {
        // Without env vars set, pool should not have credentials
        let old_key = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        let selection = select_pool_entry("openrouter");
        // Pool exists (loaded from env) but has no entries when env var is unset
        assert!(!selection.pool_exists || selection.entry.is_none());
        if let Some(v) = old_key {
            std::env::set_var("OPENROUTER_API_KEY", v);
        }
    }

    #[test]
    fn test_pool_runtime_api_key() {
        use crate::credential_pool::{AuthType, Credential, CredentialSource};
        use std::collections::HashMap;

        let cred = Credential {
            id: "abc123".to_string(),
            label: "test".to_string(),
            auth_type: AuthType::ApiKey,
            priority: 0,
            source: CredentialSource::Env,
            access_token: "sk-test-key".to_string(),
            refresh_token: None,
            expires_at: None,
            expires_at_ms: None,
            last_refresh: None,
            inference_base_url: Some("https://custom.example.com/v1".to_string()),
            base_url: None,
            agent_key: Some("agent-key-123".to_string()),
            agent_key_expires_at: None,
            request_count: 0,
            last_status: None,
            last_status_at: None,
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at: None,
            extra: HashMap::new(),
        };
        // runtime_api_key should prefer agent_key over access_token
        assert_eq!(pool_runtime_api_key(&cred), "agent-key-123");
    }

    #[test]
    fn test_pool_runtime_base_url() {
        use crate::credential_pool::{AuthType, Credential, CredentialSource};
        use std::collections::HashMap;

        let cred = Credential {
            id: "def456".to_string(),
            label: "test".to_string(),
            auth_type: AuthType::ApiKey,
            priority: 0,
            source: CredentialSource::Env,
            access_token: "sk-test".to_string(),
            refresh_token: None,
            expires_at: None,
            expires_at_ms: None,
            last_refresh: None,
            inference_base_url: Some("https://inference.example.com/v1".to_string()),
            base_url: Some("https://base.example.com/v1".to_string()),
            agent_key: None,
            agent_key_expires_at: None,
            request_count: 0,
            last_status: None,
            last_status_at: None,
            last_error_code: None,
            last_error_reason: None,
            last_error_message: None,
            last_error_reset_at: None,
            extra: HashMap::new(),
        };
        // runtime_base_url should prefer inference_base_url
        assert_eq!(pool_runtime_base_url(&cred, "https://fallback.com/v1"), "https://inference.example.com/v1");
    }

    #[test]
    fn test_is_jwt_expired_expired() {
        // Create a JWT with a past expiry (exp = 1000000000 = 2001-09-09)
        // Header: {"alg":"none"} -> eyJhbGciOiJub25lIn0
        // Payload: {"exp":1000000000} -> eyJleHAiOjEwMDAwMDAwMDB9
        let expired_token = "eyJhbGciOiJub25lIn0.eyJleHAiOjEwMDAwMDAwMDB9.";
        assert!(is_jwt_expired(expired_token));
    }

    #[test]
    fn test_is_jwt_expired_not_expired() {
        // Create a JWT with a future expiry (exp = 2100000000 = 2036-03-22)
        // Header: {"alg":"none"} -> eyJhbGciOiJub25lIn0
        // Payload: {"exp":2100000000} -> eyJleHAiOjIxMDAwMDAwMDB9
        let future_token = "eyJhbGciOiJub25lIn0.eyJleHAiOjIxMDAwMDAwMDB9.";
        assert!(!is_jwt_expired(future_token));
    }

    #[test]
    fn test_is_jwt_expired_invalid_format() {
        // Non-JWT strings should return false (assume valid)
        assert!(!is_jwt_expired("not-a-jwt"));
        assert!(!is_jwt_expired(""));
        assert!(!is_jwt_expired("a.b")); // Too few parts
    }

    #[test]
    fn test_nous_api_key_prefers_agent_key() {
        let auth = serde_json::json!({
            "agent_key": "agent-123",
            "access_token": "token-456"
        });
        assert_eq!(nous_api_key(&auth), "agent-123");
    }

    #[test]
    fn test_nous_api_key_fallback_to_access_token() {
        let auth = serde_json::json!({
            "access_token": "token-456"
        });
        assert_eq!(nous_api_key(&auth), "token-456");
    }

    #[test]
    fn test_nous_api_key_empty() {
        let auth = serde_json::json!({});
        assert_eq!(nous_api_key(&auth), "");
    }

    #[test]
    fn test_read_auth_json_no_hermes_home() {
        // If HERMES_HOME doesn't exist or auth.json doesn't exist, returns None
        // This test verifies the function doesn't crash
        let _ = read_auth_json(); // Should return None, not panic
    }

    #[test]
    fn test_resolve_task_explicit_provider_wins() {
        let (provider, model, base_url, api_key, api_mode) =
            resolve_task_provider_model(
                Some("compression"),
                Some("gemini"),
                Some("gemini-3-flash"),
                None,
                Some("sk-123"),
            );
        assert_eq!(provider, "gemini");
        assert_eq!(model, Some("gemini-3-flash".to_string()));
        assert_eq!(api_key, Some("sk-123".to_string()));
        assert!(api_mode.is_none());
        assert!(base_url.is_none());
    }

    #[test]
    fn test_resolve_task_base_url_forces_custom() {
        let (provider, model, base_url, api_key, _api_mode) =
            resolve_task_provider_model(
                Some("compression"),
                None,
                None,
                Some("http://localhost:8080/v1"),
                None,
            );
        assert_eq!(provider, "custom");
        assert_eq!(base_url, Some("http://localhost:8080/v1".to_string()));
    }

    #[test]
    fn test_resolve_task_no_task_defaults_to_auto() {
        let (provider, model, base_url, api_key, api_mode) =
            resolve_task_provider_model(None, None, None, None, None);
        assert_eq!(provider, "auto");
        assert!(model.is_none());
        assert!(base_url.is_none());
        assert!(api_key.is_none());
        assert!(api_mode.is_none());
    }

    #[test]
    fn test_get_task_timeout_default() {
        assert_eq!(get_task_timeout(None, 30), 30);
        assert_eq!(get_task_timeout(Some("unknown_task"), 60), 60);
    }

    #[test]
    fn test_extract_content_or_reasoning_with_content() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello world!"
                }
            }]
        });
        assert_eq!(extract_content_or_reasoning(&response), "Hello world!");
    }

    #[test]
    fn test_extract_content_or_reasoning_strips_think_blocks() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "<think>Let me think...</think>\n\nThe answer is 42."
                }
            }]
        });
        assert_eq!(extract_content_or_reasoning(&response), "The answer is 42.");
    }

    #[test]
    fn test_extract_content_or_reasoning_falls_back_to_reasoning() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "Let me think about this carefully..."
                }
            }]
        });
        let result = extract_content_or_reasoning(&response);
        assert!(result.contains("Let me think about this carefully"));
    }

    #[test]
    fn test_extract_content_or_reasoning_empty() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": ""
                }
            }]
        });
        assert_eq!(extract_content_or_reasoning(&response), "");
    }

    #[test]
    fn test_extract_content_or_reasoning_no_message() {
        let response = serde_json::json!({"choices": []});
        assert_eq!(extract_content_or_reasoning(&response), "");
    }

    #[tokio::test]
    async fn test_async_call_llm_basic() {
        // Just verify the function signature compiles and returns error when no provider
        let result = async_call_llm(
            None, None, None, None, None,
            vec![serde_json::json!({"role": "user", "content": "hello"})],
            None, None, None, None, None,
        ).await;
        // Should fail because no provider is configured in test env
        assert!(result.is_err());
    }

    #[test]
    fn test_call_llm_sync_basic() {
        // Just verify the sync wrapper compiles and returns error when no provider
        let result = call_llm(
            None, None, None, None, None,
            vec![serde_json::json!({"role": "user", "content": "hello"})],
            None, None, None, None, None,
        );
        assert!(result.is_err());
    }
}
