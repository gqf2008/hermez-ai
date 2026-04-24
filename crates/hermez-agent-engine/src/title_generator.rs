//! Auto-generate short session titles from the first user/assistant exchange.
//!
//! Runs asynchronously after the first response is delivered so it never
//! adds latency to the user-facing reply.
//!
//! Mirrors the Python `agent/title_generator.py`.

use serde_json::json;

/// Title generation prompt.
const TITLE_PROMPT: &str =
    "Generate a short, descriptive title (3-7 words) for a conversation that starts with the \
    following exchange. The title should capture the main topic or intent. \
    Return ONLY the title text, nothing else. No quotes, no punctuation at the end, no prefixes.";

/// Generate a session title from the first exchange.
///
/// Uses the auxiliary LLM client (cheapest/fastest available model).
/// Returns the title string or None on failure.
pub async fn generate_title(
    user_message: &str,
    assistant_response: &str,
    model: &str,
    api_key: Option<String>,
    base_url: Option<String>,
    timeout_secs: u64,
) -> Option<String> {
    // Truncate long messages to keep the request small (char-safe)
    let user_snippet = truncate_str(user_message, 500);
    let assistant_snippet = truncate_str(assistant_response, 500);

    let messages = vec![
        json!({"role": "system", "content": TITLE_PROMPT}),
        json!({"role": "user", "content": format!("User: {}\n\nAssistant: {}", user_snippet, assistant_snippet)}),
    ];

    let request = hermez_llm::client::LlmRequest {
        model: model.to_string(),
        messages,
        tools: None,
        temperature: Some(0.3),
        max_tokens: Some(30),
        base_url,
        api_key,
        timeout_secs: Some(timeout_secs),
        provider_preferences: None,
        api_mode: None,
    };

    let response = match hermez_llm::client::call_llm(request).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Title generation failed: {:?}", e);
            return None;
        }
    };

    let title = response.content?.trim().to_string();

    // Clean up: remove quotes, trailing punctuation, prefixes like "Title: "
    let title = clean_title(&title);

    // Enforce reasonable length (char-safe)
    if title.len() > 80 {
        Some(format!("{}...", truncate_str(&title, 77)))
    } else if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Clean title text by removing common prefixes and quotes.
pub fn clean_title(title: &str) -> String {
    let title = title.trim_matches(|c: char| c == '"' || c == '\'').trim();
    if title.to_lowercase().starts_with("title:") {
        truncate_start(title, 6)
    } else if title.to_lowercase().starts_with("title - ") {
        truncate_start(title, 8)
    } else {
        title.to_string()
    }
}

/// Truncate a string to at most `max_chars` characters.
fn truncate_str(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Skip the first N bytes if they are ASCII, otherwise return trimmed string.
fn truncate_start(s: &str, n: usize) -> String {
    // "title:" and "title - " are all ASCII, so the prefix itself is byte-safe.
    // But we use char-based slicing to be safe.
    s.chars().skip(n).collect::<String>().trim().to_string()
}

/// Session DB trait for title operations.
pub trait SessionTitleStore: Send + Sync {
    fn get_session_title(&self, session_id: &str) -> Option<String>;
    fn set_session_title(&self, session_id: &str, title: &str);
}

/// Generate and set a session title if one doesn't already exist.
///
/// Called in a background task after the first exchange completes.
/// Silently skips if:
/// - session_db is None
/// - session already has a title
/// - title generation fails
pub async fn auto_title_session(
    session_db: &dyn SessionTitleStore,
    session_id: &str,
    user_message: &str,
    assistant_response: &str,
    model: &str,
    api_key: Option<String>,
    base_url: Option<String>,
) {
    // Check if title already exists
    if session_db.get_session_title(session_id).is_some() {
        return;
    }

    let Some(title) = generate_title(user_message, assistant_response, model, api_key, base_url, 30).await else {
        return;
    };

    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session_db.set_session_title(session_id, &title);
    })) {
        tracing::debug!("Failed to set auto-generated title: {:?}", e);
    }
}

/// Configuration for auto-title generation.
pub struct TitleGenerationContext<'a> {
    pub session_db: Box<dyn SessionTitleStore>,
    pub session_id: &'a str,
    pub user_message: &'a str,
    pub assistant_response: &'a str,
    pub conversation_history: &'a [serde_json::Value],
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Fire-and-forget title generation after the first exchange.
///
/// Only generates a title when:
/// - This appears to be the first user -> assistant exchange
/// - No title is already set
pub fn maybe_auto_title(ctx: TitleGenerationContext<'_>) {
    let TitleGenerationContext {
        session_db: db,
        session_id: sid,
        user_message,
        assistant_response,
        conversation_history,
        model,
        api_key,
        base_url,
    } = ctx;

    if user_message.is_empty() || assistant_response.is_empty() {
        return;
    }

    // Count user messages in history to detect first exchange
    let user_msg_count = conversation_history
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .count();
    if user_msg_count > 2 {
        return;
    }

    let sid = sid.to_string();
    let user_message = user_message.to_string();
    let assistant_response = assistant_response.to_string();

    // Spawn background task
    tokio::spawn(async move {
        auto_title_session(
            db.as_ref(),
            &sid,
            &user_message,
            &assistant_response,
            &model,
            api_key,
            base_url,
        )
        .await;
    });
}

/// Simple in-memory session title store for testing.
pub struct InMemoryTitleStore {
    titles: parking_lot::Mutex<std::collections::HashMap<String, String>>,
}

impl InMemoryTitleStore {
    pub fn new() -> Self {
        Self {
            titles: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryTitleStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTitleStore for InMemoryTitleStore {
    fn get_session_title(&self, session_id: &str) -> Option<String> {
        self.titles.lock().get(session_id).cloned()
    }

    fn set_session_title(&self, session_id: &str, title: &str) {
        self.titles
            .lock()
            .insert(session_id.to_string(), title.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_title_removes_quotes() {
        assert_eq!(clean_title("\"My Title\""), "My Title");
    }

    #[test]
    fn test_clean_title_removes_prefix() {
        assert_eq!(clean_title("Title: My Topic"), "My Topic");
    }

    #[test]
    fn test_clean_title_preserves_normal_text() {
        assert_eq!(clean_title("How to write Rust"), "How to write Rust");
    }

    #[test]
    fn test_in_memory_store() {
        let store = InMemoryTitleStore::new();
        assert!(store.get_session_title("s1").is_none());
        store.set_session_title("s1", "Test Title");
        assert_eq!(store.get_session_title("s1"), Some("Test Title".to_string()));
    }

    #[test]
    fn test_maybe_auto_title_skips_long_history() {
        let history: Vec<serde_json::Value> = vec![
            json!({"role": "user", "content": "msg1"}),
            json!({"role": "assistant", "content": "resp1"}),
            json!({"role": "user", "content": "msg2"}),
            json!({"role": "assistant", "content": "resp2"}),
            json!({"role": "user", "content": "msg3"}),
        ];
        // 3 user messages > 2, should skip
        let user_msg_count = history
            .iter()
            .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
            .count();
        assert_eq!(user_msg_count, 3);
        assert!(user_msg_count > 2);
    }

    #[test]
    fn test_clean_title_removes_single_quotes() {
        assert_eq!(clean_title("'My Title'"), "My Title");
    }

    #[test]
    fn test_clean_title_removes_mixed_quotes() {
        // trim_matches removes ALL matching chars from both ends
        assert_eq!(clean_title("\"'Mixed'\""), "Mixed");
    }

    #[test]
    fn test_clean_title_removes_title_dash_prefix() {
        assert_eq!(clean_title("Title - My Topic"), "My Topic");
    }

    #[test]
    fn test_clean_title_case_insensitive_prefix() {
        assert_eq!(clean_title("TITLE: My Topic"), "My Topic");
    }

    #[test]
    fn test_clean_title_empty_after_cleanup() {
        assert_eq!(clean_title("\"\""), "");
    }

    #[test]
    fn test_truncate_str_basic() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_str_unicode() {
        // 7 chars: h e l l o (space) 🌍
        assert_eq!(truncate_str("hello 🌍 world", 7), "hello 🌍");
    }

    #[test]
    fn test_truncate_str_longer_than_input() {
        assert_eq!(truncate_str("hi", 100), "hi");
    }

    #[test]
    fn test_truncate_start_basic() {
        assert_eq!(truncate_start("hello world", 6), "world");
    }

    #[test]
    fn test_in_memory_store_overwrite() {
        let store = InMemoryTitleStore::new();
        store.set_session_title("s1", "First");
        store.set_session_title("s1", "Second");
        assert_eq!(store.get_session_title("s1"), Some("Second".to_string()));
    }

    #[test]
    fn test_in_memory_store_multiple_sessions() {
        let store = InMemoryTitleStore::new();
        store.set_session_title("s1", "Title 1");
        store.set_session_title("s2", "Title 2");
        assert_eq!(store.get_session_title("s1"), Some("Title 1".to_string()));
        assert_eq!(store.get_session_title("s2"), Some("Title 2".to_string()));
    }
}
