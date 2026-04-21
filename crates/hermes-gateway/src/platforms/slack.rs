#![allow(dead_code)]
//! Slack platform adapter.
//!
//! Mirrors the Python `gateway/platforms/slack.py`.
//!
//! Receiving messages:
//! - Event API (HTTP webhook) — passive mode
//! - Socket Mode (WebSocket) — active mode, supports slash commands and
//!   Block Kit interactions natively
//!
//! Supports multi-workspace via comma-separated bot tokens.
//!
//! Outbound:
//! - chat.postMessage / chat.update
//! - files.upload (v2-compatible multipart)
//! - reactions.add / reactions.remove
//! - Block Kit approval cards
//!
//! Required env vars:
//!   - SLACK_BOT_TOKEN (xoxb-...) — API auth (comma-separated for multi-workspace)
//!   - SLACK_SIGNING_SECRET — webhook signature verification
//!
//! Socket Mode additionally requires:
//!   - SLACK_APP_TOKEN (xapp-...) — Socket Mode auth
//!
//! Optional:
//!   - SLACK_WEBHOOK_PORT (default: 8767)
//!   - SLACK_WEBHOOK_PATH (default: /slack/events)
//!   - SLACK_CONNECTION_MODE (webhook | socket_mode; default: webhook)

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::post,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::connect_async;

use crate::dedup::MessageDeduplicator;

/// Type alias for assistant thread metadata storage.
type AssistantThreads = Arc<Mutex<HashMap<(String, String), HashMap<String, String>>>>;

/// Slack Web API base URL.
const API_BASE: &str = "https://slack.com/api";
/// Max message length for a single Slack message.
const MAX_MESSAGE_LENGTH: usize = 39000;
/// Cap for bot-message timestamp tracking.
const BOT_TS_MAX: usize = 5000;
/// Cap for mentioned-threads tracking.
const MENTIONED_THREADS_MAX: usize = 5000;
/// Cap for assistant-thread metadata cache.
const ASSISTANT_THREADS_MAX: usize = 5000;

// ── Approval choice mapping ───────────────────────────────────────────────

const APPROVAL_CHOICE_MAP: &[(&str, &str)] = &[
    ("hermes_approve_once", "once"),
    ("hermes_approve_session", "session"),
    ("hermes_approve_always", "always"),
    ("hermes_deny", "deny"),
];

const APPROVAL_LABEL_MAP: &[(&str, &str)] = &[
    ("once", "Approved once"),
    ("session", "Approved for session"),
    ("always", "Approved permanently"),
    ("deny", "Denied"),
];

// ── Connection mode ───────────────────────────────────────────────────────

/// How the Slack adapter receives inbound events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackConnectionMode {
    /// HTTP webhook server (Event API).
    Webhook,
    /// Active WebSocket connection (Socket Mode).
    SocketMode,
}

impl Default for SlackConnectionMode {
    fn default() -> Self {
        match std::env::var("SLACK_CONNECTION_MODE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "socket" | "socket_mode" | "ws" | "websocket" => Self::SocketMode,
            _ => Self::Webhook,
        }
    }
}

// ── Configuration ─────────────────────────────────────────────────────────

/// Slack platform configuration.
#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Bot token (xoxb-...). Comma-separated for multi-workspace.
    pub bot_token: String,
    /// App-level token (xapp-...) for Socket Mode.
    pub app_token: String,
    /// Signing secret for webhook verification.
    pub signing_secret: String,
    /// Webhook server port.
    pub webhook_port: u16,
    /// Webhook callback path for Event API.
    pub webhook_path: String,
    /// Path for Block Kit interactive actions.
    pub interactive_path: String,
    /// Path for slash commands.
    pub command_path: String,
    /// Connection mode.
    pub connection_mode: SlackConnectionMode,
}

impl Default for SlackConfig {
    fn default() -> Self {
        let webhook_path = std::env::var("SLACK_WEBHOOK_PATH")
            .ok()
            .unwrap_or_else(|| "/slack/events".to_string());
        let base = webhook_path.trim_end_matches("/events").trim_end_matches('/').to_string();
        let interactive_path = if base.is_empty() {
            "/slack/interactive".to_string()
        } else {
            format!("{base}/interactive")
        };
        let command_path = if base.is_empty() {
            "/slack/command".to_string()
        } else {
            format!("{base}/command")
        };

        Self {
            bot_token: std::env::var("SLACK_BOT_TOKEN").unwrap_or_default(),
            app_token: std::env::var("SLACK_APP_TOKEN").unwrap_or_default(),
            signing_secret: std::env::var("SLACK_SIGNING_SECRET").unwrap_or_default(),
            webhook_port: std::env::var("SLACK_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8767),
            webhook_path,
            interactive_path,
            command_path,
            connection_mode: SlackConnectionMode::default(),
        }
    }
}

impl SlackConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

// ── Data types ────────────────────────────────────────────────────────────

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

/// Pending approval state.
#[derive(Debug, Clone)]
pub struct ApprovalState {
    pub session_key: String,
    pub message_id: String,
    pub chat_id: String,
}

type SlackMessageHandler = Arc<dyn Fn(SlackMessageEvent) + Send + Sync>;

// ── Webhook state ─────────────────────────────────────────────────────────

struct WebhookState {
    config: SlackConfig,
    dedup: Arc<MessageDeduplicator>,
    on_message: Arc<Mutex<Option<SlackMessageHandler>>>,
    adapter: Arc<SlackAdapter>,
}

// ── Slack Adapter ─────────────────────────────────────────────────────────

/// Slack platform adapter.
pub struct SlackAdapter {
    config: SlackConfig,
    client: Client,
    dedup: Arc<MessageDeduplicator>,
    /// Bot user ID (fetched lazily).
    bot_user_id: Arc<Mutex<Option<String>>>,
    /// User name cache.
    user_name_cache: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    /// team_id → bot_token mapping for multi-workspace.
    team_tokens: Arc<Mutex<HashMap<String, String>>>,
    /// team_id → bot_user_id.
    team_bot_user_ids: Arc<Mutex<HashMap<String, String>>>,
    /// channel_id → team_id.
    channel_team: Arc<Mutex<HashMap<String, String>>>,
    /// Timestamps of messages sent by the bot.
    bot_message_ts: Arc<Mutex<HashSet<String>>>,
    /// Threads where the bot was @mentioned.
    mentioned_threads: Arc<Mutex<HashSet<String>>>,
    /// Assistant thread metadata: (channel_id, thread_ts) → metadata map.
    assistant_threads: AssistantThreads,
    /// Pending approval resolved flags: message_ts → resolved.
    approval_resolved: Arc<Mutex<HashMap<String, bool>>>,
    /// Pending approval state keyed by approval_id.
    approval_state: Arc<Mutex<HashMap<u64, ApprovalState>>>,
    /// Atomic counter for generating approval IDs.
    approval_counter: Arc<AtomicU64>,
    /// Running flag for Socket Mode graceful shutdown.
    running: Arc<AtomicBool>,
    /// Gateway-level approval registry for resolving pending approvals.
    approval_registry: crate::runner::ApprovalRegistry,
}

impl SlackAdapter {
    /// Access the adapter configuration.
    pub fn config(&self) -> &SlackConfig {
        &self.config
    }

    pub fn new(config: SlackConfig, approval_registry: crate::runner::ApprovalRegistry) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: Arc::new(MessageDeduplicator::with_params(300, 2000)),
            bot_user_id: Arc::new(Mutex::new(None)),
            user_name_cache: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            team_tokens: Arc::new(Mutex::new(HashMap::new())),
            team_bot_user_ids: Arc::new(Mutex::new(HashMap::new())),
            channel_team: Arc::new(Mutex::new(HashMap::new())),
            bot_message_ts: Arc::new(Mutex::new(HashSet::new())),
            mentioned_threads: Arc::new(Mutex::new(HashSet::new())),
            assistant_threads: Arc::new(Mutex::new(HashMap::new())),
            approval_resolved: Arc::new(Mutex::new(HashMap::new())),
            approval_state: Arc::new(Mutex::new(HashMap::new())),
            approval_counter: Arc::new(AtomicU64::new(1)),
            running: Arc::new(AtomicBool::new(true)),
            approval_registry,
            config,
        }
    }

    /// Build the Authorization header for the primary token.
    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.bot_token)
    }

    /// Build the Authorization header for a specific team.
    fn auth_header_for_team(&self, team_id: &str) -> String {
        let tokens = self.team_tokens.blocking_lock();
        if let Some(token) = tokens.get(team_id) {
            format!("Bearer {token}")
        } else {
            self.auth_header()
        }
    }

    /// Resolve the workspace-specific auth header for a channel.
    fn auth_header_for_channel(&self, channel_id: &str) -> String {
        let teams = self.channel_team.blocking_lock();
        if let Some(team_id) = teams.get(channel_id).cloned() {
            drop(teams);
            self.auth_header_for_team(&team_id)
        } else {
            drop(teams);
            self.auth_header()
        }
    }

    // ── Multi-workspace initialisation ────────────────────────────────────

    /// Discover team_id / bot_user_id for each comma-separated bot token.
    async fn initialize_teams(&self) -> Result<(), String> {
        let raw = &self.config.bot_token;
        if raw.is_empty() {
            return Err("SLACK_BOT_TOKEN not set".to_string());
        }

        let tokens: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut team_map = self.team_tokens.lock().await;
        let mut bot_map = self.team_bot_user_ids.lock().await;

        for token in tokens {
            let url = format!("{API_BASE}/auth.test");
            let resp = match self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("[Slack] auth.test request failed for a token: {e}");
                    continue;
                }
            };

            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("[Slack] auth.test parse failed: {e}");
                    continue;
                }
            };

            if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                warn!("[Slack] auth.test error: {err}");
                continue;
            }

            let team_id = body
                .get("team_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let bot_uid = body
                .get("user_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let team_name = body
                .get("team")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let bot_name = body
                .get("user")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            if !team_id.is_empty() {
                team_map.insert(team_id.clone(), token.clone());
                bot_map.insert(team_id.clone(), bot_uid.clone());
            }

            // First successful token becomes the primary bot user id.
            let mut primary = self.bot_user_id.lock().await;
            if primary.is_none() && !bot_uid.is_empty() {
                *primary = Some(bot_uid);
            }

            info!(
                "[Slack] Authenticated as @{} in workspace {} (team: {})",
                bot_name, team_name,
                body.get("team_id").and_then(|v| v.as_str()).unwrap_or("?")
            );
        }

        info!(
            "[Slack] Workspace map ready: {} workspace(s)",
            team_map.len()
        );
        Ok(())
    }

    // ── Connection mode dispatcher ────────────────────────────────────────

    /// Run the adapter in the configured mode (webhook or Socket Mode).
    pub async fn run(
        &self,
        on_message: impl Fn(SlackMessageEvent) + Send + Sync + 'static,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        // Always resolve team tokens (idempotent and cheap after first call).
        self.initialize_teams().await?;

        match self.config.connection_mode {
            SlackConnectionMode::Webhook => {
                self.run_webhook(on_message, shutdown_rx).await
            }
            SlackConnectionMode::SocketMode => {
                self.run_socket_mode(on_message, shutdown_rx).await
            }
        }
    }

    // ── Webhook Server ────────────────────────────────────────────────────

    async fn run_webhook(
        &self,
        on_message: impl Fn(SlackMessageEvent) + Send + Sync + 'static,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let state = Arc::new(WebhookState {
            config: self.config.clone(),
            dedup: self.dedup.clone(),
            on_message: Arc::new(Mutex::new(Some(Arc::new(on_message)))),
            adapter: Arc::new(self.clone_like()),
        });

        let app = Router::new()
            .route(&self.config.webhook_path, post(handle_slack_webhook))
            .route(&self.config.interactive_path, post(handle_slack_interactive))
            .route(&self.config.command_path, post(handle_slash_command))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", self.config.webhook_port))
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        info!(
            "Slack webhook listening on 0.0.0.0:{}",
            self.config.webhook_port
        );
        info!("  events: {}", self.config.webhook_path);
        info!("  interactive: {}", self.config.interactive_path);
        info!("  commands: {}", self.config.command_path);

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("Slack webhook shutting down");
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    }

    // ── Socket Mode ───────────────────────────────────────────────────────

    async fn run_socket_mode(
        &self,
        on_message: impl Fn(SlackMessageEvent) + Send + Sync + 'static,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        if self.config.app_token.is_empty() {
            return Err("SLACK_APP_TOKEN not set (required for Socket Mode)".to_string());
        }

        let on_message = Arc::new(on_message);
        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        // Bridge oneshot shutdown into the AtomicBool.
        tokio::spawn(async move {
            let _ = shutdown_rx.await;
            running.store(false, Ordering::SeqCst);
        });

        const BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
        let mut backoff_idx = 0;

        while self.running.load(Ordering::SeqCst) {
            match self.socket_mode_connect_and_run(on_message.clone()).await {
                Ok(()) => {
                    backoff_idx = 0; // clean disconnect
                }
                Err(e) => {
                    if !self.running.load(Ordering::SeqCst) {
                        break;
                    }
                    // If the error carries a refresh URL, reconnect immediately.
                    if let Some(url) = e.strip_prefix("refresh_url:") {
                        info!("[Slack SM] Server requested reconnect via refresh URL");
                        if let Err(reconn_err) =
                            self.socket_mode_run_with_url(on_message.clone(), url).await
                        {
                            error!("[Slack SM] Refresh reconnect failed: {reconn_err}");
                        }
                        backoff_idx = 0;
                        continue;
                    }

                    error!("[Slack SM] Connection error: {e}");
                    let delay = BACKOFF[backoff_idx.min(BACKOFF.len() - 1)];
                    info!("[Slack SM] Reconnecting in {delay}s...");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    backoff_idx = (backoff_idx + 1).min(BACKOFF.len() - 1);
                }
            }
        }

        info!("[Slack SM] Socket Mode loop stopped");
        Ok(())
    }

    /// Fetch a fresh Socket Mode WebSocket URL from Slack.
    async fn fetch_socket_mode_url(&self) -> Result<String, String> {
        let url = format!("{API_BASE}/apps.connections.open");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.app_token))
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| format!("apps.connections.open request failed: {e}"))?;

        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("apps.connections.open parse error: {e}"))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Slack apps.connections.open error: {err}"));
        }

        body.get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "Missing url in apps.connections.open response".to_string())
    }

    async fn socket_mode_connect_and_run(
        &self,
        on_message: Arc<dyn Fn(SlackMessageEvent) + Send + Sync>,
    ) -> Result<(), String> {
        let ws_url = self.fetch_socket_mode_url().await?;
        self.socket_mode_run_with_url(on_message, &ws_url).await
    }

    async fn socket_mode_run_with_url(
        &self,
        on_message: Arc<dyn Fn(SlackMessageEvent) + Send + Sync>,
        ws_url: &str,
    ) -> Result<(), String> {
        info!("[Slack SM] Connecting to Socket Mode...");

        let uri: tokio_tungstenite::tungstenite::http::Uri = ws_url
            .parse()
            .map_err(|e| format!("Invalid WebSocket URL: {e}"))?;

        let (ws_stream, _response) = connect_async(uri)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        info!("[Slack SM] Connected");

        let (mut write_half, mut read_half) = ws_stream.split();

        loop {
            tokio::select! {
                result = read_half.next() => {
                    match result {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_socket_mode_message(
                                &text,
                                &on_message,
                                &mut write_half,
                            ).await?
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!("[Slack SM] Closed by server: {frame:?}");
                            return Err("WebSocket closed by server".to_string());
                        }
                        Some(Ok(Message::Ping(ping))) => {
                            let _ = write_half.send(Message::Pong(ping)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Binary(bin))) => {
                            if let Ok(text) = String::from_utf8(bin.into()) {
                                self.handle_socket_mode_message(
                                    &text,
                                    &on_message,
                                    &mut write_half,
                                ).await?
                            }
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(e)) => {
                            return Err(format!("WebSocket read error: {e}"));
                        }
                        None => {
                            return Err("WebSocket stream ended".to_string());
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if !self.running.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn handle_socket_mode_message(
        &self,
        text: &str,
        on_message: &Arc<dyn Fn(SlackMessageEvent) + Send + Sync>,
        write_half: &mut futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    ) -> Result<(), String> {
        let envelope: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                warn!("[Slack SM] Failed to parse envelope: {e}");
                return Ok(());
            }
        };

        let envelope_id = envelope
            .get("envelope_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let msg_type = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match msg_type {
            "hello" => {
                debug!("[Slack SM] Hello received");
                let num_connections = envelope
                    .get("num_connections")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                info!(
                    "[Slack SM] Socket Mode ready ({} connection(s))",
                    num_connections
                );
                Ok(())
            }
            "events_api" => {
                if let Some(payload) = envelope.get("payload") {
                    let full_event = serde_json::json!({
                        "event": payload.get("event"),
                        "event_id": payload.get("event_id"),
                        "team_id": payload.get("team_id"),
                    });
                    if let Some(event) = self.parse_event(&full_event) {
                        on_message(event);
                    }
                }
                self.ack_socket_mode(write_half, envelope_id).await;
                Ok(())
            }
            "interactive" => {
                if let Some(payload) = envelope.get("payload") {
                    if let Err(e) = self.handle_block_action_payload(payload).await {
                        warn!("[Slack SM] Block action handling error: {e}");
                    }
                }
                self.ack_socket_mode(write_half, envelope_id).await;
                Ok(())
            }
            "slash_command" => {
                if let Some(payload) = envelope.get("payload") {
                    if let Some(event) = self.parse_slash_command_payload(payload) {
                        on_message(event);
                    }
                }
                self.ack_socket_mode(write_half, envelope_id).await;
                Ok(())
            }
            "disconnect" => {
                let reason = envelope
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Slack SM] Disconnect received (reason: {reason})");
                if let Some(refresh_url) = envelope.get("refresh_url").and_then(|v| v.as_str()) {
                    let _ = write_half.send(Message::Close(None)).await;
                    return Err(format!("refresh_url:{refresh_url}"));
                }
                Err(format!("Server requested disconnect: {reason}"))
            }
            _ => {
                debug!("[Slack SM] Unknown envelope type: {msg_type}");
                self.ack_socket_mode(write_half, envelope_id).await;
                Ok(())
            }
        }
    }

    async fn ack_socket_mode(
        &self,
        write_half: &mut futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
        envelope_id: &str,
    ) {
        let ack = serde_json::json!({
            "envelope_id": envelope_id,
            "payload": {},
        });
        if let Err(e) = write_half.send(Message::Text(ack.to_string().into())).await {
            warn!("[Slack SM] Failed to send ack: {e}");
        }
    }

    // ── Slash command parsing ─────────────────────────────────────────────

    fn parse_slash_command_payload(&self, payload: &Value) -> Option<SlackMessageEvent> {
        let command = payload.get("command").and_then(|v| v.as_str())?;
        let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let user_id = payload.get("user_id").and_then(|v| v.as_str())?;
        let channel_id = payload.get("channel_id").and_then(|v| v.as_str())?;
        let team_id = payload.get("team_id").and_then(|v| v.as_str()).map(String::from);
        let trigger_id = payload
            .get("trigger_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Track workspace for this channel.
        if let Some(ref tid) = team_id {
            self.channel_team
                .blocking_lock()
                .insert(channel_id.to_string(), tid.clone());
        }

        let content = if text.is_empty() {
            command.to_string()
        } else {
            format!("{command} {text}")
        };

        Some(SlackMessageEvent {
            event_id: format!("cmd:{trigger_id}"),
            channel_id: channel_id.to_string(),
            team_id,
            user_id: user_id.to_string(),
            user_name: None,
            content,
            thread_ts: None,
            is_dm: false,
            attachments: Vec::new(),
        })
    }

    // ── Block Kit action handling ─────────────────────────────────────────

    async fn handle_block_action_payload(&self, payload: &Value) -> Result<(), String> {
        let actions = payload
            .get("actions")
            .and_then(|v| v.as_array())
            .ok_or("No actions in payload")?;

        let channel = payload
            .get("channel")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user = payload.get("user").unwrap_or(&Value::Null);
        let user_id = user.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let user_name = user.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
        let message = payload.get("message").unwrap_or(&Value::Null);
        let msg_ts = message.get("ts").and_then(|v| v.as_str()).unwrap_or("");

        // Authorisation check (SLACK_ALLOWED_USERS).
        let allowed_csv = std::env::var("SLACK_ALLOWED_USERS").unwrap_or_default();
        if !allowed_csv.is_empty() {
            let allowed: HashSet<String> = allowed_csv
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !allowed.contains("*") && !allowed.contains(user_id) {
                warn!(
                    "[Slack] Unauthorized approval click by {} ({}) — ignoring",
                    user_name, user_id
                );
                return Ok(());
            }
        }

        for action in actions {
            let action_id = action
                .get("action_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let value = action
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Only handle Hermes approval actions.
            let choice = APPROVAL_CHOICE_MAP
                .iter()
                .find(|&&(aid, _)| aid == action_id)
                .map(|&(_, c)| c);

            if let Some(choice) = choice {
                // Prevent double-clicks.
                let already_resolved = {
                    let mut guard = self.approval_resolved.lock().await;
                    guard.insert(msg_ts.to_string(), true).unwrap_or(false)
                };
                if already_resolved {
                    return Ok(());
                }

                // Update the message to show decision and remove buttons.
                let label = APPROVAL_LABEL_MAP
                    .iter()
                    .find(|&&(c, _)| c == choice)
                    .map(|&(_, l)| format!("{l} by {user_name}"))
                    .unwrap_or_else(|| format!("Resolved by {user_name}"));

                // Extract original text from section block.
                let original_text = message
                    .get("blocks")
                    .and_then(|v| v.as_array())
                    .and_then(|blocks| {
                        blocks.iter().find_map(|b| {
                            if b.get("type")? == "section" {
                                b.get("text")?.get("text")?.as_str()
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or("Command approval request");

                let updated_blocks = serde_json::json!([
                    {
                        "type": "section",
                        "text": {
                            "type": "mrkdwn",
                            "text": original_text,
                        },
                    },
                    {
                        "type": "context",
                        "elements": [
                            {"type": "mrkdwn", "text": label},
                        ],
                    },
                ]);

                let auth = self.auth_header_for_channel(channel);
                let _ = self
                    .client
                    .post(format!("{API_BASE}/chat.update"))
                    .header("Authorization", auth)
                    .header("Content-Type", "application/json")
                    .json(&serde_json::json!({
                        "channel": channel,
                        "ts": msg_ts,
                        "text": label,
                        "blocks": updated_blocks,
                    }))
                    .send()
                    .await;

                // Resolve approval state.
                info!(
                    "Slack button resolved approval for session {} (choice={choice}, user={user_name})",
                    value
                );
                // TODO: wire into gateway-level approval blocking mechanism.
            }
        }

        Ok(())
    }

    // ── Inbound message parsing ───────────────────────────────────────────

    fn parse_event(&self, event: &Value) -> Option<SlackMessageEvent> {
        let event_id = event
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let event_data = event.get("event")?;
        let event_type = event_data.get("type").and_then(|v| v.as_str())?;

        // Handle assistant thread lifecycle events.
        if event_type == "assistant_thread_started"
            || event_type == "assistant_thread_context_changed"
        {
            let meta = self.extract_assistant_thread_metadata(event_data);
            self.cache_assistant_thread_metadata(meta);
            return None; // No user message to process.
        }

        // Only process message events.
        if event_type != "message" && event_type != "app_mention" {
            return None;
        }

        // Skip bot messages and message subtypes we don't care about.
        if event_data.get("bot_id").is_some() {
            return None;
        }
        if let Some(subtype) = event_data.get("subtype").and_then(|v| v.as_str()) {
            if subtype == "message_changed" || subtype == "message_deleted" || subtype == "bot_message" {
                return None;
            }
        }

        let channel_id = event_data
            .get("channel")
            .and_then(|v| v.as_str())?
            .to_string();
        let channel_type = event_data
            .get("channel_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_dm = channel_type == "im";

        let user_id = event_data
            .get("user")
            .and_then(|v| v.as_str())?
            .to_string();
        let thread_ts = event_data
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .map(String::from);
        let team_id = event.get("team_id").and_then(|v| v.as_str()).map(String::from);

        let content = event_data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Merge assistant thread metadata if available.
        let _assistant_meta = self.lookup_assistant_thread_metadata(
            event_data,
            &channel_id,
            &thread_ts.clone().unwrap_or_default(),
        );

        // Track workspace for this channel.
        if let Some(ref tid) = team_id {
            self.channel_team
                .blocking_lock()
                .insert(channel_id.clone(), tid.clone());
        }

        // Parse file attachments.
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

        // Deduplication.
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

    // ── Assistant threads ─────────────────────────────────────────────────

    fn extract_assistant_thread_metadata(&self, event: &Value) -> HashMap<String, String> {
        let assistant_thread = event.get("assistant_thread").unwrap_or(&Value::Null);
        let context = assistant_thread
            .get("context")
            .or_else(|| event.get("context"))
            .unwrap_or(&Value::Null);

        let mut m = HashMap::new();
        let channel_id = assistant_thread
            .get("channel_id")
            .or_else(|| event.get("channel"))
            .or_else(|| context.get("channel_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let thread_ts = assistant_thread
            .get("thread_ts")
            .or_else(|| event.get("thread_ts"))
            .or_else(|| event.get("message_ts"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user_id = assistant_thread
            .get("user_id")
            .or_else(|| event.get("user"))
            .or_else(|| context.get("user_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let team_id = event
            .get("team")
            .or_else(|| event.get("team_id"))
            .or_else(|| assistant_thread.get("team_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        m.insert("channel_id".to_string(), channel_id.to_string());
        m.insert("thread_ts".to_string(), thread_ts.to_string());
        m.insert("user_id".to_string(), user_id.to_string());
        m.insert("team_id".to_string(), team_id.to_string());
        m.insert(
            "context_channel_id".to_string(),
            context.get("channel_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        );
        m
    }

    fn cache_assistant_thread_metadata(&self, metadata: HashMap<String, String>) {
        let channel_id = metadata.get("channel_id").cloned().unwrap_or_default();
        let thread_ts = metadata.get("thread_ts").cloned().unwrap_or_default();
        if channel_id.is_empty() || thread_ts.is_empty() {
            return;
        }
        let key = (channel_id.clone(), thread_ts.clone());

        let mut guard = self.assistant_threads.blocking_lock();
        let existing = guard.get(&key).cloned().unwrap_or_default();
        let mut merged = existing;
        for (k, v) in metadata {
            if !v.is_empty() {
                merged.insert(k, v);
            }
        }
        guard.insert(key.clone(), merged);

        // Evict oldest entries when over limit.
        if guard.len() > ASSISTANT_THREADS_MAX {
            let excess = guard.len() - ASSISTANT_THREADS_MAX / 2;
            let keys_to_remove: Vec<_> = guard.keys().take(excess).cloned().collect();
            for k in keys_to_remove {
                guard.remove(&k);
            }
        }

        // Also map channel → team.
        let team_id = guard.get(&key).and_then(|m| m.get("team_id")).cloned().unwrap_or_default();
        if !team_id.is_empty() && !channel_id.is_empty() {
            self.channel_team
                .blocking_lock()
                .insert(channel_id, team_id);
        }
    }

    fn lookup_assistant_thread_metadata(
        &self,
        event: &Value,
        channel_id: &str,
        thread_ts: &str,
    ) -> HashMap<String, String> {
        let mut metadata = self.extract_assistant_thread_metadata(event);
        if !channel_id.is_empty() && metadata.get("channel_id").map(|s| s.is_empty()).unwrap_or(true) {
            metadata.insert("channel_id".to_string(), channel_id.to_string());
        }
        if !thread_ts.is_empty() && metadata.get("thread_ts").map(|s| s.is_empty()).unwrap_or(true) {
            metadata.insert("thread_ts".to_string(), thread_ts.to_string());
        }
        let key = (
            metadata.get("channel_id").cloned().unwrap_or_default(),
            metadata.get("thread_ts").cloned().unwrap_or_default(),
        );
        let guard = self.assistant_threads.blocking_lock();
        if let Some(cached) = guard.get(&key) {
            let mut merged = cached.clone();
            for (k, v) in metadata {
                if !v.is_empty() {
                    merged.insert(k, v);
                }
            }
            merged
        } else {
            metadata
        }
    }

    // ── User resolution ───────────────────────────────────────────────────

    async fn resolve_user_name(&self, user_id: &str) -> Option<String> {
        {
            let cache = self.user_name_cache.lock();
            if let Some(name) = cache.get(user_id) {
                return Some(name.clone());
            }
        }

        let url = format!("{API_BASE}/users.info");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .query(&[("user", user_id)])
            .send()
            .await
            .ok()?;

        let body: Value = resp.json().await.ok()?;
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

        self.user_name_cache
            .lock()
            .insert(user_id.to_string(), name.clone());
        Some(name)
    }

    // ── Outbound sending ──────────────────────────────────────────────────

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
            body["thread_ts"] = Value::String(ts.to_string());
        }

        let auth = self.auth_header_for_channel(channel_id);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("postMessage request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("postMessage HTTP error: {}", resp.status()));
        }

        let resp_body: Value = resp
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

        // Track sent message ts for thread auto-reply logic.
        if let Some(ts) = resp_body.get("ts").and_then(|v| v.as_str()) {
            let mut guard = self.bot_message_ts.lock().await;
            guard.insert(ts.to_string());
            if let Some(root) = thread_ts {
                guard.insert(root.to_string());
            }
            if guard.len() > BOT_TS_MAX {
                let excess = guard.len() - BOT_TS_MAX / 2;
                let to_remove: Vec<String> = guard.iter().take(excess).cloned().collect();
                for t in to_remove {
                    guard.remove(&t);
                }
            }
        }

        Ok(())
    }

    // ── Message editing ───────────────────────────────────────────────────

    /// Edit a previously sent Slack message.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let url = format!("{API_BASE}/chat.update");
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": message_id,
            "text": text,
        });

        let auth = self.auth_header_for_channel(channel_id);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("chat.update request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("chat.update HTTP error: {}", resp.status()));
        }

        let resp_body: Value = resp
            .json()
            .await
            .map_err(|e| format!("chat.update parse error: {e}"))?;

        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Slack API error: {err}"));
        }

        Ok(())
    }

    // ── Rich media uploads ────────────────────────────────────────────────

    /// Upload and send an image file.
    pub async fn send_image(
        &self,
        channel_id: &str,
        image_path: &str,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        let file_name = std::path::Path::new(image_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image");
        self.upload_file_v2(channel_id, image_path, file_name, caption, thread_ts)
            .await
    }

    /// Upload and send a voice/audio file.
    pub async fn send_voice(
        &self,
        channel_id: &str,
        audio_path: &str,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        let file_name = std::path::Path::new(audio_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio");
        self.upload_file_v2(channel_id, audio_path, file_name, caption, thread_ts)
            .await
    }

    /// Upload and send a video file.
    pub async fn send_video(
        &self,
        channel_id: &str,
        video_path: &str,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        let file_name = std::path::Path::new(video_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video");
        self.upload_file_v2(channel_id, video_path, file_name, caption, thread_ts)
            .await
    }

    /// Upload and send a document file.
    pub async fn send_document(
        &self,
        channel_id: &str,
        file_path: &str,
        file_name: Option<&str>,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        let display_name = file_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                std::path::Path::new(file_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string()
            });
        self.upload_file_v2(channel_id, file_path, &display_name, caption, thread_ts)
            .await
    }

    async fn upload_file_v2(
        &self,
        channel_id: &str,
        file_path: &str,
        filename: &str,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let bytes = std::fs::read(file_path).map_err(|e| format!("read file failed: {e}"))?;
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());

        let mut form = reqwest::multipart::Form::new()
            .text("channels", channel_id.to_string())
            .text("filename", filename.to_string())
            .part("file", part);

        if let Some(c) = caption {
            form = form.text("initial_comment", c.to_string());
        }
        if let Some(ts) = thread_ts {
            form = form.text("thread_ts", ts.to_string());
        }

        let auth = self.auth_header_for_channel(channel_id);
        let resp = self
            .client
            .post(format!("{API_BASE}/files.upload"))
            .header("Authorization", auth)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("files.upload request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("files.upload HTTP error: {}", resp.status()));
        }

        let body: Value = resp
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

    /// Upload and send a file to a channel (legacy alias).
    pub async fn send_file(
        &self,
        channel_id: &str,
        file_path: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), String> {
        self.send_document(channel_id, file_path, None, None, thread_ts)
            .await
    }

    // ── Reactions ─────────────────────────────────────────────────────────

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

        let auth = self.auth_header_for_channel(channel_id);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("reactions.add failed: {e}"))?;

        let resp_body: Value = resp.json().await.unwrap_or_default();
        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(format!("Slack reaction error: {err}"));
        }
        Ok(())
    }

    /// Remove a reaction emoji from a message.
    pub async fn remove_reaction(
        &self,
        channel_id: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/reactions.remove");
        let body = serde_json::json!({
            "channel": channel_id,
            "timestamp": timestamp,
            "name": emoji,
        });

        let auth = self.auth_header_for_channel(channel_id);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("reactions.remove failed: {e}"))?;

        let resp_body: Value = resp.json().await.unwrap_or_default();
        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(format!("Slack reaction error: {err}"));
        }
        Ok(())
    }

    // ── Block Kit approval ────────────────────────────────────────────────

    /// Send a Block Kit approval prompt with interactive buttons.
    pub async fn send_exec_approval(
        &self,
        chat_id: &str,
        command: &str,
        session_key: &str,
        description: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, String> {
        if self.config.bot_token.is_empty() {
            return Err("Slack bot_token not configured".to_string());
        }

        let approval_id = self
            .approval_counter
            .fetch_add(1, Ordering::SeqCst);
        let cmd_preview = if command.len() > 2900 {
            format!("{}...", &command[..2900])
        } else {
            command.to_string()
        };

        let blocks = serde_json::json!([
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        ":warning: *Command Approval Required*\n```{}```\nReason: {}",
                        cmd_preview, description
                    ),
                },
            },
            {
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "text": {"type": "plain_text", "text": "Allow Once"},
                        "style": "primary",
                        "action_id": "hermes_approve_once",
                        "value": session_key,
                    },
                    {
                        "type": "button",
                        "text": {"type": "plain_text", "text": "Allow Session"},
                        "action_id": "hermes_approve_session",
                        "value": session_key,
                    },
                    {
                        "type": "button",
                        "text": {"type": "plain_text", "text": "Always Allow"},
                        "action_id": "hermes_approve_always",
                        "value": session_key,
                    },
                    {
                        "type": "button",
                        "text": {"type": "plain_text", "text": "Deny"},
                        "style": "danger",
                        "action_id": "hermes_deny",
                        "value": session_key,
                    },
                ],
            },
        ]);

        let mut body = serde_json::json!({
            "channel": chat_id,
            "text": format!("⚠️ Command approval required: {}", &cmd_preview[..cmd_preview.len().min(100)]),
            "blocks": blocks,
        });

        if let Some(ts) = thread_ts {
            body["thread_ts"] = Value::String(ts.to_string());
        }

        let auth = self.auth_header_for_channel(chat_id);
        let resp = self
            .client
            .post(format!("{API_BASE}/chat.postMessage"))
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("send_exec_approval request failed: {e}"))?;

        let resp_body: Value = resp
            .json()
            .await
            .map_err(|e| format!("send_exec_approval parse error: {e}"))?;

        if !resp_body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Slack API error: {err}"));
        }

        let msg_ts = resp_body
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !msg_ts.is_empty() {
            self.approval_resolved
                .lock()
                .await
                .insert(msg_ts.clone(), false);
        }

        // Store approval state.
        self.approval_state.lock().await.insert(approval_id, ApprovalState {
            session_key: session_key.to_string(),
            message_id: msg_ts.clone(),
            chat_id: chat_id.to_string(),
        });

        Ok(msg_ts)
    }

    /// Resolve a pending approval by ID.
    pub async fn resolve_approval(&self, approval_id: u64, choice: &str, user_name: &str) {
        let state = {
            let mut guard = self.approval_state.lock().await;
            guard.remove(&approval_id)
        };
        let Some(state) = state else {
            debug!("[Slack] Approval {approval_id} already resolved or unknown");
            return;
        };
        info!(
            "Slack button resolved approval for session {} (choice={choice}, user={user_name})",
            state.session_key
        );
        let resolved = self.approval_registry.resolve(&state.session_key, choice);
        if !resolved {
            warn!("[Slack] No pending approval found for session {}", state.session_key);
        }
    }

    // ── Clone helper for webhooks ─────────────────────────────────────────

    /// Create a lightweight clone sharing the same state.
    fn clone_like(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            dedup: self.dedup.clone(),
            bot_user_id: self.bot_user_id.clone(),
            user_name_cache: self.user_name_cache.clone(),
            team_tokens: self.team_tokens.clone(),
            team_bot_user_ids: self.team_bot_user_ids.clone(),
            channel_team: self.channel_team.clone(),
            bot_message_ts: self.bot_message_ts.clone(),
            mentioned_threads: self.mentioned_threads.clone(),
            assistant_threads: self.assistant_threads.clone(),
            approval_resolved: self.approval_resolved.clone(),
            approval_state: self.approval_state.clone(),
            approval_counter: self.approval_counter.clone(),
            running: self.running.clone(),
            approval_registry: self.approval_registry.clone(),
        }
    }
}

// ── Webhook handlers ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SlackWebhookPayload {
    #[serde(rename = "type")]
    payload_type: String,
    token: Option<String>,
    challenge: Option<String>,
    event: Option<Value>,
    event_id: Option<String>,
    team_id: Option<String>,
}

async fn handle_slack_webhook(
    State(state): State<Arc<WebhookState>>,
    Json(payload): Json<SlackWebhookPayload>,
) -> (StatusCode, Json<Value>) {
    // URL verification challenge (required for Slack app setup).
    if payload.payload_type == "url_verification" {
        if let Some(challenge) = payload.challenge {
            info!("Slack URL verification challenge received");
            return (StatusCode::OK, Json(serde_json::json!({ "challenge": challenge })));
        }
    }

    // Only process event callbacks.
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
        if let Some(msg_event) = state.adapter.parse_event(&full_event) {
            callback(msg_event);
        }
    }
    drop(guard);

    (StatusCode::OK, Json(serde_json::json!({})))
}

async fn handle_slack_interactive(
    State(state): State<Arc<WebhookState>>,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let payload_str = form.get("payload").map(|s| s.as_str()).unwrap_or("{}");
    let payload: Value = match serde_json::from_str(payload_str) {
        Ok(v) => v,
        Err(e) => {
            warn!("[Slack] Failed to parse interactive payload: {e}");
            return (StatusCode::OK, Json(serde_json::json!({})));
        }
    };

    if let Err(e) = state.adapter.handle_block_action_payload(&payload).await {
        warn!("[Slack] Block action handling error: {e}");
    }

    (StatusCode::OK, Json(serde_json::json!({})))
}

async fn handle_slash_command(
    State(state): State<Arc<WebhookState>>,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let payload = serde_json::json!({
        "command": form.get("command").map(|s| s.as_str()).unwrap_or(""),
        "text": form.get("text").map(|s| s.as_str()).unwrap_or(""),
        "user_id": form.get("user_id").map(|s| s.as_str()).unwrap_or(""),
        "channel_id": form.get("channel_id").map(|s| s.as_str()).unwrap_or(""),
        "team_id": form.get("team_id").map(|s| s.as_str()).unwrap_or(""),
        "trigger_id": form.get("trigger_id").map(|s| s.as_str()).unwrap_or(""),
    });

    let guard = state.on_message.lock().await;
    if let Some(callback) = guard.as_ref() {
        if let Some(event) = state.adapter.parse_slash_command_payload(&payload) {
            callback(event);
        }
    }
    drop(guard);

    (StatusCode::OK, Json(serde_json::json!({"text": "Processing…"})))
}

// ── Helpers ───────────────────────────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────

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

    #[test]
    fn test_connection_mode_default() {
        // Default should be Webhook when env var is absent.
        let mode = SlackConnectionMode::default();
        assert_eq!(mode, SlackConnectionMode::Webhook);
    }

    #[test]
    fn test_approval_choice_map() {
        assert_eq!(
            APPROVAL_CHOICE_MAP.iter().find(|&&(aid, _)| aid == "hermes_approve_once").map(|&(_, c)| c),
            Some("once")
        );
    }
}
