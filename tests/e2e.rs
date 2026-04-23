//! End-to-end test suite for Hermez AI.
//!
//! Goal: if all tests in this module pass, the core program logic is correct.
//!
//! Each submodule covers one functional area.  Tests use mock HTTP servers
//! (mockito) so they run without real API keys.

#[path = "e2e/agent_conversation_cycle.rs"]
pub mod agent_conversation_cycle;
#[path = "e2e/llm_message_fidelity.rs"]
pub mod llm_message_fidelity;
#[path = "e2e/session_persistence.rs"]
pub mod session_persistence;
#[path = "e2e/tool_system.rs"]
pub mod tool_system;

use std::sync::Arc;

use hermez_agent_engine::agent::{AgentConfig, AIAgent};
use hermez_tools::registry::ToolRegistry;

/// Build an `AgentConfig` pointing at a mockito server.
pub fn mock_agent_config(base_url: &str) -> AgentConfig {
    let mut config = AgentConfig::default();
    config.model = "openai/gpt-4o-mini".to_string();
    config.base_url = Some(format!("{base_url}/v1"));
    config.api_key = Some("test-key".to_string());
    config.api_mode = Some("openai".to_string());
    config.max_iterations = 5;
    config.enable_caching = false;
    config.skip_context_files = true;
    config.platform = Some("test".to_string());
    config.persist_session = false; // E2E tests use in-memory history
    config
}

/// Build an `AIAgent` with an empty tool registry.
pub fn mock_agent(base_url: &str) -> AIAgent {
    let config = mock_agent_config(base_url);
    let registry = Arc::new(ToolRegistry::new());
    AIAgent::new(config, registry).unwrap()
}

/// Build an `AIAgent` with the full tool registry.
pub fn mock_agent_with_tools(base_url: &str) -> AIAgent {
    let config = mock_agent_config(base_url);
    let mut registry = ToolRegistry::new();
    hermez_tools::register_all_tools(&mut registry);
    let registry = Arc::new(registry);
    AIAgent::new(config, registry).unwrap()
}
