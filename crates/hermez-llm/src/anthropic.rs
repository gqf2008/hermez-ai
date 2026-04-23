#![allow(dead_code)]
//! Anthropic Messages API adapter.
//!
//! Mirrors Python `agent/anthropic_adapter.py`: Auth routing, extended thinking,
//! model output limits, beta headers, message/tool format conversion.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

// ── Model output limits ────────────────────────────────────────────────────

/// Max output token limits per Anthropic model.
/// Source: Anthropic docs + Cline model catalog.
static ANTHROPIC_OUTPUT_LIMITS: &[(&str, usize)] = &[
    // Claude 4.6
    ("claude-opus-4-6", 128_000),
    ("claude-sonnet-4-6", 64_000),
    // Claude 4.5
    ("claude-opus-4-5", 64_000),
    ("claude-sonnet-4-5", 64_000),
    ("claude-haiku-4-5", 64_000),
    // Claude 4
    ("claude-opus-4", 32_000),
    ("claude-sonnet-4", 64_000),
    // Claude 3.7
    ("claude-3-7-sonnet", 128_000),
    // Claude 3.5
    ("claude-3-5-sonnet", 8_192),
    ("claude-3-5-haiku", 8_192),
    // Claude 3
    ("claude-3-opus", 4_096),
    ("claude-3-sonnet", 4_096),
    ("claude-3-haiku", 4_096),
    // Third-party Anthropic-compatible providers
    ("minimax", 131_072),
];

const ANTHROPIC_DEFAULT_OUTPUT_LIMIT: usize = 128_000;

/// Look up the max output token limit for an Anthropic model.
///
/// Uses substring matching so date-stamped model IDs
/// (claude-sonnet-4-5-20250929) and variant suffixes (:1m, :fast)
/// resolve correctly. Longest-prefix match wins.
pub fn get_anthropic_max_output(model: &str) -> usize {
    let m = model.to_lowercase().replace('.', "-");
    let mut best_key = "";
    let mut best_val = ANTHROPIC_DEFAULT_OUTPUT_LIMIT;
    for (key, val) in ANTHROPIC_OUTPUT_LIMITS {
        if m.contains(key) && key.len() > best_key.len() {
            best_key = key;
            best_val = *val;
        }
    }
    best_val
}

// ── Model name normalization ───────────────────────────────────────────────

/// Normalize a model name for the Anthropic API.
///
/// - Strips 'anthropic/' prefix (OpenRouter format, case-insensitive)
/// - Converts dots to hyphens in version numbers (OpenRouter uses dots,
///   Anthropic uses hyphens: claude-opus-4.6 → claude-opus-4-6)
pub fn normalize_model_name(model: &str) -> String {
    let mut result = model.to_string();
    let lower = result.to_lowercase();
    if lower.starts_with("anthropic/") {
        result = result["anthropic/".len()..].to_string();
    }
    result.replace('.', "-").to_lowercase()
}

// ── Thinking support ───────────────────────────────────────────────────────

/// Thinking budget levels mapped to token counts.
static THINKING_BUDGET: &[(&str, u32)] = &[
    ("xhigh", 32_000),
    ("high", 16_000),
    ("medium", 8_000),
    ("low", 4_000),
];

/// Adaptive effort levels for Claude 4.6.
static ADAPTIVE_EFFORT_MAP: &[(&str, &str)] = &[
    ("xhigh", "max"),
    ("high", "high"),
    ("medium", "medium"),
    ("low", "low"),
    ("minimal", "low"),
];

/// Check if a model supports adaptive thinking (Claude 4.6+).
pub fn supports_adaptive_thinking(model: &str) -> bool {
    model.contains("4-6") || model.contains("4.6")
}

/// Get the thinking budget for an effort level.
pub fn thinking_budget_for_level(level: &str) -> u32 {
    THINKING_BUDGET
        .iter()
        .find(|(l, _)| *l == level.to_lowercase())
        .map(|(_, budget)| *budget)
        .unwrap_or(8_000) // default: medium
}

/// Map an effort level to the adaptive effort string.
pub fn effort_to_adaptive(effort: &str) -> &str {
    ADAPTIVE_EFFORT_MAP
        .iter()
        .find(|(l, _)| *l == effort.to_lowercase())
        .map(|(_, v)| *v)
        .unwrap_or("medium")
}

// ── Beta headers ───────────────────────────────────────────────────────────

/// Common beta headers sent with all requests.
const COMMON_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];

/// Fast mode beta — enables speed: "fast" for ~2.5x output throughput on Opus 4.6.
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

/// OAuth-only beta headers.
const OAUTH_ONLY_BETAS: &[&str] = &[
    "claude-code-20250219",
    "oauth-2025-04-20",
];

/// MiniMax's Anthropic-compatible endpoints reject tool-use requests when
/// fine-grained-tool-streaming beta is present.
const TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";

/// Get common betas safe for the configured endpoint.
pub fn common_betas_for_base_url(base_url: Option<&str>) -> Vec<String> {
    if requires_bearer_auth(base_url) {
        COMMON_BETAS
            .iter()
            .filter(|&&b| b != TOOL_STREAMING_BETA)
            .map(|&s| s.to_string())
            .collect()
    } else {
        COMMON_BETAS.iter().map(|&s| s.to_string()).collect()
    }
}

/// Get all betas including OAuth-only ones.
pub fn all_betas(base_url: Option<&str>) -> Vec<String> {
    let mut betas = common_betas_for_base_url(base_url);
    betas.extend(OAUTH_ONLY_BETAS.iter().map(|&s| s.to_string()));
    betas
}

/// Build the anthropic-beta header value from a list of beta names.
pub fn beta_header_value(betas: &[String]) -> String {
    betas.join(",")
}

// ── Auth type detection ────────────────────────────────────────────────────

/// Check if the key is an Anthropic OAuth/setup token.
///
/// - `sk-ant-api*` → Regular API keys, never OAuth
/// - `sk-ant-*` (but not `sk-ant-api*`) → setup tokens, managed keys
/// - `eyJ*` → JWTs from Anthropic OAuth flow
pub fn is_oauth_token(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    if key.starts_with("sk-ant-api") {
        return false;
    }
    if key.starts_with("sk-ant-") {
        return true;
    }
    if key.starts_with("eyJ") {
        return true;
    }
    false
}

/// Return true for non-Anthropic endpoints using the Anthropic Messages API.
pub fn is_third_party_endpoint(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else { return false };
    let normalized = url.trim().trim_end_matches('/').to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    !normalized.contains("anthropic.com")
}

/// Return true for Anthropic-compatible providers that require Bearer auth.
/// MiniMax endpoints use Bearer auth instead of x-api-key.
pub fn requires_bearer_auth(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else { return false };
    let normalized = url.trim().trim_end_matches('/').to_lowercase();
    normalized.starts_with("https://api.minimax.io/anthropic")
        || normalized.starts_with("https://api.minimaxi.com/anthropic")
}

/// Determine the auth type for the given API key and base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    /// Regular API key → x-api-key header
    ApiKey,
    /// OAuth/setup token → Bearer auth + OAuth betas + Claude Code identity
    OAuth,
    /// Third-party proxy → x-api-key (skip OAuth detection)
    ThirdParty,
    /// Bearer auth for providers like MiniMax
    Bearer,
}

pub fn detect_auth_type(api_key: &str, base_url: Option<&str>) -> AuthType {
    if requires_bearer_auth(base_url) {
        AuthType::Bearer
    } else if is_third_party_endpoint(base_url) {
        AuthType::ThirdParty
    } else if is_oauth_token(api_key) {
        AuthType::OAuth
    } else {
        AuthType::ApiKey
    }
}

// ── Claude Code credential loading ─────────────────────────────────────────

/// Read Anthropic OAuth credentials from ~/.claude/.credentials.json.
pub fn read_claude_code_credentials() -> Option<ClaudeCodeCredentials> {
    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude").join(".credentials.json");
    if !cred_path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&cred_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    let oauth_data = parsed.get("claudeAiOauth")?;
    let obj = oauth_data.as_object()?;
    let access_token = obj.get("accessToken")?.as_str()?;
    if access_token.is_empty() {
        return None;
    }
    Some(ClaudeCodeCredentials {
        access_token: access_token.to_string(),
        refresh_token: obj.get("refreshToken").and_then(|v| v.as_str()).map(String::from),
        expires_at: obj.get("expiresAt").and_then(|v| v.as_u64()),
    })
}

/// Read Claude's native managed key from ~/.claude.json (diagnostics only).
pub fn read_claude_managed_key() -> Option<String> {
    let home = dirs::home_dir()?;
    let claude_json = home.join(".claude.json");
    if !claude_json.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&claude_json).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    let primary_key = parsed.get("primaryApiKey")?.as_str()?;
    let trimmed = primary_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Claude Code OAuth credentials.
#[derive(Debug, Clone)]
pub struct ClaudeCodeCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>, // milliseconds since epoch
}

impl ClaudeCodeCredentials {
    /// Check if the access token is still valid (with 60s buffer).
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            Some(expires_ms) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                now_ms < expires_ms.saturating_sub(60_000)
            }
            None => true, // No expiry — valid if token present
        }
    }
}

/// Parsed result from an Anthropic OAuth token refresh.
#[derive(Debug, Clone)]
pub struct RefreshedToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
}

/// Refresh an Anthropic OAuth token without mutating local credential files.
///
/// Mirrors Python `refresh_anthropic_oauth_pure()`. Tries both
/// `platform.claude.com` and `console.anthropic.com` endpoints.
///
/// `use_json` controls the request body format:
/// - `true` → JSON (`application/json`)
/// - `false` → form-encoded (`application/x-www-form-urlencoded`)
pub async fn refresh_anthropic_oauth_pure(
    refresh_token: &str,
    use_json: bool,
) -> anyhow::Result<RefreshedToken> {
    if refresh_token.is_empty() {
        return Err(anyhow::anyhow!("refresh_token is required"));
    }

    let client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    let version = detect_claude_code_version();
    let user_agent = format!("claude-cli/{} (external, cli)", version);

    let endpoints = [
        "https://platform.claude.com/v1/oauth/token",
        "https://console.anthropic.com/v1/oauth/token",
    ];

    let mut last_error = None;
    for endpoint in &endpoints {
        let client = reqwest::Client::new();
        let mut req = client.post(*endpoint).header("User-Agent", &user_agent);

        if use_json {
            let body = serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": client_id,
            });
            req = req.header("Content-Type", "application/json").json(&body);
        } else {
            let params = [
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ];
            req = req
                .header("Content-Type", "application/x-www-form-urlencoded")
                .form(&params);
        }

        match req.send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(result) => {
                    let access_token = result
                        .get("access_token")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if access_token.is_empty() {
                        last_error = Some(anyhow::anyhow!("Anthropic refresh response was missing access_token"));
                        continue;
                    }
                    let next_refresh = result
                        .get("refresh_token")
                        .and_then(|v| v.as_str())
                        .unwrap_or(refresh_token)
                        .to_string();
                    let expires_in = result
                        .get("expires_in")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(3600);
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    return Ok(RefreshedToken {
                        access_token: access_token.to_string(),
                        refresh_token: next_refresh,
                        expires_at_ms: now_ms + (expires_in * 1000),
                    });
                }
                Err(e) => {
                    last_error = Some(anyhow::anyhow!("Failed to parse response: {e}"));
                    continue;
                }
            },
            Err(e) => {
                tracing::debug!("Anthropic token refresh failed at {}: {}", endpoint, e);
                last_error = Some(anyhow::anyhow!("Request failed: {e}"));
                continue;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Anthropic token refresh failed")))
}

/// Write refreshed credentials back to ~/.claude/.credentials.json.
///
/// Mirrors Python `_write_claude_code_credentials()`. Preserves existing
/// fields and optionally updates scopes.
pub fn write_claude_code_credentials(
    access_token: &str,
    refresh_token: &str,
    expires_at_ms: u64,
    scopes: Option<&[String]>,
) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let cred_path = home.join(".claude").join(".credentials.json");

    // Read existing file to preserve other fields
    let mut existing: serde_json::Value = if cred_path.exists() {
        let data = std::fs::read_to_string(&cred_path)
            .map_err(|e| anyhow::anyhow!("Failed to read credentials: {e}"))?;
        serde_json::from_str(&data)
            .map_err(|e| anyhow::anyhow!("Failed to parse credentials: {e}"))?
    } else {
        serde_json::json!({})
    };

    let mut oauth_data = serde_json::Map::new();
    oauth_data.insert("accessToken".to_string(), serde_json::json!(access_token));
    oauth_data.insert("refreshToken".to_string(), serde_json::json!(refresh_token));
    oauth_data.insert("expiresAt".to_string(), serde_json::json!(expires_at_ms));

    // Handle scopes
    if let Some(s) = scopes {
        oauth_data.insert("scopes".to_string(), serde_json::json!(s));
    } else if let Some(existing_oauth) = existing.get("claudeAiOauth") {
        if let Some(existing_scopes) = existing_oauth.get("scopes") {
            oauth_data.insert("scopes".to_string(), existing_scopes.clone());
        }
    }

    if let Some(obj) = existing.as_object_mut() {
        obj.insert("claudeAiOauth".to_string(), serde_json::Value::Object(oauth_data));
    }

    // Ensure parent directory exists
    if let Some(parent) = cred_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create directory: {e}"))?;
    }

    let output = serde_json::to_string_pretty(&existing)
        .map_err(|e| anyhow::anyhow!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, output)
        .map_err(|e| anyhow::anyhow!("Failed to write credentials: {e}"))?;

    // Restrict permissions (credentials file) — Unix only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&cred_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&cred_path, perms);
        }
    }

    Ok(())
}

/// Attempt to refresh an expired Claude Code OAuth token.
/// Returns the new access token on success.
pub async fn try_refresh_oauth_token(
    refresh_token: &str,
) -> Option<String> {
    if refresh_token.is_empty() {
        tracing::debug!("No refresh token available — cannot refresh");
        return None;
    }

    match refresh_anthropic_oauth_pure(refresh_token, false).await {
        Ok(refreshed) => {
            let scopes = None; // Preserve existing scopes during refresh
            if let Err(e) = write_claude_code_credentials(
                &refreshed.access_token,
                &refreshed.refresh_token,
                refreshed.expires_at_ms,
                scopes,
            ) {
                tracing::debug!("Failed to write refreshed credentials: {}", e);
            }
            tracing::debug!("Successfully refreshed Claude Code OAuth token");
            Some(refreshed.access_token)
        }
        Err(e) => {
            tracing::debug!("Failed to refresh Claude Code token: {}", e);
            None
        }
    }
}

/// Resolve a token from Claude Code credential files, refreshing if needed.
///
/// Priority: valid creds → expired creds with refresh → None.
/// Returns (access_token, is_oauth=true).
pub async fn resolve_claude_code_token_with_refresh() -> Option<(String, bool)> {
    let creds = read_claude_code_credentials()?;

    if creds.is_valid() {
        tracing::debug!("Using Claude Code credentials (auto-detected)");
        return Some((creds.access_token, true));
    }

    tracing::debug!("Claude Code credentials expired — attempting refresh");
    if let Some(refresh_token) = &creds.refresh_token {
        if let Some(new_token) = try_refresh_oauth_token(refresh_token).await {
            return Some((new_token, true));
        }
        tracing::debug!("Token refresh failed — re-run 'claude setup-token' to reauthenticate");
    }

    None
}

/// Prefer Claude Code creds when a persisted env OAuth token would shadow refresh.
///
/// Hermez historically persisted setup tokens into ANTHROPIC_TOKEN. That makes
/// later refresh impossible because the static env token wins before we ever
/// inspect Claude Code's refreshable credential file. If we have a refreshable
/// Claude Code credential record, prefer it over the static env OAuth token.
pub async fn prefer_refreshable_claude_code_token(
    env_token: &str,
) -> Option<(String, bool)> {
    if env_token.is_empty() || !is_oauth_token(env_token) {
        return None;
    }

    let creds = read_claude_code_credentials()?;
    if creds.refresh_token.is_none() || creds.refresh_token.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
        return None;
    }

    if let Some((resolved, is_oauth)) = resolve_claude_code_token_with_refresh().await {
        if resolved != env_token {
            tracing::debug!(
                "Preferring Claude Code credential file over static env OAuth token so refresh can proceed"
            );
            return Some((resolved, is_oauth));
        }
    }

    None
}

/// Resolve an Anthropic token from all available sources.
///
/// Priority:
///   1. ANTHROPIC_TOKEN env var
///   2. CLAUDE_CODE_OAUTH_TOKEN env var
///   3. Claude Code credentials (~/.claude/.credentials.json)
///   4. ANTHROPIC_API_KEY env var (regular API key)
///
/// Returns (token, is_oauth).
pub fn resolve_anthropic_token() -> Option<(String, bool)> {
    // 1. ANTHROPIC_TOKEN
    if let Ok(token) = std::env::var("ANTHROPIC_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let is_oauth = is_oauth_token(&token);
            return Some((token, is_oauth));
        }
    }

    // 2. CLAUDE_CODE_OAUTH_TOKEN
    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let is_oauth = is_oauth_token(&token);
            return Some((token, is_oauth));
        }
    }

    // 3. Claude Code credentials
    if let Some(creds) = read_claude_code_credentials() {
        return Some((creds.access_token, true));
    }

    // 4. ANTHROPIC_API_KEY
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            let is_oauth = is_oauth_token(&key);
            return Some((key, is_oauth));
        }
    }

    None
}

// ── Tool conversion ────────────────────────────────────────────────────────

/// Sanitize a tool call ID for the Anthropic API.
/// Anthropic requires IDs matching [a-zA-Z0-9_-].
pub fn sanitize_tool_id(tool_id: &str) -> String {
    static INVALID_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-zA-Z0-9_-]").unwrap());
    if tool_id.is_empty() {
        return "tool_0".to_string();
    }
    let sanitized = INVALID_CHARS.replace_all(tool_id, "_").to_string();
    if sanitized.is_empty() {
        "tool_0".to_string()
    } else {
        sanitized
    }
}

/// Convert OpenAI tool definitions to Anthropic format.
pub fn convert_tools_to_anthropic(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let fn_def = t.get("function").unwrap_or(t);
            json!({
                "name": fn_def.get("name").and_then(Value::as_str).unwrap_or(""),
                "description": fn_def.get("description").and_then(Value::as_str).unwrap_or(""),
                "input_schema": fn_def.get("parameters").cloned().unwrap_or_else(|| json!({
                    "type": "object", "properties": {}
                })),
            })
        })
        .collect()
}

// ── Image content handling ─────────────────────────────────────────────────

/// Convert an OpenAI-style image URL/data URL to an Anthropic image source.
fn image_source_from_openai_url(url: &str) -> Value {
    let url = url.trim();
    if url.starts_with("data:") {
        let media_type = if let Some(comma_pos) = url.find(',') {
            let header = &url[..comma_pos];
            let mime_part = header
                .strip_prefix("data:")
                .and_then(|s| s.split(';').next())
                .unwrap_or("image/jpeg");
            if mime_part.starts_with("image/") {
                mime_part.to_string()
            } else {
                "image/jpeg".to_string()
            }
        } else {
            "image/jpeg".to_string()
        };
        let data = url.find(',').map(|i| &url[i + 1..]).unwrap_or("");
        json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        })
    } else {
        json!({
            "type": "url",
            "url": url,
        })
    }
}

/// Convert a single OpenAI-style content part to Anthropic format.
fn convert_content_part(part: &Value) -> Option<Value> {
    if let Some(text) = part.as_str() {
        return Some(json!({"type": "text", "text": text}));
    }
    if !part.is_object() {
        return Some(json!({"type": "text", "text": part.to_string()}));
    }
    let obj = part.as_object().unwrap();
    match obj.get("type").and_then(Value::as_str) {
        Some("input_text") => {
            Some(json!({"type": "text", "text": obj.get("text").and_then(Value::as_str).unwrap_or("")}))
        }
        Some("image_url") | Some("input_image") => {
            let image_value = obj.get("image_url").unwrap_or(part);
            let url = if let Some(img_obj) = image_value.as_object() {
                img_obj.get("url").and_then(Value::as_str).unwrap_or("").to_string()
            } else {
                image_value.as_str().unwrap_or("").to_string()
            };
            Some(json!({
                "type": "image",
                "source": image_source_from_openai_url(&url),
            }))
        }
        _ => Some(part.clone()),
    }
}

/// Convert OpenAI-style multimodal content array to Anthropic blocks.
pub fn convert_content_to_anthropic(content: &Value) -> Value {
    if let Some(arr) = content.as_array() {
        let converted: Vec<Value> = arr
            .iter()
            .filter_map(convert_content_part)
            .collect();
        Value::Array(converted)
    } else {
        content.clone()
    }
}

// ── Message conversion ─────────────────────────────────────────────────────

/// Extract preserved thinking blocks from an assistant message.
fn extract_thinking_blocks(message: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(raw_details) = message.get("reasoning_details").and_then(|v| v.as_array()) {
        for detail in raw_details {
            if let Some(obj) = detail.as_object() {
                let block_type = obj
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                if block_type == "thinking" || block_type == "redacted_thinking" {
                    blocks.push(detail.clone());
                }
            }
        }
    }
    blocks
}

/// Convert OpenAI-format messages to Anthropic format.
///
/// Returns (system_prompt, anthropic_messages).
/// System messages are extracted since Anthropic takes them as a separate param.
pub fn convert_messages(messages: &[Value], strip_signatures: bool) -> (Option<Value>, Vec<Value>) {
    let mut system: Option<Value> = None;
    let mut result = Vec::new();

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);

        match role {
            "system" => {
                if let Some(arr) = content.as_array() {
                    // Check for cache_control markers
                    let has_cache = arr.iter().any(|p| {
                        p.get("cache_control").is_some()
                    });
                    if has_cache {
                        system = Some(Value::Array(
                            arr.iter().filter(|p| p.is_object()).cloned().collect(),
                        ));
                    } else {
                        let texts: Vec<String> = arr
                            .iter()
                            .filter_map(|p| {
                                p.get("type")
                                    .and_then(|t| t.as_str())
                                    .filter(|&t| t == "text")
                                    .and_then(|_| p.get("text").and_then(Value::as_str))
                                    .map(String::from)
                            })
                            .collect();
                        system = Some(Value::String(texts.join("\n")));
                    }
                } else {
                    system = Some(content);
                }
            }
            "assistant" => {
                let mut blocks = extract_thinking_blocks(m);

                if strip_signatures {
                    blocks = blocks.into_iter().map(|mut b| {
                        if let Some(obj) = b.as_object_mut() {
                            obj.remove("signature");
                        }
                        b
                    }).collect();
                }

                // Add text content
                if let Some(arr) = content.as_array() {
                    let converted = convert_content_to_anthropic(&Value::Array(arr.clone()));
                    if let Some(arr) = converted.as_array() {
                        blocks.extend(arr.clone());
                    }
                } else if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }

                // Add tool calls
                if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if let Some(fn_def) = tc.get("function") {
                            let args = fn_def.get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .unwrap_or_else(|| json!({}));
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": sanitize_tool_id(tc.get("id").and_then(Value::as_str).unwrap_or("")),
                                "name": fn_def.get("name").and_then(Value::as_str).unwrap_or(""),
                                "input": args,
                            }));
                        }
                    }
                }

                // Anthropic rejects empty assistant content
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": "(empty)"}));
                }
                result.push(json!({
                    "role": "assistant",
                    "content": Value::Array(blocks),
                }));
            }
            "tool" => {
                let content_str = content.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| {
                        content.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| "(no output)".to_string())
                    });
                let content_str = if content_str.is_empty() {
                    "(no output)".to_string()
                } else {
                    content_str
                };
                let mut tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": sanitize_tool_id(
                        m.get("tool_call_id").and_then(Value::as_str).unwrap_or("")
                    ),
                    "content": content_str,
                });
                if let Some(cache_ctrl) = m.get("cache_control") {
                    if let Some(obj) = tool_result.as_object_mut() {
                        obj.insert("cache_control".to_string(), cache_ctrl.clone());
                    }
                }

                // Merge consecutive tool results into one user message
                if let Some(last) = result.last_mut() {
                    if last.get("role").and_then(Value::as_str) == Some("user") {
                        if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                            arr.push(tool_result);
                            continue;
                        }
                    }
                }
                result.push(json!({
                    "role": "user",
                    "content": Value::Array(vec![tool_result]),
                }));
            }
            _ => {
                // user or unknown — treat as user
                let converted = if content.is_array() {
                    convert_content_to_anthropic(&content)
                } else if let Some(text) = content.as_str() {
                    Value::Array(vec![json!({"type": "text", "text": text})])
                } else {
                    Value::Array(vec![json!({"type": "text", "text": content.to_string()})])
                };
                result.push(json!({
                    "role": "user",
                    "content": converted,
                }));
            }
        }
    }

    (system, result)
}

// ── Request builder ────────────────────────────────────────────────────────

/// Build the Anthropic API request body as JSON.
pub struct AnthropicRequestBuilder {
    pub model: String,
    pub messages: Vec<Value>,
    pub system_prompt: Option<Value>,
    pub max_tokens: usize,
    pub temperature: Option<f64>,
    pub tools: Option<Vec<Value>>,
    pub api_key: String,
    pub base_url: Option<String>,
    pub thinking_enabled: bool,
    pub thinking_effort: Option<String>, // "low", "medium", "high", "xhigh"
    pub fast_mode: bool,
    pub stream: bool,
}

impl AnthropicRequestBuilder {
    /// Build the request body, headers, and URL.
    pub fn build(&self) -> (String, HashMap<String, String>, String) {
        let model = normalize_model_name(&self.model);
        let base_url = self.base_url.as_deref();

        // Determine auth
        let auth_type = detect_auth_type(&self.api_key, base_url);
        let betas = match auth_type {
            AuthType::OAuth => {
                let mut all = common_betas_for_base_url(base_url);
                all.extend(OAUTH_ONLY_BETAS.iter().map(|&s| s.to_string()));
                all
            }
            _ => common_betas_for_base_url(base_url),
        };

        // Build body
        let mut body = json!({
            "model": model,
            "messages": self.messages,
            "max_tokens": self.max_tokens,
        });

        // System prompt (can be string or array of content blocks for cache_control)
        if let Some(ref sys) = self.system_prompt {
            body["system"] = sys.clone();
        }

        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }

        // Tool definitions
        if let Some(ref tools) = self.tools {
            if !tools.is_empty() {
                body["tools"] = Value::Array(convert_tools_to_anthropic(tools));
            }
        }

        // Extended thinking (Claude 4.5+)
        if self.thinking_enabled {
            let budget = if let Some(ref effort) = self.thinking_effort {
                thinking_budget_for_level(effort)
            } else {
                8_000 // default medium
            };
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
            // Adaptive effort for 4.6 models
            if supports_adaptive_thinking(&model) {
                if let Some(ref effort) = self.thinking_effort {
                    body["effort"] = json!(effort_to_adaptive(effort));
                }
            }
        }

        // Streaming
        if self.stream {
            body["stream"] = json!(true);
        }

        // Fast mode (Claude 4.6+ Opus/Sonnet)
        if self.fast_mode && supports_adaptive_thinking(&model) {
            body["speed"] = json!("fast");
            // Add fast mode beta header
        }

        let body_str = serde_json::to_string(&body).unwrap_or_default();

        // Build headers
        let mut headers = HashMap::new();
        headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
        headers.insert("content-type".to_string(), "application/json".to_string());

        // Beta header
        let mut final_betas = betas;
        if self.fast_mode {
            final_betas.push(FAST_MODE_BETA.to_string());
        }
        if !final_betas.is_empty() {
            headers.insert("anthropic-beta".to_string(), beta_header_value(&final_betas));
        }

        // Auth header
        match auth_type {
            AuthType::ApiKey | AuthType::ThirdParty => {
                if !self.api_key.is_empty() {
                    headers.insert("x-api-key".to_string(), self.api_key.clone());
                }
            }
            AuthType::OAuth => {
                headers.insert("authorization".to_string(), format!("Bearer {}", self.api_key));
                // Claude Code identity headers (required for OAuth routing)
                let version = detect_claude_code_version();
                headers.insert(
                    "user-agent".to_string(),
                    format!("claude-cli/{} (external, cli)", version),
                );
                headers.insert("x-app".to_string(), "cli".to_string());
            }
            AuthType::Bearer => {
                headers.insert("authorization".to_string(), format!("Bearer {}", self.api_key));
            }
        }

        // Build URL
        let base = self.base_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("https://api.anthropic.com");
        let url = format!("{}/v1/messages", base.trim_end_matches('/'));

        (body_str, headers, url)
    }
}

// ── Claude Code version detection ──────────────────────────────────────────

static CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";
static CACHED_CLAUDE_VERSION: Lazy<String> = Lazy::new(detect_claude_code_version_impl);

fn detect_claude_code_version_impl() -> String {
    for cmd in &["claude", "claude-code"] {
        if let Ok(output) = std::process::Command::new(cmd)
            .arg("--version")
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let version = stdout.split_whitespace().next().unwrap_or("");
                if !version.is_empty() && version.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    return version.to_string();
                }
            }
        }
    }
    CLAUDE_CODE_VERSION_FALLBACK.to_string()
}

/// Get the detected Claude Code version.
pub fn detect_claude_code_version() -> String {
    CACHED_CLAUDE_VERSION.clone()
}

// ── OAuth credential resolution (enhanced resolve_anthropic_token) ───────

/// Enhanced token resolution that tries OAuth refresh from Claude Code credentials.
///
/// Mirrors Python's combined resolution: env vars → credentials file (with refresh) → API key.
/// Returns (token, is_oauth).
pub async fn resolve_anthropic_token_async() -> Option<(String, bool)> {
    // 1. ANTHROPIC_TOKEN
    if let Ok(token) = std::env::var("ANTHROPIC_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let is_oauth = is_oauth_token(&token);
            // Check if Claude Code has a refreshable credential that should take priority
            if is_oauth {
                if let Some((resolved, _)) = prefer_refreshable_claude_code_token(&token).await {
                    return Some((resolved, true));
                }
            }
            return Some((token, is_oauth));
        }
    }

    // 2. CLAUDE_CODE_OAUTH_TOKEN
    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let is_oauth = is_oauth_token(&token);
            if is_oauth {
                if let Some((resolved, _)) = prefer_refreshable_claude_code_token(&token).await {
                    return Some((resolved, true));
                }
            }
            return Some((token, is_oauth));
        }
    }

    // 3. Claude Code credentials (with auto-refresh)
    if let Some(result) = resolve_claude_code_token_with_refresh().await {
        return Some(result);
    }

    // 4. ANTHROPIC_API_KEY
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            let is_oauth = is_oauth_token(&key);
            return Some((key, is_oauth));
        }
    }

    None
}

// ── Message preprocessing ────────────────────────────────────────────────

/// Thinking block types that require signature management.
const THINKING_TYPES: &[&str] = &["thinking", "redacted_thinking"];

/// Preprocess Anthropic messages for API submission.
///
/// Mirrors Python's post-conversion processing:
/// - Strip orphaned tool_use blocks (no matching tool_result follows)
/// - Strip orphaned tool_result blocks (no matching tool_use precedes them)
/// - Enforce strict role alternation (merge consecutive same-role messages)
/// - Strip thinking block signatures for third-party endpoints
/// - Validate non-empty content
///
/// Returns the preprocessed messages.
pub fn preprocess_anthropic_messages(
    messages: Vec<Value>,
    base_url: Option<&str>,
) -> Vec<Value> {
    let is_third_party = is_third_party_endpoint(base_url);
    let mut result = messages;

    // Strip orphaned tool_use blocks (no matching tool_result follows)
    let mut tool_result_ids = std::collections::HashSet::new();
    for m in &result {
        if m.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(arr) = m.get("content").and_then(Value::as_array) {
                for block in arr {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        tool_result_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    for m in &mut result {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(content) = m.get_mut("content") {
                if let Some(arr) = content.as_array_mut() {
                    *arr = arr
                        .drain(..)
                        .filter(|b: &Value| {
                            b.get("type").and_then(Value::as_str) != Some("tool_use")
                                || b.get("id").and_then(Value::as_str).map(|s| tool_result_ids.contains(s)).unwrap_or(true)
                        })
                        .collect();
                    if arr.is_empty() {
                        arr.push(json!({"type": "text", "text": "(tool call removed)"}));
                    }
                }
            }
        }
    }

    // Strip orphaned tool_result blocks (no matching tool_use precedes them)
    let mut tool_use_ids = std::collections::HashSet::new();
    for m in &result {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(arr) = m.get("content").and_then(Value::as_array) {
                for block in arr {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        tool_use_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    for m in &mut result {
        if m.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(content) = m.get_mut("content") {
                if let Some(arr) = content.as_array_mut() {
                    *arr = arr
                        .drain(..)
                        .filter(|b: &Value| {
                            b.get("type").and_then(Value::as_str) != Some("tool_result")
                                || b.get("tool_use_id").and_then(Value::as_str).map(|s| tool_use_ids.contains(s)).unwrap_or(true)
                        })
                        .collect();
                    if arr.is_empty() {
                        arr.push(json!({"type": "text", "text": "(tool result removed)"}));
                    }
                }
            }
        }
    }

    // Enforce strict role alternation (merge consecutive same-role messages)
    let mut fixed: Vec<Value> = Vec::new();
    for m in result {
        if let Some(last) = fixed.last() {
            let last_role = last.get("role").and_then(Value::as_str).unwrap_or("");
            let curr_role = m.get("role").and_then(Value::as_str).unwrap_or("");
            if last_role == curr_role {
                let last_idx = fixed.len() - 1;
                if curr_role == "user" {
                    // Merge consecutive user messages
                    let prev = fixed[last_idx]["content"].clone();
                    let curr = m["content"].clone();
                    fixed[last_idx]["content"] = merge_content_blocks(&prev, &curr);
                } else {
                    // Consecutive assistant messages — merge text, drop thinking from second
                    let mut curr_content = m["content"].clone();
                    if let Some(arr) = curr_content.as_array_mut() {
                        *arr = arr
                            .drain(..)
                            .filter(|b: &Value| {
                                !b.get("type").and_then(Value::as_str).map(|t| THINKING_TYPES.contains(&t)).unwrap_or(false)
                            })
                            .collect();
                    }
                    let prev = fixed[last_idx]["content"].clone();
                    fixed[last_idx]["content"] = merge_content_blocks(&prev, &curr_content);
                }
                continue;
            }
        }
        fixed.push(m);
    }
    result = fixed;

    // Strip thinking blocks for third-party endpoints (signatures are Anthropic-proprietary)
    if is_third_party {
        for m in &mut result {
            if m.get("role").and_then(Value::as_str) == Some("assistant") {
                if let Some(content) = m.get_mut("content") {
                    if let Some(arr) = content.as_array_mut() {
                        // Extract thinking text before removing
                        let mut thinking_texts = Vec::new();
                        for b in arr.iter() {
                            if let Some(obj) = b.as_object() {
                                if let Some(t) = obj.get("type").and_then(Value::as_str) {
                                    if THINKING_TYPES.contains(&t) {
                                        if let Some(thinking) = obj.get("thinking").and_then(Value::as_str) {
                                            if !thinking.is_empty() {
                                                thinking_texts.push(thinking.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Remove thinking/redacted_thinking blocks
                        *arr = arr
                            .drain(..)
                            .filter(|b: &Value| {
                                !b.get("type").and_then(Value::as_str).map(|t| THINKING_TYPES.contains(&t)).unwrap_or(false)
                            })
                            .collect();
                        // Preserve thinking text as regular text
                        for text in thinking_texts {
                            arr.insert(0, json!({"type": "text", "text": text}));
                        }
                        if arr.is_empty() {
                            arr.push(json!({"type": "text", "text": "(empty)"}));
                        }
                    }
                }
            }
        }
    }

    // Strip cache_control from thinking blocks — cache markers interfere with signature validation
    for m in &mut result {
        if let Some(content) = m.get_mut("content") {
            if let Some(arr) = content.as_array_mut() {
                for block in arr.iter_mut() {
                    if let Some(obj) = block.as_object_mut() {
                        if let Some(t) = obj.get("type").and_then(|v: &Value| v.as_str()) {
                            if THINKING_TYPES.contains(&t) {
                                obj.remove("cache_control");
                            }
                        }
                    }
                }
            }
        }
    }

    result
}

/// Merge two content blocks (string, array, or mixed).
fn merge_content_blocks(prev: &Value, curr: &Value) -> Value {
    match (prev, curr) {
        (Value::String(a), Value::String(b)) => Value::String(format!("{}\n{}", a, b)),
        (Value::Array(a), Value::Array(b)) => {
            let mut merged = a.clone();
            merged.extend(b.clone());
            Value::Array(merged)
        }
        _ => {
            // Mixed types — normalize both to list and merge
            let to_array = |v: &Value| -> Vec<Value> {
                if let Some(arr) = v.as_array() {
                    arr.clone()
                } else if let Some(s) = v.as_str() {
                    vec![json!({"type": "text", "text": s})]
                } else {
                    vec![json!({"type": "text", "text": v.to_string()})]
                }
            };
            let mut merged = to_array(prev);
            merged.extend(to_array(curr));
            Value::Array(merged)
        }
    }
}

// ── Response normalization ───────────────────────────────────────────────

/// Normalized Anthropic response.
///
/// Mirrors Python `normalize_anthropic_response()`. Maps Anthropic's
/// response shape to the internal dict format expected by AIAgent.
#[derive(Debug, Clone)]
pub struct NormalizedAnthropicResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<Value>>,
    pub reasoning: Option<String>,
    pub reasoning_details: Option<Vec<Value>>,
    pub finish_reason: String,
    pub usage: Option<Value>,
}

/// Normalize an Anthropic API response JSON to internal format.
///
/// Maps Anthropic stop_reason to OpenAI finish_reason:
/// - `end_turn` → `stop`
/// - `tool_use` → `tool_calls`
/// - `max_tokens` → `length`
/// - `stop_sequence` → `stop`
///
/// When `strip_tool_prefix` is true, removes the `mcp_` prefix from tool names
/// (used for OAuth Claude Code compatibility).
pub fn normalize_anthropic_response(
    response: &Value,
    strip_tool_prefix: bool,
) -> NormalizedAnthropicResponse {
    let mcp_prefix = "mcp_";
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut reasoning_details: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(content_arr) = response.get("content").and_then(Value::as_array) {
        for block in content_arr {
            if let Some(obj) = block.as_object() {
                match obj.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("thinking") => {
                        if let Some(thinking) = obj.get("thinking").and_then(Value::as_str) {
                            reasoning_parts.push(thinking.to_string());
                        }
                        reasoning_details.push(block.clone());
                    }
                    Some("tool_use") => {
                        let mut name = obj
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if strip_tool_prefix && name.starts_with(mcp_prefix) {
                            name = name[mcp_prefix.len()..].to_string();
                        }
                        let tool_call = json!({
                            "id": obj.get("id").and_then(Value::as_str).unwrap_or(""),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": obj.get("input").cloned().unwrap_or_else(|| json!({})).to_string(),
                            }
                        });
                        tool_calls.push(tool_call);
                    }
                    _ => {}
                }
            }
        }
    }

    // Map Anthropic stop_reason to OpenAI finish_reason
    let finish_reason = match response.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") => "stop".to_string(),
        Some("tool_use") => "tool_calls".to_string(),
        Some("max_tokens") => "length".to_string(),
        Some("stop_sequence") => "stop".to_string(),
        _ => "stop".to_string(),
    };

    let usage = response.get("usage").cloned();

    NormalizedAnthropicResponse {
        content: if text_parts.is_empty() { None } else { Some(text_parts.join("\n")) },
        tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
        reasoning: if reasoning_parts.is_empty() {
            None
        } else {
            Some(reasoning_parts.join("\n\n"))
        },
        reasoning_details: if reasoning_details.is_empty() {
            None
        } else {
            Some(reasoning_details)
        },
        finish_reason,
        usage,
    }
}

/// Convert normalized response back to a serde_json::Value dict.
pub fn normalized_response_to_value(resp: &NormalizedAnthropicResponse) -> Value {
    let mut result = serde_json::Map::new();
    if let Some(ref content) = resp.content {
        result.insert("content".to_string(), json!(content));
    }
    if let Some(ref tool_calls) = resp.tool_calls {
        result.insert("tool_calls".to_string(), json!(tool_calls));
    }
    if let Some(ref reasoning) = resp.reasoning {
        result.insert("reasoning".to_string(), json!(reasoning));
    }
    if let Some(ref details) = resp.reasoning_details {
        result.insert("reasoning_details".to_string(), json!(details));
    }
    result.insert("finish_reason".to_string(), json!(resp.finish_reason));
    if let Some(ref usage) = resp.usage {
        result.insert("usage".to_string(), usage.clone());
    }
    Value::Object(result)
}

// ── Error context extraction ─────────────────────────────────────────────

/// Parse available output tokens from an Anthropic API error message.
///
/// Anthropic returns errors like:
/// "Your max_tokens setting of 16000 is too large for the given context size.
///  Available output tokens: 8192 (20480 context - 12288 used in prompt)"
///
/// Returns the available output tokens if extractable.
pub fn parse_available_output_tokens_from_error(error_message: &str) -> Option<usize> {
    static AVAILABLE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"[Aa]vailable output tokens:\s*(\d+)").unwrap()
    });
    AVAILABLE_RE
        .captures(error_message)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse::<usize>().ok())
}

/// Parse the max_tokens value from an Anthropic API error message.
///
/// Returns the max_tokens value that was rejected.
pub fn parse_max_tokens_from_error(error_message: &str) -> Option<usize> {
    static MAX_TOKENS_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"max_tokens.*?(\d+)").unwrap()
    });
    MAX_TOKENS_RE
        .captures(error_message)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse::<usize>().ok())
}

/// Compute an ephemeral max_output_tokens value from context length and prompt size.
///
/// When Anthropic rejects a request due to context overflow, compute a safe
/// retry value. Mirrors Python's _ephemeral_max_output_tokens logic.
pub fn ephemeral_max_output_tokens(
    context_length: usize,
    prompt_tokens: usize,
    buffer: usize,
) -> usize {
    let available = context_length.saturating_sub(prompt_tokens);
    available.saturating_sub(buffer).max(1)
}

/// Extract structured error context from an Anthropic API error.
///
/// Returns a map with keys like:
/// - "error_type": classification of the error
/// - "retryable": whether the error is likely retryable
/// - "available_output_tokens": if context overflow, how many tokens remain
/// - "suggested_max_tokens": if context overflow, a safe retry value
#[derive(Debug, Default)]
pub struct AnthropicErrorContext {
    pub error_type: String,
    pub retryable: bool,
    pub available_output_tokens: Option<usize>,
    pub suggested_max_tokens: Option<usize>,
    pub raw_message: Option<String>,
}

impl AnthropicErrorContext {
    /// Parse an Anthropic error message into structured context.
    pub fn from_error(status_code: u16, error_message: &str) -> Self {
        let mut ctx = AnthropicErrorContext {
            raw_message: Some(error_message.to_string()),
            ..Default::default()
        };

        // Context overflow — retry with smaller max_tokens
        if error_message.contains("max_tokens")
            && (error_message.contains("too large") || error_message.contains("context"))
        {
            ctx.error_type = "context_overflow".to_string();
            ctx.retryable = true;
            ctx.available_output_tokens = parse_available_output_tokens_from_error(error_message);
        }
        // Rate limit
        else if status_code == 429
            || error_message.contains("rate limit")
            || error_message.contains("overloaded")
        {
            ctx.error_type = "rate_limit".to_string();
            ctx.retryable = true;
        }
        // Invalid thinking signature — retry without signatures
        else if error_message.contains("Invalid signature")
            || error_message.contains("thinking block")
        {
            ctx.error_type = "invalid_thinking_signature".to_string();
            ctx.retryable = true;
        }
        // Server error — retryable
        else if status_code >= 500 {
            ctx.error_type = "server_error".to_string();
            ctx.retryable = true;
        }
        // Authentication error — not retryable
        else if status_code == 401 || status_code == 403 {
            ctx.error_type = "auth_error".to_string();
            ctx.retryable = false;
        }
        // Invalid request — not retryable
        else if status_code == 400 {
            ctx.error_type = "invalid_request".to_string();
            ctx.retryable = false;
        }
        // Unknown
        else {
            ctx.error_type = "unknown".to_string();
            ctx.retryable = status_code >= 500;
        }

        ctx
    }

    /// Convert to a serde_json::Value for structured logging.
    pub fn to_value(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("error_type".to_string(), json!(self.error_type));
        map.insert("retryable".to_string(), json!(self.retryable));
        if let Some(tokens) = self.available_output_tokens {
            map.insert("available_output_tokens".to_string(), json!(tokens));
        }
        if let Some(tokens) = self.suggested_max_tokens {
            map.insert("suggested_max_tokens".to_string(), json!(tokens));
        }
        if let Some(ref msg) = self.raw_message {
            map.insert("raw_message".to_string(), json!(msg));
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_model_name() {
        assert_eq!(normalize_model_name("anthropic/claude-opus-4.6"), "claude-opus-4-6");
        assert_eq!(normalize_model_name("claude-sonnet-4.5"), "claude-sonnet-4-5");
        assert_eq!(normalize_model_name("Anthropic/Claude-3-Opus"), "claude-3-opus");
    }

    #[test]
    fn test_output_limits() {
        assert_eq!(get_anthropic_max_output("claude-opus-4-6"), 128_000);
        assert_eq!(get_anthropic_max_output("claude-sonnet-4-5-20250929"), 64_000);
        assert_eq!(get_anthropic_max_output("claude-3-opus"), 4_096);
        assert_eq!(get_anthropic_max_output("unknown-model"), ANTHROPIC_DEFAULT_OUTPUT_LIMIT);
    }

    #[test]
    fn test_oauth_token_detection() {
        assert!(!is_oauth_token("sk-ant-api03-xxx"));
        assert!(is_oauth_token("sk-ant-oat01-xxx"));
        assert!(is_oauth_token("sk-ant-something"));
        assert!(is_oauth_token("eyJhbGciOi..."));
        assert!(!is_oauth_token(""));
        assert!(!is_oauth_token("some-other-key"));
    }

    #[test]
    fn test_third_party_endpoint_detection() {
        assert!(!is_third_party_endpoint(None));
        assert!(!is_third_party_endpoint(Some("https://api.anthropic.com")));
        assert!(is_third_party_endpoint(Some("https://ai.azure.com/anthropic")));
        assert!(is_third_party_endpoint(Some("https://api.minimax.io/anthropic")));
    }

    #[test]
    fn test_bearer_auth_detection() {
        assert!(requires_bearer_auth(Some("https://api.minimax.io/anthropic")));
        assert!(requires_bearer_auth(Some("https://api.minimaxi.com/anthropic")));
        assert!(!requires_bearer_auth(None));
        assert!(!requires_bearer_auth(Some("https://api.anthropic.com")));
    }

    #[test]
    fn test_thinking_budget() {
        assert_eq!(thinking_budget_for_level("xhigh"), 32_000);
        assert_eq!(thinking_budget_for_level("high"), 16_000);
        assert_eq!(thinking_budget_for_level("medium"), 8_000);
        assert_eq!(thinking_budget_for_level("low"), 4_000);
        assert_eq!(thinking_budget_for_level("unknown"), 8_000);
    }

    #[test]
    fn test_adaptive_thinking() {
        assert!(supports_adaptive_thinking("claude-opus-4-6"));
        assert!(supports_adaptive_thinking("claude-sonnet-4.6"));
        assert!(!supports_adaptive_thinking("claude-sonnet-4-5"));
    }

    #[test]
    fn test_sanitize_tool_id() {
        assert_eq!(sanitize_tool_id("tool_123"), "tool_123");
        assert_eq!(sanitize_tool_id("tool@#$%"), "tool____");
        assert_eq!(sanitize_tool_id(""), "tool_0");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![json!({
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }
        })];
        let converted = convert_tools_to_anthropic(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["name"], "read_file");
        assert_eq!(converted[0]["input_schema"]["properties"]["path"]["type"], "string");
    }

    #[test]
    fn test_message_conversion_system_extraction() {
        let messages = vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "Hello"}),
        ];
        let (system, msgs) = convert_messages(&messages, false);
        assert_eq!(system.as_ref().and_then(Value::as_str), Some("You are a helpful assistant."));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn test_message_conversion_assistant_tool_use() {
        let messages = vec![
            json!({"role": "assistant", "content": "", "tool_calls": [{
                "id": "call_123",
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": "{\"path\": \"test.txt\"}"
                }
            }]}),
        ];
        let (_, msgs) = convert_messages(&messages, false);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["name"], "read_file");
        assert_eq!(content[0]["input"]["path"], "test.txt");
    }

    #[test]
    fn test_detect_auth_type_api_key() {
        let auth = detect_auth_type("sk-ant-api03-xxx", None);
        assert!(matches!(auth, AuthType::ApiKey));
    }

    #[test]
    fn test_detect_auth_type_oauth() {
        let auth = detect_auth_type("sk-ant-oat01-xxx", None);
        assert!(matches!(auth, AuthType::OAuth));
    }

    #[test]
    fn test_detect_auth_type_bearer() {
        let auth = detect_auth_type("any-key", Some("https://api.minimax.io/anthropic"));
        assert!(matches!(auth, AuthType::Bearer));
    }

    #[test]
    fn test_image_source_data_url() {
        let source = image_source_from_openai_url("data:image/png;base64,ABC123");
        assert_eq!(source["type"], "base64");
        assert_eq!(source["media_type"], "image/png");
        assert_eq!(source["data"], "ABC123");
    }

    #[test]
    fn test_image_source_regular_url() {
        let source = image_source_from_openai_url("https://example.com/image.jpg");
        assert_eq!(source["type"], "url");
        assert_eq!(source["url"], "https://example.com/image.jpg");
    }

    #[test]
    fn test_response_normalization_text() {
        let response = json!({
            "id": "msg_123",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let normalized = normalize_anthropic_response(&response, false);
        assert_eq!(normalized.content, Some("Hello!".to_string()));
        assert_eq!(normalized.finish_reason, "stop");
        assert!(normalized.tool_calls.is_none());
        assert!(normalized.reasoning.is_none());
    }

    #[test]
    fn test_response_normalization_tool_use() {
        let response = json!({
            "content": [
                {"type": "tool_use", "id": "tool_1", "name": "mcp_read_file", "input": {"path": "test.txt"}}
            ],
            "stop_reason": "tool_use"
        });
        let normalized = normalize_anthropic_response(&response, true);
        assert!(normalized.content.is_none());
        assert_eq!(normalized.finish_reason, "tool_calls");
        let tools = normalized.tool_calls.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        // mcp_ prefix stripped
        assert_eq!(tools[0]["function"]["name"], "read_file");
    }

    #[test]
    fn test_response_normalization_thinking() {
        let response = json!({
            "content": [
                {"type": "thinking", "thinking": "Let me think...", "signature": "abc123"},
                {"type": "text", "text": "Done thinking."}
            ],
            "stop_reason": "end_turn"
        });
        let normalized = normalize_anthropic_response(&response, false);
        assert_eq!(normalized.content, Some("Done thinking.".to_string()));
        assert_eq!(normalized.reasoning, Some("Let me think...".to_string()));
        assert!(normalized.reasoning_details.is_some());
        let details = normalized.reasoning_details.as_ref().unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["thinking"], "Let me think...");
    }

    #[test]
    fn test_response_stop_reason_mapping() {
        let test_cases = [
            ("end_turn", "stop"),
            ("tool_use", "tool_calls"),
            ("max_tokens", "length"),
            ("stop_sequence", "stop"),
            ("unknown", "stop"),
        ];
        for (input, expected) in test_cases {
            let response = json!({
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": input
            });
            let normalized = normalize_anthropic_response(&response, false);
            assert_eq!(normalized.finish_reason, expected, "Failed for stop_reason={}", input);
        }
    }

    #[test]
    fn test_normalized_response_to_value() {
        let resp = NormalizedAnthropicResponse {
            content: Some("Hello".to_string()),
            tool_calls: None,
            reasoning: Some("Thinking...".to_string()),
            reasoning_details: None,
            finish_reason: "stop".to_string(),
            usage: Some(json!({"input_tokens": 10})),
        };
        let value = normalized_response_to_value(&resp);
        assert_eq!(value["content"], "Hello");
        assert_eq!(value["reasoning"], "Thinking...");
        assert_eq!(value["finish_reason"], "stop");
        assert_eq!(value["usage"]["input_tokens"], 10);
    }

    #[test]
    fn test_parse_output_tokens_from_error() {
        let msg = "Your max_tokens setting of 16000 is too large for the given context size. Available output tokens: 8192 (20480 context - 12288 used in prompt)";
        assert_eq!(parse_available_output_tokens_from_error(msg), Some(8192));
        assert!(parse_available_output_tokens_from_error("no relevant info").is_none());
    }

    #[test]
    fn test_parse_max_tokens_from_error() {
        let msg = "max_tokens must be less than 4096, got 8192";
        assert!(parse_max_tokens_from_error(msg).is_some());
    }

    #[test]
    fn test_ephemeral_max_output_tokens() {
        assert_eq!(ephemeral_max_output_tokens(20480, 12288, 1024), 7168);
        assert_eq!(ephemeral_max_output_tokens(8192, 8000, 1000), 1); // minimum 1
        assert_eq!(ephemeral_max_output_tokens(4096, 1000, 500), 2596);
    }

    #[test]
    fn test_error_context_classification() {
        // Context overflow
        let ctx = AnthropicErrorContext::from_error(400, "max_tokens too large for context");
        assert_eq!(ctx.error_type, "context_overflow");
        assert!(ctx.retryable);

        // Rate limit
        let ctx = AnthropicErrorContext::from_error(429, "rate limit exceeded");
        assert_eq!(ctx.error_type, "rate_limit");
        assert!(ctx.retryable);

        // Invalid signature
        let ctx = AnthropicErrorContext::from_error(400, "Invalid signature in thinking block");
        assert_eq!(ctx.error_type, "invalid_thinking_signature");
        assert!(ctx.retryable);

        // Server error
        let ctx = AnthropicErrorContext::from_error(500, "internal server error");
        assert_eq!(ctx.error_type, "server_error");
        assert!(ctx.retryable);

        // Auth error
        let ctx = AnthropicErrorContext::from_error(401, "unauthorized");
        assert_eq!(ctx.error_type, "auth_error");
        assert!(!ctx.retryable);

        // Invalid request
        let ctx = AnthropicErrorContext::from_error(400, "bad request");
        assert_eq!(ctx.error_type, "invalid_request");
        assert!(!ctx.retryable);
    }

    #[test]
    fn test_error_context_to_value() {
        let ctx = AnthropicErrorContext::from_error(400, "max_tokens too large for context");
        let value = ctx.to_value();
        assert_eq!(value["error_type"], "context_overflow");
        assert_eq!(value["retryable"], true);
        assert!(value.get("raw_message").is_some());
    }

    #[test]
    fn test_preprocess_orphan_tool_use_stripped() {
        // tool_use with no matching tool_result
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tool_1", "name": "read_file", "input": {}},
                    {"type": "text", "text": "Let me check"}
                ]
            }),
            json!({"role": "user", "content": "OK"}),
        ];
        let preprocessed = preprocess_anthropic_messages(messages, None);
        // tool_use should be stripped since no tool_result references it
        let assistant = &preprocessed[0];
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(content.len(), 1); // only the text block remains
        assert_eq!(content[0]["text"], "Let me check");
    }

    #[test]
    fn test_preprocess_merging_consecutive_user() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "user", "content": "World"}),
        ];
        let preprocessed = preprocess_anthropic_messages(messages, None);
        assert_eq!(preprocessed.len(), 1);
        assert_eq!(preprocessed[0]["content"], "Hello\nWorld");
    }

    #[test]
    fn test_preprocess_third_party_strips_thinking() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Hmm...", "signature": "sig123"},
                    {"type": "text", "text": "Answer"}
                ]
            }),
        ];
        // Third-party endpoint should strip thinking blocks
        let preprocessed = preprocess_anthropic_messages(messages, Some("https://api.minimax.io/anthropic"));
        let content = preprocessed[0]["content"].as_array().unwrap();
        // Thinking converted to text
        assert!(content.iter().any(|b| b.get("text").and_then(Value::as_str) == Some("Hmm...")));
        // No thinking blocks remain
        assert!(!content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("thinking")));
    }

    #[test]
    fn test_merge_content_blocks() {
        // String + string
        let merged = merge_content_blocks(&Value::String("a".to_string()), &Value::String("b".to_string()));
        assert_eq!(merged, Value::String("a\nb".to_string()));

        // Array + array
        let merged = merge_content_blocks(
            &json!([{"type": "text", "text": "a"}]),
            &json!([{"type": "text", "text": "b"}]),
        );
        assert_eq!(merged.as_array().unwrap().len(), 2);

        // Mixed (string + array)
        let merged = merge_content_blocks(
            &Value::String("text".to_string()),
            &json!([{"type": "text", "text": "block"}]),
        );
        assert_eq!(merged.as_array().unwrap().len(), 2);
    }
}
