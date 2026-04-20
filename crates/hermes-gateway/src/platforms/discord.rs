//! Discord platform adapter.
//!
//! Mirrors the Python `gateway/platforms/discord.py`.
//!
//! Uses Discord's Gateway WebSocket + REST API directly:
//! - Gateway WebSocket for receiving MESSAGE_CREATE and INTERACTION_CREATE events
//! - REST API for sending messages, embeds, files, reactions, and slash commands
//! - Supports text channels, DM channels, threads, and forum channels
//!
//! Does NOT include voice support (see Python adapter for that).

use futures::{SinkExt, StreamExt};
use reqwest::{Client, multipart};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use crate::dedup::MessageDeduplicator;
use crate::platforms::helpers::ThreadParticipationTracker;

/// Discord API base URL.
const API_BASE: &str = "https://discord.com/api/v10";
/// Gateway version.
const GATEWAY_VERSION: u8 = 10;
/// Bot intents: GUILDS + GUILD_MESSAGES + DIRECT_MESSAGES + MESSAGE_CONTENT.
const INTENTS: u32 = 1 | 512 | 4096 | 32768;
/// Max message length for a single Discord message.
const MAX_MESSAGE_LENGTH: usize = 2000;
/// Discord forum channel type value.
const CHANNEL_TYPE_GUILD_FORUM: u8 = 15;
/// Discord text channel type value.
const CHANNEL_TYPE_GUILD_TEXT: u8 = 0;
/// Near-split threshold for text batching heuristic.
const SPLIT_THRESHOLD: usize = 1900;

// ── Configuration ───────────────────────────────────────────────────────────

/// Discord platform configuration.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Bot token from Discord Developer Portal.
    pub bot_token: String,
    /// Optional: application ID for slash commands.
    pub application_id: Option<String>,
    /// Reply threading mode: "off", "first" (default), or "all".
    pub reply_to_mode: String,
    /// Whether to add 👀/✅/❌ reactions while processing.
    pub reactions_enabled: bool,
    /// Auto-create threads on @mention in text channels.
    pub auto_thread: bool,
    /// Require @mention in server channels (default true).
    pub require_mention: bool,
    /// Allowed user IDs (comma-separated from env).
    pub allowed_users: HashSet<String>,
    /// Ignored channel IDs.
    pub ignored_channels: HashSet<String>,
    /// Allowed channel IDs (if set, only respond here).
    pub allowed_channels: HashSet<String>,
    /// No-thread channel IDs.
    pub no_thread_channels: HashSet<String>,
    /// Free-response channel IDs (no mention required).
    pub free_response_channels: HashSet<String>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        let bot_token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
        let parse_csv = |key: &str| {
            std::env::var(key)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<HashSet<_>>()
        };

        Self {
            bot_token: bot_token.clone(),
            application_id: std::env::var("DISCORD_APPLICATION_ID").ok(),
            reply_to_mode: std::env::var("DISCORD_REPLY_TO_MODE")
                .unwrap_or_else(|_| "first".to_string())
                .to_lowercase(),
            reactions_enabled: !is_env_false("DISCORD_REACTIONS"),
            auto_thread: !is_env_false("DISCORD_AUTO_THREAD"),
            require_mention: !is_env_false("DISCORD_REQUIRE_MENTION"),
            allowed_users: parse_csv("DISCORD_ALLOWED_USERS"),
            ignored_channels: parse_csv("DISCORD_IGNORED_CHANNELS"),
            allowed_channels: parse_csv("DISCORD_ALLOWED_CHANNELS"),
            no_thread_channels: parse_csv("DISCORD_NO_THREAD_CHANNELS"),
            free_response_channels: parse_csv("DISCORD_FREE_RESPONSE_CHANNELS"),
        }
    }
}

impl DiscordConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

fn is_env_false(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

// ── Data types ──────────────────────────────────────────────────────────────

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
    /// Whether this is a thread / forum post.
    pub is_thread: bool,
    /// Attachments.
    pub attachments: Vec<DiscordAttachment>,
    /// Parent channel ID (for threads).
    pub parent_id: Option<String>,
    /// Channel type (from payload).
    pub channel_type: Option<u8>,
}

/// Parsed slash command interaction.
#[derive(Debug, Clone)]
pub struct DiscordInteractionEvent {
    /// Interaction ID.
    pub interaction_id: String,
    /// Interaction token.
    pub interaction_token: String,
    /// Channel ID.
    pub channel_id: String,
    /// Guild ID (if in a server).
    pub guild_id: Option<String>,
    /// User ID.
    pub user_id: String,
    /// User display name.
    pub user_name: String,
    /// Synthetic command text (e.g. "/reset").
    pub content: String,
    /// Application ID.
    pub application_id: String,
}

/// Safe allowed-mentions defaults for outbound messages.
#[derive(Debug, Clone)]
pub struct AllowedMentions {
    pub everyone: bool,
    pub roles: bool,
    pub users: bool,
    pub replied_user: bool,
}

impl Default for AllowedMentions {
    fn default() -> Self {
        Self {
            everyone: is_env_true("DISCORD_ALLOW_MENTION_EVERYONE"),
            roles: is_env_true("DISCORD_ALLOW_MENTION_ROLES"),
            users: !is_env_false("DISCORD_ALLOW_MENTION_USERS"),
            replied_user: !is_env_false("DISCORD_ALLOW_MENTION_REPLIED_USER"),
        }
    }
}

impl AllowedMentions {
    fn to_json(&self) -> serde_json::Value {
        let mut parse = Vec::new();
        if self.users {
            parse.push("users");
        }
        if self.replied_user {
            parse.push("replied_user");
        }
        serde_json::json!({
            "parse": parse,
            "roles": self.roles,
            "everyone": self.everyone,
        })
    }
}

fn is_env_true(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

// ── Adapter ─────────────────────────────────────────────────────────────────

/// Discord platform adapter.
pub struct DiscordAdapter {
    config: DiscordConfig,
    client: Client,
    dedup: MessageDeduplicator,
    /// Sequence number for gateway heartbeat.
    seq: Arc<parking_lot::Mutex<Option<u64>>>,
    /// Session ID for gateway resume.
    session_id: Arc<parking_lot::Mutex<Option<String>>>,
    /// Bot user ID populated after READY.
    bot_user_id: Arc<parking_lot::Mutex<Option<String>>>,
    /// Threads the bot has participated in.
    threads: ThreadParticipationTracker,
    /// Allowed mentions config.
    allowed_mentions: AllowedMentions,
    /// Pending text batches for rapid successive messages.
    pending_batches: Arc<Mutex<HashMap<String, (DiscordMessageEvent, Instant, tokio::task::AbortHandle)>>>,
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
            bot_user_id: Arc::new(parking_lot::Mutex::new(None)),
            threads: ThreadParticipationTracker::new("discord", 1000),
            allowed_mentions: AllowedMentions::default(),
            pending_batches: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.config.bot_token)
    }

    // ── Gateway WebSocket ───────────────────────────────────────────────────

    /// Connect to Discord Gateway and process events.
    pub async fn run(
        self: Arc<Self>,
        handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        running: Arc<AtomicBool>,
    ) {
        if self.config.bot_token.is_empty() {
            error!("Discord bot_token not configured");
            return;
        }

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

            match Arc::clone(&self)
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
        self: Arc<Self>,
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

        let mut heartbeat = interval(Duration::from_millis(heartbeat_interval));
        heartbeat.reset();

        while running.load(Ordering::SeqCst) {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let s = *self.seq.lock();
                    let heartbeat = serde_json::json!({"op": 1, "d": s});
                    write
                        .send(WsMessage::Text(heartbeat.to_string().into()))
                        .await
                        .map_err(|e: WsError| format!("Heartbeat send failed: {e}"))?;
                }

                msg = read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
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
                                                    if let Some(user) = d.get("user") {
                                                        if let Some(id) = user.get("id").and_then(|v| v.as_str()) {
                                                            *self.bot_user_id.lock() = Some(id.to_string());
                                                            debug!("Discord bot user id: {id}");
                                                        }
                                                    }
                                                    // Register slash commands after ready
                                                    let adapter = self.clone();
                                                    tokio::spawn(async move {
                                                        sleep(Duration::from_secs(2)).await;
                                                        if let Err(e) = adapter.register_slash_commands().await {
                                                            warn!("Discord slash command registration failed: {e}");
                                                        }
                                                    });
                                                }
                                            }
                                            "MESSAGE_CREATE" => {
                                                if let Some(d) = event.get("d") {
                                                    if let Some(msg_event) = self.parse_message(d).await {
                                                        let adapter = self.clone();
                                                        let handler = handler.clone();
                                                        tokio::spawn(async move {
                                                            adapter.handle_message_event(handler, msg_event).await;
                                                        });
                                                    }
                                                }
                                            }
                                            "INTERACTION_CREATE" => {
                                                if let Some(d) = event.get("d") {
                                                    if let Some(interaction) = self.parse_interaction(d) {
                                                        let adapter = self.clone();
                                                        let handler = handler.clone();
                                                        tokio::spawn(async move {
                                                            adapter.handle_interaction_event(handler, interaction).await;
                                                        });
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
                        Some(Ok(WsMessage::Close(_frame))) => {
                            info!("Discord Gateway closed");
                            return Ok(());
                        }
                        Some(Ok(_)) => {
                            // Ignore other message types
                        }
                        Some(Err(e)) => {
                            return Err(format!("WebSocket error: {e}"));
                        }
                        None => {
                            info!("Discord Gateway stream ended");
                            return Ok(());
                        }
                    }
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

        // Determine if thread/forum post from channel type if present
        let channel_type = msg
            .get("channel")
            .and_then(|c| c.get("type"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u8);
        let is_thread = channel_type.map(|t| t == 11 || t == 12).unwrap_or(false);
        let parent_id = msg
            .get("channel")
            .and_then(|c| c.get("parent_id"))
            .and_then(|v| v.as_str())
            .map(String::from);

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

        // Strip bot mention from content
        if let Some(bot_id) = self.bot_user_id.lock().as_ref() {
            content = content.replace(&format!("<@{bot_id}>"), "").replace(&format!("<@!{bot_id}>"), "");
            content = content.trim().to_string();
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
            is_thread,
            attachments,
            parent_id,
            channel_type,
        })
    }

    fn parse_interaction(&self, data: &serde_json::Value) -> Option<DiscordInteractionEvent> {
        let interaction_type = data.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
        // Only handle application commands (type 2)
        if interaction_type != 2 {
            return None;
        }

        let interaction_id = data.get("id").and_then(|v| v.as_str())?.to_string();
        let interaction_token = data.get("token").and_then(|v| v.as_str())?.to_string();
        let channel_id = data.get("channel_id").and_then(|v| v.as_str())?.to_string();
        let guild_id = data.get("guild_id").and_then(|v| v.as_str()).map(String::from);
        let application_id = data.get("application_id").and_then(|v| v.as_str())?.to_string();

        let user = data
            .get("user")
            .or_else(|| data.get("member").and_then(|m| m.get("user")))?;
        let user_id = user.get("id").and_then(|v| v.as_str())?.to_string();
        let user_name = user
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let cmd_data = data.get("data")?;
        let name = cmd_data.get("name").and_then(|v| v.as_str())?.to_string();
        let options = cmd_data.get("options");
        let content = build_slash_text(&name, options);

        Some(DiscordInteractionEvent {
            interaction_id,
            interaction_token,
            channel_id,
            guild_id,
            user_id,
            user_name,
            content,
            application_id,
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

    // ── Event handlers ──────────────────────────────────────────────────────

    async fn handle_message_event(
        self: Arc<Self>,
        handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        msg_event: DiscordMessageEvent,
    ) {
        let chat_id = msg_event.channel_id.clone();
        let message_id = msg_event.message_id.clone();
        let content = msg_event.content.clone();

        // Channel allowlist / ignorelist checks (non-DM only)
        if !msg_event.is_dm {
            let channel_ids: HashSet<String> = [
                Some(chat_id.clone()),
                msg_event.parent_id.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();

            if !self.config.allowed_channels.is_empty() {
                if channel_ids.is_disjoint(&self.config.allowed_channels) {
                    debug!("Discord: ignoring message in non-allowed channel {chat_id}");
                    return;
                }
            }
            if !self.config.ignored_channels.is_disjoint(&channel_ids) {
                debug!("Discord: ignoring message in ignored channel {chat_id}");
                return;
            }
        }

        // Require mention check (non-DM only)
        if !msg_event.is_dm && self.config.require_mention {
            let in_bot_thread = msg_event.is_thread && self.threads.contains(&chat_id);
            let is_free = !self.config.free_response_channels.is_disjoint(&HashSet::from([chat_id.clone()]));
            if !in_bot_thread && !is_free {
                // Check if bot is mentioned
                let bot_mentioned = self.bot_user_id.lock().as_ref().map_or(false, |bot_id| {
                    content.contains(&format!("<@{bot_id}>"))
                        || content.contains(&format!("<@!{bot_id}>"))
                });
                if !bot_mentioned {
                    return;
                }
            }
        }

        // Auto-thread on mention in text channels
        let effective_chat_id = if !msg_event.is_dm
            && !msg_event.is_thread
            && self.config.auto_thread
            && !self.config.no_thread_channels.contains(&chat_id)
            && !self.config.free_response_channels.contains(&chat_id)
        {
            match self.try_auto_thread(&msg_event).await {
                Some(thread_id) => {
                    self.threads.mark(&thread_id);
                    thread_id
                }
                None => chat_id.clone(),
            }
        } else {
            chat_id.clone()
        };

        // Track thread participation
        if msg_event.is_thread {
            self.threads.mark(&chat_id);
        }

        // Add processing reaction
        if self.config.reactions_enabled {
            let _ = self.add_reaction(&chat_id, &message_id, "👀").await;
        }

        let handler_guard = handler.lock().await;
        let handler_ref = handler_guard.as_ref().cloned();
        drop(handler_guard);

        if let Some(h) = handler_ref {
            match h
                .handle_message(
                    crate::config::Platform::Discord,
                    &effective_chat_id,
                    &content,
                    None,
                )
                .await
            {
                Ok(result) => {
                    if self.config.reactions_enabled {
                        let _ = self.remove_reaction(&chat_id, &message_id, "👀").await;
                        let _ = self.add_reaction(&chat_id, &message_id, "✅").await;
                    }
                    if !result.response.is_empty() {
                        if let Err(e) = self.send_text(&effective_chat_id, &result.response).await {
                            error!("Discord send failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    if self.config.reactions_enabled {
                        let _ = self.remove_reaction(&chat_id, &message_id, "👀").await;
                        let _ = self.add_reaction(&chat_id, &message_id, "❌").await;
                    }
                    error!("Discord handler error: {e}");
                }
            }
        }
    }

    async fn handle_interaction_event(
        self: Arc<Self>,
        handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        interaction: DiscordInteractionEvent,
    ) {
        // Acknowledge the interaction immediately (deferred)
        if let Err(e) = self
            .acknowledge_interaction(&interaction.interaction_id, &interaction.interaction_token)
            .await
        {
            warn!("Failed to acknowledge Discord interaction: {e}");
        }

        let handler_guard = handler.lock().await;
        let handler_ref = handler_guard.as_ref().cloned();
        drop(handler_guard);

        if let Some(h) = handler_ref {
            match h
                .handle_message(
                    crate::config::Platform::Discord,
                    &interaction.channel_id,
                    &interaction.content,
                    None,
                )
                .await
            {
                Ok(result) => {
                    if !result.response.is_empty() {
                        // Send followup to replace the deferred message
                        let _ = self
                            .send_interaction_followup(
                                &interaction.application_id,
                                &interaction.interaction_token,
                                &result.response,
                            )
                            .await;
                    } else {
                        // Delete the deferred message if no response
                        let _ = self
                            .delete_interaction_response(
                                &interaction.application_id,
                                &interaction.interaction_token,
                            )
                            .await;
                    }
                }
                Err(e) => {
                    error!("Discord slash handler error: {e}");
                    let _ = self
                        .send_interaction_followup(
                            &interaction.application_id,
                            &interaction.interaction_token,
                            &format!("Error: {e}"),
                        )
                        .await;
                }
            }
        }
    }

    // ── Slash commands ──────────────────────────────────────────────────────

    async fn register_slash_commands(&self) -> Result<(), String> {
        let app_id = match self.config.application_id.as_ref() {
            Some(id) => id,
            None => {
                debug!("Discord application_id not set, skipping slash command registration");
                return Ok(());
            }
        };

        let url = format!("{API_BASE}/applications/{app_id}/commands");

        let commands = serde_json::json!([
            { "name": "new", "description": "Start a new conversation", "type": 1 },
            { "name": "reset", "description": "Reset your Hermes session", "type": 1 },
            { "name": "status", "description": "Show Hermes session status", "type": 1 },
            { "name": "stop", "description": "Stop the running Hermes agent", "type": 1 },
            { "name": "model", "description": "Show or change the model", "type": 1, "options": [
                { "name": "name", "description": "Model name", "type": 3, "required": false }
            ]},
            { "name": "retry", "description": "Retry your last message", "type": 1 },
            { "name": "undo", "description": "Remove the last exchange", "type": 1 },
            { "name": "compress", "description": "Compress conversation context", "type": 1 },
            { "name": "title", "description": "Set or show the session title", "type": 1, "options": [
                { "name": "name", "description": "Session title", "type": 3, "required": false }
            ]},
            { "name": "resume", "description": "Resume a previously-named session", "type": 1, "options": [
                { "name": "name", "description": "Session name", "type": 3, "required": false }
            ]},
            { "name": "usage", "description": "Show token usage for this session", "type": 1 },
            { "name": "help", "description": "Show available commands", "type": 1 },
            { "name": "approve", "description": "Approve a pending dangerous command", "type": 1, "options": [
                { "name": "scope", "description": "Optional scope", "type": 3, "required": false }
            ]},
            { "name": "deny", "description": "Deny a pending dangerous command", "type": 1, "options": [
                { "name": "scope", "description": "Optional scope", "type": 3, "required": false }
            ]},
            { "name": "voice", "description": "Toggle voice reply mode", "type": 1, "options": [
                { "name": "mode", "description": "Voice mode: on, off, tts, channel, leave, or status", "type": 3, "required": false }
            ]},
        ]);

        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&commands)
            .send()
            .await
            .map_err(|e| format!("slash command register request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord slash command register error {status}: {body}"));
        }

        info!("Discord slash commands registered");
        Ok(())
    }

    async fn acknowledge_interaction(&self, interaction_id: &str, token: &str) -> Result<(), String> {
        let url = format!("{API_BASE}/interactions/{interaction_id}/{token}/callback");
        let body = serde_json::json!({ "type": 5 }); // deferred channel message with source

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("interaction ack failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("interaction ack error {status}: {body}"));
        }
        Ok(())
    }

    async fn send_interaction_followup(
        &self,
        app_id: &str,
        token: &str,
        content: &str,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/webhooks/{app_id}/{token}");
        let chunks = split_discord_message(content);
        for (i, chunk) in chunks.iter().enumerate() {
            let body = serde_json::json!({
                "content": chunk,
                "allowed_mentions": self.allowed_mentions.to_json(),
            });
            if i == chunks.len() - 1 {
                // nothing special
            }
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("interaction followup failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let err_body = resp.text().await.unwrap_or_default();
                return Err(format!("interaction followup error {status}: {err_body}"));
            }
        }
        Ok(())
    }

    async fn delete_interaction_response(&self, app_id: &str, token: &str) -> Result<(), String> {
        let url = format!("{API_BASE}/webhooks/{app_id}/{token}/messages/@original");
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| format!("interaction delete failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("interaction delete error {status}: {err_body}"));
        }
        Ok(())
    }

    // ── Reactions ───────────────────────────────────────────────────────────

    /// Add an emoji reaction to a Discord message.
    pub async fn add_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<(), String> {
        let encoded_emoji = urlencode(emoji);
        let url = format!("{API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me");
        let resp = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("add_reaction failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("add_reaction error {status}: {body}"));
        }
        Ok(())
    }

    /// Remove the bot's own emoji reaction from a Discord message.
    pub async fn remove_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<(), String> {
        let encoded_emoji = urlencode(emoji);
        let url = format!("{API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me");
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("remove_reaction failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("remove_reaction error {status}: {body}"));
        }
        Ok(())
    }

    // ── Outbound sending ────────────────────────────────────────────────────

    /// Send a text message to a Discord channel, with forum and reply support.
    pub async fn send_text(&self, channel_id: &str, text: &str) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        // Check if target is a forum channel
        if let Ok(true) = self.is_forum_channel(channel_id).await {
            return self.send_to_forum(channel_id, text).await;
        }

        let chunks = split_discord_message(text);
        for chunk in chunks {
            self.send_message(channel_id, &chunk, None, None).await?;
        }
        Ok(())
    }

    /// Send a text message with a reply reference.
    pub async fn send_text_reply(
        &self,
        channel_id: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        if let Ok(true) = self.is_forum_channel(channel_id).await {
            return self.send_to_forum(channel_id, text).await;
        }

        let chunks = split_discord_message(text);
        for (i, chunk) in chunks.iter().enumerate() {
            let reference = match self.config.reply_to_mode.as_str() {
                "off" => None,
                "all" => reply_to.map(|id| serde_json::json!({ "message_id": id })),
                _ => {
                    // "first" (default) — reply on first chunk only
                    if i == 0 {
                        reply_to.map(|id| serde_json::json!({ "message_id": id }))
                    } else {
                        None
                    }
                }
            };
            self.send_message(channel_id, chunk, None, reference).await?;
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
        self.send_message(channel_id, "", Some(embed), None).await
    }

    /// Send a file attachment to a channel.
    pub async fn send_file(
        &self,
        channel_id: &str,
        file_path: &std::path::Path,
        file_name: Option<&str>,
        caption: Option<&str>,
    ) -> Result<(), String> {
        let name = file_name.unwrap_or_else(|| {
            file_path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        });

        let file_content = tokio::fs::read(file_path)
            .await
            .map_err(|e| format!("read file failed: {e}"))?;

        let part = multipart::Part::bytes(file_content)
            .file_name(name.to_string());

        let form = multipart::Form::new()
            .part("file", part);

        // Build payload_json with allowed_mentions
        let payload = serde_json::json!({
            "content": caption.unwrap_or(""),
            "allowed_mentions": self.allowed_mentions.to_json(),
        });
        let form = form.text("payload_json", payload.to_string());

        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("send file request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord API error {status}: {err_body}"));
        }
        Ok(())
    }

    /// Send an image from a URL as a Discord file attachment.
    pub async fn send_image(
        &self,
        channel_id: &str,
        image_url: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        info!("Discord send_image to {channel_id}: {image_url}");

        let resp = self
            .client
            .get(image_url)
            .send()
            .await
            .map_err(|e| format!("download image failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("download image failed: HTTP {}", resp.status()));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png");

        let ext = if content_type.contains("jpeg") || content_type.contains("jpg") {
            "jpg"
        } else if content_type.contains("gif") {
            "gif"
        } else if content_type.contains("webp") {
            "webp"
        } else {
            "png"
        };

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("read image bytes failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes.to_vec(), &format!("image.{ext}"), caption, reply_to)
            .await
    }

    /// Send a local image file as a Discord attachment.
    pub async fn send_image_file(
        &self,
        channel_id: &str,
        image_path: &std::path::Path,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        let file_name = image_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image.png")
            .to_string();

        let bytes = tokio::fs::read(image_path)
            .await
            .map_err(|e| format!("read image file failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes, &file_name, caption, reply_to)
            .await
    }

    /// Send a document/file as a Discord attachment.
    pub async fn send_document(
        &self,
        channel_id: &str,
        file_path: &std::path::Path,
        caption: Option<&str>,
        file_name: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        let name = file_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string()
            });

        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| format!("read document failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes, &name, caption, reply_to)
            .await
    }

    /// Send a voice/audio file as a Discord attachment.
    /// Discord does not have native voice message support via REST;
    /// the file is uploaded as a regular audio attachment.
    pub async fn send_voice(
        &self,
        channel_id: &str,
        audio_path: &std::path::Path,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        let file_name = audio_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.ogg")
            .to_string();

        let bytes = tokio::fs::read(audio_path)
            .await
            .map_err(|e| format!("read voice file failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes, &file_name, caption, reply_to)
            .await
    }

    /// Send a local video file as a Discord attachment.
    pub async fn send_video(
        &self,
        channel_id: &str,
        video_path: &std::path::Path,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        let file_name = video_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4")
            .to_string();

        let bytes = tokio::fs::read(video_path)
            .await
            .map_err(|e| format!("read video file failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes, &file_name, caption, reply_to)
            .await
    }

    /// Send an animation (GIF) from a URL as a Discord file attachment.
    pub async fn send_animation(
        &self,
        channel_id: &str,
        animation_url: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Discord bot_token not configured".to_string());
        }

        info!("Discord send_animation to {channel_id}: {animation_url}");

        let resp = self
            .client
            .get(animation_url)
            .send()
            .await
            .map_err(|e| format!("download animation failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("download animation failed: HTTP {}", resp.status()));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("read animation bytes failed: {e}"))?;

        self.send_file_bytes(channel_id, bytes.to_vec(), "animation.gif", caption, reply_to)
            .await
    }

    /// Helper: send raw file bytes as a Discord attachment.
    async fn send_file_bytes(
        &self,
        channel_id: &str,
        file_bytes: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<(), String> {
        let part = multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string());

        let mut payload = serde_json::json!({
            "content": caption.unwrap_or(""),
            "allowed_mentions": self.allowed_mentions.to_json(),
        });
        if let Some(reply_id) = reply_to {
            payload["message_reference"] = serde_json::json!({ "message_id": reply_id });
        }

        let form = multipart::Form::new()
            .part("file", part)
            .text("payload_json", payload.to_string());

        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("send file request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord API error {status}: {err_body}"));
        }
        Ok(())
    }

    async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        embed: Option<serde_json::Value>,
        message_reference: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let mut body = serde_json::json!({
            "content": content,
            "allowed_mentions": self.allowed_mentions.to_json(),
        });
        if let Some(e) = embed {
            body["embeds"] = serde_json::Value::Array(vec![e]);
        }
        if let Some(r) = message_reference {
            body["message_reference"] = r;
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
            let err_body: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = err_body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Discord API error {status}: {msg}"));
        }

        Ok(())
    }

    // ── Forum channel support ───────────────────────────────────────────────

    async fn is_forum_channel(&self, channel_id: &str) -> Result<bool, String> {
        let url = format!("{API_BASE}/channels/{channel_id}");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| format!("channel fetch failed: {e}"))?;

        if !resp.status().is_success() {
            return Ok(false);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("channel parse error: {e}"))?;

        let channel_type = body.get("type").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        Ok(channel_type == CHANNEL_TYPE_GUILD_FORUM)
    }

    async fn send_to_forum(&self, forum_channel_id: &str, content: &str) -> Result<(), String> {
        let thread_name = derive_forum_thread_name(content);
        let chunks = split_discord_message(content);
        let starter = chunks.first().map(String::as_str).unwrap_or(&thread_name);

        let url = format!("{API_BASE}/channels/{forum_channel_id}/threads");
        let body = serde_json::json!({
            "name": thread_name,
            "message": {
                "content": starter,
                "allowed_mentions": self.allowed_mentions.to_json(),
            },
            "auto_archive_duration": 1440,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("forum thread create failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("forum thread create error {status}: {err_body}"));
        }

        let thread_data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("forum thread parse error: {e}"))?;

        let thread_id = thread_data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(forum_channel_id);

        // Send remaining chunks into the new thread
        for chunk in chunks.iter().skip(1) {
            if let Err(e) = self.send_message(thread_id, chunk, None, None).await {
                warn!("Failed to send follow-up chunk to forum thread {thread_id}: {e}");
            }
        }

        Ok(())
    }

    async fn try_auto_thread(&self, msg_event: &DiscordMessageEvent) -> Option<String> {
        let thread_name = {
            let mut text = msg_event.content.clone();
            // Strip mention syntax for cleaner titles
            let re = regex::Regex::new(r"<@[!&]?\d+>").ok()?;
            text = re.replace_all(&text, "").to_string();
            let re2 = regex::Regex::new(r"<#\d+>").ok()?;
            text = re2.replace_all(&text, "").to_string();
            text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                "Hermes".to_string()
            } else if text.chars().count() > 80 {
                text.chars().take(77).collect::<String>() + "..."
            } else {
                text
            }
        };

        // Try creating a thread from the message
        let url = format!(
            "{API_BASE}/channels/{}/messages/{}/threads",
            msg_event.channel_id, msg_event.message_id
        );
        let body = serde_json::json!({
            "name": thread_name,
            "auto_archive_duration": 1440,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.ok()?;
                data.get("id").and_then(|v| v.as_str()).map(String::from)
            }
            _ => {
                // Fallback: create thread in channel directly
                let url = format!("{API_BASE}/channels/{}/threads", msg_event.channel_id);
                let body = serde_json::json!({
                    "name": thread_name,
                    "type": 11,
                    "auto_archive_duration": 1440,
                });
                let resp = self
                    .client
                    .post(&url)
                    .header("Authorization", self.auth_header())
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        let data: serde_json::Value = r.json().await.ok()?;
                        data.get("id").and_then(|v| v.as_str()).map(String::from)
                    }
                    _ => None,
                }
            }
        }
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

/// URL-encode an emoji string for Discord API paths.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Build synthetic command text from slash command name + options.
fn build_slash_text(name: &str, options: Option<&serde_json::Value>) -> String {
    let mut parts = vec![format!("/{name}")];
    if let Some(opts) = options.and_then(|v| v.as_array()) {
        for opt in opts {
            if let Some(val) = opt.get("value").and_then(|v| v.as_str()) {
                parts.push(val.to_string());
            } else if let Some(val) = opt.get("value").and_then(|v| v.as_i64()) {
                parts.push(val.to_string());
            } else if let Some(val) = opt.get("value").and_then(|v| v.as_f64()) {
                parts.push(val.to_string());
            }
        }
    }
    parts.join(" ")
}

/// Derive a short forum thread name from message content.
fn derive_forum_thread_name(content: &str) -> String {
    let text = content.lines().next().unwrap_or(content).trim();
    let text: String = text
        .chars()
        .take(80)
        .collect();
    if text.is_empty() {
        "New Post".to_string()
    } else if text.chars().count() > 80 {
        text.chars().take(77).collect::<String>() + "..."
    } else {
        text.to_string()
    }
}

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

    #[test]
    fn test_build_slash_text() {
        assert_eq!(build_slash_text("reset", None), "/reset");
        let opts = serde_json::json!([{"name": "name", "value": "claude"}]);
        assert_eq!(build_slash_text("model", Some(&opts)), "/model claude");
    }

    #[test]
    fn test_derive_forum_thread_name() {
        assert_eq!(derive_forum_thread_name("Hello world\nMore text"), "Hello world");
        assert_eq!(derive_forum_thread_name(""), "New Post");
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("👀"), "%F0%9F%91%80");
        assert_eq!(urlencode("✅"), "%E2%9C%85");
    }
}
