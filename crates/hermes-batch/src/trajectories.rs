//! Trajectory processing — format, combine, filter batch outputs.
//!
//! Mirrors the Python trajectory functions in `batch_runner.py` and `trajectory.py`.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use hermes_core::{HermesError, Result};

/// A single trajectory entry (one agent run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryEntry {
    /// Conversations (message history).
    pub conversations: Vec<TrajectoryMessage>,
    /// Timestamp when this was recorded.
    pub timestamp: String,
    /// Model used.
    pub model: String,
    /// Whether the agent completed successfully.
    pub completed: bool,
    /// Optional tool usage statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_stats: Option<serde_json::Value>,
    /// Optional reasoning statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_stats: Option<serde_json::Value>,
    /// Metadata (prompt index, batch number, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// A single message in a trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryMessage {
    /// Role: "human", "gpt", "tool", "system".
    pub from: String,
    /// Message content.
    pub value: String,
}

/// Extracted tool usage statistics from a conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    /// Total tool calls made.
    pub total_calls: usize,
    /// Successful tool calls.
    pub success: usize,
    /// Failed tool calls.
    pub errors: usize,
    /// Per-tool breakdown: tool_name -> {calls, success, errors}.
    pub per_tool: std::collections::HashMap<String, serde_json::Value>,
}

/// Extracted reasoning statistics from a conversation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningStats {
    /// Number of assistant turns with reasoning content.
    pub with_reasoning: usize,
    /// Number of assistant turns without reasoning content.
    pub without_reasoning: usize,
    /// Total assistant turns.
    pub total_turns: usize,
}

/// Extract tool stats from conversation messages.
pub fn extract_tool_stats(messages: &[serde_json::Value]) -> ToolStats {
    let mut stats = ToolStats::default();

    for msg in messages {
        // Check for tool calls in assistant messages
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let tool_name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                stats.total_calls += 1;

                let entry = stats
                    .per_tool
                    .entry(tool_name.to_string())
                    .or_insert_with(|| serde_json::json!({"calls": 0, "success": 0, "errors": 0}));

                if let Some(obj) = entry.as_object_mut() {
                    let calls = obj.get("calls").and_then(|v| v.as_u64()).unwrap_or(0) as usize + 1;
                    obj["calls"] = serde_json::json!(calls);
                }
            }
        }

        // Check for tool results (success/error based on content)
        if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
            if role == "tool" {
                if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                    let is_error = content.starts_with("Error:");
                    if is_error {
                        stats.errors += 1;
                    } else {
                        stats.success += 1;
                    }
                }
            }
        }
    }

    stats
}

/// Extract reasoning stats from conversation messages.
pub fn extract_reasoning_stats(messages: &[serde_json::Value]) -> ReasoningStats {
    let mut stats = ReasoningStats::default();

    for msg in messages {
        if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
            if role == "assistant" {
                stats.total_turns += 1;
                let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");

                let has_reasoning = content.contains("<REASONING_SCRATCHPAD>")
                    || content.contains("<think>")
                    || content.contains("<think>")
                    || msg.get("reasoning").is_some();

                if has_reasoning {
                    stats.with_reasoning += 1;
                } else {
                    stats.without_reasoning += 1;
                }
            }
        }
    }

    stats
}

/// Combine all batch JSONL files into a single trajectories file.
///
/// Reads all `batch_*.jsonl` files in the output directory and writes
/// them to a combined `trajectories.jsonl`.
pub fn combine_batch_files(output_dir: &Path) -> Result<usize> {
    let mut total_entries = 0;
    let output_path = output_dir.join("trajectories.jsonl");

    let mut writer = std::io::BufWriter::new(
        std::fs::File::create(&output_path).map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to create trajectories file: {e}"),
            )
        })?,
    );

    // Find all batch_*.jsonl files
    let mut batch_files: Vec<_> = std::fs::read_dir(output_dir)
        .map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to read output directory: {e}"),
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with("batch_") && name.ends_with(".jsonl") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort by batch number for consistent ordering
    batch_files.sort();

    for batch_file in &batch_files {
        let file = std::fs::File::open(batch_file).map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to open batch file {:?}: {e}", batch_file),
            )
        })?;

        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader) {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            // Validate JSON and filter out invalid tool names
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(convos) = entry.get("conversations").and_then(|v| v.as_array()) {
                    // Filter out entries with obviously invalid tool names
                    let has_valid_tool = convos.iter().any(|c| {
                        if let Some(tool) = c.get("from").and_then(|v| v.as_str()) {
                            tool == "gpt" || tool == "human" || tool == "system" || tool == "tool"
                        } else {
                            true
                        }
                    });
                    if !has_valid_tool {
                        continue;
                    }
                }

                let line_out = serde_json::to_string(&entry)?;
                writeln!(writer, "{line_out}").map_err(|e| {
                    HermesError::new(
                        hermes_core::errors::ErrorCategory::InternalError,
                        format!("Failed to write trajectory: {e}"),
                    )
                })?;
                total_entries += 1;
            }
        }
    }

    writer.flush().map_err(|e| {
        HermesError::new(
            hermes_core::errors::ErrorCategory::InternalError,
            format!("Failed to flush trajectories: {e}"),
        )
    })?;

    Ok(total_entries)
}

/// Convert `<REASONING_SCRATCHPAD>` tags to `<think>` tags in content.
pub fn convert_scratchpad_to_think(content: &str) -> String {
    content
        .replace("<REASONING_SCRATCHPAD>", "<think>")
        .replace("</REASONING_SCRATCHPAD>", "</think>")
}

/// Check if content has an incomplete (unclosed) scratchpad tag.
pub fn has_incomplete_scratchpad(content: &str) -> bool {
    let has_open = content.contains("<REASONING_SCRATCHPAD>");
    let has_close = content.contains("</REASONING_SCRATCHPAD>");
    has_open && !has_close
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tool_stats() {
        let messages = vec![
            serde_json::json!({
                "role": "assistant",
                "content": "Let me check.",
                "tool_calls": [{
                    "function": {"name": "read_file"}
                }]
            }),
            serde_json::json!({
                "role": "tool",
                "content": "File contents here"
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "Another call.",
                "tool_calls": [{
                    "function": {"name": "write_file"}
                }]
            }),
            serde_json::json!({
                "role": "tool",
                "content": "Error: permission denied"
            }),
        ];

        let stats = extract_tool_stats(&messages);
        assert_eq!(stats.total_calls, 2);
        assert_eq!(stats.success, 1);
        assert_eq!(stats.errors, 1);
        assert!(stats.per_tool.contains_key("read_file"));
        assert!(stats.per_tool.contains_key("write_file"));
    }

    #[test]
    fn test_extract_reasoning_stats() {
        let messages = vec![
            serde_json::json!({
                "role": "assistant",
                "content": "<think>Let me reason about this.</think>I think the answer is 42."
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "Simple response without reasoning."
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "<REASONING_SCRATCHPAD>Analysis here.</REASONING_SCRATCHPAD>Done."
            }),
        ];

        let stats = extract_reasoning_stats(&messages);
        assert_eq!(stats.total_turns, 3);
        assert_eq!(stats.with_reasoning, 2);
        assert_eq!(stats.without_reasoning, 1);
    }

    #[test]
    fn test_convert_scratchpad() {
        let input = "<REASONING_SCRATCHPAD>thinking</REASONING_SCRATCHPAD>";
        let output = convert_scratchpad_to_think(input);
        assert_eq!(output, "<think>thinking</think>");
    }

    #[test]
    fn test_incomplete_scratchpad() {
        assert!(has_incomplete_scratchpad("<REASONING_SCRATCHPAD>thinking"));
        assert!(!has_incomplete_scratchpad("<REASONING_SCRATCHPAD>done</REASONING_SCRATCHPAD>"));
        assert!(!has_incomplete_scratchpad("no tags here"));
    }

    #[test]
    fn test_combine_batch_files() {
        let dir = std::env::temp_dir().join("test_batch_combine");
        let _ = std::fs::create_dir_all(&dir);

        // Write two batch files
        std::fs::write(
            dir.join("batch_0.jsonl"),
            r#"{"conversations":[{"from":"human","value":"hi"}],"timestamp":"2026-01-01T00:00:00","model":"test","completed":true}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("batch_1.jsonl"),
            r#"{"conversations":[{"from":"human","value":"hello"}],"timestamp":"2026-01-01T00:00:01","model":"test","completed":true}"#,
        )
        .unwrap();

        let count = combine_batch_files(&dir).unwrap();
        assert_eq!(count, 2);

        let output = std::fs::read_to_string(dir.join("trajectories.jsonl")).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_tool_stats_empty() {
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "hello!"}),
        ];
        let stats = extract_tool_stats(&messages);
        assert_eq!(stats.total_calls, 0);
        assert_eq!(stats.success, 0);
        assert_eq!(stats.errors, 0);
        assert!(stats.per_tool.is_empty());
    }

    #[test]
    fn test_extract_reasoning_stats_empty() {
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        let stats = extract_reasoning_stats(&messages);
        assert_eq!(stats.total_turns, 0);
        assert_eq!(stats.with_reasoning, 0);
        assert_eq!(stats.without_reasoning, 0);
    }

    #[test]
    fn test_extract_reasoning_stats_with_think_tags() {
        // Both XML and HTML-entity variants
        let messages = vec![
            serde_json::json!({"role": "assistant", "content": "<think>thinking</think>answer"}),
            serde_json::json!({"role": "assistant", "content": "<think>analysis</think>done"}),
            serde_json::json!({"role": "assistant", "content": "just answer"}),
        ];
        let stats = extract_reasoning_stats(&messages);
        assert_eq!(stats.total_turns, 3);
        assert_eq!(stats.with_reasoning, 2);
        assert_eq!(stats.without_reasoning, 1);
    }

    #[test]
    fn test_extract_tool_stats_multiple_same_tool() {
        let messages = vec![
            serde_json::json!({"role": "assistant", "tool_calls": [{"function": {"name": "read_file"}}]}),
            serde_json::json!({"role": "tool", "content": "ok"}),
            serde_json::json!({"role": "assistant", "tool_calls": [{"function": {"name": "read_file"}}]}),
            serde_json::json!({"role": "tool", "content": "ok again"}),
            serde_json::json!({"role": "assistant", "tool_calls": [{"function": {"name": "write_file"}}]}),
            serde_json::json!({"role": "tool", "content": "Error: fail"}),
        ];
        let stats = extract_tool_stats(&messages);
        assert_eq!(stats.total_calls, 3);
        assert_eq!(stats.success, 2);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.per_tool["read_file"]["calls"], 2);
        assert_eq!(stats.per_tool["write_file"]["calls"], 1);
    }

    #[test]
    fn test_combine_batch_files_empty_dir() {
        let dir = std::env::temp_dir().join("test_batch_empty");
        let _ = std::fs::create_dir_all(&dir);

        let count = combine_batch_files(&dir).unwrap();
        assert_eq!(count, 0);

        let output = std::fs::read_to_string(dir.join("trajectories.jsonl")).unwrap();
        assert!(output.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_combine_batch_files_with_non_batch_files() {
        let dir = std::env::temp_dir().join("test_batch_mixed");
        let _ = std::fs::create_dir_all(&dir);

        // Write a batch file and a non-batch file
        std::fs::write(
            dir.join("batch_0.jsonl"),
            r#"{"conversations":[{"from":"human","value":"hi"}],"timestamp":"2026-01-01","model":"test","completed":true}"#,
        ).unwrap();
        std::fs::write(
            dir.join("checkpoint.json"),
            r#"{}"#,
        ).unwrap();

        let count = combine_batch_files(&dir).unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_trajectory_entry_serialization() {
        let entry = TrajectoryEntry {
            conversations: vec![
                TrajectoryMessage { from: "human".to_string(), value: "hi".to_string() },
                TrajectoryMessage { from: "gpt".to_string(), value: "hello".to_string() },
            ],
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            model: "test-model".to_string(),
            completed: true,
            tool_stats: None,
            reasoning_stats: None,
            metadata: Some(serde_json::json!({"batch": 0})),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let loaded: TrajectoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.conversations.len(), 2);
        assert_eq!(loaded.model, "test-model");
        assert!(loaded.completed);
    }

    #[test]
    fn test_combine_batch_files_sorts_by_name() {
        let dir = std::env::temp_dir().join("test_batch_sort");
        let _ = std::fs::create_dir_all(&dir);

        // Write files out of order
        std::fs::write(
            dir.join("batch_2.jsonl"),
            r#"{"conversations":[{"from":"human","value":"third"}],"timestamp":"t3","model":"test","completed":true}"#,
        ).unwrap();
        std::fs::write(
            dir.join("batch_0.jsonl"),
            r#"{"conversations":[{"from":"human","value":"first"}],"timestamp":"t1","model":"test","completed":true}"#,
        ).unwrap();
        std::fs::write(
            dir.join("batch_1.jsonl"),
            r#"{"conversations":[{"from":"human","value":"second"}],"timestamp":"t2","model":"test","completed":true}"#,
        ).unwrap();

        let output_path = dir.join("trajectories.jsonl");
        let _ = combine_batch_files(&dir).unwrap();
        let output = std::fs::read_to_string(&output_path).unwrap();
        let lines: Vec<_> = output.lines().collect();

        // Should be sorted by filename (batch_0, batch_1, batch_2)
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));
        assert!(lines[2].contains("third"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
