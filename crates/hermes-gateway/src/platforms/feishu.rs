//! Feishu/Lark platform adapter.
//!
//! Supports:
//! - WebSocket long connection and Webhook transport
//! - Direct-message and group @mention-gated text receive/send
//! - Inbound image/file/audio media caching
//! - Gateway allowlist integration
//!
//! Mirrors Python `gateway/platforms/feishu.py`.

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::Json,
    Router,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::dedup::MessageDeduplicator;

/// Feishu webhook max body size (2MB, matches Python).
const FEISHU_WEBHOOK_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

// Fallback texts (mirrors Python feishu.py).
const FALLBACK_POST_TEXT: &str = "[Rich text message]";
const FALLBACK_IMAGE_TEXT: &str = "[Image]";
const FALLBACK_FORWARD_TEXT: &str = "[Forwarded message]";
const FALLBACK_SHARE_CHAT_TEXT: &str = "[Shared chat]";
const FALLBACK_INTERACTIVE_TEXT: &str = "[Interactive card]";

/// 24-hour dedup TTL (matches Python `_FEISHU_DEDUP_TTL_SECONDS`).
const FEISHU_DEDUP_TTL_SECONDS: u64 = 24 * 60 * 60;
/// 10-minute sender-name cache TTL.
const FEISHU_SENDER_NAME_TTL_SECONDS: u64 = 10 * 60;
/// Max text document size to inject inline (1 MB).
const MAX_TEXT_INJECT_BYTES: u64 = 1024 * 1024;

/// Regex for markdown special chars (mirrors Python `_MARKDOWN_SPECIAL_CHARS_RE`).
static MARKDOWN_SPECIAL_CHARS_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"([][_*()~`>#+\-.!{}|])").unwrap()
    });

/// Regex for markdown links.
static MARKDOWN_LINK_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap()
    });

/// Regex for mention placeholders.
static MENTION_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"<at id=["']([^"']+)["']>([^<]*)</at>"#).unwrap()
    });

/// Regex for whitespace collapse.
static WHITESPACE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"[ \t]+").unwrap());

/// Regex for multi-space collapse.
static MULTISPACE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r" {2,}").unwrap());

/// Preferred locales for post payload resolution.
const PREFERRED_LOCALES: &[&str] = &["zh_cn", "en_us", "ja_jp", "zh_hk", "zh_tw"];

/// Feishu connection mode.
#[derive(Debug, Clone, Default)]
pub enum FeishuConnectionMode {
    /// WebSocket long connection.
    #[default]
    WebSocket,
    /// HTTP webhook (requires public URL).
    Webhook,
}

/// Feishu group policy.
#[derive(Debug, Clone, Default)]
pub enum GroupPolicy {
    /// Accept messages from anyone.
    #[default]
    Open,
    /// Only accept from allowlisted users.
    Allowlist,
    /// Reject blacklisted users.
    Blacklist,
    /// Only admins can interact.
    AdminOnly,
    /// Group is disabled.
    Disabled,
}

/// Per-group policy rule for controlling which users may interact with the bot.
/// Mirrors Python `FeishuGroupRule`.
#[derive(Debug, Clone, Default)]
pub struct FeishuGroupRule {
    pub policy: String, // "open" | "allowlist" | "blacklist" | "admin_only" | "disabled"
    pub allowlist: HashSet<String>,
    pub blacklist: HashSet<String>,
}

/// Media reference extracted from a post message.
/// Mirrors Python `FeishuPostMediaRef`.
#[derive(Debug, Clone, Default)]
pub struct FeishuPostMediaRef {
    pub file_key: String,
    pub file_name: String,
    pub resource_type: String,
}

/// Result of parsing a Feishu post payload.
/// Mirrors Python `FeishuPostParseResult`.
#[derive(Debug, Clone, Default)]
pub struct FeishuPostParseResult {
    pub text_content: String,
    pub image_keys: Vec<String>,
    pub media_refs: Vec<FeishuPostMediaRef>,
    pub mentioned_ids: Vec<String>,
}

/// Normalized inbound Feishu message.
/// Mirrors Python `FeishuNormalizedMessage`.
#[derive(Debug, Clone)]
pub struct FeishuNormalizedMessage {
    pub raw_type: String,
    pub text_content: String,
    pub preferred_message_type: String,
    pub image_keys: Vec<String>,
    pub media_refs: Vec<FeishuPostMediaRef>,
    pub mentioned_ids: Vec<String>,
    pub relation_kind: String,
    pub metadata: Value,
}

impl Default for FeishuNormalizedMessage {
    fn default() -> Self {
        Self {
            raw_type: String::new(),
            text_content: String::new(),
            preferred_message_type: "text".to_string(),
            image_keys: Vec::new(),
            media_refs: Vec::new(),
            mentioned_ids: Vec::new(),
            relation_kind: "plain".to_string(),
            metadata: Value::Null,
        }
    }
}

/// Feishu platform configuration.
#[derive(Debug, Clone)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    pub connection_mode: FeishuConnectionMode,
    pub verification_token: String,
    pub encrypt_key: String,
    pub group_policy: GroupPolicy,
    pub allowed_users: HashSet<String>,
    pub webhook_port: u16,
    pub webhook_path: String,
    /// Per-group policy rules (chat_id → rule).
    pub group_rules: std::collections::HashMap<String, FeishuGroupRule>,
    /// Bot-level admin open_ids.
    pub admins: HashSet<String>,
    /// Bot identity for mention gating.
    pub bot_open_id: String,
    pub bot_user_id: String,
    pub bot_name: String,
}

impl FeishuConfig {
    pub fn from_env() -> Self {
        let allowed_users: HashSet<String> = std::env::var("FEISHU_ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let admins: HashSet<String> = std::env::var("FEISHU_ADMINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut group_rules = std::collections::HashMap::new();
        if let Ok(rules_json) = std::env::var("FEISHU_GROUP_RULES") {
            if let Ok(rules) = serde_json::from_str::<Value>(&rules_json) {
                if let Some(obj) = rules.as_object() {
                    for (chat_id, rule_val) in obj {
                        if let Some(rule_obj) = rule_val.as_object() {
                            let policy = rule_obj
                                .get("policy")
                                .and_then(|v| v.as_str())
                                .unwrap_or("open")
                                .to_string();
                            let allowlist: HashSet<String> = rule_obj
                                .get("allowlist")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                                        .filter(|s| !s.is_empty())
                                        .collect()
                                })
                                .unwrap_or_default();
                            let blacklist: HashSet<String> = rule_obj
                                .get("blacklist")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                                        .filter(|s| !s.is_empty())
                                        .collect()
                                })
                                .unwrap_or_default();
                            group_rules.insert(
                                chat_id.clone(),
                                FeishuGroupRule {
                                    policy,
                                    allowlist,
                                    blacklist,
                                },
                            );
                        }
                    }
                }
            }
        }

        Self {
            app_id: std::env::var("FEISHU_APP_ID").unwrap_or_default(),
            app_secret: std::env::var("FEISHU_APP_SECRET").unwrap_or_default(),
            connection_mode: FeishuConnectionMode::default(),
            verification_token: std::env::var("FEISHU_VERIFICATION_TOKEN").unwrap_or_default(),
            encrypt_key: std::env::var("FEISHU_ENCRYPT_KEY").unwrap_or_default(),
            group_policy: GroupPolicy::default(),
            allowed_users,
            webhook_port: std::env::var("FEISHU_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8765),
            webhook_path: std::env::var("FEISHU_WEBHOOK_PATH")
                .ok()
                .unwrap_or_else(|| "/feishu/webhook".to_string()),
            group_rules,
            admins,
            bot_open_id: std::env::var("FEISHU_BOT_OPEN_ID").unwrap_or_default(),
            bot_user_id: std::env::var("FEISHU_BOT_USER_ID").unwrap_or_default(),
            bot_name: std::env::var("FEISHU_BOT_NAME").unwrap_or_default(),
        }
    }
}

/// Cached token with expiry tracking.
struct CachedToken {
    token: String,
    expires_at: std::time::Instant,
}

/// Persistent 24h TTL deduplication backed by a JSON file.
/// Mirrors Python `_seen_message_ids` persistence.
pub struct PersistentDedup {
    path: std::path::PathBuf,
    entries: parking_lot::Mutex<std::collections::HashMap<String, f64>>,
    order: parking_lot::Mutex<Vec<String>>,
    max_size: usize,
    ttl_secs: u64,
}

impl PersistentDedup {
    pub fn new(max_size: usize, ttl_secs: u64) -> Self {
        let path = hermes_core::get_hermes_home().join("feishu_seen_message_ids.json");
        let mut entries = std::collections::HashMap::new();
        let mut order = Vec::new();

        // Load existing state
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(payload) = serde_json::from_str::<Value>(&text) {
                let seen_data = payload.get("message_ids").cloned().unwrap_or(Value::Null);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();

                let loaded: std::collections::HashMap<String, f64> = match seen_data {
                    Value::Array(arr) => {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| (s.trim().to_string(), 0.0)))
                            .filter(|(k, _)| !k.is_empty())
                            .collect()
                    }
                    Value::Object(obj) => obj
                        .iter()
                        .filter_map(|(k, v)| {
                            v.as_f64().map(|ts| (k.trim().to_string(), ts))
                        })
                        .filter(|(k, _)| !k.is_empty())
                        .collect(),
                    _ => std::collections::HashMap::new(),
                };

                for (msg_id, ts) in loaded {
                    if ts == 0.0 || ttl_secs == 0 || now - ts < ttl_secs as f64 {
                        entries.insert(msg_id.clone(), ts);
                        order.push(msg_id);
                    }
                }
            }
        }

        // Apply size cap — keep most recent
        if order.len() > max_size {
            let mut sorted: Vec<(String, f64)> =
                order.iter().filter_map(|k| entries.get(k).map(|&v| (k.clone(), v))).collect();
            sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let keep: std::collections::HashSet<String> =
                sorted.into_iter().take(max_size).map(|(k, _)| k).collect();
            entries.retain(|k, _| keep.contains(k));
            order.retain(|k| keep.contains(k));
        }

        Self {
            path,
            entries: parking_lot::Mutex::new(entries),
            order: parking_lot::Mutex::new(order),
            max_size,
            ttl_secs,
        }
    }

    /// Check if key was already seen within the TTL window.
    /// If not, records it and persists.
    pub fn is_duplicate(&self, key: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let mut entries = self.entries.lock();
        let mut order = self.order.lock();

        if let Some(&seen_at) = entries.get(key) {
            if self.ttl_secs == 0 || now - seen_at < self.ttl_secs as f64 {
                return true;
            }
            // Expired — remove
            entries.remove(key);
            order.retain(|k| k != key);
        }

        // Record new
        entries.insert(key.to_string(), now);
        order.push(key.to_string());

        // Evict oldest if over max size
        while order.len() > self.max_size {
            if let Some(stale) = order.first().cloned() {
                order.remove(0);
                entries.remove(&stale);
            }
        }

        drop(entries);
        drop(order);
        self.persist();
        false
    }

    fn persist(&self) {
        let entries = self.entries.lock();
        let order = self.order.lock();
        let recent: std::collections::HashMap<String, f64> = order
            .iter()
            .filter_map(|k| entries.get(k).map(|&v| (k.clone(), v)))
            .collect();
        let payload = serde_json::json!({ "message_ids": recent });
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.path, serde_json::to_string(&payload).unwrap_or_default());
    }
}

/// In-memory TTL cache for sender names.
struct SenderNameCache {
    entries: parking_lot::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    ttl: Duration,
}

impl SenderNameCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: parking_lot::Mutex::new(std::collections::HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn get(&self, sender_id: &str) -> Option<String> {
        let mut map = self.entries.lock();
        let now = std::time::Instant::now();
        if let Some((name, expiry)) = map.get(sender_id) {
            if now < *expiry {
                return Some(name.clone());
            }
            map.remove(sender_id);
        }
        None
    }

    fn insert(&self, sender_id: String, name: String) {
        let mut map = self.entries.lock();
        let expiry = std::time::Instant::now() + self.ttl;
        map.insert(sender_id, (name, expiry));
    }
}

impl CachedToken {
    fn new(token: String, expire_secs: u64) -> Self {
        let refresh_buffer = std::time::Duration::from_secs(300);
        let expires_at = std::time::Instant::now()
            + std::time::Duration::from_secs(expire_secs)
            - refresh_buffer;
        Self { token, expires_at }
    }
}

/// Media key extracted from a Feishu message.
#[derive(Debug, Clone)]
pub struct MediaKey {
    /// Media type: image, file, audio.
    pub media_type: String,
    /// image_key or file_key.
    pub key: String,
    /// Original file name (for files).
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Post parsing and message normalization
// ---------------------------------------------------------------------------

/// Escape markdown special characters.
fn escape_markdown_text(text: &str) -> String {
    MARKDOWN_SPECIAL_CHARS_RE.replace_all(text, r"\$1").to_string()
}

/// Wrap inline code with appropriate backtick fence.
fn wrap_inline_code(text: &str) -> String {
    let max_run = regex::Regex::new(r"`+")
        .unwrap()
        .find_iter(text)
        .map(|m| m.len())
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(max_run + 1);
    let body = if text.starts_with('`') || text.ends_with('`') {
        format!(" {text} ")
    } else {
        text.to_string()
    };
    format!("{fence}{body}{fence}")
}

/// Coerce a JSON value to boolean.
fn to_boolean(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// Check if a style key is enabled.
fn is_style_enabled(style: Option<&Value>, key: &str) -> bool {
    style.and_then(|s| s.get(key)).map(|v| to_boolean(Some(v))).unwrap_or(false)
}

/// Render a text element from a post payload.
fn render_text_element(element: &Value) -> String {
    let text = element.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let style = element.get("style");

    if is_style_enabled(style, "code") {
        return wrap_inline_code(text);
    }

    let mut rendered = escape_markdown_text(text);
    if rendered.is_empty() {
        return String::new();
    }
    if is_style_enabled(style, "bold") {
        rendered = format!("**{rendered}**");
    }
    if is_style_enabled(style, "italic") {
        rendered = format!("*{rendered}*");
    }
    if is_style_enabled(style, "underline") {
        rendered = format!("<u>{rendered}</u>");
    }
    if is_style_enabled(style, "strikethrough") {
        rendered = format!("~~{rendered}~~");
    }
    rendered
}

/// Render a code_block element.
fn render_code_block_element(element: &Value) -> String {
    let language = element
        .get("language")
        .or_else(|| element.get("lang"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .replace('\n', " ")
        .replace('\r', " ");
    let code = element
        .get("text")
        .or_else(|| element.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace("\r\n", "\n");
    let trailing = if code.ends_with('\n') { "" } else { "\n" };
    format!("```{language}\n{code}{trailing}```")
}

/// Normalize Feishu text: collapse whitespace, strip empty lines.
fn normalize_feishu_text(text: &str) -> String {
    let cleaned = MENTION_PLACEHOLDER_RE.replace_all(text, " ");
    let cleaned = cleaned.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = cleaned
        .lines()
        .map(|line| WHITESPACE_RE.replace_all(line, " ").trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    MULTISPACE_RE.replace_all(&cleaned, " ").trim().to_string()
}

/// Render a single post element.
fn render_post_element(element: &Value, result: &mut FeishuPostParseResult) -> String {
    let element = match element.as_object() {
        Some(o) => o,
        None => return String::new(),
    };

    let tag = element
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    match tag.as_str() {
        "text" => render_text_element(&Value::Object(element.clone())),
        "a" => {
            let href = element.get("href").and_then(|v| v.as_str()).unwrap_or("").trim();
            let label = element
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or(href)
                .trim();
            if label.is_empty() {
                return String::new();
            }
            let escaped = escape_markdown_text(label);
            if href.is_empty() {
                escaped
            } else {
                format!("[{escaped}]({href})")
            }
        }
        "at" => {
            let mentioned_id = element
                .get("open_id")
                .or_else(|| element.get("user_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !mentioned_id.is_empty() && !result.mentioned_ids.iter().any(|s| s == mentioned_id) {
                result.mentioned_ids.push(mentioned_id.to_string());
            }
            let display_name = element
                .get("user_name")
                .or_else(|| element.get("name"))
                .or_else(|| element.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or(mentioned_id)
                .trim();
            if display_name.is_empty() {
                "@".to_string()
            } else {
                format!("@{}", escape_markdown_text(display_name))
            }
        }
        "img" | "image" => {
            let image_key = element
                .get("image_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !image_key.is_empty() && !result.image_keys.iter().any(|s| s == image_key) {
                result.image_keys.push(image_key.to_string());
            }
            let alt = element
                .get("text")
                .or_else(|| element.get("alt"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if alt.is_empty() {
                "[Image]".to_string()
            } else {
                format!("[Image: {alt}]")
            }
        }
        "media" | "file" | "audio" | "video" => {
            let file_key = element
                .get("file_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let file_name = element
                .get("file_name")
                .or_else(|| element.get("title"))
                .or_else(|| element.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !file_key.is_empty() {
                let resource_type = if tag == "audio" || tag == "video" {
                    tag.to_string()
                } else {
                    "file".to_string()
                };
                result.media_refs.push(FeishuPostMediaRef {
                    file_key: file_key.to_string(),
                    file_name: file_name.to_string(),
                    resource_type,
                });
            }
            if file_name.is_empty() {
                "[Attachment]".to_string()
            } else {
                format!("[Attachment: {file_name}]")
            }
        }
        "emotion" | "emoji" => {
            let label = element
                .get("text")
                .or_else(|| element.get("emoji_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if label.is_empty() {
                "[Emoji]".to_string()
            } else {
                format!(":{label}:")
            }
        }
        "br" => "\n".to_string(),
        "hr" | "divider" => "\n\n---\n\n".to_string(),
        "code" => {
            let code = element
                .get("text")
                .or_else(|| element.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if code.is_empty() {
                String::new()
            } else {
                wrap_inline_code(code)
            }
        }
        "code_block" | "pre" => render_code_block_element(&Value::Object(element.clone())),
        _ => {
            let mut nested_parts = Vec::new();
            for key in &["text", "title", "content", "children", "elements"] {
                if let Some(value) = element.get(*key) {
                    let extracted = render_nested_post(value, result);
                    if !extracted.is_empty() {
                        nested_parts.push(extracted);
                    }
                }
            }
            nested_parts.join(" ")
        }
    }
}

/// Recursively render nested post content.
fn render_nested_post(value: &Value, result: &mut FeishuPostParseResult) -> String {
    match value {
        Value::String(s) => escape_markdown_text(s),
        Value::Array(arr) => arr
            .iter()
            .map(|item| render_nested_post(item, result))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(_) => {
            let direct = render_post_element(value, result);
            if !direct.is_empty() {
                return direct;
            }
            value
                .as_object()
                .unwrap()
                .values()
                .map(|v| render_nested_post(v, result))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}

/// Resolve the inner post payload from various wrapper formats.
fn resolve_post_payload(payload: &Value) -> Option<&Value> {
    if let Some(direct) = to_post_payload(payload) {
        return Some(direct);
    }
    let obj = payload.as_object()?;
    if let Some(wrapped) = obj.get("post") {
        if let Some(direct) = to_post_payload(wrapped) {
            return Some(direct);
        }
    }
    resolve_locale_payload(payload)
}

/// Try to extract a post payload object (must have a `content` array).
fn to_post_payload(candidate: &Value) -> Option<&Value> {
    let obj = candidate.as_object()?;
    if obj.get("content").and_then(|v| v.as_array()).is_some() {
        return Some(candidate);
    }
    None
}

/// Resolve locale-wrapped post payload.
fn resolve_locale_payload(payload: &Value) -> Option<&Value> {
    if let Some(direct) = to_post_payload(payload) {
        return Some(direct);
    }
    let obj = payload.as_object()?;
    for key in PREFERRED_LOCALES {
        if let Some(candidate) = obj.get(*key) {
            if let Some(direct) = to_post_payload(candidate) {
                return Some(direct);
            }
        }
    }
    obj.values().find_map(|v| to_post_payload(v))
}

/// Parse a Feishu post payload into markdown-like text.
/// Mirrors Python `parse_feishu_post_payload`.
pub fn parse_feishu_post_payload(payload: &Value) -> FeishuPostParseResult {
    let resolved = resolve_post_payload(payload);
    if resolved.is_none() {
        return FeishuPostParseResult {
            text_content: FALLBACK_POST_TEXT.to_string(),
            ..Default::default()
        };
    }
    let resolved = resolved.unwrap();
    let mut result = FeishuPostParseResult::default();
    let mut parts: Vec<String> = Vec::new();

    let title = normalize_feishu_text(
        resolved
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim(),
    );
    if !title.is_empty() {
        parts.push(title);
    }

    if let Some(content) = resolved.get("content").and_then(|v| v.as_array()) {
        for row in content {
            if let Some(cols) = row.as_array() {
                let row_text: String = cols
                    .iter()
                    .map(|item| render_post_element(item, &mut result))
                    .collect();
                let row_text = normalize_feishu_text(&row_text);
                if !row_text.is_empty() {
                    parts.push(row_text);
                }
            }
        }
    }

    let text_content = if parts.is_empty() {
        FALLBACK_POST_TEXT.to_string()
    } else {
        parts.join("\n").trim().to_string()
    };

    FeishuPostParseResult {
        text_content,
        image_keys: result.image_keys,
        media_refs: result.media_refs,
        mentioned_ids: result.mentioned_ids,
    }
}

/// Load a Feishu content JSON string into a Value.
fn load_feishu_payload(raw_content: &str) -> Value {
    if raw_content.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(raw_content).unwrap_or_else(|_| {
        serde_json::json!({ "text": raw_content })
    })
}

/// Normalize a merge_forward message.
fn normalize_merge_forward_message(payload: &Value) -> FeishuNormalizedMessage {
    let title = find_first_text(payload, &["title", "summary", "preview", "description"])
        .unwrap_or_default();
    let entries = collect_forward_entries(payload);
    let mut lines: Vec<String> = Vec::new();
    if !title.is_empty() {
        lines.push(title);
    }
    for entry in entries.iter().take(8) {
        lines.push(entry.clone());
    }
    let text_content = lines.join("\n").trim().to_string();
    FeishuNormalizedMessage {
        raw_type: "merge_forward".to_string(),
        text_content: if text_content.is_empty() {
            FALLBACK_FORWARD_TEXT.to_string()
        } else {
            text_content
        },
        relation_kind: "merge_forward".to_string(),
        metadata: serde_json::json!({
            "entry_count": entries.len(),
            "title": lines.first().cloned().unwrap_or_default(),
        }),
        ..Default::default()
    }
}

/// Collect entries from a merge_forward payload.
fn collect_forward_entries(payload: &Value) -> Vec<String> {
    let mut candidates = Vec::new();
    for key in &["messages", "items", "message_list", "records", "content"] {
        if let Some(arr) = payload.get(key).and_then(|v| v.as_array()) {
            candidates.extend(arr.iter().cloned());
        }
    }

    let mut entries = Vec::new();
    for item in &candidates {
        if let Some(obj) = item.as_object() {
            let sender = find_first_text(&serde_json::Value::Object(obj.clone()), &["sender_name", "user_name", "sender", "name"])
                .unwrap_or_default();
            let nested_type = obj
                .get("message_type")
                .or_else(|| obj.get("msg_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let body = if nested_type == "post" {
                let content = obj.get("content").cloned().unwrap_or(Value::Null);
                parse_feishu_post_payload(&content).text_content
            } else {
                obj.get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| normalize_feishu_text(s))
                    .unwrap_or_default()
            };
            if !body.is_empty() {
                let entry = if !sender.is_empty() {
                    format!("{sender}: {body}")
                } else {
                    body
                };
                entries.push(entry);
            }
        } else if let Some(s) = item.as_str() {
            let text = normalize_feishu_text(s);
            if !text.is_empty() {
                entries.push(format!("- {text}"));
            }
        }
    }
    entries
}

/// Normalize a share_chat message.
fn normalize_share_chat_message(payload: &Value) -> FeishuNormalizedMessage {
    let chat_name = find_first_text(payload, &["chat_name", "name", "title"]).unwrap_or_default();
    let share_id = payload
        .get("chat_id")
        .or_else(|| payload.get("open_chat_id"))
        .or_else(|| payload.get("share_chat_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut lines = Vec::new();
    if !chat_name.is_empty() {
        lines.push(format!("Shared chat: {chat_name}"));
    } else {
        lines.push(FALLBACK_SHARE_CHAT_TEXT.to_string());
    }
    if !share_id.is_empty() {
        lines.push(format!("Chat ID: {share_id}"));
    }

    FeishuNormalizedMessage {
        raw_type: "share_chat".to_string(),
        text_content: lines.join("\n"),
        relation_kind: "share_chat".to_string(),
        metadata: serde_json::json!({
            "chat_id": share_id,
            "chat_name": chat_name,
        }),
        ..Default::default()
    }
}

/// Normalize an interactive/card message.
fn normalize_interactive_message(message_type: &str, payload: &Value) -> FeishuNormalizedMessage {
    let card_payload = payload
        .get("card")
        .and_then(|v| v.as_object())
        .map(|_| payload.get("card").unwrap())
        .unwrap_or(payload);

    let title = find_header_title(card_payload)
        .or_else(|| find_first_text(payload, &["title", "summary", "subtitle"]))
        .unwrap_or_default();
    let body_lines = collect_card_lines(card_payload);
    let actions = collect_action_labels(card_payload);

    let mut lines: Vec<String> = Vec::new();
    if !title.is_empty() {
        lines.push(title.clone());
    }
    for line in &body_lines {
        if line != &title {
            lines.push(line.clone());
        }
    }
    if !actions.is_empty() {
        lines.push(format!("Actions: {}", actions.join(", ")));
    }

    let text_content = lines.into_iter().take(12).collect::<Vec<_>>().join("\n");
    FeishuNormalizedMessage {
        raw_type: message_type.to_string(),
        text_content: if text_content.trim().is_empty() {
            FALLBACK_INTERACTIVE_TEXT.to_string()
        } else {
            text_content.trim().to_string()
        },
        relation_kind: "interactive".to_string(),
        metadata: serde_json::json!({
            "title": title,
            "actions": actions,
        }),
        ..Default::default()
    }
}

/// Find the first non-empty text value among keys.
fn find_first_text(payload: &Value, keys: &[&str]) -> Option<String> {
    let obj = payload.as_object()?;
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Extract the header title from a card payload.
fn find_header_title(card_payload: &Value) -> Option<String> {
    let header = card_payload.get("header")?.as_object()?;
    let title = header.get("title")?.as_object()?;
    title
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Collect text lines from a card payload.
fn collect_card_lines(card_payload: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(elements) = card_payload.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            lines.extend(collect_text_segments(el, false));
        }
    }
    lines.into_iter().filter(|l| !l.is_empty()).collect()
}

/// Collect action labels from a card payload.
fn collect_action_labels(card_payload: &Value) -> Vec<String> {
    let mut labels = Vec::new();
    if let Some(elements) = card_payload.get("elements").and_then(|v| v.as_array()) {
        for el in elements {
            if let Some(obj) = el.as_object() {
                if obj.get("tag").and_then(|v| v.as_str()) == Some("action") {
                    if let Some(actions) = obj.get("actions").and_then(|v| v.as_array()) {
                        for action in actions {
                            if let Some(label) = action
                                .get("text")
                                .and_then(|v| v.as_object())
                                .and_then(|o| o.get("content"))
                                .and_then(|v| v.as_str())
                            {
                                let trimmed = label.trim();
                                if !trimmed.is_empty() {
                                    labels.push(trimmed.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    labels
}

/// Recursively collect text segments from a card element.
fn collect_text_segments(value: &Value, in_rich_block: bool) -> Vec<String> {
    match value {
        Value::String(s) => {
            if in_rich_block {
                vec![normalize_feishu_text(s)]
            } else {
                vec![]
            }
        }
        Value::Array(arr) => arr
            .iter()
            .flat_map(|item| collect_text_segments(item, in_rich_block))
            .collect(),
        Value::Object(obj) => {
            let tag = obj
                .get("tag")
                .or_else(|| obj.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let next_in_rich = in_rich_block
                || tag == "plain_text"
                || tag == "lark_md"
                || tag == "markdown"
                || tag == "note";

            if tag == "markdown" || tag == "lark_md" {
                if let Some(text) = obj.get("content").and_then(|v| v.as_str()) {
                    return vec![normalize_feishu_text(text)];
                }
            }
            if tag == "plain_text" {
                if let Some(text) = obj.get("content").and_then(|v| v.as_str()) {
                    return vec![normalize_feishu_text(text)];
                }
            }

            let mut segments = Vec::new();
            for key in &["content", "text", "title", "elements", "children"] {
                if let Some(child) = obj.get(*key) {
                    segments.extend(collect_text_segments(child, next_in_rich));
                }
            }
            segments
        }
        _ => vec![],
    }
}

/// Normalize a Feishu message based on its type.
/// Mirrors Python `normalize_feishu_message`.
pub fn normalize_feishu_message(message_type: &str, raw_content: &str) -> FeishuNormalizedMessage {
    let normalized_type = message_type.trim().to_lowercase();
    let payload = load_feishu_payload(raw_content);

    match normalized_type.as_str() {
        "text" => FeishuNormalizedMessage {
            raw_type: normalized_type,
            text_content: normalize_feishu_text(
                payload.get("text").and_then(|v| v.as_str()).unwrap_or("")
            ),
            ..Default::default()
        },
        "post" => {
            let parsed = parse_feishu_post_payload(&payload);
            FeishuNormalizedMessage {
                raw_type: normalized_type.clone(),
                text_content: parsed.text_content,
                image_keys: parsed.image_keys,
                media_refs: parsed.media_refs,
                mentioned_ids: parsed.mentioned_ids,
                relation_kind: "post".to_string(),
                ..Default::default()
            }
        }
        "image" => {
            let image_key = payload
                .get("image_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let alt_text = normalize_feishu_text(
                payload
                    .get("text")
                    .or_else(|| payload.get("alt"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(FALLBACK_IMAGE_TEXT)
            );
            FeishuNormalizedMessage {
                raw_type: normalized_type.clone(),
                text_content: if alt_text == FALLBACK_IMAGE_TEXT {
                    String::new()
                } else {
                    alt_text
                },
                preferred_message_type: "photo".to_string(),
                image_keys: if image_key.is_empty() { vec![] } else { vec![image_key] },
                relation_kind: "image".to_string(),
                ..Default::default()
            }
        }
        "file" | "audio" | "media" => {
            let file_key = payload
                .get("file_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let file_name = payload
                .get("file_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let resource_type = if normalized_type == "audio" { "audio" } else { "file" };
            FeishuNormalizedMessage {
                raw_type: normalized_type.clone(),
                text_content: String::new(),
                preferred_message_type: if normalized_type == "audio" {
                    "audio".to_string()
                } else {
                    "document".to_string()
                },
                media_refs: if file_key.is_empty() {
                    vec![]
                } else {
                    vec![FeishuPostMediaRef {
                        file_key,
                        file_name,
                        resource_type: resource_type.to_string(),
                    }]
                },
                relation_kind: normalized_type.clone(),
                metadata: serde_json::json!({
                    "placeholder_text": if resource_type == "audio" {
                        "[Audio message]"
                    } else {
                        "[File attachment]"
                    },
                }),
                ..Default::default()
            }
        }
        "merge_forward" => normalize_merge_forward_message(&payload),
        "share_chat" => normalize_share_chat_message(&payload),
        "interactive" | "card" => normalize_interactive_message(&normalized_type, &payload),
        _ => FeishuNormalizedMessage {
            raw_type: normalized_type,
            text_content: String::new(),
            ..Default::default()
        },
    }
}

/// Inbound message event from Feishu.
#[derive(Debug, Clone)]
pub struct FeishuMessageEvent {
    pub message_id: String,
    pub chat_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub content: String,
    pub msg_type: String,
    pub is_group: bool,
    pub mentions: Vec<String>,
    pub media_keys: Vec<MediaKey>,
}

/// Callback type for inbound Feishu messages.
pub type FeishuInboundCallback = Arc<dyn Fn(FeishuMessageEvent) + Send + Sync>;

/// Feishu card action event.
#[derive(Debug, Clone)]
pub struct FeishuCardActionEvent {
    pub action_tag: String,
    pub action_value: Value,
    pub open_id: String,
    pub open_message_id: String,
    pub tenant_key: Option<String>,
}

/// Callback for card action triggers.
pub type FeishuCardActionCallback = Arc<dyn Fn(FeishuCardActionEvent) + Send + Sync>;

/// Delay before flushing a text batch (seconds).
/// Mirrors Python `text_batch_delay_seconds` (default 0.5s).
const TEXT_BATCH_DELAY_MS: u64 = 500;

/// Card action dedup TTL (15 minutes).
const CARD_ACTION_DEDUP_TTL_SECONDS: u64 = 15 * 60;

/// Approval choice mapping: button action → canonical choice.
const APPROVAL_CHOICE_MAP: &[(&str, &str)] = &[
    ("approve_once", "once"),
    ("approve_session", "session"),
    ("approve_always", "always"),
    ("deny", "deny"),
];

/// Approval label mapping: canonical choice → display label.
const APPROVAL_LABEL_MAP: &[(&str, &str)] = &[
    ("once", "Approved once"),
    ("session", "Approved for session"),
    ("always", "Approved permanently"),
    ("deny", "Denied"),
];

/// State stored for a pending approval.
#[derive(Debug, Clone)]
pub struct ApprovalState {
    pub session_key: String,
    pub message_id: String,
    pub chat_id: String,
}

/// Feishu platform adapter.
#[derive(Clone)]
pub struct FeishuAdapter {
    pub config: FeishuConfig,
    client: Client,
    /// In-memory short-term dedup (kept for backward compat).
    dedup: Arc<MessageDeduplicator>,
    /// Persistent 24h dedup backed by JSON file.
    persistent_dedup: Arc<PersistentDedup>,
    /// Cached sender names (10min TTL).
    sender_name_cache: Arc<SenderNameCache>,
    access_token: Arc<RwLock<Option<CachedToken>>>,
    /// Called when a webhook message is received.
    /// Set before starting the webhook server.
    pub on_message: Arc<RwLock<Option<FeishuInboundCallback>>>,
    /// Called when a card action is triggered.
    pub on_card_action: Arc<RwLock<Option<FeishuCardActionCallback>>>,
    /// Pending text batches for auto-merging rapid successive messages.
    text_batches: Arc<tokio::sync::Mutex<std::collections::HashMap<String, FeishuMessageEvent>>>,
    /// Abort handles for batch flush timers.
    batch_timers: Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>>,
    /// Pending approval state keyed by approval_id.
    approval_state: Arc<tokio::sync::Mutex<std::collections::HashMap<u64, ApprovalState>>>,
    /// Atomic counter for generating approval IDs.
    approval_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Dedup for card actions (token → expiry Instant).
    card_action_dedup: Arc<tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
}

impl FeishuAdapter {
    pub fn new(config: FeishuConfig) -> Self {
        let dedup_cache_size = std::env::var("HERMES_FEISHU_DEDUP_CACHE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000);
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: Arc::new(MessageDeduplicator::new()),
            persistent_dedup: Arc::new(PersistentDedup::new(dedup_cache_size, FEISHU_DEDUP_TTL_SECONDS)),
            sender_name_cache: Arc::new(SenderNameCache::new(FEISHU_SENDER_NAME_TTL_SECONDS)),
            access_token: Arc::new(RwLock::new(None)),
            on_message: Arc::new(RwLock::new(None)),
            on_card_action: Arc::new(RwLock::new(None)),
            text_batches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            batch_timers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            approval_state: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            approval_counter: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            card_action_dedup: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            config,
        }
    }

    async fn get_access_token(&self) -> Result<String, String> {
        {
            let guard = self.access_token.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.expires_at > std::time::Instant::now() {
                    return Ok(cached.token.clone());
                }
            }
        }

        let resp = self
            .client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&serde_json::json!({
                "app_id": &self.config.app_id,
                "app_secret": &self.config.app_secret,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to get access token: {e}"))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token response: {e}"))?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!("Token request failed: code={code}, msg={}", body.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown")));
        }

        let token = body
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or("Missing tenant_access_token in response")?
            .to_string();

        *self.access_token.write().await = Some(CachedToken::new(token.clone(), 7200));
        Ok(token)
    }

    /// Send a text message to a Feishu chat.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());

        let resp = self
            .client
            .post("https://open.feishu.cn/open-apis/im/v1/messages")
            .query(&[("receive_id_type", "chat_id")])
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": serde_json::json!({"text": text}).to_string(),
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Send failed: HTTP {}", status));
        }

        debug!("Feishu message sent to {chat_id}: msg_id={msg_id}");
        Ok(msg_id)
    }

    /// Send a message with an arbitrary msg_type and content payload.
    async fn send_message(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());

        let resp = self
            .client
            .post("https://open.feishu.cn/open-apis/im/v1/messages")
            .query(&[("receive_id_type", "chat_id")])
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": msg_type,
                "content": content.to_string(),
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Send failed: HTTP {}", status));
        }

        debug!("Feishu {msg_type} message sent to {chat_id}: msg_id={msg_id}");
        Ok(msg_id)
    }

    /// Send a rich-text (post) message.
    ///
    /// Converts simple markdown-like text to Feishu post format.
    pub async fn send_post(&self,
        chat_id: &str,
        title: &str,
        content: &str,
    ) -> Result<String, String> {
        let post_content = build_post_payload(title, content);
        self.send_message(chat_id, "post", &post_content).await
    }

    /// Send an interactive approval card with four buttons.
    ///
    /// Mirrors Python `send_exec_approval()` (feishu.py:1440).
    /// Returns the message_id of the sent card.
    pub async fn send_exec_approval(
        &self,
        chat_id: &str,
        command: &str,
        session_key: &str,
        description: &str,
    ) -> Result<String, String> {
        let approval_id = self.approval_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let cmd_preview = if command.len() > 3000 {
            format!("{}...", &command[..3000])
        } else {
            command.to_string()
        };

        let btn = |label: &str, action_name: &str, btn_type: &str| -> Value {
            serde_json::json!({
                "tag": "button",
                "text": {"tag": "plain_text", "content": label},
                "type": btn_type,
                "value": {"hermes_action": action_name, "approval_id": approval_id},
            })
        };

        let card = serde_json::json!({
            "config": {"wide_screen_mode": true},
            "header": {
                "title": {"content": "⚠️ Command Approval Required", "tag": "plain_text"},
                "template": "orange",
            },
            "elements": [
                {
                    "tag": "markdown",
                    "content": format!("```\n{}\n```\n**Reason:** {}", cmd_preview, description),
                },
                {
                    "tag": "action",
                    "actions": [
                        btn("✅ Allow Once", "approve_once", "primary"),
                        btn("✅ Session", "approve_session", "default"),
                        btn("✅ Always", "approve_always", "default"),
                        btn("❌ Deny", "deny", "danger"),
                    ],
                },
            ],
        });

        let msg_id = self.send_message(chat_id, "interactive", &card).await?;

        let mut state = self.approval_state.lock().await;
        state.insert(approval_id, ApprovalState {
            session_key: session_key.to_string(),
            message_id: msg_id.clone(),
            chat_id: chat_id.to_string(),
        });

        // Clean up old approvals periodically
        if state.len() > 256 {
            let to_remove: Vec<u64> = state.keys().take(state.len() - 128).copied().collect();
            for k in to_remove {
                state.remove(&k);
            }
        }

        Ok(msg_id)
    }

    /// Build a resolved approval card for updating after button click.
    ///
    /// Mirrors Python `_build_resolved_approval_card()` (feishu.py:1510).
    pub fn build_resolved_approval_card(choice: &str, user_name: &str) -> Value {
        let icon = if choice == "deny" { "❌" } else { "✅" };
        let label = APPROVAL_LABEL_MAP
            .iter()
            .find(|(k, _)| *k == choice)
            .map(|(_, v)| *v)
            .unwrap_or("Resolved");
        serde_json::json!({
            "config": {"wide_screen_mode": true},
            "header": {
                "title": {"content": format!("{icon} {label}"), "tag": "plain_text"},
                "template": if choice == "deny" { "red" } else { "green" },
            },
            "elements": [
                {
                    "tag": "markdown",
                    "content": format!("{icon} **{label}** by {user_name}"),
                },
            ],
        })
    }

    /// Resolve a pending approval by ID.
    ///
    /// Mirrors Python `_resolve_approval()` (feishu.py:1916).
    /// TODO: wire up to agent-side approval blocking mechanism.
    pub async fn resolve_approval(&self, approval_id: u64, choice: &str, user_name: &str) {
        let state = {
            let mut guard = self.approval_state.lock().await;
            guard.remove(&approval_id)
        };
        let Some(state) = state else {
            debug!("[Feishu] Approval {approval_id} already resolved or unknown");
            return;
        };
        info!(
            "Feishu button resolved approval for session {} (choice={choice}, user={user_name})",
            state.session_key,
        );
        // TODO: call into gateway approval system to unblock agent
        // In Python: tools.approval.resolve_gateway_approval(state["session_key"], choice)
        let _ = (state, choice, user_name);
    }

    /// Upload an image to Feishu and return the image_key.
    async fn upload_image(&self,
        image_path: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let bytes = if image_path.starts_with("http://") || image_path.starts_with("https://") {
            let resp = self
                .client
                .get(image_path)
                .send()
                .await
                .map_err(|e| format!("Failed to download image: {e}"))?;
            resp.bytes()
                .await
                .map_err(|e| format!("Failed to read image body: {e}"))?
                .to_vec()
        } else {
            tokio::fs::read(image_path).await.map_err(|e| format!("Failed to read image: {e}"))?
        };

        let ext = std::path::Path::new(image_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg");
        let (file_name, mime) = match ext.to_lowercase().as_str() {
            "png" => ("image.png", "image/png"),
            "gif" => ("image.gif", "image/gif"),
            "bmp" => ("image.bmp", "image/bmp"),
            "webp" => ("image.webp", "image/webp"),
            _ => ("image.jpg", "image/jpeg"),
        };
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(mime)
            .map_err(|e| format!("Invalid mime type: {e}"))?;
        let form = reqwest::multipart::Form::new().part("image", part);

        let resp = self
            .client
            .post("https://open.feishu.cn/open-apis/im/v1/images")
            .query(&[("image_type", "message")])
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("Failed to upload image: {e}"))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse upload response: {e}"))?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!("Image upload failed: code={code}"));
        }

        body.get("data")
            .and_then(|d| d.get("image_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or("Missing image_key in upload response".to_string())
    }

    /// Upload a file to Feishu and return the file_key.
    async fn upload_file(
        &self,
        file_path: &str,
        file_type: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let bytes = if file_path.starts_with("http://") || file_path.starts_with("https://") {
            let resp = self
                .client
                .get(file_path)
                .send()
                .await
                .map_err(|e| format!("Failed to download file: {e}"))?;
            resp.bytes()
                .await
                .map_err(|e| format!("Failed to read file body: {e}"))?
                .to_vec()
        } else {
            tokio::fs::read(file_path).await.map_err(|e| format!("Failed to read file: {e}"))?
        };

        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let mime = match file_type {
            "stream" => "application/octet-stream",
            "opus" => "audio/opus",
            "mp4" => "video/mp4",
            _ => "application/octet-stream",
        };

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name.to_string())
            .mime_str(mime)
            .map_err(|e| format!("Invalid mime type: {e}"))?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("file_type", file_type.to_string());

        let resp = self
            .client
            .post("https://open.feishu.cn/open-apis/im/v1/files")
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("Failed to upload file: {e}"))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse upload response: {e}"))?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!("File upload failed: code={code}"));
        }

        body.get("data")
            .and_then(|d| d.get("file_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or("Missing file_key in upload response".to_string())
    }

    /// Send an image message.
    pub async fn send_image(
        &self,
        chat_id: &str,
        image_path: &str,
    ) -> Result<String, String> {
        let image_key = self.upload_image(image_path).await?;
        let content = serde_json::json!({"image_key": image_key});
        self.send_message(chat_id, "image", &content).await
    }

    /// Send a file message.
    pub async fn send_file(
        &self,
        chat_id: &str,
        file_path: &str,
    ) -> Result<String, String> {
        let file_key = self.upload_file(file_path, "stream").await?;
        let content = serde_json::json!({"file_key": file_key});
        self.send_message(chat_id, "file", &content).await
    }

    /// Send an interactive card message.
    pub async fn send_interactive_card(
        &self,
        chat_id: &str,
        card_json: &Value,
    ) -> Result<String, String> {
        self.send_message(chat_id, "interactive", card_json).await
    }

    /// Edit an existing message.
    pub async fn edit_message(
        &self,
        message_id: &str,
        content: &Value,
        msg_type: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;

        let resp = self
            .client
            .patch(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}",
                message_id
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "content": content.to_string(),
                "msg_type": msg_type,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to edit message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Edit failed: HTTP {}", status));
        }

        debug!("Feishu message {message_id} edited");
        Ok("ok".to_string())
    }

    /// Add an ACK emoji reaction to a message.
    ///
    /// Mirrors Python `_add_ack_reaction()` (feishu.py:~1860).
    /// Fails silently — ACK is best-effort.
    pub async fn add_ack_reaction(&self, message_id: &str) {
        let token = match self.get_access_token().await {
            Ok(t) => t,
            Err(e) => {
                debug!("[Feishu] ACK reaction skipped: no token ({e})");
                return;
            }
        };
        let resp = self
            .client
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/reactions"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "reaction_type": {"emoji_type": "OK"}
            }))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                debug!("[Feishu] ACK reaction added to {message_id}");
            }
            Ok(r) => {
                debug!("[Feishu] ACK reaction failed: HTTP {}", r.status());
            }
            Err(e) => {
                debug!("[Feishu] ACK reaction request error: {e}");
            }
        }
    }

    /// Detect if text contains markdown and send as post if so, otherwise text.
    pub async fn send_text_or_post(&self,
        chat_id: &str,
        text: &str,
    ) -> Result<String, String> {
        if looks_like_markdown(text) {
            self.send_post(chat_id, "", text).await
        } else {
            self.send_text(chat_id, text).await
        }
    }

    /// Extract media keys from a Feishu content object.
    fn extract_media_keys(content_obj: &Value, msg_type: &str) -> Vec<MediaKey> {
        let mut keys = Vec::new();
        // Image
        if let Some(key) = content_obj.get("image_key").and_then(|v| v.as_str()) {
            keys.push(MediaKey {
                media_type: "image".to_string(),
                key: key.to_string(),
                name: None,
            });
        }
        // File (skip if this is an audio message to avoid double-counting)
        if msg_type != "audio" {
            if let Some(key) = content_obj.get("file_key").and_then(|v| v.as_str()) {
                let name = content_obj.get("file_name").and_then(|v| v.as_str()).map(String::from);
                keys.push(MediaKey {
                    media_type: "file".to_string(),
                    key: key.to_string(),
                    name,
                });
            }
        }
        // Audio (uses file_key but different semantics)
        if msg_type == "audio" {
            if let Some(key) = content_obj.get("file_key").and_then(|v| v.as_str()) {
                keys.push(MediaKey {
                    media_type: "audio".to_string(),
                    key: key.to_string(),
                    name: None,
                });
            }
        }
        keys
    }

    /// Download a media file from Feishu API.
    async fn download_media(&self, media_key: &str, media_type: &str) -> Result<Vec<u8>, String> {
        let token = self.get_access_token().await?;
        let url = match media_type {
            "image" => format!("https://open.feishu.cn/open-apis/im/v1/images/{}", media_key),
            "file" | "audio" => format!("https://open.feishu.cn/open-apis/im/v1/files/{}", media_key),
            _ => return Err("Unknown media type".to_string()),
        };

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to download media: {e}"))?;

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read media body: {e}"))?;
        Ok(bytes.to_vec())
    }

    /// Download and cache a Feishu media file to disk.
    /// Compute a short content hash for cache deduplication.
    fn content_hash(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(&hasher.finalize()[..8])
    }

    async fn download_and_cache_media(
        &self,
        media_key: &MediaKey,
    ) -> Option<String> {
        let bytes = self.download_media(&media_key.key, &media_key.media_type)
            .await
            .ok()?;

        let cache_dir = hermes_core::get_hermes_home().join("feishu").join("media");
        tokio::fs::create_dir_all(&cache_dir).await.ok()?;

        let ext = match media_key.media_type.as_str() {
            "image" => "jpg",
            "audio" => "mp3",
            "file" => media_key.name.as_deref().and_then(|n| n.rsplit('.').next()).unwrap_or("bin"),
            _ => "bin",
        };

        let hash = Self::content_hash(&bytes);
        let file_name = format!("{}_{}.{}", hash, media_key.media_type, ext);
        let path = cache_dir.join(&file_name);

        // Skip write if already cached (dedup)
        if !path.exists() {
            tokio::fs::write(&path, bytes).await.ok()?;
        }
        Some(path.to_string_lossy().to_string())
    }

    /// Build a `FeishuMessageEvent` from a normalized payload.
    /// Handles both webhook (`payload.message.*`) and WS (`payload.event.message.*`) shapes.
    async fn build_event_from_payload(&self, payload: &Value) -> Option<FeishuMessageEvent> {
        // Support both webhook and WS event shapes
        let message = payload
            .get("message")
            .or_else(|| payload.get("event").and_then(|e| e.get("message")));
        let sender = payload
            .get("sender")
            .or_else(|| payload.get("event").and_then(|e| e.get("sender")));

        let msg_id = message
            .and_then(|m| m.get("message_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !msg_id.is_empty() && self.persistent_dedup.is_duplicate(&msg_id) {
            debug!("[Feishu] Persistent dedup: skipping {msg_id}");
            return None;
        }

        // Also check in-memory dedup as a secondary guard
        if !msg_id.is_empty() && self.dedup.is_duplicate(&msg_id) {
            debug!("[Feishu] Memory dedup: skipping {msg_id}");
            return None;
        }

        let chat_id = message
            .and_then(|m| m.get("chat_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let sender_id = sender
            .and_then(|s| s.get("sender_id"))
            .and_then(|s| s.get("open_id"))
            .or_else(|| sender.and_then(|s| s.get("open_id")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Sender name: try cache first, then payload nickname
        let sender_name = if let Some(cached) = self.sender_name_cache.get(&sender_id) {
            Some(cached)
        } else {
            let name = sender
                .and_then(|s| s.get("nickname"))
                .or_else(|| sender.and_then(|s| s.get("name")))
                .and_then(|v| v.as_str())
                .map(String::from);
            if let Some(ref n) = name {
                self.sender_name_cache.insert(sender_id.clone(), n.clone());
            }
            name
        };

        let content_type = message
            .and_then(|m| m.get("message_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();

        let content_str = message
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let is_group = message
            .and_then(|m| m.get("chat_type"))
            .and_then(|v| v.as_str())
            .map(|t| t == "group")
            .unwrap_or(false);

        // Extract mentions from payload
        let mut mentions: Vec<String> = Vec::new();
        if let Some(mentions_arr) = message.and_then(|m| m.get("mentions")).and_then(|v| v.as_array()) {
            for m in mentions_arr {
                if let Some(open_id) = m
                    .get("id")
                    .and_then(|id| id.get("open_id"))
                    .and_then(|v| v.as_str())
                {
                    mentions.push(open_id.to_string());
                }
            }
        }

        // Normalize the message content
        let normalized = normalize_feishu_message(&content_type, &content_str);

        // Download media attachments from normalized message
        let mut media_keys = Vec::new();
        let mut media_paths = Vec::new();

        for image_key in &normalized.image_keys {
            let mk = MediaKey {
                media_type: "image".to_string(),
                key: image_key.clone(),
                name: None,
            };
            if let Some(path) = self.download_and_cache_media(&mk).await {
                media_paths.push(format!("[image: {}]", path));
            }
            media_keys.push(mk);
        }

        for media_ref in &normalized.media_refs {
            let mk = MediaKey {
                media_type: media_ref.resource_type.clone(),
                key: media_ref.file_key.clone(),
                name: if media_ref.file_name.is_empty() {
                    None
                } else {
                    Some(media_ref.file_name.clone())
                },
            };
            if let Some(path) = self.download_and_cache_media(&mk).await {
                media_paths.push(format!("[{}: {}]", media_ref.resource_type, path));
            }
            media_keys.push(mk);
        }

        let mut final_content = normalized.text_content;
        if !media_paths.is_empty() {
            if !final_content.is_empty() {
                final_content.push('\n');
            }
            final_content.push_str(&media_paths.join("\n"));
        }

        // Record in both dedup systems
        if !msg_id.is_empty() {
            self.dedup.insert(msg_id.clone());
        }

        Some(FeishuMessageEvent {
            message_id: msg_id,
            chat_id,
            sender_id,
            sender_name,
            content: final_content,
            msg_type: content_type,
            is_group,
            mentions,
            media_keys,
        })
    }

    /// Process an inbound webhook event and return a message event.
    pub async fn handle_inbound(&self, payload: &Value) -> Option<FeishuMessageEvent> {
        self.build_event_from_payload(payload).await
    }

    /// Process a WebSocket event and dispatch through the adapter pipeline.
    pub async fn process_ws_event(&self, event: Value) {
        let event_type = event
            .get("header")
            .and_then(|h| h.get("event_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "im.message.receive_v1" => {
                if let Some(message_event) = self.build_event_from_payload(&event).await {
                    // Group policy check
                    if message_event.is_group
                        && !self.is_group_message_allowed(&message_event.sender_id, &message_event.chat_id)
                    {
                        debug!(
                            "[Feishu WS] Group message from {} blocked by policy",
                            message_event.sender_id
                        );
                        return;
                    }

                    // Mention gating for groups
                    if message_event.is_group {
                        let mentions_bot = self.message_mentions_bot(&message_event.mentions)
                            || self.post_mentions_bot(&message_event.mentions);
                        if !mentions_bot {
                            debug!(
                                "[Feishu WS] Group message from {} ignored: no bot mention",
                                message_event.sender_id
                            );
                            return;
                        }
                    }

                    let msg_id = message_event.message_id.clone();

                    // Route through on_message callback (same as webhook)
                    if let Some(ref cb) = *self.on_message.read().await {
                        cb(message_event);
                    }

                    // Best-effort ACK reaction
                    let adapter = self.clone_for_webhook();
                    tokio::spawn(async move {
                        adapter.add_ack_reaction(&msg_id).await;
                    });
                }
            }
            "card.action.trigger" => {
                if let Some(action) = event.get("action") {
                    let action_value = action.get("value").cloned().unwrap_or(Value::Null);
                    let action_tag = action
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    info!(
                        "[Feishu WS] Card action triggered: tag={action_tag}, value={action_value}"
                    );

                    // Handle Hermes approval card actions inline
                    if let Some(hermes_action) = action_value
                        .get("hermes_action")
                        .and_then(|v| v.as_str())
                    {
                        let open_id = event
                            .get("open_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let dedup_key = format!(
                            "{}:{}",
                            event
                                .get("open_message_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            hermes_action
                        );
                        {
                            let mut dedup = self.card_action_dedup.lock().await;
                            let now = std::time::Instant::now();
                            dedup.retain(|_, expiry| *expiry > now);
                            if dedup.contains_key(&dedup_key) {
                                return;
                            }
                            dedup.insert(
                                dedup_key,
                                now + std::time::Duration::from_secs(CARD_ACTION_DEDUP_TTL_SECONDS),
                            );
                        }

                        let approval_id = action_value
                            .get("approval_id")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let choice = APPROVAL_CHOICE_MAP
                            .iter()
                            .find(|(k, _)| *k == hermes_action)
                            .map(|(_, v)| *v)
                            .unwrap_or("deny");

                        let adapter = self.clone_for_webhook();
                        let open_id_for_spawn = open_id.clone();
                        tokio::spawn(async move {
                            adapter.resolve_approval(approval_id, choice, &open_id_for_spawn).await;
                        });

                        // TODO: send resolved card update back via WS response if needed
                        let _ = (action_tag, action_value, open_id);
                    } else if let Some(ref cb) = *self.on_card_action.read().await {
                        let card_event = FeishuCardActionEvent {
                            action_tag,
                            action_value,
                            open_id: event
                                .get("open_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            open_message_id: event
                                .get("open_message_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            tenant_key: event
                                .get("tenant_key")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                        };
                        cb(card_event);
                    }
                }
            }
            "im.chat.member.bot.added_v1" => {
                let chat_id = event
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu WS] Bot added to chat: {chat_id}");
            }
            "im.chat.member.bot.deleted_v1" => {
                let chat_id = event
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu WS] Bot removed from chat: {chat_id}");
            }
            "im.message.reaction.created_v1" | "im.message.reaction.deleted_v1" => {
                if let Some(reaction) = event.get("event") {
                    let emoji = reaction
                        .get("reaction_type")
                        .and_then(|v| v.get("emoji_type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let operator_type = reaction
                        .get("operator_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Ignore bot/app reactions and our own ACK emoji to avoid loops
                    if operator_type == "bot" || operator_type == "app" || emoji == "OK" {
                        debug!(
                            "[Feishu WS] Reaction ignored: operator_type={operator_type}, emoji={emoji}"
                        );
                        return;
                    }
                    let operator_id = reaction
                        .get("operator")
                        .and_then(|v| v.get("operator_id"))
                        .and_then(|v| v.get("open_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let message_id = reaction
                        .get("message_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let chat_id = reaction
                        .get("chat_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let action = if event_type == "im.message.reaction.created_v1" {
                        "added"
                    } else {
                        "removed"
                    };
                    debug!("[Feishu WS] Reaction {action} by {operator_id}: {emoji}");

                    if !message_id.is_empty() && !chat_id.is_empty() {
                        let synthetic = FeishuMessageEvent {
                            message_id: message_id.to_string(),
                            chat_id: chat_id.to_string(),
                            sender_id: operator_id.to_string(),
                            sender_name: None,
                            content: format!("reaction:{action}:{emoji}"),
                            msg_type: "text".to_string(),
                            is_group: true,
                            mentions: Vec::new(),
                            media_keys: Vec::new(),
                        };
                        if let Some(ref cb) = *self.on_message.read().await {
                            info!("[Feishu WS] Routing reaction {action}:{emoji} as synthetic event");
                            cb(synthetic);
                        }
                    }
                }
            }
            "drive.notice.comment_add_v1" => {
                match self.get_access_token().await {
                    Ok(token) => {
                        let client = &self.client;
                        let self_open_id = &self.config.bot_open_id;
                        crate::platforms::feishu_comment::handle_drive_comment_event(
                            client, &token, &event, self_open_id,
                        ).await;
                    }
                    Err(e) => {
                        warn!("[Feishu WS] Cannot handle comment event: failed to get access token: {e}");
                    }
                }
            }
            "im.message.message_read_v1" => {
                debug!("[Feishu WS] Message read event");
            }
            _ => {
                debug!("[Feishu WS] Unknown event: {event_type}");
            }
        }
    }

    /// Start the Feishu WebSocket connection loop.
    ///
    /// Mirrors the Python WebSocket mode. Dispatches events through
    /// the same normalization, dedup, and policy pipeline as webhooks.
    pub async fn run_ws(&self) {
        let ws_client = crate::platforms::feishu_ws::FeishuWsClient::new(self.config.clone());
        let adapter = self.clone_for_webhook();
        let callback = Arc::new(move |event: Value| {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                adapter.process_ws_event(event).await;
            });
        });
        ws_client.run(callback).await;
    }

    /// Check group policy for inbound messages with per-group rules.
    ///
    /// Mirrors Python `_allow_group_message()` (feishu.py:3201).
    fn is_group_message_allowed(&self, sender_id: &str, chat_id: &str) -> bool {
        // Admins bypass all checks
        if self.config.admins.contains(sender_id) {
            return true;
        }

        let (policy, allowlist, blacklist) = if let Some(rule) = self.config.group_rules.get(chat_id) {
            (
                rule.policy.as_str(),
                &rule.allowlist,
                &rule.blacklist,
            )
        } else {
            (
                match self.config.group_policy {
                    GroupPolicy::Open => "open",
                    GroupPolicy::Allowlist => "allowlist",
                    GroupPolicy::Blacklist => "blacklist",
                    GroupPolicy::AdminOnly => "admin_only",
                    GroupPolicy::Disabled => "disabled",
                },
                &self.config.allowed_users,
                &HashSet::new(),
            )
        };

        match policy {
            "disabled" => false,
            "open" => true,
            "admin_only" => false,
            "allowlist" => allowlist.contains(sender_id),
            "blacklist" => !blacklist.contains(sender_id),
            _ => self.config.allowed_users.contains(sender_id),
        }
    }

    /// Check if message mentions the bot.
    ///
    /// Mirrors Python `_message_mentions_bot()` (feishu.py:3252).
    fn message_mentions_bot(&self, mentions: &[String]) -> bool {
        if self.config.bot_open_id.is_empty()
            && self.config.bot_user_id.is_empty()
            && self.config.bot_name.is_empty()
        {
            return true; // No bot identity configured, accept all
        }
        mentions.iter().any(|m| {
            (!self.config.bot_open_id.is_empty() && m == &self.config.bot_open_id)
                || (!self.config.bot_user_id.is_empty() && m == &self.config.bot_user_id)
                || (!self.config.bot_name.is_empty() && m == &self.config.bot_name)
        })
    }

    /// Check if post message mentions include the bot.
    fn post_mentions_bot(&self, mentioned_ids: &[String]) -> bool {
        if mentioned_ids.is_empty() {
            return false;
        }
        if !self.config.bot_open_id.is_empty() && mentioned_ids.contains(&self.config.bot_open_id) {
            return true;
        }
        if !self.config.bot_user_id.is_empty() && mentioned_ids.contains(&self.config.bot_user_id) {
            return true;
        }
        false
    }

    /// Verify Feishu webhook signature.
    ///
    /// Mirrors Python `_is_webhook_signature_valid()` (feishu.py:2452).
    /// SHA256(timestamp + nonce + encrypt_key + body) == signature.
    fn is_signature_valid(&self, headers: &HeaderMap, body: &[u8]) -> bool {
        if self.config.encrypt_key.is_empty() {
            return true; // No encryption configured, skip verification
        }

        use sha2::{Digest, Sha256};

        let timestamp = headers
            .get("x-lark-request-timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let nonce = headers
            .get("x-lark-request-nonce")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let signature = headers
            .get("x-lark-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(self.config.encrypt_key.as_bytes());
        hasher.update(body);
        let computed = hex::encode(hasher.finalize());

        // Timing-safe comparison
        computed == signature
    }

    /// Start the Feishu webhook HTTP server.
    ///
    /// Mirrors Python `_handle_webhook_request()` (feishu.py:2358).
    /// Listens on the configured port/path and dispatches inbound messages
    /// to the `on_message` callback.
    pub async fn run_webhook(
        &self,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let path = self.config.webhook_path.clone();
        let adapter = Arc::new(self.clone_for_webhook());

        let app = Router::new()
            .route(&path, axum::routing::post(move |headers: HeaderMap, body: Bytes| {
                let adapter = adapter.clone();
                async move {
                    adapter.handle_webhook_request(&headers, &body).await
                }
            }));

        let addr = format!("0.0.0.0:{}", self.config.webhook_port);
        info!("Feishu webhook listening on {addr}{path}");

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("Failed to bind to {addr}: {e}"))?;

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .map_err(|e| format!("Feishu webhook server error: {e}"))
    }

    /// Clone the adapter for webhook server use (shares state via Arc).
    fn clone_for_webhook(&self) -> FeishuAdapter {
        FeishuAdapter {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            persistent_dedup: self.persistent_dedup.clone(),
            sender_name_cache: self.sender_name_cache.clone(),
            access_token: self.access_token.clone(),
            on_message: self.on_message.clone(),
            on_card_action: self.on_card_action.clone(),
            text_batches: self.text_batches.clone(),
            batch_timers: self.batch_timers.clone(),
            approval_state: self.approval_state.clone(),
            approval_counter: self.approval_counter.clone(),
            card_action_dedup: self.card_action_dedup.clone(),
        }
    }

    /// Handle a single webhook request.
    ///
    /// Mirrors Python `_handle_webhook_request()` (feishu.py:2358).
    async fn handle_webhook_request(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
    ) -> (StatusCode, Json<Value>) {
        // Content-Type guard
        let content_type = headers
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if !content_type.is_empty() && content_type != "application/json" {
            warn!("[Feishu] Webhook rejected: unexpected Content-Type {content_type:?}");
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(serde_json::json!({"code": 415})));
        }

        // Body size guard
        if body.len() > FEISHU_WEBHOOK_MAX_BODY_BYTES {
            warn!("[Feishu] Webhook body too large: {} bytes", body.len());
            return (StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({"code": 413})));
        }

        // Parse JSON
        let payload: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                warn!("[Feishu] Invalid webhook JSON: {e}");
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code": 400, "msg": "invalid json"})));
            }
        };

        // URL verification challenge (Feishu setup)
        if payload.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
            let challenge = payload.get("challenge").cloned().unwrap_or(Value::Null);
            return (StatusCode::OK, Json(serde_json::json!({"challenge": challenge})));
        }

        // Verification token check
        if !self.config.verification_token.is_empty() {
            let header = payload.get("header").and_then(|v| v.as_object());
            let incoming_token = header
                .and_then(|h| h.get("token").and_then(|v| v.as_str()))
                .or_else(|| payload.get("token").and_then(|v| v.as_str()))
                .unwrap_or("");
            if incoming_token != self.config.verification_token {
                warn!("[Feishu] Webhook rejected: invalid verification token");
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"code": 401})));
            }
        }

        // Signature verification
        if !self.is_signature_valid(headers, body) {
            warn!("[Feishu] Webhook rejected: invalid signature");
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"code": 401})));
        }

        // Encrypted payload not supported
        if payload.get("encrypt").is_some() {
            error!("[Feishu] Encrypted webhook payloads are not supported");
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code": 400, "msg": "encrypted not supported"})));
        }

        // Route by event type
        let event_type = payload
            .get("header")
            .and_then(|h| h.get("event_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "im.message.receive_v1" => {
                if let Some(event) = self.handle_inbound(&payload).await {
                    // Group policy check
                    if event.is_group && !self.is_group_message_allowed(&event.sender_id, &event.chat_id) {
                        debug!("[Feishu] Group message from {} blocked by policy", event.sender_id);
                        return (StatusCode::OK, Json(serde_json::json!({"code": 0, "msg": "ok"})));
                    }

                    let msg_id = event.message_id.clone();

                    // Auto-merge rapid successive text messages (batching)
                    if event.msg_type == "text" {
                        let chat_id = event.chat_id.clone();
                        let mut batches = self.text_batches.lock().await;
                        let mut timers = self.batch_timers.lock().await;

                        if let Some(existing) = batches.get_mut(&chat_id) {
                            existing.content = format!("{}\n{}", existing.content, event.content);
                            if let Some(old) = timers.remove(&chat_id) {
                                old.abort();
                            }
                        } else {
                            batches.insert(chat_id.clone(), event);
                        }

                        let batches_clone = self.text_batches.clone();
                        let timers_clone = self.batch_timers.clone();
                        let on_message_clone = self.on_message.clone();
                        let chat_id_for_timer = chat_id.clone();
                        let handle = tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(TEXT_BATCH_DELAY_MS)).await;
                            let mut batches = batches_clone.lock().await;
                            if let Some(ev) = batches.remove(&chat_id_for_timer) {
                                if let Some(ref cb) = *on_message_clone.read().await {
                                    cb(ev);
                                }
                            }
                            let mut timers = timers_clone.lock().await;
                            timers.remove(&chat_id_for_timer);
                        });
                        timers.insert(chat_id, handle.abort_handle());
                    } else {
                        // Non-text: flush pending batch for this chat first
                        let chat_id = event.chat_id.clone();
                        let mut batches = self.text_batches.lock().await;
                        let mut timers = self.batch_timers.lock().await;
                        if let Some(old) = timers.remove(&chat_id) {
                            old.abort();
                        }
                        if let Some(batch) = batches.remove(&chat_id) {
                            if let Some(ref cb) = *self.on_message.read().await {
                                cb(batch);
                            }
                        }
                        if let Some(ref cb) = *self.on_message.read().await {
                            cb(event);
                        }
                    }

                    // Best-effort ACK reaction (fire-and-forget)
                    let adapter = self.clone_for_webhook();
                    tokio::spawn(async move {
                        adapter.add_ack_reaction(&msg_id).await;
                    });
                }
            }
            "card.action.trigger" => {
                if let Some(action) = payload.get("action") {
                    let action_value = action.get("value").cloned().unwrap_or(Value::Null);
                    let action_tag = action
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    info!(
                        "[Feishu] Card action triggered: tag={action_tag}, value={action_value}"
                    );

                    // Handle Hermes approval card actions inline
                    if let Some(hermes_action) = action_value
                        .get("hermes_action")
                        .and_then(|v| v.as_str())
                    {
                        let open_id = payload
                            .get("open_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        // Deduplicate card actions by open_message_id + action
                        let dedup_key = format!(
                            "{}:{}",
                            payload
                                .get("open_message_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            hermes_action
                        );
                        {
                            let mut dedup = self.card_action_dedup.lock().await;
                            let now = std::time::Instant::now();
                            // Clean expired entries
                            dedup.retain(|_, expiry| *expiry > now);
                            if dedup.contains_key(&dedup_key) {
                                return (
                                    StatusCode::OK,
                                    Json(serde_json::json!({"code": 0, "msg": "dedup"})),
                                );
                            }
                            dedup.insert(
                                dedup_key,
                                now + std::time::Duration::from_secs(CARD_ACTION_DEDUP_TTL_SECONDS),
                            );
                        }

                        let approval_id = action_value
                            .get("approval_id")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let choice = APPROVAL_CHOICE_MAP
                            .iter()
                            .find(|(k, _)| *k == hermes_action)
                            .map(|(_, v)| *v)
                            .unwrap_or("deny");

                        // Resolve approval asynchronously
                        let adapter = self.clone_for_webhook();
                        let open_id_for_spawn = open_id.clone();
                        tokio::spawn(async move {
                            adapter.resolve_approval(approval_id, choice, &open_id_for_spawn).await;
                        });

                        // Return updated card synchronously
                        let card = Self::build_resolved_approval_card(choice, &open_id);
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "toast": {
                                    "type": "success",
                                    "content": APPROVAL_LABEL_MAP.iter().find(|(k, _)| *k == choice).map(|(_, v)| *v).unwrap_or("Resolved"),
                                },
                                "card": {
                                    "type": "raw",
                                    "data": card,
                                },
                            })),
                        );
                    }

                    // Route non-Hermes card actions to registered handler
                    let event = FeishuCardActionEvent {
                        action_tag,
                        action_value,
                        open_id: payload
                            .get("open_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        open_message_id: payload
                            .get("open_message_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        tenant_key: payload
                            .get("tenant_key")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    };
                    if let Some(ref cb) = *self.on_card_action.read().await {
                        cb(event);
                    }
                }
            }
            "im.chat.member.bot.added_v1" => {
                let chat_id = payload
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu] Bot added to chat: {chat_id}");
                // Optionally send a welcome message
                let _ = self.send_text(chat_id, "Hello! I'm Hermes Agent. How can I help you today?").await;
            }
            "im.chat.member.bot.deleted_v1" => {
                let chat_id = payload
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu] Bot removed from chat: {chat_id}");
            }
            "im.message.reaction.created_v1" | "im.message.reaction.deleted_v1" => {
                if let Some(reaction) = payload.get("event") {
                    let emoji = reaction.get("reaction_type")
                        .and_then(|v| v.get("emoji_type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let operator_type = reaction.get("operator_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Ignore bot/app reactions and our own ACK emoji to avoid loops
                    if operator_type == "bot" || operator_type == "app" || emoji == "OK" {
                        debug!("[Feishu] Reaction ignored: operator_type={operator_type}, emoji={emoji}");
                        return (StatusCode::OK, Json(serde_json::json!({"code": 0, "msg": "ok"})));
                    }
                    let operator_id = reaction.get("operator")
                        .and_then(|v| v.get("operator_id"))
                        .and_then(|v| v.get("open_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let message_id = reaction.get("message_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let chat_id = reaction.get("chat_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let action = if event_type == "im.message.reaction.created_v1" { "added" } else { "removed" };
                    debug!("[Feishu] Reaction {action} by {operator_id}: {emoji}");

                    if !message_id.is_empty() && !chat_id.is_empty() {
                        let synthetic = FeishuMessageEvent {
                            message_id: message_id.to_string(),
                            chat_id: chat_id.to_string(),
                            sender_id: operator_id.to_string(),
                            sender_name: None,
                            content: format!("reaction:{action}:{emoji}"),
                            msg_type: "text".to_string(),
                            is_group: true,
                            mentions: Vec::new(),
                            media_keys: Vec::new(),
                        };
                        if let Some(ref cb) = *self.on_message.read().await {
                            info!("[Feishu] Routing reaction {action}:{emoji} as synthetic event");
                            cb(synthetic);
                        }
                    }
                }
            }
            "im.message.message_read_v1" => {
                debug!("[Feishu] Message read event");
            }
            _ => {
                debug!("[Feishu] Unknown event type: {event_type}");
            }
        }

        (StatusCode::OK, Json(serde_json::json!({"code": 0, "msg": "ok"})))
    }

    pub fn is_configured(&self) -> bool {
        !self.config.app_id.is_empty() && !self.config.app_secret.is_empty()
    }
}

/// Check if text contains simple markdown markers.
///
/// Only counts markers at the start of a line (headings, lists, quotes)
/// or inline formatting sequences (bold, code) to avoid false positives
/// like "Issue #123" or "2024-01-01".
pub(crate) fn looks_like_markdown(text: &str) -> bool {
    text.contains("**")
        || text.contains("__")
        || text.contains("`")
        || text.lines().any(|line| {
            let t = line.trim_start();
            t.starts_with("# ")
                || t.starts_with("## ")
                || t.starts_with("### ")
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("| ")
                || t.starts_with("> ")
        })
}

/// Build a Feishu post payload from plain text.
///
/// Does a best-effort conversion of simple markdown to post segments.
pub(crate) fn build_post_payload(title: &str, text: &str) -> Value {
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut segments = Vec::new();

        // Simple heuristic: heading lines
        if trimmed.starts_with("# ") {
            segments.push(serde_json::json!({
                "tag": "text",
                "text": trimmed.strip_prefix("# ").unwrap_or(trimmed),
                "style": {"bold": true, "underline": true}
            }));
        } else if trimmed.starts_with("## ") {
            segments.push(serde_json::json!({
                "tag": "text",
                "text": trimmed.strip_prefix("## ").unwrap_or(trimmed),
                "style": {"bold": true}
            }));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            segments.push(serde_json::json!({
                "tag": "text",
                "text": format!("• {}", trimmed[2..].trim()),
            }));
        } else {
            // Plain text segment
            segments.push(serde_json::json!({
                "tag": "text",
                "text": line,
            }));
        }
        lines.push(segments);
    }

    // If no lines parsed, add the raw text as a single segment
    if lines.is_empty() {
        lines.push(vec![serde_json::json!({
            "tag": "text",
            "text": text,
        })]);
    }

    let mut payload = serde_json::json!({
        "zh_cn": {
            "content": lines,
        }
    });

    if !title.is_empty() {
        payload["zh_cn"]["title"] = serde_json::Value::String(title.to_string());
    }

    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env() {
        let config = FeishuConfig::from_env();
        assert_eq!(config.webhook_port, 8765);
        assert_eq!(config.webhook_path, "/feishu/webhook");
    }

    #[test]
    fn test_not_configured_when_empty() {
        let config = FeishuConfig::from_env();
        let adapter = FeishuAdapter::new(config);
        assert!(!adapter.is_configured());
    }

    #[test]
    fn test_group_policy_open() {
        let config = FeishuConfig {
            group_policy: GroupPolicy::Open,
            ..FeishuConfig::from_env()
        };
        let adapter = FeishuAdapter::new(config);
        assert!(adapter.is_group_message_allowed("any_user", "chat1"));
    }

    #[test]
    fn test_group_policy_allowlist() {
        let mut allowed = HashSet::new();
        allowed.insert("user1".to_string());
        let config = FeishuConfig {
            group_policy: GroupPolicy::Allowlist,
            allowed_users: allowed,
            ..FeishuConfig::from_env()
        };
        let adapter = FeishuAdapter::new(config);
        assert!(adapter.is_group_message_allowed("user1", "chat1"));
        assert!(!adapter.is_group_message_allowed("user2", "chat1"));
    }

    #[test]
    fn test_group_policy_disabled() {
        let config = FeishuConfig {
            group_policy: GroupPolicy::Disabled,
            ..FeishuConfig::from_env()
        };
        let adapter = FeishuAdapter::new(config);
        assert!(!adapter.is_group_message_allowed("any_user", "chat1"));
    }

    #[test]
    fn test_signature_verification() {
        use sha2::{Digest, Sha256};

        let config = FeishuConfig {
            encrypt_key: "test_encrypt_key".to_string(),
            ..FeishuConfig::from_env()
        };
        let adapter = FeishuAdapter::new(config);

        let body = b"test body";
        let timestamp = "1234567890";
        let nonce = "abc123";

        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update("test_encrypt_key".as_bytes());
        hasher.update(body);
        let expected_sig = hex::encode(hasher.finalize());

        let mut headers = HeaderMap::new();
        headers.insert("x-lark-request-timestamp", timestamp.parse().unwrap());
        headers.insert("x-lark-request-nonce", nonce.parse().unwrap());
        headers.insert("x-lark-signature", expected_sig.parse().unwrap());

        assert!(adapter.is_signature_valid(&headers, body));
    }

    #[test]
    fn test_extract_media_keys_image_and_file() {
        let content = serde_json::json!({
            "image_key": "img_123",
            "file_key": "file_456",
            "file_name": "report.pdf",
        });
        let keys = FeishuAdapter::extract_media_keys(&content, "text");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].media_type, "image");
        assert_eq!(keys[0].key, "img_123");
        assert_eq!(keys[1].media_type, "file");
        assert_eq!(keys[1].key, "file_456");
        assert_eq!(keys[1].name.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn test_extract_media_keys_audio_no_double_count() {
        let content = serde_json::json!({
            "file_key": "file_789",
            "file_name": "voice.mp3",
        });
        let keys = FeishuAdapter::extract_media_keys(&content, "audio");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].media_type, "audio");
        assert_eq!(keys[0].key, "file_789");
    }

    #[test]
    fn test_extract_media_keys_empty() {
        let content = serde_json::json!({"text": "hello"});
        let keys = FeishuAdapter::extract_media_keys(&content, "text");
        assert!(keys.is_empty());
    }

    #[test]
    fn test_looks_like_markdown_positive() {
        assert!(looks_like_markdown("# Heading\nSome text"));
        assert!(looks_like_markdown("**bold** text"));
        assert!(looks_like_markdown("`code`"));
        assert!(looks_like_markdown("- list item"));
    }

    #[test]
    fn test_looks_like_markdown_negative() {
        assert!(!looks_like_markdown("Issue #123 is fixed"));
        assert!(!looks_like_markdown("Date: 2024-01-01"));
        assert!(!looks_like_markdown("Asterisk * in middle"));
        assert!(!looks_like_markdown("Pipe | separator"));
    }

    #[test]
    fn test_build_post_payload() {
        let payload = build_post_payload("Title", "# Hello\n- Item 1\n- Item 2");
        let zh = payload.get("zh_cn").unwrap();
        let title = zh.get("title").and_then(|v| v.as_str());
        assert_eq!(title, Some("Title"));
        let content = zh.get("content").and_then(|v| v.as_array()).unwrap();
        assert_eq!(content.len(), 3);
    }

    #[test]
    fn test_build_post_payload_no_title() {
        let payload = build_post_payload("", "Plain text");
        assert!(payload.get("zh_cn").unwrap().get("title").is_none());
    }
}
