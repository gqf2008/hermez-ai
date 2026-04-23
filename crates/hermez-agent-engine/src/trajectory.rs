//! Trajectory saving utilities.
//!
//! Converts internal message formats to ShareGPT-style trajectory entries
//! and appends them to JSONL files for offline analysis and training.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::types::Message;
use hermez_core::Result;

/// A single turn in ShareGPT conversation format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub from: String, // "human" | "gpt" | "tool" | "system"
    pub value: String,
}

/// A complete trajectory entry for JSONL output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryEntry {
    pub conversations: Vec<ConversationTurn>,
    pub timestamp: String,
    pub model: String,
    pub completed: bool,
}

/// Convert `<REASONING_SCRATCHPAD>` tags to `<|thinking|>` style tags.
pub fn convert_scratchpad_to_think(content: &str) -> String {
    if !content.contains("<REASONING_SCRATCHPAD>") {
        return content.to_string();
    }
    content
        .replace("<REASONING_SCRATCHPAD>", "<|thinking|>")
        .replace("</REASONING_SCRATCHPAD>", "<|/thinking|>")
}

/// Check if content has an opening scratchpad tag without a closing tag.
pub fn has_incomplete_scratchpad(content: &str) -> bool {
    content.contains("<REASONING_SCRATCHPAD>") && !content.contains("</REASONING_SCRATCHPAD>")
}

/// Convert a list of OpenAI-format messages to ShareGPT conversation turns.
pub fn messages_to_conversation(messages: &[Message]) -> Vec<ConversationTurn> {
    messages
        .iter()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(Value::as_str)?;
            let content = msg
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");

            let from = match role {
                "user" => "human",
                "assistant" => "gpt",
                "tool" => "tool",
                "system" => "system",
                _ => return None,
            };

            Some(ConversationTurn {
                from: from.to_string(),
                value: convert_scratchpad_to_think(content),
            })
        })
        .collect()
}

/// Save a trajectory entry to a JSONL file.
///
/// If `filename` is None, defaults to `trajectory_samples.jsonl` for
/// completed conversations or `failed_trajectories.jsonl` for failed ones.
pub fn save_trajectory(
    conversations: Vec<ConversationTurn>,
    model: &str,
    completed: bool,
    filename: Option<&Path>,
) -> Result<PathBuf> {
    let default_name = if completed {
        "trajectory_samples.jsonl"
    } else {
        "failed_trajectories.jsonl"
    };
    let path = filename.unwrap_or_else(|| Path::new(default_name));

    let entry = TrajectoryEntry {
        conversations,
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: model.to_string(),
        completed,
    };

    let line = serde_json::to_string(&entry)?;

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(format!("{line}\n").as_bytes())?;

    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_convert_scratchpad() {
        let input = "Thinking<REASONING_SCRATCHPAD>step 1</REASONING_SCRATCHPAD>done";
        let output = convert_scratchpad_to_think(input);
        assert!(output.contains("<|thinking|>"));
        assert!(output.contains("<|/thinking|>"));
    }

    #[test]
    fn test_no_scratchpad_passthrough() {
        let input = "Hello world";
        assert_eq!(convert_scratchpad_to_think(input), input);
    }

    #[test]
    fn test_incomplete_scratchpad() {
        assert!(has_incomplete_scratchpad("<REASONING_SCRATCHPAD>step 1"));
        assert!(!has_incomplete_scratchpad(
            "<REASONING_SCRATCHPAD>step 1</REASONING_SCRATCHPAD>"
        ));
        assert!(!has_incomplete_scratchpad("no tags"));
    }

    #[test]
    fn test_messages_to_conversation() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "system", "content": "You are helpful"})),
            Arc::new(serde_json::json!({"role": "user", "content": "Hi"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "Hello!"})),
        ];
        let turns = messages_to_conversation(&messages);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].from, "system");
        assert_eq!(turns[1].from, "human");
        assert_eq!(turns[2].from, "gpt");
    }

    #[test]
    fn test_save_trajectory() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_trajectory.jsonl");
        let _ = std::fs::remove_file(&path);

        let conversations = vec![ConversationTurn {
            from: "human".to_string(),
            value: "test".to_string(),
        }];

        let result = save_trajectory(conversations, "test-model", true, Some(&path));
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&path).unwrap();
        let entry: TrajectoryEntry = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(entry.model, "test-model");
        assert!(entry.completed);
        assert_eq!(entry.conversations.len(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
