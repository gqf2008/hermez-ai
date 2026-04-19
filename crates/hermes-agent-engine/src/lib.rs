//! # Hermes Agent Engine
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! Core agent conversation loop (AIAgent class).
//! Mirrors the Python `run_agent.py`.

pub mod agent;
pub(crate) mod budget;
pub(crate) mod failover;
pub(crate) mod memory_manager;
pub(crate) mod memory_provider;
pub(crate) mod message_loop;
pub(crate) mod review_agent;
pub(crate) mod self_evolution;
pub(crate) mod skill_commands;
pub(crate) mod skill_utils;
pub(crate) mod smart_model_routing;
pub(crate) mod subagent;
pub(crate) mod title_generator;
pub(crate) mod trajectory;
pub(crate) mod usage_pricing;

pub use agent::AIAgent;
pub use agent::types::{
    ActivityCallback, AgentConfig, FallbackProvider, InterimAssistantCallback,
    PreLlmHook, PreLlmHookResult, PrimaryRuntime, ReasoningCallback,
    StatusCallback, StreamCallback, ToolGenCallback, TurnResult, TurnUsage,
};
pub use memory_manager::{build_memory_context_block, sanitize_context as sanitize_memory_context, MemoryManager};
pub use memory_provider::MemoryProvider;
pub use message_loop::{MessageLoop, MessageResult, PlatformMessage};
pub use smart_model_routing::{
    choose_cheap_model_route, parse_routing_config, resolve_turn_route, RoutingConfig, TurnRoute,
};
pub use title_generator::{generate_title, maybe_auto_title, SessionTitleStore};
pub use trajectory::{
    has_incomplete_scratchpad, messages_to_conversation, save_trajectory,
    ConversationTurn, TrajectoryEntry,
};
pub use skill_commands::{
    build_plan_path, build_skill_invocation_message, get_skill_commands, load_skill_payload,
    resolve_skill_command_key, scan_skill_commands, SkillCommand, SkillPayload,
};
