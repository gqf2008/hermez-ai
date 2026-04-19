#![allow(dead_code)]
//! Tool result persistence — preserves large outputs instead of truncating.
//!
//! Defense against context-window overflow operates at three levels:
//!
//! 1. **Per-tool output cap** (inside each tool): Tools pre-truncate their own
//!    output before returning.
//! 2. **Per-result persistence** (`maybe_persist_tool_result`): After a tool
//!    returns, if its output exceeds the registered threshold, the full output
//!    is written into the sandbox temp dir. The in-context content is replaced
//!    with a preview + file path reference.
//! 3. **Per-turn aggregate budget** (`enforce_turn_budget`): After all tool
//!    results in a single turn are collected, if the total exceeds
//!    `MAX_TURN_BUDGET_CHARS` (200K), the largest non-persisted results are
//!    spilled to disk until the aggregate is under budget.
//!
//! Mirrors the Python `tools/tool_result_storage.py`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::budget_config::BudgetConfig;

/// Tag marking persisted output blocks in assistant context.
pub const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
/// Closing tag for persisted output blocks.
pub const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";

/// Default storage directory for persisted tool results.
pub const STORAGE_DIR: &str = "/tmp/hermes-results";

/// Budget enforcement tool name (sentinel, not a real tool).
const BUDGET_TOOL_NAME: &str = "__budget_enforcement__";

/// Resolved storage directory (lazy, can be overridden at runtime).
static STORAGE_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var("HERMES_TOOL_RESULTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(STORAGE_DIR))
});

/// A single tool result that may need persistence.
#[derive(Debug, Clone)]
pub struct ToolResultEntry {
    /// Original tool result content.
    pub content: String,
    /// Tool name for threshold lookup.
    pub tool_name: String,
    /// Unique ID used as filename for persistence.
    pub tool_use_id: String,
    /// Whether this entry is already persisted (skip during budget enforcement).
    pub is_persisted: bool,
}

/// Result of preview generation.
#[derive(Debug, Clone)]
pub struct PreviewResult {
    /// Truncated preview text.
    pub preview: String,
    /// Whether there is more content beyond the preview.
    pub has_more: bool,
}

/// Generate a preview by truncating at the last newline within `max_chars`.
pub fn generate_preview(content: &str, max_chars: usize) -> PreviewResult {
    if content.len() <= max_chars {
        return PreviewResult {
            preview: content.to_string(),
            has_more: false,
        };
    }
    let truncated = &content[..max_chars];
    let last_nl = truncated.rfind('\n');
    let preview = if last_nl.is_some_and(|pos| pos > max_chars / 2) {
        truncated[..last_nl.unwrap() + 1].to_string()
    } else {
        truncated.to_string()
    };
    PreviewResult {
        preview,
        has_more: true,
    }
}

/// Format a number with comma separators (e.g., 500000 → "500,000").
fn format_number(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let len = s.len();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Build the `<persisted-output>` replacement message.
fn build_persisted_message(
    preview: &str,
    has_more: bool,
    original_size: usize,
    file_path: &str,
) -> String {
    let size_kb = original_size as f64 / 1024.0;
    let size_str = if size_kb >= 1024.0 {
        format!("{:.1} MB", size_kb / 1024.0)
    } else {
        format!("{:.1} KB", size_kb)
    };
    let size_formatted = format_number(original_size);

    let mut msg = format!(
        "{PERSISTED_OUTPUT_TAG}\n\
         This tool result was too large ({size_formatted} characters, {size_str}).\n\
         Full output saved to: {file_path}\n\
         Use the read_file tool with offset and limit to access specific sections of this output.\n\
         \n\
         Preview (first {} chars):\n\
         {preview}",
        preview.len()
    );
    if has_more {
        msg.push_str("\n...");
    }
    msg.push_str(&format!("\n{PERSISTED_OUTPUT_CLOSING_TAG}"));
    msg
}

/// Write content to a file on the local filesystem (for local backend).
///
/// For remote backends (Docker, SSH, etc.), the caller should use
/// `env.execute()` to write the file instead.
fn write_local_persist(content: &str, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

/// Persist a tool result to disk if it exceeds the threshold.
///
/// For local execution, writes directly to the filesystem.
/// For remote backends, the caller must handle sandbox writes separately.
///
/// Returns `Some(replacement_message)` if persisted, `None` if content was
/// within threshold.
pub fn maybe_persist_tool_result(
    content: &str,
    tool_name: &str,
    tool_use_id: &str,
    threshold: Option<usize>,
    config: &BudgetConfig,
) -> Option<String> {
    let effective_threshold = threshold.unwrap_or_else(|| config.resolve_threshold(tool_name, None));

    if content.len() <= effective_threshold {
        return None;
    }

    let preview = generate_preview(content, config.preview_size);
    let storage_dir = STORAGE_PATH.as_path();
    let file_path = storage_dir.join(format!("{tool_use_id}.txt"));

    match write_local_persist(content, &file_path) {
        Ok(()) => {
            tracing::info!(
                "Persisted large tool result: {} ({}, {} chars -> {})",
                tool_name,
                tool_use_id,
                content.len(),
                file_path.display()
            );
            Some(build_persisted_message(
                &preview.preview,
                preview.has_more,
                content.len(),
                &file_path.to_string_lossy(),
            ))
        }
        Err(e) => {
            tracing::warn!(
                "Sandbox write failed for {}: {}",
                tool_use_id,
                e
            );
            // Fallback: inline truncation
            Some(format!(
                "{}\n\n\
                 [Truncated: tool response was {} chars. \
                 Full output could not be saved to sandbox.]",
                preview.preview,
                content.len()
            ))
        }
    }
}

/// Enforce aggregate budget across all tool results in a turn.
///
/// If total chars exceed `config.turn_budget`, persist the largest
/// non-persisted results first until under budget.
///
/// Returns the (potentially modified) list of entries.
pub fn enforce_turn_budget(
    entries: &mut [ToolResultEntry],
    config: &BudgetConfig,
) {
    let mut total_size: usize = entries.iter().map(|e| e.content.len()).sum();

    if total_size <= config.turn_budget {
        return;
    }

    // Collect candidates: non-persisted entries, sorted by size descending
    let mut candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_persisted)
        .map(|(i, _)| i)
        .collect();
    candidates.sort_by_key(|&i| entries[i].content.len());
    candidates.reverse();

    for &idx in &candidates {
        if total_size <= config.turn_budget {
            break;
        }
        let entry = &entries[idx];
        let original_size = entry.content.len();

        // Persist this entry
        let replacement = maybe_persist_tool_result(
            &entry.content,
            BUDGET_TOOL_NAME,
            &entry.tool_use_id,
            Some(0), // Force persist (threshold=0)
            config,
        );

        if let Some(new_content) = replacement {
            total_size = total_size.saturating_sub(original_size).saturating_add(new_content.len());
            entries[idx].content = new_content;
            entries[idx].is_persisted = true;
            tracing::info!(
                "Budget enforcement: persisted tool result {} ({} chars)",
                entries[idx].tool_use_id,
                original_size
            );
        }
    }
}

/// Get the storage directory path for tool result persistence.
pub fn get_storage_dir() -> &'static Path {
    STORAGE_PATH.as_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(5), "5");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(500_000), "500,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn test_generate_preview_under_limit() {
        let result = generate_preview("hello world", 100);
        assert_eq!(result.preview, "hello world");
        assert!(!result.has_more);
    }

    #[test]
    fn test_generate_preview_truncates_at_newline() {
        let content = "first line\nsecond line\nthird line";
        let result = generate_preview(content, 20);
        assert!(result.has_more);
        // Should truncate at the newline after "first line"
        assert!(result.preview.contains("first line"));
        assert!(result.preview.len() <= 20);
    }

    #[test]
    fn test_generate_preview_no_newline_fallback() {
        let content = "a".repeat(100);
        let result = generate_preview(&content, 50);
        assert!(result.has_more);
        assert_eq!(result.preview.len(), 50);
    }

    #[test]
    fn test_build_persisted_message_format() {
        let msg = build_persisted_message("preview", true, 500_000, "/tmp/test.txt");
        assert!(msg.contains("<persisted-output>"));
        assert!(msg.contains("</persisted-output>"));
        assert!(msg.contains("500,000 characters"));
        assert!(msg.contains("/tmp/test.txt"));
        assert!(msg.contains("preview"));
        assert!(msg.contains("..."));
    }

    #[test]
    fn test_build_persisted_message_no_more() {
        let msg = build_persisted_message("preview", false, 1000, "/tmp/test.txt");
        assert!(!msg.contains("..."));
    }

    #[test]
    fn test_build_persisted_message_mb_size() {
        let msg = build_persisted_message("preview", true, 2_000_000, "/tmp/test.txt");
        assert!(msg.contains("MB"));
    }

    #[test]
    fn test_maybe_persist_tool_result_under_threshold() {
        let config = BudgetConfig::default();
        let result = maybe_persist_tool_result("short content", "bash", "id-1", None, &config);
        assert!(result.is_none());
    }

    #[test]
    fn test_maybe_persist_tool_result_over_threshold() {
        let config = BudgetConfig::default();
        let long_content = "x".repeat(200_000);
        let result =
            maybe_persist_tool_result(&long_content, "bash", "test-id-1", None, &config);
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.contains("<persisted-output>"));
        assert!(msg.contains("200,000 characters"));
    }

    #[test]
    fn test_maybe_persist_with_explicit_threshold() {
        let config = BudgetConfig::default();
        let result =
            maybe_persist_tool_result("abc", "bash", "id-2", Some(2), &config);
        assert!(result.is_some());
    }

    #[test]
    fn test_enforce_turn_budget_under_limit() {
        let config = BudgetConfig::default();
        let mut entries = vec![
            ToolResultEntry {
                content: "short".to_string(),
                tool_name: "tool1".to_string(),
                tool_use_id: "id-1".to_string(),
                is_persisted: false,
            },
        ];
        enforce_turn_budget(&mut entries, &config);
        assert_eq!(entries[0].content, "short");
    }

    #[test]
    fn test_enforce_turn_budget_persists_largest() {
        // Verify that the largest entry is persisted first.
        // With replacement messages being ~280 chars, any persist + small entries
        // will likely stay over budget, so we just check ordering behavior.
        let mut config = BudgetConfig::default();
        config.turn_budget = 100; // Well under any realistic total

        let mut entries = vec![
            ToolResultEntry {
                content: "a".repeat(50),
                tool_name: "tool1".to_string(),
                tool_use_id: "id-1".to_string(),
                is_persisted: false,
            },
            ToolResultEntry {
                content: "x".repeat(500),
                tool_name: "tool2".to_string(),
                tool_use_id: "large-id".to_string(),
                is_persisted: false,
            },
        ];

        enforce_turn_budget(&mut entries, &config);

        // Both entries should be persisted (total > budget even after each persist)
        assert!(entries[0].is_persisted || entries[1].is_persisted);
        // At minimum, the 500-char entry should have been processed
        assert!(entries[1].is_persisted);
    }

    #[test]
    fn test_enforce_turn_budget_skips_already_persisted() {
        let mut config = BudgetConfig::default();
        config.turn_budget = 50;

        let mut entries = vec![
            ToolResultEntry {
                content: "<persisted-output>\nalready persisted\n</persisted-output>".to_string(),
                tool_name: "tool1".to_string(),
                tool_use_id: "id-1".to_string(),
                is_persisted: true,
            },
            ToolResultEntry {
                content: "x".repeat(100),
                tool_name: "tool2".to_string(),
                tool_use_id: "id-2".to_string(),
                is_persisted: false,
            },
        ];

        enforce_turn_budget(&mut entries, &config);

        // Already-persisted entry should not be touched
        assert!(entries[0].content.contains("already persisted"));
    }

    #[test]
    fn test_get_storage_dir() {
        let dir = get_storage_dir();
        assert!(dir.to_string_lossy().contains("hermes-results")
            || std::env::var("HERMES_TOOL_RESULTS_DIR").is_ok());
    }

    #[test]
    fn test_preview_rounds_at_newline() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = generate_preview(content, 15);
        assert!(result.has_more);
        // 15 chars = "line1\nline2\nlin", should round to "line1\nline2\n"
        assert_eq!(result.preview, "line1\nline2\n");
    }

    #[test]
    fn test_persisted_message_uses_format_specifier() {
        let content = "x".repeat(150_000);
        let msg = maybe_persist_tool_result(&content, "test", "fmt-id", None, &BudgetConfig::default())
            .unwrap();
        // Should use comma-separated format (e.g., "150,000" not "150000")
        assert!(msg.contains("150,000"));
    }
}
