//! Utility functions for AIAgent.
//!
//! Message sanitization, normalization, token estimation, backoff,
//! stale-call timeout, failure hints, rollback, and thinking budget detection.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::agent::types::Message;

/// Check if any tool call has truncated JSON arguments.
///
/// Returns true when finish_reason indicates length truncation AND
/// any tool_call's function arguments don't parse as valid JSON
/// or don't end with `}` or `]`.
pub(crate) fn has_truncated_tool_args(tool_calls: &[Value]) -> bool {
    for tc in tool_calls {
        if let Some(args_str) = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            let trimmed = args_str.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed.ends_with('}') && !trimmed.ends_with(']') {
                return true;
            }
            if serde_json::from_str::<Value>(trimmed).is_err() {
                return true;
            }
        }
    }
    false
}

/// Check if the base URL is a local endpoint (localhost, 127.0.0.1, etc.).
pub(crate) fn is_local_endpoint(base_url: &str) -> bool {
    let url = base_url.to_lowercase();
    url.contains("://localhost") || url.contains("://127.") || url.contains("://0.0.0.0")
}

/// Estimate token count from message length (rough chars/4 heuristic).
///
/// Mirrors Python: `sum(len(str(v)) for v in messages) // 4` — counts all
/// string fields in each message, not just `content`, so tool calls and
/// metadata are included in the estimate.
pub(crate) fn estimate_tokens(messages: &[Message]) -> usize {
    let mut total = 0;
    for msg in messages {
        if let Some(obj) = msg.as_object() {
            for value in obj.values() {
                if let Some(s) = value.as_str() {
                    total += s.len() / 4;
                } else if let Some(arr) = value.as_array() {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            total += s.len() / 4;
                        }
                    }
                }
            }
        }
    }
    total
}

/// Compute stale-call timeout for non-streaming API calls.
///
/// Mirrors Python: default 300s, scales up for large contexts
/// (>100K tokens → 600s, >50K → 450s), disabled for local endpoints.
pub(crate) fn stale_call_timeout(base_url: Option<&str>, messages: &[Message]) -> Duration {
    const DEFAULT: f64 = 300.0;

    if let Ok(val) = std::env::var("HERMEZ_API_CALL_STALE_TIMEOUT") {
        if let Ok(secs) = val.parse::<f64>() {
            if secs > 0.0 {
                return Duration::from_secs_f64(secs);
            }
        }
    }

    if base_url.is_some_and(is_local_endpoint) {
        return Duration::from_secs(u64::MAX);
    }

    let est_tokens = estimate_tokens(messages);
    let secs = if est_tokens > 100_000 {
        600.0
    } else if est_tokens > 50_000 {
        450.0
    } else {
        DEFAULT
    };
    Duration::from_secs_f64(secs)
}

/// Compute stale-call timeout for streaming API calls.
///
/// Streaming has a separate shorter default (180s) because the provider sends
/// keep-alive chunks. Large contexts scale up. Local endpoints are exempt.
///
/// Mirrors Python HERMES_STREAM_STALE_TIMEOUT + streaming stale detector
/// (run_agent.py:6551+).
pub(crate) fn streaming_stale_call_timeout(base_url: Option<&str>, messages: &[Message]) -> Duration {
    const DEFAULT_STREAM: f64 = 180.0;

    if let Ok(val) = std::env::var("HERMEZ_STREAM_STALE_TIMEOUT") {
        if let Ok(secs) = val.parse::<f64>() {
            if secs > 0.0 {
                return Duration::from_secs_f64(secs);
            }
        }
    }

    // Fall back to non-streaming env var if streaming-specific is unset
    if let Ok(val) = std::env::var("HERMEZ_API_CALL_STALE_TIMEOUT") {
        if let Ok(secs) = val.parse::<f64>() {
            if secs > 0.0 {
                return Duration::from_secs_f64(secs);
            }
        }
    }

    if base_url.is_some_and(is_local_endpoint) {
        return Duration::from_secs(u64::MAX);
    }

    let est_tokens = estimate_tokens(messages);
    let secs = if est_tokens > 100_000 {
        300.0
    } else if est_tokens > 50_000 {
        240.0
    } else {
        DEFAULT_STREAM
    };
    Duration::from_secs_f64(secs)
}

/// Compute exponential backoff in milliseconds based on retry count.
///
/// Mirrors Python: backoff starts at 2s and doubles each retry,
/// with jitter to avoid thundering herd.
pub(crate) fn compute_backoff_ms(retry_count: u32) -> u64 {
    let base_ms = 2000u64;
    let exponent = retry_count.min(5);
    let backoff = base_ms.saturating_mul(1u64 << exponent);
    let jitter = (backoff as f64 * 0.25) as u64;
    backoff.saturating_sub(jitter) + (jitter * 2)
}

/// Build a human-readable failure hint from the error classification.
///
/// Mirrors Python: instead of always assuming "rate limiting", extract
/// HTTP error code (429/504/524/500/503) and response time for context.
pub(crate) fn build_failure_hint(classification: &hermez_llm::error_classifier::ClassifiedError, api_duration: f64) -> String {
    use hermez_llm::error_classifier::FailoverReason;

    match classification.status_code {
        Some(524) => format!("upstream provider timed out (Cloudflare 524, {:.0}s)", api_duration),
        Some(504) => format!("upstream gateway timeout (504, {:.0}s)", api_duration),
        Some(429) => "rate limited by upstream provider (429)".to_string(),
        Some(402) => {
            match classification.reason {
                FailoverReason::Billing => "billing/payment issue — check account".to_string(),
                FailoverReason::RateLimit => "rate limited by upstream provider (402)".to_string(),
                _ => format!("billing or rate limit (402, {:.1}s)", api_duration),
            }
        }
        Some(code @ 500) | Some(code @ 502) => format!("upstream server error (code {code}, {:.0}s)", api_duration),
        Some(code @ 503) | Some(code @ 529) => format!("upstream provider overloaded ({code})"),
        Some(code) => format!("upstream error (code {code}, {:.1}s)", api_duration),
        None => {
            match classification.reason {
                FailoverReason::RateLimit => "likely rate limited by provider".to_string(),
                FailoverReason::Timeout => format!("upstream timeout ({:.0}s)", api_duration),
                FailoverReason::Overloaded => "upstream overloaded".to_string(),
                FailoverReason::ServerError => format!("upstream server error ({:.0}s)", api_duration),
                FailoverReason::Billing => "billing/payment issue — check account".to_string(),
                FailoverReason::Auth | FailoverReason::AuthPermanent => "authentication failed — check API key".to_string(),
                _ if api_duration < 10.0 => format!("fast response ({:.1}s) — likely rate limited", api_duration),
                _ if api_duration > 60.0 => format!("slow response ({:.0}s) — likely upstream timeout", api_duration),
                _ => format!("response time {:.1}s", api_duration),
            }
        }
    }
}

/// Rollback message history to the last complete assistant turn.
///
/// When an unrecoverable error occurs during a conversation turn,
/// discard the last incomplete assistant message and return to the
/// state before it was added.
#[allow(dead_code)]
pub(crate) fn rollback_to_last_assistant(messages: &[Message]) -> Vec<Message> {
    let mut last_assistant_idx: Option<usize> = None;

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" {
            last_assistant_idx = Some(i);
        }
    }

    if let Some(idx) = last_assistant_idx {
        messages[..idx].to_vec()
    } else {
        messages.to_vec()
    }
}

/// Check if the model output contains thinking tags.
///
/// Detects `<think>`, `<thinking>`, `<reasoning>` tags.
/// Used for thinking-exhaustion gating: only reasoning models
/// (Claude, o1/o3) should be marked as having exhausted their
/// thinking budget.
#[allow(dead_code)]
pub(crate) fn has_think_tags(content: &str) -> bool {
    content.contains("<think>") || content.contains("</think>")
        || content.contains("<thinking>") || content.contains("</thinking>")
        || content.contains("<reasoning>") || content.contains("</reasoning>")
}

/// Sanitize messages before sending to the API.
///
/// Mirrors Python `_sanitize_api_messages()` (run_agent.py:~8615).
///
/// Issues fixed:
/// 1. Orphaned tool results — tool messages without a preceding assistant
///    message with matching tool_calls cause API errors.
/// 2. Role sequence violations — two consecutive user/assistant messages.
pub(crate) fn sanitize_api_messages(messages: &[Message]) -> Vec<Message> {
    use std::collections::HashSet;

    let mut result: Vec<Message> = Vec::with_capacity(messages.len());

    let mut valid_tool_call_ids: HashSet<String> = HashSet::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        valid_tool_call_ids.insert(id.to_string());
                    }
                }
            }
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");

        if role == "tool" {
            let tool_call_id = msg.get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !tool_call_id.is_empty() && !valid_tool_call_ids.contains(tool_call_id) {
                tracing::warn!(
                    "Sanitizing orphaned tool result (tool_call_id={})",
                    tool_call_id
                );
                continue;
            }
        }

        if let Some(last) = result.last() {
            let last_role = last.get("role").and_then(Value::as_str).unwrap_or("");
            if role == last_role {
                if role == "user" {
                    let new_content = format!(
                        "{}\n\n{}",
                        last.get("content").and_then(Value::as_str).unwrap_or(""),
                        msg.get("content").and_then(Value::as_str).unwrap_or(""),
                    );
                    if let Some(last_msg) = result.last_mut() {
                        Arc::make_mut(last_msg)["content"] = Value::String(new_content);
                    }
                    continue;
                } else if role == "assistant" {
                    let new_content = format!(
                        "{}\n\n{}",
                        last.get("content").and_then(Value::as_str).unwrap_or(""),
                        msg.get("content").and_then(Value::as_str).unwrap_or(""),
                    );
                    let msg_tool_calls: Option<Vec<Value>> = msg.get("tool_calls")
                        .and_then(Value::as_array)
                        .map(|arr: &Vec<Value>| {
                            arr.iter().filter(|tc| !tc.is_null()).cloned().collect()
                        })
                        .filter(|arr: &Vec<Value>| !arr.is_empty());
                    if let Some(last_msg) = result.last_mut() {
                        let value = Arc::make_mut(last_msg);
                        value["content"] = Value::String(new_content);
                        if let Some(tool_calls) = msg_tool_calls {
                            if let Some(existing) = value.get_mut("tool_calls") {
                                if let Some(existing_arr) = existing.as_array_mut() {
                                    existing_arr.extend(tool_calls);
                                }
                            } else {
                                value["tool_calls"] = serde_json::Value::Array(tool_calls);
                            }
                        }
                    }
                    continue;
                }
            }
        }

        result.push(Arc::new((**msg).clone()));
    }

    result
}

/// Normalize messages for consistent prompt caching and comparison.
///
/// Mirrors Python message normalization (run_agent.py:~8623-8645).
///
/// Operations:
/// 1. Strip leading/trailing whitespace from assistant text content.
/// 2. Canonicalize tool-call JSON arguments (sort keys, compact) for
///    consistent cache prefix matching.
/// 3. Normalize empty content to empty string.
pub(crate) fn normalize_messages(messages: &[Message]) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());

    for msg in messages {
        let mut msg = (**msg).clone();
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("").to_string();

        if role == "assistant" {
            if let Some(content) = msg.get_mut("content") {
                if let Some(s) = content.as_str() {
                    let trimmed = s.trim();
                    if trimmed != s {
                        *content = Value::String(trimmed.to_string());
                    }
                }
            }
        }

        if role == "assistant" {
            if let Some(tool_calls) = msg.get_mut("tool_calls") {
                if let Some(arr) = tool_calls.as_array_mut() {
                    for tc in arr {
                        if let Some(args) = tc.get_mut("function")
                            .and_then(|f| f.get_mut("arguments"))
                        {
                            if let Some(args_str) = args.as_str() {
                                if let Ok(parsed) = serde_json::from_str::<Value>(args_str) {
                                    let canonical = serde_json::to_string(&parsed)
                                        .unwrap_or_else(|_| args_str.to_string());
                                    *args = Value::String(canonical);
                                }
                            }
                        }
                    }
                }
            }
        }

        result.push(Arc::new(msg));
    }

    result
}

/// Detect thinking-budget exhaustion.
///
/// Mirrors Python thinking-exhaustion detection (run_agent.py:~9049-9123).
///
/// When reasoning models exhaust their thinking/output token budget, the API
/// returns `finish_reason="length"` but the content may contain valid text.
/// This function determines whether to treat the response as a genuine
/// completion or as a budget-exhausted partial response.
pub(crate) fn is_thinking_budget_exhausted(response: &Value, model: &str) -> bool {
    let finish_reason = response.get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("");

    let is_reasoning_model = model.starts_with("anthropic/claude")
        || model.starts_with("openai/o1")
        || model.starts_with("openai/o3")
        || model.starts_with("openai/o4");

    if !is_reasoning_model {
        return false;
    }

    if !matches!(finish_reason, "length" | "length_limit") {
        return false;
    }

    let content = response.get("content")
        .and_then(Value::as_str)
        .unwrap_or("");

    if content.is_empty() {
        return true;
    }

    let has_open_think = content.contains("<think>")
        || content.contains("<thinking>")
        || content.contains("<reasoning>");
    let has_close_think = content.contains("</think>")
        || content.contains("</thinking>")
        || content.contains("</reasoning>");

    has_open_think && !has_close_think
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── has_truncated_tool_args ───────────────────────────────────────────────

    #[test]
    fn test_truncated_args_incomplete_json() {
        let tc = vec![serde_json::json!({
            "function": {"arguments": "{\"x\": 1"}
        })];
        assert!(has_truncated_tool_args(&tc));
    }

    #[test]
    fn test_truncated_args_valid_json() {
        let tc = vec![serde_json::json!({
            "function": {"arguments": "{\"x\": 1}"}
        })];
        assert!(!has_truncated_tool_args(&tc));
    }

    #[test]
    fn test_truncated_args_empty() {
        let tc: Vec<Value> = vec![];
        assert!(!has_truncated_tool_args(&tc));
    }

    // ── is_local_endpoint ─────────────────────────────────────────────────────

    #[test]
    fn test_local_endpoint_localhost() {
        assert!(is_local_endpoint("http://localhost:11434/v1"));
    }

    #[test]
    fn test_local_endpoint_127() {
        assert!(is_local_endpoint("https://127.0.0.1:8080"));
    }

    #[test]
    fn test_local_endpoint_remote() {
        assert!(!is_local_endpoint("https://api.openai.com/v1"));
    }

    // ── estimate_tokens ───────────────────────────────────────────────────────

    #[test]
    fn test_estimate_tokens_basic() {
        let msg = Arc::new(serde_json::json!({
            "role": "user",
            "content": "hello world"
        }));
        let est = estimate_tokens(&[msg]);
        // Counts all string values: "user" (4/4=1) + "hello world" (11/4=2) = 3
        assert_eq!(est, 3);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        let est = estimate_tokens(&[]);
        assert_eq!(est, 0);
    }

    // ── compute_backoff_ms ────────────────────────────────────────────────────

    #[test]
    fn test_backoff_increases() {
        let b0 = compute_backoff_ms(0);
        let b1 = compute_backoff_ms(1);
        let b2 = compute_backoff_ms(2);
        assert!(b1 >= b0);
        assert!(b2 >= b1);
    }

    #[test]
    fn test_backoff_capped() {
        let b5 = compute_backoff_ms(5);
        let b10 = compute_backoff_ms(10);
        assert_eq!(b5, b10); // capped at exponent 5
    }

    // ── rollback_to_last_assistant ────────────────────────────────────────────

    #[test]
    fn test_rollback_finds_last_assistant() {
        let msgs = vec![
            Arc::new(serde_json::json!({"role": "user"})),
            Arc::new(serde_json::json!({"role": "assistant"})),
            Arc::new(serde_json::json!({"role": "user"})),
            Arc::new(serde_json::json!({"role": "assistant"})),
            Arc::new(serde_json::json!({"role": "tool"})),
        ];
        let rolled = rollback_to_last_assistant(&msgs);
        assert_eq!(rolled.len(), 3);
    }

    #[test]
    fn test_rollback_no_assistant() {
        let msgs = vec![
            Arc::new(serde_json::json!({"role": "user"})),
            Arc::new(serde_json::json!({"role": "user"})),
        ];
        let rolled = rollback_to_last_assistant(&msgs);
        assert_eq!(rolled.len(), 2);
    }

    // ── has_think_tags ────────────────────────────────────────────────────────

    #[test]
    fn test_think_tags_detected() {
        assert!(has_think_tags("<think>some reasoning</think>"));
        assert!(has_think_tags("<thinking>..."));
        assert!(has_think_tags("<reasoning>...</reasoning>"));
    }

    #[test]
    fn test_think_tags_not_detected() {
        assert!(!has_think_tags("normal text"));
        assert!(!has_think_tags(""));
    }

    // ── sanitize_api_messages ─────────────────────────────────────────────────

    #[test]
    fn test_sanitize_merges_consecutive_user() {
        let msgs = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "user", "content": "world"})),
        ];
        let out = sanitize_api_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "hello\n\nworld");
    }

    #[test]
    fn test_sanitize_removes_orphaned_tool() {
        let msgs = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hi"})),
            Arc::new(serde_json::json!({
                "role": "tool",
                "tool_call_id": "nonexistent",
                "content": "result"
            })),
        ];
        let out = sanitize_api_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
    }

    // ── normalize_messages ────────────────────────────────────────────────────

    #[test]
    fn test_normalize_trims_assistant_content() {
        let msgs = vec![
            Arc::new(serde_json::json!({
                "role": "assistant",
                "content": "  trimmed  "
            })),
        ];
        let out = normalize_messages(&msgs);
        assert_eq!(out[0]["content"], "trimmed");
    }

    // ── is_thinking_budget_exhausted ──────────────────────────────────────────

    #[test]
    fn test_thinking_exhausted_reasoning_model_length() {
        let resp = serde_json::json!({
            "finish_reason": "length",
            "content": "<think>partial"
        });
        assert!(is_thinking_budget_exhausted(&resp, "anthropic/claude-opus-4-6"));
    }

    #[test]
    fn test_thinking_not_exhausted_non_reasoning() {
        let resp = serde_json::json!({
            "finish_reason": "length",
            "content": "<think>partial"
        });
        assert!(!is_thinking_budget_exhausted(&resp, "openai/gpt-4o"));
    }

    #[test]
    fn test_thinking_not_exhausted_complete_tags() {
        let resp = serde_json::json!({
            "finish_reason": "length",
            "content": "<think>done</think>"
        });
        assert!(!is_thinking_budget_exhausted(&resp, "anthropic/claude-opus-4-6"));
    }

    #[test]
    fn test_thinking_not_exhausted_stop_reason() {
        let resp = serde_json::json!({
            "finish_reason": "stop",
            "content": "<think>partial"
        });
        assert!(!is_thinking_budget_exhausted(&resp, "anthropic/claude-opus-4-6"));
    }
}
