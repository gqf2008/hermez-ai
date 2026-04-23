//! # Hermez Prompt
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! System prompt construction, context compression, and Anthropic prompt caching.
//! Mirrors the Python `agent/prompt_builder.py`, `agent/context_compressor.py`,
//! and `agent/prompt_caching.py`.

pub(crate) mod builder;
pub(crate) mod cache_control;
pub(crate) mod context_compressor;
pub(crate) mod context_engine;
pub(crate) mod context_references;
pub(crate) mod injection_scan;
pub(crate) mod skills_prompt;
pub(crate) mod manual_compression_feedback;
pub(crate) mod soul;
pub(crate) mod subdirectory_hints;

// Re-export main public types for convenience.
pub use builder::{
    build_system_prompt, build_context_files_prompt, should_use_developer_role,
    PromptBuilderConfig, PromptBuilderResult, ToolUseEnforcement,
    GOOGLE_MODEL_OPERATIONAL_GUIDANCE, MEMORY_GUIDANCE, OPENAI_MODEL_EXECUTION_GUIDANCE,
    SESSION_SEARCH_GUIDANCE, SKILLS_GUIDANCE, TOOL_USE_ENFORCEMENT_GUIDANCE,
    TOOL_USE_ENFORCEMENT_MODELS, DEFAULT_AGENT_IDENTITY,
};
pub use cache_control::{apply_anthropic_cache_control, CacheTtl};
pub use context_compressor::{CompressorConfig, ContextCompressor};
pub use context_engine::{ContextEngine, create_engine, available_engines};
pub use injection_scan::{sanitize_context_content, scan_context_content};
pub use skills_prompt::build_skills_system_prompt;
pub use soul::{load_soul_md, has_soul_md, CONTEXT_FILE_MAX_CHARS};
