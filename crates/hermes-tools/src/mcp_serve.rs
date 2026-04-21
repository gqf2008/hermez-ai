//! Hermes MCP Server — expose messaging conversations as MCP tools.
//!
//! Starts a stdio MCP server that lets any MCP client (Claude Code, Cursor, Codex,
//! etc.) list conversations, read message history, send messages, poll for live
//! events, and manage approval requests across all connected platforms.
//!
//! Mirrors the Python `mcp_serve.py` surface:
//!   conversations_list, conversation_get, messages_read, attachments_fetch,
//!   events_poll, events_wait, messages_send, channels_list,
//!   permissions_list_open, permissions_respond

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::time::interval;

use rmcp::{
    ServerHandler,
    model::{ServerCapabilities, ServerInfo},
    tool,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const QUEUE_LIMIT: usize = 1000;
const POLL_INTERVAL_MS: u64 = 200;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_sessions_dir() -> PathBuf {
    hermes_core::get_hermes_home().join("sessions")
}

fn load_sessions_index() -> Value {
    let sessions_file = get_sessions_dir().join("sessions.json");
    if !sessions_file.exists() {
        return Value::Object(serde_json::Map::new());
    }
    match std::fs::read_to_string(&sessions_file) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| {
            Value::Object(serde_json::Map::new())
        }),
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

fn load_channel_directory() -> Value {
    let directory_file = hermes_core::get_hermes_home().join("channel_directory.json");
    if !directory_file.exists() {
        return Value::Object(serde_json::Map::new());
    }
    match std::fs::read_to_string(&directory_file) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| {
            Value::Object(serde_json::Map::new())
        }),
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

fn extract_message_content(msg: &hermes_state::Message) -> String {
    msg.content.clone().unwrap_or_default()
}

fn extract_attachments(msg: &hermes_state::Message) -> Vec<Value> {
    let mut attachments = Vec::new();
    let text = extract_message_content(msg);

    // MEDIA: tags in text content
    if !text.is_empty() {
        let media_pattern = Regex::new(r"MEDIA:\s*(\S+)").unwrap();
        for cap in media_pattern.captures_iter(&text) {
            if let Some(path) = cap.get(1) {
                attachments.push(serde_json::json!({
                    "type": "media",
                    "path": path.as_str(),
                }));
            }
        }
    }

    attachments
}

fn file_mtime(path: &std::path::Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------------------
// Event Bridge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct QueueEvent {
    cursor: usize,
    event_type: String,
    session_key: String,
    data: Value,
}

/// Background poller that watches SessionDB for new messages and
/// maintains an in-memory event queue with waiter support.
pub struct EventBridge {
    queue: Mutex<Vec<QueueEvent>>,
    cursor: Mutex<usize>,
    notify: Notify,
    last_poll_timestamps: Mutex<HashMap<String, f64>>,
    pending_approvals: Mutex<HashMap<String, Value>>,
    sessions_json_mtime: Mutex<std::time::SystemTime>,
    state_db_mtime: Mutex<std::time::SystemTime>,
    cached_sessions_index: Mutex<Value>,
    db: Mutex<Option<hermes_state::SessionDB>>,
}

impl std::fmt::Debug for EventBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBridge").finish_non_exhaustive()
    }
}

impl EventBridge {
    pub fn new() -> Self {
        let db = hermes_state::SessionDB::open_default().ok();
        Self {
            queue: Mutex::new(Vec::new()),
            cursor: Mutex::new(0),
            notify: Notify::new(),
            last_poll_timestamps: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            sessions_json_mtime: Mutex::new(std::time::SystemTime::UNIX_EPOCH),
            state_db_mtime: Mutex::new(std::time::SystemTime::UNIX_EPOCH),
            cached_sessions_index: Mutex::new(Value::Object(serde_json::Map::new())),
            db: Mutex::new(db),
        }
    }

    /// Run the background polling loop.
    pub async fn run(&self) {
        let mut ticker = interval(Duration::from_millis(POLL_INTERVAL_MS));
        loop {
            ticker.tick().await;
            self.poll_once().await;
        }
    }

    pub async fn poll_events(
        &self,
        after_cursor: usize,
        session_key: Option<String>,
        limit: usize,
    ) -> Value {
        let queue = self.queue.lock().await;
        let events: Vec<Value> = queue
            .iter()
            .filter(|e| {
                e.cursor > after_cursor
                    && session_key.as_ref().is_none_or(|sk| &e.session_key == sk)
            })
            .take(limit)
            .map(|e| {
                let mut obj = serde_json::json!({
                    "cursor": e.cursor,
                    "type": &e.event_type,
                    "session_key": &e.session_key,
                });
                if let Value::Object(data_map) = &e.data {
                    for (k, v) in data_map {
                        obj[k.clone()] = v.clone();
                    }
                }
                obj
            })
            .collect();

        let next_cursor = events
            .last()
            .and_then(|e| e.get("cursor").and_then(Value::as_u64))
            .unwrap_or(after_cursor as u64) as usize;

        serde_json::json!({
            "events": events,
            "next_cursor": next_cursor,
        })
    }

    pub async fn wait_for_event(
        &self,
        after_cursor: usize,
        session_key: Option<String>,
        timeout_ms: u64,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            {
                let queue = self.queue.lock().await;
                for e in queue.iter() {
                    if e.cursor > after_cursor
                        && session_key.as_ref().is_none_or(|sk| &e.session_key == sk)
                    {
                        let mut obj = serde_json::json!({
                            "cursor": e.cursor,
                            "type": &e.event_type,
                            "session_key": &e.session_key,
                        });
                        if let Value::Object(data_map) = &e.data {
                            for (k, v) in data_map {
                                obj[k.clone()] = v.clone();
                            }
                        }
                        return Some(obj);
                    }
                }
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }

            let remaining = deadline - now;
            let poll_dur = Duration::from_millis(POLL_INTERVAL_MS).min(remaining);

            tokio::select! {
                _ = self.notify.notified() => {},
                _ = tokio::time::sleep(poll_dur) => {},
            }
        }
    }

    pub async fn list_pending_approvals(&self) -> Vec<Value> {
        let approvals = self.pending_approvals.lock().await;
        let mut values: Vec<Value> = approvals.values().cloned().collect();
        values.sort_by(|a, b| {
            let a_ts = a.get("created_at").and_then(Value::as_str).unwrap_or("");
            let b_ts = b.get("created_at").and_then(Value::as_str).unwrap_or("");
            a_ts.cmp(b_ts)
        });
        values
    }

    pub async fn respond_to_approval(&self, approval_id: String, decision: String) -> Value {
        let approval = {
            let mut approvals = self.pending_approvals.lock().await;
            approvals.remove(&approval_id)
        };

        if approval.is_some() {
            let session_key = approval
                .as_ref()
                .and_then(|a| a.get("session_key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            self.enqueue(QueueEvent {
                cursor: 0,
                event_type: "approval_resolved".to_string(),
                session_key,
                data: serde_json::json!({
                    "approval_id": approval_id,
                    "decision": decision,
                }),
            })
            .await;

            serde_json::json!({
                "resolved": true,
                "approval_id": approval_id,
                "decision": decision,
            })
        } else {
            serde_json::json!({
                "error": format!("Approval not found: {approval_id}"),
            })
        }
    }

    async fn enqueue(&self, mut event: QueueEvent) {
        let mut cursor = self.cursor.lock().await;
        *cursor += 1;
        event.cursor = *cursor;

        let mut queue = self.queue.lock().await;
        queue.push(event);
        while queue.len() > QUEUE_LIMIT {
            queue.remove(0);
        }
        drop(queue);

        self.notify.notify_waiters();
    }

    async fn poll_once(&self) {
        let sessions_file = get_sessions_dir().join("sessions.json");
        let sj_mtime = file_mtime(&sessions_file);

        let mut cached_mtime = self.sessions_json_mtime.lock().await;
        if sj_mtime != *cached_mtime {
            *cached_mtime = sj_mtime;
            let mut cached_index = self.cached_sessions_index.lock().await;
            *cached_index = load_sessions_index();
        }
        drop(cached_mtime);

        let db_file = hermes_core::get_hermes_home().join("state.db");
        let db_mtime = file_mtime(&db_file);

        let mut cached_db_mtime = self.state_db_mtime.lock().await;
        if db_mtime == *cached_db_mtime && sj_mtime == *cached_db_mtime {
            return;
        }
        *cached_db_mtime = db_mtime;
        drop(cached_db_mtime);

        let entries = {
            let cached = self.cached_sessions_index.lock().await;
            cached.clone()
        };

        let entries_map = match entries.as_object() {
            Some(m) => m.clone(),
            None => return,
        };

        let db_guard = self.db.lock().await;
        let db = match db_guard.as_ref() {
            Some(db) => db,
            None => return,
        };

        for (session_key, entry) in entries_map {
            let session_id = entry
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if session_id.is_empty() {
                continue;
            }

            let messages = match db.get_messages(session_id) {
                Ok(msgs) => msgs,
                Err(_) => continue,
            };

            if messages.is_empty() {
                continue;
            }

            let mut last_seen = self
                .last_poll_timestamps
                .lock()
                .await
                .get(&session_key)
                .copied()
                .unwrap_or(0.0);

            let mut new_messages = Vec::new();
            for msg in &messages {
                let role = &msg.role;
                if role != "user" && role != "assistant" {
                    continue;
                }
                let ts = msg.timestamp;
                if ts > last_seen {
                    new_messages.push(msg);
                }
            }

            for msg in new_messages {
                let content = extract_message_content(msg);
                if content.is_empty() {
                    continue;
                }
                self.enqueue(QueueEvent {
                    cursor: 0,
                    event_type: "message".to_string(),
                    session_key: session_key.clone(),
                    data: serde_json::json!({
                        "role": &msg.role,
                        "content": &content[..content.len().min(500)],
                        "timestamp": msg.timestamp.to_string(),
                        "message_id": msg.id.to_string(),
                    }),
                })
                .await;
            }

            let all_ts: Vec<f64> = messages.iter().map(|m| m.timestamp).collect();
            if let Some(&latest) = all_ts.iter().max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)) {
                if latest > last_seen {
                    let mut timestamps = self.last_poll_timestamps.lock().await;
                    timestamps.insert(session_key.clone(), latest);
                    last_seen = latest;
                }
            }
        }
    }
}

impl Default for EventBridge {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tool request structs
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConversationsListReq {
    #[schemars(description = "Filter by platform name (telegram, discord, slack, etc.)")]
    platform: Option<String>,
    #[serde(default = "default_limit_50")]
    #[schemars(description = "Maximum number of conversations to return")]
    limit: usize,
    #[schemars(description = "Optional text to filter conversations by name")]
    search: Option<String>,
}

fn default_limit_50() -> usize {
    50
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MessagesReadReq {
    #[schemars(description = "The session key from conversations_list")]
    session_key: String,
    #[serde(default = "default_limit_50")]
    #[schemars(description = "Maximum number of messages to return")]
    limit: usize,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AttachmentsFetchReq {
    #[schemars(description = "The session key from conversations_list")]
    session_key: String,
    #[schemars(description = "The message ID from messages_read")]
    message_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EventsPollReq {
    #[serde(default)]
    #[schemars(description = "Return events after this cursor (0 for all)")]
    after_cursor: usize,
    #[schemars(description = "Optional filter to one conversation")]
    session_key: Option<String>,
    #[serde(default = "default_limit_20")]
    #[schemars(description = "Maximum events to return")]
    limit: usize,
}

fn default_limit_20() -> usize {
    20
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EventsWaitReq {
    #[serde(default)]
    #[schemars(description = "Wait for events after this cursor")]
    after_cursor: usize,
    #[schemars(description = "Optional filter to one conversation")]
    session_key: Option<String>,
    #[serde(default = "default_timeout_30000")]
    #[schemars(description = "Maximum wait time in milliseconds")]
    timeout_ms: u64,
}

fn default_timeout_30000() -> u64 {
    30000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MessagesSendReq {
    #[schemars(description = "Platform target in 'platform:identifier' format")]
    target: String,
    #[schemars(description = "The message text to send")]
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ChannelsListReq {
    #[schemars(description = "Filter by platform name")]
    platform: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PermissionsRespondReq {
    #[schemars(description = "The approval ID from permissions_list_open")]
    id: String,
    #[schemars(description = "One of allow-once, allow-always, or deny")]
    decision: String,
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct McpServer {
    bridge: Arc<EventBridge>,
}

impl McpServer {
    pub fn new(bridge: Arc<EventBridge>) -> Self {
        Self { bridge }
    }
}

#[tool(tool_box)]
impl McpServer {
    #[tool(description = "List active messaging conversations across connected platforms.")]
    async fn conversations_list(
        &self,
        #[tool(aggr)] req: ConversationsListReq,
    ) -> String {
        let entries = load_sessions_index();
        let mut conversations = Vec::new();

        if let Some(entries_map) = entries.as_object() {
            for (key, entry) in entries_map {
                let origin = entry.get("origin").cloned().unwrap_or(Value::Null);
                let entry_platform = entry
                    .get("platform")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| origin.get("platform").and_then(Value::as_str).map(String::from))
                    .unwrap_or_default();

                if let Some(ref platform) = req.platform {
                    if entry_platform.to_lowercase() != platform.to_lowercase() {
                        continue;
                    }
                }

                let display_name = entry
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let chat_name = origin
                    .get("chat_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                if let Some(ref search) = req.search {
                    let search_lower = search.to_lowercase();
                    if !display_name.to_lowercase().contains(&search_lower)
                        && !chat_name.to_lowercase().contains(&search_lower)
                        && !key.to_lowercase().contains(&search_lower)
                    {
                        continue;
                    }
                }

                conversations.push(serde_json::json!({
                    "session_key": key,
                    "session_id": entry.get("session_id").and_then(Value::as_str).unwrap_or(""),
                    "platform": entry_platform,
                    "chat_type": entry.get("chat_type").and_then(Value::as_str).unwrap_or(
                        origin.get("chat_type").and_then(Value::as_str).unwrap_or("")
                    ),
                    "display_name": display_name,
                    "chat_name": chat_name,
                    "user_name": origin.get("user_name").and_then(Value::as_str).unwrap_or(""),
                    "updated_at": entry.get("updated_at").and_then(Value::as_str).unwrap_or(""),
                }));
            }
        }

        conversations.sort_by(|a, b| {
            let a_ts = a.get("updated_at").and_then(Value::as_str).unwrap_or("");
            let b_ts = b.get("updated_at").and_then(Value::as_str).unwrap_or("");
            b_ts.cmp(a_ts)
        });
        conversations.truncate(req.limit);

        serde_json::json!({
            "count": conversations.len(),
            "conversations": conversations,
        })
        .to_string()
    }

    #[tool(description = "Get detailed info about one conversation by its session key.")]
    async fn conversation_get(
        &self,
        #[tool(param)]
        #[schemars(description = "The session key from conversations_list")]
        session_key: String,
    ) -> String {
        let entries = load_sessions_index();
        let entry = match entries.get(&session_key) {
            Some(e) => e.clone(),
            None => {
                return serde_json::json!({
                    "error": format!("Conversation not found: {session_key}")
                })
                .to_string();
            }
        };

        let origin = entry.get("origin").cloned().unwrap_or(Value::Null);
        serde_json::json!({
            "session_key": session_key,
            "session_id": entry.get("session_id").and_then(Value::as_str).unwrap_or(""),
            "platform": entry.get("platform").and_then(Value::as_str).unwrap_or(
                origin.get("platform").and_then(Value::as_str).unwrap_or("")
            ),
            "chat_type": entry.get("chat_type").and_then(Value::as_str).unwrap_or(
                origin.get("chat_type").and_then(Value::as_str).unwrap_or("")
            ),
            "display_name": entry.get("display_name").and_then(Value::as_str).unwrap_or(""),
            "user_name": origin.get("user_name").and_then(Value::as_str).unwrap_or(""),
            "chat_name": origin.get("chat_name").and_then(Value::as_str).unwrap_or(""),
            "chat_id": origin.get("chat_id").and_then(Value::as_str).unwrap_or(""),
            "thread_id": origin.get("thread_id"),
            "updated_at": entry.get("updated_at").and_then(Value::as_str).unwrap_or(""),
            "created_at": entry.get("created_at").and_then(Value::as_str).unwrap_or(""),
            "input_tokens": entry.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": entry.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            "total_tokens": entry.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
        })
        .to_string()
    }

    #[tool(description = "Read recent messages from a conversation.")]
    async fn messages_read(
        &self,
        #[tool(aggr)] req: MessagesReadReq,
    ) -> String {
        let entries = load_sessions_index();
        let entry = match entries.get(&req.session_key) {
            Some(e) => e.clone(),
            None => {
                return serde_json::json!({
                    "error": format!("Conversation not found: {}", req.session_key)
                })
                .to_string();
            }
        };

        let session_id = entry
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if session_id.is_empty() {
            return serde_json::json!({"error": "No session ID for this conversation"}).to_string();
        }

        let db = match hermes_state::SessionDB::open_default() {
            Ok(db) => db,
            Err(_) => {
                return serde_json::json!({"error": "Session database unavailable"}).to_string();
            }
        };

        let all_messages = match db.get_messages(session_id) {
            Ok(msgs) => msgs,
            Err(e) => {
                return serde_json::json!({
                    "error": format!("Failed to read messages: {e}")
                })
                .to_string();
            }
        };

        let mut filtered = Vec::new();
        for msg in &all_messages {
            let role = &msg.role;
            if role != "user" && role != "assistant" {
                continue;
            }
            let content = extract_message_content(msg);
            if content.is_empty() {
                continue;
            }
            filtered.push(serde_json::json!({
                "id": msg.id.to_string(),
                "role": role,
                "content": &content[..content.len().min(2000)],
                "timestamp": msg.timestamp,
            }));
        }

        let start = filtered.len().saturating_sub(req.limit);
        let messages: Vec<Value> = filtered.into_iter().skip(start).collect();

        serde_json::json!({
            "session_key": req.session_key,
            "count": messages.len(),
            "total_in_session": all_messages.len(),
            "messages": messages,
        })
        .to_string()
    }

    #[tool(description = "List non-text attachments for a message in a conversation.")]
    async fn attachments_fetch(
        &self,
        #[tool(aggr)] req: AttachmentsFetchReq,
    ) -> String {
        let entries = load_sessions_index();
        let entry = match entries.get(&req.session_key) {
            Some(e) => e.clone(),
            None => {
                return serde_json::json!({
                    "error": format!("Conversation not found: {}", req.session_key)
                })
                .to_string();
            }
        };

        let session_id = entry
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if session_id.is_empty() {
            return serde_json::json!({"error": "No session ID for this conversation"}).to_string();
        }

        let db = match hermes_state::SessionDB::open_default() {
            Ok(db) => db,
            Err(_) => {
                return serde_json::json!({"error": "Session database unavailable"}).to_string();
            }
        };

        let all_messages = match db.get_messages(session_id) {
            Ok(msgs) => msgs,
            Err(e) => {
                return serde_json::json!({
                    "error": format!("Failed to read messages: {e}")
                })
                .to_string();
            }
        };

        let target_msg = all_messages
            .iter()
            .find(|m| m.id.to_string() == req.message_id);

        let target_msg = match target_msg {
            Some(m) => m,
            None => {
                return serde_json::json!({
                    "error": format!("Message not found: {}", req.message_id)
                })
                .to_string();
            }
        };

        let attachments = extract_attachments(target_msg);

        serde_json::json!({
            "message_id": req.message_id,
            "count": attachments.len(),
            "attachments": attachments,
        })
        .to_string()
    }

    #[tool(description = "Poll for new conversation events since a cursor position.")]
    async fn events_poll(
        &self,
        #[tool(aggr)] req: EventsPollReq,
    ) -> String {
        let result = self
            .bridge
            .poll_events(req.after_cursor, req.session_key, req.limit)
            .await;
        result.to_string()
    }

    #[tool(description = "Wait for the next conversation event (long-poll).")]
    async fn events_wait(
        &self,
        #[tool(aggr)] req: EventsWaitReq,
    ) -> String {
        let timeout_ms = req.timeout_ms.min(300000); // Cap at 5 minutes
        let event = self
            .bridge
            .wait_for_event(req.after_cursor, req.session_key, timeout_ms)
            .await;

        if let Some(evt) = event {
            serde_json::json!({"event": evt}).to_string()
        } else {
            serde_json::json!({"event": Value::Null, "reason": "timeout"}).to_string()
        }
    }

    #[tool(description = "Send a message to a platform conversation.")]
    async fn messages_send(
        &self,
        #[tool(aggr)] req: MessagesSendReq,
    ) -> String {
        if req.target.is_empty() || req.message.is_empty() {
            return serde_json::json!({
                "error": "Both target and message are required"
            })
            .to_string();
        }

        let args = serde_json::json!({
            "action": "send",
            "target": req.target,
            "message": req.message,
        });

        match crate::send_message::handle_send_message(args) {
            Ok(result) => result,
            Err(e) => {
                serde_json::json!({
                    "error": format!("Send failed: {e}")
                })
                .to_string()
            }
        }
    }

    #[tool(description = "List available messaging channels and targets across platforms.")]
    async fn channels_list(
        &self,
        #[tool(aggr)] req: ChannelsListReq,
    ) -> String {
        let directory = load_channel_directory();
        let mut channels = Vec::new();

        if let Some(dir_map) = directory.as_object() {
            if !dir_map.is_empty() {
                for (plat, entries_val) in dir_map {
                    if let Some(ref platform) = req.platform {
                        if plat.to_lowercase() != platform.to_lowercase() {
                            continue;
                        }
                    }
                    if let Some(entries_list) = entries_val.as_array() {
                        for ch in entries_list {
                            if let Some(ch_obj) = ch.as_object() {
                                let chat_id = ch_obj
                                    .get("id")
                                    .or_else(|| ch_obj.get("chat_id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                channels.push(serde_json::json!({
                                    "target": if chat_id.is_empty() { plat.clone() } else { format!("{plat}:{chat_id}") },
                                    "platform": plat,
                                    "name": ch_obj.get("name").or_else(|| ch_obj.get("display_name")).and_then(Value::as_str).unwrap_or(""),
                                    "chat_type": ch_obj.get("type").and_then(Value::as_str).unwrap_or(""),
                                }));
                            }
                        }
                    }
                }
                return serde_json::json!({
                    "count": channels.len(),
                    "channels": channels,
                })
                .to_string();
            }
        }

        // Fallback to sessions index
        let entries = load_sessions_index();
        let mut seen = std::collections::HashSet::new();
        if let Some(entries_map) = entries.as_object() {
            for (_key, entry) in entries_map {
                let origin = entry.get("origin").cloned().unwrap_or(Value::Null);
                let p = entry
                    .get("platform")
                    .and_then(Value::as_str)
                    .unwrap_or(
                        origin.get("platform").and_then(Value::as_str).unwrap_or("")
                    );
                let chat_id = origin
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if p.is_empty() || chat_id.is_empty() {
                    continue;
                }
                if let Some(ref platform) = req.platform {
                    if p.to_lowercase() != platform.to_lowercase() {
                        continue;
                    }
                }
                let target_str = format!("{p}:{chat_id}");
                if seen.contains(&target_str) {
                    continue;
                }
                seen.insert(target_str.clone());
                channels.push(serde_json::json!({
                    "target": target_str,
                    "platform": p,
                    "name": entry.get("display_name").and_then(Value::as_str).unwrap_or(
                        origin.get("chat_name").and_then(Value::as_str).unwrap_or("")
                    ),
                    "chat_type": entry.get("chat_type").and_then(Value::as_str).unwrap_or(
                        origin.get("chat_type").and_then(Value::as_str).unwrap_or("")
                    ),
                }));
            }
        }

        serde_json::json!({
            "count": channels.len(),
            "channels": channels,
        })
        .to_string()
    }

    #[tool(description = "List pending approval requests observed during this bridge session.")]
    async fn permissions_list_open(&self) -> String {
        let approvals = self.bridge.list_pending_approvals().await;
        serde_json::json!({
            "count": approvals.len(),
            "approvals": approvals,
        })
        .to_string()
    }

    #[tool(description = "Respond to a pending approval request.")]
    async fn permissions_respond(
        &self,
        #[tool(aggr)] req: PermissionsRespondReq,
    ) -> String {
        if !matches!(req.decision.as_str(), "allow-once" | "allow-always" | "deny") {
            return serde_json::json!({
                "error": format!("Invalid decision: {}. Must be allow-once, allow-always, or deny", req.decision)
            })
            .to_string();
        }

        let result = self
            .bridge
            .respond_to_approval(req.id, req.decision)
            .await;
        result.to_string()
    }
}

#[tool(tool_box)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Hermes Agent messaging bridge. Use these tools to interact with \
                 conversations across Telegram, Discord, Slack, WhatsApp, Signal, \
                 Matrix, and other connected platforms."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Start the Hermes MCP server on stdio.
pub async fn run_mcp_server(verbose: bool) -> Result<(), std::io::Error> {
    if verbose {
        tracing::info!("MCP server starting in verbose mode");
    }

    let bridge = Arc::new(EventBridge::new());
    let bridge_clone = bridge.clone();

    // Spawn background polling task
    tokio::spawn(async move {
        bridge_clone.run().await;
    });

    let server = McpServer::new(bridge);

    let service = rmcp::serve_server(server, rmcp::transport::stdio()).await?;

    // Wait for service to complete (stdio closes)
    service.waiting().await.map_err(std::io::Error::other)?;

    Ok(())
}
