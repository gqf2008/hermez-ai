//! # Hermez Tools
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Tool registry and all ~60 tool implementations.
//! Mirrors the Python `tools/` directory and `model_tools.py`.

pub mod registry;
pub mod tool_result;
pub mod toolsets_def;

// Simple tools
pub mod budget_config;
pub(crate) mod interrupt;
pub(crate) mod url_safety;
pub(crate) mod website_policy;
pub(crate) mod ansi_strip;
pub(crate) mod binary_extensions;
pub(crate) mod debug_helpers;
pub(crate) mod fuzzy_match;
pub(crate) mod patch_parser;
pub(crate) mod osv_check;
pub(crate) mod clipboard;
pub(crate) mod credential_files;
pub mod tool_result_storage;
pub(crate) mod openrouter_client;
pub(crate) mod transcription;

// Complex tools (stub modules — implementations added progressively)
pub mod approval;
pub(crate) mod file_ops;
pub(crate) mod terminal;
pub mod process_reg;
pub(crate) mod web;
#[cfg(feature = "browser")]
pub(crate) mod browser;
pub(crate) mod code_exec;
pub(crate) mod delegate;
#[cfg(feature = "mcp")]
pub(crate) mod mcp_client;
#[cfg(feature = "mcp")]
pub mod mcp_serve;
pub(crate) mod memory;
pub(crate) mod todo;
pub mod skills;
pub(crate) mod skills_hub;
pub(crate) mod skills_sync;
pub(crate) mod tts;
pub(crate) mod voice;
pub(crate) mod vision;
#[cfg(feature = "image")]
pub(crate) mod image_gen;
pub(crate) mod clarify;
pub(crate) mod session_search;
pub(crate) mod homeassistant;
pub(crate) mod send_message;
pub mod checkpoint;
pub(crate) mod shell_file_ops;
pub(crate) mod credentials;
pub(crate) mod rl_training;
pub(crate) mod skills_guard;
pub(crate) mod tirith;
pub(crate) mod cron_tools;
pub(crate) mod moa;

// Backend helpers
pub(crate) mod env_passthrough;
pub mod managed_tool_gateway;
pub(crate) mod mcp_oauth;
pub(crate) mod neutts_synth;
pub(crate) mod path_security;
pub mod tool_backend_helpers;

// Environment backends
pub(crate) mod environments;
pub(crate) mod feishu;

use std::sync::Arc;

/// Register all tools in the given registry.
///
/// This is the single entry point called at startup. Each tool module
/// exposes a `register_*` function that adds its tools to the registry.
pub fn register_all_tools(registry: &mut crate::registry::ToolRegistry) {
    // Core tools
    todo::register(registry);
    clarify::register(registry);
    fuzzy_match::register(registry);
    memory::register_memory_tool(registry);
    approval::register_approval_tool(registry);
    web::register_web_tools(registry);
    vision::register_vision_tool(registry);
    homeassistant::register_ha_tools(registry);
    skills::register_skills_tools(registry);
    skills_hub::register(registry);
    file_ops::register_file_tools(registry);
    #[cfg(feature = "image")]
    image_gen::register_image_tool(registry);
    cron_tools::register_cron_tools(registry);
    session_search::register_session_search_tool(registry);
    send_message::register_send_message_tool(registry);
    feishu::register_feishu_tools(registry);
    tts::register_tts_tool(registry);
    voice::register_voice_tool(registry);
    process_reg::register_process_tool(registry);
    terminal::register_terminal_tool(registry);
    delegate::register_delegate_tool(registry);
    #[cfg(feature = "mcp")]
    mcp_client::register_mcp_client_tool(registry);
    rl_training::register_rl_tools(registry);
    #[cfg(feature = "browser")]
    browser::register_browser_tools(registry);
    // code_exec registered last — it needs a snapshot of the full registry
    // so the Python sandbox can RPC-dispatch to any registered tool.
    let registry_arc = Arc::new(registry.clone());
    code_exec::register_code_exec_tool(registry, registry_arc);
    moa::register_moa_tool(registry);
    // Load permanent allowlist from disk so it survives restarts
    approval::load_permanent_allowlist();
}
