//! Weixin (WeChat) platform adapter.
//!
//! Connects Hermez Agent to WeChat personal accounts via Tencent's iLink Bot API.
//!
//! Design notes:
//! - Long-poll `getupdates` drives inbound delivery.
//! - Every outbound reply must echo the latest `context_token` for the peer.
//! - Media files move through an AES-128-ECB encrypted CDN protocol.
//! - QR login is exposed as a helper for the gateway setup wizard.

use aes::Aes128;
use aes::cipher::{KeyInit, BlockDecrypt, BlockEncrypt, Block};
use parking_lot::Mutex;
use reqwest::Client;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// iLink API base URL.
const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// iLink app ID.
const ILINK_APP_ID: &str = "bot";
/// Channel version.
const CHANNEL_VERSION: &str = "2.2.0";

/// Long poll timeout in milliseconds.
const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
/// API timeout in milliseconds.
const API_TIMEOUT_MS: u64 = 15_000;
/// Message dedup TTL in seconds.
const MESSAGE_DEDUP_TTL_SECONDS: u64 = 300;
/// Max consecutive failures before backoff.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Session expired error code.
const SESSION_EXPIRED_ERRCODE: i64 = -14;

/// Media type constants.
#[allow(dead_code)]
const MEDIA_IMAGE: u32 = 1;
#[allow(dead_code)]
const MEDIA_VIDEO: u32 = 2;
#[allow(dead_code)]
const MEDIA_FILE: u32 = 3;
#[allow(dead_code)]
const MEDIA_VOICE: u32 = 4;

/// CDN upload URL response from iLink API.
#[derive(Debug, Clone)]
struct CdnUploadInfo {
    /// The CDN upload URL.
    url: String,
    /// Auth token for the upload.
    auth_token: String,
    /// Expected SHA-256 hash of the encrypted content.
    #[allow(dead_code)]
    sha256: String,
}

/// Weixin platform configuration.
#[derive(Debug, Clone)]
pub struct WeixinConfig {
    /// iLink session key.
    pub session_key: String,
    /// Encryption key for AES-128-ECB.
    pub encrypt_key: String,
    /// DM policy: `open`, `allowlist`, `disabled`.
    pub dm_policy: String,
    /// Group policy: `open`, `allowlist`, `disabled`.
    pub group_policy: String,
    /// Allowed users for DM allowlist (comma-separated user IDs).
    pub allow_from: Vec<String>,
    /// Allowed groups for group allowlist (comma-separated group IDs).
    pub group_allow_from: Vec<String>,
}

impl WeixinConfig {
    pub fn from_env() -> Self {
        // Try loading persisted account first, then fall back to env vars
        if let Some(account) = WeixinAccount::load() {
            return Self {
                session_key: account.session_key,
                encrypt_key: account.encrypt_key,
                dm_policy: std::env::var("WEIXIN_DM_POLICY").unwrap_or_else(|_| "open".into()),
                group_policy: std::env::var("WEIXIN_GROUP_POLICY").unwrap_or_else(|_| "disabled".into()),
                allow_from: parse_comma_list(&std::env::var("WEIXIN_ALLOWED_USERS").unwrap_or_default()),
                group_allow_from: parse_comma_list(&std::env::var("WEIXIN_GROUP_ALLOWED_USERS").unwrap_or_default()),
            };
        }
        Self {
            session_key: std::env::var("WEIXIN_SESSION_KEY").unwrap_or_default(),
            encrypt_key: std::env::var("WEIXIN_ENCRYPT_KEY").unwrap_or_default(),
            dm_policy: std::env::var("WEIXIN_DM_POLICY").unwrap_or_else(|_| "open".into()),
            group_policy: std::env::var("WEIXIN_GROUP_POLICY").unwrap_or_else(|_| "disabled".into()),
            allow_from: parse_comma_list(&std::env::var("WEIXIN_ALLOWED_USERS").unwrap_or_default()),
            group_allow_from: parse_comma_list(&std::env::var("WEIXIN_GROUP_ALLOWED_USERS").unwrap_or_default()),
        }
    }
}

/// Persisted Weixin account credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WeixinAccount {
    /// iLink session key.
    pub session_key: String,
    /// Encryption key for AES-128-ECB.
    pub encrypt_key: String,
    /// Bot WeChat ID (if known).
    pub bot_wxid: Option<String>,
}

impl WeixinAccount {
    /// Load account from disk at `~/.hermez/weixin/account.json`.
    pub fn load() -> Option<Self> {
        let path = hermez_core::get_hermez_home().join("weixin").join("account.json");
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Save account to disk.
    pub fn save(&self) -> Result<(), String> {
        let dir = hermez_core::get_hermez_home().join("weixin");
        std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create weixin dir: {e}"))?;
        let path = dir.join("account.json");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("Failed to serialize account: {e}"))?;
        std::fs::write(&path, bytes).map_err(|e| format!("Failed to write account: {e}"))?;
        Ok(())
    }
}

/// Persistent store for per-peer context tokens.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ContextTokenStore {
    /// account_session_key -> peer_id -> token
    tokens: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl ContextTokenStore {
    fn path() -> std::path::PathBuf {
        hermez_core::get_hermez_home().join("weixin").join("context_tokens.json")
    }

    fn load() -> Self {
        let path = Self::path();
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(store) = serde_json::from_slice(&bytes) {
                return store;
            }
        }
        Self::default()
    }

    fn get(&self, account: &str, peer: &str) -> Option<String> {
        self.tokens.get(account)?.get(peer).cloned()
    }

    fn set(&mut self, account: &str, peer: &str, token: &str) {
        self.tokens
            .entry(account.to_string())
            .or_default()
            .insert(peer.to_string(), token.to_string());
    }

    fn save(&self) -> std::io::Result<()> {
        let dir = hermez_core::get_hermez_home().join("weixin");
        std::fs::create_dir_all(&dir)?;
        let bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        std::fs::write(Self::path(), bytes)
    }
}

/// Parsed media attachment from an inbound Weixin message.
#[derive(Debug, Clone)]
pub struct MediaItem {
    /// Media type: image, voice, video, file.
    pub media_type: String,
    /// CDN download URL.
    pub media_url: Option<String>,
    /// Local filesystem path after download + decrypt.
    pub local_path: Option<String>,
    /// Original file name (for file messages).
    pub file_name: Option<String>,
    /// File size in bytes.
    pub file_size: Option<u64>,
}

/// Inbound message event from Weixin.
#[derive(Debug, Clone)]
pub struct WeixinMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Peer/session ID.
    pub peer_id: String,
    /// Sender display name (if available).
    pub sender_name: Option<String>,
    /// Message content (text).
    pub content: String,
    /// Message type: text, image, voice, video, file.
    pub msg_type: String,
    /// Whether this is a group message.
    pub is_group: bool,
    /// Parsed media attachments from item_list.
    pub media_items: Vec<MediaItem>,
}

/// Deduplication cache entry.
struct DedupEntry {
    message_id: String,
    timestamp: u64,
}

/// Parse a comma-separated list, filtering empty items.
fn parse_comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Persistent sync buffer cursor for iLink long-poll.
///
/// Mirrors Python `_load_sync_buf` / `_save_sync_buf`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SyncBufStore {
    get_updates_buf: String,
}

impl SyncBufStore {
    fn path() -> PathBuf {
        hermez_core::get_hermez_home().join("weixin").join("sync_buf.json")
    }

    fn load() -> Self {
        let path = Self::path();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(store) = serde_json::from_slice(&bytes) {
                return store;
            }
        }
        Self::default()
    }

    fn save(&self) -> std::io::Result<()> {
        let dir = hermez_core::get_hermez_home().join("weixin");
        std::fs::create_dir_all(&dir)?;
        let bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        std::fs::write(Self::path(), bytes)
    }
}

/// Short-lived typing ticket cache from `getconfig`.
///
/// Mirrors Python `TypingTicketCache` (weixin.py:280).
struct TypingTicketCache {
    ttl_seconds: u64,
    cache: Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

impl TypingTicketCache {
    fn new(ttl_seconds: u64) -> Self {
        Self {
            ttl_seconds,
            cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn get(&self, user_id: &str) -> Option<String> {
        let mut cache = self.cache.lock();
        if let Some((ticket, inserted)) = cache.get(user_id) {
            if inserted.elapsed().as_secs() < self.ttl_seconds {
                return Some(ticket.clone());
            }
            cache.remove(user_id);
        }
        None
    }

    fn set(&self, user_id: &str, ticket: &str) {
        self.cache
            .lock()
            .insert(user_id.to_string(), (ticket.to_string(), std::time::Instant::now()));
    }
}

/// Weixin platform adapter.
pub struct WeixinAdapter {
    config: WeixinConfig,
    client: Client,
    /// Monotonically increasing offset for long-poll (legacy fallback).
    offset: AtomicU64,
    /// Persistent sync buffer cursor for iLink long-poll.
    /// Mirrors Python `sync_buf`.
    sync_buf: RwLock<String>,
    /// Context token that must be echoed on outbound replies.
    context_token: RwLock<Option<String>>,
    /// Persistent per-peer context token store.
    token_store: tokio::sync::Mutex<ContextTokenStore>,
    /// Dedup cache.
    seen_messages: Mutex<Vec<DedupEntry>>,
    /// Consecutive failure counter.
    consecutive_failures: AtomicU64,
    /// Typing ticket cache.
    typing_cache: TypingTicketCache,
}

impl WeixinAdapter {
    pub fn new(config: WeixinConfig) -> Self {
        let token_store = ContextTokenStore::load();
        let sync_buf_store = SyncBufStore::load();
        // Restore the most recent in-memory token from the store as a default
        let default_token = token_store
            .tokens
            .values()
            .flat_map(|m| m.values())
            .next()
            .cloned();
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_millis(API_TIMEOUT_MS))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            offset: AtomicU64::new(0),
            sync_buf: RwLock::new(sync_buf_store.get_updates_buf),
            context_token: RwLock::new(default_token),
            token_store: tokio::sync::Mutex::new(token_store),
            seen_messages: Mutex::new(Vec::new()),
            consecutive_failures: AtomicU64::new(0),
            typing_cache: TypingTicketCache::new(600),
            config,
        }
    }

    /// Build the common iLink API request body.
    fn build_request(&self) -> serde_json::Value {
        serde_json::json!({
            "ilink_appid": ILINK_APP_ID,
            "channel_version": CHANNEL_VERSION,
            "session_key": self.config.session_key,
        })
    }

    /// Get the current context token for outbound replies.
    pub async fn get_context_token(&self) -> Option<String> {
        self.context_token.read().await.clone()
    }

    /// Get the context token for a specific peer.
    async fn get_context_token_for_peer(&self, peer_id: &str) -> Option<String> {
        let store = self.token_store.lock().await;
        let account = &self.config.session_key;
        store.get(account, peer_id)
    }

    /// Update the context token from an inbound message and persist it.
    async fn update_context_token(&self, peer_id: &str, token: &str) {
        let account = self.config.session_key.clone();
        {
            let mut store = self.token_store.lock().await;
            store.set(&account, peer_id, token);
            let _ = store.save();
        }
        // Also update in-memory default
        *self.context_token.write().await = Some(token.to_string());
    }

    /// Check if a message has been seen before (dedup).
    fn is_duplicate(&self, message_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut seen = self.seen_messages.lock();
        // Purge expired entries
        seen.retain(|e| now - e.timestamp < MESSAGE_DEDUP_TTL_SECONDS);
        seen.iter().any(|e| e.message_id == message_id)
    }

    /// Record a message as seen.
    fn record_seen(&self, message_id: String) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut seen = self.seen_messages.lock();
        seen.push(DedupEntry {
            message_id,
            timestamp: now,
        });
    }

    /// Check if a DM from `sender_id` is allowed by policy.
    /// Mirrors Python `_is_dm_allowed()`.
    pub fn is_dm_allowed(&self, sender_id: &str) -> bool {
        match self.config.dm_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.config.allow_from.contains(&sender_id.to_string()),
            _ => true, // open
        }
    }

    /// Check if a group message is allowed by policy.
    /// Mirrors Python `_is_group_allowed()`.
    pub fn is_group_allowed(&self, group_id: &str) -> bool {
        match self.config.group_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.config.group_allow_from.contains(&group_id.to_string()),
            _ => true, // open
        }
    }

    /// Fetch typing ticket for a user if not cached.
    ///
    /// Mirrors Python `_maybe_fetch_typing_ticket()` (weixin.py:1387).
    async fn maybe_fetch_typing_ticket(&self, user_id: &str, context_token: Option<&str>) {
        if self.config.session_key.is_empty() {
            return;
        }
        if self.typing_cache.get(user_id).is_some() {
            return;
        }
        let mut req = self.build_request();
        req["ilink_user_id"] = serde_json::Value::String(user_id.to_string());
        if let Some(token) = context_token {
            req["context_token"] = serde_json::Value::String(token.to_string());
        }
        match self
            .client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/getconfig"))
            .json(&req)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(ticket) = body.get("typing_ticket").and_then(|v| v.as_str()) {
                        self.typing_cache.set(user_id, ticket);
                    }
                }
            }
            _ => {}
        }
    }

    /// Send a text message to a Weixin peer with per-chunk retry.
    ///
    /// Mirrors Python `_send_text_chunk()` retry logic.
    pub async fn send_text(&self, peer_id: &str, text: &str) -> Result<String, String> {
        if self.config.session_key.is_empty() {
            return Err("Weixin session_key not configured".to_string());
        }
        let text = self.format_message(text);
        let _ = self.send_typing(peer_id).await;

        const MAX_RETRIES: u32 = 2;
        const BASE_DELAY_MS: u64 = 500;
        let mut last_err = String::new();

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = std::cmp::min(BASE_DELAY_MS * (1u64 << (attempt - 1)), 5000);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                tracing::warn!(peer_id = peer_id, attempt, "Weixin send retrying");
            }

            match self.send_text_once(peer_id, &text).await {
                Ok(result) => {
                    let _ = self.stop_typing(peer_id).await;
                    return Ok(result);
                }
                Err(e) if e.contains("session expired") => return Err(e),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(format!(
            "Weixin send failed after {MAX_RETRIES} retries: {last_err}"
        ))
    }

    /// Single-shot send (no retry).
    async fn send_text_once(&self, peer_id: &str, text: &str) -> Result<String, String> {
        let mut req = self.build_request();
        req["peer_id"] = serde_json::Value::String(peer_id.to_string());
        req["msg_type"] = serde_json::Value::Number(1.into()); // text
        req["content"] = serde_json::Value::String(text.to_string());

        // Include per-peer context token if available
        if let Some(token) = self.get_context_token_for_peer(peer_id).await {
            req["context_token"] = serde_json::Value::String(token);
        }

        let resp = self
            .client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/sendmessage"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let err = format!("Send failed: HTTP {status}");
            self.increment_failures();
            return Err(err);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;

        if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
            if errcode == SESSION_EXPIRED_ERRCODE {
                return Err("Weixin session expired".to_string());
            }
            if errcode != 0 {
                return Err(format!("iLink API error: {errcode}"));
            }
        }

        self.reset_failures();
        debug!("Weixin message sent to {peer_id}");
        Ok("ok".to_string())
    }

    /// Long-poll for inbound messages.
    pub async fn get_updates(&self) -> Result<Vec<WeixinMessageEvent>, String> {
        if self.config.session_key.is_empty() {
            return Err("Weixin session_key not configured".to_string());
        }

        let mut req = self.build_request();
        // Prefer sync_buf (mirrors Python) over legacy offset
        let buf = self.sync_buf.read().await.clone();
        if !buf.is_empty() {
            req["get_updates_buf"] = serde_json::Value::String(buf);
        } else {
            req["offset"] = serde_json::Value::Number(self.offset.load(Ordering::SeqCst).into());
        }
        req["limit"] = serde_json::Value::Number(10.into());
        req["timeout"] = serde_json::Value::Number(LONG_POLL_TIMEOUT_MS.into());

        let resp = self
            .client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/getupdates"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Failed to get updates: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse updates: {e}"))?;

        if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
            if errcode == SESSION_EXPIRED_ERRCODE {
                return Err("Weixin session expired".to_string());
            }
            if errcode != 0 {
                return Err(format!("iLink API error: {errcode}"));
            }
        }

        let updates = body
            .get("updates")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut events = Vec::new();
        for update in updates {
            let msg_type = update.get("msg_type").and_then(|v| v.as_u64()).unwrap_or(0);
            let message_id = update
                .get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Dedup
            if !message_id.is_empty() && self.is_duplicate(&message_id) {
                debug!("Weixin dedup: skipping {message_id}");
                continue;
            }

            let peer_id = update
                .get("peer_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let content = update
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Update sync_buf (mirrors Python) or fallback to offset
            if let Some(new_buf) = update.get("get_updates_buf").and_then(|v| v.as_str()) {
                let mut buf = self.sync_buf.write().await;
                *buf = new_buf.to_string();
                let _ = SyncBufStore { get_updates_buf: buf.clone() }.save();
            } else if let Some(new_offset) = update.get("offset").and_then(|v| v.as_u64()) {
                self.offset.store(new_offset, Ordering::SeqCst);
            }

            // Update context token
            if let Some(token) = update.get("context_token").and_then(|v| v.as_str()) {
                self.update_context_token(&peer_id, token).await;
            }

            if !message_id.is_empty() {
                self.record_seen(message_id.clone());
            }

            let msg_type_str = match msg_type {
                1 => "text",
                2 => "image",
                3 => "voice",
                4 => "video",
                5 => "file",
                _ => "unknown",
            };

            // Parse item_list for media attachments
            let mut media_items = Vec::new();
            let mut voice_to_text_parts = Vec::new();
            if let Some(item_list) = update.get("item_list").and_then(|v| v.as_array()) {
                for item in item_list {
                    let item_type = item.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
                    match item_type {
                        1 => {
                            // IMAGE
                            let url = item.get("cdn_url").and_then(|v| v.as_str()).map(String::from);
                            media_items.push(MediaItem {
                                media_type: "image".to_string(),
                                media_url: url,
                                local_path: None,
                                file_name: None,
                                file_size: item.get("file_size").and_then(|v| v.as_u64()),
                            });
                        }
                        3 => {
                            // VOICE
                            let url = item.get("cdn_url").and_then(|v| v.as_str()).map(String::from);
                            media_items.push(MediaItem {
                                media_type: "voice".to_string(),
                                media_url: url.clone(),
                                local_path: None,
                                file_name: None,
                                file_size: item.get("file_size").and_then(|v| v.as_u64()),
                            });
                            // Extract voice-to-text if present
                            if let Some(voice_item) = item.get("voice_item") {
                                if let Some(vtt) = voice_item.get("text").and_then(|v| v.as_str()) {
                                    voice_to_text_parts.push(vtt.to_string());
                                }
                            }
                        }
                        4 => {
                            // VIDEO
                            let url = item.get("cdn_url").and_then(|v| v.as_str()).map(String::from);
                            media_items.push(MediaItem {
                                media_type: "video".to_string(),
                                media_url: url,
                                local_path: None,
                                file_name: None,
                                file_size: item.get("file_size").and_then(|v| v.as_u64()),
                            });
                        }
                        5 => {
                            // FILE
                            let url = item.get("cdn_url").and_then(|v| v.as_str()).map(String::from);
                            let name = item
                                .get("file_item")
                                .and_then(|v| v.get("file_name"))
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            media_items.push(MediaItem {
                                media_type: "file".to_string(),
                                media_url: url,
                                local_path: None,
                                file_name: name,
                                file_size: item.get("file_size").and_then(|v| v.as_u64()),
                            });
                        }
                        _ => {}
                    }
                }
            }

            // Append voice-to-text to content for agent comprehension
            let final_content = if !voice_to_text_parts.is_empty() {
                let vtt = voice_to_text_parts.join(" ");
                if content.is_empty() {
                    format!("[Voice message] {}", vtt)
                } else {
                    format!("{} [Voice: {}]", content, vtt)
                }
            } else {
                content
            };

            // Guess chat type (mirrors Python `_guess_chat_type`)
            let room_id = update.get("room_id").and_then(|v| v.as_str()).unwrap_or("");
            let chat_room_id = update.get("chat_room_id").and_then(|v| v.as_str()).unwrap_or("");
            let to_user_id = update.get("to_user_id").and_then(|v| v.as_str()).unwrap_or("");
            let is_group = !room_id.is_empty()
                || !chat_room_id.is_empty()
                || (!to_user_id.is_empty()
                    && !self.config.session_key.is_empty()
                    && to_user_id != self.config.session_key
                    && msg_type == 1);

            events.push(WeixinMessageEvent {
                message_id: message_id.clone(),
                peer_id: peer_id.clone(),
                sender_name: None,
                content: final_content,
                msg_type: msg_type_str.to_string(),
                is_group,
                media_items,
            });

            // Best-effort fetch typing ticket for this peer
            let context_token = update.get("context_token").and_then(|v| v.as_str());
            self.maybe_fetch_typing_ticket(&peer_id, context_token).await;
        }

        // Download and cache media attachments
        for event in &mut events {
            for item in &mut event.media_items {
                if let Some(path) = self.download_and_cache_media(item).await {
                    item.local_path = Some(path);
                }
            }
        }

        if !events.is_empty() {
            debug!("Weixin received {} message(s)", events.len());
        }

        self.reset_failures();
        Ok(events)
    }

    fn increment_failures(&self) {
        let count = self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        if count >= MAX_CONSECUTIVE_FAILURES as u64 {
            warn!("Weixin: {count} consecutive failures, may need reconnect");
        }
    }

    fn reset_failures(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
    }

    /// Decrypt AES-128-ECB encrypted CDN data.
    ///
    /// Mirrors Python `_decrypt_aes_ecb()` (weixin.py:565).
    /// iLink CDN uses AES-128-ECB with PKCS7 padding.
    fn decrypt_aes_ecb(&self, encrypted: &[u8]) -> Result<Vec<u8>, String> {
        if self.config.encrypt_key.is_empty() {
            return Err("encrypt_key not configured".to_string());
        }
        if encrypted.is_empty() || encrypted.len() % 16 != 0 {
            return Err(format!("Invalid encrypted data length: {}", encrypted.len()));
        }

        let key_bytes = self.config.encrypt_key.as_bytes();
        if key_bytes.len() != 16 {
            return Err(format!("encrypt_key must be 16 bytes, got {}", key_bytes.len()));
        }

        let cipher = Aes128::new_from_slice(key_bytes)
            .map_err(|e| format!("Invalid AES key: {e}"))?;

        let mut result = Vec::with_capacity(encrypted.len());
        for chunk in encrypted.chunks_exact(16) {
            let mut block = Block::<Aes128>::clone_from_slice(chunk);
            cipher.decrypt_block(&mut block);
            result.extend_from_slice(&block);
        }

        // Strip PKCS7 padding
        if let Some(&pad_len) = result.last() {
            if pad_len > 0 && pad_len <= 16 {
                let padding_valid = result.iter().rev().take(pad_len as usize).all(|&b| b == pad_len);
                if padding_valid {
                    let new_len = result.len() - pad_len as usize;
                    result.truncate(new_len);
                }
            }
        }

        Ok(result)
    }

    /// Encrypt plaintext with AES-128-ECB and PKCS7 padding.
    ///
    /// Mirrors Python `_encrypt_aes_ecb()` used for CDN uploads.
    fn encrypt_aes_ecb(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        if self.config.encrypt_key.is_empty() {
            return Err("encrypt_key not configured".to_string());
        }

        let key_bytes = self.config.encrypt_key.as_bytes();
        if key_bytes.len() != 16 {
            return Err(format!("encrypt_key must be 16 bytes, got {}", key_bytes.len()));
        }

        let cipher = Aes128::new_from_slice(key_bytes)
            .map_err(|e| format!("Invalid AES key: {e}"))?;

        // PKCS7 padding
        let block_size = 16;
        let pad_len = block_size - (plaintext.len() % block_size);
        let mut padded = plaintext.to_vec();
        padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));

        let mut result = Vec::with_capacity(padded.len());
        for chunk in padded.chunks_exact(block_size) {
            let mut block = Block::<Aes128>::clone_from_slice(chunk);
            cipher.encrypt_block(&mut block);
            result.extend_from_slice(&block);
        }

        Ok(result)
    }

    /// Download and decrypt a media file from iLink CDN.
    ///
    /// Mirrors Python `_download_media()` (weixin.py:603).
    #[allow(dead_code)]
    pub async fn download_media(&self, media_url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get(media_url)
            .send()
            .await
            .map_err(|e| format!("Failed to download media: {e}"))?;

        let encrypted_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read media body: {e}"))?
            .to_vec();

        self.decrypt_aes_ecb(&encrypted_bytes)
    }

    /// Download a media file from CDN, decrypt it, and cache to disk.
    ///
    /// Returns the local filesystem path on success.
    /// Compute a short content hash for cache deduplication.
    fn content_hash(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(&hasher.finalize()[..8])
    }

    async fn download_and_cache_media(&self, item: &MediaItem) -> Option<String> {
        let url = item.media_url.as_ref()?;
        let bytes = self.download_media(url).await.ok()?;

        let cache_dir = hermez_core::get_hermez_home().join("weixin").join("media");
        std::fs::create_dir_all(&cache_dir).ok()?;

        let ext = match item.media_type.as_str() {
            "image" => "jpg",
            "voice" => "silk",
            "video" => "mp4",
            "file" => item.file_name.as_deref().and_then(|n| n.rsplit('.').next()).unwrap_or("bin"),
            _ => "bin",
        };

        let hash = Self::content_hash(&bytes);
        let file_name = format!("{}_{}.{}", hash, item.media_type, ext);
        let path = cache_dir.join(&file_name);

        // Skip write if already cached (dedup)
        if !path.exists() {
            std::fs::write(&path, bytes).ok()?;
        }
        Some(path.to_string_lossy().to_string())
    }

    /// Get a CDN upload URL from the iLink API.
    ///
    /// Mirrors Python `_get_upload_url()` (weixin.py:645).
    async fn get_upload_url(
        &self,
        peer_id: &str,
        file_size: u64,
        media_type: u32,
    ) -> Result<CdnUploadInfo, String> {
        let mut req = self.build_request();
        req["peer_id"] = serde_json::Value::String(peer_id.to_string());
        req["file_size"] = serde_json::Value::Number(file_size.into());
        req["media_type"] = serde_json::Value::Number(media_type.into());

        let resp = self
            .client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/getuploadurl"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Failed to get upload URL: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse upload URL response: {e}"))?;

        if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
            if errcode != 0 {
                return Err(format!("iLink upload URL error: {errcode}"));
            }
        }

        let url = body
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("Missing upload URL in response")?
            .to_string();
        let auth_token = body
            .get("auth_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sha256 = body
            .get("sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(CdnUploadInfo {
            url,
            auth_token,
            sha256,
        })
    }

    /// Encrypt plaintext and upload to iLink CDN.
    ///
    /// Mirrors Python `_upload_ciphertext()` (weixin.py:681).
    async fn upload_to_cdn(
        &self,
        upload_info: &CdnUploadInfo,
        plaintext: &[u8],
    ) -> Result<String, String> {
        let encrypted = self.encrypt_aes_ecb(plaintext)?;

        let mut req = self.client.post(&upload_info.url);
        if !upload_info.auth_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", upload_info.auth_token));
        }

        let resp = req
            .body(encrypted)
            .send()
            .await
            .map_err(|e| format!("Failed to upload to CDN: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("CDN upload failed: HTTP {status}"));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse CDN upload response: {e}"))?;

        let cdn_param = body
            .get("cdn_param")
            .and_then(|v| v.as_str())
            .ok_or("Missing cdn_param in upload response")?
            .to_string();

        Ok(cdn_param)
    }

    /// Send a media message using a CDN parameter.
    ///
    /// Mirrors Python `_send_file()` (weixin.py:711).
    async fn send_media_message(
        &self,
        peer_id: &str,
        media_type: u32,
        cdn_param: &str,
        file_name: &str,
        file_size: u64,
    ) -> Result<String, String> {
        let mut req = self.build_request();
        req["peer_id"] = serde_json::Value::String(peer_id.to_string());
        req["msg_type"] = serde_json::Value::Number(media_type.into());

        // Build item_list with a single media item
        req["item_list"] = serde_json::json!([{
            "type": media_type,
            "cdn_param": cdn_param,
            "file_name": file_name,
            "file_size": file_size,
        }]);

        // Include per-peer context token if available
        if let Some(token) = self.get_context_token_for_peer(peer_id).await {
            req["context_token"] = serde_json::Value::String(token);
        }

        let resp = self
            .client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/sendmessage"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Failed to send media message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Send media failed: HTTP {status}"));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;

        if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
            if errcode != 0 {
                return Err(format!("iLink media send error: {errcode}"));
            }
        }

        debug!("Weixin media message sent to {peer_id}");
        Ok("ok".to_string())
    }

    /// Read file bytes from a local path or download from a URL.
    async fn read_file_bytes(&self, path_or_url: &str,
    ) -> Result<(Vec<u8>, String), String> {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            let resp = self
                .client
                .get(path_or_url)
                .send()
                .await
                .map_err(|e| format!("Failed to download file: {e}"))?;
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("Failed to read downloaded body: {e}"))?
                .to_vec();
            let name = path_or_url
                .rsplit('/')
                .next()
                .unwrap_or("file")
                .to_string();
            Ok((bytes, name))
        } else {
            let bytes = std::fs::read(path_or_url)
                .map_err(|e| format!("Failed to read file: {e}"))?;
            let name = std::path::Path::new(path_or_url)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            Ok((bytes, name))
        }
    }

    /// Send an image message.
    ///
    /// Mirrors Python `send_image()` (weixin.py:731).
    /// Supports local file paths and URLs.
    #[allow(dead_code)]
    pub async fn send_image(&self, peer_id: &str, image_path: &str) -> Result<String, String> {
        let (bytes, _name) = self.read_file_bytes(image_path).await?;
        let upload_info = self
            .get_upload_url(peer_id, bytes.len() as u64, MEDIA_IMAGE)
            .await?;
        let cdn_param = self.upload_to_cdn(&upload_info, &bytes).await?;
        self.send_media_message(peer_id, MEDIA_IMAGE, &cdn_param, "image.jpg", bytes.len() as u64)
            .await
    }

    /// Send a voice/audio message.
    #[allow(dead_code)]
    pub async fn send_voice(
        &self,
        peer_id: &str,
        voice_path: &str,
    ) -> Result<String, String> {
        let (bytes, name) = self.read_file_bytes(voice_path).await?;
        let upload_info = self
            .get_upload_url(peer_id, bytes.len() as u64, MEDIA_VOICE)
            .await?;
        let cdn_param = self.upload_to_cdn(&upload_info, &bytes).await?;
        let file_name = if name.ends_with(".silk") { name } else { format!("{name}.silk") };
        self.send_media_message(peer_id, MEDIA_VOICE, &cdn_param, &file_name, bytes.len() as u64)
            .await
    }

    /// Send a video message.
    #[allow(dead_code)]
    pub async fn send_video(
        &self,
        peer_id: &str,
        video_path: &str,
    ) -> Result<String, String> {
        let (bytes, name) = self.read_file_bytes(video_path).await?;
        let upload_info = self
            .get_upload_url(peer_id, bytes.len() as u64, MEDIA_VIDEO)
            .await?;
        let cdn_param = self.upload_to_cdn(&upload_info, &bytes).await?;
        self.send_media_message(peer_id, MEDIA_VIDEO, &cdn_param, &name, bytes.len() as u64)
            .await
    }

    /// Send a document/file message.
    #[allow(dead_code)]
    pub async fn send_document(
        &self,
        peer_id: &str,
        doc_path: &str,
    ) -> Result<String, String> {
        let (bytes, name) = self.read_file_bytes(doc_path).await?;
        let upload_info = self
            .get_upload_url(peer_id, bytes.len() as u64, MEDIA_FILE)
            .await?;
        let cdn_param = self.upload_to_cdn(&upload_info, &bytes).await?;
        self.send_media_message(peer_id, MEDIA_FILE, &cdn_param, &name, bytes.len() as u64)
            .await
    }

    /// Send typing indicator.
    ///
    /// Mirrors Python `_send_typing()` (weixin.py:424).
    pub async fn send_typing(&self, peer_id: &str) -> Result<String, String> {
        if let Some(ticket) = self.typing_cache.get(peer_id) {
            let mut req = self.build_request();
            req["ilink_user_id"] = serde_json::Value::String(peer_id.to_string());
            req["typing_ticket"] = serde_json::Value::String(ticket);
            req["status"] = serde_json::Value::Number(1.into());
            let _resp = self
                .client
                .post(format!("{ILINK_BASE_URL}/ilink/bot/sendtyping"))
                .json(&req)
                .send()
                .await
                .map_err(|e| format!("Failed to send typing: {e}"))?;
            debug!("Weixin typing indicator sent to {peer_id}");
        }
        Ok("ok".to_string())
    }

    /// Stop typing indicator.
    pub async fn stop_typing(&self, peer_id: &str) -> Result<String, String> {
        if let Some(ticket) = self.typing_cache.get(peer_id) {
            let mut req = self.build_request();
            req["ilink_user_id"] = serde_json::Value::String(peer_id.to_string());
            req["typing_ticket"] = serde_json::Value::String(ticket);
            req["status"] = serde_json::Value::Number(0.into());
            let _ = self
                .client
                .post(format!("{ILINK_BASE_URL}/ilink/bot/sendtyping"))
                .json(&req)
                .send()
                .await;
        }
        Ok("ok".to_string())
    }

    /// Format a message with optional markdown-like styling.
    ///
    /// Format message content for WeChat compatibility.
    ///
    /// Mirrors Python `format_message()` (weixin.py:1755).
    pub fn format_message(&self, text: &str) -> String {
        normalize_markdown_blocks(text)
    }

    /// Send text with chunking for long messages.
    ///
    /// Mirrors Python `_send_text_chunk()` (weixin.py:1058).
    /// Splits messages >2000 chars into multiple sends with (N/M) prefix.
    #[allow(dead_code)]
    pub async fn send_text_chunked(
        &self,
        peer_id: &str,
        text: &str,
        max_chunk: usize,
    ) -> Result<Vec<String>, String> {
        if text.len() <= max_chunk {
            return self.send_text(peer_id, text).await.map(|r| vec![r]);
        }

        // Split on sentence boundaries when possible
        let chunks: Vec<String> = text
            .chars()
            .collect::<Vec<_>>()
            .chunks(max_chunk)
            .map(|c| c.iter().collect::<String>())
            .collect();

        let total = chunks.len();
        let mut msg_ids = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            let prefix = if total > 1 {
                format!("({}/{}) ", i + 1, total)
            } else {
                String::new()
            };
            let msg = format!("{prefix}{chunk}");
            let msg_id = self.send_text(peer_id, &msg).await?;
            msg_ids.push(msg_id);
            // Small delay between chunks to avoid rate limiting
            if i < total - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        Ok(msg_ids)
    }

    /// Check if the adapter is properly configured.
    pub fn is_configured(&self) -> bool {
        !self.config.session_key.is_empty()
    }
}

/// QR code login flow for Weixin iLink.
///
/// 1. Request QR code from iLink API.
/// 2. Display URL to user (terminal-friendly).
/// 3. Poll status every 3s until scanned and confirmed.
/// 4. Save credentials to `~/.hermez/weixin/account.json`.
///
/// Mirrors Python `qr_login()` (weixin.py:1120).
pub async fn qr_login() -> Result<WeixinConfig, String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_millis(API_TIMEOUT_MS))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let mut req = serde_json::json!({
        "ilink_appid": ILINK_APP_ID,
        "channel_version": CHANNEL_VERSION,
    });

    // Step 1: Get QR code
    let resp = client
        .post(format!("{ILINK_BASE_URL}/ilink/bot/get_bot_qrcode"))
        .json(&req)
        .send()
        .await
        .map_err(|e| format!("Failed to get QR code: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse QR response: {e}"))?;

    if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
        if errcode != 0 {
            return Err(format!("iLink QR error: {errcode}"));
        }
    }

    let qr_url = body
        .get("qrcode_url")
        .and_then(|v| v.as_str())
        .ok_or("Missing qrcode_url in response")?;
    let qr_ticket = body
        .get("qrcode_ticket")
        .and_then(|v| v.as_str())
        .ok_or("Missing qrcode_ticket in response")?;

    println!("\n◆ Weixin QR Login");
    println!("  Please scan the QR code with WeChat:");
    println!("  {qr_url}\n");

    // Step 2: Poll status
    req["qrcode_ticket"] = serde_json::Value::String(qr_ticket.to_string());

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let resp = client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/get_qrcode_status"))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("Failed to poll QR status: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse QR status: {e}"))?;

        if let Some(errcode) = body.get("errcode").and_then(|v| v.as_i64()) {
            if errcode != 0 {
                return Err(format!("iLink QR status error: {errcode}"));
            }
        }

        let status = body
            .get("status")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        match status {
            0 => {
                // wait
                print!(".");
                let _ = std::io::stdout().flush();
            }
            1 => {
                // scaned
                println!("\n  QR code scanned, waiting for confirmation...");
            }
            2 => {
                // confirmed
                let session_key = body
                    .get("session_key")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing session_key after confirmation")?;
                let encrypt_key = body
                    .get("encrypt_key")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing encrypt_key after confirmation")?;

                let account = WeixinAccount {
                    session_key: session_key.to_string(),
                    encrypt_key: encrypt_key.to_string(),
                    bot_wxid: body.get("bot_wxid").and_then(|v| v.as_str()).map(String::from),
                };
                account.save()?;

                println!("  Login successful! Credentials saved.\n");
                return Ok(WeixinConfig {
                    session_key: session_key.to_string(),
                    encrypt_key: encrypt_key.to_string(),
                    dm_policy: "open".to_string(),
                    group_policy: "disabled".to_string(),
                    allow_from: Vec::new(),
                    group_allow_from: Vec::new(),
                });
            }
            3 => {
                // expired
                return Err("QR code expired. Please try again.".to_string());
            }
            _ => {
                return Err(format!("Unknown QR status: {status}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown normalization — mirrors Python `_normalize_markdown_blocks()`
// ---------------------------------------------------------------------------

use regex::Regex;
use std::sync::LazyLock;

static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*$").unwrap());
static TABLE_RULE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\|?(?:\s*:?-{3,}:?\s*\|)+\s*:?-{3,}:?\s*\|?\s*$").unwrap());
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^```([^\n`]*)$").unwrap());
static MARKDOWN_LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
static MULTI_NEWLINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// Normalize markdown for WeChat compatibility.
///
/// Mirrors Python `_normalize_markdown_blocks()` (weixin.py:623).
fn normalize_markdown_blocks(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    let mut in_code_block = false;

    while i < lines.len() {
        let line = lines[i].trim_end();
        if FENCE_RE.is_match(line.trim()) {
            in_code_block = !in_code_block;
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if in_code_block {
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if i + 1 < lines.len()
            && lines[i].contains('|')
            && TABLE_RULE_RE.is_match(lines[i + 1].trim_end())
        {
            let mut table_lines = vec![
                lines[i].trim_end().to_string(),
                lines[i + 1].trim_end().to_string(),
            ];
            i += 2;
            while i < lines.len() && lines[i].contains('|') {
                table_lines.push(lines[i].trim_end().to_string());
                i += 1;
            }
            result.push(rewrite_table_block_for_weixin(&table_lines));
            continue;
        }

        let rewritten = rewrite_headers_for_weixin(line);
        let no_links = MARKDOWN_LINK_RE.replace_all(&rewritten, "$1 ($2)");
        result.push(no_links.to_string());
        i += 1;
    }

    let normalized = result.join("\n");
    MULTI_NEWLINE_RE.replace_all(&normalized, "\n\n").trim().to_string()
}

/// Rewrite markdown headers for WeChat.
///
/// Mirrors Python `_rewrite_headers_for_weixin()` (weixin.py:577).
fn rewrite_headers_for_weixin(line: &str) -> String {
    if let Some(caps) = HEADER_RE.captures(line) {
        let level = caps[1].len();
        let title = caps[2].trim();
        if level == 1 {
            return format!("【{title}】");
        }
        return format!("**{title}**");
    }
    line.to_string()
}

/// Split a GFM table row into cells.
///
/// Mirrors Python `_split_table_row()` (weixin.py:568).
fn split_table_row(line: &str) -> Vec<String> {
    let mut row = line.trim();
    if row.starts_with('|') {
        row = &row[1..];
    }
    if row.ends_with('|') {
        row = &row[..row.len() - 1];
    }
    row.split('|').map(|s| s.trim().to_string()).collect()
}

/// Flatten a markdown table into WeChat-friendly lines.
///
/// Mirrors Python `_rewrite_table_block_for_weixin()` (weixin.py:588).
fn rewrite_table_block_for_weixin(lines: &[String]) -> String {
    if lines.len() < 2 {
        return lines.join("\n");
    }
    let headers = split_table_row(&lines[0]);
    let body_rows: Vec<Vec<String>> = lines[2..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| split_table_row(l))
        .collect();
    if headers.is_empty() || body_rows.is_empty() {
        return lines.join("\n");
    }

    let mut formatted_rows = Vec::new();
    for row in &body_rows {
        let mut pairs = Vec::new();
        for (idx, header) in headers.iter().enumerate() {
            if idx >= row.len() {
                break;
            }
            let label = if header.is_empty() {
                format!("Column {}", idx + 1)
            } else {
                header.clone()
            };
            let value = row[idx].trim();
            if !value.is_empty() {
                pairs.push((label, value.to_string()));
            }
        }
        if pairs.is_empty() {
            continue;
        }
        if pairs.len() == 1 {
            let (label, value) = &pairs[0];
            formatted_rows.push(format!("- {label}: {value}"));
        } else if pairs.len() == 2 {
            let (l1, v1) = &pairs[0];
            let (l2, v2) = &pairs[1];
            formatted_rows.push(format!("- {l1}: {v1}"));
            formatted_rows.push(format!("  {l2}: {v2}"));
        } else {
            let summary = pairs
                .iter()
                .map(|(l, v)| format!("{l}: {v}"))
                .collect::<Vec<_>>()
                .join(" | ");
            formatted_rows.push(format!("- {summary}"));
        }
    }

    if formatted_rows.is_empty() {
        lines.join("\n")
    } else {
        formatted_rows.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env() {
        let config = WeixinConfig::from_env();
        assert!(config.session_key.is_empty());
    }

    #[test]
    fn test_not_configured_when_empty() {
        let config = WeixinConfig::from_env();
        let adapter = WeixinAdapter::new(config);
        assert!(!adapter.is_configured());
    }

    #[test]
    fn test_build_request() {
        let config = WeixinConfig {
            session_key: "test_key".to_string(),
            encrypt_key: "".to_string(),
            dm_policy: "open".to_string(),
            group_policy: "disabled".to_string(),
            allow_from: Vec::new(),
            group_allow_from: Vec::new(),
        };
        let adapter = WeixinAdapter::new(config);
        let req = adapter.build_request();
        assert_eq!(req["ilink_appid"], "bot");
        assert_eq!(req["channel_version"], "2.2.0");
        assert_eq!(req["session_key"], "test_key");
    }

    #[test]
    fn test_content_hash() {
        let h1 = WeixinAdapter::content_hash(b"hello");
        let h2 = WeixinAdapter::content_hash(b"hello");
        let h3 = WeixinAdapter::content_hash(b"world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 16); // 8 bytes = 16 hex chars
    }

    #[test]
    fn test_context_token_store_roundtrip() {
        let mut store = ContextTokenStore::default();
        store.set("acct1", "peer1", "token_a");
        store.set("acct1", "peer2", "token_b");
        store.set("acct2", "peer1", "token_c");

        assert_eq!(store.get("acct1", "peer1"), Some("token_a".to_string()));
        assert_eq!(store.get("acct1", "peer2"), Some("token_b".to_string()));
        assert_eq!(store.get("acct2", "peer1"), Some("token_c".to_string()));
        assert_eq!(store.get("acct1", "missing"), None);

        // Save and reload
        let _ = store.save();
        let loaded = ContextTokenStore::load();
        assert_eq!(loaded.get("acct1", "peer1"), Some("token_a".to_string()));
        assert_eq!(loaded.get("acct2", "peer1"), Some("token_c".to_string()));

        // Cleanup
        let _ = std::fs::remove_file(ContextTokenStore::path());
    }

    #[test]
    fn test_parse_comma_list() {
        assert_eq!(parse_comma_list("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(parse_comma_list("  a  ,  b  "), vec!["a", "b"]);
        assert!(parse_comma_list("").is_empty());
    }

    #[test]
    fn test_content_hash_length() {
        let h = WeixinAdapter::content_hash(b"test");
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn test_normalize_markdown_blocks() {
        let input = "```\ncode\n```";
        let out = normalize_markdown_blocks(input);
        assert!(!out.is_empty());
    }

    #[test]
    fn test_is_duplicate_and_record() {
        let config = WeixinConfig::from_env();
        let adapter = WeixinAdapter::new(config);
        assert!(!adapter.is_duplicate("msg1"));
        adapter.record_seen("msg1".into());
        assert!(adapter.is_duplicate("msg1"));
        assert!(!adapter.is_duplicate("msg2"));
    }
}
