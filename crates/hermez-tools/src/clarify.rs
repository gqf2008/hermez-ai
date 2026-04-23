#![allow(dead_code)]
//! Clarify tool — structured multiple-choice or open-ended questions.
//!
//! Presents questions to the user. The actual UI interaction is delegated
//! to a platform-provided callback (CLI arrow-key navigation or messaging
//! platform numbered list).
//!
//! Mirrors the Python `tools/clarify_tool.py`.

use serde_json::Value;

use crate::registry::{tool_error, tool_result, ToolRegistry};
use hermez_core::Result;

/// Maximum number of choices.
const MAX_CHOICES: usize = 4;

/// Clarify callback type.
///
/// Platform implementations must provide this to handle user input.
/// Returns the user's response as a string.
pub type ClarifyCallback = dyn Fn(&str, Option<&[String]>) -> Result<String> + Send + Sync;

/// Global clarify callback — set by the CLI or gateway at startup.
static CLARIFY_CALLBACK: parking_lot::Mutex<Option<std::sync::Arc<ClarifyCallback>>> =
    parking_lot::Mutex::new(None);

/// Set the platform-provided clarify callback.
pub fn set_clarify_callback(callback: impl Fn(&str, Option<&[String]>) -> Result<String> + Send + Sync + 'static) {
    *CLARIFY_CALLBACK.lock() = Some(std::sync::Arc::new(callback));
}

/// Clear the clarify callback (for testing).
pub fn clear_clarify_callback() {
    *CLARIFY_CALLBACK.lock() = None;
}

/// Clarify tool JSON schema.
pub fn clarify_schema() -> Value {
    serde_json::json!({
        "name": "clarify",
        "description": "Ask the user a clarifying question. Returns the user's response.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user."
                },
                "choices": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Up to 4 choices for the user to select from. If omitted, open-ended.",
                    "maxItems": 4
                }
            },
            "required": ["question"]
        }
    })
}

/// Handle the clarify tool call.
pub fn handle_clarify(args: Value) -> Result<String> {
    let question = args["question"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let Some(question) = question else {
        return Ok(tool_error("'question' is required and must be non-empty"));
    };

    // Parse choices (up to 4, trimmed)
    let choices: Option<Vec<String>> = args.get("choices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .take(MAX_CHOICES)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());

    // Convert to slice for callback
    let choices_vec: Vec<String> = choices.clone().unwrap_or_default();
    let choices_slice: Option<&[String]> = if choices_vec.is_empty() {
        None
    } else {
        Some(&choices_vec)
    };

    // Call the platform-provided callback
    let callback = CLARIFY_CALLBACK.lock();
    let Some(callback) = callback.as_ref() else {
        return Ok(tool_error("clarify callback not set — no user interface available"));
    };

    match callback(question, choices_slice) {
        Ok(response) => {
            tool_result(serde_json::json!({
                "question": question,
                "choices_offered": choices,
                "user_response": response,
            }))
        }
        Err(e) => Ok(tool_error(format!("clarify failed: {e}"))),
    }
}

/// Register the clarify tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(
        "clarify".to_string(),
        "organization".to_string(),
        clarify_schema(),
        std::sync::Arc::new(handle_clarify),
        None,
        vec![],
        "Ask the user a clarifying question".to_string(),
        "❓".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_no_callback_error() {
        clear_clarify_callback();
        let result = handle_clarify(serde_json::json!({
            "question": "What is your name?"
        })).unwrap();
        assert!(result.contains("error"));
        assert!(result.contains("callback not set"));
    }

    #[test]
    #[serial]
    fn test_missing_question() {
        let result = handle_clarify(serde_json::json!({})).unwrap();
        assert!(result.contains("error"));
        assert!(result.contains("question"));
    }

    #[test]
    #[serial]
    fn test_empty_question() {
        let result = handle_clarify(serde_json::json!({
            "question": "   "
        })).unwrap();
        assert!(result.contains("error"));
    }

    #[test]
    #[serial]
    fn test_with_callback() {
        clear_clarify_callback();
        set_clarify_callback(|_q, _choices| Ok("user chose A".to_string()));

        let result = handle_clarify(serde_json::json!({
            "question": "Choose one:",
            "choices": ["A", "B", "C"]
        })).unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["user_response"], "user chose A");
        assert_eq!(parsed["choices_offered"].as_array().unwrap().len(), 3);

        clear_clarify_callback();
    }

    #[test]
    #[serial]
    fn test_max_choices_trimmed() {
        clear_clarify_callback();
        let called_choices = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let captured_ref = std::sync::Arc::clone(&called_choices);
        set_clarify_callback(move |_q, choices| {
            *captured_ref.lock() = choices.map(|c| c.to_vec());
            Ok("picked".to_string())
        });

        handle_clarify(serde_json::json!({
            "question": "Pick:",
            "choices": ["A", "B", "C", "D", "E", "F"]
        })).unwrap();

        let captured = called_choices.lock().take().unwrap();
        assert_eq!(captured.len(), 4); // Max 4 choices
    }
}
