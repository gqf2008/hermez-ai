#![allow(dead_code)]
//! Token estimation utilities.
//!
//! Character-based rough token estimation (chars/4 heuristic).
//! Mirrors the Python `estimate_tokens_rough` family of functions.

/// Estimate tokens in text using a rough character-count heuristic.
///
/// English text averages ~4 characters per token.
/// This is fast and avoids network calls to tiktoken.
pub fn estimate_tokens_rough(text: &str) -> usize {
    text.len() / 4
}

/// Estimate tokens in a list of message dicts.
///
/// Each message is serialized to JSON string and estimated.
pub fn estimate_messages_tokens_rough(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .map(|msg| estimate_tokens_rough(&msg.to_string()))
        .sum()
}

/// Estimate total request tokens including system prompt and tool schema.
///
/// Accounts for:
/// - System prompt text
/// - Messages array
/// - Tool definitions (JSON schema serialized)
pub fn estimate_request_tokens_rough(
    system_prompt: Option<&str>,
    messages: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
) -> usize {
    let system = system_prompt.map(estimate_tokens_rough).unwrap_or(0);
    let msgs = estimate_messages_tokens_rough(messages);
    let tool_tokens = tools
        .map(|t| t.iter().map(|tool| estimate_tokens_rough(&tool.to_string())).sum())
        .unwrap_or(0);
    system + msgs + tool_tokens
}

/// Check if estimated token count exceeds a limit.
pub fn would_exceed(text: &str, limit: usize) -> bool {
    estimate_tokens_rough(text) > limit
}

/// Truncate text to fit within a token budget.
///
/// Uses the chars/4 heuristic to estimate, truncates to the nearest
/// character boundary.
pub fn truncate_to_budget(text: &str, budget: usize) -> &str {
    let max_chars = budget * 4;
    if text.len() <= max_chars {
        return text;
    }
    // Truncate to nearest char boundary
    &text[..max_chars.min(text.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_rough() {
        // "hello world" = 11 chars -> 11/4 = 2 tokens (integer division)
        assert_eq!(estimate_tokens_rough("hello world"), 2);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens_rough(""), 0);
    }

    #[test]
    fn test_estimate_messages() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi there"}),
        ];
        let tokens = estimate_messages_tokens_rough(&messages);
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_request() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let total = estimate_request_tokens_rough(
            Some("You are a helpful assistant"),
            &messages,
            None,
        );
        assert!(total > 0);
    }

    #[test]
    fn test_would_exceed() {
        assert!(would_exceed("a".repeat(405).as_str(), 100));
        assert!(!would_exceed("a".repeat(400).as_str(), 100));
    }

    #[test]
    fn test_truncate_to_budget() {
        let text = "a".repeat(100);
        let truncated = truncate_to_budget(&text, 10);
        assert_eq!(truncated.len(), 40);
    }

    #[test]
    fn test_truncate_no_op_when_short() {
        let text = "short";
        assert_eq!(truncate_to_budget(text, 100), text);
    }
}
