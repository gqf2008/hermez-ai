//! AIAgent — core conversation loop with tool calling.
//!
//! Mirrors the Python `AIAgent` class in `run_agent.py`.
//! Manages:
//! - System prompt assembly (via hermes_prompt)
//! - Main tool-calling loop
//! - Context compression integration
//! - Sub-agent delegation
//! - Session persistence
//!
//! Split into sub-modules:
//! - `types` — callback types, AgentConfig, FallbackProvider, TurnResult, TurnUsage
//! - `constants` — NEVER/PARALLEL_SAFE/PATH_SCOPED tool sets, dispatch_delegation
//! - `utils` — message sanitization, normalization, token estimation, backoff
//! - `session` — shutdown, persist_session, flush_messages, save_trajectory
//! - `control` — chat, interrupt, switch_model, reset_session_state, activity

// Sub-modules (each has its own `impl AIAgent` block)
pub mod types;
pub(crate) mod constants;
pub(crate) mod utils;
pub mod session;
pub mod control;

// Re-export public types from sub-modules
pub use types::{
    ActivityCallback, AgentConfig, FallbackProvider, InterimAssistantCallback,
    PreLlmHook, PreLlmHookResult, PrimaryRuntime, ReasoningCallback,
    StatusCallback, StreamCallback, ToolGenCallback, TurnResult, TurnUsage,
};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use parking_lot::Mutex;

use serde_json::Value;

use crate::agent::types::Message;

use hermes_core::{HermesConfig, Result};
use hermes_prompt::{
    apply_anthropic_cache_control, build_system_prompt, CompressorConfig,
    PromptBuilderConfig, ToolUseEnforcement, CacheTtl,
};
use hermes_llm::reasoning::extract_reasoning;
use hermes_tools::registry::ToolRegistry;
use crate::plugin_system::global_hooks;

use crate::budget::IterationBudget;
use crate::failover::{self, FailoverAction, FailoverState};
use crate::memory_manager::{sanitize_context as sanitize_memory_context, MemoryManager};
use crate::subagent::{SubagentManager, SubagentResult};

// Re-export from sub-modules for use within this module
use constants::*;
use utils::*;

/// AI Agent with tool calling capabilities.
pub struct AIAgent {
    config: AgentConfig,
    tool_registry: Arc<ToolRegistry>,
    /// Cached system prompt (rebuilt only after compression).
    cached_system_prompt: Option<String>,
    /// Context engine (if enabled).
    context_engine: Option<Box<dyn hermes_prompt::ContextEngine>>,
    /// Memory manager for built-in + external memory providers.
    memory_manager: MemoryManager,
    /// Failover state for error recovery chain.
    failover_state: FailoverState,
    /// Shared iteration budget.
    pub budget: Arc<IterationBudget>,
    /// Subagent manager for delegation.
    subagent_mgr: Option<Arc<SubagentManager>>,
    /// Shared interrupt flag for child agents.
    interrupt: Arc<AtomicBool>,
    /// Message accompanying the interrupt request (from Python: `_interrupt_message`).
    #[allow(dead_code)]
    interrupt_message: Mutex<Option<String>>,
    /// Delegation depth (0 = top-level agent).
    #[allow(dead_code)]
    delegate_depth: u32,
    /// Pending subagent results to inject as tool messages.
    delegate_results: Mutex<Vec<SubagentResult>>,
    /// Token usage from the last LLM call (for API response propagation).
    last_usage: Mutex<Option<TurnUsage>>,
    /// Activity callback to prevent gateway inactivity timeout.
    activity_callback: Option<ActivityCallback>,
    /// Turns since last memory tool use (starts at 0).
    turns_since_memory: usize,
    /// Iterations since last skill_manage tool use (starts at 0).
    iters_since_skill: usize,
    /// Provider signaled "stream not supported" — switch to non-streaming
    /// for the rest of this session instead of re-failing every retry.
    /// Mirrors Python: `_disable_streaming` in `run_agent.py`.
    disable_streaming: bool,
    /// Force ASCII-only payload for API calls (set when ASCII codec error detected).
    #[allow(dead_code)]
    force_ascii_payload: bool,
    /// Stream callback for TTS/display delta notifications.
    stream_callback: Option<StreamCallback>,
    /// Status callback for gateway platform notifications.
    #[allow(dead_code)]
    status_callback: Option<StatusCallback>,
    /// Reasoning/thinking delta callback for streaming.
    reasoning_callback: Option<ReasoningCallback>,
    /// Tool generation started callback.
    #[allow(dead_code)]
    tool_gen_callback: Option<ToolGenCallback>,
    /// Interim assistant message callback (mid-turn commentary).
    #[allow(dead_code)]
    interim_assistant_callback: Option<InterimAssistantCallback>,
    /// Accumulated assistant text emitted through stream callbacks.
    /// Mirrors Python `_current_streamed_assistant_text`.
    current_streamed_assistant_text: Mutex<String>,
    /// Whether a paragraph break is needed before the next stream delta.
    /// Mirrors Python `_stream_needs_break`.
    stream_needs_break: Mutex<bool>,
    /// Whether a fallback provider was activated this turn.
    /// Restored to primary at the start of the next turn.
    fallback_activated: bool,
    /// Snapshot of the primary runtime for fallback restoration.
    primary_runtime: Option<PrimaryRuntime>,
    /// Rate limit state captured from provider response headers (x-ratelimit-*).
    /// Mirrors Python: `_rate_limit_state` in `run_agent.py`.
    #[allow(dead_code)]
    rate_limit_state: Mutex<Option<Value>>,
    /// Last activity timestamp (epoch seconds) for gateway diagnostics.
    /// Mirrors Python: `_last_activity_ts` in `run_agent.py`.
    #[allow(dead_code)]
    last_activity_ts: Mutex<f64>,
    /// Last activity description for gateway diagnostics.
    /// Mirrors Python: `_last_activity_desc` in `run_agent.py`.
    #[allow(dead_code)]
    last_activity_desc: Mutex<String>,
    /// Current tool being executed (for activity summary).
    /// Mirrors Python: `_current_tool` in `run_agent.py`.
    #[allow(dead_code)]
    current_tool: Mutex<Option<String>>,
    /// Pre-LLM hook for plugin interception.
    /// Mirrors Python plugin system.
    #[allow(dead_code)]
    pre_llm_hook: Option<PreLlmHook>,
    /// Turn number for this session (incremented each conversation).
    turn_number: u64,
    /// Session database for persistence.
    session_db: Option<Arc<hermes_state::SessionDB>>,
    /// Whether to persist sessions to disk (default true).
    persist_session: bool,
    /// Index of last flushed message to session DB (prevents duplicate writes).
    last_flushed_db_idx: usize,
    /// WASM plugins loaded at startup (Phase 1).
    wasm_plugins: Vec<Arc<crate::plugin_system::WasmPluginRuntime>>,
}

impl AIAgent {
    /// Create a new agent.
    pub fn new(config: AgentConfig, tool_registry: Arc<ToolRegistry>) -> Result<Self> {
        Self::with_depth(config, tool_registry, 0)
    }

    /// Create a new agent at a specific delegation depth.
    pub fn with_depth(config: AgentConfig, tool_registry: Arc<ToolRegistry>, depth: u32) -> Result<Self> {
        // Load full config from YAML for disabled tools, etc.
        let global_config = HermesConfig::load().ok();

        let context_engine = if config.compression_enabled {
            let comp_config = config.compression_config.clone().unwrap_or_else(|| {
                let mut c = CompressorConfig::default();
                if let Some(ref gc) = global_config {
                    if gc.compression.enabled {
                        c.config_context_length = gc.compression.target_tokens;
                        c.protect_first_n = gc.compression.protect_first_n;
                        c.summary_model_override = gc.compression.model.clone();
                    }
                }
                c
            });
            let engine_name = config.context_engine_name.as_deref().unwrap_or("compressor");
            hermes_prompt::create_engine(engine_name, Some(comp_config))
        } else {
            None
        };

        let max_iterations = config.max_iterations;
        let interrupt = Arc::new(AtomicBool::new(false));

        // Create subagent manager for top-level agents
        let subagent_mgr = if depth == 0 {
            let target = global_config
                .as_ref()
                .and_then(|gc| gc.compression.target_tokens)
                .unwrap_or(50);
            let max_child = target.min(200); // cap at reasonable max
            Some(Arc::new(SubagentManager::new(depth, interrupt.clone(), max_child)))
        } else {
            None
        };

        let session_db = config.session_db.clone();
        let persist_session = config.persist_session;

        // Auto-load plugins on agent startup
        let plugin_mgr = crate::plugin_system::PluginManager::new();
        let loaded_plugins = plugin_mgr.auto_load(Some(tool_registry.clone()));
        let mut wasm_plugins = Vec::new();
        for plugin in &loaded_plugins {
            if let Some(ref rt) = plugin.wasm_runtime {
                wasm_plugins.push(rt.clone());
            }
        }
        if !loaded_plugins.is_empty() {
            tracing::info!("Auto-loaded {} plugin(s), {} WASM", loaded_plugins.len(), wasm_plugins.len());
        }

        Ok(Self {
            config,
            tool_registry,
            cached_system_prompt: None,
            context_engine,
            memory_manager: MemoryManager::new(),
            failover_state: FailoverState::default(),
            budget: Arc::new(IterationBudget::new(max_iterations)),
            subagent_mgr,
            interrupt,
            interrupt_message: Mutex::new(None),
            delegate_depth: depth,
            delegate_results: Mutex::new(Vec::new()),
            activity_callback: None,
            turns_since_memory: 0,
            iters_since_skill: 0,
            disable_streaming: false,
            force_ascii_payload: false,
            last_usage: Mutex::new(None),
            stream_callback: None,
            status_callback: None,
            reasoning_callback: None,
            tool_gen_callback: None,
            interim_assistant_callback: None,
            current_streamed_assistant_text: Mutex::new(String::new()),
            stream_needs_break: Mutex::new(false),
            fallback_activated: false,
            primary_runtime: None,
            rate_limit_state: Mutex::new(None),
            last_activity_ts: Mutex::new(0.0),
            last_activity_desc: Mutex::new(String::new()),
            current_tool: Mutex::new(None),
            pre_llm_hook: None,
            turn_number: 0,
            session_db,
            persist_session,
            last_flushed_db_idx: 0,
            wasm_plugins,
        })
    }

    /// Invoke a lifecycle hook on all loaded WASM plugins.
    fn invoke_wasm_hooks(&self, hook_name: &str, context: &std::collections::HashMap<String, serde_json::Value>) {
        if self.wasm_plugins.is_empty() {
            return;
        }
        for plugin in &self.wasm_plugins {
            if let Err(e) = plugin.call_hook(hook_name, context) {
                tracing::debug!("WASM hook '{}' failed for plugin '{}': {}", hook_name, plugin.plugin_name, e);
            }
        }
    }

    /// Build or retrieve the cached system prompt.
    pub fn build_system_prompt(&mut self, system_message: Option<&str>) -> String {
        if let Some(ref cached) = self.cached_system_prompt {
            return cached.clone();
        }

        let available_tools: std::collections::HashSet<String> = self
            .tool_registry
            .get_definitions(None)
            .into_iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();

        let builder_config = PromptBuilderConfig {
            model: Some(self.config.model.clone()),
            provider: self.config.provider.clone(),
            session_id: self.config.session_id.clone(),
            platform: self.config.platform.clone(),
            skip_context_files: self.config.skip_context_files,
            terminal_cwd: self.config.terminal_cwd.clone(),
            tool_use_enforcement: ToolUseEnforcement::Auto,
            available_tools: Some(available_tools),
        };

        let result = build_system_prompt(&builder_config, system_message);
        let mut system_prompt = result.system_prompt;

        // Append memory system prompt block from external providers
        let memory_block = self.memory_manager.build_system_prompt();
        if !memory_block.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&memory_block);
        }

        self.cached_system_prompt = Some(system_prompt.clone());
        system_prompt
    }

    /// Run a complete conversation turn with the user.
    ///
    /// This is the main entry point for the agent loop:
    /// 1. Build system prompt (cached after first call)
    /// 2. Add user message to history
    /// 3. Loop: call LLM → parse tool calls → execute tools → append results
    /// 4. Return when no more tool calls or budget exhausted
    pub async fn run_conversation(
        &mut self,
        user_message: &str,
        system_message: Option<&str>,
        conversation_history: Option<&[Message]>,
    ) -> TurnResult {
        // Restore primary runtime if fallback was activated last turn.
        // Mirrors Python `_restore_primary_runtime()` (run_agent.py:5994).
        // Makes fallback turn-scoped instead of pinning the whole session.
        if self.fallback_activated {
            self.restore_primary_runtime();
        }
        // Snapshot primary runtime for potential fallback restoration next turn.
        if self.primary_runtime.is_none() {
            self.primary_runtime = Some(PrimaryRuntime {
                model: self.config.model.clone(),
                base_url: self.config.base_url.clone(),
                api_key: self.config.api_key.clone(),
                provider: self.config.provider.clone(),
            });
        }

        let mut messages: Vec<Message> = conversation_history
            .map(|h| h.to_vec())
            .unwrap_or_default();

        // Build system prompt
        let active_system_prompt: Arc<str> = Arc::from(self.build_system_prompt(system_message));

        // Add user message — sanitize stale memory-context blocks first.
        // Mirrors Python: sanitize_context(user_message) (run_agent.py:8163-8165).
        // Prevents stale memory tags from leaking into the conversation.
        let sanitized_user = sanitize_memory_context(user_message);
        messages.push(Arc::new(serde_json::json!({
            "role": "user",
            "content": sanitized_user
        })));

        // Memory manager on_turn_start notification.
        // Mirrors Python: memory_manager.on_turn_start() (run_agent.py).
        // Notifies all registered memory providers of the new turn so they
        // can prefetch context, update internal state, etc.
        self.turn_number += 1;
        let mut hook_ctx = std::collections::HashMap::new();
        hook_ctx.insert("turn_number".into(), serde_json::json!(self.turn_number));
        hook_ctx.insert("user_message".into(), serde_json::json!(user_message));
        global_hooks().invoke("on_session_start", &hook_ctx);
        self.invoke_wasm_hooks("on_session_start", &hook_ctx);

        self.memory_manager.on_turn_start(
            self.turn_number,
            user_message,
            &std::collections::HashMap::new(),
        );

        let mut api_call_count = 0;
        let mut final_response = String::new();
        // Exit reason — all branches in the loop set this before breaking.
        #[allow(unused_assignments)]
        let mut exit_reason = "max_iterations".to_string();
        let mut truncated_retry = false;
        let mut length_continue_retries: u32 = 0;
        let mut truncated_response_prefix = String::new();
        let mut compression_attempts: u32 = 0;
        let max_compression_attempts: u32 = 3;
        // Post-tool empty response nudge — only nudge once per tool round.
        let mut post_tool_empty_retried = false;
        // Compression exhaustion — set when max attempts reached without
        // resolving context overflow. Caller (gateway) should auto-reset.
        let mut compression_exhausted = false;

        // Self-evolution: increment turn counter, check memory nudge threshold
        let mut should_review_memory = false;
        let mut should_review_skills = false;
        self.turns_since_memory += 1;
        if self.config.self_evolution_enabled
            && self.turns_since_memory >= self.config.memory_nudge_interval
        {
            should_review_memory = true;
            self.turns_since_memory = 0;
        }

        // Main conversation loop
        // Grace call: when budget is exhausted, give the model one final chance.
        // Mirrors Python: `while (budget remaining > 0) or self._budget_grace_call`
        loop {
            let should_continue = if self.budget.remaining() > 0 {
                self.budget.consume()
            } else if self.budget.take_grace_call() {
                // Grace call — budget was exhausted but we get one more chance.
                // Consume the flag so loop exits after this iteration.
                tracing::debug!("Budget grace call — one final iteration");
                true
            } else {
                // Budget exhausted, no grace call available
                exit_reason = "budget_exhausted".to_string();
                break;
            };

            if !should_continue {
                // Budget exhausted — set grace call for one more iteration
                self.budget.set_grace_call();
                exit_reason = "budget_exhausted".to_string();
                break;
            }

            // Interrupt check — before each LLM call.
            // Mirrors Python: `if self._interrupt_requested` (run_agent.py:~8474).
            // Allows graceful termination of the tool-calling loop when
            // a new message arrives (gateway) or user presses Ctrl-C.
            if self.is_interrupted() {
                let msg = self.interrupt_message.lock()
                    .clone()
                    .unwrap_or_else(|| "Interrupted by user".to_string());
                tracing::info!(
                    "Interrupt requested — breaking conversation loop: {}",
                    msg
                );
                final_response = msg;
                exit_reason = "interrupted".to_string();
                break;
            }

            // Memory prefetch: recall relevant context for this turn.
            // Only on the first LLM call — retries should not re-inject memory.
            if api_call_count == 0 {
                if let Some(ref sid) = self.config.session_id {
                let memory_block = self.memory_manager.prefetch_all(user_message, sid);
                if !memory_block.is_empty() {
                    // Inject as a system note before the LLM call
                    let injected = format!(
                        "<memory-context>\n\
                        [System note: The following is recalled memory context, \
                        NOT new user input. Treat as informational background data.]\n\n\
                        {}\n\
                        </memory-context>",
                        sanitize_memory_context(&memory_block)
                    );
                    // Insert after the system prompt in API messages
                    // We'll prepend to the first user message
                    if let Some(first_user) = messages.iter_mut().find(|m| {
                        m.get("role").and_then(Value::as_str) == Some("user")
                    }) {
                        if let Some(content) = first_user.get("content").and_then(Value::as_str) {
                            let combined = format!("{}\n\n{}", injected, content);
                            let value = Arc::make_mut(first_user);
                            value["content"] = Value::String(combined);
                        }
                    }
                }
            }
            }

            // Context pressure warning — emit when nearing compaction threshold.
            // Mirrors Python `_emit_context_pressure()` (run_agent.py:7917).
            if let Some(ref engine) = self.context_engine {
                let threshold = engine.threshold_tokens();
                let approx_tokens = estimate_tokens(&messages);
                let pressure_pct = approx_tokens * 100 / threshold;
                if pressure_pct >= 80 {
                    self.emit_context_pressure(approx_tokens, threshold);
                }
            }

            // Message sanitization: strip orphaned tool results and fix
            // role sequences before sending to the API.
            // Mirrors Python `_sanitize_api_messages()` (run_agent.py:~8615).
            let sanitized = sanitize_api_messages(&messages);

            // Message normalization: strip whitespace from assistant text,
            // canonicalize tool-call JSON for cache prefix matching.
            // Mirrors Python normalization (run_agent.py:~8623-8645).
            let normalized = normalize_messages(&sanitized);

            // Plugin hook: pre_llm_call.
            // Allows plugins to inspect/modify messages, system prompt,
            // or abort the call entirely.
            let (mut hook_system_prompt, mut hook_messages) =
                (active_system_prompt.clone(), normalized);
            if let Some(ref hook) = self.pre_llm_hook {
                match hook(&hook_system_prompt, &hook_messages, api_call_count) {
                    PreLlmHookResult::Continue => {}
                    PreLlmHookResult::Abort(msg) => {
                        tracing::info!("Pre-LLM hook aborted: {}", msg);
                        final_response = msg;
                        exit_reason = "hook_aborted".to_string();
                        break;
                    }
                    PreLlmHookResult::OverrideSystem(sys) => {
                        hook_system_prompt = Arc::from(sys);
                    }
                    PreLlmHookResult::OverrideMessages(msgs) => {
                        hook_messages = msgs;
                    }
                    PreLlmHookResult::OverrideBoth(sys, msgs) => {
                        hook_system_prompt = Arc::from(sys);
                        hook_messages = msgs;
                    }
                }
            }

            // Call the LLM with stale-call timeout wrapper.
            // Mirrors Python: stale-call detector kills hung connections
            // after configured timeout (default 300s) so the retry loop
            // can apply richer recovery (credential rotation, provider fallback).
            let stale_timeout = stale_call_timeout(
                self.config.base_url.as_deref(),
                &hook_messages,
            );
            let call_start = std::time::Instant::now();

            // Choose streaming path when enabled and consumers are registered.
            // Mirrors Python `_stream_response()` branching (run_agent.py:~5143).
            let used_streaming = !self.disable_streaming && self.has_stream_consumers();
            let llm_result = if used_streaming {
                tokio::time::timeout(
                    stale_timeout,
                    self.call_llm_stream(&hook_system_prompt, &hook_messages),
                ).await
            } else {
                tokio::time::timeout(
                    stale_timeout,
                    self.call_llm(&hook_system_prompt, &hook_messages),
                ).await
            };

            match llm_result {
                Ok(Ok(response)) => {
                    api_call_count += 1;

                    // Fire stream delta with response text (TTS/display).
                    // When streaming was used, deltas were already fired during
                    // stream consumption; skip the post-hoc firing.
                    if !used_streaming {
                        if let Some(content) = response.get("content").and_then(Value::as_str) {
                            if !content.is_empty() {
                                self.fire_stream_delta(content);
                            }
                        }

                        // Extract and fire reasoning content from structured fields.
                        let reasoning_text = extract_reasoning(&response);
                        if !reasoning_text.is_empty() {
                            self.fire_reasoning_delta(&reasoning_text);
                        }
                    }

                    // Thinking-budget exhaustion detection.
                    // Mirrors Python reasoning-model handling (run_agent.py:~9049-9123).
                    // When reasoning models exhaust output tokens on thinking,
                    // the response may be mid-thought with no final answer.
                    // Treat this as a truncated response and attempt continuation.
                    if is_thinking_budget_exhausted(&response, &self.config.model) {
                        tracing::warn!(
                            "Thinking budget exhausted on model {} — treating as truncated",
                            self.config.model
                        );
                        if length_continue_retries < 3 {
                            length_continue_retries += 1;
                            let content = response
                                .get("content")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            truncated_response_prefix.push_str(content);
                            messages.push(Arc::new(response));
                            messages.push(Arc::new(serde_json::json!({
                                "role": "user",
                                "content": "Please continue your previous response from exactly where you left off. Do NOT repeat content, do NOT summarize — just continue."
                            })));
                            continue;
                        }
                        // Exceeded retries — fall through to partial handling
                    }

                    // Detect truncated tool_call arguments (finish_reason="length"
                    // with invalid JSON in tool arguments). Mirrors Python: retry
                    // once instead of wasting 3 continuation attempts.
                    if truncated_retry {
                        truncated_retry = false;
                        // Previous call had truncated tool args — don't append,
                        // just re-run from current message state.
                        continue;
                    }

                    // Successful response — reset compression counter
                    if compression_attempts > 0 {
                        compression_attempts = 0;
                    }

                    // Check for tool calls
                    if let Some(tool_calls) = response.get("tool_calls").and_then(Value::as_array).cloned() {
                        if tool_calls.is_empty() {
                            // Empty tool_calls array — may still be length truncated
                            let is_length = response.get("finish_reason")
                                .and_then(Value::as_str)
                                .is_some_and(|fr| fr == "length" || fr == "length_limit");

                            if is_length && length_continue_retries < 3 {
                                length_continue_retries += 1;
                                let content = response
                                    .get("content")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                truncated_response_prefix.push_str(content);
                                tracing::warn!(
                                    "Response truncated with empty tool_calls — continuing (attempt {}/{})",
                                    length_continue_retries, 3
                                );
                                messages.push(Arc::new(response));
                                messages.push(Arc::new(serde_json::json!({
                                    "role": "user",
                                    "content": "Please continue your previous response from exactly where you left off. Do NOT repeat content, do NOT summarize — just continue."
                                })));
                                continue;
                            }

                            // Not truncated or exceeded retries — treat as final
                            // Post-tool empty response nudge (Python PR #9400):
                            // Weaker models sometimes return empty after tool results
                            // instead of continuing. Nudge once per tool round.
                            let content = response.get("content").and_then(Value::as_str).unwrap_or("");
                            let has_recent_tool_result = messages.iter().rev().take(5)
                                .any(|m| m.get("role").and_then(Value::as_str) == Some("tool"));
                            if content.is_empty() && has_recent_tool_result && !post_tool_empty_retried {
                                post_tool_empty_retried = true;
                                tracing::info!(
                                    "Empty response after tool calls — nudging model to continue"
                                );
                                // Append the empty assistant message first so the
                                // message sequence stays valid: tool(result) → assistant("(empty)") → user(nudge)
                                messages.push(Arc::new(serde_json::json!({
                                    "role": "assistant",
                                    "content": "(empty)"
                                })));
                                messages.push(Arc::new(serde_json::json!({
                                    "role": "user",
                                    "content": "You just executed tool calls but returned an \
                                    empty response. Please process the tool \
                                    results above and continue with the task."
                                })));
                                continue;
                            }

                            if !truncated_response_prefix.is_empty() {
                                let mut full = truncated_response_prefix.clone();
                                full.push_str(content);
                                final_response = full;
                            } else {
                                final_response = content.to_string();
                            }
                            exit_reason = "completed".to_string();
                            messages.push(Arc::new(response));
                            break;
                        }

                        // Check for truncated tool arguments
                        let is_truncated = response.get("finish_reason")
                            .and_then(Value::as_str)
                            .is_some_and(|fr| fr == "length" || fr == "length_limit")
                            && has_truncated_tool_args(&tool_calls);

                        if is_truncated {
                            truncated_retry = true;
                            tracing::warn!(
                                "Truncated tool call detected — retrying API call (tool_calls={})",
                                tool_calls.len()
                            );
                            continue;
                        }

                        // Add assistant message with tool calls
                        messages.push(Arc::new(response));

                        // Deduplicate tool calls before execution.
                        // Mirrors Python `_deduplicate_tool_calls()` (run_agent.py:3573).
                        let deduped = Self::deduplicate_tool_calls(&tool_calls);

                        // Execute tools: concurrent for independent batches,
                        // sequential for interactive/dependent tools.
                        // Mirrors Python `_execute_tool_calls()` dispatch
                        // (run_agent.py:7163).
                        let tool_calls_json: Vec<serde_json::Value> = deduped.iter().cloned().collect();
                        let mut pre_ctx = std::collections::HashMap::new();
                        pre_ctx.insert("tool_calls".into(), serde_json::json!(tool_calls_json));
                        global_hooks().invoke("pre_tool_call", &pre_ctx);
                        self.invoke_wasm_hooks("pre_tool_call", &pre_ctx);

                        let tool_results = if Self::should_parallelize_tool_batch(&deduped) {
                            tracing::debug!("Using concurrent tool execution for {} tools", deduped.len());
                            self.execute_tool_calls_concurrent(&deduped).await
                        } else {
                            tracing::debug!("Using sequential tool execution for {} tools", deduped.len());
                            self.execute_tool_calls_sequential(&deduped).await
                        };

                        let mut post_ctx = std::collections::HashMap::new();
                        post_ctx.insert("tool_count".into(), serde_json::json!(tool_results.len()));
                        global_hooks().invoke("post_tool_call", &post_ctx);
                        self.invoke_wasm_hooks("post_tool_call", &post_ctx);

                        // Append all tool results to message history
                        for tool_result in tool_results {
                            messages.push(Arc::new(tool_result));
                        }

                        // Check for subagent delegation results
                        if let Some(delegate_results) = self.take_delegate_results() {
                            for r in delegate_results {
                                messages.push(Arc::new(serde_json::json!({
                                    "role": "tool",
                                    "content": serde_json::json!({
                                        "goal": r.goal,
                                        "response": r.response,
                                        "exit_reason": r.exit_reason,
                                        "api_calls": r.api_calls,
                                    }).to_string(),
                                    "tool_call_id": "delegate_result",
                                })));
                            }
                        }

                        // Check context compression
                        if let Some(ref mut engine) = self.context_engine {
                            if engine.should_compress(None) {
                                let temp: Vec<Value> = messages.iter().map(|m| (**m).clone()).collect();
                                messages = engine.compress(&temp, None, None)
                                    .into_iter().map(Arc::new).collect();
                                // Rebuild system prompt after compression
                                self.cached_system_prompt = None;
                                let _ = self.build_system_prompt(system_message);
                                // Compression resets retry counters so the model
                                // gets a fresh budget on the compressed context.
                                // Without this, pre-compression retries carry over
                                // and the model hits errors immediately after
                                // compression-induced context loss.
                                compression_attempts = 0;
                                length_continue_retries = 0;
                                truncated_response_prefix.clear();
                                truncated_retry = false;
                            }
                        }

                        // Self-evolution: increment iteration counter after each tool-calling iteration
                        self.iters_since_skill += 1;
                        // Successful tool execution — reset the post-tool nudge flag
                        // so it can fire again if the model goes empty on a later tool round.
                        post_tool_empty_retried = false;
                    } else {
                        // No tool_calls key in response — check content
                        let is_length_truncated = response.get("finish_reason")
                            .and_then(Value::as_str)
                            .is_some_and(|fr| fr == "length" || fr == "length_limit");

                        if is_length_truncated {
                            // Text was cut off — try to continue (up to 3 times)
                            if length_continue_retries < 3 {
                                length_continue_retries += 1;
                                let content = response
                                    .get("content")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                truncated_response_prefix.push_str(content);
                                tracing::warn!(
                                    "Response truncated (length) — continuing (attempt {}/{})",
                                    length_continue_retries, 3
                                );
                                // Inject continue message
                                messages.push(Arc::new(response));
                                messages.push(Arc::new(serde_json::json!({
                                    "role": "user",
                                    "content": "Please continue your previous response from exactly where you left off. Do NOT repeat content, do NOT summarize — just continue."
                                })));
                                continue;
                            } else {
                                // Exceeded 3 retries — return partial response
                                let content = response
                                    .get("content")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                truncated_response_prefix.push_str(content);
                                final_response = truncated_response_prefix.clone();
                                exit_reason = "partial".to_string();
                                messages.push(Arc::new(response));
                                break;
                            }
                        }

                        // Not truncated — this is the final response
                        if let Some(content) = response.get("content").and_then(Value::as_str) {
                            // Prepend any accumulated continuation prefix
                            if truncated_response_prefix.is_empty() {
                                final_response = content.to_string();
                            } else {
                                let mut full = truncated_response_prefix.clone();
                                full.push_str(content);
                                final_response = full;
                            }
                        }
                        exit_reason = "completed".to_string();
                        messages.push(Arc::new(response));
                        break;
                    }
                }
                Ok(Err(e)) => {
                    // Full failover chain: classify → recover → retry or abort.
                    // Mirrors Python failover chain (run_agent.py:9350-10127).
                    let error_msg = e.to_string();
                    // Use the actual provider from config for accurate classification
                    let provider = self.config.provider.as_deref().unwrap_or("unknown");
                    let classification = hermes_llm::error_classifier::classify_api_error(
                        provider, &self.config.model, None, &error_msg,
                    );

                    // Map ClassifiedError → failover action
                    let has_compressor = self.context_engine.is_some();
                    let had_prior_success = api_call_count > 0;
                    let action = failover::apply_failover(
                        &classification,
                        &mut self.failover_state,
                        self.config.credential_pool.as_deref(),
                        has_compressor,
                        had_prior_success,
                    );

                    let api_duration = call_start.elapsed().as_secs_f64();
                    let failure_hint = build_failure_hint(&classification, api_duration);

                    match action {
                        FailoverAction::SanitizeUnicode => {
                            tracing::warn!("Failover: sanitizing Unicode surrogate characters");
                            failover::sanitize_unicode_messages(&mut messages);
                            continue;
                        }
                        FailoverAction::RotateCredential => {
                            tracing::warn!("Failover: rotating credential");
                            if let Some(ref pool) = self.config.credential_pool {
                                pool.mark_exhausted_and_rotate(None, None);
                            }
                            continue;
                        }
                        FailoverAction::RefreshProviderAuth => {
                            // Provider-specific OAuth refresh (Anthropic, Codex, Nous).
                            // Mirrors Python: refresh_anthropic_oauth_pure (run_agent.py:9500-9570).
                            tracing::warn!("Failover: attempting provider auth refresh");
                            if let Some(ref pool) = self.config.credential_pool {
                                if pool.try_refresh_current().await {
                                    tracing::info!("Failover: provider auth refresh succeeded");
                                    continue;
                                }
                                tracing::warn!("Failover: provider auth refresh failed, will rotate");
                            }
                            // No pool or refresh failed — fall through to retry/backoff
                            let backoff_ms = compute_backoff_ms(self.failover_state.retry_count);
                            tracing::warn!(
                                "Failover: retrying with backoff {}ms (auth refresh failed)",
                                backoff_ms
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                            continue;
                        }
                        FailoverAction::StripThinkingSignature => {
                            tracing::warn!("Failover: stripping thinking signature");
                            failover::strip_reasoning_from_messages(&mut messages);
                            continue;
                        }
                        FailoverAction::RollbackToLastAssistant => {
                            tracing::warn!("Failover: rolling back to last assistant turn");
                            let rolled = rollback_to_last_assistant(&messages);
                            if rolled.len() < messages.len() {
                                tracing::info!(
                                    "Rolled back messages: {} → {} entries",
                                    messages.len(), rolled.len()
                                );
                                messages = rolled;
                                // Reset retry counters after rollback
                                length_continue_retries = 0;
                                truncated_response_prefix.clear();
                                truncated_retry = false;
                            }
                            continue;
                        }
                        FailoverAction::ReduceContextTier => {
                            // Reduce context tier: degrade probe level, remove oldest turns.
                            // Mirrors Python: reduce_tier / degrade_probe (run_agent.py:9800-9900).
                            // Step 1: Remove oldest user turns first (least valuable)
                            // Step 2: Strip reasoning from assistant messages
                            tracing::warn!("Failover: reducing context tier");
                            let original_len = messages.len();
                            // Strip reasoning from all messages
                            failover::strip_reasoning_from_messages(&mut messages);
                            // Remove oldest non-system message (typically oldest user turn)
                            if let Some(idx) = messages.iter().position(|m| {
                                m.get("role").and_then(Value::as_str) != Some("system")
                            }) {
                                messages.remove(idx);
                            }
                            tracing::info!(
                                "Context tier reduced: {} → {} entries",
                                original_len, messages.len()
                            );
                            // Clear cached prompt since context changed
                            self.cached_system_prompt = None;
                            continue;
                        }
                        FailoverAction::CompressContext => {
                            if compression_attempts < max_compression_attempts {
                                compression_attempts += 1;
                                tracing::warn!(
                                    "Failover: compressing context (attempt {}/{})",
                                    compression_attempts, max_compression_attempts
                                );
                                if let Some(ref mut engine) = self.context_engine {
                                    let temp: Vec<Value> = messages.iter().map(|m| (**m).clone()).collect();
                                    messages = engine.compress(&temp, None, None)
                                        .into_iter().map(Arc::new).collect();
                                    self.cached_system_prompt = None;
                                    let _ = self.build_system_prompt(system_message);
                                    continue;
                                }
                            } else {
                                tracing::error!(
                                    "Failover: max compression attempts ({}) reached",
                                    max_compression_attempts
                                );
                                compression_exhausted = true;
                                final_response = format!("Error: context too large after {} compression attempts: {}", max_compression_attempts, e);
                                exit_reason = "llm_error".to_string();
                                break;
                            }
                        }
                        FailoverAction::RetryWithBackoff => {
                            // Apply exponential backoff
                            let backoff_ms = compute_backoff_ms(self.failover_state.retry_count);
                            tracing::warn!(
                                "Failover: retrying with backoff {}ms ({})",
                                backoff_ms, failure_hint
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                            continue;
                        }
                        FailoverAction::TryFallback => {
                            // Try all fallback providers in order.
                            // Mirrors Python: iterates through fallback_providers chain.
                            if self.config.fallback_providers.is_empty() {
                                tracing::error!("LLM call failed: {} ({})", e, failure_hint);
                                final_response = format!("Error: {} ({})", e, failure_hint);
                                exit_reason = "llm_error".to_string();
                                break;
                            }

                            let orig_model = self.config.model.clone();
                            let orig_base_url = self.config.base_url.clone();
                            let orig_api_key = self.config.api_key.clone();
                            let orig_provider = self.config.provider.clone();

                            let mut fallback_succeeded = false;
                            let mut last_fb_err = e.to_string();

                            for fallback in &self.config.fallback_providers {
                                tracing::warn!("Failover: trying fallback provider {}", fallback.model);

                                self.config.model.clone_from(&fallback.model);
                                self.config.base_url.clone_from(&fallback.base_url);
                                self.config.api_key.clone_from(&fallback.api_key);
                                self.config.provider.clone_from(&fallback.provider);

                                // Reset failover state and compression counter for fresh attempt
                                self.failover_state = FailoverState::default();
                                #[allow(unused_assignments)] { compression_attempts = 0; }

                                match self.call_llm(&active_system_prompt, &messages).await {
                                    Ok(resp) => {
                                        api_call_count += 1;
                                        final_response = resp.get("content")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string();
                                        exit_reason = "completed".to_string();
                                        messages.push(Arc::new(resp));
                                        fallback_succeeded = true;
                                        // Mark fallback activated — next turn will restore primary.
                                        // Mirrors Python `_fallback_activated = True` (run_agent.py:5920).
                                        self.fallback_activated = true;
                                        break;
                                    }
                                    Err(fb_err) => {
                                        last_fb_err = fb_err.to_string();
                                        tracing::warn!(
                                            "Failover: fallback {} also failed: {}",
                                            fallback.model, last_fb_err
                                        );
                                    }
                                }
                            }

                            // Restore original config regardless
                            self.config.model = orig_model;
                            self.config.base_url = orig_base_url;
                            self.config.api_key = orig_api_key;
                            self.config.provider = orig_provider;

                            if fallback_succeeded {
                                break;
                            }

                            tracing::error!(
                                "Failover: all {} fallback(s) failed. Last error: {} ({})",
                                self.config.fallback_providers.len(), last_fb_err, failure_hint
                            );
                            final_response = format!(
                                "Error: {} ({}); all fallbacks also failed. Last: {}",
                                e, failure_hint, last_fb_err
                            );
                            exit_reason = "llm_error".to_string();
                            break;
                        }
                        FailoverAction::Abort => {
                            tracing::error!("LLM call failed (non-recoverable): {} ({})", e, failure_hint);
                            final_response = format!("Error: {} ({})", e, failure_hint);
                            exit_reason = "llm_error".to_string();
                            break;
                        }
                    }
                }
                Err(_timeout) => {
                    // Stale-call timeout: no response arrived within timeout.
                    // Kill the connection and return error so retry loop can
                    // apply richer recovery (credential rotation, provider fallback).
                    let est_tokens = estimate_tokens(&messages);
                    let timeout_secs = stale_timeout.as_secs();
                    tracing::warn!(
                        "Non-streaming API call stale for {}s (threshold {}s). model={} context=~{} tokens. Killing connection.",
                        timeout_secs, timeout_secs, self.config.model, est_tokens,
                    );
                    final_response = format!(
                        "Error: no response from provider for {}s (model: {}, ~{} tokens)",
                        timeout_secs, self.config.model, est_tokens
                    );
                    exit_reason = "llm_error".to_string();
                    break;
                }
            }
        }

        // Self-evolution: check skill nudge at turn end
        if self.config.self_evolution_enabled
            && self.iters_since_skill >= self.config.skill_nudge_interval
        {
            should_review_skills = true;
            self.iters_since_skill = 0;
        }

        // Spawn background review if warranted
        if self.config.self_evolution_enabled
            && !final_response.is_empty()
            && (should_review_memory || should_review_skills)
        {
            self.spawn_background_review(&messages, should_review_memory, should_review_skills);
        }

        // If loop ended without setting exit_reason
        if !matches!(exit_reason.as_ref(), "completed" | "llm_error" | "budget_exhausted") {
            exit_reason = "max_iterations".to_string();
        }

        // Memory sync: record user/assistant turn for external providers
        if exit_reason == "completed" && !final_response.is_empty() {
            if let Some(ref sid) = self.config.session_id {
                self.memory_manager.sync_all(user_message, &final_response, sid);
            }
        }

        // Persist session to SQLite and trajectory files
        let completed = exit_reason == "completed";
        self.persist_session(&messages, user_message, completed);

        let mut end_ctx = std::collections::HashMap::new();
        end_ctx.insert("exit_reason".into(), serde_json::json!(&exit_reason));
        end_ctx.insert("api_calls".into(), serde_json::json!(api_call_count));
        global_hooks().invoke("on_session_end", &end_ctx);
        self.invoke_wasm_hooks("on_session_end", &end_ctx);

        TurnResult {
            response: final_response,
            messages,
            api_calls: api_call_count,
            exit_reason: exit_reason.to_string(),
            compression_exhausted,
            usage: self.take_last_usage(),
        }
    }

    /// Call the LLM with the current messages.
    ///
    /// Dispatches to hermes_llm::client::call_llm based on the model prefix.
    async fn call_llm(
        &self,
        system_prompt: &str,
        messages: &[Message],
    ) -> Result<Value> {
        // Build API request with system prompt and messages
        let mut api_messages: Vec<Value> = vec![serde_json::json!({
            "role": "system",
            "content": system_prompt
        })];
        api_messages.extend(messages.iter().map(|m| (**m).clone()));

        // Apply Anthropic caching if enabled (skip for Codex Responses API)
        let is_codex = self.config.api_mode.as_deref() == Some("codex");
        let cached_messages = if self.config.enable_caching && !is_codex {
            apply_anthropic_cache_control(&api_messages, CacheTtl::FiveMinutes, false)
        } else {
            api_messages
        };

        // Get tool definitions for the API request
        let tool_definitions = self.tool_registry.get_definitions(None);

        tracing::info!(
            "LLM call: model={}, messages={}, tools={}",
            self.config.model,
            cached_messages.len(),
            tool_definitions.len()
        );

        // Build the LLM request.
        // If a credential pool is available, use the current credential's
        // API key and base URL (mirrors Python _swap_credential pattern).
        let (resolved_api_key, resolved_base_url) = if let Some(ref pool) = self.config.credential_pool {
            if let Some(cred) = pool.current() {
                let key = cred.runtime_api_key().to_string();
                let url = cred.runtime_base_url().map(String::from)
                    .or_else(|| self.config.base_url.clone());
                (Some(key), url)
            } else {
                (self.config.api_key.clone(), self.config.base_url.clone())
            }
        } else {
            (self.config.api_key.clone(), self.config.base_url.clone())
        };

        let request = hermes_llm::client::LlmRequest {
            model: self.config.model.clone(),
            messages: cached_messages,
            tools: if tool_definitions.is_empty() { None } else { Some(tool_definitions.iter().map(|v| (**v).clone()).collect()) },
            temperature: None,
            max_tokens: None,
            base_url: resolved_base_url,
            api_key: resolved_api_key,
            timeout_secs: None,
            provider_preferences: self.config.provider_preferences.clone(),
            api_mode: self.config.api_mode.clone(),
        };

        let response = hermes_llm::client::call_llm(request).await
            .map_err(|e| hermes_core::HermesError::new(
                hermes_core::ErrorCategory::ApiError,
                e.to_string(),
            ))?;

        // Capture usage for later propagation to API responses
        if let Some(ref usage_info) = response.usage {
            let usage = TurnUsage {
                prompt_tokens: usage_info.prompt_tokens,
                completion_tokens: usage_info.completion_tokens,
                total_tokens: usage_info.total_tokens,
            };
            // Safety: we hold &mut self through the async borrow, so this is safe
            // to update via a separate method call after the match.
            // We'll store it after returning. For now, save it via a helper.
            self.set_last_usage(usage);
        }

        // Convert to internal format
        let mut result = serde_json::json!({
            "role": "assistant",
            "content": response.content.unwrap_or_default(),
        });

        if let Some(tool_calls) = response.tool_calls {
            result["tool_calls"] = serde_json::Value::Array(tool_calls);
        }

        if let Some(ref finish) = response.finish_reason {
            result["finish_reason"] = serde_json::Value::String(finish.clone());
        }

        Ok(result)
    }

    /// Streaming variant of `call_llm`.
    ///
    /// Consumes `LlmStreamEvent`s from the provider, fires display/TTS
    /// callbacks, and assembles the final response `Value`.
    /// Mirrors Python `_stream_response()` (run_agent.py:~5143).
    async fn call_llm_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
    ) -> Result<Value> {
        use futures::StreamExt;

        // Reset per-response stream tracking.
        self.reset_stream_delivery_tracking();

        let mut api_messages: Vec<Value> = vec![serde_json::json!({
            "role": "system",
            "content": system_prompt
        })];
        api_messages.extend(messages.iter().map(|m| (**m).clone()));

        let is_codex = self.config.api_mode.as_deref() == Some("codex");
        let cached_messages = if self.config.enable_caching && !is_codex {
            apply_anthropic_cache_control(&api_messages, CacheTtl::FiveMinutes, false)
        } else {
            api_messages
        };

        let tool_definitions = self.tool_registry.get_definitions(None);

        let (resolved_api_key, resolved_base_url) =
            if let Some(ref pool) = self.config.credential_pool {
                if let Some(cred) = pool.current() {
                    let key = cred.runtime_api_key().to_string();
                    let url = cred.runtime_base_url()
                        .map(String::from)
                        .or_else(|| self.config.base_url.clone());
                    (Some(key), url)
                } else {
                    (self.config.api_key.clone(), self.config.base_url.clone())
                }
            } else {
                (self.config.api_key.clone(), self.config.base_url.clone())
            };

        let request = hermes_llm::client::LlmRequest {
            model: self.config.model.clone(),
            messages: cached_messages,
            tools: if tool_definitions.is_empty() { None } else { Some(tool_definitions.iter().map(|v| (**v).clone()).collect()) },
            temperature: None,
            max_tokens: None,
            base_url: resolved_base_url,
            api_key: resolved_api_key,
            timeout_secs: None,
            provider_preferences: self.config.provider_preferences.clone(),
            api_mode: self.config.api_mode.clone(),
        };

        let mut stream = hermes_llm::client::call_llm_stream(request).await
            .map_err(|e| hermes_core::HermesError::new(
                hermes_core::ErrorCategory::ApiError,
                e.to_string(),
            ))?;

        // Accumulators
        let mut text_parts = Vec::new();
        let mut reasoning_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut final_usage: Option<hermes_llm::client::UsageInfo> = None;

        while let Some(event) = stream.next().await {
            match event {
                hermes_llm::client::LlmStreamEvent::TextDelta { delta } => {
                    if !delta.is_empty() {
                        self.fire_stream_delta(&delta);
                        text_parts.push(delta);
                    }
                }
                hermes_llm::client::LlmStreamEvent::ReasoningDelta { delta } => {
                    if !delta.is_empty() {
                        self.fire_reasoning_delta(&delta);
                        reasoning_parts.push(delta);
                    }
                }
                hermes_llm::client::LlmStreamEvent::ToolGenStarted { name } => {
                    self.fire_tool_gen_started(&name);
                }
                hermes_llm::client::LlmStreamEvent::ToolCall { id, name, arguments } => {
                    self.fire_tool_gen_started(&name);
                    tool_calls.push(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }));
                }
                hermes_llm::client::LlmStreamEvent::Done { usage } => {
                    final_usage = usage;
                    break;
                }
                hermes_llm::client::LlmStreamEvent::Error { message } => {
                    return Err(hermes_core::HermesError::new(
                        hermes_core::ErrorCategory::ApiError,
                        message,
                    ));
                }
            }
        }

        if let Some(usage_info) = final_usage {
            self.set_last_usage(TurnUsage {
                prompt_tokens: usage_info.prompt_tokens,
                completion_tokens: usage_info.completion_tokens,
                total_tokens: usage_info.total_tokens,
            });
        }

        let mut result = serde_json::json!({
            "role": "assistant",
            "content": text_parts.join(""),
        });

        if !tool_calls.is_empty() {
            result["tool_calls"] = serde_json::Value::Array(tool_calls);
        }

        // If we collected reasoning, attach it for downstream extraction
        if !reasoning_parts.is_empty() {
            result["reasoning"] = serde_json::Value::String(reasoning_parts.join(""));
        }

        Ok(result)
    }

    /// Store usage from the last LLM call (interior mutability via Mutex).
    fn set_last_usage(&self, usage: TurnUsage) {
        {
            let mut guard = self.last_usage.lock();
            *guard = Some(usage);
        }
    }

    /// Extract and clear the last LLM usage.
    fn take_last_usage(&self) -> Option<TurnUsage> {
        self.last_usage.lock().take()
    }

    /// Extract and clear pending delegate results.
    fn take_delegate_results(&self) -> Option<Vec<SubagentResult>> {
        let mut guard = self.delegate_results.lock();
        if guard.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *guard))
        }
    }

    /// Store delegate results for injection into the conversation.
    fn store_delegate_results(&self, results: Vec<SubagentResult>) {
        let mut guard = self.delegate_results.lock();
        guard.extend(results);
    }

    /// Set an activity callback to prevent gateway inactivity timeout.
    ///
    /// Called before each tool execution with a message like
    /// `"calling tool: {name}"`. Useful for gateway deployments
    /// that need to signal activity to avoid inactivity timeouts.
    pub fn set_activity_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.activity_callback = Some(Arc::new(callback));
    }

    /// Set a stream callback to receive text deltas during LLM streaming.
    ///
    /// Mirrors Python `_stream_callback` (run_agent.py:5168).
    /// Used by TTS pipeline to start audio generation before the full response.
    pub fn set_stream_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.stream_callback = Some(Arc::new(callback));
    }

    /// Set a status callback for gateway platform notifications.
    ///
    /// Mirrors Python `status_callback` (run_agent.py:5194+).
    /// Receives (event_type, message) pairs for context pressure,
    /// compression warnings, and other user-facing status updates.
    pub fn set_status_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        self.status_callback = Some(Arc::new(callback));
    }

    /// Set a reasoning callback to receive thinking/thinking deltas.
    ///
    /// Mirrors Python `reasoning_callback` (run_agent.py:5163).
    /// Receives reasoning text chunks during streaming for display.
    pub fn set_reasoning_stream_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.reasoning_callback = Some(Arc::new(callback));
    }

    /// Set a tool generation started callback.
    ///
    /// Mirrors Python `_fire_tool_gen_started()` (run_agent.py:5172).
    /// Fires once per tool name when the streaming response begins
    /// producing tool_call / tool_use tokens.
    pub fn set_tool_gen_started_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.tool_gen_callback = Some(Arc::new(callback));
    }

    /// Set an interim assistant message callback.
    ///
    /// Mirrors Python `_emit_interim_assistant_message()` (run_agent.py:5128).
    /// Fires mid-turn to surface real assistant commentary to the UI layer.
    /// Signature: (visible_text, already_streamed).
    #[allow(dead_code)]
    pub fn set_interim_assistant_callback<F>(&mut self, callback: F)
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
    {
        self.interim_assistant_callback = Some(Arc::new(callback));
    }

    /// Restore the primary runtime if fallback was activated last turn.
    ///
    /// Mirrors Python `_restore_primary_runtime()` (run_agent.py:5994).
    /// Makes fallback turn-scoped instead of pinning the whole session
    /// to the fallback provider for every subsequent turn.
    fn restore_primary_runtime(&mut self) {
        let Some(primary) = self.primary_runtime.take() else {
            tracing::warn!("Fallback activated but no primary runtime snapshot");
            self.fallback_activated = false;
            return;
        };

        let old_model = self.config.model.clone();
        self.config.model = primary.model.clone();
        self.config.base_url = primary.base_url.clone();
        self.config.api_key = primary.api_key.clone();
        self.config.provider = primary.provider.clone();

        self.fallback_activated = false;
        self.failover_state = FailoverState::default();

        tracing::info!(
            "Primary runtime restored: {} → {} (provider={:?})",
            old_model, primary.model, primary.provider
        );
    }

    /// Fire the stream delta callback with new text.
    ///
    /// Mirrors Python `_fire_stream_delta()` (run_agent.py:5143).
    /// Delivers text to TTS pipeline and display layers.
    /// Handles paragraph break injection across tool boundaries
    /// and tracks what has been streamed to avoid resending.
    fn fire_stream_delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }

        // If a tool iteration set the break flag, prepend a paragraph
        // break before the first real text delta.
        // Mirrors Python: `_stream_needs_break` handling.
        let mut needs_break = self.stream_needs_break.lock();
        let text = if *needs_break && !text.trim().is_empty() {
            *needs_break = false;
            format!("\n\n{text}")
        } else {
            text.to_string()
        };
        drop(needs_break);

        // Fire callback and record delivery.
        if let Some(ref cb) = self.stream_callback {
            cb(text.as_str());
            self.record_streamed_assistant_text(&text);
        }
    }

    /// Fire the reasoning delta callback with reasoning text.
    ///
    /// Mirrors Python `_fire_reasoning_delta()` (run_agent.py:5163).
    /// Delivers thinking/reasoning chunks to display layers.
    fn fire_reasoning_delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ref cb) = self.reasoning_callback {
            cb(text);
        }
    }

    /// Notify display layer that tool call generation has started.
    ///
    /// Mirrors Python `_fire_tool_gen_started()` (run_agent.py:5172).
    /// Fires once per tool name when streaming begins producing
    /// tool_call / tool_use tokens.
    fn fire_tool_gen_started(&self, tool_name: &str) {
        if let Some(ref cb) = self.tool_gen_callback {
            cb(tool_name);
        }
    }

    /// Check if any streaming consumer is registered.
    ///
    /// Mirrors Python `_has_stream_consumers()` (run_agent.py:5187).
    fn has_stream_consumers(&self) -> bool {
        self.stream_callback.is_some() || self.reasoning_callback.is_some()
    }

    /// Reset tracking for text delivered during the current model response.
    ///
    /// Mirrors Python `_reset_stream_delivery_tracking()` (run_agent.py:5100).
    fn reset_stream_delivery_tracking(&self) {
        let mut tracked = self.current_streamed_assistant_text.lock();
        tracked.clear();
        let mut needs_break = self.stream_needs_break.lock();
        *needs_break = false;
    }

    /// Accumulate visible assistant text emitted through stream callbacks.
    ///
    /// Mirrors Python `_record_streamed_assistant_text()` (run_agent.py:5104).
    fn record_streamed_assistant_text(&self, text: &str) {
        if !text.is_empty() {
            let mut tracked = self.current_streamed_assistant_text.lock();
            tracked.push_str(text);
        }
    }

    /// Normalize interim visible text for display comparison.
    ///
    /// Mirrors Python `_normalize_interim_visible_text()` (run_agent.py:5112).
    /// Collapses whitespace and trims.
    #[allow(dead_code)]
    fn normalize_interim_visible_text(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut prev_space = true;
        for c in text.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    result.push(' ');
                    prev_space = true;
                }
            } else {
                result.push(c);
                prev_space = false;
            }
        }
        result.trim().to_string()
    }

    /// Strip reasoning/thinking blocks from content.
    ///
    /// Mirrors Python `_strip_think_blocks()` (run_agent.py:2096).
    /// Handles all tag variants: <think>, <thinking>, <reasoning>,
    /// <REASONING_SCRATCHPAD>, <thought>, <think>, etc.
    #[allow(dead_code)]
    fn strip_think_blocks(content: &str) -> String {
        let mut result = String::with_capacity(content.len());
        let bytes = content.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            // <|think|>...|>
            if content[i..].starts_with("<|think|>") {
                if let Some(end) = content[i+9..].find("|>") {
                    i += end + 11;
                    continue;
                } else {
                    break;
                }
            }

            // <think>...
            if content[i..].starts_with("<think>") {
                if let Some(end) = content[i..].find("</think>") {
                    i += end + 9;
                    continue;
                } else {
                    break;
                }
            }

            // <thinking>...</thinking> (case-insensitive)
            if content[i..].to_lowercase().starts_with("<thinking>") {
                let lower = content[i..].to_lowercase();
                if let Some(end) = lower.find("</thinking>") {
                    i += end + 11;
                    continue;
                } else {
                    break;
                }
            }

            // <reasoning>...</reasoning>
            if content[i..].starts_with("<reasoning>") {
                if let Some(end) = content[i..].find("</reasoning>") {
                    i += end + 12;
                    continue;
                } else {
                    break;
                }
            }

            // <REASONING_SCRATCHPAD>...</REASONING_SCRATCHPAD>
            if content[i..].starts_with("<REASONING_SCRATCHPAD>") {
                if let Some(end) = content[i..].find("</REASONING_SCRATCHPAD>") {
                    i += end + 25;
                    continue;
                } else {
                    break;
                }
            }

            // <thought>...</thought> (case-insensitive)
            if content[i..].to_lowercase().starts_with("<thought>") {
                let lower = content[i..].to_lowercase();
                if let Some(end) = lower.find("</thought>") {
                    i += end + 10;
                    continue;
                } else {
                    break;
                }
            }

            // Strip bare closing tags that leaked through
            if content[i..].starts_with("</think>")
                || content[i..].starts_with("</thinking>")
                || content[i..].starts_with("</reasoning>")
                || content[i..].starts_with("</thought>")
                || content[i..].starts_with("</REASONING_SCRATCHPAD>")
            {
                if let Some(gt) = content[i..].find('>') {
                    i += gt + 1;
                    continue;
                }
            }

            // Not inside a think block — emit byte
            result.push(bytes[i] as char);
            i += 1;
        }

        result
    }

    /// Check if content has actual text after reasoning/thinking blocks.
    ///
    /// Mirrors Python `_has_content_after_think_block()` (run_agent.py:2073).
    /// Detects cases where the model only outputs reasoning but no actual
    /// response, indicating incomplete generation that should be retried.
    #[allow(dead_code)]
    fn has_content_after_think_block(content: &str) -> bool {
        if content.is_empty() {
            return false;
        }
        !Self::strip_think_blocks(content).trim().is_empty()
    }

    /// Detect Codex-style intermediate acknowledgment.
    ///
    /// Mirrors Python `_looks_like_codex_intermediate_ack()` (run_agent.py:2110).
    /// Detects a planning/ack message that should continue instead of ending the turn.
    #[allow(dead_code)]
    fn looks_like_codex_intermediate_ack(
        user_message: &str,
        assistant_content: &str,
        messages: &[Message],
    ) -> bool {
        // Don't trigger if any tool result are present.
        if messages.iter().any(|msg| msg.get("role").and_then(Value::as_str) == Some("tool")) {
            return false;
        }

        let assistant_text = Self::strip_think_blocks(assistant_content);
        let assistant_text = assistant_text.trim().to_lowercase();
        if assistant_text.is_empty() {
            return false;
        }
        if assistant_text.len() > 1200 {
            return false;
        }

        // Check for future acknowledgment: "i'll", "i will", "let me", etc.
        let has_future_ack = assistant_text.contains("i'll")
            || assistant_text.contains("i' ll")
            || assistant_text.contains("i will")
            || assistant_text.contains("let me")
            || assistant_text.contains("i can do that")
            || assistant_text.contains("i can help with that");
        if !has_future_ack {
            return false;
        }

        // Check for action markers.
        let action_markers = [
            "look into", "look at", "inspect", "check", "verify",
            "find", "search", "read", "try", "see if",
        ];
        let has_action = action_markers.iter().any(|&m| assistant_text.contains(m));
        if !has_action {
            return false;
        }

        // Don't continue on "I can help with that" without an action plan.
        if assistant_text.contains("i can help with that") && !has_action {
            return false;
        }

        // Must be a single user message (or at least the last one matters).
        let _ = user_message; // consumed by caller context

        true
    }

    /// Check if content was already streamed via stream callbacks.
    ///
    /// Mirrors Python `_interim_content_was_streamed()` (run_agent.py:5117).
    #[allow(dead_code)]
    fn interim_content_was_streamed(&self, content: &str) -> bool {
        let visible = Self::normalize_interim_visible_text(&Self::strip_think_blocks(content));
        if visible.is_empty() {
            return false;
        }
        let streamed = {
            let tracked = self.current_streamed_assistant_text.lock();
            Self::normalize_interim_visible_text(&Self::strip_think_blocks(&tracked))
        };
        !streamed.is_empty() && streamed == visible
    }

    /// Emit an interim assistant message to the UI layer.
    ///
    /// Mirrors Python `_emit_interim_assistant_message()` (run_agent.py:5128).
    /// Surfaces real mid-turn assistant commentary, stripping thinking blocks.
    #[allow(dead_code)]
    fn emit_interim_assistant_message(&self, assistant_msg: &Value) {
        let Some(ref cb) = self.interim_assistant_callback else {
            return;
        };
        let Some(content) = assistant_msg.get("content").and_then(Value::as_str) else {
            return;
        };
        let visible = Self::strip_think_blocks(content);
        let visible = visible.trim();
        if visible.is_empty() || visible == "(empty)" {
            return;
        }
        let already_streamed = self.interim_content_was_streamed(visible);
        cb(visible, already_streamed);
    }

    /// Mark that the next stream delta should be preceded by a paragraph break.
    ///
    /// Called when a tool iteration completes and more text will follow,
    /// preventing text concatenation across tool boundaries.
    /// Mirrors Python `_stream_needs_break = True`.
    #[allow(dead_code)]
    fn mark_stream_break_needed(&self) {
        let mut needs_break = self.stream_needs_break.lock();
        *needs_break = true;
    }

    /// Emit context pressure warning to gateway.
    ///
    /// Mirrors Python `_emit_context_pressure()` (run_agent.py:7917).
    /// Notifies the user that context is approaching the compaction threshold.
    fn emit_context_pressure(&self, approx_tokens: usize, threshold_tokens: usize) {
        let Some(ref cb) = self.status_callback else {
            return;
        };
        let threshold_pct = if threshold_tokens > 0 {
            approx_tokens * 100 / threshold_tokens
        } else {
            0
        };
        let msg = format!(
            "⚠️ Context pressure: ~{approx_tokens} tokens ({threshold_pct}% of threshold). Compression will trigger at {threshold_tokens} tokens."
        );
        cb("context_pressure", &msg);
    }

    /// Spawn a fire-and-forget background review agent.
    ///
    /// Mirrors Python `_spawn_background_review()`: creates a separate task
    /// that reviews the just-completed conversation and creates/updates
    /// memories or skills if warranted. Never blocks the main conversation.
    fn spawn_background_review(
        &self,
        messages: &[Message],
        review_memory: bool,
        review_skills: bool,
    ) {
        let config = self.config.clone();
        let registry = Arc::clone(&self.tool_registry);
        let history = messages.to_vec();
        let prompt = if review_memory && review_skills {
            crate::self_evolution::COMBINED_REVIEW_PROMPT.to_string()
        } else if review_memory {
            crate::self_evolution::MEMORY_REVIEW_PROMPT.to_string()
        } else {
            crate::self_evolution::SKILL_REVIEW_PROMPT.to_string()
        };

        tokio::spawn(async move {
            if let Err(e) = crate::review_agent::run_review(
                config, registry, history, prompt,
                review_memory, review_skills,
            ).await {
                tracing::warn!("Self-evolution review failed: {e}");
            }
        });
    }

    /// Release all resources held by this agent instance.
    ///
    /// Cleans up:
    /// - Signals running child agents to stop via interrupt flag
    /// - Clears pending delegate results
    ///
    /// Safe to call multiple times (idempotent).
    /// Each cleanup step is independently guarded.
    pub fn close(&mut self) {
        // 1. Signal child agents to stop (mirrors Python: kill_all, cleanup_vm, cleanup_browser)
        self.interrupt.store(true, std::sync::atomic::Ordering::SeqCst);

        // 2. Clear pending delegate results (mirrors Python: close active child agents)
        {
            let mut guard = self.delegate_results.lock();
            guard.clear();
        }

        // Note: Rust doesn't hold persistent HTTP clients or subprocess handles
        // at the agent level — those are per-request/per-call in the Rust architecture.
        // This matches Python's close() intent without needing explicit teardown.

        tracing::debug!(
            "Agent closed: session_id={:?}",
            self.config.session_id
        );
    }

    /// Deduplicate tool calls by (name, arguments) within a single turn.
    ///
    /// Mirrors Python `_deduplicate_tool_calls()` (run_agent.py:3573).
    /// Weak models sometimes emit duplicate tool calls with identical
    /// name and arguments — executing them twice is wasteful and can
    /// cause side effects (e.g., double file writes).
    fn deduplicate_tool_calls(tool_calls: &[Value]) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut unique = Vec::new();

        for tc in tool_calls {
            let key = format!(
                "{}:{}",
                tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or(""),
            );
            if seen.insert(key) {
                unique.push(tc.clone());
            } else {
                let dup_name = tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                tracing::warn!("Removed duplicate tool call: {dup_name}");
            }
        }

        unique
    }

    /// Attempt to repair a mismatched tool name.
    ///
    /// Mirrors Python `_repair_tool_call()` (run_agent.py:3590).
    /// Tries: lowercase → normalized (hyphens/spaces to underscores) →
    /// fuzzy match via Levenshtein distance (cutoff 0.7).
    fn repair_tool_call(tool_name: &str, valid_names: &[String]) -> Option<String> {
        // 1. Lowercase
        let lowered = tool_name.to_lowercase();
        if valid_names.contains(&lowered) {
            return Some(lowered);
        }

        // 2. Normalized
        let normalized = lowered.replace(['-', ' '], "_");
        if valid_names.contains(&normalized) {
            return Some(normalized);
        }

        // 3. Fuzzy match — Levenshtein distance
        let cutoff = 0.7;
        let mut best_match: Option<(f64, &String)> = None;
        for valid in valid_names {
            let dist = Self::levenshtein(tool_name, valid);
            let max_len = tool_name.len().max(valid.len());
            if max_len == 0 {
                continue;
            }
            let similarity = 1.0 - (dist as f64 / max_len as f64);
            if similarity >= cutoff {
                match best_match {
                    None => best_match = Some((similarity, valid)),
                    Some((best_sim, _)) => {
                        if similarity > best_sim {
                            best_match = Some((similarity, valid));
                        }
                    }
                }
            }
        }

        best_match.map(|(_, name)| name.clone())
    }

    /// Compute Levenshtein distance between two strings.
    fn levenshtein(a: &str, b: &str) -> usize {
        let a_chars: Vec<char> = a.chars().collect();
        let b_chars: Vec<char> = b.chars().collect();
        let m = a_chars.len();
        let n = b_chars.len();

        if m == 0 { return n; }
        if n == 0 { return m; }

        let mut dp = vec![vec![0usize; n + 1]; m + 1];
        #[allow(clippy::needless_range_loop)]
        for i in 0..=m { dp[i][0] = i; }
        #[allow(clippy::needless_range_loop)]
        for j in 0..=n { dp[0][j] = j; }

        for i in 1..=m {
            for j in 1..=n {
                let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
                dp[i][j] = (dp[i - 1][j] + 1)
                    .min(dp[i][j - 1] + 1)
                    .min(dp[i - 1][j - 1] + cost);
            }
        }

        dp[m][n]
    }

    /// Execute a single tool call and return the result.
    async fn execute_tool_call(&mut self, tool_call: &Value) -> Value {
        let tool_name = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        let tool_call_id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");

        let arguments = tool_call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("{}");

        let args: std::result::Result<Value, _> = serde_json::from_str(arguments);
        let args = match args {
            Ok(v) => v,
            Err(e) => {
                return serde_json::json!({
                    "role": "tool",
                    "content": format!("Invalid JSON arguments for {}: {}", tool_name, e),
                    "tool_call_id": tool_call_id
                });
            }
        };

        // Intercept delegate_task and route through SubagentManager.
        // We handle this at the tool-call level but outside the main conversation
        // loop to avoid circular async dependencies between modules.
        if tool_name == "delegate_task" {
            if let Some(ref mgr) = self.subagent_mgr {
                let mgr = Arc::clone(mgr);
                let registry = Arc::clone(&self.tool_registry);
                let args_clone = args.clone();
                // Use a separate async boundary to break the type-level cycle.
                // The spawned task has its own Send requirement that doesn't
                // feed back into execute_tool_call's future type.
                let rx = dispatch_delegation(mgr, registry, args_clone);
                let results = rx.await.unwrap_or_default();
                self.store_delegate_results(results);
                return serde_json::json!({
                    "role": "tool",
                    "content": "Subagent tasks dispatched. Results will be provided after the next LLM call.",
                    "tool_call_id": tool_call_id
                });
            }
            // Child agents don't have subagent_mgr — fall through to regular dispatch
        }

        tracing::info!(
            "Executing tool: {} (id: {})",
            tool_name,
            tool_call_id
        );

        // Signal activity to prevent gateway inactivity timeout
        if let Some(ref cb) = self.activity_callback {
            cb(&format!("calling tool: {tool_name}"));
        }

        // Self-evolution: reset nudge counters on relevant tool use
        if tool_name == "memory" {
            self.turns_since_memory = 0;
        } else if tool_name == "skill_manage" {
            self.iters_since_skill = 0;
        }

        // Dispatch through the tool registry
        match self.tool_registry.dispatch(tool_name, args.clone()) {
            Ok(result) => {
                serde_json::json!({
                    "role": "tool",
                    "content": result,
                    "tool_call_id": tool_call_id
                })
            }
            Err(e) => {
                // Attempt to repair mismatched tool name before returning error.
                // Mirrors Python `_repair_tool_call()` (run_agent.py:3590).
                let valid_names: Vec<String> = self
                    .tool_registry
                    .get_definitions(None)
                    .into_iter()
                    .filter_map(|s| {
                        s.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect();

                if let Some(repaired) = Self::repair_tool_call(tool_name, &valid_names) {
                    tracing::warn!(
                        "Tool name repair: {} → {}", tool_name, repaired
                    );
                    match self.tool_registry.dispatch(&repaired, args) {
                        Ok(result) => {
                            return serde_json::json!({
                                "role": "tool",
                                "content": result,
                                "tool_call_id": tool_call_id
                            });
                        }
                        Err(e2) => {
                            return serde_json::json!({
                                "role": "tool",
                                "content": format!(
                                    "Error executing tool {} (tried repair to '{}'): {}. {}",
                                    tool_name, repaired, e, e2
                                ),
                                "tool_call_id": tool_call_id
                            });
                        }
                    }
                }

                serde_json::json!({
                    "role": "tool",
                    "content": format!("Error executing tool {}: {}", tool_name, e),
                    "tool_call_id": tool_call_id
                })
            }
        }
    }

    // ── Concurrent tool execution ─────────────────────────────────────────

    /// Decide whether a batch of tool calls is safe to run in parallel.
    ///
    /// Mirrors Python `_should_parallelize_tool_batch()` (run_agent.py:267).
    ///
    /// Rules:
    /// - Single tool: no point in parallelism (returns false)
    /// - Any tool in `_NEVER_PARALLEL_TOOLS`: sequential (e.g., `clarify`)
    /// - Path-scoped tools (`read_file`, `write_file`, `patch`): safe only
    ///   when their target paths don't overlap (no same-file read+write)
    /// - Other tools: must be in `_PARALLEL_SAFE_TOOLS` (read-only set)
    /// - Invalid JSON args: fall back to sequential
    fn should_parallelize_tool_batch(tool_calls: &[Value]) -> bool {
        if tool_calls.len() <= 1 {
            return false;
        }

        // Check for never-parallel tools
        for tc in tool_calls {
            if let Some(name) = tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                if NEVER_PARALLEL_TOOLS.contains(name) {
                    return false;
                }
            }
        }

        // Collect reserved paths for path-scoped overlap detection
        let mut reserved_paths: Vec<std::path::PathBuf> = Vec::new();

        for tc in tool_calls {
            let name = tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");

            // Parse arguments
            let args_str = tc.get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str);

            let args_str = match args_str {
                Some(s) => s,
                None => {
                    tracing::debug!("No arguments for tool '{}' — defaulting to sequential", name);
                    return false;
                }
            };

            let args: Value = match serde_json::from_str(args_str) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(
                        "Invalid JSON args for '{}' ({}) — defaulting to sequential",
                        name, e
                    );
                    return false;
                }
            };

            if !args.is_object() {
                tracing::debug!("Non-object args for '{}' — defaulting to sequential", name);
                return false;
            }

            if PATH_SCOPED_TOOLS.contains(name) {
                // Extract the path scope
                let scoped_path = Self::extract_parallel_scope_path(name, &args);
                match scoped_path {
                    Some(path) => {
                        // Check overlap with existing reserved paths
                        for existing in &reserved_paths {
                            if Self::paths_overlap(&path, existing) {
                                return false;
                            }
                        }
                        reserved_paths.push(path);
                    }
                    None => {
                        // Couldn't extract path — play it safe
                        return false;
                    }
                }
                continue;
            }

            if !PARALLEL_SAFE_TOOLS.contains(name) {
                // Unknown tool — default to sequential
                return false;
            }
        }

        true
    }

    /// Extract the normalized file target for path-scoped tools.
    ///
    /// Mirrors Python `_extract_parallel_scope_path()` (run_agent.py:311).
    fn extract_parallel_scope_path(tool_name: &str, args: &Value) -> Option<std::path::PathBuf> {
        if !PATH_SCOPED_TOOLS.contains(tool_name) {
            return None;
        }

        let raw_path = args.get("path").and_then(Value::as_str)?;
        if raw_path.trim().is_empty() {
            return None;
        }

        // Expand ~ and resolve to absolute path
        let expanded = shellexpand::tilde(raw_path);
        let path = std::path::Path::new(expanded.as_ref());

        if path.is_absolute() {
            // Use canonicalize if file exists, otherwise just use the absolute path
            if path.exists() {
                path.canonicalize().ok()
            } else {
                Some(path.to_path_buf())
            }
        } else {
            // Prepend current directory to make it absolute
            std::env::current_dir().ok().map(|cwd| cwd.join(path))
        }
    }

    /// Check if two paths may refer to the same subtree.
    ///
    /// Mirrors Python `_paths_overlap()` (run_agent.py:328).
    fn paths_overlap(a: &std::path::PathBuf, b: &std::path::PathBuf) -> bool {
        // Exact match
        if a == b {
            return true;
        }

        // Component-wise: check if one is a prefix of the other
        let a_components: Vec<_> = a.components().collect();
        let b_components: Vec<_> = b.components().collect();

        let min_len = a_components.len().min(b_components.len());
        if min_len == 0 {
            return false;
        }

        // Check common prefix — if they share the same prefix up to the
        // shorter path's length, they could be the same file or parent/child
        a_components[..min_len] == b_components[..min_len]
    }

    /// Execute multiple tool calls sequentially with richer display output.
    ///
    /// Mirrors Python `_execute_tool_calls_sequential()` (run_agent.py:7536).
    /// Each tool runs one at a time, with per-tool logging, interrupt checks,
    /// and nudge counter resets.
    async fn execute_tool_calls_sequential(
        &mut self,
        tool_calls: &[Value],
    ) -> Vec<Value> {
        let mut results = Vec::with_capacity(tool_calls.len());
        let num_tools = tool_calls.len();

        for (i, tc) in tool_calls.iter().enumerate() {
            // Interrupt check before each tool
            if self.is_interrupted() {
                let remaining = num_tools - i;
                if remaining > 0 {
                    tracing::info!(
                        "Interrupt: skipping {} remaining tool call(s)",
                        remaining
                    );
                }
                // Add cancellation messages for skipped tools
                for skipped_tc in tool_calls.iter().skip(i) {
                    let skip_name = skipped_tc.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let skip_id = skipped_tc.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    results.push(serde_json::json!({
                        "role": "tool",
                        "content": format!(
                            "[Tool execution cancelled — {} was skipped due to user interrupt]",
                            skip_name
                        ),
                        "tool_call_id": skip_id
                    }));
                }
                break;
            }

            let tool_name = tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let args_str = tc.get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");

            // Log tool invocation
            tracing::info!(
                "Executing tool {}/{}: {} (args: {})",
                i + 1, num_tools, tool_name,
                &args_str.chars().take(120).collect::<String>()
            );

            // Self-evolution: reset nudge counters on relevant tool use
            if tool_name == "memory" {
                self.turns_since_memory = 0;
            } else if tool_name == "skill_manage" {
                self.iters_since_skill = 0;
            }

            // Signal activity
            if let Some(ref cb) = self.activity_callback {
                cb(&format!("calling tool: {tool_name}"));
            }

            let tool_start = std::time::Instant::now();

            // Execute the tool
            let tool_result = self.execute_tool_call(tc).await;

            let duration = tool_start.elapsed();

            // Log completion
            let content_len = tool_result.get("content")
                .and_then(Value::as_str)
                .map(|s| s.len())
                .unwrap_or(0);

            tracing::info!(
                "Tool {} completed in {:.2}s ({} chars output)",
                tool_name,
                duration.as_secs_f64(),
                content_len
            );

            results.push(tool_result);
        }

        results
    }

    /// Execute multiple tool calls concurrently using tokio tasks.
    ///
    /// Mirrors Python `_execute_tool_calls_concurrent()` (run_agent.py:7298).
    /// Independent tool calls (read-only, non-overlapping paths) are spawned
    /// as separate tokio tasks and results are collected in original order.
    async fn execute_tool_calls_concurrent(
        &mut self,
        tool_calls: &[Value],
    ) -> Vec<Value> {
        let num_tools = tool_calls.len();

        // Pre-flight: interrupt check
        if self.is_interrupted() {
            tracing::info!("Interrupt: skipping {} tool call(s)", num_tools);
            return tool_calls.iter().map(|tc| {
                let name = tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let id = tc.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                serde_json::json!({
                    "role": "tool",
                    "content": format!("[Tool execution cancelled — {} was skipped due to user interrupt]", name),
                    "tool_call_id": id
                })
            }).collect()
        }

        // Parse and log all calls up front
        let tool_names: Vec<String> = tool_calls.iter()
            .filter_map(|tc| tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from))
            .collect();

        let tool_names_str = tool_names.join(", ");
        tracing::info!(
            "Concurrent: {} tool calls — {}",
            num_tools, tool_names_str
        );

        // Reset nudge counters for all tools
        for name in &tool_names {
            if name == "memory" {
                self.turns_since_memory = 0;
            } else if name == "skill_manage" {
                self.iters_since_skill = 0;
            }
        }

        // Signal activity
        if let Some(ref cb) = self.activity_callback {
            cb(&format!("executing {} tools concurrently: {}", num_tools, tool_names_str));
        }

        // Clone tool calls for spawning (we don't have &mut self across tasks)
        let tool_calls_clone: Vec<Value> = tool_calls.to_vec();

        // Spawn concurrent tasks.
        // We use `futures::future::join_all` which runs them concurrently
        // and collects results in order.
        let registry = Arc::clone(&self.tool_registry);
        let subagent_mgr = self.subagent_mgr.clone();

        let tasks: Vec<_> = tool_calls_clone.iter().enumerate().map(|(index, tc)| {
            let registry = Arc::clone(&registry);
            let subagent_mgr = subagent_mgr.clone();
            async move {
                let tool_name = tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let tool_call_id = tc.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = tc.get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("{}");

                let args: std::result::Result<Value, _> = serde_json::from_str(arguments);
                let args = match args {
                    Ok(v) => v,
                    Err(e) => {
                        return (index, serde_json::json!({
                            "role": "tool",
                            "content": format!("Invalid JSON arguments for {}: {}", tool_name, e),
                            "tool_call_id": tool_call_id
                        }));
                    }
                };

                // Handle delegate_task specially (same as execute_tool_call)
                if tool_name == "delegate_task" {
                    if let Some(ref mgr) = subagent_mgr {
                        let mgr = Arc::clone(mgr);
                        let registry = Arc::clone(&registry);
                        let args_clone = args.clone();
                        let rx = dispatch_delegation(mgr, registry, args_clone);
                        let _results = rx.await.unwrap_or_default();
                        return (index, serde_json::json!({
                            "role": "tool",
                            "content": "Subagent tasks dispatched. Results will be provided after the next LLM call.",
                            "tool_call_id": tool_call_id
                        }));
                    }
                }

                // Dispatch through the tool registry
                let tool_start = std::time::Instant::now();
                let result = match registry.dispatch(tool_name, args) {
                    Ok(output) => serde_json::json!({
                        "role": "tool",
                        "content": output,
                        "tool_call_id": tool_call_id
                    }),
                    Err(e) => serde_json::json!({
                        "role": "tool",
                        "content": format!("Error executing tool {}: {}", tool_name, e),
                        "tool_call_id": tool_call_id
                    }),
                };

                let duration = tool_start.elapsed();
                tracing::info!(
                    "Tool {} completed in {:.2}s (concurrent)",
                    tool_name,
                    duration.as_secs_f64()
                );

                (index, result)
            }
        }).collect();

        // Run all tasks concurrently and collect results
        let mut indexed_results: Vec<(usize, Value)> = futures::future::join_all(tasks).await;

        // Sort by original index to maintain order
        indexed_results.sort_by_key(|(idx, _)| *idx);

        // Extract just the result values
        indexed_results.into_iter().map(|(_, result)| result).collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.model, "anthropic/claude-opus-4-6");
        assert_eq!(config.max_iterations, 90);
        assert!(config.enable_caching);
        assert!(!config.compression_enabled);
    }

    #[test]
    fn test_iteration_budget_shared() {
        let budget = Arc::new(IterationBudget::new(5));
        assert_eq!(budget.remaining(), 5);

        // Simulate consuming budget
        for _ in 0..5 {
            budget.consume();
        }
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn test_build_system_prompt() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let prompt = agent.build_system_prompt(None);
        assert!(!prompt.is_empty());
        // Should contain the default agent identity
        assert!(prompt.contains("Hermes Agent") || prompt.contains("You are"));
    }

    #[test]
    fn test_build_system_prompt_cached() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let first = agent.build_system_prompt(None);
        let second = agent.build_system_prompt(Some("different"));
        // Second call should return cached version (ignores new system_message)
        assert_eq!(first, second);
    }

    #[test]
    fn test_agent_config_custom() {
        let config = AgentConfig {
            model: "openai/gpt-4".to_string(),
            provider: Some("openai".to_string()),
            base_url: Some("http://custom.api".to_string()),
            api_key: Some("sk-test".to_string()),
            api_mode: Some("openai".to_string()),
            max_iterations: 30,
            skip_context_files: true,
            platform: Some("telegram".to_string()),
            session_id: Some("sess-123".to_string()),
            enable_caching: false,
            compression_enabled: true,
            compression_config: None,
            context_engine_name: None,
            terminal_cwd: Some(std::path::PathBuf::from("/tmp")),
            ephemeral_system_prompt: Some("override".to_string()),
            memory_nudge_interval: 5,
            skill_nudge_interval: 5,
            memory_flush_min_turns: 3,
            self_evolution_enabled: true,
            credential_pool: None,
            provider_preferences: None,
            fallback_providers: Vec::new(),
            session_db: None,
            persist_session: true,
        };
        assert_eq!(config.model, "openai/gpt-4");
        assert_eq!(config.max_iterations, 30);
        assert!(!config.enable_caching);
        assert!(config.compression_enabled);
        assert!(config.skip_context_files);
    }

    #[test]
    fn test_agent_creation() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        assert_eq!(agent.budget.max_total, 90);
        assert!(agent.subagent_mgr.is_some());
        // Nudge counters start at 0
        assert_eq!(agent.turns_since_memory, 0);
        assert_eq!(agent.iters_since_skill, 0);
    }

    #[test]
    fn test_self_evolution_defaults() {
        let config = AgentConfig::default();
        assert_eq!(config.memory_nudge_interval, 10);
        assert_eq!(config.skill_nudge_interval, 10);
        assert_eq!(config.memory_flush_min_turns, 6);
        assert!(config.self_evolution_enabled);
    }

    #[test]
    fn test_agent_with_depth_zero_has_manager() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::with_depth(config, registry, 0).unwrap();
        assert!(agent.subagent_mgr.is_some());
    }

    #[test]
    fn test_agent_with_depth_nonzero_no_manager() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::with_depth(config, registry, 1).unwrap();
        assert!(agent.subagent_mgr.is_none());
    }

    #[test]
    fn test_take_delegate_results_empty() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        let results = agent.take_delegate_results();
        assert!(results.is_none());
    }

    #[test]
    fn test_store_and_take_delegate_results() {
        use crate::subagent::SubagentResult;

        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();

        agent.store_delegate_results(vec![SubagentResult {
            goal: "test".to_string(),
            response: "done".to_string(),
            exit_reason: "completed".to_string(),
            api_calls: 3,
        }]);

        let results = agent.take_delegate_results();
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].goal, "test");

        // Second take should return None (cleared)
        let results2 = agent.take_delegate_results();
        assert!(results2.is_none());
    }

    #[tokio::test]
    async fn test_execute_tool_call_unknown_tool() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let tool_call = serde_json::json!({
            "id": "call_123",
            "function": {
                "name": "nonexistent_tool",
                "arguments": "{}"
            }
        });

        let result = agent.execute_tool_call(&tool_call).await;
        assert_eq!(result["role"], "tool");
        assert!(result["content"].as_str().unwrap().contains("Error executing tool"));
        assert_eq!(result["tool_call_id"], "call_123");
    }

    #[tokio::test]
    async fn test_execute_tool_call_invalid_json_args() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let tool_call = serde_json::json!({
            "id": "call_456",
            "function": {
                "name": "todo",
                "arguments": "{invalid json"
            }
        });

        let result = agent.execute_tool_call(&tool_call).await;
        assert_eq!(result["role"], "tool");
        assert!(result["content"].as_str().unwrap().contains("Invalid JSON"));
        assert_eq!(result["tool_call_id"], "call_456");
    }

    #[tokio::test]
    async fn test_execute_tool_call_empty_args() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let tool_call = serde_json::json!({
            "id": "call_789",
            "function": {
                "name": "todo",
                "arguments": ""
            }
        });

        let result = agent.execute_tool_call(&tool_call).await;
        // Empty string is invalid JSON, should return error
        assert_eq!(result["role"], "tool");
        assert!(result["content"].as_str().unwrap().contains("Invalid JSON"));
    }

    #[tokio::test]
    async fn test_execute_tool_call_missing_function() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let tool_call = serde_json::json!({
            "id": "call_missing"
        });

        let result = agent.execute_tool_call(&tool_call).await;
        assert_eq!(result["role"], "tool");
        // name defaults to "unknown"
        assert!(result["content"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config.clone(), registry.clone()).unwrap();

        let prompt = agent.build_system_prompt(None);
        assert!(!prompt.is_empty());

        // Add a tool and check that prompt gets rebuilt (cache invalidated)
        // Note: cache is only invalidated when compression happens, so this
        // verifies the cached path
        let prompt2 = agent.build_system_prompt(None);
        assert_eq!(prompt, prompt2); // Same due to caching
    }

    #[test]
    fn test_turn_result_fields() {
        let result = TurnResult {
            response: "hello".to_string(),
            messages: vec![Arc::new(serde_json::json!({"role": "user", "content": "hi"}))],
            api_calls: 1,
            exit_reason: "completed".to_string(),
            compression_exhausted: false,
            usage: None,
        };
        assert_eq!(result.response, "hello");
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.api_calls, 1);
        assert_eq!(result.exit_reason, "completed");
        assert!(!result.compression_exhausted);
    }

    #[test]
    fn test_agent_config_all_defaults_explicit() {
        let config = AgentConfig::default();
        assert_eq!(config.model, "anthropic/claude-opus-4-6");
        assert!(config.provider.is_none());
        assert!(config.base_url.is_none());
        assert!(config.api_key.is_none());
        assert!(config.api_mode.is_none());
        assert_eq!(config.max_iterations, 90);
        assert!(!config.skip_context_files);
        assert!(config.platform.is_none());
        assert!(config.session_id.is_none());
        assert!(config.enable_caching);
        assert!(!config.compression_enabled);
        assert!(config.compression_config.is_none());
        assert!(config.terminal_cwd.is_none());
        assert!(config.ephemeral_system_prompt.is_none());
    }

    #[test]
    fn test_close_sets_interrupt() {
        use std::sync::atomic::Ordering;

        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        // Interrupt should be false initially
        assert!(!agent.interrupt.load(Ordering::SeqCst));

        // Close should set it
        agent.close();
        assert!(agent.interrupt.load(Ordering::SeqCst));
    }

    #[test]
    fn test_close_idempotent() {
        use std::sync::atomic::Ordering;

        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        agent.close();
        agent.close(); // Second call should not panic
        assert!(agent.interrupt.load(Ordering::SeqCst));
    }

    #[test]
    fn test_close_clears_delegate_results() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        agent.store_delegate_results(vec![SubagentResult {
            goal: "test".to_string(),
            response: "done".to_string(),
            exit_reason: "completed".to_string(),
            api_calls: 1,
        }]);
        assert!(agent.take_delegate_results().is_some());

        // Store again and close
        agent.store_delegate_results(vec![SubagentResult {
            goal: "test2".to_string(),
            response: "done2".to_string(),
            exit_reason: "completed".to_string(),
            api_calls: 2,
        }]);
        agent.close();

        // After close, delegate results should be cleared
        assert!(agent.take_delegate_results().is_none());
    }

    #[test]
    fn test_has_truncated_tool_args_valid_json() {
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "todo",
                "arguments": "{\"action\": \"view\"}"
            }
        })];
        assert!(!has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_has_truncated_tool_args_truncated() {
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "todo",
                "arguments": "{\"action\": \"vie"
            }
        })];
        assert!(has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_has_truncated_tool_args_empty() {
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "todo",
                "arguments": ""
            }
        })];
        // Empty args is not truncated (treated as no args)
        assert!(!has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_has_truncated_tool_args_no_function() {
        let tool_calls = vec![serde_json::json!({
            "id": "call_1"
        })];
        assert!(!has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_has_truncated_tool_args_multiple_one_truncated() {
        let tool_calls = vec![
            serde_json::json!({
                "id": "call_1",
                "function": {
                    "name": "todo",
                    "arguments": "{\"action\": \"view\"}"
                }
            }),
            serde_json::json!({
                "id": "call_2",
                "function": {
                    "name": "file_ops",
                    "arguments": "{\"path\": \"/hom"
                }
            }),
        ];
        assert!(has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_has_truncated_tool_args_ends_with_bracket_invalid_json() {
        // Ends with } but inner structure is broken
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "todo",
                "arguments": "\"key\": \"value\"}"
            }
        })];
        assert!(has_truncated_tool_args(&tool_calls));
    }

    #[test]
    fn test_rollback_to_last_assistant() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi there"})),
            Arc::new(serde_json::json!({"role": "user", "content": "follow up"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "partial response", "tool_calls": []})),
        ];
        let rolled_back = rollback_to_last_assistant(&messages);
        // Should keep everything before the last assistant message
        assert_eq!(rolled_back.len(), 3);
        assert_eq!(rolled_back[0]["content"], "hello");
        assert_eq!(rolled_back[1]["content"], "hi there");
        assert_eq!(rolled_back[2]["content"], "follow up");
    }

    #[test]
    fn test_rollback_no_assistant() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
        ];
        let rolled_back = rollback_to_last_assistant(&messages);
        // No assistant message — return original
        assert_eq!(rolled_back.len(), 1);
    }

    #[test]
    fn test_rollback_empty_messages() {
        let messages: Vec<Message> = vec![];
        let rolled_back = rollback_to_last_assistant(&messages);
        assert!(rolled_back.is_empty());
    }

    #[test]
    fn test_rollback_single_assistant() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "done"})),
        ];
        let rolled_back = rollback_to_last_assistant(&messages);
        // Only one assistant — rollback removes it, keeping just the user msg
        assert_eq!(rolled_back.len(), 1);
        assert_eq!(rolled_back[0]["content"], "hello");
    }

    #[test]
    fn test_has_think_tags_thonking() {
        assert!(has_think_tags("<think>Let me think"));
        assert!(has_think_tags("Some text\n</think>\nresponse"));
    }

    #[test]
    fn test_has_think_tags_thinking() {
        assert!(has_think_tags("<thinking>I need to analyze</thinking>"));
    }

    #[test]
    fn test_has_think_tags_reasoning() {
        assert!(has_think_tags("<reasoning>Step 1: parse input</reasoning>"));
    }

    #[test]
    fn test_has_think_tags_no_tags() {
        // Non-reasoning models (GLM, MiniMax) don't produce think tags
        assert!(!has_think_tags("Hello! How can I help you?"));
        assert!(!has_think_tags(""));
        assert!(!has_think_tags("Some text with <b>html</b> tags"));
    }

    #[test]
    fn test_has_think_tags_mixed_content() {
        // Tags embedded in larger response
        assert!(has_think_tags("<think>\nThe answer is 42\n</think>\nThe answer is 42."));
        assert!(has_think_tags("Let me reason... <thinking>analysis</thinking> done."));
    }

    #[tokio::test]
    async fn test_activity_callback_invoked() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        agent.set_activity_callback(move |_msg| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Execute a tool — callback should fire
        let tool_call = serde_json::json!({
            "id": "call_cb",
            "function": {
                "name": "todo",
                "arguments": "{}"
            }
        });
        let _ = agent.execute_tool_call(&tool_call).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_no_activity_callback_none() {
        // Without a callback, tool execution should not panic
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        let tool_call = serde_json::json!({
            "id": "call_nocb",
            "function": {
                "name": "todo",
                "arguments": "{}"
            }
        });
        let result = agent.execute_tool_call(&tool_call).await;
        assert_eq!(result["role"], "tool");
    }

    // ── Stale-call timeout tests ──────────────────────────────────────

    #[test]
    fn test_is_local_endpoint() {
        assert!(is_local_endpoint("http://localhost:8080"));
        assert!(is_local_endpoint("http://127.0.0.1:11434"));
        assert!(is_local_endpoint("http://0.0.0.0:8000"));
        assert!(is_local_endpoint("https://127.0.0.1/v1"));
        assert!(!is_local_endpoint("https://api.openai.com/v1"));
        assert!(!is_local_endpoint("https://api.openrouter.ai/v1"));
        // http://local.something should NOT match anymore (too broad)
        assert!(!is_local_endpoint("http://local.example.com/v1"));
    }

    #[test]
    fn test_estimate_tokens() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi there"})),
        ];
        let tokens = estimate_tokens(&messages);
        // Now counts role strings too: "user" + "hello" + "assistant" + "hi there"
        // = ~23 chars / 4 ≈ 5 tokens, so range is wider
        assert!(tokens >= 4 && tokens <= 10);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        let messages: Vec<Message> = vec![];
        assert_eq!(estimate_tokens(&messages), 0);
    }

    // Serialize env-var tests to avoid cross-test contamination.
    static STALE_TIMEOUT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_stale_call_timeout_default() {
        let _guard = STALE_TIMEOUT_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_API_CALL_STALE_TIMEOUT");
        let timeout = stale_call_timeout(None, &[]);
        assert_eq!(timeout, std::time::Duration::from_secs_f64(300.0));
    }

    #[test]
    fn test_stale_call_timeout_local_disabled() {
        let _guard = STALE_TIMEOUT_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_API_CALL_STALE_TIMEOUT");
        let timeout = stale_call_timeout(Some("http://localhost:8080"), &[]);
        assert_eq!(timeout, std::time::Duration::from_secs(u64::MAX));
    }

    #[test]
    fn test_stale_call_timeout_large_context() {
        let _guard = STALE_TIMEOUT_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_API_CALL_STALE_TIMEOUT");
        // Simulate >100K tokens (chars/4 heuristic → need >400K chars)
        let large_content = "x".repeat(440_000);
        let messages = vec![Arc::new(serde_json::json!({"role": "user", "content": large_content}))];
        let timeout = stale_call_timeout(Some("https://api.openai.com/v1"), &messages);
        assert_eq!(timeout, std::time::Duration::from_secs_f64(600.0));
    }

    #[test]
    fn test_stale_call_timeout_mid_context() {
        let _guard = STALE_TIMEOUT_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_API_CALL_STALE_TIMEOUT");
        // Simulate >50K tokens but <100K (200K-400K chars)
        let content = "x".repeat(240_000);
        let messages = vec![Arc::new(serde_json::json!({"role": "user", "content": content}))];
        let timeout = stale_call_timeout(Some("https://api.openai.com/v1"), &messages);
        assert_eq!(timeout, std::time::Duration::from_secs_f64(450.0));
    }

    #[test]
    fn test_stale_call_timeout_env_override() {
        let _guard = STALE_TIMEOUT_LOCK.lock().unwrap();
        std::env::set_var("HERMES_API_CALL_STALE_TIMEOUT", "60");
        let timeout = stale_call_timeout(None, &[]);
        assert_eq!(timeout, std::time::Duration::from_secs_f64(60.0));
        std::env::remove_var("HERMES_API_CALL_STALE_TIMEOUT");
    }

    // ── Failure hint tests ────────────────────────────────────────────

    #[test]
    fn test_failure_hint_524() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "openrouter", "gpt-4", Some(524), "A timeout occurred");
        let hint = build_failure_hint(&classification, 120.0);
        assert!(hint.contains("524"));
        assert!(hint.contains("120s"));
    }

    #[test]
    fn test_failure_hint_429() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "openai", "gpt-4", Some(429), "Rate limit exceeded");
        let hint = build_failure_hint(&classification, 5.0);
        assert!(hint.contains("rate limited"));
        assert!(hint.contains("429"));
    }

    #[test]
    fn test_failure_hint_500() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "openrouter", "gpt-4", Some(500), "Internal server error");
        let hint = build_failure_hint(&classification, 30.0);
        assert!(hint.contains("server error"));
        assert!(hint.contains("500"));
    }

    #[test]
    fn test_failure_hint_no_status_fast() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "unknown", "model", None, "Something went wrong");
        let hint = build_failure_hint(&classification, 3.0);
        assert!(hint.contains("fast response"));
        assert!(hint.contains("likely rate limited"));
    }

    #[test]
    fn test_failure_hint_no_status_slow() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "unknown", "model", None, "Something went wrong");
        let hint = build_failure_hint(&classification, 90.0);
        assert!(hint.contains("slow response"));
        assert!(hint.contains("timeout"));
    }

    #[test]
    fn test_failure_hint_timeout_reason() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "unknown", "model", None, "Request timed out");
        let hint = build_failure_hint(&classification, 15.0);
        assert!(hint.contains("upstream timeout"));
    }

    #[test]
    fn test_failure_hint_billing() {
        let classification = hermes_llm::error_classifier::classify_api_error(
            "openrouter", "model", Some(402), "Insufficient credits");
        let hint = build_failure_hint(&classification, 2.0);
        assert!(hint.contains("billing"));
    }

    // ── New feature tests ──

    #[test]
    fn test_deduplicate_tool_calls_no_dupes() {
        let tool_calls = vec![
            serde_json::json!({
                "id": "call_1",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}
            }),
            serde_json::json!({
                "id": "call_2",
                "function": {"name": "write_file", "arguments": "{\"path\": \"b.rs\"}"}
            }),
        ];
        let deduped = AIAgent::deduplicate_tool_calls(&tool_calls);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn test_deduplicate_tool_calls_removes_dupes() {
        let tool_calls = vec![
            serde_json::json!({
                "id": "call_1",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}
            }),
            serde_json::json!({
                "id": "call_2",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}
            }),
        ];
        let deduped = AIAgent::deduplicate_tool_calls(&tool_calls);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].get("id").and_then(Value::as_str), Some("call_1"));
    }

    #[test]
    fn test_repair_tool_call_lowercase() {
        let valid = vec!["read_file".to_string(), "write_file".to_string()];
        let result = AIAgent::repair_tool_call("READ_FILE", &valid);
        assert_eq!(result, Some("read_file".to_string()));
    }

    #[test]
    fn test_repair_tool_call_normalized() {
        let valid = vec!["read_file".to_string(), "write_file".to_string()];
        let result = AIAgent::repair_tool_call("read-file", &valid);
        assert_eq!(result, Some("read_file".to_string()));
    }

    #[test]
    fn test_repair_tool_call_fuzzy() {
        let valid = vec!["read_file".to_string(), "write_file".to_string()];
        let result = AIAgent::repair_tool_call("reed_file", &valid);
        assert_eq!(result, Some("read_file".to_string()));
    }

    #[test]
    fn test_repair_tool_call_no_match() {
        let valid = vec!["read_file".to_string(), "write_file".to_string()];
        let result = AIAgent::repair_tool_call("completely_unknown", &valid);
        assert!(result.is_none());
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(AIAgent::levenshtein("", ""), 0);
        assert_eq!(AIAgent::levenshtein("kitten", "sitting"), 3);
        assert_eq!(AIAgent::levenshtein("read_file", "reed_file"), 1);
        assert_eq!(AIAgent::levenshtein("abc", "abc"), 0);
        assert_eq!(AIAgent::levenshtein("a", "b"), 1);
    }

    #[test]
    fn test_restore_primary_runtime() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        // Simulate fallback activation
        agent.fallback_activated = true;
        agent.config.model = "openai/gpt-4-fallback".to_string();
        agent.primary_runtime = Some(PrimaryRuntime {
            model: "anthropic/claude-opus-4-6".to_string(),
            base_url: None,
            api_key: None,
            provider: Some("anthropic".to_string()),
        });

        agent.restore_primary_runtime();

        assert!(!agent.fallback_activated);
        assert_eq!(agent.config.model, "anthropic/claude-opus-4-6");
        assert!(agent.primary_runtime.is_none());
    }

    #[test]
    fn test_restore_primary_runtime_no_snapshot() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        agent.fallback_activated = true;
        agent.primary_runtime = None;

        agent.restore_primary_runtime();

        // Should gracefully handle missing snapshot
        assert!(!agent.fallback_activated);
    }

    // ── Message sanitization tests ────────────────────────────────────

    #[test]
    fn test_sanitize_api_messages_basic() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi"})),
        ];
        let result = sanitize_api_messages(&messages);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_sanitize_api_messages_removes_orphaned_tool() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi", "tool_calls": [
                {"id": "call_1", "function": {"name": "read", "arguments": "{}"}}
            ]})),
            // Orphaned tool result — references non-existent tool_call_id
            Arc::new(serde_json::json!({"role": "tool", "content": "result", "tool_call_id": "call_999"})),
        ];
        let result = sanitize_api_messages(&messages);
        // Should remove the orphaned tool result
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_sanitize_api_messages_keeps_valid_tool_result() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi", "tool_calls": [
                {"id": "call_1", "function": {"name": "read", "arguments": "{}"}}
            ]})),
            Arc::new(serde_json::json!({"role": "tool", "content": "result", "tool_call_id": "call_1"})),
        ];
        let result = sanitize_api_messages(&messages);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_sanitize_api_messages_merges_consecutive_users() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "user", "content": "follow up"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "hi"})),
        ];
        let result = sanitize_api_messages(&messages);
        // Consecutive user messages should be merged
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "hello\n\nfollow up");
    }

    // ── Message normalization tests ───────────────────────────────────

    #[test]
    fn test_normalize_messages_strips_assistant_whitespace() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
            Arc::new(serde_json::json!({"role": "assistant", "content": "  hi there  \n"})),
        ];
        let result = normalize_messages(&messages);
        assert_eq!(result[1]["content"], "hi there");
    }

    #[test]
    fn test_normalize_messages_canonicalizes_tool_args() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "function": {
                    "name": "read",
                    "arguments": "{\"path\":\"/tmp\",\"mode\":\"r\"}"
                }}
            ]})),
        ];
        let result = normalize_messages(&messages);
        let args = result[0]["tool_calls"][0]["function"]["arguments"]
            .as_str().unwrap();
        // Should be valid JSON (canonicalized)
        assert!(serde_json::from_str::<Value>(args).is_ok());
    }

    #[test]
    fn test_normalize_messages_preserves_user_content() {
        let messages = vec![
            Arc::new(serde_json::json!({"role": "user", "content": "  hello  "})),
        ];
        let result = normalize_messages(&messages);
        // User content should NOT be stripped
        assert_eq!(result[0]["content"], "  hello  ");
    }

    // ── Thinking budget exhaustion tests ──────────────────────────────

    #[test]
    fn test_thinking_budget_exhausted_claude_no_content() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "",
            "finish_reason": "length"
        });
        assert!(is_thinking_budget_exhausted(&response, "anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn test_thinking_budget_exhausted_open_o1() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "",
            "finish_reason": "length"
        });
        assert!(is_thinking_budget_exhausted(&response, "openai/o1"));
    }

    #[test]
    fn test_thinking_budget_not_exhausted_non_reasoning_model() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "",
            "finish_reason": "length"
        });
        assert!(!is_thinking_budget_exhausted(&response, "openai/gpt-4"));
    }

    #[test]
    fn test_thinking_budget_not_exhausted_normal_finish() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "Hello!",
            "finish_reason": "stop"
        });
        assert!(!is_thinking_budget_exhausted(&response, "anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn test_thinking_budget_exhausted_open_think_no_close() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "<think>Let me think about this...",
            "finish_reason": "length"
        });
        assert!(is_thinking_budget_exhausted(&response, "anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn test_thinking_budget_not_exhausted_closed_think() {
        let response = serde_json::json!({
            "role": "assistant",
            "content": "<think>Analysis done</think>Here's my answer.",
            "finish_reason": "length"
        });
        // Has both open and close — not considered exhausted
        assert!(!is_thinking_budget_exhausted(&response, "anthropic/claude-sonnet-4-6"));
    }

    // ── Pre-LLM hook tests ────────────────────────────────────────────

    #[test]
    fn test_pre_llm_hook_abort() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        agent.set_pre_llm_hook(|_sys, _msgs, _count| {
            PreLlmHookResult::Abort("Plugin blocked this request".to_string())
        });

        // Hook is set — verify no panic
        assert!(agent.pre_llm_hook.is_some());
    }

    #[test]
    fn test_pre_llm_hook_override_system() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let mut agent = AIAgent::new(config, registry).unwrap();

        agent.set_pre_llm_hook(|sys, _msgs, _count| {
            PreLlmHookResult::OverrideSystem(format!("OVERRIDE: {}", sys))
        });

        assert!(agent.pre_llm_hook.is_some());
    }

    #[test]
    fn test_turn_number_increments() {
        let config = AgentConfig::default();
        let registry = Arc::new(ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();

        assert_eq!(agent.turn_number(), 0);
    }
}
