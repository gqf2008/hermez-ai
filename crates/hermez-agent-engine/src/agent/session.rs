//! Session persistence methods for AIAgent.
//!
//! Mirrors Python `_persist_session`, `_flush_messages_to_session_db`,
//! `_save_trajectory`, `_save_session_log` (run_agent.py:2436-2750).

use serde_json::Value;
use std::sync::Arc;

use super::AIAgent;
use crate::agent::types::Message;
use crate::memory_provider::MemoryProvider;

impl AIAgent {
    /// Shutdown the agent, cleaning up memory providers and other resources.
    pub fn shutdown(&self) {
        self.memory_manager.shutdown_all();
        if let Some(ref db) = self.session_db {
            db.close();
        }
    }

    /// Register an external memory provider.
    pub fn register_memory_provider(&mut self, provider: Arc<dyn MemoryProvider>) {
        self.memory_manager.add_provider(provider);
    }

    /// Initialize memory providers for a session.
    pub fn init_memory(&self, session_id: &str) {
        self.memory_manager.initialize_all(session_id, std::collections::HashMap::new());
    }

    // ── Session persistence ────────────────────────────────────────────────

    /// Save session state to both JSON log and SQLite on any exit path.
    ///
    /// Mirrors Python `_persist_session()` (run_agent.py:2436).
    /// Ensures conversations are never lost, even on errors or early returns.
    /// Skipped when `persist_session=false` (ephemeral helper flows).
    pub fn persist_session(&mut self, messages: &[Message], _user_query: &str, completed: bool) {
        if !self.persist_session {
            return;
        }
        let session_id = match self.config.session_id.clone() {
            Some(sid) => sid,
            None => return,
        };
        self.flush_messages_to_session_db(messages);
        self.save_trajectory(messages, _user_query, completed, &session_id);
        self.save_session_log(&session_id);
    }

    /// Flush buffered messages to the SQLite session store.
    ///
    /// Mirrors Python `_flush_messages_to_session_db()` (run_agent.py:2449).
    /// Uses `last_flushed_db_idx` to track which messages have already been
    /// written, so repeated calls only write truly new messages.
    pub fn flush_messages_to_session_db(&mut self, messages: &[Message]) {
        let db = match &self.session_db {
            Some(db) => db,
            None => return,
        };
        let session_id = match &self.config.session_id {
            Some(sid) => sid.clone(),
            None => return,
        };

        let flush_from = self.last_flushed_db_idx;
        if flush_from >= messages.len() {
            return;
        }

        let platform = self.config.platform.clone().unwrap_or_else(|| "cli".to_string());
        let model = self.config.model.clone();

        if let Err(e) = db.ensure_session(&session_id, &platform, Some(&model)) {
            tracing::warn!("Session DB ensure_session failed: {e}");
            return;
        }

        let mut all_ok = true;
        for msg in &messages[flush_from..] {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
            let content = msg.get("content").and_then(Value::as_str);

            let tool_calls_str = msg.get("tool_calls")
                .and_then(|tc| serde_json::to_string(tc).ok());

            let tool_call_id = msg.get("tool_call_id").and_then(Value::as_str);
            let tool_name = msg.get("tool_name").and_then(Value::as_str);
            let finish_reason = msg.get("finish_reason").and_then(Value::as_str);

            let reasoning = if role == "assistant" {
                msg.get("reasoning").and_then(Value::as_str)
            } else {
                None
            };
            let reasoning_details = if role == "assistant" {
                msg.get("reasoning_details").and_then(Value::as_str)
            } else {
                None
            };
            let codex_reasoning_items = if role == "assistant" {
                msg.get("codex_reasoning_items").and_then(Value::as_str)
            } else {
                None
            };

            if let Err(e) = db.append_message(
                &session_id, role, content,
                tool_name, tool_calls_str.as_deref(), tool_call_id,
                None, finish_reason, reasoning, reasoning_details,
                codex_reasoning_items,
            ) {
                tracing::warn!("Session DB append_message failed: {e}");
                all_ok = false;
            }
        }

        if all_ok {
            self.last_flushed_db_idx = messages.len();
        }
    }

    /// Save conversation trajectory to JSONL file.
    ///
    /// Mirrors Python `_save_trajectory()` (run_agent.py:2717).
    /// Saves to `~/.hermez/trajectories/` directory.
    pub fn save_trajectory(
        &self,
        messages: &[Message],
        _user_query: &str,
        completed: bool,
        session_id: &str,
    ) {
        use crate::trajectory::{messages_to_conversation, save_trajectory as save_traj};

        let conversations = messages_to_conversation(messages);
        if conversations.is_empty() {
            return;
        }

        let trajectories_dir = hermez_core::get_hermez_home().join("trajectories");
        if let Err(e) = std::fs::create_dir_all(&trajectories_dir) {
            tracing::warn!("Failed to create trajectories dir: {e}");
            return;
        }

        let filename = if completed {
            format!("trajectory_{}.jsonl", session_id.chars().take(12).collect::<String>())
        } else {
            format!("failed_trajectory_{}.jsonl", session_id.chars().take(12).collect::<String>())
        };
        let path = trajectories_dir.join(&filename);

        match save_traj(conversations, &self.config.model, completed, Some(&path)) {
            Ok(p) => tracing::info!("Saved trajectory to {}", p.display()),
            Err(e) => tracing::warn!("Failed to save trajectory: {e}"),
        }
    }

    /// Save session log file.
    ///
    /// Mirrors Python `_save_session_log()` (run_agent.py:2733).
    /// Writes a JSON summary of the session to `~/.hermez/logs/`.
    pub fn save_session_log(&self, session_id: &str) {
        let logs_dir = hermez_core::get_hermez_home().join("logs");
        if let Err(e) = std::fs::create_dir_all(&logs_dir) {
            tracing::warn!("Failed to create logs dir: {e}");
            return;
        }

        let filename = format!("session_{}.json", session_id.chars().take(12).collect::<String>());
        let path = logs_dir.join(&filename);

        let log_entry = serde_json::json!({
            "session_id": session_id,
            "model": self.config.model,
            "platform": self.config.platform,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "last_flushed_idx": self.last_flushed_db_idx,
        });

        match serde_json::to_string_pretty(&log_entry) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("Failed to write session log: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to serialize session log: {e}"),
        }
    }
}
