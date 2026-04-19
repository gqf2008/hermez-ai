#![allow(dead_code)]
//! WhatsApp platform adapter.
//!
//! Mirrors Python `gateway/platforms/whatsapp.py`.
//!
//! Uses a Node.js bridge process (Baileys-based) that runs a local HTTP
//! server. The Rust adapter communicates with the bridge via HTTP:
//! - `GET  /health`     — check bridge + WhatsApp connection status
//! - `GET  /messages`   — long-poll for incoming messages
//! - `POST /send`       — send a text message
//! - `POST /edit`       — edit a sent message
//! - `POST /send-media` — send image/video/document/audio
//! - `POST /typing`     — send typing indicator
//! - `GET  /chat/:id`   — get chat info
//!
//! The bridge script is expected at `scripts/whatsapp-bridge/bridge.js`
//! relative to the project root, or via `WHATSAPP_BRIDGE_SCRIPT` env var.

use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::dedup::MessageDeduplicator;

/// WhatsApp message limits — practical UX limit.
const MAX_MESSAGE_LENGTH: usize = 4096;
/// Default bridge port.
const DEFAULT_BRIDGE_PORT: u16 = 3000;
/// Poll interval for incoming messages.
const POLL_INTERVAL_SECS: u64 = 1;
/// HTTP request timeout.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Bridge startup timeout.
const BRIDGE_STARTUP_TIMEOUT_SECS: u64 = 30;

/// WhatsApp platform configuration.
#[derive(Debug, Clone)]
pub struct WhatsAppConfig {
    /// Path to the Node.js bridge script.
    pub bridge_script: String,
    /// Port for HTTP communication with the bridge.
    pub bridge_port: u16,
    /// Path to store WhatsApp session data.
    pub session_path: PathBuf,
    /// Optional prefix prepended to outgoing messages.
    pub reply_prefix: Option<String>,
    /// Whether to require @mention in group chats.
    pub require_mention: bool,
    /// Comma-separated list of chat IDs that don't require mention.
    pub free_response_chats: Vec<String>,
    /// Optional regex patterns for mention detection.
    pub mention_patterns: Vec<String>,
}

impl Default for WhatsAppConfig {
    fn default() -> Self {
        let session_path = hermes_core::get_hermes_home().join("whatsapp").join("session");
        Self {
            bridge_script: std::env::var("WHATSAPP_BRIDGE_SCRIPT")
                .unwrap_or_else(|_| default_bridge_script()),
            bridge_port: std::env::var("WHATSAPP_BRIDGE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_BRIDGE_PORT),
            session_path,
            reply_prefix: std::env::var("WHATSAPP_REPLY_PREFIX").ok(),
            require_mention: std::env::var("WHATSAPP_REQUIRE_MENTION")
                .map(|v| v.to_lowercase() == "true" || v == "1")
                .unwrap_or(false),
            free_response_chats: std::env::var("WHATSAPP_FREE_RESPONSE_CHATS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            mention_patterns: Vec::new(),
        }
    }
}

impl WhatsAppConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

fn default_bridge_script() -> String {
    // Try to find bridge.js relative to the executable or current dir
    let candidates = [
        PathBuf::from("scripts/whatsapp-bridge/bridge.js"),
        PathBuf::from("../scripts/whatsapp-bridge/bridge.js"),
        PathBuf::from("../../scripts/whatsapp-bridge/bridge.js"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }
    candidates[0].to_string_lossy().to_string()
}

/// Inbound message event from WhatsApp.
#[derive(Debug, Clone)]
pub struct WhatsAppMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Chat ID (e.g., "1234567890@s.whatsapp.net" or "1234567890@g.us").
    pub chat_id: String,
    /// Sender ID.
    pub sender_id: String,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// Chat name (group name or contact name).
    pub chat_name: Option<String>,
    /// Whether the chat is a group.
    pub is_group: bool,
    /// Message content text.
    pub content: String,
    /// Message type: text, image, video, voice, document.
    pub msg_type: String,
    /// Cached local paths for media attachments.
    pub media_paths: Vec<String>,
    /// MIME types for media attachments.
    pub media_types: Vec<String>,
    /// Quoted message ID (for replies).
    pub quoted_message_id: Option<String>,
    /// IDs mentioned in the message.
    pub mentioned_ids: Vec<String>,
    /// Bot IDs detected in the message (for mention filtering).
    pub bot_ids: Vec<String>,
}

/// WhatsApp platform adapter.
pub struct WhatsAppAdapter {
    config: WhatsAppConfig,
    client: reqwest::Client,
    /// Bridge HTTP base URL.
    bridge_url: String,
    /// Managed bridge child process (None if external).
    bridge_process: RwLock<Option<Child>>,
    /// Deduplication cache.
    dedup: MessageDeduplicator,
    /// Whether the adapter is connected.
    connected: AtomicBool,
    /// Compiled mention regex patterns.
    mention_regexes: Vec<Regex>,
    /// Bridge log file path.
    bridge_log: Option<PathBuf>,
}

impl WhatsAppAdapter {
    pub fn new(config: WhatsAppConfig) -> Self {
        let mention_regexes: Vec<Regex> = config
            .mention_patterns
            .iter()
            .filter_map(|p| match Regex::new(&format!("(?i){}", regex::escape(p))) {
                Ok(re) => Some(re),
                Err(e) => {
                    warn!("Invalid WhatsApp mention pattern {:?}: {}", p, e);
                    None
                }
            })
            .collect();

        Self {
            bridge_url: format!("http://127.0.0.1:{}", config.bridge_port),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    reqwest::Client::new()
                }),
            bridge_process: RwLock::new(None),
            dedup: MessageDeduplicator::with_params(300, 2000),
            connected: AtomicBool::new(false),
            mention_regexes,
            bridge_log: None,
            config,
        }
    }

    /// Check if Node.js is available.
    pub fn check_requirements() -> bool {
        match std::process::Command::new("node")
            .arg("--version")
            .output()
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Start the WhatsApp bridge and connect.
    pub async fn connect(&self) -> Result<(), String> {
        if !Self::check_requirements() {
            return Err("Node.js not found. WhatsApp requires Node.js.".into());
        }

        let bridge_path = PathBuf::from(&self.config.bridge_script);
        if !bridge_path.exists() {
            return Err(format!(
                "Bridge script not found: {}",
                bridge_path.display()
            ));
        }

        info!("WhatsApp bridge found at {}", bridge_path.display());

        // Ensure session directory exists
        if let Err(e) = tokio::fs::create_dir_all(&self.config.session_path).await {
            return Err(format!("Failed to create session dir: {e}"));
        }

        // Check if an existing bridge is already running and connected
        match self.check_health().await {
            Ok(HealthStatus::Connected) => {
                info!("Using existing WhatsApp bridge (connected)");
                self.connected.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Ok(other) => {
                info!("Existing bridge found but not connected ({:?}), restarting", other);
            }
            Err(_) => {
                // Bridge not running, start a new one
            }
        }

        // Kill any orphaned bridge on the port
        self.kill_port_process(self.config.bridge_port).await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Auto-install npm dependencies if needed
        let bridge_dir = bridge_path.parent().unwrap_or(Path::new("."));
        let node_modules = bridge_dir.join("node_modules");
        if !node_modules.exists() {
            info!("Installing WhatsApp bridge dependencies...");
            let install_result = tokio::process::Command::new("npm")
                .args(["install", "--silent"])
                .current_dir(bridge_dir)
                .output()
                .await
                .map_err(|e| format!("npm install failed: {e}"))?;
            if !install_result.status.success() {
                let stderr = String::from_utf8_lossy(&install_result.stderr);
                return Err(format!("npm install failed: {stderr}"));
            }
            info!("Dependencies installed");
        }

        // Start the bridge process
        let bridge_log = self.config.session_path.parent()
            .unwrap_or(&self.config.session_path)
            .join("bridge.log");

        let mut cmd = tokio::process::Command::new("node");
        cmd.arg(&bridge_path)
            .arg("--port")
            .arg(self.config.bridge_port.to_string())
            .arg("--session")
            .arg(&self.config.session_path)
            .arg("--mode")
            .arg(std::env::var("WHATSAPP_MODE").unwrap_or_else(|_| "self-chat".into()))
            .stdout({
                let file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&bridge_log)
                    .await
                    .map_err(|e| format!("Failed to open bridge log: {e}"))?;
                Stdio::from(file.into_std().await)
            })
            .stderr({
                let file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&bridge_log)
                    .await
                    .map_err(|e| format!("Failed to open bridge log: {e}"))?;
                Stdio::from(file.into_std().await)
            });

        // Pass reply prefix via env
        if let Some(ref prefix) = self.config.reply_prefix {
            cmd.env("WHATSAPP_REPLY_PREFIX", prefix);
        }

        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn bridge process: {e}"))?;

        *self.bridge_process.write().await = Some(child);

        // Wait for bridge to start
        let deadline = Instant::now() + Duration::from_secs(BRIDGE_STARTUP_TIMEOUT_SECS);
        let mut http_ready = false;
        let mut connected = false;

        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Check if process died
            {
                let mut proc_lock = self.bridge_process.write().await;
                if let Some(ref mut proc) = *proc_lock {
                    match proc.try_wait() {
                        Ok(Some(status)) => {
                            return Err(format!(
                                "Bridge process exited unexpectedly (code: {}). Check log: {}",
                                status,
                                bridge_log.display()
                            ));
                        }
                        Ok(None) => {}
                        Err(e) => warn!("Error checking bridge process: {e}"),
                    }
                }
            }

            match self.check_health().await {
                Ok(HealthStatus::Connected) => {
                    info!("WhatsApp bridge ready (connected)");
                    connected = true;
                    break;
                }
                Ok(HealthStatus::HttpReady) => {
                    http_ready = true;
                }
                _ => {}
            }
        }

        if !http_ready && !connected {
            return Err(format!(
                "Bridge HTTP server did not start in {BRIDGE_STARTUP_TIMEOUT_SECS}s. Check log: {}",
                bridge_log.display()
            ));
        }

        if !connected {
            info!("Bridge HTTP ready, waiting for WhatsApp connection...");
            let extra_deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < extra_deadline {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Ok(HealthStatus::Connected) = self.check_health().await {
                    info!("WhatsApp bridge ready (connected)");
                    connected = true;
                    break;
                }
            }
            if !connected {
                warn!(
                    "WhatsApp not connected after startup. Bridge may auto-reconnect later. Log: {}",
                    bridge_log.display()
                );
            }
        }

        self.connected.store(true, Ordering::SeqCst);
        info!("WhatsApp bridge started on port {}", self.config.bridge_port);
        Ok(())
    }

    /// Disconnect and clean up the bridge process.
    pub async fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);

        let mut proc_lock = self.bridge_process.write().await;
        if let Some(mut child) = proc_lock.take() {
            // Try graceful termination first
            #[cfg(unix)]
            {
                // On Unix, try to kill the process group
                unsafe {
                    if let Some(pid) = child.id() {
                        let pid_i32 = pid as i32;
                        if pid_i32 > 0 {
                            libc::kill(-pid_i32, libc::SIGTERM);
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.start_kill();
            }

            tokio::time::sleep(Duration::from_secs(1)).await;

            // Force kill if still running
            if child.try_wait().ok().flatten().is_none() {
                #[cfg(unix)]
                {
                    unsafe {
                        if let Some(pid) = child.id() {
                            let pid_i32 = pid as i32;
                            if pid_i32 > 0 {
                                libc::kill(-pid_i32, libc::SIGKILL);
                            }
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
            }
            let _ = child.wait().await;
        }

        info!("WhatsApp disconnected");
    }

    /// Check if the adapter is connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Check bridge health status.
    async fn check_health(&self) -> Result<HealthStatus, String> {
        let url = format!("{}/health", self.bridge_url);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Ok(HealthStatus::Unavailable);
        }

        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        match body.get("status").and_then(|v| v.as_str()) {
            Some("connected") => Ok(HealthStatus::Connected),
            Some("disconnected") => Ok(HealthStatus::HttpReady),
            _ => Ok(HealthStatus::HttpReady),
        }
    }

    /// Poll for incoming messages from the bridge.
    pub async fn get_updates(&self) -> Result<Vec<WhatsAppMessageEvent>, String> {
        if !self.is_connected() {
            return Err("Not connected".into());
        }

        // Check if managed bridge exited
        if let Some(err) = self.check_managed_bridge_exit().await {
            return Err(err);
        }

        let url = format!("{}/messages", self.bridge_url);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| format!("Poll failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Poll HTTP error: {}", resp.status()));
        }

        let messages: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| format!("Poll parse error: {e}"))?;

        let mut events = Vec::with_capacity(messages.len());
        for msg_data in messages {
            if let Some(event) = self.build_message_event(&msg_data).await {
                if !self.dedup.is_duplicate(&event.message_id) {
                    self.dedup.insert(event.message_id.clone());
                    events.push(event);
                }
            }
        }

        Ok(events)
    }

    /// Build a message event from bridge data.
    async fn build_message_event(&self, data: &serde_json::Value) -> Option<WhatsAppMessageEvent> {
        if !self.should_process_message(data) {
            return None;
        }

        let msg_id = data.get("messageId")?.as_str()?.to_string();
        let chat_id = data.get("chatId")?.as_str()?.to_string();
        let sender_id = data
            .get("senderId")
            .and_then(|v| v.as_str())
            .unwrap_or(&chat_id)
            .to_string();
        let sender_name = data.get("senderName").and_then(|v| v.as_str()).map(String::from);
        let chat_name = data.get("chatName").and_then(|v| v.as_str()).map(String::from);
        let is_group = data.get("isGroup").and_then(|v| v.as_bool()).unwrap_or(false);
        let quoted_message_id = data.get("quotedId").and_then(|v| v.as_str()).map(String::from);

        let mentioned_ids: Vec<String> = data
            .get("mentionedIds")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let bot_ids: Vec<String> = data
            .get("botIds")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Determine message type
        let has_media = data.get("hasMedia").and_then(|v| v.as_bool()).unwrap_or(false);
        let media_type = data
            .get("mediaType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let msg_type = if has_media {
            if media_type.contains("image") {
                "image"
            } else if media_type.contains("video") {
                "video"
            } else if media_type.contains("audio") || media_type.contains("ptt") {
                "voice"
            } else {
                "document"
            }
        } else {
            "text"
        }
        .to_string();

        // Collect media URLs/paths
        let mut media_paths = Vec::new();
        let mut media_types = Vec::new();

        if let Some(urls) = data.get("mediaUrls").and_then(|v| v.as_array()) {
            for url_val in urls {
                let url = url_val.as_str().unwrap_or("");
                if url.is_empty() {
                    continue;
                }

                if msg_type == "image" {
                    if url.starts_with("http://") || url.starts_with("https://") {
                        // Bridge should have already downloaded, but fallback
                        media_paths.push(url.to_string());
                        media_types.push("image/jpeg".to_string());
                    } else if Path::new(url).is_absolute() {
                        media_paths.push(url.to_string());
                        media_types.push("image/jpeg".to_string());
                    }
                } else if msg_type == "voice" {
                    if url.starts_with("http://") || url.starts_with("https://") || Path::new(url).is_absolute() {
                        media_paths.push(url.to_string());
                        media_types.push("audio/ogg".to_string());
                    }
                } else if msg_type == "document" || msg_type == "video" {
                    if Path::new(url).is_absolute() || url.starts_with("http") {
                        media_paths.push(url.to_string());
                        let mime = if msg_type == "video" {
                            "video/mp4"
                        } else {
                            "application/octet-stream"
                        };
                        media_types.push(mime.to_string());
                    }
                } else {
                    media_paths.push(url.to_string());
                    media_types.push("unknown".to_string());
                }
            }
        }

        // Build content
        let mut body = data.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Clean bot mentions from group messages
        if is_group {
            body = self.clean_bot_mention_text(&body, &bot_ids);
        }

        // Inject text-readable document content
        if msg_type == "document" && !media_paths.is_empty() {
            const MAX_TEXT_INJECT_BYTES: u64 = 100 * 1024;
            for doc_path in &media_paths {
                let path = Path::new(doc_path);
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if !["txt", "md", "csv", "json", "xml", "yaml", "yml", "log", "py", "js", "ts", "html", "css"]
                    .contains(&ext.as_str())
                {
                    continue;
                }
                match tokio::fs::metadata(path).await {
                    Ok(meta) => {
                        if meta.len() > MAX_TEXT_INJECT_BYTES {
                            info!(
                                "Skipping text injection for {} ({} bytes > {})",
                                doc_path,
                                meta.len(),
                                MAX_TEXT_INJECT_BYTES
                            );
                            continue;
                        }
                        match tokio::fs::read_to_string(path).await {
                            Ok(content) => {
                                let fname = path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(doc_path);
                                let injection = format!("[Content of {}]:\n{}", fname, content);
                                if !body.is_empty() {
                                    body = format!("{}\n\n{}", injection, body);
                                } else {
                                    body = injection;
                                }
                            }
                            Err(e) => warn!("Failed to read document text from {}: {}", doc_path, e),
                        }
                    }
                    Err(e) => warn!("Failed to stat document {}: {}", doc_path, e),
                }
            }
        }

        // Append media paths to content
        if !media_paths.is_empty() {
            for (path, mime) in media_paths.iter().zip(media_types.iter()) {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&format!("[{}: {}]", mime, path));
            }
        }

        Some(WhatsAppMessageEvent {
            message_id: msg_id,
            chat_id,
            sender_id,
            sender_name,
            chat_name,
            is_group,
            content: body,
            msg_type,
            media_paths,
            media_types,
            quoted_message_id,
            mentioned_ids,
            bot_ids,
        })
    }

    /// Determine if a message should be processed.
    fn should_process_message(&self, data: &serde_json::Value) -> bool {
        let is_group = data.get("isGroup").and_then(|v| v.as_bool()).unwrap_or(false);
        if !is_group {
            return true;
        }

        let chat_id = data.get("chatId").and_then(|v| v.as_str()).unwrap_or("");
        if self.config.free_response_chats.contains(&chat_id.to_string()) {
            return true;
        }

        if !self.config.require_mention {
            return true;
        }

        let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
        if body.starts_with('/') {
            return true;
        }

        if self.message_is_reply_to_bot(data) {
            return true;
        }

        if self.message_mentions_bot(data) {
            return true;
        }

        self.message_matches_mention_patterns(data)
    }

    fn message_is_reply_to_bot(&self, data: &serde_json::Value) -> bool {
        let quoted = data
            .get("quotedParticipant")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if quoted.is_empty() {
            return false;
        }
        let bot_ids: Vec<String> = data
            .get("botIds")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        bot_ids.iter().any(|id| normalize_whatsapp_id(id) == normalize_whatsapp_id(quoted))
    }

    fn message_mentions_bot(&self, data: &serde_json::Value) -> bool {
        let bot_ids: Vec<String> = data
            .get("botIds")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if bot_ids.is_empty() {
            return false;
        }

        let mentioned_ids: Vec<String> = data
            .get("mentionedIds")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let normalized_mentioned: std::collections::HashSet<String> =
            mentioned_ids.iter().map(|id| normalize_whatsapp_id(id)).collect();
        let normalized_bots: std::collections::HashSet<String> =
            bot_ids.iter().map(|id| normalize_whatsapp_id(id)).collect();

        if !normalized_mentioned.is_disjoint(&normalized_bots) {
            return true;
        }

        let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        for bot_id in &bot_ids {
            let bare = bot_id.split('@').next().unwrap_or("").to_lowercase();
            if bare.is_empty() {
                continue;
            }
            if body.contains(&format!("@{}", bare)) || body.contains(&bare) {
                return true;
            }
        }
        false
    }

    fn message_matches_mention_patterns(&self, data: &serde_json::Value) -> bool {
        if self.mention_regexes.is_empty() {
            return false;
        }
        let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("");
        self.mention_regexes.iter().any(|re| re.is_match(body))
    }

    fn clean_bot_mention_text(&self, text: &str, bot_ids: &[String]) -> String {
        if text.is_empty() || bot_ids.is_empty() {
            return text.to_string();
        }
        let mut cleaned = text.to_string();
        for bot_id in bot_ids {
            let bare = bot_id.split('@').next().unwrap_or("");
            if bare.is_empty() {
                continue;
            }
            let pattern = format!(r"@{}\b[,:\-]*\s*", regex::escape(bare));
            if let Ok(re) = Regex::new(&pattern) {
                cleaned = re.replace_all(&cleaned, "").to_string();
            }
        }
        cleaned.trim().to_string()
    }

    /// Send a text message.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<(), String> {
        self.send_text_with_reply(chat_id, text, None).await
    }

    /// Send a text message with optional reply-to.
    pub async fn send_text_with_reply(
        &self,
        chat_id: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if !self.is_connected() {
            return Err("Not connected".into());
        }
        if let Some(err) = self.check_managed_bridge_exit().await {
            return Err(err);
        }

        let formatted = format_message(text);
        let chunks = split_message(&formatted, MAX_MESSAGE_LENGTH);

        let mut last_message_id: Option<String> = None;
        for chunk in chunks {
            let mut payload = serde_json::json!({
                "chatId": chat_id,
                "message": chunk,
            });
            if let Some(reply) = reply_to {
                if last_message_id.is_none() {
                    payload["replyTo"] = serde_json::Value::String(reply.to_string());
                }
            }

            let url = format!("{}/send", self.bridge_url);
            let resp = self
                .client
                .post(&url)
                .json(&payload)
                .send()
                .await
                .map_err(|e| format!("Send failed: {e}"))?;

            if !resp.status().is_success() {
                let err_text = resp.text().await.unwrap_or_default();
                return Err(format!("Send HTTP error: {err_text}"));
            }

            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            if let Some(id) = body.get("messageId").and_then(|v| v.as_str()) {
                last_message_id = Some(id.to_string());
            }

            // Small delay between chunks
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        Ok(())
    }

    /// Edit a previously sent message.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), String> {
        if !self.is_connected() {
            return Err("Not connected".into());
        }
        if let Some(err) = self.check_managed_bridge_exit().await {
            return Err(err);
        }

        let url = format!("{}/edit", self.bridge_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "chatId": chat_id,
                "messageId": message_id,
                "message": content,
            }))
            .send()
            .await
            .map_err(|e| format!("Edit failed: {e}"))?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(format!("Edit HTTP error: {err_text}"));
        }
        Ok(())
    }

    /// Send media (image, video, document, audio) via the bridge.
    pub async fn send_media(
        &self,
        chat_id: &str,
        file_path: &str,
        media_type: &str,
        caption: Option<&str>,
        file_name: Option<&str>,
    ) -> Result<(), String> {
        if !self.is_connected() {
            return Err("Not connected".into());
        }
        if let Some(err) = self.check_managed_bridge_exit().await {
            return Err(err);
        }

        if !std::path::Path::new(file_path).exists() {
            return Err(format!("File not found: {file_path}"));
        }

        let mut payload = serde_json::json!({
            "chatId": chat_id,
            "filePath": file_path,
            "mediaType": media_type,
        });
        if let Some(cap) = caption {
            payload["caption"] = serde_json::Value::String(cap.to_string());
        }
        if let Some(name) = file_name {
            payload["fileName"] = serde_json::Value::String(name.to_string());
        }

        let url = format!("{}/send-media", self.bridge_url);
        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Send media failed: {e}"))?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(format!("Send media HTTP error: {err_text}"));
        }
        Ok(())
    }

    /// Send an image file.
    pub async fn send_image(
        &self,
        chat_id: &str,
        image_path: &str,
        caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, image_path, "image", caption, None).await
    }

    /// Send a video file.
    pub async fn send_video(
        &self,
        chat_id: &str,
        video_path: &str,
        caption: Option<&str>,
    ) -> Result<(), String> {
        self.send_media(chat_id, video_path, "video", caption, None).await
    }

    /// Send a document file.
    pub async fn send_document(
        &self,
        chat_id: &str,
        file_path: &str,
        file_name: Option<&str>,
        caption: Option<&str>,
    ) -> Result<(), String> {
        let name = file_name.map(String::from).or_else(|| {
            std::path::Path::new(file_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        });
        self.send_media(chat_id, file_path, "document", caption, name.as_deref())
            .await
    }

    /// Send typing indicator.
    pub async fn send_typing(&self, chat_id: &str) -> Result<(), String> {
        if !self.is_connected() {
            return Ok(());
        }
        let url = format!("{}/typing", self.bridge_url);
        let _ = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "chatId": chat_id }))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        Ok(())
    }

    /// Get chat info.
    pub async fn get_chat_info(&self, chat_id: &str) -> HashMap<String, serde_json::Value> {
        if !self.is_connected() {
            let mut map = HashMap::new();
            map.insert("name".to_string(), serde_json::json!(chat_id));
            map.insert("type".to_string(), serde_json::json!("dm"));
            return map;
        }
        let url = format!("{}/chat/{}", self.bridge_url, chat_id);
        match self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        let mut map = HashMap::new();
                        map.insert(
                            "name".to_string(),
                            serde_json::json!(data.get("name").and_then(|v| v.as_str()).unwrap_or(chat_id)),
                        );
                        map.insert(
                            "type".to_string(),
                            serde_json::json!(
                                if data.get("isGroup").and_then(|v| v.as_bool()).unwrap_or(false) {
                                    "group"
                                } else {
                                    "dm"
                                }
                            ),
                        );
                        if let Some(participants) = data.get("participants").and_then(|v| v.as_array()) {
                            map.insert(
                                "participants".to_string(),
                                serde_json::Value::Array(participants.clone()),
                            );
                        }
                        map
                    }
                    Err(_) => {
                        let mut map = HashMap::new();
                        map.insert("name".to_string(), serde_json::json!(chat_id));
                        map.insert("type".to_string(), serde_json::json!("dm"));
                        map
                    }
                }
            }
            _ => {
                let mut map = HashMap::new();
                map.insert("name".to_string(), serde_json::json!(chat_id));
                map.insert("type".to_string(), serde_json::json!("dm"));
                map
            }
        }
    }

    /// Check if the managed bridge process has exited.
    async fn check_managed_bridge_exit(&self) -> Option<String> {
        let mut proc_lock = self.bridge_process.write().await;
        if let Some(ref mut child) = *proc_lock {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let msg = format!(
                        "WhatsApp bridge process exited unexpectedly (code: {})",
                        status
                    );
                    error!("{msg}");
                    self.connected.store(false, Ordering::SeqCst);
                    Some(msg)
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// Kill any process listening on the given port.
    async fn kill_port_process(&self, port: u16) {
        #[cfg(windows)]
        {
            match tokio::process::Command::new("netstat")
                .args(["-ano", "-p", "TCP"])
                .output()
                .await
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    for line in stdout.lines() {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 5 && parts[3] == "LISTENING" {
                            let local_addr = parts[1];
                            if local_addr.ends_with(&format!(":{port}")) {
                                let _ = tokio::process::Command::new("taskkill")
                                    .args(["/PID", parts[4], "/F"])
                                    .output()
                                    .await;
                            }
                        }
                    }
                }
                Err(_) => {}
            }
        }
        #[cfg(unix)]
        {
            let _ = tokio::process::Command::new("fuser")
                .arg(format!("{port}/tcp"))
                .output()
                .await;
        }
    }
}

/// Bridge health status.
#[derive(Debug, Clone)]
enum HealthStatus {
    Connected,
    HttpReady,
    Unavailable,
}

/// Normalize a WhatsApp ID (replace `:` with `@`).
fn normalize_whatsapp_id(value: &str) -> String {
    value.replace(':', "@")
}

/// Convert standard markdown to WhatsApp-compatible formatting.
///
/// WhatsApp supports: *bold*, _italic_, ~strikethrough~, ```code```,
/// and monospaced `inline`.
pub fn format_message(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }

    // 1. Protect fenced code blocks
    let mut fences: Vec<String> = Vec::new();
    let fence_ph = "\x00FENCE";
    let fence_re = Regex::new(r"```[\s\S]*?```").unwrap();
    let mut result = fence_re
        .replace_all(content, |caps: &regex::Captures| {
            fences.push(caps[0].to_string());
            format!("{fence_ph}{}\x00", fences.len() - 1)
        })
        .to_string();

    // 2. Protect inline code
    let mut codes: Vec<String> = Vec::new();
    let code_ph = "\x00CODE";
    let code_re = Regex::new(r"`[^`\n]+`").unwrap();
    result = code_re
        .replace_all(&result, |caps: &regex::Captures| {
            codes.push(caps[0].to_string());
            format!("{code_ph}{}\x00", codes.len() - 1)
        })
        .to_string();

    // 3. Convert markdown bold/strikethrough
    // Bold: **text** or __text__ → *text*
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    result = bold_re.replace_all(&result, "*$1*").to_string();
    let bold2_re = Regex::new(r"__(.+?)__").unwrap();
    result = bold2_re.replace_all(&result, "*$1*").to_string();

    // Strikethrough: ~~text~~ → ~text~
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    result = strike_re.replace_all(&result, "~$1~").to_string();

    // 4. Convert markdown headers to bold
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    result = header_re.replace_all(&result, "*$1*").to_string();

    // 5. Convert markdown links: [text](url) → text (url)
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    result = link_re.replace_all(&result, "$1 ($2)").to_string();

    // 6. Restore protected sections
    for (i, fence) in fences.iter().enumerate() {
        result = result.replace(&format!("{fence_ph}{i}\x00"), fence);
    }
    for (i, code) in codes.iter().enumerate() {
        result = result.replace(&format!("{code_ph}{i}\x00"), code);
    }

    result
}

/// Split a long message into chunks that fit within the limit.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let char_indices: Vec<(usize, char)> = remaining.char_indices().collect();
        let mut split_pos = max_len;

        // Look for newline before threshold
        for i in (0..char_indices.len().min(max_len)).rev() {
            if char_indices[i].1 == '\n' {
                split_pos = i + 1;
                break;
            }
        }

        // If no newline, look for space
        if split_pos == max_len {
            for i in (0..char_indices.len().min(max_len)).rev() {
                if char_indices[i].1 == ' ' {
                    split_pos = i + 1;
                    break;
                }
            }
        }

        let byte_pos = if split_pos >= char_indices.len() {
            remaining.len()
        } else {
            char_indices[split_pos].0
        };

        let (chunk, rest) = remaining.split_at(byte_pos);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_whatsapp_id() {
        assert_eq!(normalize_whatsapp_id("123:10@s.whatsapp.net"), "123@10@s.whatsapp.net");
        assert_eq!(normalize_whatsapp_id("123@s.whatsapp.net"), "123@s.whatsapp.net");
    }

    #[test]
    fn test_format_message_bold() {
        assert_eq!(format_message("**hello**"), "*hello*");
        assert_eq!(format_message("__hello__"), "*hello*");
    }

    #[test]
    fn test_format_message_strikethrough() {
        assert_eq!(format_message("~~deleted~~"), "~deleted~");
    }

    #[test]
    fn test_format_message_header() {
        assert_eq!(format_message("# Title"), "*Title*");
        assert_eq!(format_message("## Subtitle"), "*Subtitle*");
    }

    #[test]
    fn test_format_message_link() {
        assert_eq!(format_message("[link](https://example.com)"), "link (https://example.com)");
    }

    #[test]
    fn test_format_message_code_blocks_preserved() {
        let input = "**bold** and ```code\nblock```";
        assert_eq!(format_message(input), "*bold* and ```code\nblock```");
    }

    #[test]
    fn test_format_message_inline_code_preserved() {
        let input = "use `code` here";
        assert_eq!(format_message(input), "use `code` here");
    }

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("Hello world", 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello world");
    }

    #[test]
    fn test_split_message_long() {
        let long = "a".repeat(5000);
        let chunks = split_message(&long, 4096);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4096);
        }
    }

    #[test]
    fn test_whatsapp_config_default() {
        let cfg = WhatsAppConfig::default();
        assert_eq!(cfg.bridge_port, DEFAULT_BRIDGE_PORT);
    }

    #[test]
    fn test_clean_bot_mention_text() {
        let adapter = WhatsAppAdapter::new(WhatsAppConfig::default());
        assert_eq!(
            adapter.clean_bot_mention_text("Hello @bot how are you?", &["bot@s.whatsapp.net".to_string()]),
            "Hello how are you?"
        );
        assert_eq!(
            adapter.clean_bot_mention_text("No mentions here", &["bot@s.whatsapp.net".to_string()]),
            "No mentions here"
        );
    }

    #[test]
    fn test_should_process_message_dm() {
        let adapter = WhatsAppAdapter::new(WhatsAppConfig::default());
        let dm_msg = serde_json::json!({ "isGroup": false, "chatId": "123@s.whatsapp.net" });
        assert!(adapter.should_process_message(&dm_msg));
    }

    #[test]
    fn test_should_process_message_group_require_mention() {
        let mut config = WhatsAppConfig::default();
        config.require_mention = true;
        let adapter = WhatsAppAdapter::new(config);

        // Group message without mention
        let group_msg = serde_json::json!({
            "isGroup": true,
            "chatId": "123@g.us",
            "body": "Hello everyone",
        });
        assert!(!adapter.should_process_message(&group_msg));

        // Group message with slash command
        let slash_msg = serde_json::json!({
            "isGroup": true,
            "chatId": "123@g.us",
            "body": "/help",
        });
        assert!(adapter.should_process_message(&slash_msg));
    }

    #[test]
    fn test_should_process_message_free_response_chat() {
        let mut config = WhatsAppConfig::default();
        config.require_mention = true;
        config.free_response_chats = vec!["123@g.us".to_string()];
        let adapter = WhatsAppAdapter::new(config);

        let group_msg = serde_json::json!({
            "isGroup": true,
            "chatId": "123@g.us",
            "body": "Hello everyone",
        });
        assert!(adapter.should_process_message(&group_msg));
    }

    #[test]
    fn test_message_mentions_bot_mentioned_ids() {
        let adapter = WhatsAppAdapter::new(WhatsAppConfig::default());
        let msg = serde_json::json!({
            "botIds": ["bot@s.whatsapp.net"],
            "mentionedIds": ["bot@s.whatsapp.net"],
            "body": "Hello",
        });
        assert!(adapter.message_mentions_bot(&msg));
    }

    #[test]
    fn test_message_mentions_bot_body() {
        let adapter = WhatsAppAdapter::new(WhatsAppConfig::default());
        let msg = serde_json::json!({
            "botIds": ["bot123@s.whatsapp.net"],
            "mentionedIds": [],
            "body": "Hey bot123 can you help?",
        });
        assert!(adapter.message_mentions_bot(&msg));
    }

    #[test]
    fn test_message_is_reply_to_bot() {
        let adapter = WhatsAppAdapter::new(WhatsAppConfig::default());
        let msg = serde_json::json!({
            "botIds": ["bot@s.whatsapp.net"],
            "quotedParticipant": "bot@s.whatsapp.net",
        });
        assert!(adapter.message_is_reply_to_bot(&msg));

        let msg_not_reply = serde_json::json!({
            "botIds": ["bot@s.whatsapp.net"],
            "quotedParticipant": "user@s.whatsapp.net",
        });
        assert!(!adapter.message_is_reply_to_bot(&msg_not_reply));
    }
}
