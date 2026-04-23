#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//! # Hermez RL
//!
//! Reinforcement learning environments for training hermez-agent models.
//!
//! Provides:
//! - Base environment trait and shared types (`Environment`, `AgentResult`, `ToolError`)
//! - Agent loop simulation for rollout generation
//! - Concrete environments:
//!   - `MathEnv` — math problem solving with exact-match rewards
//!   - `ToolUseEnv` — tool calling tasks with correctness rewards
//!   - `AtroposEnv` — code generation with test-verification rewards
//!   - `WebResearchEnv` — multi-step web research with LLM-judge rewards
//!
//! Inspired by the Python Atropos integration in `environments/`.

pub mod base;
pub mod math_env;
pub mod tool_use_env;
pub mod atropos_env;
pub mod web_research_env;
pub mod swe_env;

pub use base::{
    AgentResult, AgentLoopConfig, Environment, EnvironmentConfig,
    RewardSignal, ScoredTrajectory, ToolError, ToolCall,
};
