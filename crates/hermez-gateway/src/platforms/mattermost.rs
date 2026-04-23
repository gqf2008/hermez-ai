#![allow(dead_code)]
//! Mattermost platform adapter.
//!
//! Mirrors the Python `gateway/platforms/mattermost.py`.
//!
//! Connects to a self-hosted (or cloud) Mattermost instance via its REST API
//! (v4) and WebSocket for real-time events.
//!
//! Receiving messages:
//! - WebSocket API for real-time `posted` events
//!
//! Outbound:
//! - `POST /api/v4/posts` for text messages
//! - File upload via `POST /api/v4/files` then post with `file_ids`
//!
//! Required env vars:
//!   - MATTERMOST_SERVER_URL
//!   - MATTERMOST_TOKEN
//!
//! Optional:
//!   - MATTERMOST_TEAM
//!   - MATTERMOST_CHANNEL
//!   - MATTERMOST_BOT_USERNAME
//!   - MATTERMOST_REPLY_MODE (off | thread; default: off)
//!   - MATTERMOST_REQUIRE_MENTION (default: true)
//!   - MATTERMOST_FREE_RESPONSE_CHANNELS (comma-separated channel IDs)
//!   - MATTERMOST_ALLOWED_USERS (comma-separated user IDs)
//!   - MATTERMOST_ALLOW_ALL_USERS (default: false)
//!   - MATTERMOST_HOME_CHANNEL

use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::MessageDeduplicator;
use crate::runner::MessageHandler;

// ── Constants ──────────────────────────────────────────────────────────────

const API_VERSION: &str = "/api/v4";
const MAX_POST_LENGTH: usize = 4000;
const RECONNECT_BASE_DELAY: f64 = 2.0;
const RECONNECT_MAX_DELAY: f64 = 60.0;
const RECONNECT_JITTER: f64 = 0.2;

// ── Configuration ──────────────────────────────────────────────────────────

/// Mattermost platform configuration.
#[derive(Debug, Clone)]
pub struct MattermostConfig {
    pub server_url: String,
    pub token: String,
    pub team_name: String,
    pub channel_name: String,
    pub bot_username: String,
    pub reply_mode: String,
    pub require_mention: bool,
    pub free_response_channels: HashSet<String>,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub home_channel: Option<String>,
}

impl Default for MattermostConfig {
    fn default() -> Self {
        let free_channels = std::env::var("MATTERMOST_FREE_RESPONSE_CHANNELS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let allowed_users = std::env::var("MATTERMOST_ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            server_url: std::env::var("MATTERMOST_SERVER_URL").unwrap_or_default(),
            token: std::env::var("MATTERMOST_TOKEN").unwrap_or_default(),
            team_name: std::env::var("MATTERMOST_TEAM").unwrap_or_default(),
            channel_name: std::env::var("MATTERMOST_CHANNEL").unwrap_or_default(),
            bot_username: std::env::var("MATTERMOST_BOT_USERNAME").unwrap_or_default(),
            reply_mode: std::env::var("MATTERMOST_REPLY_MODE")
                .unwrap_or_else(|_| "off".to_string())
                .to_lowercase(),
            require_mention: !is_env_false("MATTERMOST_REQUIRE_MENTION"),
            free_response_channels: free_channels,
            allowed_users,
            allow_all_users: is_env_true("MATTERMOST_ALLOW_ALL_USERS"),
            home_channel: std::env::var("MATTERMOST_HOME_CHANNEL").ok(),
        }
    }
}

impl MattermostConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        !self.server_url.is_empty() && !self.token.is_empty()
    }
}

fn is_env_false(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

fn is_env_true(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

// ── Data types ─────────────────────────────────────────────────────────────

/// Parsed file attachment from an inbound Mattermost message.
#[derive(Debug, Clone)]
pub struct MattermostAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub local_path: Option<String>,
}

/// Inbound message event from Mattermost.
#[derive(Debug, Clone)]
pub struct MattermostMessageEvent {
    pub post_id: String,
    pub channel_id: String,
    pub channel_type: String,
    pub user_id: String,
    pub user_name: String,
    pub content: String,
    pub is_dm: bool,
    pub root_id: Option<String>,
    pub attachments: Vec<MattermostAttachment>,
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// Mattermost platform adapter.
pub struct MattermostAdapter {
    config: MattermostConfig,
    client: Client,
    dedup: MessageDeduplicator,
    bot_user_id: Arc<Mutex<Option<String>>>,
    bot_username: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
}

impl MattermostAdapter {
    pub fn new(config: MattermostConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: MessageDeduplicator::new(2000, 300.0),
            bot_user_id: Arc::new(Mutex::new(None)),
            bot_username: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            config,
        }
    }

    fn base_url(&self) -> String {
        self.config.server_url.trim_end_matches('/').to_string()
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}/{}", self.base_url(), API_VERSION, path.trim_start_matches('/'))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.token)
    }

    // ── HTTP helpers ──────────────────────────────────────────────────────

    async fn api_get(&self, path: &str) -> Result<Value, String> {
        let url = self.api_url(path);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("GET {path} request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET {path} HTTP error {status}: {body}"));
        }

        resp.json().await.map_err(|e| format!("GET {path} parse error: {e}"))
    }

    async fn api_post(&self, path: &str, payload: &Value) -> Result<Value, String> {
        let url = self.api_url(path);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("POST {path} request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("POST {path} HTTP error {status}: {body}"));
        }

        resp.json().await.map_err(|e| format!("POST {path} parse error: {e}"))
    }

    async fn api_put(&self, path: &str, payload: &Value) -> Result<Value, String> {
        let url = self.api_url(path);
        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("PUT {path} request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("PUT {path} HTTP error {status}: {body}"));
        }

        resp.json().await.map_err(|e| format!("PUT {path} parse error: {e}"))
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────

    /// Connect to Mattermost and start the WebSocket listener.
    pub async fn run(
        self: Arc<Self>,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) {
        if !self.config.is_configured() {
            error!("Mattermost: URL or token not configured");
            return;
        }

        self.running.store(true, Ordering::SeqCst);

        // Verify credentials and fetch bot identity
        match self.api_get("users/me").await {
            Ok(me) => {
                let bot_id = me.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let bot_name = me.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if bot_id.is_empty() {
                    error!("Mattermost: failed to authenticate — check MATTERMOST_TOKEN and MATTERMOST_SERVER_URL");
                    return;
                }
                *self.bot_user_id.lock().await = Some(bot_id);
                *self.bot_username.lock().await = Some(bot_name);
                info!(
                    "Mattermost: authenticated on {}",
                    self.config.server_url
                );
            }
            Err(e) => {
                error!("Mattermost: auth failed: {e}");
                return;
            }
        }

        // WebSocket loop with reconnect
        let mut delay = RECONNECT_BASE_DELAY;
        while running.load(Ordering::SeqCst) && self.running.load(Ordering::SeqCst) {
            match self.ws_connect_and_listen(handler.clone(), running.clone()).await {
                Ok(()) => {
                    delay = RECONNECT_BASE_DELAY;
                }
                Err(e) => {
                    if !running.load(Ordering::SeqCst) || !self.running.load(Ordering::SeqCst) {
                        break;
                    }
                    let err_lower = e.to_lowercase();
                    if err_lower.contains("401") || err_lower.contains("403") || err_lower.contains("unauthorized") {
                        error!("Mattermost WS permanent error: {e} — stopping reconnect");
                        break;
                    }
                    warn!("Mattermost WS error: {e} — reconnecting in {delay:.0}s");
                    let jitter = delay * RECONNECT_JITTER * (now_secs() % 1.0);
                    sleep(Duration::from_secs_f64(delay + jitter)).await;
                    delay = (delay * 2.0).min(RECONNECT_MAX_DELAY);
                }
            }
        }

        info!("Mattermost: adapter stopped");
    }

    // ── WebSocket ─────────────────────────────────────────────────────────

    async fn ws_connect_and_listen(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let ws_url = self
            .config
            .server_url
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1)
            + "/api/v4/websocket";

        info!("Mattermost: connecting to {ws_url}");

        let uri: tokio_tungstenite::tungstenite::http::Uri = ws_url
            .parse()
            .map_err(|e| format!("Invalid WebSocket URL: {e}"))?;

        let (ws_stream, _) = tokio_tungstenite::connect_async(uri)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (mut write_half, mut read_half) = ws_stream.split();

        // Authenticate via WebSocket
        let auth_msg = serde_json::json!({
            "seq": 1,
            "action": "authentication_challenge",
            "data": {"token": self.config.token},
        });
        write_half
            .send(tokio_tungstenite::tungstenite::Message::Text(auth_msg.to_string().into()))
            .await
            .map_err(|e| format!("WS auth send failed: {e}"))?;

        info!("Mattermost: WebSocket connected and authenticated");

        while running.load(Ordering::SeqCst) && self.running.load(Ordering::SeqCst) {
            tokio::select! {
                msg = read_half.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            if let Err(e) = self.handle_ws_message(&text, &handler).await {
                                warn!("Mattermost WS message handling error: {e}");
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                            info!("Mattermost: WebSocket closed: {frame:?}");
                            return Err("WebSocket closed by server".to_string());
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(ping))) => {
                            let _ = write_half.send(tokio_tungstenite::tungstenite::Message::Pong(ping)).await;
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {}
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bin))) => {
                            if let Ok(text) = String::from_utf8(bin.into()) {
                                if let Err(e) = self.handle_ws_message(&text, &handler).await {
                                    warn!("Mattermost WS binary handling error: {e}");
                                }
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Frame(_))) => {}
                        Some(Err(e)) => {
                            return Err(format!("WebSocket read error: {e}"));
                        }
                        None => {
                            return Err("WebSocket stream ended".to_string());
                        }
                    }
                }
                _ = sleep(Duration::from_millis(200)) => {
                    if !running.load(Ordering::SeqCst) || !self.running.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_ws_message(
        &self,
        text: &str,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    ) -> Result<(), String> {
        let event: Value = serde_json::from_str(text).map_err(|e| format!("JSON parse error: {e}"))?;

        let event_type = event.get("event").and_then(|v| v.as_str()).unwrap_or("");
        if event_type != "posted" {
            return Ok(());
        }

        let data = event.get("data").unwrap_or(&Value::Null);
        let raw_post_str = data.get("post").and_then(|v| v.as_str()).unwrap_or("");
        if raw_post_str.is_empty() {
            return Ok(());
        }

        let post: Value = serde_json::from_str(raw_post_str).map_err(|e| format!("post parse error: {e}"))?;

        // Ignore own messages
        let bot_id = self.bot_user_id.lock().await.clone().unwrap_or_default();
        if post.get("user_id").and_then(|v| v.as_str()) == Some(&bot_id) {
            return Ok(());
        }

        // Ignore system posts
        if post.get("type").and_then(|v| v.as_str()).is_some_and(|t| !t.is_empty()) {
            return Ok(());
        }

        let post_id = post.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Dedup
        if self.dedup.is_duplicate(&post_id) {
            return Ok(());
        }

        let channel_id = post.get("channel_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let channel_type_raw = data.get("channel_type").and_then(|v| v.as_str()).unwrap_or("O");
        let is_dm = channel_type_raw == "D";

        let mut message_text = post.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Mention-gating for non-DM channels
        if !is_dm {
            let bot_username = self.bot_username.lock().await.clone().unwrap_or_default();
            let bot_user_id = self.bot_user_id.lock().await.clone().unwrap_or_default();
            let is_free_channel = self.config.free_response_channels.contains(&channel_id);

            let mention_patterns = vec![
                format!("@{bot_username}"),
                format!("@{bot_user_id}"),
            ];
            let has_mention = mention_patterns.iter().any(|p| {
                message_text.to_lowercase().contains(&p.to_lowercase())
            });

            if self.config.require_mention && !is_free_channel && !has_mention {
                debug!("Mattermost: skipping non-DM message without @mention (channel={channel_id})");
                return Ok(());
            }

            // Strip @mention from message text
            if has_mention {
                for pattern in &mention_patterns {
                    let re = regex::Regex::new(&regex::escape(pattern)).ok();
                    if let Some(re) = re {
                        message_text = re.replace_all(&message_text, "").to_string();
                    }
                }
                message_text = message_text.trim().to_string();
            }
        }

        // User authorization
        let sender_id = post.get("user_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !self.config.allow_all_users && !self.config.allowed_users.is_empty()
            && !self.config.allowed_users.iter().any(|u| u == &sender_id) {
                warn!("Mattermost: unauthorized user {sender_id}");
                return Ok(());
            }

        let sender_name = data.get("sender_name").and_then(|v| v.as_str()).unwrap_or("").trim_start_matches('@').to_string();
        let root_id = post.get("root_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);

        // Download file attachments
        let file_ids = post.get("file_ids").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut attachments = Vec::new();
        for fid_val in file_ids {
            let fid = fid_val.as_str().unwrap_or("");
            if fid.is_empty() {
                continue;
            }
            match self.download_file(fid).await {
                Ok(att) => {
                    attachments.push(att);
                }
                Err(e) => {
                    warn!("Mattermost: failed to download file {fid}: {e}");
                }
            }
        }

        let _msg_event = MattermostMessageEvent {
            post_id: post_id.clone(),
            channel_id: channel_id.clone(),
            channel_type: channel_type_raw.to_string(),
            user_id: sender_id,
            user_name: sender_name,
            content: message_text.clone(),
            is_dm,
            root_id: root_id.clone(),
            attachments,
        };

        // Dispatch to handler
        let handler_guard = handler.lock().await;
        let handler_ref = handler_guard.as_ref().cloned();
        drop(handler_guard);

        if let Some(h) = handler_ref {
            let chat_id = channel_id.clone();
            let content = message_text.clone();
            let adapter = self.clone_like();
            tokio::spawn(async move {
                match h.handle_message(Platform::Mattermost, &chat_id, &content, None).await {
                    Ok(result) => {
                        if !result.response.is_empty() {
                            let reply_to = if adapter.config.reply_mode == "thread" {
                                root_id.as_deref().or(Some(&post_id))
                            } else {
                                None
                            };
                            if let Err(e) = adapter.send_text(&chat_id, &result.response, reply_to).await {
                                error!("Mattermost send failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Mattermost handler error: {e}");
                    }
                }
            });
        }

        Ok(())
    }

    async fn download_file(&self, file_id: &str) -> Result<MattermostAttachment, String> {
        let info = self.api_get(&format!("files/{file_id}/info")).await?;
        let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let mime = info.get("mime_type").and_then(|v| v.as_str()).unwrap_or("application/octet-stream").to_string();
        let size = info.get("size").and_then(|v| v.as_u64()).unwrap_or(0);

        let url = self.api_url(&format!("files/{file_id}"));
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("download request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("download HTTP error: {}", resp.status()));
        }

        let bytes = resp.bytes().await.map_err(|e| format!("download bytes error: {e}"))?;

        let cache_dir = hermez_core::get_hermez_home().join("mattermost").join("media");
        tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| format!("mkdir failed: {e}"))?;

        let safe_name = name.replace('/', "_");
        let local_path = cache_dir.join(format!("{safe_name}_{file_id}"));
        tokio::fs::write(&local_path, bytes).await.map_err(|e| format!("write failed: {e}"))?;

        Ok(MattermostAttachment {
            id: file_id.to_string(),
            name,
            mime_type: mime,
            size,
            local_path: Some(local_path.to_string_lossy().to_string()),
        })
    }

    // ── Outbound sending ──────────────────────────────────────────────────

    /// Send a text message to a channel.
    pub async fn send_text(&self, channel_id: &str, text: &str, reply_to: Option<&str>) -> Result<(), String> {
        if self.config.token.is_empty() {
            return Err("Mattermost token not configured".to_string());
        }

        let formatted = self.format_message(text);
        let chunks = split_message(&formatted, MAX_POST_LENGTH);

        for chunk in chunks {
            let mut payload = serde_json::json!({
                "channel_id": channel_id,
                "message": chunk,
            });
            if let Some(root) = reply_to {
                payload["root_id"] = Value::String(root.to_string());
            }

            self.api_post("posts", &payload).await?;
        }

        Ok(())
    }

    /// Edit an existing post.
    pub async fn edit_message(&self, _channel_id: &str, message_id: &str, content: &str) -> Result<(), String> {
        if self.config.token.is_empty() {
            return Err("Mattermost token not configured".to_string());
        }

        let formatted = self.format_message(content);
        let payload = serde_json::json!({
            "message": formatted,
        });

        self.api_put(&format!("posts/{message_id}/patch"), &payload).await?;
        Ok(())
    }

    /// Send a typing indicator.
    pub async fn send_typing(&self, channel_id: &str) -> Result<(), String> {
        let bot_id = self.bot_user_id.lock().await.clone().unwrap_or_default();
        if bot_id.is_empty() {
            return Err("Bot user ID not known".to_string());
        }
        let payload = serde_json::json!({
            "channel_id": channel_id,
        });
        self.api_post(&format!("users/{bot_id}/typing"), &payload).await?;
        Ok(())
    }

    /// Upload and send a local file.
    pub async fn send_file(
        &self,
        channel_id: &str,
        file_path: &std::path::Path,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let file_data = tokio::fs::read(file_path).await.map_err(|e| format!("read file failed: {e}"))?;
        self.send_file_bytes(channel_id, &file_data, file_name, caption, reply_to).await
    }

    /// Send a file from an in-memory byte slice.
    pub async fn send_file_bytes(
        &self,
        channel_id: &str,
        data: &[u8],
        file_name: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        let file_id = self.upload_file(channel_id, data, file_name).await?;
        let mut payload = serde_json::json!({
            "channel_id": channel_id,
            "message": caption.unwrap_or(""),
            "file_ids": [file_id],
        });
        if let Some(root) = reply_to {
            payload["root_id"] = Value::String(root.to_string());
        }
        self.api_post("posts", &payload).await?;
        Ok(())
    }

    /// Send an image from a URL (downloads and uploads).
    pub async fn send_image(
        &self,
        channel_id: &str,
        image_url: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        let bytes = self
            .client
            .get(image_url)
            .send()
            .await
            .map_err(|e| format!("download image failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("read image bytes failed: {e}"))?;
        let filename = image_url
            .split('/')
            .next_back()
            .unwrap_or("image.png");
        self.send_file_bytes(channel_id, &bytes, filename, caption, reply_to).await
    }

    /// Send an image from an in-memory byte slice.
    pub async fn send_image_bytes(
        &self,
        channel_id: &str,
        data: &[u8],
        file_name: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        self.send_file_bytes(channel_id, data, file_name, caption, reply_to).await
    }

    async fn upload_file(&self, channel_id: &str, file_data: &[u8], filename: &str) -> Result<String, String> {
        let url = self.api_url("files");
        let part = reqwest::multipart::Part::bytes(file_data.to_vec())
            .file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("channel_id", channel_id.to_string())
            .part("files", part);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("file upload request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("file upload HTTP error {status}: {body}"));
        }

        let body: Value = resp.json().await.map_err(|e| format!("file upload parse error: {e}"))?;
        let infos = body.get("file_infos").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        infos
            .first()
            .and_then(|info| info.get("id").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| "file upload returned no file info".to_string())
    }

    fn format_message(&self, content: &str) -> String {
        // Convert ![alt](url) to just the URL — Mattermost renders image URLs as inline previews
        static IMAGE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = IMAGE_RE.get_or_init(|| {
            regex::Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)")
                .unwrap_or_else(|_| regex::Regex::new("").unwrap())
        });
        re.replace_all(content, "${2}").to_string()
    }

    /// Get channel info.
    pub async fn get_chat_info(&self, channel_id: &str) -> Result<HashMap<String, String>, String> {
        match self.api_get(&format!("channels/{channel_id}")).await {
            Ok(data) => {
                let ch_type = match data.get("type").and_then(|v| v.as_str()).unwrap_or("O") {
                    "D" => "dm",
                    "G" | "P" => "group",
                    _ => "channel",
                };
                let name = data
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .or_else(|| data.get("name").and_then(|v| v.as_str()))
                    .unwrap_or(channel_id)
                    .to_string();
                let mut map = HashMap::new();
                map.insert("name".to_string(), name);
                map.insert("type".to_string(), ch_type.to_string());
                Ok(map)
            }
            Err(_) => {
                let mut map = HashMap::new();
                map.insert("name".to_string(), channel_id.to_string());
                map.insert("type".to_string(), "channel".to_string());
                Ok(map)
            }
        }
    }

    // ── Clone helper ──────────────────────────────────────────────────────

    fn clone_like(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            bot_user_id: self.bot_user_id.clone(),
            bot_username: self.bot_username.clone(),
            running: self.running.clone(),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a long message into chunks.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let mut split_at = max_len;
        while split_at > 0 && !remaining.is_char_boundary(split_at) {
            split_at -= 1;
        }
        if split_at == 0 {
            split_at = max_len;
        }

        // Prefer newline split
        if let Some(pos) = remaining[..split_at].rfind('\n') {
            if pos > 0 {
                split_at = pos + 1;
            }
        } else if let Some(pos) = remaining[..split_at].rfind(' ') {
            if pos > 0 {
                split_at = pos + 1;
            }
        }

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    chunks
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ── Tests ─────────────────────────────────────────────────────────────────--

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("Hello world", MAX_POST_LENGTH);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_message_long() {
        let long = "a".repeat(5000);
        let chunks = split_message(&long, MAX_POST_LENGTH);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_POST_LENGTH);
        }
    }

    #[test]
    fn test_format_message() {
        let adapter = MattermostAdapter::new(MattermostConfig::default());
        let result = adapter.format_message("Hello ![alt](http://x.com/img.png) world");
        assert!(result.contains("http://x.com/img.png"));
        assert!(!result.contains("![alt]"));
    }

    #[test]
    fn test_config_from_env() {
        let cfg = MattermostConfig::default();
        assert_eq!(cfg.server_url, std::env::var("MATTERMOST_SERVER_URL").unwrap_or_default());
    }

    #[test]
    fn test_is_configured() {
        let mut cfg = MattermostConfig::default();
        cfg.server_url = "https://mm.example.com".to_string();
        cfg.token = "test_token".to_string();
        assert!(cfg.is_configured());

        cfg.token = "".to_string();
        assert!(!cfg.is_configured());
    }
}
