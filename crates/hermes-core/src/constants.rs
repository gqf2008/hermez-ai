//! Constants and defaults shared across the Hermes system.
//!
//! Mirrors the constant values scattered across the Python codebase.
#![allow(dead_code)]

/// Default agent identity / system prompt text.
pub const DEFAULT_AGENT_IDENTITY: &str =
    "You are Hermes Agent, created by Nous Research. You are a helpful \
     assistant that uses tools to complete tasks.";

/// Default maximum iterations for the agent loop.
pub const DEFAULT_MAX_ITERATIONS: usize = 90;

/// Default maximum iterations for delegated sub-agents.
pub const DEFAULT_DELEGATION_MAX_ITERATIONS: usize = 50;

/// Maximum delegation depth (parent → child → grandchild).
pub const MAX_DELEGATION_DEPTH: usize = 2;

/// Maximum concurrent sub-agent executions.
pub const MAX_CONCURRENT_CHILDREN: usize = 3;

/// Maximum concurrent tool executions.
pub const MAX_CONCURRENT_TOOL_CALLS: usize = 8;

/// Default token threshold for context compression (as fraction of context).
pub const COMPRESSION_THRESHOLD_PCT: f64 = 0.5;

/// Default target ratio for compression summaries.
pub const COMPRESSION_TARGET_RATIO: f64 = 0.2;

/// Default number of first messages to protect during compression.
pub const COMPRESSION_PROTECT_FIRST_N: usize = 3;

/// Minimum tail messages to protect during compression.
pub const COMPRESSION_MIN_TAIL_MESSAGES: usize = 3;

/// Maximum characters for context file injection (head/tail truncation).
pub const MAX_CONTEXT_FILE_CHARS: usize = 20_000;

/// Head ratio for context file truncation (70% head, 20% tail).
pub const CONTEXT_FILE_HEAD_RATIO: f64 = 0.7;

/// Tail ratio for context file truncation.
pub const CONTEXT_FILE_TAIL_RATIO: f64 = 0.2;

/// Default tool result size cap in characters.
pub const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 100_000;

/// Session key prefix.
pub const SESSION_KEY_PREFIX: &str = "agent:main";

/// Session database file name.
pub const SESSION_DB_FILE: &str = "sessions.db";

/// Session JSONL directory name.
pub const SESSION_JSONL_DIR: &str = "sessions";

/// Log file name.
pub const LOG_FILE: &str = "agent.log";

/// Error log file name.
pub const ERROR_LOG_FILE: &str = "errors.log";

/// Skills directory name.
pub const SKILLS_DIR_NAME: &str = "skills";

/// Memory directory name.
pub const MEMORY_DIR_NAME: &str = "memories";

/// Cron directory name.
pub const CRON_DIR_NAME: &str = "cron";

/// Log directory name.
pub const LOG_DIR_NAME: &str = "logs";

/// Default SQLite WAL checkpoint interval (number of writes).
pub const WAL_CHECKPOINT_INTERVAL: u64 = 50;

/// Write contention retry max attempts.
pub const WRITE_RETRY_MAX_ATTEMPTS: u32 = 15;

/// Write contention retry min jitter in milliseconds.
pub const WRITE_RETRY_MIN_JITTER_MS: u64 = 20;

/// Write contention retry max jitter in milliseconds.
pub const WRITE_RETRY_MAX_JITTER_MS: u64 = 150;

/// Supported platforms for skill targeting.
pub const SUPPORTED_PLATFORMS: &[&str] = &[
    "cli", "telegram", "discord", "slack", "whatsapp", "signal",
    "homeassistant", "matrix", "email", "sms", "dingtalk", "feishu",
    "wecom", "weixin", "mattermost", "bluebubbles",
];

/// Models that require "developer" role instead of "system" role.
pub const DEVELOPER_ROLE_MODELS: &[&str] = &["gpt-5", "codex"];

/// Models that need tool-use enforcement guidance.
pub const TOOL_ENFORCEMENT_MODEL_FAMILIES: &[&str] =
    &["gpt", "codex", "gemini", "gemma", "grok"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_agent_identity_non_empty() {
        assert!(!DEFAULT_AGENT_IDENTITY.is_empty());
        assert!(DEFAULT_AGENT_IDENTITY.contains("Hermes Agent"));
    }

    #[test]
    fn test_iteration_limits_sane() {
        assert!(DEFAULT_MAX_ITERATIONS > 0);
        assert!(DEFAULT_DELEGATION_MAX_ITERATIONS > 0);
        assert!(DEFAULT_DELEGATION_MAX_ITERATIONS <= DEFAULT_MAX_ITERATIONS);
    }

    #[test]
    fn test_delegation_depth_and_concurrency() {
        assert_eq!(MAX_DELEGATION_DEPTH, 2);
        assert!(MAX_CONCURRENT_CHILDREN > 0);
        assert!(MAX_CONCURRENT_TOOL_CALLS >= MAX_CONCURRENT_CHILDREN);
    }

    #[test]
    fn test_compression_constants() {
        assert!(COMPRESSION_THRESHOLD_PCT > 0.0 && COMPRESSION_THRESHOLD_PCT <= 1.0);
        assert!(COMPRESSION_TARGET_RATIO > 0.0 && COMPRESSION_TARGET_RATIO <= 1.0);
        assert!(COMPRESSION_PROTECT_FIRST_N >= 1);
        assert!(COMPRESSION_MIN_TAIL_MESSAGES >= 1);
    }

    #[test]
    fn test_context_file_ratios_sum() {
        assert!(
            (CONTEXT_FILE_HEAD_RATIO + CONTEXT_FILE_TAIL_RATIO) <= 1.0,
            "head + tail ratios should not exceed 1.0"
        );
    }

    #[test]
    fn test_supported_platforms_nonempty() {
        assert!(!SUPPORTED_PLATFORMS.is_empty());
        assert!(SUPPORTED_PLATFORMS.contains(&"cli"));
        assert!(SUPPORTED_PLATFORMS.contains(&"telegram"));
    }

    #[test]
    fn test_session_key_prefix() {
        assert_eq!(SESSION_KEY_PREFIX, "agent:main");
        assert_eq!(SESSION_DB_FILE, "sessions.db");
    }

    #[test]
    fn test_developer_role_models() {
        assert!(DEVELOPER_ROLE_MODELS.contains(&"gpt-5"));
        assert!(DEVELOPER_ROLE_MODELS.contains(&"codex"));
    }

    #[test]
    fn test_tool_enforcement_families() {
        assert!(TOOL_ENFORCEMENT_MODEL_FAMILIES.contains(&"gpt"));
        assert!(TOOL_ENFORCEMENT_MODEL_FAMILIES.contains(&"gemini"));
    }
}
