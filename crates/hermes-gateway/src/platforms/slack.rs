#![allow(dead_code)]
//! Slack platform adapter.
//!
//! Mirrors the Python `gateway/platforms/slack.py`.
//!
//! Uses Slack's Event API (HTTP webhook) for receiving messages:
//! - Configures a webhook endpoint that Slack POSTs events to
//! - Supports DMs, channel messages, and app mentions
//! - Thread support via thread_ts
//!
//! Outbound messages sent via Slack Web API (chat.postMessage).
//!
//! Required env vars:
//!   - SLACK_BOT_TOKEN (xoxb-...) — API auth
//!   - SLACK_SIGNING_SECRET — webhook signature verification
//!
//! Optional:
//!   - SLACK_WEBHOOK_PORT (default: 8767)
//!   - SLACK_WEBHOOK_PATH (default: /slack/events)

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::post,
};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::info;

use crate::dedup::MessageDeduplicator;

/// Slack Web API base URL.
const API_BASE: &str = "https://slack.com/api";
/// Max message length for a single Slack message.
const MAX_MESSAGE_LENGTH: usize = 39000;

/// Slack platform configuration.
#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Bot token (xoxb-...).
    pub bot_token: String,
    /// Signing secret for webhook verification.
    pub signing_secret: String,
    /// Webhook server port.
    pub webhook_port: u16,
    /// Webhook callback path.
    pub webhook_path: String,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            bot_token: std::env::var("SLACK_BOT_TOKEN").unwrap_or_default(),
            signing_secret: std::env::var("SLACK_SIGNING_SECRET").unwrap_or_default(),
            webhook_port: std::env::var("SLACK_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8767),
            webhook_path: std::env::var("SLACK_WEBHOOK_PATH")
                .ok()
                .unwrap_or_else(|| "/slack/events".to_string()),
        }
    }
}

impl SlackConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Parsed file attachment from an inbound Slack message.
#[derive(Debug, Clone)]
pub struct SlackAttachment {
    /// File ID.
    pub id: String,
    /// File name.
    pub file_name: String,
    /// MIME type.
    pub mime_type: Option<String>,
    /// Download URL (private, requires bot token).
    pub url_private: String,
    /// File size in bytes.
    pub size: u64,
    /// Local filesystem path after download.
    pub local_path: Option<String>,
}

/// Inbound message event from Slack.
#[derive(Debug, Clone)]
pub struct SlackMessageEvent {
    /// Unique event ID (for dedup).
    pub event_id: String,
    /// Channel ID.
    pub channel_id: String,
    /// Team ID.
    pub team_id: Option<String>,
    /// User ID of sender.
    pub user_id: String,
    /// User display name (if resolved).
    pub user_name: Option<String>,
    /// Message content.
    pub content: String,
    /// Thread timestamp (if in a thread).
    pub thread_ts: Option<String>,
    /// Whether this is a DM (im) vs channel.
    pub is_dm: bool,
    /// File attachments.
    pub attachments: Vec<SlackAttachment>,
}

type SlackMessageHandler = Arc<dyn Fn(SlackMessageEvent) + Send + Sync>;

/// Webhook state shared between route handlers.
struct WebhookState {
    _config: SlackConfig,
    _dedup: Arc<MessageDeduplicator>,
    on_message: Arc<Mutex<Option<SlackMessageHandler>>>,
}

/// Slack platform adapter.
pub struct SlackAdapter {
    config: SlackConfig,
    client: Client,
    dedup: Arc<MessageDeduplicator>,
    /// Bot user ID (fetched lazily).
    _bot_user_id: Arc<Mutex<Option<String>>>,
    /// User name cache.
    _user_name_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl SlackAdapter {
    pub fn new(config: SlackConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: Arc::new(MessageDeduplicator::with_params(300, 2000)),
            _bot_user_id: Arc::new(Mutex::new(None)),
            _user_name_cache: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new())),
            config,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.bot_token)
    }

    // ── Webhook Server ──────────────────────────────────────────────────────

    /// Run the webhook server for Slack Event API.
    pub async fn run(
        &self,
        on_message: impl Fn(SlackMessageEvent) + Send + Sync + 'static,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let state = WebhookState {
            _config: self.config.clone(),
            _dedup: self.dedup.clone(),
            on_message: Arc::new(Mutex::new(Some(Arc::new(on_message)))),
        };

        let app = Router::new()
            .route(&self.config.webhook_path, post(handle_slack_webhook))
            .with_state(Arc::new(state));

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", self.config.webhook_port))
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        info!("Slack webhook listening on 0.0.0.0:{}{}",
            self.config.webhook_port, self.config.webhook_path);

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("Slack webhook shutting down");
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    }

    // ── Inbound message parsing ─────────────────────────────────────────────

    fn parse_event(&self, event: &serde_json::Value) -> Option<SlackMessageEvent> {
        let event_id = event.get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let event_data = event.get("event")?;
        let event_type = event_data.get("type").and_then(|v| v.as_str())?;

        // Only process message events
        if event_type != "message" && event_type != "app_mention" {
            return None;
        }

        // Skip bot messages and message subtypes we don't care about
        if event_data.get("bot_id").is_some() {
            return None;
        }
        if let Some(subtype) = event_data.get("subtype").and_then(|v| v.as_str()) {
            // Skip message_changed, message_deleted, bot messages
            if subtype == "message_changed" || subtype == "message_deleted" || subtype == "bot_message" {
                return None;
            }
        }

        let channel_id = event_data.get("channel").and_then(|v| v.as_str())?.to_string();
        let channel_type = event_data.get("channel_type").and_then(|v| v.as_str()).unwrap_or("");
        let is_dm = channel_type == "im";

        let user_id = event_data.get("user").and_then(|v| v.as_str())?.to_string();
        let thread_ts = event_data.get("thread_ts").and_then(|v| v.as_str()).map(String::from);
        let team_id = event.get("team_id").and_then(|v| v.as_str()).map(String::from);

        let content = event_data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Parse file attachments
        let mut attachments = Vec::new();
        if let Some(files) = event_data.get("files").and_then(|v| v.as_array()) {
            for file in files {
                let id = file.get("id").and_then(|v| v.as_str())?;
                let file_name = file.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let mime_type = file.get("mimetype").and_then(|v| v.as_str()).map(String::from);
                let url_private = file.get("url_private").and_then(|v| v.as_str())?;
                let size = file.get("size").and_then(|v| v.as_u64()).unwrap_or(0);

                attachments.push(SlackAttachment {
                    id: id.to_string(),
                    file_name: file_name.to_string(),
                    mime_type,
                    url_private: url_private.to_string(),
                    size,
                    local_path: None,
                });
            }
        }

        // Deduplication
        let dedup_key = format!("{}:{}", channel_id, event_id);
        if self.dedup.is_duplicate(&dedup_key) {
            return None;
        }
        self.dedup.insert(dedup_key);

        Some(SlackMessageEvent {
            event_id,
            channel_id,
            team_id,
            user_id,
            user_name: None,
            content,
            thread_ts,
            is_dm,
            attachments,
        })
    }

    // ── User resolution ─────────────────────────────────────────────────────

    async fn resolve_user_name(&self, user_id: &str) -> Option<String> {
        // Check cache first
        {
            let cache = self._user_name_cache.lock().unwrap();
            if let Some(name) = cache.get(user_id) {
                return Some(name.clone());
            }
        }

        // Fetch from Slack API
        let url = format!("{API_BASE}/users.info");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .query(&[("user", user_id)])
            .send()
            .await
            .ok()?;

        let body: serde_json::Value = resp.json().await.ok()?;
        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return None;
        }

        let user = body.get("user")?;
        let name = user
            .get("real_name")
            .and_then(|v| v.as_str())
            .or_else(|| user.get("name").and_then(|v| v.as_str()))
            .unwrap_or(user_id)
            .to_string();

        self._user_name_cache.lock().unwrap().insert(user_id.to_string(), name.clone());
        Some(name)
    }

    // ── Outbound sending ────────────────────────────────────────────────────

    /// Send a text message to a Slack channel or thread.
    pub async fn send_text(&self, channel_id: &str, text: &str) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let chunks = split_slack_message(text);
        for chunk in chunks {
            self.post_message(channel_id, &chunk, None).await?;
        }
        Ok(())
    }

    /// Send a text message in a thread.
    pub async fn send_text_in_thread(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: &str,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let chunks = split_slack_message(text);
        for chunk in chunks {
            self.post_message(channel_id, &chunk, Some(thread_ts)).await?;
        }
        Ok(())
    }

    async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/chat.postMessage");
        let mut body = serde_json::json!({
            "channel": channel_id,
            "text": text,
            "unfurl_links": false,
        });

        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("postMessage request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("postMessage HTTP error: {}", resp.status()));
        }

        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("postMessage parse error: {e}"))?;

        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Slack API error: {err}"));
        }

        Ok(())
    }

    /// Upload and send a file to a channel.
    pub async fn send_file(
        &self,
        channel_id: &str,
        file_path: &str,
        _thread_ts: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let bytes = std::fs::read(file_path).map_err(|e| format!("read file failed: {e}"))?;
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name.to_string());

        let form = reqwest::multipart::Form::new()
            .text("channels", channel_id.to_string())
            .part("file", part);

        let resp = self
            .client
            .post(format!("{API_BASE}/files.upload"))
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("files.upload request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("files.upload HTTP error: {}", resp.status()));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("files.upload parse error: {e}"))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Slack files.upload error: {err}"));
        }

        Ok(())
    }

    /// Add a reaction emoji to a message.
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/reactions.add");
        let body = serde_json::json!({
            "channel": channel_id,
            "timestamp": timestamp,
            "name": emoji,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("reactions.add failed: {e}"))?;

        let resp_body: serde_json::Value = resp.json().await.unwrap_or_default();
        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(format!("Slack reaction error: {err}"));
        }
        Ok(())
    }
}

// ── Webhook handler ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SlackWebhookPayload {
    #[serde(rename = "type")]
    payload_type: String,
    token: Option<String>,
    challenge: Option<String>,
    event: Option<serde_json::Value>,
    event_id: Option<String>,
    team_id: Option<String>,
}

async fn handle_slack_webhook(
    State(state): State<Arc<WebhookState>>,
    Json(payload): Json<SlackWebhookPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    // URL verification challenge (required for Slack app setup)
    if payload.payload_type == "url_verification" {
        if let Some(challenge) = payload.challenge {
            info!("Slack URL verification challenge received");
            return (StatusCode::OK, Json(serde_json::json!({ "challenge": challenge })));
        }
    }

    // Only process event callbacks
    if payload.payload_type != "event_callback" {
        return (StatusCode::OK, Json(serde_json::json!({})));
    }

    let Some(event_data) = payload.event else {
        return (StatusCode::OK, Json(serde_json::json!({})));
    };

    let full_event = serde_json::json!({
        "event": event_data,
        "event_id": payload.event_id.unwrap_or_default(),
        "team_id": payload.team_id,
    });

    let guard = state.on_message.lock().await;
    if let Some(callback) = guard.as_ref() {
        // Parse the event into a SlackMessageEvent
        // For simplicity, we construct it inline here
        if let Some(msg_event) = parse_slack_event_data(&full_event) {
            callback(msg_event);
        }
    }
    drop(guard);

    (StatusCode::OK, Json(serde_json::json!({})))
}

fn parse_slack_event_data(event: &serde_json::Value) -> Option<SlackMessageEvent> {
    let event_data = event.get("event")?;
    let event_type = event_data.get("type").and_then(|v| v.as_str())?;

    if event_type != "message" && event_type != "app_mention" {
        return None;
    }

    if event_data.get("bot_id").is_some() {
        return None;
    }
    if let Some(subtype) = event_data.get("subtype").and_then(|v| v.as_str()) {
        if subtype == "message_changed" || subtype == "message_deleted" || subtype == "bot_message" {
            return None;
        }
    }

    let channel_id = event_data.get("channel").and_then(|v| v.as_str())?.to_string();
    let channel_type = event_data.get("channel_type").and_then(|v| v.as_str()).unwrap_or("");
    let is_dm = channel_type == "im";
    let user_id = event_data.get("user").and_then(|v| v.as_str())?.to_string();
    let thread_ts = event_data.get("thread_ts").and_then(|v| v.as_str()).map(String::from);
    let team_id = event.get("team_id").and_then(|v| v.as_str()).map(String::from);
    let content = event_data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let event_id = event.get("event_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

    Some(SlackMessageEvent {
        event_id,
        channel_id,
        team_id,
        user_id,
        user_name: None,
        content,
        thread_ts,
        is_dm,
        attachments: Vec::new(),
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a long message into chunks that fit within Slack's ~40000 char limit.
fn split_slack_message(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_MESSAGE_LENGTH {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= MAX_MESSAGE_LENGTH {
            chunks.push(remaining.to_string());
            break;
        }

        let mut split_pos = MAX_MESSAGE_LENGTH;
        let char_indices: Vec<(usize, char)> = remaining.char_indices().collect();

        for i in (0..char_indices.len().min(MAX_MESSAGE_LENGTH)).rev() {
            if char_indices[i].1 == '\n' {
                split_pos = i + 1;
                break;
            }
        }

        if split_pos == MAX_MESSAGE_LENGTH {
            for i in (0..char_indices.len().min(MAX_MESSAGE_LENGTH)).rev() {
                if char_indices[i].1 == ' ' {
                    split_pos = i + 1;
                    break;
                }
            }
        }

        let actual_byte_pos = if split_pos >= char_indices.len() {
            remaining.len()
        } else {
            char_indices[split_pos].0
        };

        let (chunk, rest) = remaining.split_at(actual_byte_pos);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    chunks
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_slack_message_short() {
        let chunks = split_slack_message("Hello world");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_slack_message_long() {
        let long = "a".repeat(40000);
        let chunks = split_slack_message(&long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn test_slack_config_from_env() {
        let cfg = SlackConfig::default();
        assert_eq!(cfg.bot_token, std::env::var("SLACK_BOT_TOKEN").unwrap_or_default());
    }

    #[test]
    fn test_parse_url_verification() {
        let payload = serde_json::json!({
            "type": "url_verification",
            "challenge": "test_challenge_123",
        });
        let parsed: SlackWebhookPayload = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.payload_type, "url_verification");
        assert_eq!(parsed.challenge, Some("test_challenge_123".to_string()));
    }
}
