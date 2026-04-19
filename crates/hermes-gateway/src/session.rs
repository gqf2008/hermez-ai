#![allow(dead_code)]
//! Gateway session management.
//!
//! Mirrors Python `gateway/session.py` and `gateway/session_context.py`:
//! - SessionSource: where messages come from
//! - SessionContext: full context for dynamic system prompt injection
//! - SessionEntry: session storage with metadata
//! - SessionStore: session storage and retrieval (JSON + SQLite)
//! - PII redaction helpers
//! - Session key construction
//! - Reset policy evaluation

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Local, Timelike};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{GatewayConfig, HomeChannel, Platform};

// ── PII redaction helpers ──────────────────────────────────────────────────

/// Deterministic 12-char hex hash of an identifier.
fn hash_id(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

/// Hash a sender ID to `user_<12hex>`.
pub fn hash_sender_id(value: &str) -> String {
    format!("user_{}", hash_id(value))
}

/// Hash the numeric portion of a chat ID, preserving platform prefix.
///
/// `telegram:12345` → `telegram:<hash>`
/// `12345`          → `<hash>`
pub fn hash_chat_id(value: &str) -> String {
    if let Some(colon) = value.find(':') {
        let prefix = &value[..colon];
        format!("{}:{}", prefix, hash_id(&value[colon + 1..]))
    } else {
        hash_id(value)
    }
}

// ── SessionSource ──────────────────────────────────────────────────────────

/// Describes where a message originated from.
///
/// This information is used to:
/// 1. Route responses back to the right place
/// 2. Inject context into the system prompt
/// 3. Track origin for cron job delivery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: Platform,
    pub chat_id: String,
    pub chat_name: Option<String>,
    pub chat_type: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub thread_id: Option<String>,
    pub chat_topic: Option<String>,
    pub user_id_alt: Option<String>,
    pub chat_id_alt: Option<String>,
}

impl SessionSource {
    /// Human-readable description of the source.
    pub fn description(&self) -> String {
        match self.chat_type.as_str() {
            "dm" => format!("DM with {}", self.user_name.as_deref().or(self.user_id.as_deref()).unwrap_or("user")),
            "group" => format!("group: {}", self.chat_name.as_deref().unwrap_or(&self.chat_id)),
            "channel" => format!("channel: {}", self.chat_name.as_deref().unwrap_or(&self.chat_id)),
            _ => self.chat_name.as_deref().unwrap_or(&self.chat_id).to_string(),
        }
    }

    /// Build a PII-safe description (hashes IDs).
    pub fn safe_description(&self) -> String {
        let hashed_user = hash_sender_id(self.user_id.as_deref().unwrap_or("user"));
        let uname = self.user_name.as_deref().unwrap_or(&hashed_user);
        let hashed_chat = hash_chat_id(&self.chat_id);
        let cname = self.chat_name.as_deref().unwrap_or(&hashed_chat);
        match self.chat_type.as_str() {
            "dm" => format!("DM with {uname}"),
            "group" => format!("group: {cname}"),
            "channel" => format!("channel: {cname}"),
            _ => cname.to_string(),
        }
    }
}

// ── SessionContext ─────────────────────────────────────────────────────────

/// Full context for a session, used for dynamic system prompt injection.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub source: SessionSource,
    pub connected_platforms: Vec<Platform>,
    pub home_channels: HashMap<Platform, HomeChannel>,
    pub session_key: String,
    pub session_id: String,
    pub created_at: Option<DateTime<Local>>,
    pub updated_at: Option<DateTime<Local>>,
}

// ── SessionContext prompt builder ──────────────────────────────────────────

/// Platforms where user IDs can be safely redacted.
///
/// Discord is excluded because mentions use `<@user_id>` and the LLM needs
/// the real ID to tag users.
const PII_SAFE_PLATFORMS: &[Platform] = &[
    Platform::Whatsapp,
    Platform::Signal,
    Platform::Telegram,
    Platform::Bluebubbles,
];

/// Build the dynamic system prompt section that tells the agent about its context.
pub fn build_session_context_prompt(context: &SessionContext, redact_pii: bool) -> String {
    let should_redact = redact_pii && PII_SAFE_PLATFORMS.contains(&context.source.platform);
    let mut lines = vec![
        "## Current Session Context".to_string(),
        String::new(),
    ];

    // Source info
    let platform_name = capitalize(context.source.platform.as_str());
    if context.source.platform == Platform::Local {
        lines.push(format!("**Source:** {platform_name} (the machine running this agent)"));
    } else {
        let desc = if should_redact {
            context.source.safe_description()
        } else {
            context.source.description()
        };
        lines.push(format!("**Source:** {platform_name} ({desc})"));
    }

    // Channel topic
    if let Some(ref topic) = context.source.chat_topic {
        lines.push(format!("**Channel Topic:** {topic}"));
    }

    // Session type
    let is_shared_thread = context.source.chat_type != "dm"
        && context.source.thread_id.is_some();
    if is_shared_thread {
        lines.push(
            "**Session type:** Multi-user thread — messages are prefixed \
             with [sender name]. Multiple users may participate."
                .to_string(),
        );
    } else if let Some(ref user_name) = context.source.user_name {
        lines.push(format!("**User:** {user_name}"));
    } else if let Some(ref user_id) = context.source.user_id {
        let uid = if should_redact {
            hash_sender_id(user_id)
        } else {
            user_id.clone()
        };
        lines.push(format!("**User ID:** {uid}"));
    }

    // Platform-specific behavioral notes
    match context.source.platform {
        Platform::Slack => {
            lines.push(String::new());
            lines.push(
                "**Platform notes:** You are running inside Slack. \
                 You do NOT have access to Slack-specific APIs — you cannot search \
                 channel history, pin/unpin messages, manage channels, or list users. \
                 Do not promise to perform these actions. If the user asks, explain \
                 that you can only read messages sent directly to you and respond."
                    .to_string(),
            );
        }
        Platform::Discord => {
            lines.push(String::new());
            lines.push(
                "**Platform notes:** You are running inside Discord. \
                 You do NOT have access to Discord-specific APIs — you cannot search \
                 channel history, pin messages, manage roles, or list server members. \
                 Do not promise to perform these actions. If the user asks, explain \
                 that you can only read messages sent directly to you and respond."
                    .to_string(),
            );
        }
        _ => {}
    }

    // Connected platforms
    let mut platforms_list = vec!["local (files on this machine)".to_string()];
    for p in &context.connected_platforms {
        if *p != Platform::Local {
            platforms_list.push(format!("{}: Connected \u{2713}", p.as_str()));
        }
    }
    lines.push(format!("**Connected Platforms:** {}", platforms_list.join(", ")));

    // Home channels
    if !context.home_channels.is_empty() {
        lines.push(String::new());
        lines.push("**Home Channels (default destinations):**".to_string());
        for (platform, home) in &context.home_channels {
            let hc_id = if should_redact {
                hash_chat_id(&home.chat_id)
            } else {
                home.chat_id.clone()
            };
            lines.push(format!("  - {}: {} (ID: {hc_id})", platform.as_str(), home.name));
        }
    }

    // Delivery options for scheduled tasks
    lines.push(String::new());
    lines.push("**Delivery options for scheduled tasks:**".to_string());

    if context.source.platform == Platform::Local {
        lines.push("- `\"origin\"` → Local output (saved to files)".to_string());
    } else {
        let origin_label = if let Some(ref name) = context.source.chat_name {
            name.clone()
        } else if should_redact {
            hash_chat_id(&context.source.chat_id)
        } else {
            context.source.chat_id.clone()
        };
        lines.push(format!("- `\"origin\"` → Back to this chat ({origin_label})"));
    }

    lines.push("- `\"local\"` → Save to local files only (~/.hermes/cron/output/)".to_string());

    for (platform, home) in &context.home_channels {
        lines.push(format!(
            "- `\"{}\"` → Home channel ({})",
            platform.as_str(),
            home.name
        ));
    }

    lines.push(String::new());
    lines.push(
        "*For explicit targeting, use `\"platform:chat_id\"` format if the user provides a specific chat ID.*"
            .to_string(),
    );

    lines.join("\n")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── SessionEntry ───────────────────────────────────────────────────────────

/// Entry in the session store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_key: String,
    pub session_id: String,
    pub created_at: DateTime<Local>,
    pub updated_at: DateTime<Local>,
    pub origin: Option<SessionSource>,
    pub display_name: Option<String>,
    pub platform: Option<Platform>,
    pub chat_type: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub cost_status: String,
    pub last_prompt_tokens: u64,
    pub was_auto_reset: bool,
    pub auto_reset_reason: Option<String>,
    pub reset_had_activity: bool,
    pub memory_flushed: bool,
    pub suspended: bool,
}

impl SessionEntry {
    pub fn new(
        session_key: impl Into<String>,
        session_id: impl Into<String>,
        origin: Option<SessionSource>,
    ) -> Self {
        let now = Local::now();
        let origin_ref = origin.clone();
        Self {
            session_key: session_key.into(),
            session_id: session_id.into(),
            created_at: now,
            updated_at: now,
            origin,
            display_name: origin_ref.as_ref().and_then(|o| o.chat_name.clone()),
            platform: origin_ref.as_ref().map(|o| o.platform),
            chat_type: origin_ref
                .as_ref()
                .map(|o| o.chat_type.clone())
                .unwrap_or_else(|| "dm".to_string()),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            cost_status: "unknown".to_string(),
            last_prompt_tokens: 0,
            was_auto_reset: false,
            auto_reset_reason: None,
            reset_had_activity: false,
            memory_flushed: false,
            suspended: false,
        }
    }
}

// ── Session key construction ───────────────────────────────────────────────

/// Build a deterministic session key from a message source.
///
/// This is the single source of truth for session key construction.
pub fn build_session_key(
    source: &SessionSource,
    group_sessions_per_user: bool,
    thread_sessions_per_user: bool,
) -> String {
    let platform = source.platform.as_str();

    if source.chat_type == "dm" {
        if !source.chat_id.is_empty() {
            if let Some(ref thread_id) = source.thread_id {
                return format!("agent:main:{platform}:dm:{}:{thread_id}", source.chat_id);
            }
            return format!("agent:main:{platform}:dm:{}", source.chat_id);
        }
        if let Some(ref thread_id) = source.thread_id {
            return format!("agent:main:{platform}:dm:{thread_id}");
        }
        return format!("agent:main:{platform}:dm");
    }

    let participant_id = source
        .user_id_alt
        .as_ref()
        .or(source.user_id.as_ref());
    let mut key_parts = vec!["agent:main", platform, &source.chat_type];

    if !source.chat_id.is_empty() {
        key_parts.push(&source.chat_id);
    }
    if let Some(ref thread_id) = source.thread_id {
        key_parts.push(thread_id);
    }

    // In threads, default to shared sessions. Per-user isolation only
    // applies when explicitly enabled via thread_sessions_per_user,
    // or when there is no thread (regular group).
    let isolate_user = if source.thread_id.is_some() && !thread_sessions_per_user {
        false
    } else {
        group_sessions_per_user
    };

    if isolate_user {
        if let Some(pid) = participant_id {
            key_parts.push(pid);
        }
    }

    key_parts.join(":")
}

// ── SessionStore ───────────────────────────────────────────────────────────

/// Manages session storage and retrieval.
///
/// Uses SQLite (via SessionDB) for session metadata and message transcripts.
/// Falls back to legacy JSONL files if SQLite is unavailable.
pub struct SessionStore {
    sessions_dir: PathBuf,
    config: GatewayConfig,
    entries: parking_lot::Mutex<HashMap<String, SessionEntry>>,
    loaded: parking_lot::Mutex<bool>,
    db: parking_lot::Mutex<Option<hermes_state::SessionDB>>,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf, config: GatewayConfig) -> Self {
        let db = hermes_state::SessionDB::open_default().ok();
        Self {
            sessions_dir,
            config,
            entries: parking_lot::Mutex::new(HashMap::new()),
            loaded: parking_lot::Mutex::new(false),
            db: parking_lot::Mutex::new(db),
        }
    }

    /// Load sessions index from disk if not already loaded.
    fn ensure_loaded(&self) {
        let mut loaded = self.loaded.lock();
        if *loaded {
            return;
        }
        *loaded = true;

        std::fs::create_dir_all(&self.sessions_dir).ok();
        let sessions_file = self.sessions_dir.join("sessions.json");

        if sessions_file.exists() {
            if let Ok(data) = std::fs::read_to_string(&sessions_file) {
                if let Ok(parsed) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&data) {
                    let mut entries = self.entries.lock();
                    for (key, entry_data) in parsed {
                        if let Ok(entry) = serde_json::from_value::<SessionEntry>(entry_data) {
                            entries.insert(key, entry);
                        }
                    }
                }
            }
        }
    }

    /// Save sessions index to disk atomically.
    fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.sessions_dir).map_err(|e| e.to_string())?;
        let sessions_file = self.sessions_dir.join("sessions.json");

        let entries = self.entries.lock();
        let data: HashMap<&str, &SessionEntry> = entries.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let json = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;

        // Atomic write via temp file + rename
        let tmp_path = self.sessions_dir.join(format!(".sessions_{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&tmp_path, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp_path, &sessions_file).map_err(|e| {
            std::fs::remove_file(&tmp_path).ok();
            e.to_string()
        })
    }

    /// Generate a session key from a source using config settings.
    fn generate_session_key(&self, source: &SessionSource) -> String {
        build_session_key(
            source,
            self.config.group_sessions_per_user,
            self.config.thread_sessions_per_user,
        )
    }

    /// Check if a session should be reset based on policy.
    ///
    /// Returns the reset reason ("idle" or "daily") if needed, or None.
    fn should_reset(&self, entry: &SessionEntry, source: &SessionSource) -> Option<String> {
        let policy = self.config.get_reset_policy(Some(source.platform), &source.chat_type);

        if policy.mode == "none" {
            return None;
        }

        let now = Local::now();

        if policy.mode == "idle" || policy.mode == "both" {
            let idle_deadline = entry.updated_at
                + chrono::Duration::try_minutes(policy.idle_minutes as i64).unwrap_or_default();
            if now > idle_deadline {
                return Some("idle".to_string());
            }
        }

        if policy.mode == "daily" || policy.mode == "both" {
            let today_reset = today_cutoff(policy.at_hour);
            if entry.updated_at < today_reset {
                return Some("daily".to_string());
            }
        }

        None
    }

    /// Get an existing session or create a new one.
    pub fn get_or_create_session(&self, source: SessionSource, force_new: bool) -> SessionEntry {
        self.ensure_loaded();

        let session_key = self.generate_session_key(&source);
        let now = Local::now();

        let mut entry_to_return: Option<SessionEntry> = None;

        {
            let mut entries = self.entries.lock();

            if let Some(entry) = entries.get(&session_key) {
                if !force_new {
                    let reset_reason = if entry.suspended {
                        Some("suspended".to_string())
                    } else {
                        self.should_reset(entry, &source)
                    };

                    if reset_reason.is_none() {
                        // Session is still valid — update timestamp
                        let mut e = entry.clone();
                        e.updated_at = now;
                        entries.insert(session_key.clone(), e.clone());
                        entry_to_return = Some(e);
                    } else {
                        // Session is being auto-reset — fall through to create
                        let had_activity = entry.total_tokens > 0;
                        let new_entry = SessionEntry::new(
                            &session_key,
                            generate_session_id(),
                            Some(source.clone()),
                        );
                        let mut e = new_entry;
                        e.was_auto_reset = true;
                        e.auto_reset_reason = reset_reason;
                        e.reset_had_activity = had_activity;
                        entries.insert(session_key.clone(), e.clone());
                        entry_to_return = Some(e);
                    }
                }
            }

            if entry_to_return.is_none() {
                let new_entry = SessionEntry::new(
                    &session_key,
                    generate_session_id(),
                    Some(source),
                );
                entries.insert(session_key.clone(), new_entry.clone());
                entry_to_return = Some(new_entry);
            }
        }

        // Save outside the lock
        self.save().ok();

        entry_to_return.expect("session entry should have been created above")
    }

    /// Update lightweight session metadata after an interaction.
    pub fn update_session(&self, session_key: &str, last_prompt_tokens: Option<u64>) {
        self.ensure_loaded();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get_mut(session_key) {
            entry.updated_at = Local::now();
            if let Some(tokens) = last_prompt_tokens {
                entry.last_prompt_tokens = tokens;
            }
        }
        drop(entries);
        self.save().ok();
    }

    /// Mark a session as suspended so it auto-resets on next access.
    pub fn suspend_session(&self, session_key: &str) -> bool {
        self.ensure_loaded();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get_mut(session_key) {
            entry.suspended = true;
            drop(entries);
            self.save().ok();
            return true;
        }
        false
    }

    /// Mark recently-active sessions as suspended.
    pub fn suspend_recently_active(&self, max_age_seconds: i64) -> usize {
        self.ensure_loaded();
        let cutoff = Local::now() - chrono::Duration::try_seconds(max_age_seconds).unwrap_or_default();
        let mut entries = self.entries.lock();
        let mut count = 0;
        for entry in entries.values_mut() {
            if !entry.suspended && entry.updated_at >= cutoff {
                entry.suspended = true;
                count += 1;
            }
        }
        if count > 0 {
            drop(entries);
            self.save().ok();
        }
        count
    }

    /// Force reset a session, creating a new session ID.
    pub fn reset_session(&self, session_key: &str) -> Option<SessionEntry> {
        self.ensure_loaded();
        let mut entries = self.entries.lock();
        let old_entry = entries.get(session_key)?.clone();

        let new_entry = SessionEntry {
            session_key: session_key.to_string(),
            session_id: generate_session_id(),
            created_at: Local::now(),
            updated_at: Local::now(),
            origin: old_entry.origin.clone(),
            display_name: old_entry.display_name.clone(),
            platform: old_entry.platform,
            chat_type: old_entry.chat_type.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            cost_status: "unknown".to_string(),
            last_prompt_tokens: 0,
            was_auto_reset: false,
            auto_reset_reason: None,
            reset_had_activity: false,
            memory_flushed: old_entry.memory_flushed,
            suspended: false,
        };

        entries.insert(session_key.to_string(), new_entry.clone());
        drop(entries);
        self.save().ok();

        Some(new_entry)
    }

    /// Switch a session key to point at an existing session ID.
    pub fn switch_session(&self, session_key: &str, target_session_id: &str) -> Option<SessionEntry> {
        self.ensure_loaded();
        let mut entries = self.entries.lock();
        let old_entry = entries.get(session_key)?;

        if old_entry.session_id == target_session_id {
            return Some(old_entry.clone());
        }

        let new_entry = SessionEntry {
            session_key: session_key.to_string(),
            session_id: target_session_id.to_string(),
            created_at: Local::now(),
            updated_at: Local::now(),
            origin: old_entry.origin.clone(),
            display_name: old_entry.display_name.clone(),
            platform: old_entry.platform,
            chat_type: old_entry.chat_type.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            cost_status: "unknown".to_string(),
            last_prompt_tokens: 0,
            was_auto_reset: false,
            auto_reset_reason: None,
            reset_had_activity: false,
            memory_flushed: old_entry.memory_flushed,
            suspended: false,
        };

        entries.insert(session_key.to_string(), new_entry.clone());
        drop(entries);
        self.save().ok();

        Some(new_entry)
    }

    /// List all sessions, optionally filtered by activity.
    pub fn list_sessions(&self, active_minutes: Option<u64>) -> Vec<SessionEntry> {
        self.ensure_loaded();
        let entries = self.entries.lock();
        let cutoff = active_minutes.map(|mins| {
            Local::now() - chrono::Duration::try_minutes(mins as i64).unwrap_or_default()
        });

        let mut result: Vec<SessionEntry> = entries
            .values()
            .filter(|e| {
                if let Some(c) = cutoff {
                    e.updated_at >= c
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        result
    }

    /// Check if any sessions have ever been created.
    pub fn has_any_sessions(&self) -> bool {
        self.ensure_loaded();

        // Check in-memory entries first
        let entries = self.entries.lock();
        if !entries.is_empty() {
            return true;
        }
        drop(entries);

        // Also check SQLite (for sessions created by other processes)
        let db_lock = self.db.lock();
        if let Some(db) = db_lock.as_ref() {
            if let Ok(count) = db.session_count(None) {
                return count > 0;
            }
        }

        false
    }

    /// Get the path to a session's legacy transcript file.
    pub fn get_transcript_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }

    /// Append a message to a session's transcript (SQLite + legacy JSONL).
    pub fn append_to_transcript(&self, session_id: &str, message: &serde_json::Value) {
        // Write to SQLite
        let db_lock = self.db.lock();
        if let Some(db) = db_lock.as_ref() {
            let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
            let content = message.get("content").and_then(|v| v.as_str());
            db.append_message(session_id, role, content, None, None, None, None, None, None, None, None)
                .ok();
        }

        // Also write legacy JSONL
        let transcript_path = self.get_transcript_path(session_id);
        if let Ok(line) = serde_json::to_string(message) {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&transcript_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "{line}")
                })
                .ok();
        }
    }

    /// Replace the entire transcript for a session with new messages.
    pub fn rewrite_transcript(&self, session_id: &str, messages: &[serde_json::Value]) {
        // SQLite: clear and re-insert
        let db_lock = self.db.lock();
        if let Some(db) = db_lock.as_ref() {
            db.clear_messages(session_id).ok();
            for msg in messages {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
                let content = msg.get("content").and_then(|v| v.as_str());
                db.append_message(session_id, role, content, None, None, None, None, None, None, None, None)
                    .ok();
            }
        }

        // JSONL: overwrite
        let transcript_path = self.get_transcript_path(session_id);
        if let Ok(file) = std::fs::File::create(&transcript_path) {
            use std::io::Write;
            let mut writer = std::io::BufWriter::new(file);
            for msg in messages {
                if let Ok(line) = serde_json::to_string(msg) {
                    writeln!(writer, "{line}").ok();
                }
            }
        }
    }

    /// Load all messages from a session's transcript.
    pub fn load_transcript(&self, session_id: &str) -> Vec<serde_json::Value> {
        let mut db_messages = Vec::new();

        // Try SQLite first
        let db_lock = self.db.lock();
        if let Some(db) = db_lock.as_ref() {
            if let Ok(msgs) = db.get_messages_as_conversation(session_id) {
                db_messages = msgs;
            }
        }

        // Load legacy JSONL transcript
        let transcript_path = self.get_transcript_path(session_id);
        let mut jsonl_messages = Vec::new();
        if transcript_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&transcript_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        if let Ok(msg) = serde_json::from_str(line) {
                            jsonl_messages.push(msg);
                        }
                    }
                }
            }
        }

        // Prefer whichever source has more messages
        if jsonl_messages.len() > db_messages.len() {
            jsonl_messages
        } else {
            db_messages
        }
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.ensure_loaded();
        self.entries.lock().len()
    }
}

/// Generate a unique session ID.
fn generate_session_id() -> String {
    let now = Local::now();
    let uuid = uuid::Uuid::new_v4();
    let hex_suffix = uuid.as_bytes()[..8].iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("{}_{hex_suffix}", now.format("%Y%m%d_%H%M%S"))
}

/// Compute today's reset cutoff time at the given hour.
fn today_cutoff(at_hour: u32) -> DateTime<Local> {
    let now = Local::now();
    let mut cutoff = now
        .with_hour(at_hour)
        .unwrap_or(now)
        .with_minute(0)
        .unwrap_or(now)
        .with_second(0)
        .unwrap_or(now)
        .with_nanosecond(0)
        .unwrap_or(now);

    // If we're before the cutoff hour today, use yesterday's cutoff
    if now.hour() < at_hour {
        cutoff -= chrono::Duration::try_days(1).unwrap_or_default();
    }

    cutoff
}

/// Build a full session context from a source and config.
pub fn build_session_context(
    source: SessionSource,
    config: &GatewayConfig,
    session_entry: Option<&SessionEntry>,
) -> SessionContext {
    let connected = config.get_connected_platforms();

    let mut home_channels = HashMap::new();
    for platform in &connected {
        if let Some(home) = config.get_home_channel(*platform) {
            home_channels.insert(*platform, home.clone());
        }
    }

    let mut context = SessionContext {
        source,
        connected_platforms: connected,
        home_channels,
        session_key: String::new(),
        session_id: String::new(),
        created_at: None,
        updated_at: None,
    };

    if let Some(entry) = session_entry {
        context.session_key = entry.session_key.clone();
        context.session_id = entry.session_id.clone();
        context.created_at = Some(entry.created_at);
        context.updated_at = Some(entry.updated_at);
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn test_source(platform: Platform) -> SessionSource {
        SessionSource {
            platform,
            chat_id: "12345".to_string(),
            chat_name: Some("Test Chat".to_string()),
            chat_type: "dm".to_string(),
            user_id: Some("user1".to_string()),
            user_name: Some("Alice".to_string()),
            thread_id: None,
            chat_topic: None,
            user_id_alt: None,
            chat_id_alt: None,
        }
    }

    #[test]
    fn test_hash_id_deterministic() {
        let h1 = hash_id("test_value");
        let h2 = hash_id("test_value");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 12);
    }

    #[test]
    fn test_hash_sender_id_format() {
        let h = hash_sender_id("user123");
        assert!(h.starts_with("user_"));
        assert_eq!(h.len(), 17); // "user_" + 12 hex chars
    }

    #[test]
    fn test_hash_chat_id_with_prefix() {
        let h = hash_chat_id("telegram:12345");
        assert!(h.starts_with("telegram:"));
    }

    #[test]
    fn test_hash_chat_id_without_prefix() {
        let h = hash_chat_id("12345");
        assert!(!h.contains(':'));
        assert_eq!(h.len(), 12);
    }

    #[test]
    fn test_session_source_description_dm() {
        let src = test_source(Platform::Telegram);
        assert_eq!(src.description(), "DM with Alice");
    }

    #[test]
    fn test_session_source_description_group() {
        let src = SessionSource {
            chat_type: "group".to_string(),
            ..test_source(Platform::Discord)
        };
        assert_eq!(src.description(), "group: Test Chat");
    }

    #[test]
    fn test_session_source_safe_description() {
        let src = test_source(Platform::Telegram);
        let desc = src.safe_description();
        assert!(!desc.contains("user1"));
        assert!(desc.contains("DM with"));
    }

    #[test]
    fn test_build_session_key_dm() {
        let src = test_source(Platform::Telegram);
        let key = build_session_key(&src, true, false);
        assert_eq!(key, "agent:main:telegram:dm:12345");
    }

    #[test]
    fn test_build_session_key_dm_with_thread() {
        let mut src = test_source(Platform::Telegram);
        src.thread_id = Some("topic_42".to_string());
        let key = build_session_key(&src, true, false);
        assert_eq!(key, "agent:main:telegram:dm:12345:topic_42");
    }

    #[test]
    fn test_build_session_key_group_shared() {
        let mut src = test_source(Platform::Discord);
        src.chat_type = "group".to_string();
        src.chat_id = "guild123".to_string();
        src.thread_id = Some("thread1".to_string());
        // thread_sessions_per_user=false → shared thread session
        let key = build_session_key(&src, true, false);
        assert_eq!(key, "agent:main:discord:group:guild123:thread1");
    }

    #[test]
    fn test_build_session_key_group_per_user() {
        let mut src = test_source(Platform::Discord);
        src.chat_type = "group".to_string();
        src.chat_id = "guild123".to_string();
        src.thread_id = Some("thread1".to_string());
        // thread_sessions_per_user=true → per-user thread session
        let key = build_session_key(&src, true, true);
        assert_eq!(key, "agent:main:discord:group:guild123:thread1:user1");
    }

    #[test]
    fn test_build_session_key_no_chat_id_dm() {
        let mut src = test_source(Platform::Slack);
        src.chat_id = String::new();
        let key = build_session_key(&src, true, false);
        assert_eq!(key, "agent:main:slack:dm");
    }

    #[test]
    fn test_build_session_context_prompt() {
        let src = test_source(Platform::Telegram);
        let config = GatewayConfig::default();
        let context = build_session_context(src, &config, None);
        let prompt = build_session_context_prompt(&context, false);
        assert!(prompt.contains("## Current Session Context"));
        assert!(prompt.contains("Telegram"));
        assert!(prompt.contains("Alice"));
        assert!(prompt.contains("Connected Platforms"));
    }

    #[test]
    fn test_build_session_context_prompt_pii_redacted() {
        let mut src = test_source(Platform::Telegram);
        src.user_name = None; // No display name → must fall back to hashed user_id
        let config = GatewayConfig::default();
        let context = build_session_context(src, &config, None);
        let prompt = build_session_context_prompt(&context, true);
        assert!(!prompt.contains("user1"));
        assert!(!prompt.contains("12345"));
        assert!(prompt.contains("user_"));
    }

    #[test]
    fn test_build_session_context_prompt_platform_notes() {
        let mut src = test_source(Platform::Slack);
        src.chat_id = "C123".to_string();
        let config = GatewayConfig::default();
        let context = build_session_context(src, &config, None);
        let prompt = build_session_context_prompt(&context, false);
        assert!(prompt.contains("Slack-specific"));
        assert!(prompt.contains("channel history"));
    }

    #[test]
    fn test_generate_session_id_format() {
        let id = generate_session_id();
        // Should be like "20260412_143022_0123456789abcdef"
        assert!(id.contains('_'));
        assert!(id.len() > 20);
    }

    #[test]
    fn test_today_cutoff_before_hour() {
        // At 3am, cutoff should be yesterday at 4am
        let now = Local::now();
        if now.hour() < 4 {
            let cutoff = today_cutoff(4);
            // cutoff should be yesterday
            assert!(cutoff < now);
        }
    }

    #[test]
    fn test_today_cutoff_after_hour() {
        // At 5pm, cutoff should be today at 4am
        let now = Local::now();
        if now.hour() >= 17 {
            let cutoff = today_cutoff(4);
            // cutoff should be earlier today
            assert!(cutoff < now);
            assert!(cutoff.day() == now.day());
        }
    }

    #[test]
    fn test_session_entry_new() {
        let src = test_source(Platform::Telegram);
        let entry = SessionEntry::new("test:key", "sess1", Some(src.clone()));
        assert_eq!(entry.session_key, "test:key");
        assert_eq!(entry.session_id, "sess1");
        assert_eq!(entry.platform, Some(Platform::Telegram));
        assert!(!entry.was_auto_reset);
    }

    #[test]
    fn test_session_store_create_session() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);
        let src = test_source(Platform::Telegram);

        let entry = store.get_or_create_session(src, false);
        assert_eq!(entry.session_key, "agent:main:telegram:dm:12345");
        assert!(entry.was_auto_reset == false);

        // Same source should return same session
        let entry2 = store.get_or_create_session(test_source(Platform::Telegram), false);
        assert_eq!(entry2.session_id, entry.session_id);
    }

    #[test]
    fn test_session_store_force_new() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);
        let src = test_source(Platform::Telegram);

        let entry1 = store.get_or_create_session(src.clone(), false);
        let entry2 = store.get_or_create_session(src, true);
        assert_ne!(entry2.session_id, entry1.session_id);
    }

    #[test]
    fn test_session_store_suspend() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);
        let src = test_source(Platform::Telegram);

        store.get_or_create_session(src.clone(), false);
        let key = "agent:main:telegram:dm:12345";

        assert!(store.suspend_session(key));
        assert!(!store.suspend_session("nonexistent"));

        // After suspend, next get_or_create should auto-reset
        let new_entry = store.get_or_create_session(src, false);
        assert!(new_entry.was_auto_reset);
        assert_eq!(new_entry.auto_reset_reason, Some("suspended".to_string()));
    }

    #[test]
    fn test_session_store_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);
        let src = test_source(Platform::Telegram);

        let original = store.get_or_create_session(src, false);
        let key = "agent:main:telegram:dm:12345";

        let new_entry = store.reset_session(key);
        assert!(new_entry.is_some());
        let entry = new_entry.unwrap();
        // Reset should create a new session ID
        assert_ne!(entry.session_id, original.session_id);
        // Reset should clear auto-reset flags
        assert!(!entry.was_auto_reset);
        assert!(!entry.reset_had_activity);
        assert!(!entry.suspended);
        // Token counters should be zeroed
        assert_eq!(entry.total_tokens, 0);
    }

    #[test]
    fn test_session_store_list() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src1 = test_source(Platform::Telegram);
        let mut src2 = test_source(Platform::Discord);
        src2.chat_id = "guild123".to_string();
        src2.chat_type = "group".to_string();

        store.get_or_create_session(src1, false);
        store.get_or_create_session(src2, false);

        let sessions = store.list_sessions(None);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_session_store_transcript_append_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src = test_source(Platform::Telegram);
        let entry = store.get_or_create_session(src, false);

        let msg = serde_json::json!({
            "role": "user",
            "content": "Hello world"
        });
        store.append_to_transcript(&entry.session_id, &msg);

        let loaded = store.load_transcript(&entry.session_id);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["content"], "Hello world");
    }

    #[test]
    fn test_session_store_transcript_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src = test_source(Platform::Telegram);
        let entry = store.get_or_create_session(src, false);

        // First append
        store.append_to_transcript(
            &entry.session_id,
            &serde_json::json!({"role": "user", "content": "old"}),
        );

        // Then rewrite
        store.rewrite_transcript(
            &entry.session_id,
            &[serde_json::json!({"role": "user", "content": "new"})],
        );

        let loaded = store.load_transcript(&entry.session_id);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["content"], "new");
    }

    #[test]
    fn test_session_store_update_session() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src = test_source(Platform::Telegram);
        store.get_or_create_session(src, false);
        let key = "agent:main:telegram:dm:12345";

        store.update_session(key, Some(42));
        assert_eq!(store.session_count(), 1);
    }

    #[test]
    fn test_session_store_suspend_recently_active() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src = test_source(Platform::Telegram);
        store.get_or_create_session(src, false);

        // Within 120 seconds → should be suspended
        let count = store.suspend_recently_active(120);
        assert_eq!(count, 1);

        // Already suspended → should not count again
        let count2 = store.suspend_recently_active(120);
        assert_eq!(count2, 0);
    }

    #[test]
    fn test_session_store_switch_session() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        let src = test_source(Platform::Telegram);
        store.get_or_create_session(src.clone(), false);
        let key = "agent:main:telegram:dm:12345";

        let new_entry = store.switch_session(key, "target_session_123");
        assert!(new_entry.is_some());
        assert_eq!(new_entry.unwrap().session_id, "target_session_123");

        // Switching to same ID → should return unchanged
        let same = store.switch_session(key, "target_session_123");
        assert!(same.is_some());
        assert_eq!(same.unwrap().session_id, "target_session_123");
    }

    #[test]
    fn test_session_store_has_any_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayConfig::default();
        let store = SessionStore::new(tmp.path().to_path_buf(), config);

        assert!(!store.has_any_sessions());

        let src = test_source(Platform::Telegram);
        store.get_or_create_session(src, false);
        assert!(store.has_any_sessions());
    }
}
