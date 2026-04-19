//! Discord platform adapter.
//!
//! Mirrors the Python `gateway/platforms/discord.py`.
//!
//! Uses Discord's Gateway WebSocket + REST API directly:
//! - Gateway WebSocket for receiving MESSAGE_CREATE events
//! - REST API for sending messages/embeds
//! - Supports text channels, DM channels, and threads
//!
//! Does NOT include voice support (see Python adapter for that).

use reqwest::Client;
use futures::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::dedup::MessageDeduplicator;

/// Discord API base URL.
const API_BASE: &str = "https://discord.com/api/v10";
/// Gateway version.
const GATEWAY_VERSION: u8 = 10;
/// Bot intents: GUILDS + GUILD_MESSAGES + DIRECT_MESSAGES + MESSAGE_CONTENT.
const INTENTS: u32 = 1 | 512 | 4096 | 32768;
/// Max message length for a single Discord message.
const MAX_MESSAGE_LENGTH: usize = 2000;

/// Discord platform configuration.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Bot token from Discord Developer Portal.
    pub bot_token: String,
    /// Optional: application ID for slash commands.
    pub application_id: Option<String>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            bot_token: std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default(),
            application_id: std::env::var("DISCORD_APPLICATION_ID").ok(),
        }
    }
}

impl DiscordConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Parsed media attachment from an inbound Discord message.
#[derive(Debug, Clone)]
pub struct DiscordAttachment {
    /// Attachment ID.
    pub id: String,
    /// File name.
    pub file_name: String,
    /// MIME type (if available).
    pub content_type: Option<String>,
    /// Download URL.
    pub url: String,
    /// File size in bytes.
    pub size: u64,
    /// Local filesystem path after download.
    pub local_path: Option<String>,
}

/// Inbound message event from Discord.
#[derive(Debug, Clone)]
pub struct DiscordMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Channel ID (where the message was sent).
    pub channel_id: String,
    /// Guild ID (if in a server; None for DMs).
    pub guild_id: Option<String>,
    /// Author user ID.
    pub author_id: String,
    /// Author display name.
    pub author_name: String,
    /// Message content.
    pub content: String,
    /// Whether this is a DM.
    pub is_dm: bool,
    /// Attachments.
    pub attachments: Vec<DiscordAttachment>,
}

/// Discord platform adapter.
pub struct DiscordAdapter {
    config: DiscordConfig,
    client: Client,
    dedup: MessageDeduplicator,
    /// Sequence number for gateway heartbeat.
    seq: Arc<parking_lot::Mutex<Option<u64>>>,
    /// Session ID for gateway resume.
    session_id: Arc<parking_lot::Mutex<Option<String>>>,
}

impl DiscordAdapter {
    pub fn new(config: DiscordConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: MessageDeduplicator::with_params(300, 2000),
            seq: Arc::new(parking_lot::Mutex::new(None)),
            session_id: Arc::new(parking_lot::Mutex::new(None)),
            config,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.config.bot_token)
    }

    // ── Gateway WebSocket ───────────────────────────────────────────────────

    /// Connect to Discord Gateway and process events.
    pub async fn run(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) {
        if self.config.bot_token.is_empty() {
            error!("Discord bot_token not configured");
            return;
        }

        // Get gateway URL
        let gateway_url = match self.get_gateway_url().await {
            Ok(url) => url,
            Err(e) => {
                error!("Failed to get Discord gateway URL: {e}");
                return;
            }
        };

        info!("Discord connecting to gateway: {gateway_url}");

        loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }

            match self
                .gateway_loop(&gateway_url, handler.clone(), running.clone())
                .await
            {
                Ok(()) => {
                    info!("Discord gateway loop ended cleanly");
                    break;
                }
                Err(e) => {
                    warn!("Discord gateway error: {e}, reconnecting in 5s...");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn get_gateway_url(&self) -> Result<String, String> {
        let resp = self
            .client
            .get(format!("{API_BASE}/gateway/bot"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("gateway/bot request failed: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("gateway/bot parse error: {e}"))?;

        let url = body
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("gateway/bot missing url")?;

        Ok(format!("{url}/?v={GATEWAY_VERSION}&encoding=json"))
    }

    async fn gateway_loop(
        &self,
        gateway_url: &str,
        handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) -> Result<(), String> {
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::{Message as WsMessage, error::Error as WsError};

        let (ws_stream, _) = connect_async(gateway_url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // Wait for Hello
        let hello: WsMessage = read
            .next()
            .await
            .ok_or("WebSocket closed before Hello")?
            .map_err(|e: WsError| format!("WebSocket error: {e}"))?;

        let hello_data: serde_json::Value = match hello {
            WsMessage::Text(text) => {
                serde_json::from_str(&text).map_err(|e| format!("Hello parse error: {e}"))?
            }
            _ => return Err("Expected text Hello message".to_string()),
        };

        let opcode = hello_data.get("op").and_then(|v| v.as_u64()).unwrap_or(0);
        if opcode != 10 {
            return Err(format!("Expected Hello (op 10), got op {opcode}"));
        }

        let heartbeat_interval = hello_data
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(|v| v.as_u64())
            .unwrap_or(45000);

        info!("Discord Gateway Hello, heartbeat_interval={heartbeat_interval}ms");

        // Send Identify
        let identify = serde_json::json!({
            "op": 2,
            "d": {
                "token": self.config.bot_token,
                "intents": INTENTS,
                "properties": {
                    "os": "linux",
                    "browser": "hermes-agent",
                    "device": "hermes-agent"
                }
            }
        });

        write
            .send(WsMessage::Text(identify.to_string().into()))
            .await
            .map_err(|e: WsError| format!("Identify send failed: {e}"))?;

        // Main read loop with heartbeat
        let mut last_heartbeat = Instant::now();
        let heartbeat_duration = Duration::from_millis(heartbeat_interval);

        while running.load(Ordering::SeqCst) {
            // Check if we need to send heartbeat
            if last_heartbeat.elapsed() >= heartbeat_duration {
                let s = *self.seq.lock();
                let heartbeat = serde_json::json!({"op": 1, "d": s});
                write
                    .send(WsMessage::Text(heartbeat.to_string().into()))
                    .await
                    .map_err(|e: WsError| format!("Heartbeat send failed: {e}"))?;
                last_heartbeat = Instant::now();
            }

            // Read with timeout
            let msg = tokio::time::timeout(Duration::from_secs(1), read.next()).await;

            match msg {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    let event: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|e| format!("Event parse error: {e}"))?;

                    if let Some(s) = event.get("s").and_then(|v| v.as_u64()) {
                        *self.seq.lock() = Some(s);
                    }

                    let opcode = event.get("op").and_then(|v| v.as_u64()).unwrap_or(0);
                    match opcode {
                        0 => {
                            // Dispatch
                            if let Some(t) = event.get("t").and_then(|v| v.as_str()) {
                                match t {
                                    "READY" => {
                                        if let Some(d) = event.get("d") {
                                            if let Some(session_id) =
                                                d.get("session_id").and_then(|v| v.as_str())
                                            {
                                                *self.session_id.lock() =
                                                    Some(session_id.to_string());
                                                info!("Discord Gateway READY");
                                            }
                                        }
                                    }
                                    "MESSAGE_CREATE" => {
                                        if let Some(d) = event.get("d") {
                                            if let Some(msg_event) = self.parse_message(d).await {
                                                let handler_guard = handler.lock().await;
                                                let handler_ref = handler_guard.as_ref().cloned();
                                                drop(handler_guard);

                                                if let Some(h) = handler_ref {
                                                    let chat_id = msg_event.channel_id.clone();
                                                    let content = msg_event.content.clone();
                                                    let platform = crate::config::Platform::Discord;
                                                    tokio::spawn(async move {
                                                        match h
                                                            .handle_message(platform, &chat_id, &content, None)
                                                            .await
                                                        {
                                                            Ok(result) => {
                                                                if !result.response.is_empty() {
                                                                    // We'll need the adapter to send — this is a limitation of the current architecture.
                                                                    // For now, log the response.
                                                                    info!("Discord response: {}", result.response.chars().take(100).collect::<String>());
                                                                }
                                                            }
                                                            Err(e) => {
                                                                error!("Discord handler error: {e}");
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        1 => {
                            // Heartbeat request — send heartbeat immediately
                            let s = *self.seq.lock();
                            let heartbeat = serde_json::json!({"op": 1, "d": s});
                            write
                                .send(WsMessage::Text(heartbeat.to_string().into()))
                                .await
                                .map_err(|e: WsError| format!("Heartbeat ack failed: {e}"))?;
                        }
                        7 => {
                            // Reconnect
                            warn!("Discord Gateway requested reconnect");
                            return Err("Reconnect requested".to_string());
                        }
                        9 => {
                            // Invalid session
                            warn!("Discord Gateway invalid session");
                            return Err("Invalid session".to_string());
                        }
                        11 => {
                            // Heartbeat ACK
                            debug!("Discord heartbeat ACK");
                        }
                        _ => {}
                    }
                }
                Ok(Some(Ok(WsMessage::Close(_frame)))) => {
                    info!("Discord Gateway closed");
                    return Ok(());
                }
                Ok(Some(Ok(_))) => {
                    // Ignore other message types (binary, ping, pong, frame)
                }
                Ok(Some(Err(e))) => {
                    return Err(format!("WebSocket error: {e}"));
                }
                Ok(None) => {
                    info!("Discord Gateway stream ended");
                    return Ok(());
                }
                Err(_) => {
                    // Timeout — check running flag and heartbeat
                }
            }
        }

        Ok(())
    }

    // ── Inbound message parsing ─────────────────────────────────────────────

    async fn parse_message(&self, msg: &serde_json::Value) -> Option<DiscordMessageEvent> {
        let message_id = msg.get("id").and_then(|v| v.as_str())?.to_string();

        // Skip bot's own messages
        if let Some(author) = msg.get("author") {
            if author.get("bot").and_then(|v| v.as_bool()).unwrap_or(false) {
                return None;
            }
        }

        let channel_id = msg.get("channel_id").and_then(|v| v.as_str())?.to_string();
        let guild_id = msg.get("guild_id").and_then(|v| v.as_str()).map(String::from);
        let is_dm = guild_id.is_none();

        let author_id = msg
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())?
            .to_string();
        let author_name = msg
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let mut content = msg
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Parse attachments
        let mut attachments = Vec::new();
        if let Some(att_arr) = msg.get("attachments").and_then(|v| v.as_array()) {
            for att in att_arr {
                let id = att.get("id").and_then(|v| v.as_str())?.to_string();
                let file_name = att
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let content_type = att
                    .get("content_type")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let url = att.get("url").and_then(|v| v.as_str())?.to_string();
                let size = att.get("size").and_then(|v| v.as_u64()).unwrap_or(0);

                let local_path = match self.download_attachment(&url, &file_name).await {
                    Ok(path) => Some(path),
                    Err(e) => {
                        warn!("Failed to download Discord attachment {id}: {e}");
                        None
                    }
                };

                if let Some(ref path) = local_path {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[attachment: {path}]"));
                }

                attachments.push(DiscordAttachment {
                    id,
                    file_name,
                    content_type,
                    url,
                    size,
                    local_path,
                });
            }
        }

        // Deduplication
        let dedup_key = format!("{channel_id}:{message_id}");
        if self.dedup.is_duplicate(&dedup_key) {
            return None;
        }
        self.dedup.insert(dedup_key);

        Some(DiscordMessageEvent {
            message_id,
            channel_id,
            guild_id,
            author_id,
            author_name,
            content,
            is_dm,
            attachments,
        })
    }

    async fn download_attachment(&self, url: &str, file_name: &str) -> Result<String, String> {
        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("download request failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("download bytes error: {e}"))?;

        let cache_dir = hermes_core::get_hermes_home().join("discord").join("media");
        tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| format!("mkdir failed: {e}"))?;

        let safe_name = file_name.replace('/', "_");
        let local_path = cache_dir.join(safe_name);

        tokio::fs::write(&local_path, bytes).await.map_err(|e| format!("write failed: {e}"))?;

        Ok(local_path.to_string_lossy().to_string())
    }

    // ── Outbound sending ────────────────────────────────────────────────────

    /// Send a text message to a Discord channel.
    pub async fn send_text(&self, channel_id: &str, text: &str) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        let chunks = split_discord_message(text);
        for chunk in chunks {
            self.send_message(channel_id, &chunk, None).await?;
        }
        Ok(())
    }

    /// Send a message with an embed.
    pub async fn send_embed(
        &self,
        channel_id: &str,
        title: &str,
        description: &str,
    ) -> Result<(), String> {
        let embed = serde_json::json!({
            "title": title,
            "description": description.chars().take(4096).collect::<String>(),
            "color": 0x3498db,
        });
        self.send_message(channel_id, "", Some(embed)).await
    }

    async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        embed: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let mut body = serde_json::json!({
            "content": content,
        });
        if let Some(e) = embed {
            body["embeds"] = serde_json::Value::Array(vec![e]);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("send message request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: serde_json::Value = resp
                .json()
                .await
                .unwrap_or_default();
            let msg = err_body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Discord API error {status}: {msg}"));
        }

        Ok(())
    }

    /// Send typing indicator to a channel.
    pub async fn send_typing(&self, channel_id: &str) -> Result<(), String> {
        let url = format!("{API_BASE}/channels/{channel_id}/typing");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("typing request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Discord typing error: {}", resp.status()));
        }
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a long message into chunks that fit within Discord's 2000 character limit.
fn split_discord_message(text: &str) -> Vec<String> {
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_discord_message_short() {
        let chunks = split_discord_message("Hello world");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_discord_message_long() {
        let long = "a".repeat(2500);
        let chunks = split_discord_message(&long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_LENGTH);
        }
    }

    #[test]
    fn test_discord_config_from_env() {
        let cfg = DiscordConfig::default();
        assert_eq!(cfg.bot_token, std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default());
    }
}
