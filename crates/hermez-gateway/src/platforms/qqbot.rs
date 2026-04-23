//! QQ Bot platform adapter.
//!
//! Mirrors Python `gateway/platforms/qqbot/adapter.py`.
//! Connects to the QQ Bot WebSocket Gateway for inbound events and uses the
//! REST API (`api.sgroup.qq.com`) for outbound messages and media uploads.
//!
//! Reference: https://bot.q.qq.com/wiki/develop/api-v2/

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::TimeZone;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

use base64::{engine::general_purpose, Engine as _};
use crate::platforms::helpers::strip_markdown;

// ── Constants ──────────────────────────────────────────────────────────────

const API_BASE: &str = "https://api.sgroup.qq.com";
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const GATEWAY_URL_PATH: &str = "/gateway";

const DEFAULT_API_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
const MAX_RECONNECT_ATTEMPTS: usize = 100;
const RATE_LIMIT_DELAY: u64 = 60;
const QUICK_DISCONNECT_THRESHOLD: f64 = 5.0;
const MAX_QUICK_DISCONNECT_COUNT: u32 = 3;

const MAX_MESSAGE_LENGTH: usize = 4000;
const DEDUP_TTL_SECONDS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;

const MSG_TYPE_TEXT: u32 = 0;
const MSG_TYPE_MARKDOWN: u32 = 2;
const MSG_TYPE_MEDIA: u32 = 7;
const MSG_TYPE_INPUT_NOTIFY: u32 = 6;

const MEDIA_TYPE_IMAGE: u32 = 1;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_TYPE_VOICE: u32 = 3;
const MEDIA_TYPE_FILE: u32 = 4;

// ── Configuration ──────────────────────────────────────────────────────────

/// QQ Bot platform configuration.
#[derive(Debug, Clone)]
pub struct QqbotConfig {
    /// Bot app ID.
    pub app_id: String,
    /// Bot client secret.
    pub client_secret: String,
    /// Enable markdown message type.
    pub markdown_support: bool,
    /// DM policy: `open`, `allowlist`, `disabled`.
    pub dm_policy: String,
    /// Allowed users for DM allowlist.
    pub allow_from: Vec<String>,
    /// Group policy: `open`, `allowlist`, `disabled`.
    pub group_policy: String,
    /// Allowed groups for group allowlist.
    pub group_allow_from: Vec<String>,
}

impl Default for QqbotConfig {
    fn default() -> Self {
        Self {
            app_id: std::env::var("QQ_APP_ID").unwrap_or_default(),
            client_secret: std::env::var("QQ_CLIENT_SECRET").unwrap_or_default(),
            markdown_support: true,
            dm_policy: "open".to_string(),
            allow_from: Vec::new(),
            group_policy: "open".to_string(),
            group_allow_from: Vec::new(),
        }
    }
}

impl QqbotConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

// ── Inbound message event ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct QqbotMessageEvent {
    pub message_id: String,
    pub chat_id: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub content: String,
    pub chat_type: String,
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub media_urls: Vec<String>,
    pub media_types: Vec<String>,
    pub raw: serde_json::Value,
}

// ── Adapter ────────────────────────────────────────────────────────────────

pub struct QqbotAdapter {
    config: QqbotConfig,
    client: Client,
    dedup: crate::dedup::MessageDeduplicator,

    // Token cache
    access_token: Mutex<Option<String>>,
    token_expires_at: Mutex<Instant>,

    // WS / session state
    heartbeat_interval: Mutex<f64>,
    session_id: Mutex<Option<String>>,
    last_seq: Mutex<Option<u64>>,

    // Chat metadata
    chat_type_map: Mutex<HashMap<String, String>>,
    last_msg_id: Mutex<HashMap<String, String>>,
    typing_sent_at: Mutex<HashMap<String, Instant>>,
}

impl QqbotAdapter {
    pub fn new(config: QqbotConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: crate::dedup::MessageDeduplicator::with_params(
                DEDUP_TTL_SECONDS,
                DEDUP_MAX_SIZE,
            ),
            access_token: Mutex::new(None),
            token_expires_at: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            heartbeat_interval: Mutex::new(30.0),
            session_id: Mutex::new(None),
            last_seq: Mutex::new(None),
            chat_type_map: Mutex::new(HashMap::new()),
            last_msg_id: Mutex::new(HashMap::new()),
            typing_sent_at: Mutex::new(HashMap::new()),
            config,
        }
    }

    // ── Public API: event stream ─────────────────────────────────────────

    /// Connect to QQ Bot Gateway and stream events via the provided sender.
    /// Handles authentication, WebSocket lifecycle, and auto-reconnect.
    pub async fn connect_and_listen(
        &self,
        running: std::sync::Arc<AtomicBool>,
        event_tx: mpsc::Sender<QqbotMessageEvent>,
    ) -> Result<(), String> {
        let mut backoff_idx = 0;
        let mut quick_disconnect_count = 0u32;

        while running.load(Ordering::SeqCst) {
            let connect_time = Instant::now();
            match self.run_once(running.clone(), event_tx.clone()).await {
                Ok(()) => {
                    info!("QQ Bot connection closed cleanly");
                    break;
                }
                Err((code, _reason)) => {
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }

                    let duration = connect_time.elapsed().as_secs_f64();
                    if duration < QUICK_DISCONNECT_THRESHOLD {
                        quick_disconnect_count += 1;
                        warn!(
                            "QQ Bot quick disconnect ({duration:.1}s), count: {quick_disconnect_count}"
                        );
                        if quick_disconnect_count >= MAX_QUICK_DISCONNECT_COUNT {
                            error!("QQ Bot: too many quick disconnects, stopping");
                            return Err("Too many quick disconnects".to_string());
                        }
                    } else {
                        quick_disconnect_count = 0;
                    }

                    if let Some(c) = code {
                        match c {
                            4914 | 4915 => {
                                error!("QQ Bot fatal close code {c}: bot offline/banned");
                                return Err(format!("Fatal close code {c}"));
                            }
                            4008 => {
                                info!("QQ Bot rate limited, waiting {RATE_LIMIT_DELAY}s");
                                tokio::time::sleep(Duration::from_secs(RATE_LIMIT_DELAY)).await;
                                continue;
                            }
                            4004 => {
                                info!("QQ Bot token invalid, clearing cache");
                                *self.access_token.lock().await = None;
                            }
                            4006 | 4007 | 4009 | 4900..=4913 => {
                                info!("QQ Bot session error {c}, clearing session");
                                *self.session_id.lock().await = None;
                                *self.last_seq.lock().await = None;
                            }
                            _ => {}
                        }
                    }

                    if backoff_idx >= MAX_RECONNECT_ATTEMPTS {
                        error!("QQ Bot max reconnect attempts reached");
                        return Err("Max reconnect attempts reached".to_string());
                    }

                    let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
                    info!(
                        "QQ Bot reconnecting in {delay}s (attempt {})...",
                        backoff_idx + 1
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    backoff_idx += 1;
                }
            }
        }

        Ok(())
    }

    // ── Single connection lifecycle ──────────────────────────────────────

    async fn run_once(
        &self,
        running: std::sync::Arc<AtomicBool>,
        event_tx: mpsc::Sender<QqbotMessageEvent>,
    ) -> Result<(), (Option<u16>, String)> {
        // 1. Ensure token
        self.ensure_token()
            .await
            .map_err(|e| (None, e))?;

        // 2. Get gateway URL
        let gateway_url = self
            .get_gateway_url()
            .await
            .map_err(|e| (None, e))?;
        info!("QQ Bot gateway URL: {gateway_url}");

        // 3. Connect WS
        let (ws_stream, _) = connect_async(&gateway_url)
            .await
            .map_err(|e| (None, format!("WS connect failed: {e}")))?;

        let (mut write, mut read) = ws_stream.split();
        info!("QQ Bot WebSocket connected");

        *self.heartbeat_interval.lock().await = 30.0;

        // 4. Wait for Hello
        let hello = tokio::time::timeout(Duration::from_secs(10), read.next())
            .await
            .map_err(|_| (None, "Hello timeout".to_string()))?
            .ok_or((None, "WS closed before Hello".to_string()))?
            .map_err(|e| (None, format!("WS error: {e}")))?;

        let hello_data: serde_json::Value = match hello {
            WsMessage::Text(text) => serde_json::from_str(&text)
                .map_err(|e| (None, format!("Hello parse error: {e}")))?,
            _ => return Err((None, "Expected text Hello".to_string())),
        };

        if hello_data.get("op").and_then(|v| v.as_u64()) != Some(10) {
            return Err((None, "Expected op 10 Hello".to_string()));
        }

        let interval_ms = hello_data
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(|v| v.as_u64())
            .unwrap_or(30000);
        let hb_interval = interval_ms as f64 / 1000.0 * 0.8;
        *self.heartbeat_interval.lock().await = hb_interval;
        info!("QQ Bot Hello received, heartbeat interval: {hb_interval:.1}s");

        // Send Identify or Resume
        let has_session = self.session_id.lock().await.is_some() && self.last_seq.lock().await.is_some();
        if has_session {
            self.send_resume(&mut write)
                .await
                .map_err(|e| (None, e))?;
        } else {
            self.send_identify(&mut write)
                .await
                .map_err(|e| (None, e))?;
        }

        // 5. Main loop
        let mut heartbeat_timer = tokio::time::interval(Duration::from_secs_f64(hb_interval));
        heartbeat_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while running.load(Ordering::SeqCst) {
            tokio::select! {
                msg = tokio::time::timeout(Duration::from_secs(60), read.next()) => {
                    match msg {
                        Ok(Some(Ok(WsMessage::Text(text)))) => {
                            if let Err(e) = self.handle_ws_text(&text, &event_tx).await {
                                warn!("QQ Bot WS handling error: {e}");
                            }
                        }
                        Ok(Some(Ok(WsMessage::Close(frame)))) => {
                            let code = frame.map(|f| f.code.into());
                            let reason = "WebSocket closed".to_string();
                            return Err((code, reason));
                        }
                        Ok(Some(Ok(WsMessage::Ping(_)))) => {}
                        Ok(Some(Ok(WsMessage::Binary(_)))) => {}
                        Ok(Some(Ok(WsMessage::Pong(_)))) => {}
                        Ok(Some(Ok(WsMessage::Frame(_)))) => {}
                        Ok(Some(Err(e))) => {
                            return Err((None, format!("WS read error: {e}")));
                        }
                        Ok(None) => {
                            return Err((None, "WS stream ended".to_string()));
                        }
                        Err(_) => {
                            if !running.load(Ordering::SeqCst) {
                                return Ok(());
                            }
                        }
                    }
                }
                _ = heartbeat_timer.tick() => {
                    if running.load(Ordering::SeqCst) {
                        if let Err(e) = self.send_heartbeat(&mut write).await {
                            return Err((None, format!("Heartbeat failed: {e}")));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // ── WebSocket helpers ────────────────────────────────────────────────

    async fn send_heartbeat<W>(&self, write: &mut W) -> Result<(), String>
    where
        W: futures::Sink<WsMessage> + Unpin,
        W::Error: std::fmt::Display,
    {
        let seq = *self.last_seq.lock().await;
        let payload = serde_json::json!({"op": 1, "d": seq});
        write
            .send(WsMessage::Text(payload.to_string().into()))
            .await
            .map_err(|e| format!("Heartbeat send failed: {e}"))?;
        debug!("QQ Bot heartbeat sent (seq={seq:?})");
        Ok(())
    }

    async fn send_identify<W>(&self, write: &mut W) -> Result<(), String>
    where
        W: futures::Sink<WsMessage> + Unpin,
        W::Error: std::fmt::Display,
    {
        let token = self
            .ensure_token()
            .await
            .map_err(|e| format!("Token error: {e}"))?;
        let payload = serde_json::json!({
            "op": 2,
            "d": {
                "token": format!("QQBot {token}"),
                "intents": (1 << 25) | (1 << 30) | (1 << 12),
                "shard": [0, 1],
                "properties": {
                    "$os": "macOS",
                    "$browser": "hermez-agent",
                    "$device": "hermez-agent",
                },
            },
        });
        write
            .send(WsMessage::Text(payload.to_string().into()))
            .await
            .map_err(|e| format!("Identify send failed: {e}"))?;
        info!("QQ Bot Identify sent");
        Ok(())
    }

    async fn send_resume<W>(&self, write: &mut W) -> Result<(), String>
    where
        W: futures::Sink<WsMessage> + Unpin,
        W::Error: std::fmt::Display,
    {
        let token = self
            .ensure_token()
            .await
            .map_err(|e| format!("Token error: {e}"))?;
        let session_id = self.session_id.lock().await.clone().unwrap_or_default();
        let seq = self.last_seq.lock().await.unwrap_or(0);
        let payload = serde_json::json!({
            "op": 6,
            "d": {
                "token": format!("QQBot {token}"),
                "session_id": session_id,
                "seq": seq,
            },
        });
        write
            .send(WsMessage::Text(payload.to_string().into()))
            .await
            .map_err(|e| format!("Resume send failed: {e}"))?;
        info!("QQ Bot Resume sent (session_id={session_id}, seq={seq})");
        Ok(())
    }

    async fn handle_ws_text(
        &self,
        text: &str,
        event_tx: &mpsc::Sender<QqbotMessageEvent>,
    ) -> Result<(), String> {
        let payload: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("JSON parse error: {e}"))?;

        let op = payload.get("op").and_then(|v| v.as_u64()).unwrap_or(99);
        let t = payload.get("t").and_then(|v| v.as_str());
        let s = payload.get("s").and_then(|v| v.as_u64());
        let d = payload.get("d").cloned().unwrap_or(serde_json::Value::Null);

        if let Some(seq) = s {
            let mut last_seq = self.last_seq.lock().await;
            if last_seq.is_none() || seq > last_seq.unwrap_or(0) {
                *last_seq = Some(seq);
            }
        }

        match op {
            0 => {
                // Dispatch
                if let Some(event_type) = t {
                    match event_type {
                        "READY" => {
                            if let Some(sid) = d.get("session_id").and_then(|v| v.as_str()) {
                                *self.session_id.lock().await = Some(sid.to_string());
                                info!("QQ Bot READY, session_id={sid}");
                            }
                        }
                        "RESUMED" => {
                            info!("QQ Bot session resumed");
                        }
                        "C2C_MESSAGE_CREATE"
                        | "GROUP_AT_MESSAGE_CREATE"
                        | "DIRECT_MESSAGE_CREATE"
                        | "GUILD_MESSAGE_CREATE"
                        | "GUILD_AT_MESSAGE_CREATE" => {
                            if let Some(event) = self.parse_message_event(event_type, &d).await {
                                let _ = event_tx.send(event).await;
                            }
                        }
                        _ => {
                            debug!("QQ Bot unhandled dispatch: {event_type}");
                        }
                    }
                }
            }
            11 => {
                // Heartbeat ACK
                debug!("QQ Bot heartbeat ACK");
            }
            _ => {
                debug!("QQ Bot unknown op: {op}");
            }
        }

        Ok(())
    }

    // ── Inbound message parsing ──────────────────────────────────────────

    async fn parse_message_event(
        &self,
        event_type: &str,
        d: &serde_json::Value,
    ) -> Option<QqbotMessageEvent> {
        let msg_id = d.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if msg_id.is_empty() || self.is_duplicate(&msg_id) {
            debug!("QQ Bot duplicate or missing message id: {msg_id}");
            return None;
        }

        let content = d.get("content").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let timestamp_str = d.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = self.parse_qq_timestamp(timestamp_str);
        let author = d.get("author").and_then(|v| v.as_object()).cloned().unwrap_or_default();

        let event = match event_type {
            "C2C_MESSAGE_CREATE" => {
                let user_openid = author.get("user_openid").and_then(|v| v.as_str()).unwrap_or("");
                if user_openid.is_empty() || !self.is_dm_allowed(user_openid) {
                    return None;
                }
                let (text, image_urls, image_media_types) = self.process_attachments(d).await;
                if text.is_empty() && image_urls.is_empty() {
                    return None;
                }
                self.chat_type_map
                    .lock()
                    .await
                    .insert(user_openid.to_string(), "c2c".to_string());
                QqbotMessageEvent {
                    message_id: msg_id.clone(),
                    chat_id: user_openid.to_string(),
                    user_id: user_openid.to_string(),
                    user_name: None,
                    content: text,
                    chat_type: "dm".to_string(),
                    timestamp,
                    media_urls: image_urls,
                    media_types: image_media_types,
                    raw: d.clone(),
                }
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                let group_openid = d.get("group_openid").and_then(|v| v.as_str()).unwrap_or("");
                let member_openid = author.get("member_openid").and_then(|v| v.as_str()).unwrap_or("");
                if group_openid.is_empty() || !self.is_group_allowed(group_openid, member_openid) {
                    return None;
                }
                let text = self.strip_at_mention(&content);
                let (text, image_urls, image_media_types) = self.process_attachments_with_text(d, text).await;
                if text.is_empty() && image_urls.is_empty() {
                    return None;
                }
                self.chat_type_map
                    .lock()
                    .await
                    .insert(group_openid.to_string(), "group".to_string());
                QqbotMessageEvent {
                    message_id: msg_id.clone(),
                    chat_id: group_openid.to_string(),
                    user_id: member_openid.to_string(),
                    user_name: None,
                    content: text,
                    chat_type: "group".to_string(),
                    timestamp,
                    media_urls: image_urls,
                    media_types: image_media_types,
                    raw: d.clone(),
                }
            }
            "GUILD_MESSAGE_CREATE" | "GUILD_AT_MESSAGE_CREATE" => {
                let channel_id = d.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
                if channel_id.is_empty() {
                    return None;
                }
                let member = d.get("member").and_then(|v| v.as_object()).cloned().unwrap_or_default();
                let nick = member
                    .get("nick")
                    .and_then(|v| v.as_str())
                    .unwrap_or(author.get("username").and_then(|v| v.as_str()).unwrap_or(""));
                let (text, image_urls, image_media_types) = self.process_attachments_with_text(d, content).await;
                if text.is_empty() && image_urls.is_empty() {
                    return None;
                }
                self.chat_type_map
                    .lock()
                    .await
                    .insert(channel_id.to_string(), "guild".to_string());
                QqbotMessageEvent {
                    message_id: msg_id.clone(),
                    chat_id: channel_id.to_string(),
                    user_id: author.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    user_name: Some(nick.to_string()).filter(|s| !s.is_empty()),
                    content: text,
                    chat_type: "group".to_string(),
                    timestamp,
                    media_urls: image_urls,
                    media_types: image_media_types,
                    raw: d.clone(),
                }
            }
            "DIRECT_MESSAGE_CREATE" => {
                let guild_id = d.get("guild_id").and_then(|v| v.as_str()).unwrap_or("");
                if guild_id.is_empty() {
                    return None;
                }
                let (text, image_urls, image_media_types) = self.process_attachments_with_text(d, content).await;
                if text.is_empty() && image_urls.is_empty() {
                    return None;
                }
                self.chat_type_map
                    .lock()
                    .await
                    .insert(guild_id.to_string(), "dm".to_string());
                QqbotMessageEvent {
                    message_id: msg_id.clone(),
                    chat_id: guild_id.to_string(),
                    user_id: author.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    user_name: None,
                    content: text,
                    chat_type: "dm".to_string(),
                    timestamp,
                    media_urls: image_urls,
                    media_types: image_media_types,
                    raw: d.clone(),
                }
            }
            _ => return None,
        };

        // Cache last message ID per chat for typing indicator
        self.last_msg_id
            .lock()
            .await
            .insert(event.chat_id.clone(), msg_id);

        Some(event)
    }

    async fn process_attachments(
        &self,
        d: &serde_json::Value,
    ) -> (String, Vec<String>, Vec<String>) {
        self.process_attachments_with_text(d, String::new()).await
    }

    async fn process_attachments_with_text(
        &self,
        d: &serde_json::Value,
        mut text: String,
    ) -> (String, Vec<String>, Vec<String>) {
        let attachments = d.get("attachments").and_then(|v| v.as_array());
        let mut image_urls = Vec::new();
        let mut image_media_types = Vec::new();
        let mut other_info = Vec::new();

        if let Some(atts) = attachments {
            for att in atts {
                let ct = att
                    .get("content_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let url_raw = att
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let filename = att
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let url = if url_raw.starts_with("//") {
                    format!("https:{url_raw}")
                } else {
                    url_raw.to_string()
                };
                if url.is_empty() {
                    continue;
                }

                if self.is_voice_content_type(&ct, &filename) {
                    // Voice/STT: TODO stub — try QQ built-in ASR first
                    let asr_text = att
                        .get("asr_refer_text")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if let Some(transcript) = asr_text {
                        other_info.push(format!("[Voice] {transcript}"));
                    } else {
                        other_info.push("[Voice] [语音识别失败]".to_string());
                    }
                } else if ct.starts_with("image/") {
                    image_urls.push(url);
                    image_media_types.push(ct.clone());
                } else {
                    other_info.push(format!("[Attachment: {}]", filename));
                }
            }
        }

        if !other_info.is_empty() {
            let block = other_info.join("\n");
            if text.trim().is_empty() {
                text = block;
            } else {
                text = format!("{text}\n\n{block}");
            }
        }

        (text, image_urls, image_media_types)
    }

    fn is_voice_content_type(&self, content_type: &str, filename: &str) -> bool {
        let ct = content_type.to_lowercase();
        let fn_lower = filename.to_lowercase();
        if ct == "voice" || ct.starts_with("audio/") {
            return true;
        }
        const VOICE_EXTS: &[&str] = &[
            ".silk", ".amr", ".mp3", ".wav", ".ogg", ".m4a", ".aac", ".speex", ".flac",
        ];
        VOICE_EXTS.iter().any(|ext| fn_lower.ends_with(ext))
    }

    fn is_duplicate(&self, msg_id: &str) -> bool {
        if self.dedup.is_duplicate(msg_id) {
            return true;
        }
        self.dedup.insert(msg_id.to_string());
        false
    }

    fn strip_at_mention(&self, content: &str) -> String {
        static AT_MENTION_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = AT_MENTION_RE.get_or_init(|| {
            regex::Regex::new(r"^@\S+\s*").unwrap_or_else(|_| regex::Regex::new("").unwrap())
        });
        re.replace(content.trim(), "").to_string()
    }

    fn is_dm_allowed(&self, user_id: &str) -> bool {
        match self.config.dm_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.entry_matches(&self.config.allow_from, user_id),
            _ => true,
        }
    }

    fn is_group_allowed(&self, group_id: &str, _user_id: &str) -> bool {
        match self.config.group_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.entry_matches(&self.config.group_allow_from, group_id),
            _ => true,
        }
    }

    fn entry_matches(&self, entries: &[String], target: &str) -> bool {
        let target_lower = target.trim().to_lowercase();
        entries.iter().any(|e| {
            let el = e.trim().to_lowercase();
            el == "*" || el == target_lower
        })
    }

    fn parse_qq_timestamp(
        &self,
        raw: &str,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if raw.is_empty() {
            return Some(chrono::Utc::now());
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Some(dt.with_timezone(&chrono::Utc));
        }
        if let Ok(ts) = raw.parse::<i64>() {
            return chrono::Utc.timestamp_millis_opt(ts).single();
        }
        Some(chrono::Utc::now())
    }

    // ── Outbound messaging ───────────────────────────────────────────────

    /// Send a text message to a chat.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let formatted = self.format_message(text);
        let chunks = split_qq_message(&formatted);
        for chunk in chunks {
            self.send_text_chunk(chat_id, &chunk).await?;
        }
        Ok(())
    }

    async fn send_text_chunk(&self, chat_id: &str, text: &str) -> Result<(), String> {
        let chat_type = self.guess_chat_type(chat_id).await;
        let body = self.build_text_body(text);

        let path = match chat_type.as_str() {
            "c2c" => format!("/v2/users/{chat_id}/messages"),
            "group" => format!("/v2/groups/{chat_id}/messages"),
            "guild" => format!("/channels/{chat_id}/messages"),
            _ => return Err(format!("Unknown chat type for {chat_id}")),
        };

        self.api_request("POST", &path, Some(body)).await?;
        Ok(())
    }

    /// Send an image natively.
    pub async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        _caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, image_url, MEDIA_TYPE_IMAGE, "image")
            .await
    }

    /// Send a local image file natively.
    pub async fn send_image_file(
        &self,
        chat_id: &str,
        image_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, image_path, MEDIA_TYPE_IMAGE, "image")
            .await
    }

    /// Send a voice message natively.
    pub async fn send_voice(
        &self,
        chat_id: &str,
        audio_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, audio_path, MEDIA_TYPE_VOICE, "voice")
            .await
    }

    /// Send a video natively.
    pub async fn send_video(
        &self,
        chat_id: &str,
        video_path: &str,
        _caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, video_path, MEDIA_TYPE_VIDEO, "video")
            .await
    }

    /// Send a document/file natively.
    pub async fn send_document(
        &self,
        chat_id: &str,
        file_path: &str,
        _caption: Option<&str>,
        _file_name: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, file_path, MEDIA_TYPE_FILE, "file")
            .await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media_source: &str,
        file_type: u32,
        _kind: &str,
    ) -> Result<(), String> {
        let chat_type = self.guess_chat_type(chat_id).await;
        if chat_type == "guild" {
            return Err("Guild media send not supported via this path".to_string());
        }

        let (data, _content_type, resolved_name) = self.load_media(media_source, None).await?;
        let is_url = is_url(media_source);

        let upload_path = if chat_type == "c2c" {
            format!("/v2/users/{chat_id}/files")
        } else {
            format!("/v2/groups/{chat_id}/files")
        };

        let mut upload_body = serde_json::json!({
            "file_type": file_type,
            "srv_send_msg": false,
        });
        if is_url {
            upload_body["url"] = serde_json::Value::String(media_source.to_string());
        } else {
            upload_body["file_data"] = serde_json::Value::String(data);
        }
        if file_type == MEDIA_TYPE_FILE {
            upload_body["file_name"] = serde_json::Value::String(resolved_name);
        }

        let upload_resp = self
            .api_request("POST", &upload_path, Some(upload_body))
            .await?;
        let file_info = upload_resp
            .get("file_info")
            .and_then(|v| v.as_str())
            .ok_or("Upload returned no file_info")?;

        let msg_seq = self.next_msg_seq(chat_id);
        let send_body = serde_json::json!({
            "msg_type": MSG_TYPE_MEDIA,
            "media": {"file_info": file_info},
            "msg_seq": msg_seq,
        });

        let send_path = if chat_type == "c2c" {
            format!("/v2/users/{chat_id}/messages")
        } else {
            format!("/v2/groups/{chat_id}/messages")
        };

        self.api_request("POST", &send_path, Some(send_body)).await?;
        Ok(())
    }

    /// Send typing/input notify (C2C only).
    pub async fn send_typing(&self, chat_id: &str) -> Result<(), String> {
        let chat_type = self.guess_chat_type(chat_id).await;
        if chat_type != "c2c" {
            return Ok(());
        }
        let msg_id = self
            .last_msg_id
            .lock()
            .await
            .get(chat_id)
            .cloned()
            .unwrap_or_default();
        if msg_id.is_empty() {
            return Ok(());
        }

        // Debounce: 50s
        {
            let mut map = self.typing_sent_at.lock().await;
            if let Some(last) = map.get(chat_id) {
                if last.elapsed() < Duration::from_secs(50) {
                    return Ok(());
                }
            }
            map.insert(chat_id.to_string(), Instant::now());
        }

        let msg_seq = self.next_msg_seq(chat_id);
        let body = serde_json::json!({
            "msg_type": MSG_TYPE_INPUT_NOTIFY,
            "msg_id": msg_id,
            "input_notify": {
                "input_type": 1,
                "input_second": 60,
            },
            "msg_seq": msg_seq,
        });

        let path = format!("/v2/users/{chat_id}/messages");
        self.api_request("POST", &path, Some(body)).await?;
        Ok(())
    }

    // ── REST helpers ─────────────────────────────────────────────────────

    async fn ensure_token(&self) -> Result<String, String> {
        let now = Instant::now();
        {
            let token = self.access_token.lock().await;
            let expires = self.token_expires_at.lock().await;
            if token.is_some() && now < *expires - Duration::from_secs(60) {
                return Ok(token.clone().unwrap_or_default());
            }
        }

        let resp = self
            .client
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "appId": self.config.app_id,
                "clientSecret": self.config.client_secret,
            }))
            .timeout(DEFAULT_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("Token request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Token parse error: {e}"))?;

        let token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Token missing in response: {data}"))?;
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);

        *self.access_token.lock().await = Some(token.to_string());
        *self.token_expires_at.lock().await = now + Duration::from_secs(expires_in);
        info!("QQ Bot access token refreshed, expires in {expires_in}s");
        Ok(token.to_string())
    }

    async fn get_gateway_url(&self) -> Result<String, String> {
        let token = self.ensure_token().await?;
        let resp = self
            .client
            .get(format!("{API_BASE}{GATEWAY_URL_PATH}"))
            .header("Authorization", format!("QQBot {token}"))
            .timeout(DEFAULT_API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("Gateway request failed: {e}"))?;

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Gateway parse error: {e}"))?;

        data.get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Gateway URL missing: {data}"))
    }

    async fn api_request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let token = self.ensure_token().await?;
        let url = format!("{API_BASE}{path}");

        let http_method = reqwest::Method::from_bytes(method.as_bytes())
            .unwrap_or(reqwest::Method::POST);
        let mut req = self
            .client
            .request(http_method, &url)
            .header("Authorization", format!("QQBot {token}"))
            .header("Content-Type", "application/json");

        if let Some(b) = body {
            req = req.json(&b);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("API request failed: {e}"))?;

        let status = resp.status();
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("API parse error: {e}"))?;

        if status.is_client_error() || status.is_server_error() {
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("QQ Bot API error [{status}] {path}: {msg}"));
        }

        Ok(data)
    }

    // ── Format / helpers ─────────────────────────────────────────────────

    fn format_message(&self, content: &str) -> String {
        if self.config.markdown_support {
            content.to_string()
        } else {
            strip_markdown(content)
        }
    }

    fn build_text_body(&self, content: &str) -> serde_json::Value {
        let msg_seq = self.next_msg_seq("default");
        let truncated = if content.chars().count() > MAX_MESSAGE_LENGTH {
            content.chars().take(MAX_MESSAGE_LENGTH).collect()
        } else {
            content.to_string()
        };

        if self.config.markdown_support {
            serde_json::json!({
                "markdown": {"content": truncated},
                "msg_type": MSG_TYPE_MARKDOWN,
                "msg_seq": msg_seq,
            })
        } else {
            serde_json::json!({
                "content": truncated,
                "msg_type": MSG_TYPE_TEXT,
                "msg_seq": msg_seq,
            })
        }
    }

    fn next_msg_seq(&self, _seed: &str) -> u32 {
        let time_part = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32)
            % 100000000;
        let rand = uuid::Uuid::new_v4().as_u128() as u32;
        (time_part ^ rand) % 65536
    }

    async fn guess_chat_type(&self, chat_id: &str) -> String {
        self.chat_type_map
            .lock()
            .await
            .get(chat_id)
            .cloned()
            .unwrap_or_else(|| "c2c".to_string())
    }

    async fn load_media(
        &self,
        source: &str,
        file_name: Option<&str>,
    ) -> Result<(String, String, String), String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("Media source is required".to_string());
        }

        if is_url(source) {
            let ct = guess_mime_from_path(source);
            let name = file_name
                .map(|s| s.to_string())
                .or_else(|| std::path::Path::new(source).file_name().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_else(|| "media".to_string());
            return Ok((source.to_string(), ct, name));
        }

        let path = std::path::Path::new(source);
        if !path.exists() || !path.is_file() {
            if source.starts_with("<") || source.len() < 3 {
                return Err(format!("Invalid media source (placeholder): {source:?}"));
            }
            return Err(format!("Media file not found: {source}"));
        }

        let raw = std::fs::read(path).map_err(|e| format!("Read media failed: {e}"))?;
        let b64 = general_purpose::STANDARD.encode(&raw);
        let ct = guess_mime_from_path(source);
        let name = file_name
            .map(|s| s.to_string())
            .or_else(|| path.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "file".to_string());
        Ok((b64, ct, name))
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

fn split_qq_message(text: &str) -> Vec<String> {
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

fn guess_mime_from_path(path: &str) -> String {
    let lower = path.to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".mp4") {
        "video/mp4".to_string()
    } else if lower.ends_with(".mov") {
        "video/quicktime".to_string()
    } else if lower.ends_with(".mp3") {
        "audio/mpeg".to_string()
    } else if lower.ends_with(".wav") {
        "audio/wav".to_string()
    } else if lower.ends_with(".ogg") {
        "audio/ogg".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else if lower.ends_with(".txt") {
        "text/plain".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> QqbotConfig {
        QqbotConfig {
            app_id: "test_app".into(),
            client_secret: "test_secret".into(),
            markdown_support: false,
            dm_policy: "open".into(),
            allow_from: vec![],
            group_policy: "open".into(),
            group_allow_from: vec![],
        }
    }

    fn test_adapter() -> QqbotAdapter {
        QqbotAdapter::new(test_config())
    }

    #[test]
    fn test_split_qq_message_short() {
        let text = "Hello world";
        let chunks = split_qq_message(text);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn test_split_qq_message_exact_limit() {
        let text = "a".repeat(MAX_MESSAGE_LENGTH);
        let chunks = split_qq_message(&text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_split_qq_message_over_limit_splits_at_newline() {
        let part1 = "Line1\n".repeat(MAX_MESSAGE_LENGTH / 6);
        let text = format!("{}\n{}more", part1, "x".repeat(MAX_MESSAGE_LENGTH));
        let chunks = split_qq_message(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn test_guess_mime_from_path() {
        assert_eq!(guess_mime_from_path("photo.jpg"), "image/jpeg");
        assert_eq!(guess_mime_from_path("image.PNG"), "image/png");
        assert_eq!(guess_mime_from_path("doc.pdf"), "application/pdf");
        assert_eq!(guess_mime_from_path("song.mp3"), "audio/mpeg");
        assert_eq!(guess_mime_from_path("video.mp4"), "video/mp4");
        assert_eq!(guess_mime_from_path("unknown.xyz"), "application/octet-stream");
    }

    #[test]
    fn test_is_url() {
        assert!(is_url("https://example.com"));
        assert!(is_url("http://localhost:8080"));
        assert!(!is_url("/path/to/file"));
        assert!(!is_url("just text"));
    }

    #[test]
    fn test_strip_at_mention() {
        let adapter = test_adapter();
        assert_eq!(adapter.strip_at_mention("@bot hello"), "hello");
        assert_eq!(adapter.strip_at_mention("  @bot   hello  "), "hello");
        assert_eq!(adapter.strip_at_mention("no mention"), "no mention");
    }

    #[test]
    fn test_is_dm_allowed_open() {
        let adapter = test_adapter();
        assert!(adapter.is_dm_allowed("any_user"));
    }

    #[test]
    fn test_is_dm_allowed_disabled() {
        let mut config = test_config();
        config.dm_policy = "disabled".into();
        let adapter = QqbotAdapter::new(config);
        assert!(!adapter.is_dm_allowed("any_user"));
    }

    #[test]
    fn test_is_dm_allowed_allowlist() {
        let mut config = test_config();
        config.dm_policy = "allowlist".into();
        config.allow_from = vec!["user1".into(), "user2".into()];
        let adapter = QqbotAdapter::new(config);
        assert!(adapter.is_dm_allowed("user1"));
        assert!(!adapter.is_dm_allowed("user3"));
    }

    #[test]
    fn test_entry_matches_wildcard() {
        let adapter = test_adapter();
        assert!(adapter.entry_matches(&["*".into()], "anything"));
        assert!(adapter.entry_matches(&["Target".into()], "target"));
        assert!(!adapter.entry_matches(&["other".into()], "target"));
    }

    #[test]
    fn test_parse_qq_timestamp_rfc3339() {
        let adapter = test_adapter();
        let dt = adapter.parse_qq_timestamp("2024-01-15T10:30:00Z");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_qq_timestamp_millis() {
        let adapter = test_adapter();
        let dt = adapter.parse_qq_timestamp("1705315800000");
        assert!(dt.is_some());
    }

    #[test]
    fn test_parse_qq_timestamp_empty() {
        let adapter = test_adapter();
        let dt = adapter.parse_qq_timestamp("");
        assert!(dt.is_some());
    }

    #[test]
    fn test_build_text_body_plain() {
        let adapter = test_adapter();
        let body = adapter.build_text_body("hello");
        assert_eq!(body["content"], "hello");
        assert_eq!(body["msg_type"], MSG_TYPE_TEXT);
    }

    #[test]
    fn test_build_text_body_markdown() {
        let mut config = test_config();
        config.markdown_support = true;
        let adapter = QqbotAdapter::new(config);
        let body = adapter.build_text_body("hello");
        assert_eq!(body["msg_type"], MSG_TYPE_MARKDOWN);
        assert_eq!(body["markdown"]["content"], "hello");
    }

    #[test]
    fn test_build_text_body_truncates() {
        let adapter = test_adapter();
        let long = "x".repeat(MAX_MESSAGE_LENGTH + 100);
        let body = adapter.build_text_body(&long);
        let content = body["content"].as_str().unwrap();
        assert_eq!(content.chars().count(), MAX_MESSAGE_LENGTH);
    }

    #[test]
    fn test_is_group_allowed() {
        let mut config = test_config();
        config.group_policy = "allowlist".into();
        config.group_allow_from = vec!["group1".into()];
        let adapter = QqbotAdapter::new(config);
        assert!(adapter.is_group_allowed("group1", "user"));
        assert!(!adapter.is_group_allowed("group2", "user"));
    }
}
