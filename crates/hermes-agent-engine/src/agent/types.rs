//! Type definitions for AIAgent.
//!
//! Callback types, configuration, and data structs extracted from agent.rs.

use std::sync::Arc;

/// Conversation message wrapped in Arc to avoid deep clones.
pub type Message = Arc<serde_json::Value>;

use hermes_prompt::CompressorConfig;
use hermes_llm::credential_pool::CredentialPool;

/// Activity callback to prevent gateway inactivity timeout.
///
/// Called before each tool execution to signal activity.
#[allow(dead_code)]
pub type ActivityCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Plugin hook for pre-LLM call interception.
///
/// Mirrors Python plugin hooks (run_agent.py plugin system).
/// Plugins can inspect/modify messages, system prompt, or abort the call.
#[allow(dead_code)]
pub type PreLlmHook = Arc<dyn Fn(
    &str,             // system prompt
    &[Message],       // messages
    usize,            // api_call_count
) -> PreLlmHookResult + Send + Sync>;

/// Result of a pre-LLM hook.
#[derive(Debug, Clone)]
pub enum PreLlmHookResult {
    /// Proceed with the LLM call as normal.
    Continue,
    /// Abort the conversation with the given message.
    Abort(String),
    /// Replace the system prompt with a new value.
    OverrideSystem(String),
    /// Replace the entire message list with a new one.
    OverrideMessages(Vec<Message>),
    /// Replace both system prompt and messages.
    OverrideBoth(String, Vec<Message>),
}

/// Stream delta callback — fires on each text chunk from the LLM.
///
/// Mirrors Python `_fire_stream_delta()` (run_agent.py:5143).
/// Used by TTS pipeline to start audio generation before the full response.
#[allow(dead_code)]
pub type StreamCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Status callback for gateway platform notifications.
///
/// Mirrors Python `status_callback` (run_agent.py:5194+).
/// Signature: (event_type, message). Used for context pressure,
/// compression warnings, and other user-facing status updates.
#[allow(dead_code)]
pub type StatusCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Reasoning delta callback — fires on each reasoning/thinking chunk.
///
/// Mirrors Python `_fire_reasoning_delta()` (run_agent.py:5163).
/// Used to display model reasoning in real time.
pub type ReasoningCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Tool generation started callback — fires when the model begins
/// generating tool call arguments during streaming.
///
/// Mirrors Python `_fire_tool_gen_started()` (run_agent.py:5172).
/// Gives the UI a chance to show a spinner so the user isn't staring
/// at a frozen screen while a large tool payload is being generated.
pub type ToolGenCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Interim assistant message callback — fires mid-turn to surface
/// real assistant commentary to the UI layer.
///
/// Mirrors Python `_emit_interim_assistant_message()` (run_agent.py:5128).
/// Signature: (visible_text, already_streamed).
pub type InterimAssistantCallback = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// Snapshot of the primary runtime for fallback restoration.
///
/// Mirrors Python `_primary_runtime` dict (run_agent.py:6008).
/// Stored when a turn starts, restored at the next turn if fallback
/// was activated — making fallback turn-scoped instead of session-scoped.
#[derive(Debug, Clone)]
pub struct PrimaryRuntime {
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

/// Configuration for the AIAgent.
#[derive(Clone)]
pub struct AgentConfig {
    /// Model name (e.g., "anthropic/claude-opus-4-6").
    pub model: String,
    /// Provider override.
    pub provider: Option<String>,
    /// Base URL for API endpoint.
    pub base_url: Option<String>,
    /// API key.
    pub api_key: Option<String>,
    /// API mode: "openai", "anthropic", "codex".
    pub api_mode: Option<String>,
    /// Maximum tool-calling iterations per turn.
    pub max_iterations: usize,
    /// Whether to skip context files.
    pub skip_context_files: bool,
    /// Platform key (e.g., "cli", "telegram").
    pub platform: Option<String>,
    /// Session ID.
    pub session_id: Option<String>,
    /// Whether to apply Anthropic prompt caching.
    pub enable_caching: bool,
    /// Whether context compression is enabled.
    pub compression_enabled: bool,
    /// Compression configuration.
    pub compression_config: Option<CompressorConfig>,
    /// Context engine name (e.g. "compressor", "lcm").
    /// Default is "compressor" when compression_enabled is true.
    pub context_engine_name: Option<String>,
    /// Working directory for context file discovery.
    pub terminal_cwd: Option<std::path::PathBuf>,
    /// Ephemeral system message (not saved to session DB).
    pub ephemeral_system_prompt: Option<String>,
    /// Nudge interval for memory review (default 10 turns).
    pub memory_nudge_interval: usize,
    /// Nudge interval for skill review (default 10 iterations).
    pub skill_nudge_interval: usize,
    /// Minimum turns between memory flushes (default 6).
    /// Reserved for future memory flush logic.
    #[allow(dead_code)]
    pub memory_flush_min_turns: usize,
    /// Whether background self-review is enabled (default true).
    pub self_evolution_enabled: bool,
    /// Credential pool for provider key rotation.
    pub credential_pool: Option<Arc<CredentialPool>>,
    /// Provider preferences for OpenRouter (only/ignore/order/sort).
    pub provider_preferences: Option<hermes_llm::client::ProviderPreferences>,
    /// Fallback providers for failover.
    pub fallback_providers: Vec<FallbackProvider>,
    /// Session database for persistence.
    pub session_db: Option<Arc<hermes_state::SessionDB>>,
    /// Whether to persist sessions to disk.
    pub persist_session: bool,
}

/// Fallback provider configuration.
#[derive(Debug, Clone)]
pub struct FallbackProvider {
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "anthropic/claude-opus-4-6".to_string(),
            provider: None,
            base_url: None,
            api_key: None,
            api_mode: None,
            max_iterations: 90,
            skip_context_files: false,
            platform: None,
            session_id: None,
            enable_caching: true,
            compression_enabled: false,
            compression_config: None,
            context_engine_name: None,
            terminal_cwd: None,
            ephemeral_system_prompt: None,
            memory_nudge_interval: 10,
            skill_nudge_interval: 10,
            memory_flush_min_turns: 6,
            self_evolution_enabled: true,
            credential_pool: None,
            provider_preferences: None,
            fallback_providers: Vec::new(),
            session_db: None,
            persist_session: true,
        }
    }
}

/// Result of a conversation turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// Final assistant response text.
    pub response: String,
    /// Complete message history after the turn.
    pub messages: Vec<Message>,
    /// Number of API calls made.
    pub api_calls: usize,
    /// Exit reason.
    pub exit_reason: String,
    /// Compression exhaustion flag — set when max compression attempts
    /// were reached without resolving the context overflow. The caller
    /// (e.g., gateway) should auto-reset the session to break the loop.
    pub compression_exhausted: bool,
    /// Token usage from the last LLM call (if available).
    pub usage: Option<TurnUsage>,
}

/// Token usage from a turn.
#[derive(Debug, Clone)]
pub struct TurnUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Handler for interactive approval requests from tools (e.g., terminal).
///
/// When a tool returns `status: "approval_required"`, the agent loop
/// pauses and calls this handler to wait for user confirmation.
#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// Request approval for a command. Blocks until the user responds.
    ///
    /// Returns `"approve"`, `"approve_session"`, `"approve_always"`, or `"deny"`.
    async fn request_approval(&self, command: &str, description: &str) -> Result<String, String>;
}
