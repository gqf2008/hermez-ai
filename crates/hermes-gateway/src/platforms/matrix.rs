//! Matrix platform adapter.
//!
//! Connects to any Matrix homeserver via the Client-Server HTTP API.
//! Supports sync-based message receiving and room message sending.
//!
//! Does NOT implement end-to-end encryption (E2EE) — encrypted rooms
//! will not work. This is a pragmatic first-pass port of the Python
//! adapter without the heavy mautrix/crypto dependency.
//!
//! Mirrors Python `gateway/platforms/matrix.py` (core sync + send only).

use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::platforms::helpers::ThreadParticipationTracker;
use crate::runner::MessageHandler;
use crate::session::{SessionSource, SessionStore};

// ── Constants ──────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 4000;
const SYNC_TIMEOUT_MS: u64 = 30000;
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
}

impl Default for MatrixConfig {
    fn default() -> Self {
        let free_rooms_raw = std::env::var("MATRIX_FREE_RESPONSE_ROOMS").unwrap_or_default();
        let free_rooms: HashSet<String> = free_rooms_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

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
    client: Client,
    threads: ThreadParticipationTracker,
    processed_events: parking_lot::Mutex<HashSet<String>>,
    joined_rooms: parking_lot::Mutex<HashSet<String>>,
    dm_rooms: parking_lot::Mutex<HashMap<String, bool>>,
    startup_ts: f64,
    next_batch: parking_lot::Mutex<Option<String>>,
}

impl MatrixAdapter {
    pub fn new(config: MatrixConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| Client::new()),
            threads: ThreadParticipationTracker::new("matrix", 5000),
            processed_events: parking_lot::Mutex::new(HashSet::with_capacity(1000)),
            joined_rooms: parking_lot::Mutex::new(HashSet::new()),
            dm_rooms: parking_lot::Mutex::new(HashMap::new()),
            startup_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            next_batch: parking_lot::Mutex::new(None),
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

        // Validate token / login
        if self.config.access_token.is_empty() {
            self._login().await?;
        } else {
            self._whoami().await?;
        }

        // Initial sync to get room list and next_batch token
        let sync_resp = self._sync(None).await?;
        if let Some(rooms) = sync_resp.rooms.as_ref() {
            if let Some(join) = rooms.join.as_ref() {
                let mut joined = self.joined_rooms.lock();
                for room_id in join.keys() {
                    joined.insert(room_id.clone());
                }
            }
            if let Some(invite) = rooms.invite.as_ref() {
                let to_join: Vec<String> = {
                    let joined = self.joined_rooms.lock();
                    invite.keys().filter(|r| !joined.contains(*r)).cloned().collect()
                };
                for room_id in to_join {
                    info!("Matrix: auto-joining invited room {room_id}");
                    let _ = self._join_room(&room_id).await;
                    self.joined_rooms.lock().insert(room_id);
                }
            }
        }
        if let Some(nb) = &sync_resp.next_batch {
            *self.next_batch.lock() = Some(nb.clone());
            info!("Matrix: initial sync complete, joined {} rooms", self.joined_rooms.lock().len());
        }

        self._refresh_dm_cache().await;
        Ok(())
    }

    async fn _whoami(&self) -> Result<WhoamiResponse, String> {
        let url = format!("{}/_matrix/client/v3/account/whoami", self.config.homeserver);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.access_token))
            .send()
            .await
            .map_err(|e| format!("Matrix whoami request failed: {e}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("Matrix whoami parse failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("Matrix whoami failed: {status} — {body}"));
        }

        let user_id = body.get("user_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        info!("Matrix: using access token for {user_id}");
        Ok(WhoamiResponse { user_id })
    }

    async fn _login(&self) -> Result<(), String> {
        let url = format!("{}/_matrix/client/v3/login", self.config.homeserver);
        let payload = serde_json::json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": self.config.user_id,
            },
            "password": self.config.password,
            "initial_device_display_name": "Hermes Agent",
        });

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Matrix login request failed: {e}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("Matrix login parse failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("Matrix login failed: {status} — {body}"));
        }

        let _token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("Matrix login response missing access_token")?;
        // Note: access_token is stored in config; in a real scenario we'd need
        // interior mutability here. For simplicity we skip login-with-password
        // in this simplified adapter or require the caller to set access_token.
        Err("Matrix password login not fully implemented in simplified adapter — use MATRIX_ACCESS_TOKEN".into())
    }

    async fn _sync(&self, since: Option<&str>) -> Result<SyncResponse, String> {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={}",
            self.config.homeserver, SYNC_TIMEOUT_MS
        );
        if let Some(s) = since {
            url.push_str(&format!("&since={}", urlencoding::encode(s)));
        }

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.access_token))
            .send()
            .await
            .map_err(|e| format!("Matrix sync request failed: {e}"))?;

        let status = resp.status();
        let body: SyncResponse = resp.json().await.map_err(|e| format!("Matrix sync parse failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("Matrix sync failed: {status}"));
        }
        Ok(body)
    }

    async fn _join_room(&self, room_id: &str) -> Result<(), String> {
        let url = format!("{}/_matrix/client/v3/rooms/{}/join", self.config.homeserver, urlencoding::encode(room_id));
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.access_token))
            .send()
            .await
            .map_err(|e| format!("Matrix join request failed: {e}"))?;

        if !resp.status().is_success() {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            return Err(format!("Matrix join failed: {body}"));
        }
        Ok(())
    }

    async fn _refresh_dm_cache(&self) {
        // Correct endpoint for m.direct account data:
        let url = format!(
            "{}/_matrix/client/v3/user/{}/account_data/m.direct",
            self.config.homeserver,
            urlencoding::encode(&self.config.user_id),
        );

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.access_token))
            .send()
            .await;

        let dm_data: Option<HashMap<String, Vec<String>>> = match resp {
            Ok(r) if r.status().is_success() => r.json().await.ok(),
            _ => None,
        };

        let mut dm_room_ids = HashSet::new();
        if let Some(data) = dm_data {
            for rooms in data.values() {
                for rid in rooms {
                    dm_room_ids.insert(rid.clone());
                }
            }
        }

        let joined = self.joined_rooms.lock();
        *self.dm_rooms.lock() = joined
            .iter()
            .map(|rid| (rid.clone(), dm_room_ids.contains(rid)))
            .collect();
    }

    // ------------------------------------------------------------------
    // Send
    // ------------------------------------------------------------------

    pub async fn send_text(&self, room_id: &str, text: &str) -> Result<String, String> {
        if text.is_empty() {
            return Ok(String::new());
        }

        let chunks = Self::chunk_text(text, MAX_MESSAGE_LENGTH);
        let mut last_event_id = String::new();

        for chunk in chunks {
            let txn_id = format!("hermes-{}", uuid::Uuid::new_v4());
            let url = format!(
                "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
                self.config.homeserver,
                urlencoding::encode(room_id),
                urlencoding::encode(&txn_id),
            );

            let payload = serde_json::json!({
                "msgtype": "m.text",
                "body": chunk,
            });

            let resp = self
                .client
                .put(&url)
                .header("Authorization", format!("Bearer {}", self.config.access_token))
                .json(&payload)
                .send()
                .await
                .map_err(|e| format!("Matrix send request failed: {e}"))?;

            let status = resp.status();
            let body: serde_json::Value = resp.json().await.map_err(|e| format!("Matrix send parse failed: {e}"))?;

            if !status.is_success() {
                return Err(format!("Matrix send failed: {status} — {body}"));
            }

            last_event_id = body.get("event_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
    // Polling / sync loop
    // ------------------------------------------------------------------

    pub async fn run(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<AtomicBool>,
        running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: Arc<SessionStore>,
        default_model: String,
        per_chat_model: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) {
        let mut poll_interval = interval(Duration::from_millis(100));
        let mut consecutive_errors = 0u32;

        if self.config.encryption {
            #[cfg(feature = "matrix-e2ee")]
            info!("Matrix E2EE enabled (matrix-sdk path)");
            #[cfg(not(feature = "matrix-e2ee"))]
            warn!("Matrix E2EE requested (MATRIX_ENCRYPTION=true) but the 'matrix-e2ee' feature is not enabled. Encrypted rooms will NOT work. Rebuild with --features matrix-e2ee to enable E2EE support.");
        }

        info!("Matrix sync loop started");

        while running.load(Ordering::SeqCst) {
            poll_interval.tick().await;

            let next_batch = self.next_batch.lock().clone();
            match self._sync(next_batch.as_deref()).await {
                Ok(sync_resp) => {
                    consecutive_errors = 0;
                    *self.next_batch.lock() = sync_resp.next_batch.clone();

                    // Update joined rooms
                    if let Some(rooms) = sync_resp.rooms.as_ref() {
                        {
                            let mut joined = self.joined_rooms.lock();
                            if let Some(join) = rooms.join.as_ref() {
                                for room_id in join.keys() {
                                    joined.insert(room_id.clone());
                                }
                            }
                        }
                        if let Some(invite) = rooms.invite.as_ref() {
                            let to_join: Vec<String> = {
                                let joined = self.joined_rooms.lock();
                                invite.keys().filter(|r| !joined.contains(*r)).cloned().collect()
                            };
                            for room_id in to_join {
                                info!("Matrix: auto-joining invited room {room_id}");
                                let _ = self._join_room(&room_id).await;
                                self.joined_rooms.lock().insert(room_id);
                            }
                        }
                    }

                    // Process timeline events
                    if let Some(rooms) = sync_resp.rooms.as_ref() {
                        if let Some(join) = rooms.join.as_ref() {
                            for (room_id, room_data) in join {
                                if let Some(timeline) = room_data.timeline.as_ref() {
                                    for event in &timeline.events {
                                        self._process_event(
                                            room_id, event, &handler, &running,
                                            &running_sessions, &busy_ack_ts, &session_store,
                                            &default_model, &per_chat_model,
                                        ).await;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let err_lower = e.to_lowercase();
                    if err_lower.contains("401")
                        || err_lower.contains("403")
                        || err_lower.contains("unauthorized")
                        || err_lower.contains("forbidden")
                        || err_lower.contains("m_unknown_token")
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
            }
        }

        info!("Matrix sync loop stopped");
    }

    // ------------------------------------------------------------------
    // Event processing
    // ------------------------------------------------------------------

    async fn _process_event(
        &self,
        room_id: &str,
        event: &serde_json::Value,
        handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: &Arc<AtomicBool>,
        running_sessions: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: &Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: &Arc<SessionStore>,
        default_model: &str,
        per_chat_model: &Arc<parking_lot::Mutex<HashMap<String, String>>>,
    ) {
        let sender = event.get("sender").and_then(|v| v.as_str()).unwrap_or("");
        if sender == self.config.user_id {
            return; // Ignore own messages
        }

        let event_id = event.get("event_id").and_then(|v| v.as_str()).unwrap_or("");
        if event_id.is_empty() || self._is_duplicate_event(event_id) {
            return;
        }

        // Startup grace
        let raw_ts = event
            .get("origin_server_ts")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let event_ts = raw_ts as f64 / 1000.0;
        if event_ts > 0.0 && event_ts < self.startup_ts - STARTUP_GRACE_SECONDS {
            return;
        }

        let content = match event.get("content") {
            Some(c) => c,
            None => return,
        };

        let msgtype = content.get("msgtype").and_then(|v| v.as_str()).unwrap_or("");
        if msgtype != "m.text" {
            return;
        }

        // Skip edits
        let relates_to = content.get("m.relates_to");
        if let Some(rt) = relates_to {
            if rt.get("rel_type").and_then(|v| v.as_str()) == Some("m.replace") {
                return;
            }
        }

        // Skip m.notice (bot responses)
        if msgtype == "m.notice" {
            return;
        }

        let body = content.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if body.is_empty() {
            return;
        }

        let is_dm = self._is_dm_room(room_id).await;
        let chat_type = if is_dm { "dm" } else { "group" };

        // Mention gating
        let formatted_body = content.get("formatted_body").and_then(|v| v.as_str());
        let mentions = content.get("m.mentions");
        let is_mentioned = self._is_bot_mentioned(body, formatted_body, mentions);

        if !is_dm {
            let is_free_room = self.config.free_rooms.contains(room_id);
            let in_bot_thread = false; // Simplified — no thread tracking for now
            if self.config.require_mention && !is_free_room && !in_bot_thread && !is_mentioned {
                return;
            }
        }

        let display_name = self._get_display_name(room_id, sender).await;
        let thread_id = if self.config.auto_thread && !is_dm {
            Some(event_id.to_string())
        } else {
            None
        };
        if let Some(ref tid) = thread_id {
            self.threads.mark(tid);
        }

        let body = if is_mentioned && self.config.require_mention {
            self._strip_mention(body)
        } else {
            body.to_string()
        };

        // Send read receipt (fire-and-forget)
        let _ = self._send_read_receipt(room_id, event_id).await;

        info!(
            "Matrix message from {} in {}: {}",
            display_name, room_id, &body[..body.len().min(80)],
        );

        // Route to handler
        route_matrix_message(
            self, room_id, &body, sender, &display_name, chat_type,
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

    async fn _is_dm_room(&self, room_id: &str) -> bool {
        let dm_rooms = self.dm_rooms.lock();
        if let Some(&is_dm) = dm_rooms.get(room_id) {
            return is_dm;
        }
        false
    }

    fn _is_bot_mentioned(
        &self,
        body: &str,
        formatted_body: Option<&str>,
        mentions: Option<&serde_json::Value>,
    ) -> bool {
        // MSC3952 m.mentions.user_ids
        if let Some(m) = mentions {
            if let Some(user_ids) = m.get("user_ids").and_then(|v| v.as_array()) {
                for uid in user_ids {
                    if let Some(s) = uid.as_str() {
                        if s == self.config.user_id {
                            return true;
                        }
                    }
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

    async fn _get_display_name(&self, _room_id: &str, user_id: &str) -> String {
        if user_id.starts_with('@') && user_id.contains(':') {
            return user_id[1..].split(':').next().unwrap_or(user_id).to_string();
        }
        user_id.to_string()
    }

    async fn _send_read_receipt(&self, room_id: &str, event_id: &str) -> Result<(), String> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/receipt/m.read/{}",
            self.config.homeserver,
            urlencoding::encode(room_id),
            urlencoding::encode(event_id),
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.access_token))
            .send()
            .await
            .map_err(|e| format!("Matrix read receipt failed: {e}"))?;
        if !resp.status().is_success() {
            debug!("Matrix: read receipt failed for {event_id}");
        }
        Ok(())
    }
}

// ── Sync response types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
struct SyncResponse {
    next_batch: Option<String>,
    rooms: Option<Rooms>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Rooms {
    join: Option<HashMap<String, JoinedRoom>>,
    invite: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct JoinedRoom {
    timeline: Option<Timeline>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Timeline {
    events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhoamiResponse {
    user_id: String,
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
    handler: &Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
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

    // Busy session check
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
