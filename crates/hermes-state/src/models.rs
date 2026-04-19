//! Data models for session and message records.

use serde::{Deserialize, Serialize};

/// Maximum length for session titles.
pub const MAX_TITLE_LENGTH: usize = 100;

/// Session metadata record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub source: String,
    pub user_id: Option<String>,
    pub model: Option<String>,
    pub model_config: Option<String>,
    pub system_prompt: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub end_reason: Option<String>,
    pub message_count: i64,
    pub tool_call_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub billing_provider: Option<String>,
    pub billing_base_url: Option<String>,
    pub billing_mode: Option<String>,
    pub estimated_cost_usd: Option<f64>,
    pub actual_cost_usd: Option<f64>,
    pub cost_status: Option<String>,
    pub cost_source: Option<String>,
    pub pricing_version: Option<String>,
    pub title: Option<String>,
}

/// Message record within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub timestamp: f64,
    pub token_count: Option<i64>,
    pub finish_reason: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_details: Option<String>,
    pub codex_reasoning_items: Option<String>,
}

/// Result for session listing with preview.
#[derive(Debug, Clone)]
pub struct SessionWithPreview {
    pub session: Session,
    pub preview: String,
    pub last_active: f64,
}

/// Sanitize a session title.
///
/// - Strips leading/trailing whitespace
/// - Removes ASCII control characters (except \t, \n, \r)
/// - Removes zero-width and directional Unicode chars
/// - Collapses internal whitespace to single spaces
/// - Normalizes empty/whitespace-only to None
/// - Enforces MAX_TITLE_LENGTH
pub fn sanitize_title(title: &str) -> Option<String> {
    if title.is_empty() {
        return None;
    }

    // Remove ASCII control chars (keep \t, \n, \r)
    let cleaned: String = title
        .chars()
        .filter(|c| {
            let u = *c as u32;
            if u <= 0x1F {
                matches!(u, 0x09 | 0x0A | 0x0D)
            } else if u == 0x7F {
                false
            } else {
                // Remove zero-width and directional overrides
                !(matches!(u,
                    0x200B..=0x200F |
                    0x2028..=0x202E |
                    0x2060..=0x2069 |
                    0xFEFF |
                    0xFFFC |
                    0xFFF9..=0xFFFB
                ))
            }
        })
        .collect();

    // Collapse whitespace
    let collapsed: String = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if collapsed.is_empty() {
        return None;
    }

    if collapsed.len() > MAX_TITLE_LENGTH {
        return None; // Caller should handle this case
    }

    Some(collapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_title_clean() {
        assert_eq!(sanitize_title("hello world"), Some("hello world".to_string()));
    }

    #[test]
    fn test_sanitize_title_control_chars() {
        let result = sanitize_title("hello\x00world");
        assert_eq!(result, Some("helloworld".to_string()));
    }

    #[test]
    fn test_sanitize_title_empty() {
        assert_eq!(sanitize_title(""), None);
        assert_eq!(sanitize_title("   "), None);
    }

    #[test]
    fn test_sanitize_title_whitespace_collapse() {
        assert_eq!(sanitize_title("hello   world"), Some("hello world".to_string()));
    }

    #[test]
    fn test_sanitize_title_zero_width() {
        let result = sanitize_title("hello\u{200b}world");
        assert_eq!(result, Some("helloworld".to_string()));
    }

    #[test]
    fn test_max_title_length() {
        let long = "a".repeat(MAX_TITLE_LENGTH + 1);
        assert_eq!(sanitize_title(&long), None);
    }
}
