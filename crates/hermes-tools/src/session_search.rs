#![allow(dead_code)]
//! Session search tool.
//!
//! Mirrors the Python `tools/session_search_tool.py`.
//! 1 tool: `session_search` — FTS5 search over session messages
//! with optional LLM summarization.
//!
//! Flow:
//!   1. FTS5 search finds matching messages ranked by relevance
//!   2. Groups by session, takes the top N unique sessions (default 3)
//!   3. Loads each session's conversation, truncates to ~100k chars
//!   4. Sends to auxiliary LLM with a focused summarization prompt
//!   5. Returns per-session summaries with metadata

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// Hidden session sources (third-party integrations, delegation children).
const HIDDEN_SOURCES: &[&str] = &["tool"];

/// Max characters per session conversation for summarization.
const MAX_SESSION_CHARS: usize = 100_000;

/// Max summary tokens per session.
const MAX_SUMMARY_TOKENS: usize = 10_000;

/// Check if session search requirements are met.
pub fn check_session_requirements() -> bool {
    let hermes_home = hermes_core::hermes_home::get_hermes_home();
    hermes_home.join("sessions.db").exists()
}

/// FTS5 query sanitizer.
fn sanitize_fts5_query(query: &str) -> String {
    let mut result = String::new();
    let mut in_quote = false;

    for ch in query.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                result.push(ch);
            }
            '+' | '{' | '}' | '(' | ')' | '\\' | '^' => {
                if !in_quote {
                    continue;
                }
                result.push(ch);
            }
            '*' => {
                if in_quote {
                    result.push(ch);
                }
            }
            _ => result.push(ch),
        }
    }

    let trimmed = result.trim();
    let lower = trimmed.to_lowercase();
    if lower.ends_with(" and") || lower.ends_with(" or") || lower.ends_with(" not") {
        return trimmed.rsplit_once(' ').map(|x| x.0).unwrap_or(trimmed).to_string();
    }
    if lower.starts_with("and ") || lower.starts_with("or ") || lower.starts_with("not ") {
        return trimmed.split_once(' ').map(|x| x.1).unwrap_or(trimmed).to_string();
    }

    trimmed.to_string()
}

/// Open the session database.
fn open_session_db() -> Result<hermes_state::SessionDB, String> {
    let hermes_home = hermes_core::hermes_home::get_hermes_home();
    let db_path = hermes_home.join("sessions.db");

    if !db_path.exists() {
        return Err("Session database not found. No sessions have been recorded yet.".to_string());
    }

    hermes_state::SessionDB::open(&db_path)
        .map_err(|e| format!("Failed to open session database: {e}"))
}

/// Format a Unix timestamp to a human-readable date.
fn format_timestamp(ts: f64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Utc.timestamp_opt(ts as i64, 0).single();
    match dt {
        Some(d) => d.format("%B %d, %Y at %I:%M %p").to_string(),
        None => "unknown".to_string(),
    }
}

/// Truncate text around query matches to fit within max_chars.
fn truncate_around_matches(full_text: &str, query: &str, max_chars: usize) -> String {
    if full_text.len() <= max_chars {
        return full_text.to_string();
    }

    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
    let text_lower = full_text.to_lowercase();
    let first_match = query_terms
        .iter()
        .filter_map(|&term| text_lower.find(term))
        .min()
        .unwrap_or(0);

    let half = max_chars / 2;
    let start = first_match.saturating_sub(half).min(full_text.len().saturating_sub(max_chars));
    let end = (start + max_chars).min(full_text.len());

    let prefix = if start > 0 { "...[earlier conversation truncated]...\n\n" } else { "" };
    let suffix = if end < full_text.len() { "\n\n...[later conversation truncated]..." } else { "" };

    format!("{prefix}{}{suffix}", &full_text[start..end])
}

/// Format a session's messages into a readable transcript.
fn format_conversation(messages: &[Value]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown").to_uppercase();
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        let tool_name = msg.get("tool_name").and_then(Value::as_str);

        if role == "TOOL" {
            let content = if content.len() > 500 {
                format!("{}...[truncated]...{}", &content[..250], &content[content.len().saturating_sub(250)..])
            } else {
                content.to_string()
            };
            let tool_label = tool_name.unwrap_or("");
            parts.push(format!("[TOOL:{tool_label}]: {content}"));
        } else if role == "ASSISTANT" {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                let tc_names: Vec<String> = tool_calls
                    .iter()
                    .filter_map(|tc| tc.get("name").and_then(Value::as_str).map(String::from))
                    .collect();
                if !tc_names.is_empty() {
                    parts.push(format!("[ASSISTANT]: [Called: {}]", tc_names.join(", ")));
                }
            }
            if !content.is_empty() {
                parts.push(format!("[ASSISTANT]: {content}"));
            }
        } else {
            parts.push(format!("[{role}]: {content}"));
        }
    }
    parts.join("\n\n")
}

/// Summarize a single session using the auxiliary LLM.
async fn summarize_session(
    conversation_text: &str,
    query: &str,
    source: &str,
    started_at: f64,
) -> Option<String> {
    let system_prompt = concat!(
        "You are reviewing a past conversation transcript to help recall what happened. ",
        "Summarize the conversation with a focus on the search topic. Include:\n",
        "1. What the user asked about or wanted to accomplish\n",
        "2. What actions were taken and what the outcomes were\n",
        "3. Key decisions, solutions found, or conclusions reached\n",
        "4. Any specific commands, files, URLs, or technical details that were important\n",
        "5. Anything left unresolved or notable\n\n",
        "Be thorough but concise. Preserve specific details (commands, paths, error messages) ",
        "that would be useful to recall. Write in past tense as a factual recap."
    );

    let started_fmt = format_timestamp(started_at);
    let user_prompt = format!(
        "Search topic: {query}\nSession source: {source}\nSession date: {started_fmt}\n\n\
         CONVERSATION TRANSCRIPT:\n{conversation_text}\n\n\
         Summarize this conversation with focus on: {query}"
    );

    let request = hermes_llm::client::LlmRequest {
        model: "openai/gpt-4o-mini".to_string(),
        messages: vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": user_prompt}),
        ],
        tools: None,
        temperature: Some(0.1),
        max_tokens: Some(MAX_SUMMARY_TOKENS),
        base_url: None,
        api_key: None,
        timeout_secs: Some(60),
        provider_preferences: None,
        api_mode: None,
    };

    let max_retries = 3;
    for attempt in 0..max_retries {
        match hermes_llm::client::call_llm(request.clone()).await {
            Ok(resp) => {
                if let Some(content) = resp.content {
                    let trimmed = content.trim().to_string();
                    if !trimmed.is_empty() {
                        return Some(trimmed);
                    }
                }
                tracing::warn!(
                    "Session search LLM returned empty content (attempt {}/{})",
                    attempt + 1, max_retries
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Session search LLM call failed (attempt {}/{}): {e}",
                    attempt + 1, max_retries
                );
            }
        }
        if attempt < max_retries - 1 {
            tokio::time::sleep(std::time::Duration::from_secs((attempt as u64) + 1)).await;
        }
    }
    None
}

/// Handle session_search tool call — real FTS5 search + LLM summarization.
pub fn handle_session_search(args: Value) -> Result<String, hermes_core::HermesError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_default();

    let role_filter = args
        .get("role_filter")
        .and_then(Value::as_str)
        .map(|s| s.split(',').map(|r| r.trim().to_string()).collect::<Vec<_>>());

    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(3)
        .min(5) as usize;

    let db = match open_session_db() {
        Ok(db) => db,
        Err(msg) => return Ok(tool_error(&msg)),
    };

    if query.is_empty() {
        return handle_recent_sessions(&db, limit);
    }

    // Sanitize query
    let sanitized = sanitize_fts5_query(&query);
    if sanitized.is_empty() {
        return Ok(tool_error("Search query became empty after sanitization. Use a valid search query."));
    }

    // FTS5 search — get more matches to find unique sessions
    let role_filter_slice: Option<Vec<String>> = role_filter;
    let exclude_sources: Vec<String> = HIDDEN_SOURCES.iter().map(|s| s.to_string()).collect();
    let raw_results = db.search_messages(
        &sanitized,
        None,
        Some(&exclude_sources),
        role_filter_slice.as_deref(),
        50,
        0,
    );

    match raw_results {
        Ok(matches) => {
            if matches.is_empty() {
                return Ok(serde_json::json!({
                    "success": true,
                    "query": sanitized,
                    "results": [],
                    "count": 0,
                    "message": "No matching sessions found for the given query.",
                })
                .to_string());
            }

            // Deduplicate by session_id, take top N unique sessions
            let mut seen = std::collections::HashSet::new();
            let mut unique_session_ids: Vec<String> = Vec::new();
            for m in &matches {
                if let Some(sid) = m.get("session_id").and_then(Value::as_str) {
                    if seen.insert(sid.to_string()) {
                        unique_session_ids.push(sid.to_string());
                        if unique_session_ids.len() >= limit {
                            break;
                        }
                    }
                }
            }

            // Load and summarize each unique session
            let handle = match tokio::runtime::Handle::try_current() {
                Ok(h) => h,
                Err(_) => return Ok(tool_error("No async runtime available")),
            };

            let mut results = Vec::new();
            for sid in &unique_session_ids {
                // Load session messages
                let messages = db.get_messages_as_conversation(sid);
                if let Ok(msgs) = messages {
                    let transcript = format_conversation(&msgs);
                    let truncated = truncate_around_matches(&transcript, &sanitized, MAX_SESSION_CHARS);

                    // Get session metadata
                    let source = db
                        .get_session(sid)
                        .ok()
                        .flatten()
                        .map(|s| s.source)
                        .unwrap_or_else(|| "unknown".to_string());

                    let started_at = db
                        .get_session(sid)
                        .ok()
                        .flatten()
                        .map(|s| s.started_at)
                        .unwrap_or(0.0);

                    // Summarize with LLM
                    let summary = handle.block_on(summarize_session(
                        &truncated, &sanitized, &source, started_at,
                    ));

                    let msg_count = msgs.len();
                    results.push(serde_json::json!({
                        "session_id": sid,
                        "source": source,
                        "started_at": format_timestamp(started_at),
                        "message_count": msg_count,
                        "summary": summary.unwrap_or_else(|| {
                            format!("[LLM summarization unavailable — {msg_count} messages matched]")
                        }),
                    }));
                }
            }

            Ok(serde_json::json!({
                "success": true,
                "query": sanitized,
                "total_matches": matches.len(),
                "unique_sessions": results.len(),
                "results": results,
            })
            .to_string())
        }
        Err(e) => Ok(tool_error(format!("FTS5 search failed: {e}"))),
    }
}

/// Handle "recent sessions" mode (no query provided).
fn handle_recent_sessions(
    db: &hermes_state::SessionDB,
    limit: usize,
) -> Result<String, hermes_core::HermesError> {
    let sessions = db.search_sessions(None, limit + 5, 0);

    match sessions {
        Ok(sessions) => {
            // Filter out child/delegation sessions and hidden sources
            let results: Vec<Value> = sessions
                .iter()
                .filter(|s| {
                    s.parent_session_id.is_none()
                        && !HIDDEN_SOURCES.contains(&s.source.as_str())
                })
                .map(|s| {
                    serde_json::json!({
                        "session_id": s.id,
                        "source": s.source,
                        "model": s.model,
                        "started_at": format_timestamp(s.started_at),
                        "message_count": s.message_count,
                    })
                })
                .take(limit)
                .collect();

            Ok(serde_json::json!({
                "success": true,
                "mode": "recent",
                "count": results.len(),
                "sessions": results,
            })
            .to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to list recent sessions: {e}"))),
    }
}

/// Register the session_search tool.
pub fn register_session_search_tool(registry: &mut ToolRegistry) {
    registry.register(
        "session_search".to_string(),
        "session_search".to_string(),
        serde_json::json!({
            "name": "session_search",
            "description": "Search past conversation history using FTS5 full-text search. Omit 'query' to browse recent sessions without LLM cost.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search keywords, phrases, or boolean expressions (AND/OR/NOT). Omit to browse recent sessions." },
                    "role_filter": { "type": "string", "description": "Comma-separated roles to filter, e.g. 'user,assistant'." },
                    "limit": { "type": "integer", "description": "Max sessions to summarize (1-5, default 3)." }
                }
            }
        }),
        std::sync::Arc::new(handle_session_search),
        Some(std::sync::Arc::new(check_session_requirements)),
        vec!["session_search".to_string()],
        "Search past conversation history".to_string(),
        "🔍".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_fts5_basic() {
        let result = sanitize_fts5_query("hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_sanitize_fts5_strips_special() {
        let result = sanitize_fts5_query("hello + world {test}");
        assert!(!result.contains('+'));
        assert!(!result.contains('{'));
        assert!(!result.contains('}'));
    }

    #[test]
    fn test_sanitize_fts5_preserves_quoted() {
        let result = sanitize_fts5_query("\"hello + world\"");
        assert!(result.contains('+'));
    }

    #[test]
    fn test_sanitize_fts5_dangling_and() {
        let result = sanitize_fts5_query("hello world and");
        assert!(!result.to_lowercase().ends_with(" and"));
    }

    #[test]
    fn test_truncate_around_matches() {
        let text = "A".repeat(200_000) + "FIND_ME" + &"B".repeat(200_000);
        let result = truncate_around_matches(&text, "find_me", MAX_SESSION_CHARS);
        assert!(result.contains("FIND_ME"));
        assert!(result.len() <= MAX_SESSION_CHARS + 100);
    }

    #[test]
    fn test_truncate_short_text() {
        let text = "short text";
        let result = truncate_around_matches(text, "text", 1000);
        assert_eq!(result, text);
    }

    #[test]
    fn test_format_timestamp() {
        let result = format_timestamp(1700000000.0);
        assert!(result.contains("2023"));
    }

    #[test]
    fn test_check_session_requirements() {
        let _ = check_session_requirements();
    }

    #[test]
    fn test_handler_no_db() {
        let result = handle_session_search(serde_json::json!({}));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some() || json.get("mode").is_some());
    }

    #[test]
    fn test_handler_with_query() {
        let result = handle_session_search(serde_json::json!({
            "query": "test search"
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        if json["success"] == true {
            assert!(json.get("query").is_some());
        } else {
            assert!(json.get("error").is_some());
        }
    }

    #[test]
    fn test_handler_limit_capped() {
        let result = handle_session_search(serde_json::json!({
            "query": "test",
            "limit": 100
        }));
        assert!(result.is_ok());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        if json["success"] == true {
            assert!(json["limit"].as_i64().unwrap() <= 5);
        }
    }
}
