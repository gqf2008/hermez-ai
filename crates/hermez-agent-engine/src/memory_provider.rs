//! Memory provider trait and lifecycle.
//!
//! Abstract base for pluggable memory providers. Memory providers give the
//! agent persistent recall across sessions. One external provider is active
//! at a time alongside the always-on built-in memory.
//!
//! Mirrors the Python `agent/memory_provider.py`.

use async_trait::async_trait;
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::agent::types::Message;

/// Configuration field definition for setup wizards.
#[derive(Debug, Clone)]
pub struct ConfigField {
    /// Config key name (e.g. "api_key", "mode").
    pub key: String,
    /// Human-readable description.
    pub description: String,
    /// True if this should go to .env.
    pub secret: bool,
    /// True if required.
    pub required: bool,
    /// Default value.
    pub default: Option<String>,
    /// Valid choices.
    pub choices: Option<Vec<String>>,
    /// URL where user can get this credential.
    pub url: Option<String>,
    /// Explicit env var name for secrets.
    pub env_var: Option<String>,
}

/// Abstract trait for memory providers.
///
/// Lifecycle (called by MemoryManager, wired in run_agent):
/// - `is_available()` — check if configured and ready
/// - `initialize()` — connect, create resources, warm up
/// - `system_prompt_block()` — static text for the system prompt
/// - `prefetch()` — background recall before each turn
/// - `sync_turn()` — async write after each turn
/// - `get_tool_schemas()` — tool schemas to expose to the model
/// - `handle_tool_call()` — dispatch a tool call
/// - `shutdown()` — clean exit
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Short identifier for this provider (e.g. "builtin", "honcho", "hindsight").
    fn name(&self) -> &str;

    /// Return true if this provider is configured, has credentials, and is ready.
    ///
    /// Called during agent init to decide whether to activate the provider.
    /// Should not make network calls — just check config and installed deps.
    fn is_available(&self) -> bool;

    /// Initialize for a session.
    ///
    /// Called once at agent startup. May create resources, establish connections,
    /// start background tasks, etc.
    ///
    /// `kwargs` always includes:
    /// - `hermez_home`: The active HERMEZ_HOME directory path.
    /// - `platform`: "cli", "telegram", "discord", "cron", etc.
    ///
    /// `kwargs` may also include:
    /// - `agent_context`: "primary", "subagent", "cron", or "flush".
    /// - `agent_identity`: Profile name (e.g. "coder").
    /// - `agent_workspace`: Shared workspace name.
    /// - `parent_session_id`: For subagents, the parent's session_id.
    /// - `user_id`: Platform user identifier.
    fn initialize(&self, session_id: &str, kwargs: &HashMap<String, Value>);

    /// Return text to include in the system prompt.
    ///
    /// Called during system prompt assembly. Return empty string to skip.
    fn system_prompt_block(&self) -> String {
        String::new()
    }

    /// Recall relevant context for the upcoming turn.
    ///
    /// Called before each API call. Return formatted text to inject as
    /// context, or empty string if nothing relevant.
    fn prefetch(&self, _query: &str, _session_id: &str) -> String {
        String::new()
    }

    /// Queue a background recall for the NEXT turn.
    ///
    /// Called after each turn completes. Default is no-op.
    fn queue_prefetch(&self, _query: &str, _session_id: &str) {}

    /// Persist a completed turn to the backend.
    ///
    /// Called after each turn. Should be non-blocking.
    fn sync_turn(&self, _user_content: &str, _assistant_content: &str, _session_id: &str) {}

    /// Return tool schemas this provider exposes.
    ///
    /// Each schema follows the OpenAI function calling format.
    /// Return empty vec if this provider has no tools.
    fn get_tool_schemas(&self) -> Vec<Value>;

    /// Handle a tool call for one of this provider's tools.
    ///
    /// Must return a JSON string (the tool result).
    fn handle_tool_call(
        &self,
        tool_name: &str,
        _args: &Map<String, Value>,
        _kwargs: &HashMap<String, Value>,
    ) -> String {
        serde_json::json!({
            "error": format!("Provider {} does not handle tool {}", self.name(), tool_name)
        })
        .to_string()
    }

    /// Clean shutdown — flush queues, close connections.
    fn shutdown(&self) {}

    // -- Optional hooks (override to opt in) --

    /// Called at the start of each turn with the user message.
    fn on_turn_start(
        &self,
        _turn_number: u64,
        _message: &str,
        _kwargs: &HashMap<String, Value>,
    ) {
    }

    /// Called when a session ends (explicit exit or timeout).
    fn on_session_end(&self, _messages: &[Message]) {}

    /// Called before context compression discards old messages.
    fn on_pre_compress(&self, _messages: &[Message]) -> String {
        String::new()
    }

    /// Called on the PARENT agent when a subagent completes.
    fn on_delegation(
        &self,
        _task: &str,
        _result: &str,
        _child_session_id: &str,
        _kwargs: &HashMap<String, Value>,
    ) {
    }

    /// Return config fields this provider needs for setup.
    fn get_config_schema(&self) -> Vec<ConfigField> {
        Vec::new()
    }

    /// Write non-secret config to the provider's native location.
    fn save_config(&self, _values: &HashMap<String, Value>, _hermez_home: &str) {}

    /// Called when the built-in memory tool writes an entry.
    fn on_memory_write(&self, _action: &str, _target: &str, _content: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    struct MockProvider;

    #[async_trait]
    impl MemoryProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn is_available(&self) -> bool {
            true
        }

        fn initialize(&self, _session_id: &str, _kwargs: &HashMap<String, Value>) {}

        fn get_tool_schemas(&self) -> Vec<Value> {
            Vec::new()
        }
    }

    #[test]
    fn test_config_field_creation() {
        let field = ConfigField {
            key: "api_key".to_string(),
            description: "API key".to_string(),
            secret: true,
            required: true,
            default: None,
            choices: Some(vec!["a".to_string(), "b".to_string()]),
            url: Some("https://example.com".to_string()),
            env_var: Some("API_KEY".to_string()),
        };
        assert_eq!(field.key, "api_key");
        assert!(field.secret);
        assert!(field.required);
        assert_eq!(field.choices.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_provider_name() {
        let p = MockProvider;
        assert_eq!(p.name(), "mock");
    }

    #[test]
    fn test_provider_is_available() {
        let p = MockProvider;
        assert!(p.is_available());
    }

    #[test]
    fn test_default_system_prompt_block_empty() {
        let p = MockProvider;
        assert!(p.system_prompt_block().is_empty());
    }

    #[test]
    fn test_default_prefetch_empty() {
        let p = MockProvider;
        assert!(p.prefetch("query", "sid").is_empty());
    }

    #[test]
    fn test_default_queue_prefetch_no_panic() {
        let p = MockProvider;
        p.queue_prefetch("q", "sid");
    }

    #[test]
    fn test_default_sync_turn_no_panic() {
        let p = MockProvider;
        p.sync_turn("user", "assistant", "sid");
    }

    #[test]
    fn test_default_tool_schemas_empty() {
        let p = MockProvider;
        assert!(p.get_tool_schemas().is_empty());
    }

    #[test]
    fn test_default_handle_tool_call_error() {
        let p = MockProvider;
        let result = p.handle_tool_call("unknown", &Map::new(), &HashMap::new());
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("error").unwrap().as_str().unwrap().contains("mock"));
    }

    #[test]
    fn test_default_shutdown_no_panic() {
        let p = MockProvider;
        p.shutdown();
    }

    #[test]
    fn test_default_on_turn_start_no_panic() {
        let p = MockProvider;
        p.on_turn_start(1, "msg", &HashMap::new());
    }

    #[test]
    fn test_default_on_session_end_no_panic() {
        let p = MockProvider;
        p.on_session_end(&[]);
    }

    #[test]
    fn test_default_on_pre_compress_empty() {
        let p = MockProvider;
        assert!(p.on_pre_compress(&[]).is_empty());
    }

    #[test]
    fn test_default_on_delegation_no_panic() {
        let p = MockProvider;
        p.on_delegation("task", "result", "child", &HashMap::new());
    }

    #[test]
    fn test_default_get_config_schema_empty() {
        let p = MockProvider;
        assert!(p.get_config_schema().is_empty());
    }

    #[test]
    fn test_default_save_config_no_panic() {
        let p = MockProvider;
        p.save_config(&HashMap::new(), "/tmp");
    }

    #[test]
    fn test_default_on_memory_write_no_panic() {
        let p = MockProvider;
        p.on_memory_write("add", "key", "value");
    }
}
