//! Built-in memory provider backed by the local `hermes-state` SQLite database.
//!
//! Provides cross-session recall via FTS5 full-text search over all historical
//! messages. No external services required — SQLite is always available.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use hermes_state::SessionDB;

use crate::agent::types::Message;
use crate::memory_provider::MemoryProvider;

/// Memory provider backed by the local SQLite session database.
///
/// Uses the FTS5 full-text search index to recall relevant past
/// conversations across sessions.
pub struct HermesStateMemoryProvider {
    db: Arc<Mutex<Option<SessionDB>>>,
    max_prefetch_results: usize,
}

impl Default for HermesStateMemoryProvider {
    fn default() -> Self {
        Self {
            db: Arc::new(Mutex::new(None)),
            max_prefetch_results: 10,
        }
    }
}

impl HermesStateMemoryProvider {
    pub fn new() -> Self {
        Self {
            db: Arc::new(Mutex::new(None)),
            max_prefetch_results: 10,
        }
    }

    /// Execute a closure with a reference to the DB, if initialized.
    fn with_db<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&SessionDB) -> R,
    {
        let guard = self.db.lock();
        guard.as_ref().map(f)
    }

    /// Format a timestamp for display.
    fn format_ts(ts: i64) -> String {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| ts.to_string())
    }
}

impl MemoryProvider for HermesStateMemoryProvider {
    fn name(&self) -> &str {
        "hermes-state"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn initialize(&self, session_id: &str, _kwargs: &HashMap<String, Value>) {
        let db = match SessionDB::open_default() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("[HermesStateMemory] Failed to open database: {e}");
                return;
            }
        };
        {
            let mut guard = self.db.lock();
            *guard = Some(db);
        }
        tracing::info!("[HermesStateMemory] Initialized for session {session_id}");
    }

    fn system_prompt_block(&self) -> String {
        if self.db.lock().is_none() {
            return String::new();
        }
        "You have access to historical conversation memory via the `search_memory` tool. \
         Use it to recall past discussions, decisions, and context. Relevant context \
         may also be automatically injected before your response."
            .to_string()
    }

    fn prefetch(&self, query: &str, _session_id: &str) -> String {
        if query.trim().is_empty() {
            return String::new();
        }

        self.with_db(|db| {
            db.search_messages(
                query,
                None,
                None,
                Some(&["user".to_string(), "assistant".to_string()]),
                self.max_prefetch_results,
                0,
            )
        })
        .and_then(|r| r.ok())
        .map(|results| {
            if results.is_empty() {
                return String::new();
            }
            let mut lines = vec!["Relevant past conversations:".to_string()];
            for r in &results {
                let sid = r.get("session_id").and_then(Value::as_str).unwrap_or("?");
                let role = r.get("role").and_then(Value::as_str).unwrap_or("?");
                let snippet = r.get("snippet").and_then(Value::as_str).unwrap_or("");
                let ts = r.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                lines.push(format!("[{sid} {role} @{}]: {snippet}", Self::format_ts(ts)));
            }
            lines.join("\n")
        })
        .unwrap_or_default()
    }

    fn sync_turn(&self, _user_content: &str, _assistant_content: &str, _session_id: &str) {
        // No-op — the agent loop already persists via flush_messages_to_session_db.
    }

    fn get_tool_schemas(&self) -> Vec<Value> {
        if self.db.lock().is_none() {
            return Vec::new();
        }
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "search_memory",
                    "description": "Search your historical conversation memory for relevant past discussions, decisions, and context. Uses full-text search across all past sessions.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query — keywords or phrases to find in past conversations."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results (default 10, max 50).",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "list_recent_sessions",
                    "description": "List your most recent conversation sessions with summaries.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of sessions to return (default 10, max 50).",
                                "default": 10
                            }
                        }
                    }
                }
            }),
        ]
    }

    fn handle_tool_call(
        &self,
        tool_name: &str,
        args: &Map<String, Value>,
        _kwargs: &HashMap<String, Value>,
    ) -> String {
        match tool_name {
            "search_memory" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let limit = args
                    .get("limit")
                    .and_then(Value::as_i64)
                    .map(|l| l.clamp(1, 50) as usize)
                    .unwrap_or(10);

                if query.is_empty() {
                    return json!({"error": "Query is required"}).to_string();
                }

                self.with_db(|db| {
                    db.search_messages(
                        query,
                        None,
                        None,
                        Some(&["user".to_string(), "assistant".to_string()]),
                        limit,
                        0,
                    )
                })
                .and_then(|r| r.ok())
                .map(|results| {
                    if results.is_empty() {
                        return "No matching memories found.".to_string();
                    }
                    let mut lines = vec![format!("Found {} matching memories:", results.len())];
                    for r in &results {
                        let sid = r.get("session_id").and_then(Value::as_str).unwrap_or("?");
                        let role = r.get("role").and_then(Value::as_str).unwrap_or("?");
                        let snippet = r.get("snippet").and_then(Value::as_str).unwrap_or("");
                        let short_id = &sid[..sid.len().min(8)];
                        lines.push(format!("[session {short_id} {role}]: {snippet}"));
                    }
                    lines.join("\n")
                })
                .unwrap_or_else(|| json!({"error": "Memory database not available"}).to_string())
            }
            "list_recent_sessions" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_i64)
                    .map(|l| l.clamp(1, 50) as usize)
                    .unwrap_or(10);

                self.with_db(|db| db.search_sessions(None, limit, 0))
                    .and_then(|r| r.ok())
                    .map(|sessions| {
                        if sessions.is_empty() {
                            return "No sessions found.".to_string();
                        }
                        let mut lines = vec![format!("{} recent sessions:", sessions.len())];
                        for s in &sessions {
                            let title = s.title.as_deref().unwrap_or("untitled");
                            let ts = Self::format_ts(s.started_at as i64);
                            lines.push(format!(
                                "- [{ts}] {title} ({} msgs, {} tokens)",
                                s.message_count,
                                s.input_tokens + s.output_tokens
                            ));
                        }
                        lines.join("\n")
                    })
                    .unwrap_or_else(|| json!({"error": "Memory database not available"}).to_string())
            }
            _ => json!({"error": format!("Unknown tool: {tool_name}")}).to_string(),
        }
    }

    fn on_pre_compress(&self, messages: &[Message]) -> String {
        if messages.len() < 4 {
            return String::new();
        }

        let user_msgs: Vec<&str> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .filter(|c| !c.is_empty() && c.len() < 500)
            .rev()
            .take(3)
            .map(|s| s.trim())
            .collect();

        if user_msgs.is_empty() {
            return String::new();
        }

        format!(
            "[Pre-compression context — topics discussed: {}]",
            user_msgs.join("; ")
        )
    }

    fn shutdown(&self) {
        let mut guard = self.db.lock();
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_metadata() {
        let provider = HermesStateMemoryProvider::new();
        assert_eq!(provider.name(), "hermes-state");
        assert!(provider.is_available());
    }

    #[test]
    fn test_prefetch_empty_without_db() {
        let provider = HermesStateMemoryProvider::new();
        let result = provider.prefetch("test query", "session1");
        assert!(result.is_empty());
    }

    #[test]
    fn test_handle_unknown_tool() {
        let provider = HermesStateMemoryProvider::new();
        let result = provider.handle_tool_call("nonexistent_tool", &Map::new(), &HashMap::new());
        assert!(result.contains("Unknown tool"));
    }

    #[test]
    fn test_search_memory_empty_query() {
        let provider = HermesStateMemoryProvider::new();
        let mut args = Map::new();
        args.insert("query".to_string(), json!(""));
        let result = provider.handle_tool_call("search_memory", &args, &HashMap::new());
        assert!(result.contains("Query is required"));
    }
}
