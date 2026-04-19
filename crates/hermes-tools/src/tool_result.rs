//! Tool result helper types.
//!
//! Ensures all tool results are valid JSON strings, with consistent
//! error formatting. Mirrors the Python `tool_result()` / `tool_error()`
//! helper functions.

use serde::{Deserialize, Serialize};

/// Standard tool result format.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolResult {
    /// The output/result of the tool execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Error message if the tool failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether this is a partial/truncated result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

impl ToolResult {
    /// Create a successful result.
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            output: Some(output.into()),
            error: None,
            truncated: None,
        }
    }

    /// Create a truncated result.
    pub fn truncated(output: impl Into<String>) -> Self {
        Self {
            output: Some(output.into()),
            error: None,
            truncated: Some(true),
        }
    }

    /// Create an error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            output: None,
            error: Some(message.into()),
            truncated: None,
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            serde_json::json!({ "error": format!("Failed to serialize result: {e}") })
                .to_string()
        })
    }
}

/// Create a JSON error string for tool execution failures.
///
/// All tool handlers must return valid JSON strings, even on error.
/// This function ensures a consistent error format.
pub fn tool_error(message: impl Into<String>) -> String {
    serde_json::json!({ "error": message.into() }).to_string()
}

/// Find a safe byte index that doesn't split a UTF-8 character.
fn safe_truncate(text: &str, max_bytes: usize) -> &str {
    if max_bytes >= text.len() {
        return text;
    }
    // Find the last valid char boundary at or before max_bytes
    let mut safe = max_bytes;
    while safe > 0 && !text.is_char_boundary(safe) {
        safe -= 1;
    }
    &text[..safe]
}

/// Truncate a tool result to the maximum allowed size.
///
/// If the result exceeds `max_chars`, it is truncated with a notice
/// appended. Returns the truncated string and whether it was truncated.
pub fn truncate_result(result: &str, max_chars: usize) -> (String, bool) {
    if result.len() <= max_chars {
        return (result.to_string(), false);
    }

    let notice = format!(
        "\n\n[Output truncated. Showing first {max_chars} characters.]"
    );

    // Trim to make room for the notice, respecting UTF-8 boundaries
    let available = max_chars.saturating_sub(notice.len());
    let mut result = safe_truncate(result, available).to_string();
    result.push_str(&notice);
    (result, true)
}

/// Cap a tool result string, ensuring it does not exceed the limit.
pub fn cap_result(result: &str, max_chars: usize) -> String {
    let (capped, was_truncated) = truncate_result(result, max_chars);
    if was_truncated {
        capped
    } else {
        result.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_success_result() {
        let result = ToolResult::success("hello world");
        let json = result.to_json();
        assert!(json.contains("hello world"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_error_result() {
        let json = tool_error("something went wrong");
        assert!(json.contains("something went wrong"));
        assert!(json.contains("error"));
    }

    #[test]
    fn test_truncate_short() {
        let (result, truncated) = truncate_result("hello", 100);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_long() {
        let long = "a".repeat(200);
        let (result, truncated) = truncate_result(&long, 50);
        assert!(truncated);
        assert!(result.len() <= 50);
        assert!(result.contains("truncated"));
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        // "你好世界" = 12 bytes, 4 chars
        let text = "你好世界".repeat(20);
        let (result, truncated) = truncate_result(&text, 20);
        assert!(truncated);
        // Should not panic and should be valid UTF-8
        let _ = result.len();
        assert!(result.contains("truncated"));
    }
}
