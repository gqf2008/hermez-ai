#![allow(dead_code)]
//! Telegram platform adapter.
//!
//! Mirrors the Python `gateway/platforms/telegram.py`.
//!
//! Uses the Telegram Bot API directly (no library dependency):
//! - Long-poll `getUpdates` for inbound messages
//! - Webhook mode for cloud deployments
//! - `sendMessage` / `sendPhoto` / `sendDocument` / `sendVoice` / `sendVideo` /
//!   `sendAnimation` for outbound
//! - MarkdownV2 formatting with proper escaping
//! - Deduplication via MessageDeduplicator
//! - Flood control and retry handling
//!
//! Supports text, photo, document, voice, video, and animation messages.
//! Group/thread IDs are passed through as chat_id strings.

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    routing::post,
};
use reqwest::Client;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::dedup::MessageDeduplicator;

/// Telegram Bot API base URL.
const API_BASE: &str = "https://api.telegram.org/bot";
/// Poll timeout in seconds (long-polling).
const POLL_TIMEOUT_SECS: u64 = 30;
/// API request timeout in seconds.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Max message length for a single sendMessage call.
const MAX_MESSAGE_LENGTH: usize = 4096;

/// Telegram platform configuration.
#[derive(Debug, Clone)]
pub struct TelegramConfig {
    /// Bot token from BotFather.
    pub bot_token: String,
    /// Optional: disable link previews by default.
    pub disable_link_previews: bool,
    /// Webhook mode: public HTTPS URL (e.g. https://app.fly.dev/telegram).
    pub webhook_url: Option<String>,
    /// Webhook server listen port (default 8443).
    pub webhook_port: u16,
    /// Webhook secret token for update verification.
    pub webhook_secret: Option<String>,
    /// Webhook callback path (default "/telegram").
    pub webhook_path: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default(),
            disable_link_previews: std::env::var("TELEGRAM_DISABLE_LINK_PREVIEWS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(false),
            webhook_url: std::env::var("TELEGRAM_WEBHOOK_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string()),
            webhook_port: std::env::var("TELEGRAM_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8443),
            webhook_secret: std::env::var("TELEGRAM_WEBHOOK_SECRET")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string()),
            webhook_path: std::env::var("TELEGRAM_WEBHOOK_PATH")
                .ok()
                .unwrap_or_else(|| "/telegram".to_string()),
        }
    }
}

impl TelegramConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Parsed media attachment from an inbound Telegram message.
#[derive(Debug, Clone)]
pub struct TelegramMedia {
    /// Media type: photo, document, voice, video.
    pub media_type: String,
    /// File ID (unique Telegram identifier).
    pub file_id: String,
    /// Local filesystem path after download.
    pub local_path: Option<String>,
    /// Original file name (for documents).
    pub file_name: Option<String>,
    /// File size in bytes.
    pub file_size: Option<u64>,
    /// Caption text (if any).
    pub caption: Option<String>,
}

/// Inbound message event from Telegram.
#[derive(Debug, Clone)]
pub struct TelegramMessageEvent {
    /// Unique update ID.
    pub update_id: u64,
    /// Chat ID (string to support both numeric and channel IDs).
    pub chat_id: String,
    /// Sender user ID (if available).
    pub sender_id: Option<String>,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// Message content (text or caption).
    pub content: String,
    /// Message type: text, photo, document, voice, video.
    pub msg_type: String,
    /// Parsed media attachments.
    pub media: Vec<TelegramMedia>,
    /// For forum topics / threads.
    pub message_thread_id: Option<i64>,
}

/// Telegram platform adapter.
pub struct TelegramAdapter {
    pub config: TelegramConfig,
    client: Client,
    /// Monotonically increasing offset for long-poll.
    offset: AtomicU64,
    /// Deduplication cache.
    dedup: Arc<MessageDeduplicator>,
    /// API base URL with token embedded.
    api_url: String,
    /// Consecutive failure counter.
    consecutive_failures: AtomicU64,
    /// Last failure timestamp for backoff.
    last_failure: RwLock<Option<Instant>>,
}

impl TelegramAdapter {
    pub fn new(config: TelegramConfig) -> Self {
        let api_url = format!("{API_BASE}{}", config.bot_token);
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            offset: AtomicU64::new(0),
            dedup: Arc::new(MessageDeduplicator::with_params(300, 2000)),
            api_url,
            consecutive_failures: AtomicU64::new(0),
            last_failure: RwLock::new(None),
            config,
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn increment_failures(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        *self.last_failure.blocking_write() = Some(Instant::now());
    }

    fn reset_failures(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
        *self.last_failure.blocking_write() = None;
    }

    /// Build a full API method URL.
    fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_url)
    }

    // ── Flood-control / retry helpers ───────────────────────────────────────

    /// Parse `retry_after` from a Telegram error description.
    fn parse_retry_after(description: &str) -> Option<f64> {
        let lower = description.to_lowercase();
        if let Some(idx) = lower.find("retry after ") {
            let num_start = idx + "retry after ".len();
            let num_str: String = lower[num_start..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            return num_str.parse().ok();
        }
        None
    }

    /// Send a JSON API request with automatic retry for flood control and
    /// transient network errors.  Returns the response body on success.
    async fn post_json_with_retry(
        &self,
        method: &str,
        body: &mut serde_json::Value,
        thread_id: &mut Option<i64>,
        reply_to: &mut Option<i64>,
    ) -> Result<serde_json::Value, String> {
        let url = self.method_url(method);

        for attempt in 0..3 {
            // Inject current thread_id / reply_to into the body
            if let serde_json::Value::Object(ref mut map) = body {
                map.remove("message_thread_id");
                map.remove("reply_to_message_id");
            }
            if let Some(tid) = *thread_id {
                body["message_thread_id"] = serde_json::Value::Number(tid.into());
            }
            if let Some(rid) = *reply_to {
                body["reply_to_message_id"] = serde_json::Value::Number(rid.into());
            }

            match self.client.post(&url).json(body).send().await {
                Ok(resp) => {
                    let resp_body: serde_json::Value = resp
                        .json()
                        .await
                        .map_err(|e| format!("{method} parse error: {e}"))?;

                    if resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return Ok(resp_body);
                    }

                    let desc = resp_body
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");

                    // Flood control / RetryAfter
                    let retry_after = resp_body
                        .get("parameters")
                        .and_then(|p| p.get("retry_after"))
                        .and_then(|v| v.as_f64())
                        .or_else(|| Self::parse_retry_after(desc));

                    if let Some(wait) = retry_after {
                        if attempt < 2 {
                            tracing::warn!(
                                "Telegram flood control on {method} (attempt {}/3), retrying in {:.1}s: {desc}",
                                attempt + 1,
                                wait
                            );
                            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                            continue;
                        }
                        return Err(format!(
                            "Telegram flood control: retry after {wait}s"
                        ));
                    }

                    let desc_lower = desc.to_lowercase();

                    // Thread not found — retry without thread_id
                    if desc_lower.contains("thread not found") && thread_id.is_some() {
                        tracing::warn!(
                            "Thread {} not found, retrying without message_thread_id",
                            thread_id.unwrap()
                        );
                        *thread_id = None;
                        continue;
                    }

                    // Reply target deleted — retry without reply_to
                    if desc_lower.contains("message to be replied not found")
                        && reply_to.is_some()
                    {
                        tracing::warn!("Reply target deleted, retrying without reply_to");
                        *reply_to = None;
                        continue;
                    }

                    // Markdown parse error — signal caller so it can fall back
                    if method == "sendMessage"
                        && (desc_lower.contains("can't parse")
                            || desc_lower.contains("parse mode"))
                    {
                        return Err(format!("markdown_parse_error:{desc}"));
                    }

                    return Err(format!("Telegram {method} error: {desc}"));
                }
                Err(e) => {
                    if e.is_timeout() {
                        return Err(format!("Telegram {method} timed out: {e}"));
                    }
                    if attempt < 2 {
                        let wait = 2u64.pow(attempt);
                        tracing::warn!(
                            "Network error on {method} (attempt {}/3), retrying in {}s: {e}",
                            attempt + 1,
                            wait
                        );
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                    return Err(format!("Telegram {method} request failed: {e}"));
                }
            }
        }

        Err(format!("Telegram {method} max retries exceeded"))
    }

    /// Send a multipart API request with automatic retry.
    async fn post_multipart_with_retry(
        &self,
        method: &str,
        chat_id: &str,
        file_field: &'static str,
        file_bytes: Vec<u8>,
        file_name: &str,
        mime_type: Option<&str>,
        caption: Option<&str>,
        thread_id: Option<i64>,
    ) -> Result<serde_json::Value, String> {
        let url = self.method_url(method);
        let mut current_thread_id = thread_id;

        for attempt in 0..3 {
            let mut part = reqwest::multipart::Part::bytes(file_bytes.clone())
                .file_name(file_name.to_string());
            if let Some(mime) = mime_type {
                part = part
                    .mime_str(mime)
                    .map_err(|e| format!("mime error: {e}"))?;
            }

            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .part(file_field, part);

            if let Some(cap) = caption {
                form = form.text("caption", cap.to_string());
            }
            if let Some(tid) = current_thread_id {
                form = form.text("message_thread_id", tid.to_string());
            }

            match self.client.post(&url).multipart(form).send().await {
                Ok(resp) => {
                    let resp_body: serde_json::Value = resp
                        .json()
                        .await
                        .map_err(|e| format!("{method} parse error: {e}"))?;

                    if resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return Ok(resp_body);
                    }

                    let desc = resp_body
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");

                    let retry_after = resp_body
                        .get("parameters")
                        .and_then(|p| p.get("retry_after"))
                        .and_then(|v| v.as_f64())
                        .or_else(|| Self::parse_retry_after(desc));

                    if let Some(wait) = retry_after {
                        if attempt < 2 {
                            tracing::warn!(
                                "Telegram flood control on {method} (attempt {}/3), retrying in {:.1}s: {desc}",
                                attempt + 1,
                                wait
                            );
                            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                            continue;
                        }
                        return Err(format!(
                            "Telegram flood control: retry after {wait}s"
                        ));
                    }

                    let desc_lower = desc.to_lowercase();
                    if desc_lower.contains("thread not found") && current_thread_id.is_some() {
                        tracing::warn!(
                            "Thread not found, retrying without message_thread_id"
                        );
                        current_thread_id = None;
                        continue;
                    }

                    return Err(format!("Telegram {method} error: {desc}"));
                }
                Err(e) => {
                    if e.is_timeout() {
                        return Err(format!("Telegram {method} timed out: {e}"));
                    }
                    if attempt < 2 {
                        let wait = 2u64.pow(attempt);
                        tracing::warn!(
                            "Network error on {method} (attempt {}/3), retrying in {}s: {e}",
                            attempt + 1,
                            wait
                        );
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                    return Err(format!("Telegram {method} request failed: {e}"));
                }
            }
        }

        Err(format!("Telegram {method} max retries exceeded"))
    }

    // ── Polling ─────────────────────────────────────────────────────────────

    /// Long-poll for inbound updates.
    pub async fn get_updates(&self) -> Result<Vec<TelegramMessageEvent>, String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let offset = self.offset.load(Ordering::SeqCst);
        let url = self.method_url("getUpdates");

        let req_body = serde_json::json!({
            "offset": if offset > 0 { offset + 1 } else { 0 },
            "limit": 100,
            "timeout": POLL_TIMEOUT_SECS,
        });

        let resp = self
            .client
            .post(&url)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| format!("getUpdates request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            self.increment_failures();
            return Err(format!("getUpdates HTTP error: {status}"));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("getUpdates parse error: {e}"))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let desc = body
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            self.increment_failures();
            return Err(format!("Telegram API error: {desc}"));
        }

        let updates = body
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut events = Vec::with_capacity(updates.len());
        for update in updates {
            let update_id = update.get("update_id").and_then(|v| v.as_u64()).unwrap_or(0);

            // Track highest update_id for next poll
            if update_id >= self.offset.load(Ordering::SeqCst) {
                self.offset.store(update_id, Ordering::SeqCst);
            }

            // Extract message (may be in "message" or "edited_message")
            let message = update
                .get("message")
                .or_else(|| update.get("edited_message"))
                .or_else(|| update.get("channel_post"))
                .or_else(|| update.get("edited_channel_post"));

            let Some(msg) = message else {
                continue;
            };

            let msg_id = msg.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let dedup_key = format!("{update_id}:{msg_id}");
            if self.dedup.is_duplicate(&dedup_key) {
                continue;
            }
            self.dedup.insert(dedup_key);

            if let Some(event) = self.parse_message(msg, update_id).await {
                events.push(event);
            }
        }

        self.reset_failures();
        Ok(events)
    }

    // ── Webhook ─────────────────────────────────────────────────────────────

    /// Run the webhook HTTP server for Telegram updates.
    pub async fn run_webhook(
        self: Arc<Self>,
        on_message: impl Fn(TelegramMessageEvent) + Send + Sync + 'static,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let path = self.config.webhook_path.clone();
        let state = TelegramWebhookState {
            adapter: self.clone(),
            on_message: Arc::new(tokio::sync::Mutex::new(Some(Arc::new(on_message)))),
        };

        let app = Router::new()
            .route(&path, post(handle_telegram_webhook))
            .with_state(Arc::new(state));

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", self.config.webhook_port))
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        info!(
            "Telegram webhook listening on 0.0.0.0:{}{}",
            self.config.webhook_port, self.config.webhook_path
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("Telegram webhook shutting down");
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    }

    // ── Inbound message parsing ─────────────────────────────────────────────

    async fn parse_message(
        &self,
        msg: &serde_json::Value,
        update_id: u64,
    ) -> Option<TelegramMessageEvent> {
        let chat = msg.get("chat")?;
        let chat_id = chat
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string())?;

        let message_thread_id = msg.get("message_thread_id").and_then(|v| v.as_i64());

        let from = msg.get("from");
        let sender_id = from
            .and_then(|f| f.get("id").and_then(|v| v.as_i64()))
            .map(|id| id.to_string());
        let sender_name = from.and_then(|f| {
            f.get("username")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    let first = f.get("first_name").and_then(|v| v.as_str()).unwrap_or("");
                    let last = f.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
                    let name = format!("{first} {last}").trim().to_string();
                    if name.is_empty() { None } else { Some(name) }
                })
        });

        let mut content = String::new();
        let mut msg_type = "text".to_string();
        let mut media = Vec::new();

        // Text message
        if let Some(text) = msg.get("text").and_then(|v| v.as_str()) {
            content.push_str(text);
            msg_type = "text".to_string();
        }

        // Caption (for media messages)
        if let Some(caption) = msg.get("caption").and_then(|v| v.as_str()) {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(caption);
        }

        // Photo
        if let Some(photos) = msg.get("photo").and_then(|v| v.as_array()) {
            if let Some(best) = photos.last() {
                let file_id = best.get("file_id").and_then(|v| v.as_str())?;
                let file_size = best.get("file_size").and_then(|v| v.as_u64());
                media.push(TelegramMedia {
                    media_type: "photo".to_string(),
                    file_id: file_id.to_string(),
                    local_path: None,
                    file_name: None,
                    file_size,
                    caption: msg.get("caption").and_then(|v| v.as_str()).map(String::from),
                });
                msg_type = "photo".to_string();
            }
        }

        // Document
        if let Some(doc) = msg.get("document") {
            let file_id = doc.get("file_id").and_then(|v| v.as_str())?;
            let file_name = doc.get("file_name").and_then(|v| v.as_str()).map(String::from);
            let file_size = doc.get("file_size").and_then(|v| v.as_u64());
            let mime_type = doc.get("mime_type").and_then(|v| v.as_str()).map(String::from);

            // Classify as voice if mime_type starts with audio/
            let media_type = if mime_type
                .as_ref()
                .map(|m| m.starts_with("audio/"))
                .unwrap_or(false)
            {
                "voice".to_string()
            } else {
                "document".to_string()
            };

            media.push(TelegramMedia {
                media_type,
                file_id: file_id.to_string(),
                local_path: None,
                file_name,
                file_size,
                caption: msg.get("caption").and_then(|v| v.as_str()).map(String::from),
            });
            if msg_type == "text" {
                msg_type = "document".to_string();
            }
        }

        // Voice (distinct from document audio)
        if let Some(voice) = msg.get("voice") {
            let file_id = voice.get("file_id").and_then(|v| v.as_str())?;
            let file_size = voice.get("file_size").and_then(|v| v.as_u64());
            let duration = voice.get("duration").and_then(|v| v.as_i64());
            let file_name = duration.map(|d| format!("voice_{d}s.ogg"));

            media.push(TelegramMedia {
                media_type: "voice".to_string(),
                file_id: file_id.to_string(),
                local_path: None,
                file_name,
                file_size,
                caption: msg.get("caption").and_then(|v| v.as_str()).map(String::from),
            });
            msg_type = "voice".to_string();
        }

        // Video
        if let Some(video) = msg.get("video") {
            let file_id = video.get("file_id").and_then(|v| v.as_str())?;
            let file_size = video.get("file_size").and_then(|v| v.as_u64());
            let file_name = video.get("file_name").and_then(|v| v.as_str()).map(String::from);

            media.push(TelegramMedia {
                media_type: "video".to_string(),
                file_id: file_id.to_string(),
                local_path: None,
                file_name,
                file_size,
                caption: msg.get("caption").and_then(|v| v.as_str()).map(String::from),
            });
            if msg_type == "text" {
                msg_type = "video".to_string();
            }
        }

        // Download media and append local paths to content
        for item in &media {
            match self.download_media(item).await {
                Ok(path) => {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[{}: {}]", item.media_type, path));
                }
                Err(e) => {
                    warn!("Failed to download Telegram media {}: {e}", item.file_id);
                }
            }
        }

        if content.is_empty() && media.is_empty() {
            return None; // Unsupported message type
        }

        Some(TelegramMessageEvent {
            update_id,
            chat_id,
            sender_id,
            sender_name,
            content,
            msg_type,
            media,
            message_thread_id,
        })
    }

    // ── Media download ──────────────────────────────────────────────────────

    async fn download_media(&self, media: &TelegramMedia) -> Result<String, String> {
        // 1. Get file path from Telegram
        let url = self.method_url("getFile");
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "file_id": media.file_id }))
            .send()
            .await
            .map_err(|e| format!("getFile request failed: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("getFile parse error: {e}"))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err("getFile API returned error".to_string());
        }

        let file_path = body
            .get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|v| v.as_str())
            .ok_or("getFile missing file_path")?;

        // 2. Download actual file content
        let download_url = format!(
            "https://api.telegram.org/file/bot{}/{file_path}",
            self.config.bot_token
        );
        let bytes = self
            .client
            .get(&download_url)
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("download bytes error: {e}"))?;

        // 3. Cache to disk
        let cache_dir = hermes_core::get_hermes_home().join("telegram").join("media");
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("mkdir failed: {e}"))?;

        let ext = match media.media_type.as_str() {
            "photo" => "jpg",
            "voice" => "ogg",
            "video" => "mp4",
            _ => media
                .file_name
                .as_ref()
                .and_then(|n| n.rsplit('.').next())
                .unwrap_or("bin"),
        };
        let safe_name = media
            .file_name
            .clone()
            .unwrap_or_else(|| format!("{}_{}", media.media_type, media.file_id.replace('/', "_")));
        let filename = format!(
            "{}_{}.{ext}",
            safe_name.replace('/', "_"),
            &media.file_id[..media.file_id.len().min(16)]
        );
        let local_path = cache_dir.join(filename);

        std::fs::write(&local_path, bytes).map_err(|e| format!("write failed: {e}"))?;

        Ok(local_path.to_string_lossy().to_string())
    }

    // ── Outbound sending ────────────────────────────────────────────────────

    /// Send a plain text message.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<(), String> {
        self.send_text_internal(chat_id, text, None, None).await
    }

    pub async fn send_text_with_options(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<i64>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        self.send_text_internal(chat_id, text, thread_id, reply_to)
            .await
    }

    async fn send_text_internal(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<i64>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let chunks = split_message(text);
        for chunk in chunks {
            let mut body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": "MarkdownV2",
                "disable_web_page_preview": self.config.disable_link_previews,
            });

            let mut current_thread_id = thread_id;
            let mut current_reply_to = reply_to;

            match self
                .post_json_with_retry(
                    "sendMessage",
                    &mut body,
                    &mut current_thread_id,
                    &mut current_reply_to,
                )
                .await
            {
                Ok(_) => {}
                Err(e) if e.starts_with("markdown_parse_error:") => {
                    // Fallback to plain text
                    self.send_text_plain(chat_id, &strip_mdv2(&chunk), thread_id, reply_to)
                        .await?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Send as plain text without Markdown parsing.
    async fn send_text_plain(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<i64>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        let chunks = split_message(text);
        for chunk in chunks {
            let mut body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
            });

            let mut current_thread_id = thread_id;
            let mut current_reply_to = reply_to;

            self.post_json_with_retry(
                "sendMessage",
                &mut body,
                &mut current_thread_id,
                &mut current_reply_to,
            )
            .await?;
        }
        Ok(())
    }

    /// Send a photo from a local file path.
    pub async fn send_photo(&self, chat_id: &str, photo_path: &str) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let file_bytes =
            std::fs::read(photo_path).map_err(|e| format!("read photo failed: {e}"))?;

        self.post_multipart_with_retry(
            "sendPhoto",
            chat_id,
            "photo",
            file_bytes,
            "photo.jpg",
            Some("image/jpeg"),
            None,
            None,
        )
        .await?;

        Ok(())
    }

    /// Send a document from a local file path.
    pub async fn send_document(&self, chat_id: &str, doc_path: &str) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let file_bytes =
            std::fs::read(doc_path).map_err(|e| format!("read document failed: {e}"))?;

        let file_name = std::path::Path::new(doc_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document");

        self.post_multipart_with_retry(
            "sendDocument",
            chat_id,
            "document",
            file_bytes,
            file_name,
            None,
            None,
            None,
        )
        .await?;

        Ok(())
    }

    /// Send a voice message or audio file.
    /// `.ogg` / `.opus` files are sent as native voice messages;
    /// everything else is sent as a regular audio file.
    pub async fn send_voice(
        &self,
        chat_id: &str,
        audio_path: &str,
        caption: Option<&str>,
        thread_id: Option<i64>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let file_bytes =
            std::fs::read(audio_path).map_err(|e| format!("read audio failed: {e}"))?;

        let is_voice = audio_path.ends_with(".ogg") || audio_path.ends_with(".opus");
        let file_name = std::path::Path::new(audio_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.ogg");

        let (method, field, mime) = if is_voice {
            ("sendVoice", "voice", Some("audio/ogg"))
        } else {
            ("sendAudio", "audio", None)
        };

        self.post_multipart_with_retry(
            method,
            chat_id,
            field,
            file_bytes,
            file_name,
            mime,
            caption,
            thread_id,
        )
        .await?;

        Ok(())
    }

    /// Send a video file.
    pub async fn send_video(
        &self,
        chat_id: &str,
        video_path: &str,
        caption: Option<&str>,
        thread_id: Option<i64>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let file_bytes =
            std::fs::read(video_path).map_err(|e| format!("read video failed: {e}"))?;

        let file_name = std::path::Path::new(video_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4");

        self.post_multipart_with_retry(
            "sendVideo",
            chat_id,
            "video",
            file_bytes,
            file_name,
            Some("video/mp4"),
            caption,
            thread_id,
        )
        .await?;

        Ok(())
    }

    /// Send an animation (auto-playing GIF).
    pub async fn send_animation(
        &self,
        chat_id: &str,
        animation_path: &str,
        caption: Option<&str>,
        thread_id: Option<i64>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let file_bytes = std::fs::read(animation_path)
            .map_err(|e| format!("read animation failed: {e}"))?;

        let file_name = std::path::Path::new(animation_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("animation.gif");

        self.post_multipart_with_retry(
            "sendAnimation",
            chat_id,
            "animation",
            file_bytes,
            file_name,
            Some("image/gif"),
            caption,
            thread_id,
        )
        .await?;

        Ok(())
    }

    /// Edit a previously sent message.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Telegram bot_token not configured".to_string());
        }

        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id.parse::<i64>().map_err(|_| "invalid message_id")?,
            "text": text,
            "parse_mode": "MarkdownV2",
        });

        let mut thread_id: Option<i64> = None;
        let mut reply_to: Option<i64> = None;

        match self
            .post_json_with_retry("editMessageText", &mut body, &mut thread_id, &mut reply_to)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_lower = e.to_lowercase();
                // "Message is not modified" is a no-op
                if err_lower.contains("not modified") {
                    return Ok(());
                }
                // Message too long — truncate and retry without markdown
                if err_lower.contains("message_too_long") || err_lower.contains("too long") {
                    let truncated = prefix_within_limit(text, MAX_MESSAGE_LENGTH - 20);
                    let mut body2 = serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": message_id.parse::<i64>().map_err(|_| "invalid message_id")?,
                        "text": format!("{truncated}…"),
                    });
                    let _ = self
                        .post_json_with_retry(
                            "editMessageText",
                            &mut body2,
                            &mut thread_id,
                            &mut reply_to,
                        )
                        .await;
                    return Ok(());
                }
                // Fallback: retry without markdown
                if err_lower.contains("can't parse") || err_lower.contains("parse mode") {
                    let mut body2 = serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": message_id.parse::<i64>().map_err(|_| "invalid message_id")?,
                        "text": text,
                    });
                    return self
                        .post_json_with_retry(
                            "editMessageText",
                            &mut body2,
                            &mut thread_id,
                            &mut reply_to,
                        )
                        .await
                        .map(|_| ())
                        .or_else(|e2| {
                            if e2.to_lowercase().contains("not modified") {
                                Ok(())
                            } else {
                                Err(e2)
                            }
                        });
                }
                Err(e)
            }
        }
    }

    /// Send typing action to indicate the bot is processing.
    pub async fn send_typing(&self, chat_id: &str) -> Result<(), String> {
        let url = self.method_url("sendChatAction");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });

        let _resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("sendChatAction failed: {e}"))?;
        Ok(())
    }
}

// ── Webhook internals ───────────────────────────────────────────────────────

struct TelegramWebhookState {
    adapter: Arc<TelegramAdapter>,
    on_message:
        Arc<tokio::sync::Mutex<Option<Arc<dyn Fn(TelegramMessageEvent) + Send + Sync>>>>,
}

async fn handle_telegram_webhook(
    State(state): State<Arc<TelegramWebhookState>>,
    headers: axum::http::HeaderMap,
    axum::Json(update): axum::Json<serde_json::Value>,
) -> StatusCode {
    // Verify secret token if configured
    if let Some(ref secret) = state.adapter.config.webhook_secret {
        let header_token = headers
            .get("X-Telegram-Bot-Api-Secret-Token")
            .and_then(|v| v.to_str().ok());
        if header_token != Some(secret) {
            tracing::warn!("Telegram webhook rejected: invalid secret token");
            return StatusCode::UNAUTHORIZED;
        }
    }

    let update_id = update.get("update_id").and_then(|v| v.as_u64()).unwrap_or(0);

    let message = update
        .get("message")
        .or_else(|| update.get("edited_message"))
        .or_else(|| update.get("channel_post"))
        .or_else(|| update.get("edited_channel_post"));

    let Some(msg) = message else {
        return StatusCode::OK; // Acknowledge non-message updates
    };

    if let Some(event) = state.adapter.parse_message(msg, update_id).await {
        let guard = state.on_message.lock().await;
        if let Some(ref cb) = *guard {
            cb(event);
        }
    }

    StatusCode::OK
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a long message into chunks that fit within Telegram's 4096 limit.
/// Tries to split on newlines first, then on spaces, then hard breaks.
fn split_message(text: &str) -> Vec<String> {
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

        // Try to find a good split point
        let mut split_pos = MAX_MESSAGE_LENGTH;
        let char_indices: Vec<(usize, char)> = remaining.char_indices().collect();

        // Look for newline before threshold
        for i in (0..char_indices.len().min(MAX_MESSAGE_LENGTH)).rev() {
            if char_indices[i].1 == '\n' {
                split_pos = i + 1;
                break;
            }
        }

        // If no newline, look for space
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

/// Characters that must be escaped in Telegram MarkdownV2.
const MDV2_ESCAPE_CHARS: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Escape text for Telegram MarkdownV2 parse mode.
pub fn escape_mdv2(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if MDV2_ESCAPE_CHARS.contains(&ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Strip MarkdownV2 escape characters to produce clean plain text.
pub fn strip_mdv2(text: &str) -> String {
    // Simple unescape: remove backslash before known special chars
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if MDV2_ESCAPE_CHARS.contains(&next) {
                    result.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        result.push(ch);
    }
    result
}

/// Truncate text to fit within a character limit (Unicode-safe).
fn prefix_within_limit(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_mdv2() {
        assert_eq!(escape_mdv2("hello_world"), "hello\\_world");
        assert_eq!(escape_mdv2("*bold*"), "\\*bold\\*");
        assert_eq!(escape_mdv2("[link](url)"), "\\[link\\]\\(url\\)");
    }

    #[test]
    fn test_strip_mdv2() {
        assert_eq!(strip_mdv2("hello\\_world"), "hello_world");
        assert_eq!(strip_mdv2("\\*bold\\*"), "*bold*");
    }

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("Hello world");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_message_long() {
        let long = "a".repeat(5000);
        let chunks = split_message(&long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn test_telegram_config_from_env() {
        // This just tests Default without env vars
        let cfg = TelegramConfig::default();
        assert_eq!(cfg.bot_token, std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default());
    }

    #[test]
    fn test_prefix_within_limit() {
        assert_eq!(prefix_within_limit("hello", 10), "hello");
        assert_eq!(prefix_within_limit("hello world", 5), "hello");
    }

    #[test]
    fn test_parse_retry_after() {
        assert_eq!(
            TelegramAdapter::parse_retry_after("Too Many Requests: retry after 5"),
            Some(5.0)
        );
        assert_eq!(
            TelegramAdapter::parse_retry_after("Flood control exceeded. Retry after 10"),
            Some(10.0)
        );
        assert_eq!(TelegramAdapter::parse_retry_after("some error"), None);
    }
}
