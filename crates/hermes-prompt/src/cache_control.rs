#![allow(dead_code)]
//! Anthropic prompt caching — `system_and_3` strategy.
//!
//! Mirrors the Python `agent/prompt_caching.py`.
//! Reduces input token costs by ~75% on multi-turn conversations by caching
//! the conversation prefix. Uses 4 cache_control breakpoints (Anthropic max):
//!
//!   1. System prompt (stable across all turns)
//!      2-4. Last 3 non-system messages (rolling window)

use serde_json::{Map, Value};

/// Cache TTL options.
#[derive(Debug, Clone, Copy, Default)]
pub enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
}

/// Apply cache_control markers to API messages.
///
/// Deep-copies the messages and injects `cache_control` breakpoints:
/// - Breakpoint 1: system prompt (first message if role == "system")
/// - Breakpoints 2-4: last 3 non-system messages
pub fn apply_anthropic_cache_control(
    api_messages: &[Value],
    ttl: CacheTtl,
    native_anthropic: bool,
) -> Vec<Value> {
    if api_messages.is_empty() {
        return vec![];
    }

    let mut messages: Vec<Value> = api_messages.to_vec();

    let mut marker = Map::new();
    marker.insert("type".to_string(), Value::String("ephemeral".to_string()));
    if matches!(ttl, CacheTtl::OneHour) {
        marker.insert("ttl".to_string(), Value::String("1h".to_string()));
    }
    let marker = Value::Object(marker);

    let mut breakpoints_used = 0;

    // Breakpoint 1: system prompt
    if let Some(first) = messages.first() {
        if first.get("role").and_then(Value::as_str) == Some("system") {
            apply_cache_marker(&mut messages[0], &marker, native_anthropic);
            breakpoints_used += 1;
        }
    }

    // Breakpoints 2-4: last 3 non-system messages
    let remaining = 4 - breakpoints_used;
    let non_sys_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) != Some("system"))
        .map(|(i, _)| i)
        .collect();

    for &idx in non_sys_indices.iter().rev().take(remaining) {
        apply_cache_marker(&mut messages[idx], &marker, native_anthropic);
    }

    messages
}

/// Apply a cache_control marker to a single message.
fn apply_cache_marker(msg: &mut Value, marker: &Value, native_anthropic: bool) {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");

    // Tool role: only set at message level for native Anthropic
    if role == "tool" {
        if native_anthropic {
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("cache_control".to_string(), marker.clone());
            }
        }
        return;
    }

    let content = msg.get("content").cloned();

    // None or empty content: set at message level
    if content.is_none()
        || content
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|s| s.is_empty())
    {
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("cache_control".to_string(), marker.clone());
        }
        return;
    }

    // String content: convert to array with cache_control on the block
    if let Some(text) = content.as_ref().and_then(Value::as_str) {
        let text_block = serde_json::json!({
            "type": "text",
            "text": text,
            "cache_control": marker,
        });
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(vec![text_block]));
        }
        return;
    }

    // Array content: attach to last block
    if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(last) = arr.last_mut() {
            if let Some(obj) = last.as_object_mut() {
                obj.insert("cache_control".to_string(), marker.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_messages() {
        let result = apply_anthropic_cache_control(&[], CacheTtl::FiveMinutes, false);
        assert!(result.is_empty());
    }

    #[test]
    fn test_system_prompt_cached() {
        let messages = vec![serde_json::json!({
            "role": "system",
            "content": "You are a helpful assistant."
        })];
        let result =
            apply_anthropic_cache_control(&messages, CacheTtl::FiveMinutes, false);
        // System prompt should have cache_control on content
        let content = result[0].get("content");
        assert!(content.is_some());
        if let Some(arr) = content.and_then(Value::as_array) {
            assert!(arr[0].get("cache_control").is_some());
        }
    }

    #[test]
    fn test_non_system_messages_cached() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({"role": "user", "content": "Hi"}),
            serde_json::json!({"role": "assistant", "content": "Hello!"}),
            serde_json::json!({"role": "user", "content": "How are you?"}),
            serde_json::json!({"role": "assistant", "content": "Fine!"}),
        ];
        let result =
            apply_anthropic_cache_control(&messages, CacheTtl::FiveMinutes, false);

        // System + last 3 non-system = 4 breakpoints total
        // System should have cache_control
        assert!(result[0].get("content").and_then(Value::as_array).is_some());

        // Last 3 non-system messages (indices 1, 2, 3, 4 → last 3 are 2, 3, 4)
        // should have cache_control
        let mut cached_count = 0;
        for msg in &result {
            if has_cache_marker(msg) {
                cached_count += 1;
            }
        }
        assert_eq!(cached_count, 4); // system + 3 non-system
    }

    #[test]
    fn test_one_hour_ttl() {
        let messages = vec![serde_json::json!({
            "role": "system",
            "content": "Be helpful."
        })];
        let result = apply_anthropic_cache_control(&messages, CacheTtl::OneHour, false);
        let content = result[0].get("content").and_then(Value::as_array).unwrap();
        let cache_ctrl = content[0].get("cache_control").unwrap();
        assert_eq!(cache_ctrl.get("ttl").and_then(Value::as_str), Some("1h"));
    }

    #[test]
    fn test_tool_role_native() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({"role": "tool", "content": "result"}),
        ];
        let result = apply_anthropic_cache_control(&messages, CacheTtl::FiveMinutes, true);
        // Tool message with native_anthropic should have cache_control at message level
        assert!(result[1].get("cache_control").is_some());
    }

    #[test]
    fn test_tool_role_non_native() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({"role": "tool", "content": "result"}),
        ];
        let result = apply_anthropic_cache_control(&messages, CacheTtl::FiveMinutes, false);
        // Tool message without native_anthropic should NOT have cache_control
        assert!(result[1].get("cache_control").is_none());
    }

    #[test]
    fn test_array_content_last_block() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Hello"},
                    {"type": "text", "text": "World"}
                ]
            }),
        ];
        let result =
            apply_anthropic_cache_control(&messages, CacheTtl::FiveMinutes, false);
        let content = result[1].get("content").and_then(Value::as_array).unwrap();
        // Last block should have cache_control
        assert!(content[1].get("cache_control").is_some());
        // First block should not
        assert!(content[0].get("cache_control").is_none());
    }

    fn has_cache_marker(msg: &Value) -> bool {
        if msg.get("cache_control").is_some() {
            return true;
        }
        if let Some(arr) = msg.get("content").and_then(Value::as_array) {
            if let Some(last) = arr.last() {
                return last.get("cache_control").is_some();
            }
        }
        false
    }
}
