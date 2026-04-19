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
