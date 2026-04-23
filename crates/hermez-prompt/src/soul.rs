#![allow(dead_code)]
//! SOUL.md loader — agent identity.
//!
//! Mirrors the Python `load_soul_md()` in `agent/prompt_builder.py`.
//! Loads SOUL.md from HERMEZ_HOME as the primary agent identity.

use crate::injection_scan::sanitize_context_content;

/// Default agent identity when SOUL.md is not present.
pub const DEFAULT_AGENT_IDENTITY: &str =
    "You are Hermez Agent, an intelligent AI assistant created by Nous Research. \
    You are helpful, knowledgeable, and direct. You assist users with a wide \
    range of tasks including answering questions, writing and editing code, \
    analyzing information, creative work, and executing actions via your tools. \
    You communicate clearly, admit uncertainty when appropriate, and prioritize \
    being genuinely useful over being verbose unless otherwise directed below. \
    Be targeted and efficient in your exploration and investigations.";

/// Maximum characters for context files.
pub const CONTEXT_FILE_MAX_CHARS: usize = 20_000;

/// Truncation ratios for head/tail split.
const TRUNCATE_HEAD_RATIO: f64 = 0.7;
const TRUNCATE_TAIL_RATIO: f64 = 0.2;

/// Truncate content with head/tail split and marker in the middle.
fn truncate_content(content: &str, filename: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    let head_chars = ((max_chars as f64 * TRUNCATE_HEAD_RATIO) as usize).min(content.len());
    let tail_chars = ((max_chars as f64 * TRUNCATE_TAIL_RATIO) as usize).min(content.len());

    // Ensure we don't overlap
    let actual_head = head_chars.min(content.len() - tail_chars);
    let head = &content[..actual_head];
    let tail_start = content.len() - tail_chars;
    let tail = &content[tail_start..];

    let marker = format!(
        "\n\n[...truncated {}: kept {}+{} of {} chars. Use file tools to read the full file.]\n\n",
        filename, actual_head, tail_chars, content.len()
    );

    format!("{}{}{}", head, marker, tail)
}

/// Load SOUL.md from HERMEZ_HOME.
///
/// Returns `Some(content)` if the file exists and is non-empty,
/// `None` otherwise. Content is scanned for injection and truncated.
pub fn load_soul_md() -> Option<String> {
    let hermez_home = hermez_core::get_hermez_home();
    let soul_path = hermez_home.join("SOUL.md");

    if !soul_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&soul_path).ok()?;
    let content = content.trim().to_string();
    if content.is_empty() {
        return None;
    }

    // Scan for injection
    let sanitized = sanitize_context_content(&content, "SOUL.md");
    // Truncate
    let truncated = truncate_content(&sanitized, "SOUL.md", CONTEXT_FILE_MAX_CHARS);

    Some(truncated)
}

/// Check if SOUL.md exists (without reading it).
pub fn has_soul_md() -> bool {
    hermez_core::get_hermez_home().join("SOUL.md").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short_content() {
        let content = "short";
        assert_eq!(truncate_content(content, "test.md", 100), "short");
    }

    #[test]
    fn test_truncate_long_content() {
        let content = "a".repeat(30_000);
        let result = truncate_content(&content, "test.md", CONTEXT_FILE_MAX_CHARS);
        assert!(result.len() < 30_000);
        assert!(result.contains("truncated"));
        assert!(result.starts_with("aaaaaaaa"));
        assert!(result.ends_with("aaaaaaa"));
    }

    #[test]
    fn test_default_identity_not_empty() {
        assert!(!DEFAULT_AGENT_IDENTITY.is_empty());
    }

    #[test]
    fn test_load_soul_no_file() {
        // In test env, SOUL.md shouldn't exist
        let result = load_soul_md();
        // May or may not exist depending on test setup
        if result.is_some() {
            assert!(!result.as_ref().unwrap().is_empty());
        }
    }
}
