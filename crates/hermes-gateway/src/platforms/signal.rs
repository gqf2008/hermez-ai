#![allow(dead_code)]
//! Signal messenger platform adapter.
//!
//! Mirrors the Python `gateway/platforms/signal.py`.
//!
//! Connects to a signal-cli daemon running in HTTP mode.
//! Inbound messages arrive via SSE (Server-Sent Events) streaming.
//! Outbound messages and actions use JSON-RPC 2.0 over HTTP.
//!
//! Required env vars:
//!   - SIGNAL_HTTP_URL (default: http://127.0.0.1:8080)
//!   - SIGNAL_PHONE_NUMBER (the Signal account number)
//!
//! Optional:
//!   - SIGNAL_ALLOWED_USERS (comma-separated phone numbers)
//!   - SIGNAL_ALLOW_ALL_USERS (default: false)
//!   - SIGNAL_GROUP_ALLOWED_USERS (comma-separated group IDs, or * for all)
//!   - SIGNAL_IGNORE_STORIES (default: true)

use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::{redact_phone, MessageDeduplicator};
use crate::runner::MessageHandler;

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 8000;
const SIGNAL_MAX_ATTACHMENT_SIZE: usize = 100 * 1024 * 1024; // 100 MB
const SSE_RETRY_DELAY_INITIAL: f64 = 2.0;
const SSE_RETRY_DELAY_MAX: f64 = 60.0;
const HEALTH_CHECK_INTERVAL: f64 = 30.0;
const HEALTH_CHECK_STALE_THRESHOLD: f64 = 120.0;

// ── Configuration ──────────────────────────────────────────────────────────

/// Signal platform configuration.
#[derive(Debug, Clone)]
pub struct SignalConfig {
    pub signal_http_url: String,
    pub phone_number: String,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub group_allowed_users: HashSet<String>,
    pub ignore_stories: bool,
}

impl Default for SignalConfig {
    fn default() -> Self {
        let group_allowed = std::env::var("SIGNAL_GROUP_ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let allowed_users = std::env::var("SIGNAL_ALLOWED_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            signal_http_url: std::env::var("SIGNAL_HTTP_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
            phone_number: std::env::var("SIGNAL_PHONE_NUMBER").unwrap_or_default(),
            allowed_users,
            allow_all_users: is_env_true("SIGNAL_ALLOW_ALL_USERS"),
            group_allowed_users: group_allowed,
            ignore_stories: !is_env_false("SIGNAL_IGNORE_STORIES"),
        }
    }
}

impl SignalConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn is_configured(&self) -> bool {
        !self.signal_http_url.is_empty() && !self.phone_number.is_empty()
    }
}

fn is_env_true(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn is_env_false(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

// ── Data types ─────────────────────────────────────────────────────────────

/// Inbound message event from Signal.
#[derive(Debug, Clone)]
pub struct SignalMessageEvent {
    pub chat_id: String,
    pub chat_type: String,
    pub sender: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: u64,
    pub is_group: bool,
    pub media_urls: Vec<String>,
    pub media_types: Vec<String>,
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// Signal messenger adapter using signal-cli HTTP daemon.
pub struct SignalAdapter {
    config: SignalConfig,
    client: Client,
    dedup: MessageDeduplicator,
    running: Arc<AtomicBool>,
    last_sse_activity: Arc<Mutex<f64>>,
    /// Recently sent message timestamps for echo-back filtering.
    recent_sent_timestamps: Arc<Mutex<HashSet<u64>>>,
}

impl SignalAdapter {
    pub fn new(config: SignalConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: MessageDeduplicator::new(2000, 300.0),
            running: Arc::new(AtomicBool::new(false)),
            last_sse_activity: Arc::new(Mutex::new(0.0)),
            recent_sent_timestamps: Arc::new(Mutex::new(HashSet::new())),
            config,
        }
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────

    /// Connect to signal-cli daemon and start SSE listener.
    pub async fn run(
        self: Arc<Self>,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) {
        if !self.config.is_configured() {
            error!("Signal: SIGNAL_HTTP_URL and SIGNAL_PHONE_NUMBER are required");
            return;
        }

        // Health check — verify signal-cli daemon is reachable
        match self.health_check().await {
            Ok(true) => {}
            Ok(false) => {
                error!("Signal: health check failed");
                return;
            }
            Err(e) => {
                error!("Signal: cannot reach signal-cli at {}: {e}", self.config.signal_http_url);
                return;
            }
        }

        self.running.store(true, Ordering::SeqCst);
        *self.last_sse_activity.lock().await = now_secs();

        info!(
            "Signal: connected to {} account={}",
            self.config.signal_http_url,
            redact_phone(&self.config.phone_number)
        );

        let adapter = self.clone();
        let sse_handle = tokio::spawn(async move {
            adapter.sse_listener(handler.clone()).await;
        });

        let adapter = self.clone();
        let health_handle = tokio::spawn(async move {
            adapter.health_monitor().await;
        });

        // Wait for shutdown signal
        while running.load(Ordering::SeqCst) && self.running.load(Ordering::SeqCst) {
            sleep(Duration::from_secs(1)).await;
        }

        self.running.store(false, Ordering::SeqCst);
        sse_handle.abort();
        health_handle.abort();

        info!("Signal: disconnected");
    }

    async fn health_check(&self) -> Result<bool, String> {
        let url = format!("{}/api/v1/check", self.config.signal_http_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("health check request failed: {e}"))?;
        Ok(resp.status().is_success())
    }

    // ── SSE Streaming ─────────────────────────────────────────────────────

    async fn sse_listener(&self, handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>) {
        let base_url = self.config.signal_http_url.trim_end_matches('/');
        let account_escaped = urlencoding::encode(&self.config.phone_number);
        let url = format!("{base_url}/api/v1/events?account={account_escaped}");
        let mut backoff = SSE_RETRY_DELAY_INITIAL;

        while self.running.load(Ordering::SeqCst) {
            match self.sse_connect_and_listen(&url, &handler).await {
                Ok(()) => {
                    backoff = SSE_RETRY_DELAY_INITIAL;
                }
                Err(e) => {
                    if !self.running.load(Ordering::SeqCst) {
                        break;
                    }
                    warn!("Signal SSE: error: {e} (reconnecting in {backoff:.0}s)");
                    let jitter = backoff * 0.2 * (now_secs() % 1.0);
                    sleep(Duration::from_secs_f64(backoff + jitter)).await;
                    backoff = (backoff * 2.0).min(SSE_RETRY_DELAY_MAX);
                }
            }
        }
    }

    async fn sse_connect_and_listen(
        &self,
        url: &str,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    ) -> Result<(), String> {
        let resp = self
            .client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("SSE connect failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("SSE HTTP error: {}", resp.status()));
        }

        *self.last_sse_activity.lock().await = now_secs();
        info!("Signal SSE: connected");

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while self.running.load(Ordering::SeqCst) {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    *self.last_sse_activity.lock().await = now_secs();
                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer = buffer[pos + 1..].to_string();

                        if line.is_empty() {
                            continue;
                        }
                        // SSE comment keepalive
                        if line.starts_with(':') {
                            *self.last_sse_activity.lock().await = now_secs();
                            continue;
                        }
                        // SSE data line
                        if let Some(data_str) = line.strip_prefix("data:") {
                            let data_str = data_str.trim();
                            if data_str.is_empty() {
                                continue;
                            }
                            *self.last_sse_activity.lock().await = now_secs();
                            match serde_json::from_str::<Value>(data_str) {
                                Ok(data) => {
                                    if let Err(e) = self.handle_envelope(data, handler).await {
                                        debug!("Signal SSE: error handling event: {e}");
                                    }
                                }
                                Err(e) => {
                                    debug!("Signal SSE: invalid JSON: {e}");
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(format!("SSE stream error: {e}"));
                }
                None => {
                    return Err("SSE stream ended".to_string());
                }
            }
        }

        Ok(())
    }

    // ── Health Monitor ────────────────────────────────────────────────────

    async fn health_monitor(&self) {
        while self.running.load(Ordering::SeqCst) {
            sleep(Duration::from_secs_f64(HEALTH_CHECK_INTERVAL)).await;
            if !self.running.load(Ordering::SeqCst) {
                break;
            }

            let elapsed = now_secs() - *self.last_sse_activity.lock().await;
            if elapsed > HEALTH_CHECK_STALE_THRESHOLD {
                warn!("Signal: SSE idle for {elapsed:.0}s, checking daemon health");
                match self.health_check().await {
                    Ok(true) => {
                        *self.last_sse_activity.lock().await = now_secs();
                        debug!("Signal: daemon healthy, SSE idle");
                    }
                    Ok(false) => {
                        warn!("Signal: health check failed, forcing reconnect");
                    }
                    Err(e) => {
                        warn!("Signal: health check error: {e}, forcing reconnect");
                    }
                }
            }
        }
    }

    // ── Message Handling ──────────────────────────────────────────────────

    async fn handle_envelope(
        &self,
        envelope: Value,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    ) -> Result<(), String> {
        let envelope_data = envelope.get("envelope").unwrap_or(&envelope);

        // Handle syncMessage: extract "Note to Self" messages
        let mut is_note_to_self = false;
        if let Some(sync_msg) = envelope_data.get("syncMessage") {
            if let Some(sent_msg) = sync_msg.get("sentMessage") {
                let dest = sent_msg
                    .get("destinationNumber")
                    .and_then(|v| v.as_str())
                    .or_else(|| sent_msg.get("destination").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let sent_ts = sent_msg.get("timestamp").and_then(|v| v.as_u64());
                if dest == self.config.phone_number {
                    // Echo filtering
                    if let Some(ts) = sent_ts {
                        let recent = self.recent_sent_timestamps.lock().await;
                        if recent.contains(&ts) {
                            drop(recent);
                            let mut recent = self.recent_sent_timestamps.lock().await;
                            recent.remove(&ts);
                            return Ok(());
                        }
                    }
                    is_note_to_self = true;
                } else if !is_note_to_self {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        }

        // Extract sender info
        let sender = envelope_data
            .get("sourceNumber")
            .and_then(|v| v.as_str())
            .or_else(|| envelope_data.get("sourceUuid").and_then(|v| v.as_str()))
            .or_else(|| envelope_data.get("source").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        if sender.is_empty() {
            debug!("Signal: ignoring envelope with no sender");
            return Ok(());
        }

        // Self-message filtering (but allow Note to Self)
        if sender == self.config.phone_number && !is_note_to_self {
            return Ok(());
        }

        // Filter stories
        if self.config.ignore_stories && envelope_data.get("storyMessage").is_some() {
            return Ok(());
        }

        // Get data message
        let data_message = envelope_data
            .get("dataMessage")
            .or_else(|| {
                envelope_data
                    .get("editMessage")
                    .and_then(|em| em.get("dataMessage"))
            })
            .cloned()
            .unwrap_or(Value::Null);

        if data_message.is_null() {
            return Ok(());
        }

        // Group handling
        let group_info = data_message.get("groupInfo");
        let group_id = group_info.and_then(|g| g.get("groupId")).and_then(|v| v.as_str());
        let is_group = group_id.is_some();

        if is_group {
            if self.config.group_allowed_users.is_empty() {
                debug!("Signal: ignoring group message (no SIGNAL_GROUP_ALLOWED_USERS)");
                return Ok(());
            }
            let gid = match group_id {
                Some(g) => g,
                None => return Ok(()),
            };
            if !self.config.group_allowed_users.contains("*") && !self.config.group_allowed_users.contains(gid) {
                debug!("Signal: group {} not in allowlist", &gid[..gid.len().min(8)]);
                return Ok(());
            }
        }

        // User authorization for DMs
        if !is_group && !self.config.allow_all_users && !self.config.allowed_users.is_empty()
            && !self.config.allowed_users.iter().any(|u| u == &sender) {
                warn!("Signal: unauthorized user {}", redact_phone(&sender));
                return Ok(());
            }

        let chat_id = if is_group {
            format!("group:{}", group_id.unwrap_or_default())
        } else {
            sender.clone()
        };
        let _chat_type = if is_group { "group" } else { "dm" };

        // Extract text and render mentions
        let mut text = data_message.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(mentions) = data_message.get("mentions").and_then(|v| v.as_array()) {
            text = render_mentions(&text, mentions);
        }

        // Deduplication using envelope timestamp + sender
        let ts_ms = envelope_data.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
        let dedup_key = format!("{sender}:{ts_ms}");
        if self.dedup.is_duplicate(&dedup_key) {
            return Ok(());
        }

        // Process attachments
        let mut media_urls = Vec::new();
        let mut media_types = Vec::new();
        if let Some(attachments) = data_message.get("attachments").and_then(|v| v.as_array()) {
            for att in attachments {
                let att_id = att.get("id").and_then(|v| v.as_str());
                let att_size = att.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let att_id = match att_id {
                    Some(id) => id,
                    None => continue,
                };
                if att_size > SIGNAL_MAX_ATTACHMENT_SIZE as u64 {
                    warn!("Signal: attachment too large ({att_size} bytes), skipping");
                    continue;
                }
                match self.fetch_attachment(att_id).await {
                    Ok((path, mime)) => {
                        media_urls.push(path);
                        media_types.push(mime);
                    }
                    Err(e) => {
                        warn!("Signal: failed to fetch attachment {att_id}: {e}");
                    }
                }
            }
        }

        let _sender_name = envelope_data
            .get("sourceName")
            .and_then(|v| v.as_str())
            .unwrap_or(&sender)
            .to_string();

        debug!(
            "Signal: message from {} in {}: {}",
            redact_phone(&sender),
            &chat_id[..chat_id.len().min(20)],
            &text[..text.len().min(50)]
        );

        let handler_guard = handler.lock().await;
        let handler_ref = handler_guard.as_ref().cloned();
        drop(handler_guard);

        if let Some(h) = handler_ref {
            let adapter = self.clone_like();
            let chat_id_clone = chat_id.clone();
            let content_clone = text.clone();
            tokio::spawn(async move {
                match h.handle_message(Platform::Signal, &chat_id_clone, &content_clone, None).await {
                    Ok(result) => {
                        if !result.response.is_empty() {
                            if let Err(e) = adapter.send(&chat_id_clone, &result.response).await {
                                error!("Signal send failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Signal handler error: {e}");
                    }
                }
            });
        }

        Ok(())
    }

    // ── Attachment Handling ───────────────────────────────────────────────

    async fn fetch_attachment(&self, attachment_id: &str) -> Result<(String, String), String> {
        let result = self
            .rpc("getAttachment", {
                let mut params = serde_json::Map::new();
                params.insert("account".to_string(), Value::String(self.config.phone_number.clone()));
                params.insert("id".to_string(), Value::String(attachment_id.to_string()));
                Value::Object(params)
            })
            .await?;

        let b64_data = match result {
            Value::Object(mut map) => map
                .remove("data")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .ok_or("attachment response missing 'data' key")?,
            Value::String(s) => s,
            _ => return Err("unexpected attachment response type".to_string()),
        };

        let raw_data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64_data)
            .map_err(|e| format!("base64 decode failed: {e}"))?;

        let ext = guess_extension(&raw_data);
        let mime = ext_to_mime(&ext);

        let cache_dir = hermes_core::get_hermes_home().join("signal").join("media");
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .map_err(|e| format!("mkdir failed: {e}"))?;

        let file_name = format!("attachment_{attachment_id}{ext}");
        let local_path = cache_dir.join(&file_name);
        tokio::fs::write(&local_path, raw_data)
            .await
            .map_err(|e| format!("write failed: {e}"))?;

        Ok((local_path.to_string_lossy().to_string(), mime))
    }

    // ── JSON-RPC Communication ────────────────────────────────────────────

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": format!("{method}_{}", now_secs()),
        });

        let url = format!("{}/api/v1/rpc", self.config.signal_http_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("RPC {method} request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("RPC {method} HTTP error: {}", resp.status()));
        }

        let data: Value = resp.json().await.map_err(|e| format!("RPC {method} parse error: {e}"))?;

        if let Some(err) = data.get("error") {
            return Err(format!("RPC {method} error: {err}"));
        }

        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }

    // ── Sending ───────────────────────────────────────────────────────────

    /// Send a text message.
    pub async fn send(&self, chat_id: &str, content: &str) -> Result<(), String> {
        let chunks = split_message(content, MAX_MESSAGE_LENGTH);
        for chunk in chunks {
            let mut params = serde_json::Map::new();
            params.insert("account".to_string(), Value::String(self.config.phone_number.clone()));
            params.insert("message".to_string(), Value::String(chunk));

            if let Some(group_id) = chat_id.strip_prefix("group:") {
                params.insert("groupId".to_string(), Value::String(group_id.to_string()));
            } else {
                params.insert("recipient".to_string(), Value::Array(vec![Value::String(chat_id.to_string())]));
            }

            let result = self.rpc("send", Value::Object(params)).await?;
            self.track_sent_timestamp(&result).await;
        }
        Ok(())
    }

    /// Send a message with a local file attachment.
    pub async fn send_attachment(
        &self,
        chat_id: &str,
        file_path: &std::path::Path,
        caption: Option<&str>,
    ) -> Result<(), String> {
        let file_path_str = file_path.to_string_lossy().to_string();

        let mut params = serde_json::Map::new();
        params.insert("account".to_string(), Value::String(self.config.phone_number.clone()));
        params.insert("message".to_string(), Value::String(caption.unwrap_or("").to_string()));
        params.insert("attachments".to_string(), Value::Array(vec![Value::String(file_path_str)]));

        if let Some(group_id) = chat_id.strip_prefix("group:") {
            params.insert("groupId".to_string(), Value::String(group_id.to_string()));
        } else {
            params.insert("recipient".to_string(), Value::Array(vec![Value::String(chat_id.to_string())]));
        }

        let result = self.rpc("send", Value::Object(params)).await?;
        self.track_sent_timestamp(&result).await;
        Ok(())
    }

    async fn track_sent_timestamp(&self, rpc_result: &Value) {
        if let Some(ts) = rpc_result.get("timestamp").and_then(|v| v.as_u64()) {
            let mut recent = self.recent_sent_timestamps.lock().await;
            recent.insert(ts);
            if recent.len() > 50 {
                // Simple eviction: clear and re-insert latest
                let to_keep: Vec<u64> = recent.iter().cloned().collect();
                recent.clear();
                for t in to_keep.into_iter().rev().take(25) {
                    recent.insert(t);
                }
            }
        }
    }

    // ── Clone helper ──────────────────────────────────────────────────────

    fn clone_like(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            running: self.running.clone(),
            last_sse_activity: self.last_sse_activity.clone(),
            recent_sent_timestamps: self.recent_sent_timestamps.clone(),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Replace Signal mention placeholders (\uFFFC) with readable @identifiers.
fn render_mentions(text: &str, mentions: &[Value]) -> String {
    if mentions.is_empty() || !text.contains('\u{FFFC}') {
        return text.to_string();
    }
    let mut result = text.to_string();
    let mut mention_iter = mentions.iter();
    while let Some(pos) = result.find('\u{FFFC}') {
        if let Some(mention) = mention_iter.next() {
            let identifier = mention
                .get("number")
                .and_then(|v| v.as_str())
                .or_else(|| mention.get("uuid").and_then(|v| v.as_str()))
                .unwrap_or("user");
            let end = pos + '\u{FFFC}'.len_utf8();
            result.replace_range(pos..end, &format!("@{identifier}"));
        } else {
            break;
        }
    }
    result
}

/// Guess file extension from magic bytes.
fn guess_extension(data: &[u8]) -> String {
    if data.len() >= 4 && &data[..4] == b"\x89PNG" {
        return ".png".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"\xff\xd8" {
        return ".jpg".to_string();
    }
    if data.len() >= 4 && &data[..4] == b"GIF8" {
        return ".gif".to_string();
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return ".webp".to_string();
    }
    if data.len() >= 4 && &data[..4] == b"%PDF" {
        return ".pdf".to_string();
    }
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return ".mp4".to_string();
    }
    if data.len() >= 4 && &data[..4] == b"OggS" {
        return ".ogg".to_string();
    }
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        return ".mp3".to_string();
    }
    if data.len() >= 2 && &data[..2] == b"PK" {
        return ".zip".to_string();
    }
    ".bin".to_string()
}

fn ext_to_mime(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".ogg" => "audio/ogg",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".m4a" => "audio/mp4",
        ".aac" => "audio/aac",
        ".mp4" => "video/mp4",
        ".pdf" => "application/pdf",
        ".zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Split a long message into chunks.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let mut split_at = if remaining.len() <= max_len {
            remaining.len()
        } else {
            let mut pos = max_len;
            while pos > 0 && !remaining.is_char_boundary(pos) {
                pos -= 1;
            }
            if pos == 0 {
                pos = max_len;
            }
            pos
        };
        if split_at < remaining.len() {
            if let Some(pos) = remaining[..split_at].rfind('\n') {
                if pos > 0 {
                    split_at = pos + 1;
                }
            } else if let Some(pos) = remaining[..split_at].rfind(' ') {
                if pos > 0 {
                    split_at = pos + 1;
                }
            }
        }
        let (chunk, rest) = remaining.split_at(split_at);
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
    fn test_split_message_short() {
        let chunks = split_message("Hello world", MAX_MESSAGE_LENGTH);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_message_long() {
        let long = "a".repeat(10000);
        let chunks = split_message(&long, MAX_MESSAGE_LENGTH);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn test_render_mentions() {
        let mentions = vec![serde_json::json!({
            "start": 6,
            "length": 1,
            "number": "+1234567890"
        })];
        let text = "Hello \u{FFFC} world";
        let result = render_mentions(text, &mentions);
        assert!(result.contains("@+1234567890"));
        assert!(!result.contains("\u{FFFC}"));
    }

    #[test]
    fn test_guess_extension() {
        assert_eq!(guess_extension(b"\x89PNG\r\n\x1a\n"), ".png");
        assert_eq!(guess_extension(b"\xff\xd8\xff"), ".jpg");
        assert_eq!(guess_extension(b"GIF89a"), ".gif");
        assert_eq!(guess_extension(b"unknown"), ".bin");
    }

    #[test]
    fn test_ext_to_mime() {
        assert_eq!(ext_to_mime(".png"), "image/png");
        assert_eq!(ext_to_mime(".mp3"), "audio/mpeg");
        assert_eq!(ext_to_mime(".xyz"), "application/octet-stream");
    }

    #[test]
    fn test_config_from_env() {
        let cfg = SignalConfig::default();
        assert_eq!(cfg.phone_number, std::env::var("SIGNAL_PHONE_NUMBER").unwrap_or_default());
    }

    #[test]
    fn test_is_configured() {
        let mut cfg = SignalConfig::default();
        cfg.signal_http_url = "http://127.0.0.1:8080".to_string();
        cfg.phone_number = "+1234567890".to_string();
        assert!(cfg.is_configured());

        cfg.phone_number = "".to_string();
        assert!(!cfg.is_configured());
    }
}
