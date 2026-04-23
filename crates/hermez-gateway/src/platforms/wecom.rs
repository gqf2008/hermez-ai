//! WeCom (企业微信) platform adapter.
//!
//! Mirrors the Python `gateway/platforms/wecom.py`.
//!
//! Supports:
//! - WebSocket long connection to openws.work.weixin.qq.com
//! - Direct-message and group text receive/send
//! - Message deduplication
//! - Auto-reconnect with exponential backoff
//! - Application-level heartbeat
//! - Request/response correlation via req_id
//!
//! The adapter connects to WeCom's WebSocket endpoint, authenticates
//! with bot_id + secret, and receives messages as JSON frames.

use base64::{engine::general_purpose, Engine as _};
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tracing::{debug, error, info, warn};

use crate::dedup::MessageDeduplicator;

/// Type alias for pending request/response correlation.
type PendingResponses = Arc<Mutex<std::collections::HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>>>;

use crate::utils::truncate_text;

/// WeCom platform configuration.
#[derive(Debug, Clone)]
pub struct WeComConfig {
    pub bot_id: String,
    pub secret: String,
    pub websocket_url: String,
}

impl Default for WeComConfig {
    fn default() -> Self {
        Self {
            bot_id: std::env::var("WECOM_BOT_ID").unwrap_or_default(),
            secret: std::env::var("WECOM_SECRET").unwrap_or_default(),
            websocket_url: std::env::var("WECOM_WEBSOCKET_URL")
                .ok()
                .unwrap_or_else(|| "wss://openws.work.weixin.qq.com".to_string()),
        }
    }
}

impl WeComConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Inbound message event from WeCom.
#[derive(Debug, Clone)]
pub struct WeComMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Chat/session ID.
    pub chat_id: String,
    /// Sender user ID.
    pub sender_id: String,
    /// Message content (text).
    pub content: String,
    /// Message type: text, image, file, etc.
    pub msg_type: String,
    /// Whether this is a group message.
    pub is_group: bool,
    /// The original req_id from the callback, for reply correlation.
    pub req_id: String,
    /// Quoted text if this is a reply to another message.
    pub reply_to_text: Option<String>,
    /// Message ID of the quoted message.
    pub reply_to_message_id: Option<String>,
    /// Cached local paths for inbound media (images, files).
    pub media_paths: Vec<String>,
}

/// Internal command to send via WebSocket.
#[derive(Debug)]
enum WsCommand {
    /// Send a proactive message to a chat.
    SendText { chat_id: String, text: String, reply_tx: oneshot::Sender<Result<String, String>> },
    /// Reply to a specific inbound callback.
    RespondText { req_id: String, text: String, reply_tx: oneshot::Sender<Result<String, String>> },
    /// Send a generic request and await the correlated response.
    Request {
        cmd: String,
        body: serde_json::Value,
        reply_tx: oneshot::Sender<Result<serde_json::Value, String>>,
    },
}

/// Shared state for the WebSocket connection.
#[allow(dead_code)]
struct WsState {
    /// Command channel sender.
    cmd_tx: mpsc::Sender<WsCommand>,
    /// Running flag.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Inbound event channel.
    event_tx: mpsc::Sender<WeComMessageEvent>,
    /// Reply_req_id mapping: message_id -> req_id (for aibot_respond_msg).
    reply_req_ids: Arc<parking_lot::Mutex<std::collections::HashMap<String, String>>>,
    /// Pending request/response correlation: req_id -> oneshot sender.
    pending_responses: PendingResponses,
}

/// Delay before flushing a text batch (seconds).
/// Mirrors Python `HERMEZ_WECOM_TEXT_BATCH_DELAY_SECONDS`.
const TEXT_BATCH_DELAY_MS: u64 = 600;

/// WeCom platform adapter.
pub struct WeComAdapter {
    config: WeComConfig,
    client: Client,
    dedup: MessageDeduplicator,
    /// WebSocket state, set when connected.
    ws_state: Mutex<Option<Arc<WsState>>>,
    /// Counter for generating unique req_ids.
    seq: AtomicUsize,
    /// Semaphore to limit concurrent event handler tasks.
    handler_semaphore: Arc<Semaphore>,
    /// Pending text batches for auto-merging rapid successive messages.
    /// Mirrors Python `_pending_text_batches`.
    text_batches: Arc<tokio::sync::Mutex<std::collections::HashMap<String, WeComMessageEvent>>>,
    /// Abort handles for batch flush timers.
    /// Mirrors Python `_pending_text_batch_tasks`.
    batch_timers: Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>>,
}

impl WeComAdapter {
    pub fn new(config: WeComConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: MessageDeduplicator::new(),
            config,
            ws_state: Mutex::new(None),
            seq: AtomicUsize::new(0),
            handler_semaphore: Arc::new(Semaphore::new(100)),
            text_batches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            batch_timers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Generate a unique req_id with the given prefix.
    fn gen_req_id(&self, prefix: &str) -> String {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{seq:08x}-{}", uuid::Uuid::new_v4().simple())
    }

    /// Send a text message to a WeCom chat.
    ///
    /// If the adapter is connected via WebSocket, uses `aibot_send_msg`.
    /// Falls back to HTTP API if WebSocket is not available.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, String> {
        // Try WebSocket first
        if let Some(state) = self.ws_state.lock().await.clone() {
            let (reply_tx, reply_rx) = oneshot::channel();
            state
                .cmd_tx
                .send(WsCommand::SendText {
                    chat_id: chat_id.to_string(),
                    text: text.to_string(),
                    reply_tx,
                })
                .await
                .map_err(|_| "WebSocket command channel closed".to_string())?;

            return reply_rx
                .await
                .map_err(|_| "WebSocket response channel closed".to_string())?;
        }

        // Fallback: HTTP API
        self.send_text_http(chat_id, text).await
    }

    /// Send text via WeCom HTTP API (fallback when WebSocket not connected).
    async fn send_text_http(&self, chat_id: &str, text: &str) -> Result<String, String> {
        let token = self.get_access_token().await?;

        let is_dm = chat_id.starts_with("dm:");
        let user_or_chat_id = if is_dm {
            chat_id.strip_prefix("dm:").unwrap_or(chat_id)
        } else {
            chat_id
        };

        if is_dm {
            let Some(agent_id) = self.get_agent_id() else {
                return Err("WECOM_AGENT_ID not configured or invalid".to_string());
            };
            let resp = self
                .client
                .post(format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
                ))
                .json(&serde_json::json!({
                    "touser": user_or_chat_id,
                    "msgtype": "text",
                    "agentid": agent_id,
                    "text": {
                        "content": text,
                    },
                }))
                .send()
                .await
                .map_err(|e| format!("Failed to send message: {e}"))?;

            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse send response: {e}"))?;

            let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                return Err(format!(
                    "WeCom send failed: errcode={errcode}, errmsg={}",
                    body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }

            debug!("WeCom message sent to {chat_id} via HTTP");
            Ok("ok".to_string())
        } else {
            let resp = self
                .client
                .post(format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/appchat/send?access_token={token}"
                ))
                .json(&serde_json::json!({
                    "chatid": user_or_chat_id,
                    "msgtype": "text",
                    "text": {
                        "content": text,
                    },
                }))
                .send()
                .await
                .map_err(|e| format!("Failed to send group message: {e}"))?;

            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse send response: {e}"))?;

            let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                return Err(format!(
                    "WeCom group send failed: errcode={errcode}, errmsg={}",
                    body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }

            debug!("WeCom group message sent to {chat_id} via HTTP");
            Ok("ok".to_string())
        }
    }

    /// Get the agent_id from config or env.
    fn get_agent_id(&self) -> Option<i64> {
        let id = std::env::var("WECOM_AGENT_ID")
            .ok()
            .and_then(|v| v.parse().ok())?;
        if id == 0 { None } else { Some(id) }
    }

    /// Get/refresh the WeCom access token (HTTP fallback).
    async fn get_access_token(&self) -> Result<String, String> {
        // Use POST with JSON body to avoid leaking credentials in URL query strings
        let resp = self
            .client
            .post("https://qyapi.weixin.qq.com/cgi-bin/gettoken")
            .json(&serde_json::json!({
                "corpid": &self.config.bot_id,
                "corpsecret": &self.config.secret,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to get access token: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token response: {e}"))?;

        let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom token failed: errcode={errcode}, errmsg={}",
                body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        body.get("access_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "Missing access_token in response".to_string())
    }

    /// Process an inbound WebSocket message event.
    pub async fn handle_inbound(&self, event: &serde_json::Value) -> Option<WeComMessageEvent> {
        let body = event.get("body").unwrap_or(event);

        let msg_id = body
            .get("msgid")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let req_id = event
            .get("headers")
            .and_then(|h| h.get("req_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !msg_id.is_empty() && self.dedup.is_duplicate(msg_id) {
            debug!("WeCom dedup: skipping {msg_id}");
            return None;
        }

        let (text, reply_to_text) = Self::extract_text(body);
        if text.is_empty() && reply_to_text.is_none() {
            // Allow media-only messages (text empty but may have media)
        }

        let chat_type = body
            .get("chattype")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let chat_id_raw = body
            .get("chatid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Build chat_id with dm: or group: prefix for routing
        let chat_id = if chat_type == "group" || chat_type == "2" {
            if chat_id_raw.is_empty() {
                let sender = body
                    .get("from")
                    .and_then(|f| f.get("userid"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!("group:{sender}")
            } else {
                format!("group:{chat_id_raw}")
            }
        } else {
            let sender = body
                .get("from")
                .and_then(|f| f.get("userid"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("dm:{sender}")
        };

        let sender_id = body
            .get("from")
            .and_then(|f| f.get("userid"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let msg_type = body
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();

        let is_group = chat_type == "group" || chat_type == "2";

        // Extract and cache media references
        let refs = Self::extract_media_refs(body);
        let mut media_paths = Vec::new();
        for (kind, media) in refs {
            if let Some((path, _ctype)) = self.cache_media(&kind, &media).await {
                media_paths.push(path);
            }
        }

        // Build final content: text + media paths
        let mut content = text;
        if !media_paths.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&media_paths.iter().map(|p| format!("[media: {p}]")).collect::<Vec<_>>().join("\n"));
        }

        let reply_to_message_id = body
            .get("quote")
            .and_then(|q| q.get("original"))
            .and_then(|o| o.get("msgid"))
            .and_then(|v| v.as_str())
            .map(String::from);

        if !msg_id.is_empty() {
            self.dedup.insert(msg_id.to_string());
        }

        Some(WeComMessageEvent {
            message_id: msg_id.to_string(),
            chat_id,
            sender_id,
            content,
            msg_type,
            is_group,
            req_id,
            reply_to_text,
            reply_to_message_id,
            media_paths,
        })
    }

    /// Extract text and quoted reply text from inbound event body.
    ///
    /// Returns `(text, reply_text)` where `reply_text` is the original quoted message text.
    /// Handles text, mixed (text + images), voice, appmsg, and quoted messages.
    fn extract_text(body: &serde_json::Value) -> (String, Option<String>) {
        let msgtype = body
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();

        let mut text_parts: Vec<String> = Vec::new();

        if msgtype == "mixed" {
            // Mixed messages: array of msg_item under mixed.msg_item
            let items = body
                .get("mixed")
                .and_then(|m| m.get("msg_item"))
                .and_then(|v| v.as_array())
                .or_else(|| body.get("mixed").and_then(|v| v.as_array()));
            if let Some(arr) = items {
                for item in arr {
                    if item.get("msgtype").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(content) = item
                            .get("text")
                            .and_then(|t| t.get("content"))
                            .and_then(|v| v.as_str())
                        {
                            let s = content.trim();
                            if !s.is_empty() {
                                text_parts.push(s.to_string());
                            }
                        }
                    }
                }
            }
        } else {
            // Normal text
            if let Some(content) = body
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(|v| v.as_str())
            {
                let s = content.trim();
                if !s.is_empty() {
                    text_parts.push(s.to_string());
                }
            }

            // Voice text
            if msgtype == "voice" {
                if let Some(content) = body
                    .get("voice")
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.as_str())
                {
                    let s = content.trim();
                    if !s.is_empty() {
                        text_parts.push(s.to_string());
                    }
                }
            }

            // Appmsg title (filename for attachments)
            if msgtype == "appmsg" {
                if let Some(title) = body
                    .get("appmsg")
                    .and_then(|a| a.get("title"))
                    .and_then(|v| v.as_str())
                {
                    let s = title.trim();
                    if !s.is_empty() {
                        text_parts.push(s.to_string());
                    }
                }
            }
        }

        let text = text_parts.join("\n");

        // Extract quoted reply text
        let mut reply_text: Option<String> = None;
        if let Some(quote) = body.get("quote").and_then(|v| v.as_object()) {
            let quote_type = quote
                .get("msgtype")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if quote_type == "text" {
                reply_text = quote
                    .get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            } else if quote_type == "voice" {
                reply_text = quote
                    .get("voice")
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
        }

        (text, reply_text)
    }

    /// Extract media references from an inbound event body.
    ///
    /// Returns a list of (kind, media_ref) tuples for images and files.
    fn extract_media_refs(body: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
        let mut refs: Vec<(String, serde_json::Value)> = Vec::new();
        let msgtype = body
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();

        if msgtype == "mixed" {
            let items = body
                .get("mixed")
                .and_then(|m| m.get("msg_item"))
                .and_then(|v| v.as_array())
                .or_else(|| body.get("mixed").and_then(|v| v.as_array()));
            if let Some(arr) = items {
                for item in arr {
                    let item_type = item
                        .get("msgtype")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if item_type == "image" && item.get("image").is_some() {
                        refs.push(("image".to_string(), item["image"].clone()));
                    }
                }
            }
        } else {
            if msgtype == "image" && body.get("image").is_some() {
                refs.push(("image".to_string(), body["image"].clone()));
            }
            if msgtype == "file" && body.get("file").is_some() {
                refs.push(("file".to_string(), body["file"].clone()));
            }
            if msgtype == "appmsg" && body.get("appmsg").is_some() {
                let appmsg = &body["appmsg"];
                if appmsg.get("file").is_some() {
                    refs.push(("file".to_string(), appmsg["file"].clone()));
                } else if appmsg.get("image").is_some() {
                    refs.push(("image".to_string(), appmsg["image"].clone()));
                }
            }
        }

        // Quote/reply may also contain media
        if let Some(quote) = body.get("quote") {
            let quote_type = quote
                .get("msgtype")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if quote_type == "image" && quote.get("image").is_some() {
                refs.push(("image".to_string(), quote["image"].clone()));
            } else if quote_type == "file" && quote.get("file").is_some() {
                refs.push(("file".to_string(), quote["file"].clone()));
            }
        }

        refs
    }

    /// Cache inbound media to local storage.
    ///
    /// Supports base64 inline data, remote URL download, and AES-256-CBC decryption.
    async fn cache_media(
        &self,
        kind: &str,
        media: &serde_json::Value,
    ) -> Option<(String, String)> {
        // 1) Base64 inline data
        if let Some(b64_str) = media.get("base64").and_then(|v| v.as_str()) {
            let payload = b64_str.split(',').next_back().unwrap_or(b64_str).trim();
            let raw = general_purpose::STANDARD.decode(payload).ok()?;

            if kind == "image" {
                let ext = Self::detect_image_ext(&raw);
                let path = Self::cache_image_from_bytes(&raw, &ext).ok()?;
                let mime = Self::mime_for_ext(&ext);
                return Some((path, mime));
            }

            let filename = media
                .get("filename")
                .or_else(|| media.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("wecom_file")
                .to_string();
            let path = Self::cache_document_from_bytes(&raw, &filename).ok()?;
            let mime = Self::mime_for_ext(
                Path::new(&filename)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(""),
            );
            return Some((path, mime));
        }

        // 2) Remote URL
        let url = media.get("url").and_then(|v| v.as_str())?.trim();
        if url.is_empty() {
            return None;
        }

        let resp = self.client.get(url).send().await.ok()?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_string();
        let mut raw = resp.bytes().await.ok()?.to_vec();

        // 3) AES-256-CBC decryption
        if let Some(aes_key) = media.get("aeskey").and_then(|v| v.as_str()) {
            let aes_key = aes_key.trim();
            if !aes_key.is_empty() {
                raw = Self::decrypt_aes_256_cbc(&raw, aes_key).ok()?;
            }
        }

        if kind == "image" {
            let ext = Self::guess_extension(url, &content_type, &Self::detect_image_ext(&raw));
            let path = Self::cache_image_from_bytes(&raw, &ext).ok()?;
            let mime = Self::mime_for_ext(&ext);
            return Some((path, mime));
        }

        let filename = Self::guess_filename(url, &content_type);
        let path = Self::cache_document_from_bytes(&raw, &filename).ok()?;
        Some((path, content_type))
    }

    /// Decrypt bytes using AES-256-CBC with PKCS#7 padding.
    fn decrypt_aes_256_cbc(encrypted: &[u8], aes_key_b64: &str) -> Result<Vec<u8>, String> {
        use aes::Aes256;
        use cbc::cipher::{BlockDecryptMut, KeyIvInit};

        let key = general_purpose::STANDARD
            .decode(aes_key_b64)
            .map_err(|e| format!("Invalid base64 aes_key: {e}"))?;
        if key.len() != 32 {
            return Err(format!(
                "Invalid WeCom AES key length: expected 32, got {}",
                key.len()
            ));
        }
        if encrypted.is_empty() {
            return Err("encrypted_data is empty".to_string());
        }

        type Aes256CbcDec = cbc::Decryptor<Aes256>;
        let iv: [u8; 16] = key[..16].try_into().map_err(|_| "IV must be 16 bytes")?;
        let dec =
            Aes256CbcDec::new_from_slices(&key, &iv).map_err(|e| format!("Invalid key/IV: {e}"))?;
        let mut buf = encrypted.to_vec();
        let pt = dec
            .decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf)
            .map_err(|e| format!("Decryption failed: {e}"))?;
        Ok(pt.to_vec())
    }

    /// Detect image extension from magic bytes.
    fn detect_image_ext(data: &[u8]) -> String {
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return ".png".to_string();
        }
        if data.starts_with(b"\xff\xd8\xff") {
            return ".jpg".to_string();
        }
        if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
            return ".gif".to_string();
        }
        if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
            return ".webp".to_string();
        }
        ".jpg".to_string()
    }

    /// Guess MIME type from extension.
    fn mime_for_ext(ext: &str) -> String {
        match ext.to_lowercase().as_str() {
            ".png" => "image/png".to_string(),
            ".jpg" | ".jpeg" => "image/jpeg".to_string(),
            ".gif" => "image/gif".to_string(),
            ".webp" => "image/webp".to_string(),
            ".pdf" => "application/pdf".to_string(),
            ".md" => "text/markdown".to_string(),
            ".txt" => "text/plain".to_string(),
            ".zip" => "application/zip".to_string(),
            ".docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string(),
            ".xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
            ".pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string(),
            _ => "application/octet-stream".to_string(),
        }
    }

    /// Guess file extension from URL or content-type.
    fn guess_extension(url: &str, content_type: &str, fallback: &str) -> String {
        let ct_ext = match content_type {
            "image/png" => ".png",
            "image/jpeg" => ".jpg",
            "image/gif" => ".gif",
            "image/webp" => ".webp",
            "application/pdf" => ".pdf",
            "text/markdown" => ".md",
            "text/plain" => ".txt",
            _ => "",
        };
        if !ct_ext.is_empty() {
            return ct_ext.to_string();
        }
        if let Some(path_ext) = Path::new(url)
            .extension()
            .and_then(|e| e.to_str())
        {
            return format!(".{path_ext}");
        }
        fallback.to_string()
    }

    /// Guess filename from URL or content-type.
    fn guess_filename(url: &str, content_type: &str) -> String {
        let path = Path::new(url);
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() && !name.contains('?') {
                return name.to_string();
            }
        }
        let ext = Self::guess_extension(url, content_type, ".bin");
        format!("wecom_download{ext}")
    }

    /// Save image bytes to cache directory.
    fn cache_image_from_bytes(data: &[u8], ext: &str) -> Result<String, String> {
        if !Self::looks_like_image(data) {
            let snippet = String::from_utf8_lossy(&data[..data.len().min(80)]);
            return Err(format!(
                "Refusing to cache non-image data as {ext} (starts with: {snippet:?})"
            ));
        }
        let cache_dir = hermez_core::get_hermez_home().join("cache").join("images");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create image cache dir: {e}"))?;
        let name = format!("img_{}{}", uuid::Uuid::new_v4().simple(), ext);
        let path = cache_dir.join(&name);
        std::fs::write(&path, data)
            .map_err(|e| format!("Failed to write image cache: {e}"))?;
        Ok(path.to_string_lossy().to_string())
    }

    /// Save document bytes to cache directory.
    fn cache_document_from_bytes(data: &[u8], filename: &str) -> Result<String, String> {
        let safe_name = Path::new(filename)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document")
            .replace('\x00', "")
            .trim()
            .to_string();
        if safe_name.is_empty() {
            return Self::cache_image_from_bytes(data, ".bin");
        }
        let cache_dir = hermez_core::get_hermez_home().join("cache").join("documents");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("Failed to create document cache dir: {e}"))?;
        let name = format!("doc_{}_{}", uuid::Uuid::new_v4().simple(), safe_name);
        let path = cache_dir.join(&name);
        std::fs::write(&path, data)
            .map_err(|e| format!("Failed to write document cache: {e}"))?;
        Ok(path.to_string_lossy().to_string())
    }

    /// Check if data starts with known image magic bytes.
    fn looks_like_image(data: &[u8]) -> bool {
        if data.len() < 4 {
            return false;
        }
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return true;
        }
        if data.starts_with(b"\xff\xd8\xff") {
            return true;
        }
        if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
            return true;
        }
        if data.starts_with(b"BM") {
            return true;
        }
        if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
            return true;
        }
        false
    }

    /// Check if the adapter is properly configured.
    pub fn is_configured(&self) -> bool {
        !self.config.bot_id.is_empty() && !self.config.secret.is_empty()
    }

    // --- Rich media send methods (mirrors Python wecom.py:830-1050) ---

    /// Send an image via WeCom HTTP API.
    #[allow(dead_code)]
    pub async fn send_image(&self, chat_id: &str, media_id: &str) -> Result<String, String> {
        self.send_media_message(chat_id, "image", media_id).await
    }

    /// Send a voice/audio via WeCom HTTP API.
    #[allow(dead_code)]
    pub async fn send_voice(&self, chat_id: &str, media_id: &str) -> Result<String, String> {
        self.send_media_message(chat_id, "voice", media_id).await
    }

    /// Send a document/file via WeCom HTTP API.
    #[allow(dead_code)]
    pub async fn send_document(&self, chat_id: &str, media_id: &str) -> Result<String, String> {
        self.send_media_message(chat_id, "file", media_id).await
    }

    /// Send a video via WeCom HTTP API.
    #[allow(dead_code)]
    pub async fn send_video(&self, chat_id: &str, media_id: &str) -> Result<String, String> {
        self.send_media_message(chat_id, "video", media_id).await
    }

    /// Generic media message send (image/voice/file/video).
    async fn send_media_message(
        &self,
        chat_id: &str,
        msg_type: &str,
        media_id: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;
        let is_dm = chat_id.starts_with("dm:");
        let user_or_chat_id = if is_dm {
            chat_id.strip_prefix("dm:").unwrap_or(chat_id)
        } else {
            chat_id
        };

        let media_key = match msg_type {
            "image" => "media_id",
            "voice" => "media_id",
            "file" => "media_id",
            "video" => "media_id",
            _ => "media_id",
        };

        if is_dm {
            let Some(agent_id) = self.get_agent_id() else {
                return Err("WECOM_AGENT_ID not configured or invalid".to_string());
            };
            let resp = self
                .client
                .post(format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token={token}"
                ))
                .json(&serde_json::json!({
                    "touser": user_or_chat_id,
                    "msgtype": msg_type,
                    "agentid": agent_id,
                    msg_type: {
                        media_key: media_id,
                    },
                }))
                .send()
                .await
                .map_err(|e| format!("Failed to send {msg_type}: {e}"))?;

            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse response: {e}"))?;

            let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                return Err(format!(
                    "WeCom {msg_type} send failed: errcode={errcode}, errmsg={}",
                    body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }
        } else {
            let resp = self
                .client
                .post(format!(
                    "https://qyapi.weixin.qq.com/cgi-bin/appchat/send?access_token={token}"
                ))
                .json(&serde_json::json!({
                    "chatid": user_or_chat_id,
                    "msgtype": msg_type,
                    msg_type: {
                        media_key: media_id,
                    },
                }))
                .send()
                .await
                .map_err(|e| format!("Failed to send group {msg_type}: {e}"))?;

            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse response: {e}"))?;

            let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                return Err(format!(
                    "WeCom group {msg_type} send failed: errcode={errcode}, errmsg={}",
                    body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }
        }

        debug!("WeCom {msg_type} sent to {chat_id}");
        Ok("ok".to_string())
    }

    // --- Chunked media upload (mirrors Python _upload_media_bytes) ---

    /// Chunk size for media upload: 512KB.
    const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;
    /// Maximum number of chunks (~50MB total).
    const MAX_UPLOAD_CHUNKS: usize = 100;

    /// Upload media bytes via chunked WebSocket protocol.
    ///
    /// Returns the `media_id` assigned by WeCom.
    pub async fn upload_media_chunked(
        &self,
        data: &[u8],
        media_type: &str,
        filename: &str,
    ) -> Result<String, String> {
        if data.is_empty() {
            return Err("Cannot upload empty media".to_string());
        }

        let total_size = data.len();
        let total_chunks = total_size.div_ceil(Self::UPLOAD_CHUNK_SIZE);
        if total_chunks > Self::MAX_UPLOAD_CHUNKS {
            return Err(format!(
                "File too large: {total_chunks} chunks exceeds maximum of {}",
                Self::MAX_UPLOAD_CHUNKS
            ));
        }

        let ws_state = self.ws_state.lock().await.clone()
            .ok_or("WebSocket not connected".to_string())?;

        // 1) Init
        let md5_hash = format!("{:x}", md5::compute(data));
        let (reply_tx, reply_rx) = oneshot::channel();
        ws_state.cmd_tx.send(WsCommand::Request {
            cmd: "aibot_upload_media_init".to_string(),
            body: serde_json::json!({
                "type": media_type,
                "filename": filename,
                "total_size": total_size,
                "total_chunks": total_chunks,
                "md5": md5_hash,
            }),
            reply_tx,
        }).await.map_err(|_| "Command channel closed".to_string())?;

        let init_response = tokio::time::timeout(
            Duration::from_secs(15),
            reply_rx
        ).await.map_err(|_| "upload_media_init timeout".to_string())?
            .map_err(|_| "Response channel closed".to_string())?;
        let init_response = match init_response {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

        let init_body = init_response.get("body").unwrap_or(&init_response);
        let errcode = init_body.get("errcode").and_then(|v: &serde_json::Value| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "upload_media_init failed: errcode={errcode}, errmsg={}",
                init_body.get("errmsg").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("")
            ));
        }

        let upload_id = init_body
            .get("upload_id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .ok_or("Missing upload_id in init response")?
            .to_string();

        // 2) Upload chunks
        for chunk_index in 0..total_chunks {
            let start = chunk_index * Self::UPLOAD_CHUNK_SIZE;
            let end = (start + Self::UPLOAD_CHUNK_SIZE).min(total_size);
            let chunk = &data[start..end];
            let b64_data = general_purpose::STANDARD.encode(chunk);

            let (reply_tx, reply_rx) = oneshot::channel();
            ws_state.cmd_tx.send(WsCommand::Request {
                cmd: "aibot_upload_media_chunk".to_string(),
                body: serde_json::json!({
                    "upload_id": &upload_id,
                    "chunk_index": chunk_index,
                    "base64_data": b64_data,
                }),
                reply_tx,
            }).await.map_err(|_| "Command channel closed".to_string())?;

            let chunk_response = tokio::time::timeout(
                Duration::from_secs(15),
                reply_rx
            ).await.map_err(|_| format!("upload_media_chunk {chunk_index} timeout"))?
                .map_err(|_| "Response channel closed".to_string())?;
            let chunk_response = match chunk_response {
                Ok(v) => v,
                Err(e) => return Err(e),
            };

            let chunk_body = chunk_response.get("body").unwrap_or(&chunk_response);
            let errcode = chunk_body.get("errcode").and_then(|v: &serde_json::Value| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                return Err(format!(
                    "upload_media_chunk {chunk_index} failed: errcode={errcode}, errmsg={}",
                    chunk_body.get("errmsg").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("")
                ));
            }
        }

        // 3) Finish
        let (reply_tx, reply_rx) = oneshot::channel();
        ws_state.cmd_tx.send(WsCommand::Request {
            cmd: "aibot_upload_media_finish".to_string(),
            body: serde_json::json!({"upload_id": upload_id}),
            reply_tx,
        }).await.map_err(|_| "Command channel closed".to_string())?;

        let finish_response = tokio::time::timeout(
            Duration::from_secs(15),
            reply_rx
        ).await.map_err(|_| "upload_media_finish timeout".to_string())?
            .map_err(|_| "Response channel closed".to_string())?;
        let finish_response = match finish_response {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

        let finish_body = finish_response.get("body").unwrap_or(&finish_response);
        let errcode = finish_body.get("errcode").and_then(|v: &serde_json::Value| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "upload_media_finish failed: errcode={errcode}, errmsg={}",
                finish_body.get("errmsg").and_then(|v: &serde_json::Value| v.as_str()).unwrap_or("")
            ));
        }

        let media_id = finish_body
            .get("media_id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .ok_or("Missing media_id in finish response")?
            .to_string();

        Ok(media_id)
    }

    /// Flush a text batch (send multiple messages in sequence).
    ///
    /// Mirrors Python `_flush_text_batch()` (wecom.py:972).
    #[allow(dead_code)]
    pub async fn flush_text_batch(
        &self,
        chat_id: &str,
        messages: &[String],
    ) -> Result<Vec<String>, String> {
        let mut msg_ids = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            let msg_id = self.send_text(chat_id, msg).await?;
            msg_ids.push(msg_id);
            // Rate limit: small delay between batch items
            if i < messages.len() - 1 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(msg_ids)
    }

    /// Get chat info via WeCom HTTP API.
    ///
    /// Mirrors Python `get_chat_info()` (wecom.py:1010).
    #[allow(dead_code)]
    pub async fn get_chat_info(&self, chat_id: &str) -> Result<serde_json::Value, String> {
        let token = self.get_access_token().await?;

        let resp = self
            .client
            .get(format!(
                "https://qyapi.weixin.qq.com/cgi-bin/appchat/get?access_token={token}&chatid={chat_id}"
            ))
            .send()
            .await
            .map_err(|e| format!("Failed to get chat info: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;

        let errcode = body.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            return Err(format!(
                "WeCom chat info failed: errcode={errcode}, errmsg={}",
                body.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }

        Ok(body)
    }

    /// Send typing indicator (not natively supported by WeCom).
    ///
    /// Mirrors Python `send_typing()` (wecom.py:1085) — no-op in WeCom.
    #[allow(dead_code)]
    pub async fn send_typing(&self, _chat_id: &str) -> Result<String, String> {
        // WeCom does not support typing indicators
        Ok("not_supported".to_string())
    }

    /// Send a reply via WebSocket, trying respond_msg first then falling back to send_msg.
    async fn send_reply(
        ws_state: &WsState,
        req_id: &str,
        chat_id: &str,
        response: String,
    ) {
        if !req_id.is_empty() {
            let (reply_tx, reply_rx) = oneshot::channel();
            if ws_state
                .cmd_tx
                .send(WsCommand::RespondText {
                    req_id: req_id.to_string(),
                    text: response.clone(),
                    reply_tx,
                })
                .await
                .is_ok()
            {
                if let Ok(Ok(_)) = reply_rx.await {
                    return; // respond_msg succeeded
                }
            }
        }
        // Fallback: proactive send
        let (reply_tx, _) = oneshot::channel();
        let _ = ws_state
            .cmd_tx
            .send(WsCommand::SendText {
                chat_id: chat_id.to_string(),
                text: response,
                reply_tx,
            })
            .await;
    }

    /// Run the WeCom WebSocket connection loop.
    ///
    /// Connects to the WeCom WebSocket endpoint, authenticates with
    /// `aibot_subscribe`, and processes inbound messages forever.
    /// Auto-reconnects with exponential backoff on failure.
    pub async fn run(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
        let mut backoff_idx = 0;

        while running.load(Ordering::SeqCst) {
            match self.connect_and_run(&handler, &running).await {
                Ok(()) => {
                    // Clean disconnect
                    backoff_idx = 0;
                }
                Err(e) => {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    error!("WeCom connection error: {e}");
                    // Clear ws_state on disconnect
                    *self.ws_state.lock().await = None;

                    let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
                    info!("WeCom reconnecting in {delay}s...");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    backoff_idx = (backoff_idx + 1).min(RECONNECT_BACKOFF.len() - 1);
                }
            }
        }

        *self.ws_state.lock().await = None;
        info!("WeCom WebSocket loop stopped");
    }

    /// Connect to WeCom WebSocket and run the message loop.
    /// Returns Ok(()) on clean disconnect, Err on connection failure.
    async fn connect_and_run(
        &self,
        handler: &Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), String> {
        use tokio_tungstenite::{connect_async, tungstenite::http::Uri};

        let ws_url = &self.config.websocket_url;
        let uri: Uri = ws_url
            .parse()
            .map_err(|e| format!("Invalid WebSocket URL: {e}"))?;

        info!("WeCom connecting to {ws_url}...");

        let (ws_stream, _response) = connect_async(uri)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        info!("WeCom WebSocket connected");

        // Authenticate with aibot_subscribe
        let subscribe_req_id = self.gen_req_id("subscribe");
        let subscribe_frame = serde_json::json!({
            "cmd": "aibot_subscribe",
            "headers": {"req_id": &subscribe_req_id},
            "body": {
                "bot_id": &self.config.bot_id,
                "secret": &self.config.secret,
            },
        });

        let (mut write_half, read_half) = ws_stream.split();

        write_half
            .send(Message::Text(Utf8Bytes::from(subscribe_frame.to_string())))
            .await
            .map_err(|e| format!("Subscribe send failed: {e}"))?;

        // Wait for subscribe response
        let subscribed;
        let mut reader = read_half.fuse();

        loop {
            match tokio::time::timeout(
                Duration::from_secs(10),
                reader.next(),
            )
            .await
            {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
                        let frame_req_id = frame
                            .get("headers")
                            .and_then(|h| h.get("req_id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        if frame_req_id == subscribe_req_id {
                            let errcode = frame
                                .get("body")
                                .and_then(|b| b.get("errcode"))
                                .and_then(|v| v.as_i64())
                                .unwrap_or(-1);

                            if errcode == 0 {
                                info!("WeCom subscription confirmed");
                                subscribed = true;
                                break;
                            } else {
                                let errmsg = frame
                                    .get("body")
                                    .and_then(|b| b.get("errmsg"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                return Err(format!("WeCom subscribe failed: errcode={errcode}, errmsg={errmsg}"));
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => return Err(format!("WebSocket read error: {e}")),
                Ok(None) => return Err("WebSocket closed before subscribe".to_string()),
                Err(_) => return Err("Subscribe timeout (10s)".to_string()),
            }
        }

        if !subscribed {
            return Err("Subscription not confirmed".to_string());
        }

        // Set up command channel for outbound sends
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WsCommand>(32);
        let (event_tx, mut event_rx) = mpsc::channel::<WeComMessageEvent>(64);

        let reply_req_ids: Arc<parking_lot::Mutex<std::collections::HashMap<String, String>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let pending_responses: PendingResponses =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let ws_state = Arc::new(WsState {
            cmd_tx: cmd_tx.clone(),
            running: running.clone(),
            event_tx: event_tx.clone(),
            reply_req_ids: reply_req_ids.clone(),
            pending_responses: pending_responses.clone(),
        });

        *self.ws_state.lock().await = Some(ws_state.clone());

        // Unified select! loop: read, send, event — all in one task.
        // This avoids leaking spawned tasks on reconnect.
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Read inbound WebSocket messages
                result = tokio::time::timeout(Duration::from_secs(60), reader.next()) => {
                    match result {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
                                // Dispatch message to event channel
                                self.dispatch_frame(
                                    &frame,
                                    &event_tx,
                                    &reply_req_ids,
                                    &pending_responses,
                                )
                                .await;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            info!("WeCom WebSocket closed by server");
                            return Err("WebSocket closed by server".to_string());
                        }
                        Ok(Some(Ok(Message::Ping(_)))) => {
                            debug!("WeCom ping received");
                        }
                        Ok(Some(Ok(_))) => {
                            // Binary, Pong: ignore
                        }
                        Ok(Some(Err(e))) => {
                            return Err(format!("WebSocket read error: {e}"));
                        }
                        Ok(None) => {
                            return Err("WebSocket stream ended".to_string());
                        }
                        Err(_) => {
                            // 60s read timeout
                            if !running.load(Ordering::SeqCst) {
                                return Ok(());
                            }
                            debug!("WeCom read timeout, reconnecting");
                            return Err("Read timeout".to_string());
                        }
                    }
                }
                // Handle outbound commands
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(WsCommand::SendText { chat_id, text, reply_tx }) => {
                            let req_id = format!("send-{}-{}", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4().simple());
                            let frame = serde_json::json!({
                                "cmd": "aibot_send_msg",
                                "headers": {"req_id": &req_id},
                                "body": {
                                    "chatid": chat_id,
                                    "msgtype": "markdown",
                                    "markdown": {
                                        "content": truncate_text(&text, 4000),
                                    },
                                },
                            });

                            match write_half.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await {
                                Ok(()) => {
                                    debug!("WeCom aibot_send_msg sent");
                                    let _ = reply_tx.send(Ok("ok".to_string()));
                                }
                                Err(e) => {
                                    let _ = reply_tx.send(Err(format!("WebSocket send error: {e}")));
                                }
                            }
                        }
                        Some(WsCommand::RespondText { req_id, text, reply_tx }) => {
                            let stream_id = format!("stream-{}", uuid::Uuid::new_v4().simple());
                            let frame = serde_json::json!({
                                "cmd": "aibot_respond_msg",
                                "headers": {"req_id": &req_id},
                                "body": {
                                    "msgtype": "stream",
                                    "stream": {
                                        "id": stream_id,
                                        "finish": true,
                                        "content": truncate_text(&text, 4000),
                                    },
                                },
                            });

                            match write_half.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await {
                                Ok(()) => {
                                    debug!("WeCom aibot_respond_msg sent");
                                    let _ = reply_tx.send(Ok("ok".to_string()));
                                }
                                Err(e) => {
                                    let _ = reply_tx.send(Err(format!("WebSocket respond error: {e}")));
                                }
                            }
                        }
                        Some(WsCommand::Request { cmd: request_cmd, body: request_body, reply_tx }) => {
                            let req_id = format!("req-{}", uuid::Uuid::new_v4().simple());
                            let frame = serde_json::json!({
                                "cmd": request_cmd,
                                "headers": {"req_id": &req_id},
                                "body": request_body,
                            });

                            // Register pending response before sending
                            let _ = pending_responses.lock().await.insert(req_id.clone(), reply_tx);

                            match write_half.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await {
                                Ok(()) => {
                                    debug!("WeCom request sent: {request_cmd}");
                                }
                                Err(e) => {
                                    if let Some(tx) = pending_responses.lock().await.remove(&req_id) {
                                        let _ = tx.send(Err(format!("WebSocket send error: {e}")));
                                    }
                                }
                            }
                        }
                        None => {
                            info!("WeCom command channel closed");
                            return Err("Command channel closed".to_string());
                        }
                    }
                }
                // Handle inbound events (route to agent handler)
                event = event_rx.recv() => {
                    match event {
                        Some(event) => {
                            if event.content.is_empty() {
                                continue;
                            }

                            info!(
                                "WeCom message from {} via {}: {}",
                                event.sender_id,
                                event.chat_id,
                                event.content.chars().take(50).collect::<String>(),
                            );

                            // Acquire semaphore permit (limits concurrent handlers to 100)
                            let permit = self.handler_semaphore.clone()
                                .try_acquire_owned()
                                .map_err(|_| "Too many concurrent handlers")
                                .ok();

                            if permit.is_none() {
                                warn!("WeCom event rejected: too many concurrent handlers");
                                continue;
                            }

                            // Clone handler to avoid holding lock across await
                            let handler_clone = handler.clone();
                            let event_req_id = event.req_id.clone();
                            let event_chat_id = event.chat_id.clone();
                            let ws_state_clone = ws_state.clone();

                            // Spawn handler task (permit released when task completes)
                            tokio::spawn(async move {
                                // _permit is dropped here, releasing the semaphore
                                let _permit = permit;
                                let handler_guard = handler_clone.lock().await;
                                if let Some(handler) = handler_guard.as_ref() {
                                    match handler
                                        .handle_message(
                                            crate::config::Platform::Wecom,
                                            &event_chat_id,
                                            &event.content,
                                            None,
                                        )
                                        .await
                                    {
                                        Ok(result) => {
                                            if !result.response.is_empty() {
                                                Self::send_reply(
                                                    &ws_state_clone,
                                                    &event_req_id,
                                                    &event_chat_id,
                                                    result.response,
                                                )
                                                .await;
                                            }
                                        }
                                        Err(e) => {
                                            error!("Agent handler failed for WeCom message: {e}");
                                            let (reply_tx, _) = oneshot::channel();
                                            let _ = ws_state_clone
                                                .cmd_tx
                                                .send(WsCommand::SendText {
                                                    chat_id: event_chat_id.clone(),
                                                    text: "Sorry, I encountered an error processing your message.".to_string(),
                                                    reply_tx,
                                                })
                                                .await;
                                        }
                                    }
                                } else {
                                    warn!("No message handler registered for WeCom messages");
                                }
                            });
                        }
                        None => {
                            info!("WeCom event channel closed");
                            return Err("Event channel closed".to_string());
                        }
                    }
                }
                // Application-level heartbeat
                _ = heartbeat_interval.tick() => {
                    let ping_id = format!("ping-{}", uuid::Uuid::new_v4().simple());
                    let frame = serde_json::json!({
                        "cmd": "ping",
                        "headers": {"req_id": &ping_id},
                        "body": {},
                    });
                    if let Err(e) = write_half.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await {
                        warn!("WeCom heartbeat failed: {e}");
                    }
                }
                // Check running flag periodically (200ms for responsive shutdown)
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if !running.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Dispatch an inbound WebSocket frame.
    async fn dispatch_frame(
        &self,
        frame: &serde_json::Value,
        event_tx: &mpsc::Sender<WeComMessageEvent>,
        reply_req_ids: &Arc<parking_lot::Mutex<std::collections::HashMap<String, String>>>,
        pending_responses: &PendingResponses,
    ) {
        // First: check if this frame is a response to a pending request
        let frame_req_id = frame
            .get("headers")
            .and_then(|h| h.get("req_id"))
            .and_then(|v| v.as_str());
        if let Some(req_id) = frame_req_id {
            if let Some(sender) = pending_responses.lock().await.remove(req_id) {
                let _ = sender.send(Ok(frame.clone()));
                return;
            }
        }

        let cmd = frame
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match cmd {
            "aibot_msg_callback" | "aibot_callback" => {
                // Reuse handle_inbound for parsing and dedup
                let Some(event) = self.handle_inbound(frame).await else {
                    return;
                };

                // Store req_id mapping for later replies
                if !event.message_id.is_empty() && !event.req_id.is_empty() {
                    let mut map = reply_req_ids.lock();
                    map.insert(event.message_id.clone(), event.req_id.clone());
                    // Clean up old entries periodically
                    if map.len() > 2048 {
                        let to_remove: Vec<String> =
                            map.keys().take(map.len() - 1024).cloned().collect();
                        for k in to_remove {
                            map.remove(&k);
                        }
                    }
                }

                // Auto-merge rapid successive text messages (batching)
                if event.msg_type == "text" {
                    let chat_id = event.chat_id.clone();
                    let mut batches = self.text_batches.lock().await;
                    let mut timers = self.batch_timers.lock().await;

                    if let Some(existing) = batches.get_mut(&chat_id) {
                        // Merge content
                        existing.content = format!("{}\n{}", existing.content, event.content);
                        // Merge media paths
                        existing.media_paths.extend(event.media_paths);
                        // Merge reply text if present
                        if existing.reply_to_text.is_none() && event.reply_to_text.is_some() {
                            existing.reply_to_text = event.reply_to_text;
                            existing.reply_to_message_id = event.reply_to_message_id;
                        }
                        // Abort old timer
                        if let Some(old) = timers.remove(&chat_id) {
                            old.abort();
                        }
                    } else {
                        batches.insert(chat_id.clone(), event);
                    }

                    // Start new timer
                    let batches_clone = self.text_batches.clone();
                    let timers_clone = self.batch_timers.clone();
                    let event_tx_clone = event_tx.clone();
                    let chat_id_for_timer = chat_id.clone();
                    let handle = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(TEXT_BATCH_DELAY_MS)).await;
                        let mut batches = batches_clone.lock().await;
                        if let Some(ev) = batches.remove(&chat_id_for_timer) {
                            let _ = event_tx_clone.send(ev).await;
                        }
                        let mut timers = timers_clone.lock().await;
                        timers.remove(&chat_id_for_timer);
                    });
                    timers.insert(chat_id, handle.abort_handle());
                } else {
                    // Non-text message: flush any pending batch for this chat first
                    let chat_id = event.chat_id.clone();
                    let mut batches = self.text_batches.lock().await;
                    let mut timers = self.batch_timers.lock().await;
                    if let Some(old) = timers.remove(&chat_id) {
                        old.abort();
                    }
                    if let Some(batch) = batches.remove(&chat_id) {
                        let _ = event_tx.send(batch).await;
                    }
                    let _ = event_tx.send(event).await;
                }
            }
            "aibot_event_callback" => {
                debug!("WeCom event ignored");
            }
            _ => {
                debug!("WeCom unhandled frame: cmd={cmd}");
            }
        }
    }

}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = WeComConfig::default();
        assert_eq!(
            config.websocket_url,
            "wss://openws.work.weixin.qq.com"
        );
    }

    #[test]
    fn test_config_from_env() {
        let config = WeComConfig::from_env();
        assert!(config.websocket_url.starts_with("wss://"));
    }

    #[test]
    fn test_not_configured_when_empty() {
        let config = WeComConfig::default();
        let adapter = WeComAdapter::new(config);
        assert!(!adapter.is_configured());
    }

    #[tokio::test]
    async fn test_extract_text_content() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_1",
                "chattype": "1",
                "from": {"userid": "user123"},
                "msgtype": "text",
                "text": {"content": "hello wecom"},
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.content, "hello wecom");
        assert!(!evt.is_group);
        assert_eq!(evt.chat_id, "dm:user123");
    }

    #[tokio::test]
    async fn test_extract_group_message() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_2",
                "chattype": "group",
                "chatid": "group456",
                "from": {"userid": "user789"},
                "msgtype": "text",
                "text": {"content": "group message"},
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.content, "group message");
        assert!(evt.is_group);
        assert_eq!(evt.chat_id, "group:group456");
    }

    #[tokio::test]
    async fn test_extract_mixed_content() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_3",
                "chattype": "1",
                "from": {"userid": "user1"},
                "msgtype": "mixed",
                "mixed": {
                    "msg_item": [
                        {"msgtype": "text", "text": {"content": "part1"}},
                        {"msgtype": "image"},
                        {"msgtype": "text", "text": {"content": "part2"}}
                    ]
                },
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.content, "part1\npart2");
    }

    #[tokio::test]
    async fn test_extract_voice_content() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_4",
                "chattype": "1",
                "from": {"userid": "user1"},
                "msgtype": "voice",
                "voice": {"content": "voice transcription"},
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.content, "voice transcription");
    }

    #[tokio::test]
    async fn test_extract_appmsg_title() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_5",
                "chattype": "1",
                "from": {"userid": "user1"},
                "msgtype": "appmsg",
                "appmsg": {"title": "Article Title"},
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.content, "Article Title");
    }

    #[tokio::test]
    async fn test_dedup() {
        let adapter = WeComAdapter::new(WeComConfig::default());
        let event = serde_json::json!({
            "body": {
                "msgid": "dup_msg_1",
                "chattype": "1",
                "from": {"userid": "user1"},
                "msgtype": "text",
                "text": {"content": "first"},
            }
        });
        let _ = adapter.handle_inbound(&event).await.unwrap();
        let second = adapter.handle_inbound(&event).await;
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn test_req_id_extracted() {
        let event = serde_json::json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "callback-test-123"},
            "body": {
                "msgid": "wecom_msg_6",
                "chattype": "1",
                "from": {"userid": "user1"},
                "text": {"content": "hello"},
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.req_id, "callback-test-123");
    }

    #[tokio::test]
    async fn test_extract_quote_message() {
        let event = serde_json::json!({
            "body": {
                "msgid": "wecom_msg_7",
                "chattype": "1",
                "from": {"userid": "user1"},
                "msgtype": "text",
                "text": {"content": "hello"},
                "quote": {
                    "msgtype": "text",
                    "text": {"content": "this is a reply"},
                },
            }
        });
        let adapter = WeComAdapter::new(WeComConfig::default());
        let evt = adapter.handle_inbound(&event).await.unwrap();
        assert_eq!(evt.reply_to_text, Some("this is a reply".to_string()));
        assert_eq!(evt.content, "hello");
    }

    #[test]
    fn test_extract_text_plain() {
        let body = serde_json::json!({"text": {"content": "hello"}});
        let (text, _) = WeComAdapter::extract_text(&body);
        assert_eq!(text, "hello");
    }

    #[test]
    fn test_extract_text_voice() {
        let body = serde_json::json!({
            "msgtype": "voice",
            "voice": {"content": "voice text"}
        });
        let (text, _) = WeComAdapter::extract_text(&body);
        assert_eq!(text, "voice text");
    }

    #[test]
    fn test_mime_for_ext() {
        assert_eq!(WeComAdapter::mime_for_ext(".jpg"), "image/jpeg");
        assert_eq!(WeComAdapter::mime_for_ext(".jpeg"), "image/jpeg");
        assert_eq!(WeComAdapter::mime_for_ext(".png"), "image/png");
        assert_eq!(WeComAdapter::mime_for_ext(".pdf"), "application/pdf");
        assert_eq!(WeComAdapter::mime_for_ext(".unknown"), "application/octet-stream");
    }

    #[test]
    fn test_guess_extension() {
        assert_eq!(WeComAdapter::guess_extension("http://x.com/a.jpg", "image/jpeg", ".bin"), ".jpg");
        assert_eq!(WeComAdapter::guess_extension("http://x.com/a", "image/png", ".bin"), ".png");
        assert_eq!(WeComAdapter::guess_extension("http://x.com/a", "application/pdf", ".bin"), ".pdf");
        assert_eq!(WeComAdapter::guess_extension("http://x.com/a.unknown", "application/zip", ".bin"), ".unknown");
    }

    #[test]
    fn test_looks_like_image() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert!(WeComAdapter::looks_like_image(png));
        let jpeg = &[0xFF, 0xD8, 0xFF, 0xE0];
        assert!(WeComAdapter::looks_like_image(jpeg));
        let text = b"hello world";
        assert!(!WeComAdapter::looks_like_image(text));
        assert!(!WeComAdapter::looks_like_image(&[0x89]));
    }

    #[test]
    fn test_detect_image_ext() {
        assert_eq!(WeComAdapter::detect_image_ext(b"\x89PNG\r\n\x1a\n"), ".png");
        assert_eq!(WeComAdapter::detect_image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), ".jpg");
        assert_eq!(WeComAdapter::detect_image_ext(b"GIF89a"), ".gif");
        assert_eq!(WeComAdapter::detect_image_ext(b"unknown"), ".jpg");
    }

    #[test]
    fn test_decrypt_aes_256_cbc_invalid() {
        let result = WeComAdapter::decrypt_aes_256_cbc(b"invalid", "not_base64!!!");
        assert!(result.is_err());
    }
}
