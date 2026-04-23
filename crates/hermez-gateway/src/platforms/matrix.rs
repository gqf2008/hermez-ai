//! Matrix platform adapter with E2EE support via matrix-sdk 0.16.
//!
//! Connects to any Matrix homeserver via the matrix-sdk Client API.
//! Supports sync-based message receiving, room message sending, and
//! end-to-end encryption (E2EE) via the SDK's built-in Olm/Megolm
//! implementation backed by SQLite.
//!
//! Replaces the previous pure-HTTP polling adapter.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use matrix_sdk::{
    Client, Room, SessionMeta, SessionTokens,
    config::SyncSettings,
    authentication::matrix::MatrixSession,
};
use matrix_sdk::ruma::{
    events::room::message::{RoomMessageEventContent, MessageType as RumaMessageType, Relation, SyncRoomMessageEvent},
    api::client::receipt::create_receipt::v3::ReceiptType,
    events::receipt::ReceiptThread,
    OwnedRoomId, RoomId, UserId, EventId,
};

use tokio::sync::Mutex as TokioMutex;
use tokio::time::Duration;
use tracing::{error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::ThreadParticipationTracker;
use crate::runner::MessageHandler;
use crate::session::{SessionSource, SessionStore};

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 4000;
const STARTUP_GRACE_SECONDS: f64 = 5.0;

// ── Configuration ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MatrixConfig {
    pub homeserver: String,
    pub access_token: String,
    pub user_id: String,
    pub password: String,
    pub device_id: String,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub require_mention: bool,
    pub free_rooms: HashSet<String>,
    pub auto_thread: bool,
    pub reactions_enabled: bool,
    pub home_channel: Option<String>,
    pub encryption: bool,
    pub store_path: PathBuf,
}

impl Default for MatrixConfig {
    fn default() -> Self {
        let free_rooms_raw = std::env::var("MATRIX_FREE_RESPONSE_ROOMS").unwrap_or_default();
        let free_rooms: HashSet<String> = free_rooms_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let store_path = hermez_core::get_hermez_home().join("matrix_store");

        Self {
            homeserver: std::env::var("MATRIX_HOMESERVER").unwrap_or_default().trim_end_matches('/').to_string(),
            access_token: std::env::var("MATRIX_ACCESS_TOKEN").unwrap_or_default(),
            user_id: std::env::var("MATRIX_USER_ID").unwrap_or_default(),
            password: std::env::var("MATRIX_PASSWORD").unwrap_or_default(),
            device_id: std::env::var("MATRIX_DEVICE_ID").unwrap_or_default(),
            allowed_users: std::env::var("MATRIX_ALLOWED_USERS")
                .ok()
                .map(|s| s.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect())
                .unwrap_or_default(),
            allow_all_users: std::env::var("MATRIX_ALLOW_ALL_USERS")
                .ok()
                .map(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false),
            require_mention: std::env::var("MATRIX_REQUIRE_MENTION")
                .ok()
                .map(|s| !matches!(s.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true),
            free_rooms,
            auto_thread: std::env::var("MATRIX_AUTO_THREAD")
                .ok()
                .map(|s| !matches!(s.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true),
            reactions_enabled: std::env::var("MATRIX_REACTIONS")
                .ok()
                .map(|s| !matches!(s.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true),
            home_channel: std::env::var("MATRIX_HOME_ROOM").ok(),
            encryption: std::env::var("MATRIX_ENCRYPTION")
                .ok()
                .map(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false),
            store_path,
        }
    }
}

impl MatrixConfig {
    pub fn from_env() -> Self {
        Self::default()
    }

    pub fn from_extra(extra: &HashMap<String, serde_json::Value>) -> Self {
        let mut cfg = Self::from_env();
        if let Some(v) = extra.get("homeserver").and_then(|v| v.as_str()) {
            cfg.homeserver = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = extra.get("user_id").and_then(|v| v.as_str()) {
            cfg.user_id = v.to_string();
        }
        if let Some(v) = extra.get("password").and_then(|v| v.as_str()) {
            cfg.password = v.to_string();
        }
        if let Some(v) = extra.get("device_id").and_then(|v| v.as_str()) {
            cfg.device_id = v.to_string();
        }
        cfg
    }

    pub fn is_configured(&self) -> bool {
        !self.homeserver.is_empty()
            && (!self.access_token.is_empty() || (!self.user_id.is_empty() && !self.password.is_empty()))
    }
}

// ── Adapter ────────────────────────────────────────────────────────────────

pub struct MatrixAdapter {
    config: MatrixConfig,
    client: tokio::sync::Mutex<Option<Client>>,
    threads: ThreadParticipationTracker,
    processed_events: parking_lot::Mutex<HashSet<String>>,
    startup_ts: f64,
}

impl MatrixAdapter {
    pub fn new(config: MatrixConfig) -> Self {
        Self {
            client: tokio::sync::Mutex::new(None),
            threads: ThreadParticipationTracker::new("matrix", 5000),
            processed_events: parking_lot::Mutex::new(HashSet::with_capacity(1000)),
            startup_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            config,
        }
    }

    // ------------------------------------------------------------------
    // Connection
    // ------------------------------------------------------------------

    pub async fn connect(&self) -> Result<(), String> {
        if !self.config.is_configured() {
            return Err("Matrix: not configured (need homeserver + access_token or user_id+password)".into());
        }

        let store_path = &self.config.store_path;
        std::fs::create_dir_all(store_path)
            .map_err(|e| format!("Matrix store dir creation failed: {e}"))?;

        let client = Client::builder()
            .homeserver_url(&self.config.homeserver)
            .sqlite_store(store_path, None)
            .build()
            .await
            .map_err(|e| format!("Matrix client build failed: {e}"))?;

        let session_file = hermez_core::get_hermez_home().join("matrix_session.json");

        let restored = if session_file.exists() {
            match Self::_restore_session(&client, &session_file).await {
                Ok(()) => {
                    info!("Matrix: session restored from {}", session_file.display());
                    true
                }
                Err(e) => {
                    warn!("Matrix: session restore failed: {e}, falling back to login");
                    false
                }
            }
        } else {
            false
        };

        if !restored {
            if !self.config.access_token.is_empty() {
                Self::_login_with_token(&client, &self.config).await?;
            } else if !self.config.user_id.is_empty() && !self.config.password.is_empty() {
                Self::_login_with_password(&client, &self.config).await?;
            } else {
                return Err("Matrix: no access token or password available".into());
            }
            if let Err(e) = Self::_save_session(&client, &session_file).await {
                warn!("Matrix: failed to save session: {e}");
            }
        }

        info!("Matrix: logged in as {}", self.config.user_id);

        // E2EE initialization (best-effort)
        client.encryption().wait_for_e2ee_initialization_tasks().await;
        info!("Matrix: E2EE initialization complete");

        // Initial sync
        info!("Matrix: performing initial sync...");
        client.sync_once(SyncSettings::default()).await
            .map_err(|e| format!("Matrix initial sync failed: {e}"))?;

        // Auto-join invites
        for room in client.invited_rooms() {
            info!("Matrix: auto-joining invited room {}", room.room_id());
            let _ = room.join().await;
        }

        info!("Matrix: connect complete, joined {} rooms", client.joined_rooms().len());
        *self.client.lock().await = Some(client);
        Ok(())
    }

    async fn _login_with_token(client: &Client, config: &MatrixConfig) -> Result<(), String> {
        let user_id = UserId::parse(&config.user_id)
            .map_err(|e| format!("Invalid user_id: {e}"))?;
        let session = MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: config.device_id.clone().into(),
            },
            tokens: SessionTokens {
                access_token: config.access_token.clone(),
                refresh_token: None,
            },
        };
        client.matrix_auth().restore_session(session, matrix_sdk::store::RoomLoadSettings::All).await
            .map_err(|e| format!("Matrix session restore failed: {e}"))?;
        Ok(())
    }

    async fn _login_with_password(client: &Client, config: &MatrixConfig) -> Result<(), String> {
        let mut builder = client.matrix_auth()
            .login_username(&config.user_id, &config.password)
            .initial_device_display_name("Hermez Agent");
        if !config.device_id.is_empty() {
            builder = builder.device_id(&config.device_id);
        }
        let response = builder.send().await
            .map_err(|e| format!("Matrix password login failed: {e}"))?;
        info!("Matrix: password login successful, device_id={}", response.device_id);
        Ok(())
    }

    async fn _save_session(client: &Client, path: &PathBuf) -> Result<(), String> {
        let auth = client.matrix_auth();
        let session = auth.session()
            .ok_or("No active session to save")?;
        let json = serde_json::to_string_pretty(&session)
            .map_err(|e| format!("Session serialization failed: {e}"))?;
        tokio::fs::write(path, json).await
            .map_err(|e| format!("Session write failed: {e}"))?;
        Ok(())
    }

    async fn _restore_session(client: &Client, path: &PathBuf) -> Result<(), String> {
        let json = tokio::fs::read_to_string(path).await
            .map_err(|e| format!("Session read failed: {e}"))?;
        let session: MatrixSession = serde_json::from_str(&json)
            .map_err(|e| format!("Session deserialization failed: {e}"))?;
        client.matrix_auth().restore_session(session, matrix_sdk::store::RoomLoadSettings::All).await
            .map_err(|e| format!("Session restore failed: {e}"))?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Send
    // ------------------------------------------------------------------

    pub async fn send_text(&self, room_id: &str, text: &str) -> Result<String, String> {
        if text.is_empty() {
            return Ok(String::new());
        }

        let client = {
            let guard = self.client.lock().await;
            guard.as_ref().cloned().ok_or("Matrix client not connected")?
        };

        let room = client
            .get_room(&RoomId::parse(room_id).map_err(|e| format!("Invalid room_id: {e}"))?)
            .ok_or_else(|| format!("Room {room_id} not found"))?;

        let chunks = Self::chunk_text(text, MAX_MESSAGE_LENGTH);
        let mut last_event_id = String::new();

        for chunk in chunks {
            let content = RoomMessageEventContent::text_plain(chunk);
            let response = room.send(content).await
                .map_err(|e| format!("Matrix send failed: {e}"))?;
            last_event_id = response.event_id.to_string();
        }

        Ok(last_event_id)
    }

    fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
        if text.len() <= max_len {
            return vec![text.to_string()];
        }
        let mut chunks = Vec::new();
        let mut remaining = text;
        while !remaining.is_empty() {
            let split_at = if remaining.len() <= max_len {
                remaining.len()
            } else {
                let mut pos = max_len;
                while pos > 0 && !remaining.is_char_boundary(pos) {
                    pos -= 1;
                }
                if pos == 0 { pos = max_len; }
                pos
            };
            let (chunk, rest) = remaining.split_at(split_at);
            chunks.push(chunk.to_string());
            remaining = rest;
        }
        chunks
    }

    // ------------------------------------------------------------------
    // Sync loop
    // ------------------------------------------------------------------

    pub async fn run(
        &self,
        handler: Arc<TokioMutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<AtomicBool>,
        running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: Arc<SessionStore>,
        default_model: String,
        per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) {
        let client = {
            let guard = self.client.lock().await;
            match guard.as_ref().cloned() {
                Some(c) => c,
                None => {
                    error!("Matrix: run() called before connect()");
                    return;
                }
            }
        };

        let mut sync_stream = Box::pin(client.sync_stream(SyncSettings::default()).await);
        let mut consecutive_errors = 0u32;

        info!("Matrix sync loop started");

        while running.load(Ordering::SeqCst) {
            tokio::select! {
                next = sync_stream.next() => {
                    match next {
                        Some(Ok(response)) => {
                            consecutive_errors = 0;

                            for (room_id, room_update) in &response.rooms.joined {
                                for timeline_event in &room_update.timeline.events {
                                    self._process_timeline_event(
                                        room_id, timeline_event, &client,
                                        &handler, &running, &running_sessions,
                                        &busy_ack_ts, &session_store,
                                        &default_model, &per_chat_model,
                                    ).await;
                                }
                            }

                            for room in client.invited_rooms() {
                                info!("Matrix: auto-joining invited room {}", room.room_id());
                                let _ = room.join().await;
                            }
                        }
                        Some(Err(e)) => {
                            let err_str = e.to_string();
                            if err_str.contains("401")
                                || err_str.contains("403")
                                || err_str.contains("Unauthorized")
                                || err_str.contains("Forbidden")
                                || err_str.contains("M_UNKNOWN_TOKEN")
                            {
                                error!("Matrix: permanent auth error — stopping sync: {e}");
                                return;
                            }
                            consecutive_errors += 1;
                            if consecutive_errors > 5 {
                                warn!("Matrix: {consecutive_errors} consecutive sync errors: {e}");
                            } else {
                                error!("Matrix sync error: {e}");
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        None => {
                            warn!("Matrix sync stream ended");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }

        info!("Matrix sync loop stopped");
    }

    // ------------------------------------------------------------------
    // Event processing
    // ------------------------------------------------------------------

    async fn _process_timeline_event(
        &self,
        room_id: &OwnedRoomId,
        timeline_event: &matrix_sdk::deserialized_responses::TimelineEvent,
        client: &Client,
        handler: &Arc<TokioMutex<Option<Arc<dyn MessageHandler>>>>,
        running: &Arc<AtomicBool>,
        running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: &Arc<SessionStore>,
        default_model: &str,
        per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) {
        let raw = timeline_event.kind.raw();
        let msg_event = match raw.deserialize() {
            Ok(matrix_sdk::ruma::events::AnySyncTimelineEvent::MessageLike(
                matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(
                    SyncRoomMessageEvent::Original(ev)
                )
            )) => ev,
            _ => return,
        };

        let sender = msg_event.sender.as_str();
        if sender == self.config.user_id {
            return;
        }

        let event_id = msg_event.event_id.as_str();
        if event_id.is_empty() || self._is_duplicate_event(event_id) {
            return;
        }

        // Startup grace
        let event_ts = f64::from(msg_event.origin_server_ts.as_secs());
        if event_ts > 0.0 && event_ts < self.startup_ts - STARTUP_GRACE_SECONDS {
            return;
        }

        let content = &msg_event.content;
        let body = match &content.msgtype {
            RumaMessageType::Text(t) => t.body.as_str(),
            _ => return,
        };

        // Skip edits
        if matches!(content.relates_to, Some(Relation::Replacement(_))) {
            return;
        }

        // Skip notices
        if matches!(&content.msgtype, RumaMessageType::Notice(_)) {
            return;
        }

        if body.is_empty() {
            return;
        }

        // DM detection
        let room = match client.get_room(room_id) {
            Some(r) => r,
            None => return,
        };
        let is_dm = room.is_direct().await.unwrap_or(false);
        let chat_type = if is_dm { "dm" } else { "group" };

        // Mention gating
        let formatted_body = match &content.msgtype {
            RumaMessageType::Text(t) => t.formatted.as_ref().map(|f| f.body.as_str()),
            _ => None,
        };
        let is_mentioned = self._is_bot_mentioned(body, formatted_body, content.mentions.as_ref());

        if !is_dm {
            let is_free_room = self.config.free_rooms.contains(room_id.as_str());
            let in_bot_thread = false;
            if self.config.require_mention && !is_free_room && !in_bot_thread && !is_mentioned {
                return;
            }
        }

        let display_name = self._get_display_name(&room, sender).await;
        let thread_id = if self.config.auto_thread && !is_dm {
            Some(event_id.to_string())
        } else {
            None
        };
        if let Some(tid) = &thread_id {
            self.threads.mark(tid);
        }

        let body = if is_mentioned && self.config.require_mention {
            self._strip_mention(body)
        } else {
            body.to_string()
        };

        // Send read receipt (fire-and-forget)
        if let Ok(event_id_parsed) = EventId::parse(event_id) {
            let _ = room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id_parsed).await;
        }

        info!(
            "Matrix message from {} in {}: {}",
            display_name, room_id, &body[..body.len().min(80)],
        );

        route_matrix_message(
            self, room_id.as_str(), &body, sender, &display_name, chat_type,
            thread_id.as_deref(), handler, running, running_sessions,
            busy_ack_ts, session_store, default_model, per_chat_model,
        ).await;
    }

    fn _is_duplicate_event(&self, event_id: &str) -> bool {
        if event_id.is_empty() {
            return false;
        }
        let mut processed = self.processed_events.lock();
        if processed.contains(event_id) {
            return true;
        }
        if processed.len() >= 1000 {
            processed.clear();
        }
        processed.insert(event_id.to_string());
        false
    }

    fn _is_bot_mentioned(
        &self,
        body: &str,
        formatted_body: Option<&str>,
        mentions: Option<&matrix_sdk::ruma::events::Mentions>,
    ) -> bool {
        if let Some(m) = mentions {
            if let Ok(user_id) = UserId::parse(&self.config.user_id) {
                if m.user_ids.contains(&user_id) {
                    return true;
                }
            }
        }
        if body.contains(&self.config.user_id) {
            return true;
        }
        if let Some(localpart) = self.config.user_id.split(':').next() {
            let lp = localpart.trim_start_matches('@');
            if !lp.is_empty() && body.to_lowercase().contains(&lp.to_lowercase()) {
                return true;
            }
        }
        if let Some(fb) = formatted_body {
            if fb.contains(&format!("matrix.to/#/{}", self.config.user_id)) {
                return true;
            }
        }
        false
    }

    fn _strip_mention(&self, body: &str) -> String {
        body.replace(&self.config.user_id, "").trim().to_string()
    }

    async fn _get_display_name(&self, _room: &Room, user_id: &str) -> String {
        if user_id.starts_with('@') && user_id.contains(':') {
            return user_id[1..].split(':').next().unwrap_or(user_id).to_string();
        }
        user_id.to_string()
    }
}

// ── Message routing ────────────────────────────────────────────────────────

async fn route_matrix_message(
    adapter: &MatrixAdapter,
    room_id: &str,
    content: &str,
    sender: &str,
    display_name: &str,
    chat_type: &str,
    thread_id: Option<&str>,
    handler: &Arc<TokioMutex<Option<Arc<dyn MessageHandler>>>>,
    _running: &Arc<AtomicBool>,
    running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: &Arc<SessionStore>,
    _default_model: &str,
    per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let chat_id = room_id;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let busy_elapsed_min: Option<f64> = {
        let sessions = running_sessions.lock();
        sessions.get(chat_id).map(|&start_ts| (now - start_ts) / 60.0)
    };

    if let Some(elapsed_min) = busy_elapsed_min {
        let should_ack = {
            let mut ack_map = busy_ack_ts.lock();
            let last_ack = ack_map.get(chat_id).copied().unwrap_or(0.0);
            if now - last_ack < 30.0 {
                false
            } else {
                ack_map.insert(chat_id.to_string(), now);
                true
            }
        };
        if should_ack {
            let handler_guard = handler.lock().await;
            if let Some(h) = handler_guard.as_ref() {
                h.interrupt(chat_id, content);
            }
            drop(handler_guard);
            info!("Session {chat_id}: busy — agent interrupted after {elapsed_min:.1} min");
            let busy_msg = format!(
                "Still processing your previous message ({:.0}m elapsed). \
                 Please wait for my response before sending another prompt.",
                elapsed_min
            );
            let _ = adapter.send_text(chat_id, &busy_msg).await;
        }
        return;
    }

    let handler_guard = handler.lock().await;
    let Some(handler_ref) = handler_guard.as_ref().cloned() else {
        warn!("No message handler registered for Matrix messages");
        return;
    };
    drop(handler_guard);

    {
        let mut sessions = running_sessions.lock();
        sessions.insert(chat_id.to_string(), now);
    }

    let model_override = per_chat_model.lock().get(chat_id).cloned();
    match handler_ref.handle_message(Platform::Matrix, chat_id, content, model_override.as_deref()).await {
        Ok(result) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);

            if result.compression_exhausted {
                let source = SessionSource {
                    platform: Platform::Matrix,
                    chat_id: chat_id.to_string(),
                    chat_name: None,
                    chat_type: chat_type.to_string(),
                    user_id: Some(sender.to_string()),
                    user_name: Some(display_name.to_string()),
                    thread_id: thread_id.map(|s| s.to_string()),
                    chat_topic: None,
                    user_id_alt: None,
                    chat_id_alt: None,
                };
                session_store.reset_session_for(&source);
                let _ = adapter.send_text(chat_id, "Session reset: conversation context grew too large. Starting fresh.").await;
            }
            if !result.response.is_empty() {
                if let Err(e) = adapter.send_text(chat_id, &result.response).await {
                    error!("Matrix send failed: {e}");
                }
            }
        }
        Err(e) => {
            running_sessions.lock().remove(chat_id);
            busy_ack_ts.lock().remove(chat_id);
            error!("Agent handler failed for Matrix message: {e}");
            let _ = adapter.send_text(chat_id, "Sorry, I encountered an error processing your message.").await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Config tests ──────────────────────────────────────────────────────

    #[test]
    fn test_matrix_config_default_empty() {
        // Ensure default() doesn't panic even when env vars are missing.
        let cfg = MatrixConfig::default();
        assert!(cfg.homeserver.is_empty());
        assert!(cfg.access_token.is_empty());
        assert!(cfg.user_id.is_empty());
        assert!(!cfg.allow_all_users);
        assert!(cfg.require_mention);
        assert!(cfg.auto_thread);
        assert!(cfg.reactions_enabled);
        assert!(cfg.allowed_users.is_empty());
        assert!(!cfg.is_configured());
    }

    #[test]
    fn test_matrix_config_is_configured_token() {
        let cfg = MatrixConfig {
            homeserver: "https://example.com".to_string(),
            access_token: "tok123".to_string(),
            ..Default::default()
        };
        assert!(cfg.is_configured());
    }

    #[test]
    fn test_matrix_config_is_configured_password() {
        let cfg = MatrixConfig {
            homeserver: "https://example.com".to_string(),
            user_id: "@user:example.com".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        };
        assert!(cfg.is_configured());
    }

    #[test]
    fn test_matrix_config_is_configured_neither() {
        let cfg = MatrixConfig {
            homeserver: "https://example.com".to_string(),
            ..Default::default()
        };
        // No access_token and no password => not configured
        assert!(!cfg.is_configured());
    }

    #[test]
    fn test_matrix_config_from_extra() {
        let mut extra = HashMap::new();
        extra.insert("homeserver".to_string(), serde_json::json!("https://matrix.org"));
        extra.insert("user_id".to_string(), serde_json::json!("@alice:matrix.org"));
        extra.insert("password".to_string(), serde_json::json!("hunter2"));
        extra.insert("device_id".to_string(), serde_json::json!("DEVICE01"));

        let cfg = MatrixConfig::from_extra(&extra);
        assert_eq!(cfg.homeserver, "https://matrix.org");
        assert_eq!(cfg.user_id, "@alice:matrix.org");
        assert_eq!(cfg.password, "hunter2");
        assert_eq!(cfg.device_id, "DEVICE01");
        assert!(cfg.is_configured());
    }

    // ── chunk_text tests ──────────────────────────────────────────────────

    #[test]
    fn test_chunk_text_no_split() {
        let chunks = MatrixAdapter::chunk_text("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_chunk_text_exact_boundary() {
        let text = "a".repeat(4000);
        let chunks = MatrixAdapter::chunk_text(&text, 4000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 4000);
    }

    #[test]
    fn test_chunk_text_splits() {
        let text = "Hello world! ".repeat(500); // > 4000 chars
        let chunks = MatrixAdapter::chunk_text(&text, 100);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 100);
        }
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn test_chunk_text_unicode_boundary() {
        let text = "α".repeat(20); // 2 bytes each = 40 bytes
        let chunks = MatrixAdapter::chunk_text(&text, 10);
        assert_eq!(chunks.len(), 4); // 40/10 = 4
        for chunk in &chunks {
            assert!(chunk.len() <= 10);
        }
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }

    // ── _is_duplicate_event tests ─────────────────────────────────────────

    #[test]
    fn test_is_duplicate_event_empty() {
        let cfg = MatrixConfig::default();
        let adapter = MatrixAdapter::new(cfg);
        assert!(!adapter._is_duplicate_event(""));
    }

    #[test]
    fn test_is_duplicate_event_deduplication() {
        let cfg = MatrixConfig::default();
        let adapter = MatrixAdapter::new(cfg);
        assert!(!adapter._is_duplicate_event("evt_1"));
        assert!(adapter._is_duplicate_event("evt_1"));
        assert!(!adapter._is_duplicate_event("evt_2"));
    }

    #[test]
    fn test_is_duplicate_event_clear_at_limit() {
        let cfg = MatrixConfig::default();
        let adapter = MatrixAdapter::new(cfg);
        for i in 0..1001 {
            adapter._is_duplicate_event(&format!("evt_{i}"));
        }
        // After 1001 entries the set should have been cleared
        assert!(!adapter._is_duplicate_event("evt_0"));
    }

    // ── _is_bot_mentioned tests (no Mentions struct) ──────────────────────

    #[test]
    fn test_is_bot_mentioned_by_user_id() {
        let cfg = MatrixConfig {
            user_id: "@bot:example.com".to_string(),
            ..Default::default()
        };
        let adapter = MatrixAdapter::new(cfg);
        assert!(adapter._is_bot_mentioned("hello @bot:example.com", None, None));
        assert!(!adapter._is_bot_mentioned("hello @other:example.com", None, None));
    }

    #[test]
    fn test_is_bot_mentioned_by_localpart() {
        let cfg = MatrixConfig {
            user_id: "@hermez_bot:example.com".to_string(),
            ..Default::default()
        };
        let adapter = MatrixAdapter::new(cfg);
        assert!(adapter._is_bot_mentioned("hey hermez_bot", None, None));
        assert!(adapter._is_bot_mentioned("HErMeZ_BOt", None, None)); // case-insensitive
        assert!(!adapter._is_bot_mentioned("hey other_bot", None, None));
    }

    #[test]
    fn test_is_bot_mentioned_by_matrix_to_link() {
        let cfg = MatrixConfig {
            user_id: "@bot:example.com".to_string(),
            ..Default::default()
        };
        let adapter = MatrixAdapter::new(cfg);
        assert!(adapter._is_bot_mentioned(
            "hello",
            Some("Check out matrix.to/#/@bot:example.com"),
            None,
        ));
    }

    // ── _strip_mention tests ──────────────────────────────────────────────

    #[test]
    fn test_strip_mention() {
        let cfg = MatrixConfig {
            user_id: "@bot:example.com".to_string(),
            ..Default::default()
        };
        let adapter = MatrixAdapter::new(cfg);
        assert_eq!(
            adapter._strip_mention("@bot:example.com hello"),
            "hello"
        );
        assert_eq!(
            adapter._strip_mention("hello @bot:example.com world"),
            "hello  world"
        );
    }

}
