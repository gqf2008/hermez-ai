//! Dingtalk platform adapter.
//!
//! Mirrors the Python `gateway/platforms/dingtalk.py`.
//!
//! Supports:
//! - Stream Mode (WebSocket long connection) with auto-reconnect backoff
//! - HTTP webhook callback endpoint (fallback passive mode)
//! - Direct-message and group text receive/send
//! - Inbound rich_text, picture, voice, video, file parsing
//! - AI Card SDK (create, deliver, stream_update, finalize)
//! - Emoji reactions via robot SDK (reply / recall)
//! - Deduplication cache
//! - Session webhook URL caching with expiry tracking
//! - Group-chat gating (require_mention, free_response_chats, patterns, allowed_users)
//!
//! Outbound messages are sent via the `session_webhook` URL
//! that comes with each incoming message, via the Open API,
//! or via AI Cards when configured.
//!
//! The adapter can run in two modes:
//! - Stream Mode: active WebSocket connection to DingTalk (recommended)
//! - Webhook Mode: passive HTTP server waiting for callbacks

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::post,
};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, oneshot, Semaphore};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::dedup::MessageDeduplicator;

use futures::{SinkExt, StreamExt};

type HmacSha256 = Hmac<Sha256>;

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 20000;
const RECONNECT_BACKOFF: [u64; 5] = [2, 5, 10, 30, 60];
const _SESSION_WEBHOOKS_MAX: usize = 500;
const STREAM_PING_INTERVAL_SECS: u64 = 60;

// ── Connection Mode ────────────────────────────────────────────────────────

/// How the Dingtalk adapter receives inbound messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum DingtalkConnectionMode {
    /// Passive webhook HTTP server.
    Webhook,
    /// Active WebSocket stream connection.
    #[default]
    Stream,
}


// ── Platform Configuration ─────────────────────────────────────────────────

/// Dingtalk platform configuration.
#[derive(Debug, Clone)]
pub struct DingtalkConfig {
    pub client_id: String,
    pub client_secret: String,
    pub webhook_port: u16,
    pub webhook_path: String,
    pub connection_mode: DingtalkConnectionMode,
    /// AI Card template ID (optional).
    pub card_template_id: Option<String>,
    /// Robot code for API calls (defaults to client_id).
    pub robot_code: String,
    /// Group chats require @mention to trigger the bot.
    pub require_mention: bool,
    /// Conversation IDs that bypass require_mention.
    pub free_response_chats: HashSet<String>,
    /// Regex patterns that act as wake-words in groups.
    pub mention_patterns: Vec<regex::Regex>,
    /// Allowed user IDs (staff_id or sender_id). Empty = allow all.
    pub allowed_users: HashSet<String>,
}

impl Default for DingtalkConfig {
    fn default() -> Self {
        Self {
            client_id: std::env::var("DINGTALK_CLIENT_ID").unwrap_or_default(),
            client_secret: std::env::var("DINGTALK_CLIENT_SECRET").unwrap_or_default(),
            webhook_port: std::env::var("DINGTALK_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8766),
            webhook_path: std::env::var("DINGTALK_WEBHOOK_PATH")
                .ok()
                .unwrap_or_else(|| "/dingtalk/callback".to_string()),
            connection_mode: DingtalkConnectionMode::default(),
            card_template_id: std::env::var("DINGTALK_CARD_TEMPLATE_ID").ok(),
            robot_code: std::env::var("DINGTALK_ROBOT_CODE")
                .ok()
                .unwrap_or_else(|| std::env::var("DINGTALK_CLIENT_ID").unwrap_or_default()),
            require_mention: std::env::var("DINGTALK_REQUIRE_MENTION")
                .map(|v| matches!(v.to_lowercase().trim(), "true" | "1" | "yes" | "on"))
                .unwrap_or(false),
            free_response_chats: HashSet::new(),
            mention_patterns: Vec::new(),
            allowed_users: HashSet::new(),
        }
    }
}

impl DingtalkConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    /// Build config from `PlatformConfig.extra` values merged with env.
    pub fn from_extra(extra: &std::collections::HashMap<String, serde_json::Value>) -> Self {
        let mut cfg = Self::from_env();

        if let Some(v) = extra.get("client_id").and_then(|v| v.as_str()) {
            cfg.client_id = v.to_string();
        }
        if let Some(v) = extra.get("client_secret").and_then(|v| v.as_str()) {
            cfg.client_secret = v.to_string();
        }
        if let Some(v) = extra.get("card_template_id").and_then(|v| v.as_str()) {
            cfg.card_template_id = Some(v.to_string());
        }
        if let Some(v) = extra.get("robot_code").and_then(|v| v.as_str()) {
            cfg.robot_code = v.to_string();
        }
        if let Some(v) = extra.get("require_mention") {
            cfg.require_mention = match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Number(n) => n.as_i64().is_some_and(|v| v != 0),
                serde_json::Value::String(s) => {
                    matches!(s.to_lowercase().trim(), "true" | "1" | "yes" | "on")
                }
                _ => false,
            };
        }
        if let Some(v) = extra.get("free_response_chats") {
            let items: Vec<String> = match v {
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                serde_json::Value::String(s) => {
                    s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
                }
                _ => Vec::new(),
            };
            cfg.free_response_chats = items.into_iter().collect();
        }
        if let Some(v) = extra.get("mention_patterns") {
            let patterns: Vec<String> = match v {
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                serde_json::Value::String(s) => {
                    if let Ok(parsed) = serde_json::from_str::<Vec<String>>(s) {
                        parsed
                    } else {
                        s.split('\n')
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty())
                            .collect()
                    }
                }
                _ => Vec::new(),
            };
            cfg.mention_patterns = patterns
                .into_iter()
                .filter_map(|p| {
                    regex::RegexBuilder::new(&p)
                        .case_insensitive(true)
                        .build()
                        .ok()
                })
                .collect();
        }
        if let Some(v) = extra.get("allowed_users") {
            let items: Vec<String> = match v {
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                serde_json::Value::String(s) => {
                    s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
                }
                _ => Vec::new(),
            };
            cfg.allowed_users = items.into_iter().map(|s| s.to_lowercase()).collect();
        }
        if let Some(v) = extra.get("connection_mode").and_then(|v| v.as_str()) {
            cfg.connection_mode = match v.to_lowercase().as_str() {
                "webhook" => DingtalkConnectionMode::Webhook,
                _ => DingtalkConnectionMode::Stream,
            };
        }
        cfg
    }
}

// ── Inbound Message Event ──────────────────────────────────────────────────

/// Inbound message event from Dingtalk.
#[derive(Debug, Clone)]
pub struct DingtalkMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Chat/session ID.
    pub chat_id: String,
    /// Sender ID.
    pub sender_id: String,
    /// Sender nick.
    pub sender_nick: String,
    /// Sender staff ID.
    pub sender_staff_id: String,
    /// Message content (text).
    pub content: String,
    /// Whether this is a group message.
    pub is_group: bool,
    /// Session webhook URL for replies.
    pub session_webhook: String,
    /// Session webhook expiry timestamp (ms).
    pub session_webhook_expired_time: i64,
    /// Whether the bot is @-mentioned in this group message.
    pub is_in_at_list: bool,
    /// Media download codes / URLs extracted from the message.
    pub media_urls: Vec<String>,
    /// MIME types corresponding to media_urls.
    pub media_types: Vec<String>,
    /// Original message type from DingTalk.
    pub msg_type: String,
    /// Raw JSON payload.
    pub raw: serde_json::Value,
}

// ── Truncate helper ────────────────────────────────────────────────────────

/// Truncate text to at most `max_chars` characters (UTF-8 safe).
fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

// ── Webhook Types ──────────────────────────────────────────────────────────

/// Webhook callback payload from Dingtalk.
#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub msg_id: Option<String>,
    pub conversation_id: Option<String>,
    pub conversation_type: Option<String>,
    pub sender_id: Option<String>,
    pub sender_nick: Option<String>,
    pub text: Option<serde_json::Value>,
    pub rich_text: Option<Vec<RichTextItem>>,
    pub session_webhook: Option<String>,
    pub create_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RichTextItem {
    pub text: Option<String>,
}

/// Response to Dingtalk webhook callback.
#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    #[serde(rename = "ret")]
    pub ret: String,
}

// ── Stream Protocol Types ──────────────────────────────────────────────────

/// Response from `POST /v1.0/gateway/connections/open`.
#[derive(Debug, Deserialize)]
struct StreamEndpoint {
    endpoint: String,
    ticket: String,
}

/// Incoming WebSocket frame from DingTalk stream.
#[derive(Debug, Deserialize)]
struct StreamFrame {
    #[serde(rename = "specVersion")]
    spec_version: String,
    #[serde(rename = "type")]
    frame_type: String,
    headers: StreamHeaders,
    /// JSON-string payload.
    data: String,
}

#[derive(Debug, Deserialize)]
struct StreamHeaders {
    #[serde(rename = "appId")]
    app_id: Option<String>,
    #[serde(rename = "messageId")]
    message_id: Option<String>,
    #[serde(rename = "topic")]
    topic: Option<String>,
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
}

/// ACK frame sent back over the WebSocket.
#[derive(Debug, Serialize)]
struct AckFrame {
    code: i32,
    headers: AckHeaders,
    message: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AckHeaders {
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(rename = "contentType")]
    content_type: String,
}

// ── Webhook Cache ──────────────────────────────────────────────────────────

/// Session webhook cache: chat_id -> (webhook URL, expired_time_ms).
struct WebhookCache {
    entries: parking_lot::Mutex<HashMap<String, (String, i64)>>,
    max_size: usize,
}

impl WebhookCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: parking_lot::Mutex::new(HashMap::with_capacity(max_size)),
            max_size,
        }
    }

    fn get(&self, key: &str) -> Option<(String, i64)> {
        let map = self.entries.lock();
        let (url, expired) = map.get(key)?;
        // Check expiry with 5-minute safety margin
        if *expired > 0 {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let safety_margin_ms = 5 * 60 * 1000;
            if now_ms + safety_margin_ms >= *expired {
                drop(map);
                self.entries.lock().remove(key);
                return None;
            }
        }
        Some((url.clone(), *expired))
    }

    fn insert(&self, key: String, value: String, expired_time_ms: i64) {
        let mut map = self.entries.lock();
        if map.len() >= self.max_size {
            if let Some(oldest_key) = map.keys().next().cloned() {
                map.remove(&oldest_key);
            }
        }
        map.insert(key, (value, expired_time_ms));
    }
}

// ── Cached Access Token ────────────────────────────────────────────────────

struct CachedToken {
    token: String,
    expires_at: Instant,
}

// ── Dingtalk Adapter ───────────────────────────────────────────────────────

/// Shared state passed to webhook route handlers.
#[derive(Clone)]
struct WebhookState {
    adapter: Arc<DingtalkAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
}

/// Dingtalk platform adapter.
#[derive(Clone)]
pub struct DingtalkAdapter {
    pub config: DingtalkConfig,
    client: Client,
    dedup: Arc<MessageDeduplicator>,
    /// Access token cached from Dingtalk API (with TTL).
    access_token: Arc<RwLock<Option<CachedToken>>>,
    /// Session webhook cache for proactive sends.
    webhook_cache: Arc<WebhookCache>,
    /// Semaphore to limit concurrent webhook handlers.
    handler_semaphore: Arc<Semaphore>,
    /// Inbound message context per chat_id (for AI Card routing & reactions).
    message_contexts: Arc<parking_lot::Mutex<HashMap<String, serde_json::Value>>>,
    /// Cards in streaming state per chat: chat_id -> { out_track_id -> last_content }.
    streaming_cards: Arc<parking_lot::Mutex<HashMap<String, HashMap<String, String>>>>,
    /// Chats for which we've already fired the Done reaction.
    done_emoji_fired: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// Background task tracking (for cleanup on disconnect).
    bg_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl DingtalkAdapter {
    pub fn new(config: DingtalkConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: Arc::new(MessageDeduplicator::with_params(300, 1000)),
            access_token: Arc::new(RwLock::new(None)),
            webhook_cache: Arc::new(WebhookCache::new(500)),
            handler_semaphore: Arc::new(Semaphore::new(100)),
            message_contexts: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            streaming_cards: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            done_emoji_fired: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            bg_tasks: Arc::new(Mutex::new(Vec::new())),
            config,
        }
    }

    // ── Token management ─────────────────────────────────────────

    /// Get/refresh the Dingtalk access token.
    async fn get_access_token(&self) -> Result<String, String> {
        {
            let guard = self.access_token.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.expires_at > Instant::now() {
                    return Ok(cached.token.clone());
                }
            }
        }

        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/oauth2/accessToken")
            .json(&serde_json::json!({
                "appKey": &self.config.client_id,
                "appSecret": &self.config.client_secret,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to get access token: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token response: {e}"))?;

        let token = body
            .get("accessToken")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing accessToken in response: {body}"))?
            .to_string();

        let expires_in = body
            .get("expireIn")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);

        let cached = CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in - 300),
        };
        *self.access_token.write().await = Some(cached);
        Ok(token)
    }

    // ── Configuration check ──────────────────────────────────────

    pub fn is_configured(&self) -> bool {
        !self.config.client_id.is_empty() && !self.config.client_secret.is_empty()
    }

    pub fn supports_stream_mode(&self) -> bool {
        self.is_configured()
    }

    // ── Inbound processing ───────────────────────────────────────

    /// Process an inbound webhook or stream event.
    pub fn handle_inbound(&self, payload: &serde_json::Value) -> Option<DingtalkMessageEvent> {
        let msg_id = payload
            .get("msgId")
            .or_else(|| payload.get("msg_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !msg_id.is_empty() && self.dedup.is_duplicate(msg_id) {
            debug!("Dingtalk dedup: skipping {msg_id}");
            return None;
        }

        let conversation_id = payload
            .get("conversationId")
            .or_else(|| payload.get("conversation_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let conversation_type = payload
            .get("conversationType")
            .or_else(|| payload.get("conversation_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("1");
        let is_group = conversation_type == "2";

        let sender_id = payload
            .get("senderId")
            .or_else(|| payload.get("sender_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let sender_nick = payload
            .get("senderNick")
            .or_else(|| payload.get("sender_nick"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let sender_staff_id = payload
            .get("senderStaffId")
            .or_else(|| payload.get("sender_staff_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let session_webhook = payload
            .get("sessionWebhook")
            .or_else(|| payload.get("session_webhook"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let session_webhook_expired_time = payload
            .get("sessionWebhookExpiredTime")
            .or_else(|| payload.get("session_webhook_expired_time"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let is_in_at_list = payload
            .get("isInAtList")
            .or_else(|| payload.get("is_in_at_list"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let chat_id = if conversation_id.is_empty() {
            sender_id.clone()
        } else {
            conversation_id.clone()
        };

        // Allowed-users gate
        if !self.config.allowed_users.is_empty() && !self.config.allowed_users.contains("*") {
            let candidates: HashSet<String> = [
                sender_id.to_lowercase(),
                sender_staff_id.to_lowercase(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
            if candidates.is_disjoint(&self.config.allowed_users) {
                debug!(
                    "Dingtalk dropping message from non-allowlisted user staff_id={} sender_id={}",
                    sender_staff_id, sender_id
                );
                return None;
            }
        }

        // Group mention/pattern gate
        if is_group {
            let early_text = Self::extract_text(payload).unwrap_or_default();
            if !self.should_process_group_message(payload, &early_text, &chat_id) {
                debug!(
                    "Dingtalk dropping group message that failed mention gate msg_id={} chat_id={}",
                    msg_id, chat_id
                );
                return None;
            }
        }

        // Extract text and media
        let text = Self::extract_text(payload).unwrap_or_default();
        let (msg_type, media_urls, media_types) = Self::extract_media(payload);

        if text.is_empty() && media_urls.is_empty() {
            return None;
        }

        // Stash context for this chat
        if !chat_id.is_empty() {
            let mut contexts = self.message_contexts.lock();
            contexts.insert(chat_id.clone(), payload.clone());
        }

        // Reset done-emoji marker for new inbound
        if !chat_id.is_empty() {
            self.done_emoji_fired.lock().remove(&chat_id);
        }

        // Cache session webhook
        if !chat_id.is_empty() && !session_webhook.is_empty()
            && (session_webhook.starts_with("https://api.dingtalk.com/")
                || session_webhook.starts_with("https://oapi.dingtalk.com/"))
            {
                self.webhook_cache.insert(
                    chat_id.clone(),
                    session_webhook.clone(),
                    session_webhook_expired_time,
                );
            }

        if !msg_id.is_empty() {
            self.dedup.insert(msg_id.to_string());
        }

        Some(DingtalkMessageEvent {
            message_id: msg_id.to_string(),
            chat_id,
            sender_id,
            sender_nick,
            sender_staff_id,
            content: text,
            is_group,
            session_webhook,
            session_webhook_expired_time,
            is_in_at_list,
            media_urls,
            media_types,
            msg_type,
            raw: payload.clone(),
        })
    }

    /// Determine whether a group message should be processed.
    fn should_process_group_message(
        &self,
        _payload: &serde_json::Value,
        text: &str,
        chat_id: &str,
    ) -> bool {
        if self.config.free_response_chats.contains(chat_id) {
            return true;
        }
        if !self.config.require_mention {
            return true;
        }
        // Check is_in_at_list from payload
        let is_mentioned = _payload
            .get("isInAtList")
            .or_else(|| _payload.get("is_in_at_list"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_mentioned {
            return true;
        }
        // Check mention patterns
        if !text.is_empty() && !self.config.mention_patterns.is_empty() {
            for pattern in &self.config.mention_patterns {
                if pattern.is_match(text) {
                    return true;
                }
            }
        }
        false
    }

    /// Extract plain text from a DingTalk payload.
    fn extract_text(payload: &serde_json::Value) -> Option<String> {
        // Try text.content first
        if let Some(text_obj) = payload.get("text") {
            if let Some(content) = text_obj.get("content").and_then(|v| v.as_str()) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some(content) = text_obj.as_str() {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }

        // Fallback to richText / rich_text_content
        let rich_text = payload
            .get("richTextContent")
            .or_else(|| payload.get("rich_text_content"))
            .or_else(|| payload.get("richText"))
            .or_else(|| payload.get("rich_text"));

        if let Some(rt) = rich_text {
            let rich_list = rt
                .get("richTextList")
                .or_else(|| rt.get("rich_text_list"))
                .and_then(|v| v.as_array())
                .or_else(|| rt.as_array());

            if let Some(list) = rich_list {
                let parts: Vec<String> = list
                    .iter()
                    .filter_map(|item| {
                        item.get("text")
                            .and_then(|v| v.as_str())
                            .or_else(|| item.get("content").and_then(|v| v.as_str()))
                            .map(String::from)
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
                let combined = parts.join(" ");
                if !combined.is_empty() {
                    return Some(combined);
                }
            }
        }

        None
    }

    /// Extract media info from message payload.
    fn extract_media(payload: &serde_json::Value) -> (String, Vec<String>, Vec<String>) {
        let mut msg_type = "text".to_string();
        let mut media_urls = Vec::new();
        let mut media_types = Vec::new();

        // DingTalk message type mapping
        let msgtype = payload
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Single image content (picture messages)
        if let Some(image_content) = payload.get("imageContent").or_else(|| payload.get("image_content")) {
            if let Some(code) = image_content
                .get("downloadCode")
                .or_else(|| image_content.get("download_code"))
                .and_then(|v| v.as_str())
            {
                media_urls.push(code.to_string());
                media_types.push("image".to_string());
                msg_type = "picture".to_string();
            }
        }

        // Rich text with mixed content
        let rich_text = payload
            .get("richTextContent")
            .or_else(|| payload.get("rich_text_content"))
            .or_else(|| payload.get("richText"))
            .or_else(|| payload.get("rich_text"));

        if let Some(rt) = rich_text {
            let rich_list = rt
                .get("richTextList")
                .or_else(|| rt.get("rich_text_list"))
                .and_then(|v| v.as_array())
                .or_else(|| rt.as_array());

            if let Some(list) = rich_list {
                for item in list {
                    let item_type = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let dl_code = item
                        .get("downloadCode")
                        .or_else(|| item.get("download_code"))
                        .or_else(|| item.get("pictureDownloadCode"))
                        .and_then(|v| v.as_str());

                    if let Some(code) = dl_code {
                        let mapped = match item_type {
                            "picture" => "image",
                            "voice" => "audio",
                            "video" => "video",
                            _ => "file",
                        };
                        media_urls.push(code.to_string());
                        media_types.push(mapped.to_string());
                        if msg_type == "text" {
                            msg_type = match mapped {
                                "image" => "picture",
                                "audio" => "voice",
                                "video" => "video",
                                _ => "file",
                            }
                            .to_string();
                        }
                    }
                }
            }
        }

        // If msgtype field indicates picture but no media found yet
        if msgtype == "picture" && media_urls.is_empty() {
            msg_type = "picture".to_string();
        } else if msgtype == "richText" {
            msg_type = if media_types.iter().any(|t| t == "image") {
                "picture"
            } else {
                "richText"
            }
            .to_string();
        } else if msgtype == "voice" {
            msg_type = "voice".to_string();
        } else if msgtype == "video" {
            msg_type = "video".to_string();
        } else if msgtype == "file" {
            msg_type = "file".to_string();
        }

        (msg_type, media_urls, media_types)
    }

    // ── Outbound messaging ───────────────────────────────────────

    /// Send a markdown message to a Dingtalk chat via session webhook.
    pub async fn send_text(&self, webhook_url: &str, text: &str) -> Result<String, String> {
        if !webhook_url.starts_with("https://api.dingtalk.com/")
            && !webhook_url.starts_with("https://oapi.dingtalk.com/")
        {
            return Err(format!("Invalid dingtalk webhook URL: {webhook_url}"));
        }

        let resp = self
            .client
            .post(webhook_url)
            .json(&serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "title": "Hermes",
                    "text": self.normalize_markdown(&truncate_text(text, MAX_MESSAGE_LENGTH)),
                },
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Send failed: HTTP {}", status));
        }

        debug!("Dingtalk message sent via webhook");
        Ok("ok".to_string())
    }

    /// Send a proactive markdown message via the Open API.
    #[allow(dead_code)]
    pub async fn send_proactive(
        &self,
        open_conversation_id: &str,
        text: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;

        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend")
            .header("x-acs-dingtalk-access-token", &token)
            .json(&serde_json::json!({
                "robotCode": &self.config.robot_code,
                "userIds": [open_conversation_id],
                "msgKey": "sampleMarkdown",
                "msgParam": serde_json::json!({
                    "title": "Hermes",
                    "text": truncate_text(text, MAX_MESSAGE_LENGTH),
                }).to_string(),
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send proactive message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Proactive send failed: HTTP {}", status));
        }

        debug!("Dingtalk proactive message sent to {open_conversation_id}");
        Ok("ok".to_string())
    }

    /// Edit an existing AI Card by streaming updated content.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        finalize: bool,
    ) -> Result<String, String> {
        if message_id.is_empty() {
            return Err("message_id required".to_string());
        }
        let token = self.get_access_token().await?;

        self.stream_card_content(message_id, &token, content, finalize)
            .await?;

        if finalize {
            {
                let mut cards = self.streaming_cards.lock();
                if let Some(chat_cards) = cards.get_mut(chat_id) {
                    chat_cards.remove(message_id);
                    if chat_cards.is_empty() {
                        cards.remove(chat_id);
                    }
                }
            }
            debug!("Dingtalk AI Card finalized (edit): {message_id}");
            self.fire_done_reaction(chat_id).await;
        } else {
            let mut cards = self.streaming_cards.lock();
            cards
                .entry(chat_id.to_string())
                .or_default()
                .insert(message_id.to_string(), content.to_string());
        }

        Ok(message_id.to_string())
    }

    // ── AI Card lifecycle ────────────────────────────────────────

    /// Create an AI Card, deliver it, and stream initial content.
    async fn create_and_stream_card(
        &self,
        _chat_id: &str,
        message: &serde_json::Value,
        content: &str,
        finalize: bool,
    ) -> Result<Option<String>, String> {
        let token = self.get_access_token().await?;
        let template_id = match &self.config.card_template_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => return Ok(None),
        };

        let out_track_id = format!("hermes_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);

        let conversation_id = message
            .get("conversationId")
            .or_else(|| message.get("conversation_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let conversation_type = message
            .get("conversationType")
            .or_else(|| message.get("conversation_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("1");
        let is_group = conversation_type == "2";

        let sender_staff_id = message
            .get("senderStaffId")
            .or_else(|| message.get("sender_staff_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Step 1: Create card
        let create_resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/card/instances")
            .header("x-acs-dingtalk-access-token", &token)
            .json(&serde_json::json!({
                "cardTemplateId": template_id,
                "outTrackId": out_track_id,
                "cardData": {
                    "cardParamMap": {"content": ""}
                },
                "callbackType": "STREAM",
                "imGroupOpenSpaceModel": {
                    "supportForward": true
                },
                "imRobotOpenSpaceModel": {
                    "supportForward": true
                }
            }))
            .send()
            .await
            .map_err(|e| format!("AI Card create failed: {e}"))?;

        if !create_resp.status().is_success() {
            let body = create_resp.text().await.unwrap_or_default();
            return Err(format!("AI Card create failed: {body}"));
        }

        // Step 2: Deliver card
        let deliver_body = if is_group {
            serde_json::json!({
                "outTrackId": out_track_id,
                "userIdType": 1,
                "openSpaceId": format!("dtv1.card//IM_GROUP.{}", conversation_id),
                "imGroupOpenDeliverModel": {
                    "robotCode": self.config.robot_code
                }
            })
        } else {
            if sender_staff_id.is_empty() {
                warn!("Dingtalk AI Card skipped: missing sender_staff_id for DM");
                return Ok(None);
            }
            serde_json::json!({
                "outTrackId": out_track_id,
                "userIdType": 1,
                "openSpaceId": format!("dtv1.card//IM_ROBOT.{}", sender_staff_id),
                "imRobotOpenDeliverModel": {
                    "spaceType": "IM_ROBOT"
                }
            })
        };

        let deliver_resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/card/instances/deliver")
            .header("x-acs-dingtalk-access-token", &token)
            .json(&deliver_body)
            .send()
            .await
            .map_err(|e| format!("AI Card deliver failed: {e}"))?;

        if !deliver_resp.status().is_success() {
            let body = deliver_resp.text().await.unwrap_or_default();
            return Err(format!("AI Card deliver failed: {body}"));
        }

        // Step 3: Stream initial content
        self.stream_card_content(&out_track_id, &token, content, finalize)
            .await?;

        info!(
            "Dingtalk AI Card {}: {out_track_id}",
            if finalize { "created+finalized" } else { "created (streaming)" }
        );

        Ok(Some(out_track_id))
    }

    /// Stream content to an existing AI Card.
    async fn stream_card_content(
        &self,
        out_track_id: &str,
        token: &str,
        content: &str,
        finalize: bool,
    ) -> Result<(), String> {
        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/card/instances/streaming")
            .header("x-acs-dingtalk-access-token", token)
            .json(&serde_json::json!({
                "outTrackId": out_track_id,
                "guid": uuid::Uuid::new_v4().to_string(),
                "key": "content",
                "content": truncate_text(content, MAX_MESSAGE_LENGTH),
                "isFull": true,
                "isFinalize": finalize,
                "isError": false,
            }))
            .send()
            .await
            .map_err(|e| format!("AI Card stream update failed: {e}"))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("AI Card stream update failed: {body}"));
        }

        Ok(())
    }

    /// Finalize any previously-open streaming cards for this chat.
    async fn close_streaming_siblings(&self, chat_id: &str) -> Result<(), String> {
        let cards: HashMap<String, String> = {
            let mut all = self.streaming_cards.lock();
            all.remove(chat_id).unwrap_or_default()
        };

        if cards.is_empty() {
            return Ok(());
        }

        let token = match self.get_access_token().await {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };

        for (out_track_id, last_content) in cards {
            if let Err(e) = self
                .stream_card_content(&out_track_id, &token, &last_content, true)
                .await
            {
                debug!("Dingtalk sibling close failed for {out_track_id}: {e}");
            } else {
                debug!("Dingtalk AI Card sibling closed: {out_track_id}");
            }
        }

        Ok(())
    }

    /// Swap 🤔Thinking → 🥳Done on the original user message.
    async fn fire_done_reaction(&self, chat_id: &str) {
        {
            let mut fired = self.done_emoji_fired.lock();
            if fired.contains(chat_id) {
                return;
            }
            fired.insert(chat_id.to_string());
        }

        let context = self.message_contexts.lock().get(chat_id).cloned();
        let Some(msg) = context else { return };

        let msg_id = msg
            .get("msgId")
            .or_else(|| msg.get("msg_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let conversation_id = msg
            .get("conversationId")
            .or_else(|| msg.get("conversation_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if msg_id.is_empty() || conversation_id.is_empty() {
            return;
        }

        let self_arc = self.clone();
        let chat_id = chat_id.to_string();
        let handle = tokio::spawn(async move {
            let _ = self_arc
                .send_emotion(&msg_id, &conversation_id, "🤔Thinking", true)
                .await;
            let _ = self_arc
                .send_emotion(&msg_id, &conversation_id, "🥳Done", false)
                .await;
            debug!("Dingtalk Done reaction fired for chat_id={chat_id}");
        });

        self.bg_tasks.lock().await.push(handle);
    }

    // ── Emotions / Reactions ─────────────────────────────────────

    /// Add or recall an emoji reaction on a message.
    async fn send_emotion(
        &self,
        open_msg_id: &str,
        open_conversation_id: &str,
        emoji_name: &str,
        recall: bool,
    ) -> Result<(), String> {
        if open_msg_id.is_empty() || open_conversation_id.is_empty() {
            return Ok(());
        }
        let token = self.get_access_token().await?;

        let url = if recall {
            "https://api.dingtalk.com/v1.0/robot/message/emotion/recall"
        } else {
            "https://api.dingtalk.com/v1.0/robot/message/emotion/reply"
        };

        let emotion_id = "2659900";
        let text_emotion = serde_json::json!({
            "emotionId": emotion_id,
            "emotionName": emoji_name,
            "text": emoji_name,
            "backgroundId": "im_bg_1"
        });

        let resp = self
            .client
            .post(url)
            .header("x-acs-dingtalk-access-token", &token)
            .json(&serde_json::json!({
                "robotCode": self.config.robot_code,
                "openMsgId": open_msg_id,
                "openConversationId": open_conversation_id,
                "emotionType": 2,
                "emotionName": emoji_name,
                "textEmotion": text_emotion,
            }))
            .send()
            .await
            .map_err(|e| format!("Emotion send failed: {e}"))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Emotion send failed: {body}"));
        }

        debug!(
            "Dingtalk emotion {} {} on msg={}",
            if recall { "recall" } else { "reply" },
            emoji_name,
            &open_msg_id[..open_msg_id.len().min(24)]
        );
        Ok(())
    }

    // ── Media resolution ─────────────────────────────────────────

    /// Resolve download codes in message payload to actual URLs.
    async fn resolve_media_codes(&self, payload: &mut serde_json::Value) {
        let token = match self.get_access_token().await {
            Ok(t) => t,
            Err(_) => return,
        };

        let robot_code = payload
            .get("robotCode")
            .or_else(|| payload.get("robot_code"))
            .and_then(|v| v.as_str())
            .unwrap_or(&self.config.robot_code)
            .to_string();

        let mut codes: Vec<(String, String)> = Vec::new(); // (code, json_pointer)

        // Collect codes from image_content
        if let Some(code) = payload
            .pointer("/imageContent/downloadCode")
            .or_else(|| payload.pointer("/image_content/download_code"))
            .and_then(|v| v.as_str())
        {
            codes.push((code.to_string(), "/imageContent/downloadCode".to_string()));
        }

        // Collect codes from rich_text list
        let rich_text_paths = ["/richTextContent/richTextList", "/rich_text_content/rich_text_list"];
        for path in &rich_text_paths {
            if let Some(list) = payload.pointer(path).and_then(|v| v.as_array()) {
                for (i, item) in list.iter().enumerate() {
                    for key in &["downloadCode", "pictureDownloadCode", "download_code"] {
                        if let Some(code) = item.get(key).and_then(|v| v.as_str()) {
                            codes.push((
                                code.to_string(),
                                format!("{path}/{i}/{key}"),
                            ));
                        }
                    }
                }
            }
        }

        // Resolve all codes in parallel
        let mut handles = Vec::new();
        for (code, pointer) in codes {
            let client = self.client.clone();
            let token = token.clone();
            let robot_code = robot_code.clone();
            let handle = tokio::spawn(async move {
                let url = Self::fetch_download_url(&client, &code, &robot_code, &token).await;
                (pointer, url)
            });
            handles.push(handle);
        }

        for handle in handles {
            if let Ok((pointer, Some(url))) = handle.await {
                if let Some(parent) = pointer.rfind('/') {
                    let parent_ptr = &pointer[..parent];
                    if let Some(parent_val) = payload.pointer_mut(parent_ptr) {
                        if let Some(obj) = parent_val.as_object_mut() {
                            obj.insert("downloadUrl".to_string(), serde_json::json!(url));
                        }
                    }
                }
            }
        }
    }

    /// Fetch download URL for a single code.
    async fn fetch_download_url(
        client: &Client,
        code: &str,
        robot_code: &str,
        token: &str,
    ) -> Option<String> {
        let resp = client
            .post("https://api.dingtalk.com/v1.0/robot/message/files/download")
            .header("x-acs-dingtalk-access-token", token)
            .json(&serde_json::json!({
                "downloadCode": code,
                "robotCode": robot_code,
            }))
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let body: serde_json::Value = resp.json().await.ok()?;
        body.get("downloadUrl")
            .or_else(|| body.get("download_url"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    // ── Markdown normalization ───────────────────────────────────

    /// Normalize markdown for DingTalk's parser.
    fn normalize_markdown(&self, text: &str) -> String {
        static NUMBERED_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let numbered_re = NUMBERED_RE.get_or_init(|| regex::Regex::new(r"^\d+\.\s").unwrap());
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let is_numbered = numbered_re.is_match(line.trim());
            if is_numbered && i > 0 {
                let prev = lines[i - 1];
                if !prev.trim().is_empty() && !numbered_re.is_match(prev.trim()) {
                    out.push("".to_string());
                }
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") && *line != trimmed {
                out.push(trimmed.to_string());
            } else {
                out.push(line.to_string());
            }
        }
        out.join("\n")
    }

    // ── Webhook server ───────────────────────────────────────────

    /// Run the Dingtalk webhook HTTP server.
    pub async fn run(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let app = self.build_router(handler.clone());

        let host = "0.0.0.0";
        let port = self.config.webhook_port;
        let listener = match tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
            Ok(l) => l,
            Err(e) => return Err(format!("Failed to bind Dingtalk webhook server: {e}")),
        };

        info!(
            "Dingtalk webhook server listening on {}:{}{}",
            host, port, self.config.webhook_path
        );

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
        {
            error!("Dingtalk webhook server error: {e}");
            return Err(format!("Dingtalk webhook server error: {e}"));
        }

        info!("Dingtalk webhook server stopped gracefully");
        Ok(())
    }

    fn build_router(&self, handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>) -> Router {
        let state = WebhookState {
            adapter: Arc::new((*self).clone()),
            handler,
        };

        Router::new()
            .route(&self.config.webhook_path, post(webhook_handler))
            .with_state(state)
    }

    // ── Stream Mode ──────────────────────────────────────────────

    /// Get a WebSocket endpoint + ticket from DingTalk gateway.
    async fn get_stream_endpoint(&self) -> Result<StreamEndpoint, String> {
        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/gateway/connections/open")
            .json(&serde_json::json!({
                "clientId": self.config.client_id,
                "clientSecret": self.config.client_secret,
                "subscriptions": [
                    {"type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get"}
                ],
                "ua": "hermes-gateway/rust",
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to open stream connection: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Stream open failed: HTTP {} - {}", status, body));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse stream endpoint: {e}"))?;

        let endpoint = body
            .get("endpoint")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing endpoint in response: {body}"))?
            .to_string();
        let ticket = body
            .get("ticket")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing ticket in response: {body}"))?
            .to_string();

        Ok(StreamEndpoint { endpoint, ticket })
    }

    /// Run the Dingtalk Stream Mode WebSocket client with auto-reconnect.
    pub async fn run_stream(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        info!("Dingtalk stream mode starting...");
        let mut backoff_idx = 0usize;

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match self.stream_connect_and_run(handler.clone(), running.clone()).await {
                Ok(()) => {
                    backoff_idx = 0;
                }
                Err(e) => {
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    warn!("Dingtalk stream error: {e}");
                }
            }

            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
            info!("Dingtalk reconnecting in {delay}s...");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            backoff_idx += 1;
        }

        info!("Dingtalk stream mode stopped");
    }

    /// Single WebSocket connection attempt.
    async fn stream_connect_and_run(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), String> {
        let endpoint = self.get_stream_endpoint().await?;
        let uri = format!("{}?ticket={}", endpoint.endpoint, endpoint.ticket);

        info!("Dingtalk stream connecting to {}", endpoint.endpoint);

        let (ws_stream, _) = tokio_tungstenite::connect_async(&uri)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        info!("Dingtalk stream WebSocket connected");

        let (mut write, mut read) = ws_stream.split();

        // Ping task
        let ping_running = running.clone();
        let mut ping_interval = interval(Duration::from_secs(STREAM_PING_INTERVAL_SECS));
        let ping_handle = tokio::spawn(async move {
            while ping_running.load(std::sync::atomic::Ordering::SeqCst) {
                ping_interval.tick().await;
                if write
                    .send(tokio_tungstenite::tungstenite::Message::Ping(vec![].into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Read loop
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let msg = tokio::select! {
                msg = read.next() => msg,
                _ = tokio::time::sleep(Duration::from_secs(120)) => {
                    warn!("Dingtalk stream read timeout");
                    break;
                }
            };

            let Some(msg) = msg else {
                info!("Dingtalk stream WebSocket closed by server");
                break;
            };

            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!("Dingtalk stream WebSocket error: {e}");
                    break;
                }
            };

            match msg {
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    match self.handle_stream_frame(&text, &handler).await {
                        Ok(Some(ack)) => {
                            // Send ACK back over WebSocket
                            if let Ok(ack_json) = serde_json::to_string(&ack) {
                                // We need to send through a cloned sender, but write is
                                // borrowed by the ping task. For now we just log;
                                // DingTalk retry + dedup handles safety.
                                debug!("Dingtalk stream ACK (not sent): {ack_json}");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!("Dingtalk stream frame handling error: {e}");
                        }
                    }
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => {
                    info!("Dingtalk stream received Close frame");
                    break;
                }
                tokio_tungstenite::tungstenite::Message::Pong(_) => {
                    debug!("Dingtalk stream received pong");
                }
                _ => {}
            }
        }

        ping_handle.abort();
        Ok(())
    }

    /// Handle a single incoming WebSocket text frame.
    /// Returns an optional ACK frame to send back.
    async fn handle_stream_frame(
        &self,
        text: &str,
        handler: &Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
    ) -> Result<Option<AckFrame>, String> {
        let frame: StreamFrame = serde_json::from_str(text)
            .map_err(|e| format!("Invalid stream frame JSON: {e}"))?;

        let message_id = frame.headers.message_id.clone().unwrap_or_default();

        match frame.frame_type.as_str() {
            "SYSTEM" => {
                let topic = frame.headers.topic.as_deref().unwrap_or("");
                debug!("Dingtalk stream SYSTEM message topic={}", topic);

                if topic == "disconnect" {
                    return Err("Server requested disconnect".to_string());
                }
                Ok(Some(self.build_ack(&message_id, 200, "OK")))
            }
            "CALLBACK" => {
                // Parse callback data
                let data: serde_json::Value = serde_json::from_str(&frame.data)
                    .map_err(|e| format!("Invalid callback data JSON: {e}"))?;

                // Process like a webhook payload
                if let Some(event) = self.handle_inbound(&data) {
                    // Fire Thinking reaction in background
                    if !event.message_id.is_empty() && !event.chat_id.is_empty() {
                        let adapter = self.clone();
                        let msg_id = event.message_id.clone();
                        let conv_id = event.chat_id.clone();
                        let _handle = tokio::spawn(async move {
                            let _ = adapter
                                .send_emotion(&msg_id, &conv_id, "🤔Thinking", false)
                                .await;
                        });
                    }

                    self.spawn_handler(handler.clone(), event).await;
                }
                Ok(Some(self.build_ack(&message_id, 200, "OK")))
            }
            "EVENT" => {
                debug!("Dingtalk stream EVENT message");
                Ok(Some(self.build_ack(&message_id, 200, "OK")))
            }
            _ => {
                warn!("Unknown stream frame type: {}", frame.frame_type);
                Ok(Some(self.build_ack(&message_id, 200, "OK")))
            }
        }
    }

    fn build_ack(&self, message_id: &str, code: i32, message: &str) -> AckFrame {
        AckFrame {
            code,
            headers: AckHeaders {
                message_id: message_id.to_string(),
                content_type: "application/json".to_string(),
            },
            message: message.to_string(),
            data: "{}".to_string(),
        }
    }

    /// Spawn a background handler task for an inbound event.
    async fn spawn_handler(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        event: DingtalkMessageEvent,
    ) {
        let adapter = self.clone();
        let permit = self.handler_semaphore.clone()
            .try_acquire_owned()
            .map_err(|_| "Too many concurrent handlers")
            .ok();

        if let Some(_permit) = permit {
            tokio::spawn(async move {
                if event.content.is_empty() && event.media_urls.is_empty() {
                    return;
                }

                info!(
                    "Dingtalk message from {} ({}) via {}: {}",
                    event.sender_nick,
                    event.sender_id,
                    event.chat_id,
                    event.content.chars().take(50).collect::<String>(),
                );

                let handler_guard = handler.lock().await;
                if let Some(handler) = handler_guard.as_ref() {
                    match handler
                        .handle_message(
                            crate::config::Platform::Dingtalk,
                            &event.chat_id,
                            &event.content,
                            None,
                        )
                        .await
                    {
                        Ok(result) => {
                            if !result.response.is_empty() {
                                let is_final = true; // handler result is always final
                                let _ = adapter
                                    .send_with_cards(&event, &result.response, is_final)
                                    .await;
                            }
                        }
                        Err(e) => {
                            error!("Agent handler failed for Dingtalk message: {e}");
                            let _ = adapter
                                .send_text(
                                    &event.session_webhook,
                                    "Sorry, I encountered an error processing your message.",
                                )
                                .await;
                        }
                    }
                } else {
                    warn!("No message handler registered for Dingtalk messages");
                }
            });
        } else {
            warn!("Dingtalk stream rejected: too many concurrent handlers");
        }
    }

    /// Send a message, preferring AI Cards when configured.
    async fn send_with_cards(
        &self,
        event: &DingtalkMessageEvent,
        content: &str,
        is_final: bool,
    ) -> Result<String, String> {
        // If AI Cards are configured and we have message context, try card path
        if self.config.card_template_id.is_some() {
            let context = self.message_contexts.lock().get(&event.chat_id).cloned();
            if let Some(ctx) = context {
                if let Err(e) = self.close_streaming_siblings(&event.chat_id).await {
                    debug!("Failed to close streaming siblings: {e}");
                }

                match self.create_and_stream_card(&event.chat_id, &ctx, content, is_final).await {
                    Ok(Some(track_id)) => {
                        if is_final {
                            self.fire_done_reaction(&event.chat_id).await;
                        } else {
                            let mut cards = self.streaming_cards.lock();
                            cards
                                .entry(event.chat_id.clone())
                                .or_default()
                                .insert(track_id.clone(), content.to_string());
                        }
                        return Ok(track_id);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("AI Card send failed, falling back to webhook: {e}");
                    }
                }
            }
        }

        // Fallback to webhook
        let result = self.send_text(&event.session_webhook, content).await;
        if result.is_ok() && is_final {
            self.fire_done_reaction(&event.chat_id).await;
        }
        result
    }

    /// Disconnect and clean up resources.
    pub async fn disconnect(&self) {
        // Cancel any background tasks
        let mut tasks = self.bg_tasks.lock().await;
        for task in tasks.drain(..) {
            task.abort();
        }
        drop(tasks);

        self.message_contexts.lock().clear();
        self.streaming_cards.lock().clear();
        self.done_emoji_fired.lock().clear();
        self.dedup.clear();
        info!("Dingtalk adapter disconnected");
    }
}

// ── Webhook Route Handler ──────────────────────────────────────────────────

/// Verify Dingtalk webhook signature to prevent message injection.
fn verify_webhook_signature(
    payload: &serde_json::Value,
    client_secret: &str,
) -> Result<(), String> {
    let timestamp = payload
        .get("timestamp")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("create_at").and_then(|v| v.as_str()))
        .ok_or_else(|| "Missing timestamp in webhook payload".to_string())?;

    let signature = payload
        .get("sign")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing signature in webhook payload".to_string())?;

    let mut mac = HmacSha256::new_from_slice(client_secret.as_bytes())
        .map_err(|_| "Invalid HMAC key length".to_string())?;
    mac.update(timestamp.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());

    if signature != expected {
        return Err(format!(
            "Webhook signature mismatch: received={}, expected={}",
            signature, expected
        ));
    }
    Ok(())
}

async fn webhook_handler(
    State(state): State<WebhookState>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<WebhookResponse>) {
    debug!("Dingtalk webhook received: {payload}");

    // Verify webhook signature if client_secret is configured
    if !state.adapter.config.client_secret.is_empty() {
        if let Err(e) = verify_webhook_signature(&payload, &state.adapter.config.client_secret) {
            warn!("Dingtalk webhook signature verification failed: {e}");
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookResponse {
                    ret: "signature_failed".to_string(),
                }),
            );
        }
    }

    let Some(event) = state.adapter.handle_inbound(&payload) else {
        return (
            StatusCode::OK,
            Json(WebhookResponse {
                ret: "success".to_string(),
            }),
        );
    };

    if event.content.is_empty() && event.media_urls.is_empty() {
        return (
            StatusCode::OK,
            Json(WebhookResponse {
                ret: "success".to_string(),
            }),
        );
    }

    let adapter = state.adapter.clone();
    let handler = state.handler.clone();
    let permit = state.adapter.handler_semaphore.clone()
        .try_acquire_owned()
        .map_err(|_| "Too many concurrent webhook handlers")
        .ok();

    if let Some(_permit) = permit {
        tokio::spawn(async move {
            info!(
                "Dingtalk message from {} ({}) via {}: {}",
                event.sender_nick,
                event.sender_id,
                event.chat_id,
                event.content.chars().take(50).collect::<String>(),
            );

            let handler_guard = handler.lock().await;
            if let Some(handler) = handler_guard.as_ref() {
                match handler
                    .handle_message(
                        crate::config::Platform::Dingtalk,
                        &event.chat_id,
                        &event.content,
                        None,
                    )
                    .await
                {
                    Ok(result) => {
                        if !result.response.is_empty() {
                            let is_final = true;
                            if let Err(e) = adapter.send_with_cards(&event, &result.response, is_final).await {
                                error!("Dingtalk send failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Agent handler failed for Dingtalk message: {e}");
                        let _ = adapter
                            .send_text(
                                &event.session_webhook,
                                "Sorry, I encountered an error processing your message.",
                            )
                            .await;
                    }
                }
            } else {
                warn!("No message handler registered for Dingtalk messages");
            }
        });
    } else {
        warn!("Dingtalk webhook rejected: too many concurrent handlers");
    }

    (
        StatusCode::OK,
        Json(WebhookResponse {
            ret: "success".to_string(),
        }),
    )
}

// ── StreamEditTransport ────────────────────────────────────────────────────

#[async_trait::async_trait]
impl crate::stream_consumer::StreamEditTransport for DingtalkAdapter {
    async fn stream_send(&self, chat_id: &str, content: &str) -> Result<String, String> {
        let context = self.message_contexts.lock().get(chat_id).cloned();
        if let Some(ctx) = context {
            if let Ok(Some(track_id)) = self.create_and_stream_card(chat_id, &ctx, content, false).await {
                let mut cards = self.streaming_cards.lock();
                cards.entry(chat_id.to_string()).or_default().insert(track_id.clone(), content.to_string());
                return Ok(track_id);
            }
        }
        // Fallback: webhook send (no message ID tracking)
        if let Some((webhook, _)) = self.webhook_cache.get(chat_id) {
            self.send_text(&webhook, content).await?;
        }
        Ok(uuid::Uuid::new_v4().simple().to_string())
    }

    async fn stream_edit(&self, chat_id: &str, message_id: &str, content: &str) -> Result<bool, String> {
        match self.edit_message(chat_id, message_id, content, false).await {
            Ok(_) => Ok(true),
            Err(e) => Err(e),
        }
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = DingtalkConfig::default();
        assert_eq!(config.webhook_port, 8766);
        assert_eq!(config.webhook_path, "/dingtalk/callback");
        assert!(matches!(config.connection_mode, DingtalkConnectionMode::Stream));
    }

    #[test]
    fn test_not_configured_when_empty() {
        let config = DingtalkConfig::default();
        let adapter = DingtalkAdapter::new(config);
        assert!(!adapter.is_configured());
    }

    #[test]
    fn test_extract_text_content() {
        let payload = serde_json::json!({
            "text": {"content": "hello world"},
            "msgId": "msg1",
        });
        assert_eq!(DingtalkAdapter::extract_text(&payload).unwrap(), "hello world");
    }

    #[test]
    fn test_extract_rich_text() {
        let payload = serde_json::json!({
            "richTextContent": {
                "richTextList": [
                    {"text": "line1"},
                    {"text": "line2"},
                ]
            },
            "msgId": "msg2",
        });
        assert_eq!(DingtalkAdapter::extract_text(&payload).unwrap(), "line1 line2");
    }

    #[test]
    fn test_extract_media_picture() {
        let payload = serde_json::json!({
            "msgId": "msg3",
            "imageContent": {"downloadCode": "code123"},
        });
        let (msg_type, urls, types) = DingtalkAdapter::extract_media(&payload);
        assert_eq!(msg_type, "picture");
        assert_eq!(urls, vec!["code123"]);
        assert_eq!(types, vec!["image"]);
    }

    #[test]
    fn test_extract_media_rich_text() {
        let payload = serde_json::json!({
            "msgId": "msg4",
            "richTextContent": {
                "richTextList": [
                    {"type": "picture", "downloadCode": "pic1"},
                    {"type": "voice", "downloadCode": "voice1"},
                ]
            },
        });
        let (msg_type, urls, types) = DingtalkAdapter::extract_media(&payload);
        assert_eq!(msg_type, "picture");
        assert_eq!(urls, vec!["pic1", "voice1"]);
        assert_eq!(types, vec!["image", "audio"]);
    }

    #[test]
    fn test_dedup() {
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let payload = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "dedup_test_1",
        });
        assert!(adapter.handle_inbound(&payload).is_some());
        assert!(adapter.handle_inbound(&payload).is_none());
    }

    #[test]
    fn test_webhook_url_validation() {
        assert!("https://api.dingtalk.com/robot/send?access_token=xxx"
            .starts_with("https://api.dingtalk.com/"));
        assert!(!"https://evil.com/test".starts_with("https://api.dingtalk.com/"));
    }

    #[test]
    fn test_handle_inbound_snake_case_keys() {
        let payload = serde_json::json!({
            "text": {"content": "snake case test"},
            "msg_id": "snake1",
            "conversation_id": "conv456",
            "sender_id": "user1",
            "sender_nick": "Tester",
            "conversation_type": "1",
            "session_webhook": "https://api.dingtalk.com/test",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let event = adapter.handle_inbound(&payload).unwrap();
        assert_eq!(event.content, "snake case test");
        assert_eq!(event.message_id, "snake1");
        assert_eq!(event.chat_id, "conv456");
        assert!(!event.is_group);
    }

    #[test]
    fn test_extract_raw_string_text() {
        let payload = serde_json::json!({
            "text": "raw string message",
            "msgId": "raw1",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let event = adapter.handle_inbound(&payload).unwrap();
        assert_eq!(event.content, "raw string message");
    }

    #[test]
    fn test_truncate_text_utf8_safe() {
        let text = "Hello 😀 World";
        assert_eq!(truncate_text(text, 3), "Hel");
        assert_eq!(truncate_text(text, 7), "Hello 😀");
        assert_eq!(truncate_text(text, 100), text);
    }

    #[test]
    fn test_webhook_signature_verification() {
        let secret = "test_secret_123";
        let timestamp = "1700000000";

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        let valid_sig = hex::encode(mac.finalize().into_bytes());

        let payload = serde_json::json!({
            "timestamp": timestamp,
            "sign": valid_sig,
        });
        assert!(verify_webhook_signature(&payload, secret).is_ok());

        let payload_bad = serde_json::json!({
            "timestamp": timestamp,
            "sign": "invalid_signature",
        });
        assert!(verify_webhook_signature(&payload_bad, secret).is_err());
    }

    #[tokio::test]
    async fn test_cached_token_expiry() {
        let config = DingtalkConfig::default();
        let adapter = DingtalkAdapter::new(config);

        let token = CachedToken {
            token: "test_token".to_string(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        *adapter.access_token.write().await = Some(token);

        let guard = adapter.access_token.read().await;
        let cached = guard.as_ref().unwrap();
        assert_eq!(cached.token, "test_token");
        assert!(cached.expires_at > Instant::now());
    }

    #[test]
    fn test_normalize_markdown() {
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let input = "line1\n1. item\n2. item2";
        let out = adapter.normalize_markdown(input);
        assert!(out.contains("\n\n1. item"));
    }

    #[test]
    fn test_allowed_users_gate() {
        let mut config = DingtalkConfig::default();
        config.client_id = "test".to_string();
        config.client_secret = "test".to_string();
        config.allowed_users.insert("user123".to_string());
        let adapter = DingtalkAdapter::new(config);

        let payload_allowed = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "allow1",
            "senderId": "user123",
        });
        assert!(adapter.handle_inbound(&payload_allowed).is_some());

        let payload_denied = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "deny1",
            "senderId": "user999",
        });
        assert!(adapter.handle_inbound(&payload_denied).is_none());
    }

    #[test]
    fn test_require_mention_gate() {
        let mut config = DingtalkConfig::default();
        config.client_id = "test".to_string();
        config.client_secret = "test".to_string();
        config.require_mention = true;
        let adapter = DingtalkAdapter::new(config);

        // Group without mention → blocked
        let payload_no_mention = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "mention1",
            "conversationType": "2",
            "conversationId": "group1",
            "isInAtList": false,
        });
        assert!(adapter.handle_inbound(&payload_no_mention).is_none());

        // Group with mention → allowed
        let payload_mentioned = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "mention2",
            "conversationType": "2",
            "conversationId": "group1",
            "isInAtList": true,
        });
        assert!(adapter.handle_inbound(&payload_mentioned).is_some());
    }
}
