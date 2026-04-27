//! Public API control methods for AIAgent.
//!
//! Mirrors Python `chat()`, `interrupt()`, `switch_model()`,
//! `reset_session_state()`, `get_rate_limit_state()`, `get_activity_summary()`.

use serde_json::Value;

use super::AIAgent;
use super::types::{Message, PreLlmHookResult};
use hermez_core::Result;

impl AIAgent {
    /// Simple chat interface that returns just the final response string.
    ///
    /// Mirrors Python: `chat()` (run_agent.py:11257).
    /// A thin wrapper around `run_conversation` that extracts just the
    /// response text, hiding the full `TurnResult` struct from callers.
    pub async fn chat(&mut self, query: &str) -> Result<String> {
        let result = self.run_conversation(query, None, None).await;
        Ok(result.response)
    }

    /// Request the agent to interrupt its current tool-calling loop.
    ///
    /// Mirrors Python: `interrupt()` (run_agent.py:3045).
    /// Call this from another task (e.g., message receiver) to gracefully
    /// stop the agent and process a new message. Also signals long-running
    /// tool executions to terminate early.
    pub fn interrupt(&self, message: Option<&str>) {
        self.interrupt.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut guard = self.interrupt_message.lock();
            *guard = message.map(String::from);
        }
        if let Some(ref mgr) = self.subagent_mgr {
            mgr.interrupt.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        tracing::info!(
            "Interrupt requested: session_id={:?}, message={:?}",
            self.config.session_id,
            message
        );
    }

    /// Clear any pending interrupt request.
    ///
    /// Mirrors Python: `clear_interrupt()` (run_agent.py:3094).
    pub fn clear_interrupt(&self) {
        self.interrupt.store(false, std::sync::atomic::Ordering::SeqCst);
        {
            let mut guard = self.interrupt_message.lock();
            *guard = None;
        }
    }

    /// Check if an interrupt has been requested.
    ///
    /// Mirrors Python: `is_interrupted` property (run_agent.py:3278).
    /// Called by the conversation loop to check whether to abort
    /// the current turn.
    pub fn is_interrupted(&self) -> bool {
        self.interrupt.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Switch the model/provider in-place for a live agent.
    ///
    /// Mirrors Python: `switch_model()` (run_agent.py:1671).
    /// Called by the /model command handlers after credentials have been
    /// resolved and validated. Performs the actual runtime swap of model
    /// configuration.
    ///
    /// Note: Unlike the Python version which rebuilds HTTP clients, the
    /// Rust architecture creates clients per-request, so only the config
    /// fields need updating.
    pub fn switch_model(
        &mut self,
        new_model: &str,
        new_provider: Option<&str>,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) {
        let old_model = self.config.model.clone();
        let old_provider = self.config.provider.clone();

        self.config.model = new_model.to_string();
        if let Some(provider) = new_provider {
            self.config.provider = Some(provider.to_string());
        }
        if let Some(key) = api_key {
            self.config.api_key = Some(key.to_string());
        }
        if let Some(url) = base_url {
            self.config.base_url = Some(url.to_string());
        }

        // Invalidate cached system prompt so it rebuilds with new model info
        self.cached_system_prompt = None;

        // Update primary runtime snapshot so the new model persists
        self.primary_runtime = Some(super::types::PrimaryRuntime {
            model: self.config.model.clone(),
            base_url: self.config.base_url.clone(),
            api_key: self.config.api_key.clone(),
            provider: self.config.provider.clone(),
        });

        // Recalculate context compression budgets for the new model.
        // Mirrors Python ContextCompressor.update_model()
        // (context_compressor.py:301-327, run_agent.py:2143-2160).
        if let Some(ref mut engine) = self.context_engine {
            let context_len = hermez_prompt::context_compressor::estimate_context_length(new_model);
            engine.update_model(new_model, Some(context_len));
        }

        tracing::info!(
            "Model switched: {} ({:?}) → {} ({:?})",
            old_model, old_provider, new_model, new_provider
        );
    }

    /// Reset all session-scoped state for a fresh conversation.
    ///
    /// Mirrors Python: `reset_session_state()` (run_agent.py:1632).
    /// Resets:
    /// - Turn counters (budget, turns since memory/skill use)
    /// - Failover state
    /// - Context compressor (if enabled)
    /// - Last usage and delegate results
    pub fn reset_session_state(&mut self) {
        self.budget.reset();
        self.failover_state = super::failover::FailoverState::default();
        self.turns_since_memory = 0;
        self.iters_since_skill = 0;
        self.disable_streaming = false;
        self.force_ascii_payload = false;
        self.fallback_activated = false;

        {
            let mut guard = self.delegate_results.lock();
            guard.clear();
        }
        {
            let mut guard = self.last_usage.lock();
            *guard = None;
        }

        if let Some(ref mut engine) = self.context_engine {
            engine.on_session_reset();
        }

        self.turn_number = 0;

        tracing::info!(
            "Session state reset: session_id={:?}",
            self.config.session_id
        );
    }

    /// Set a pre-LLM hook for plugin interception.
    ///
    /// Mirrors Python plugin system. Plugins can inspect/modify messages,
    /// system prompt, or abort the call before it reaches the API.
    #[allow(dead_code)]
    pub fn set_pre_llm_hook<F>(&mut self, hook: F)
    where
        F: Fn(&str, &[Message], usize) -> PreLlmHookResult + Send + Sync + 'static,
    {
        self.pre_llm_hook = Some(std::sync::Arc::new(hook));
    }

    /// Return the current turn number.
    #[allow(dead_code)]
    pub fn turn_number(&self) -> u64 {
        self.turn_number
    }

    /// Return the last captured rate limit state, or None.
    ///
    /// Mirrors Python: `get_rate_limit_state()` (run_agent.py:3126).
    /// Returns the parsed x-ratelimit-* headers from the last provider
    /// response. Useful for gateway deployments to display rate limit
    /// information to users.
    pub fn get_rate_limit_state(&self) -> Option<Value> {
        self.rate_limit_state.lock().clone()
    }

    /// Capture rate limit state from provider response headers.
    ///
    /// Mirrors Python: `_capture_rate_limits()` (run_agent.py:3107).
    /// Parses x-ratelimit-* headers and caches the state for later
    /// retrieval via `get_rate_limit_state()`.
    pub fn capture_rate_limits(&self, rate_limit_data: Value) {
        {
            let mut guard = self.rate_limit_state.lock();
            *guard = Some(rate_limit_data);
        }
    }

    /// Record activity (called before each tool execution and API call).
    ///
    /// Mirrors Python: `_touch_activity()` (run_agent.py:3102).
    /// Updates the last-activity timestamp for gateway diagnostics.
    pub fn touch_activity(&self, description: &str) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        {
            let mut guard = self.last_activity_ts.lock();
            *guard = now;
        }
        {
            let mut guard = self.last_activity_desc.lock();
            *guard = description.to_string();
        }
    }

    /// Return a snapshot of the agent's current activity for diagnostics.
    ///
    /// Mirrors Python: `get_activity_summary()` (run_agent.py:3130).
    /// Called by the gateway timeout handler to report what the agent
    /// was doing, and by periodic "still working" notifications.
    pub fn get_activity_summary(&self) -> Value {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let last_activity_ts = *self.last_activity_ts.lock();
        let last_activity_desc = self.last_activity_desc.lock().clone();
        let seconds_since_activity = now - last_activity_ts;
        let current_tool = self.current_tool.lock().clone();
        let budget_used = self.budget.used();
        let budget_max = self.budget.max_total;

        serde_json::json!({
            "last_activity_ts": last_activity_ts,
            "last_activity_desc": last_activity_desc,
            "seconds_since_activity": format!("{:.1}", seconds_since_activity.max(0.0)),
            "current_tool": current_tool,
            "budget_used": budget_used,
            "budget_max": budget_max,
            "model": self.config.model,
            "provider": self.config.provider,
            "session_id": self.config.session_id,
        })
    }

    /// Set the current tool name for activity tracking.
    ///
    /// Called internally during tool execution to update the activity
    /// summary with the tool currently being run.
    #[allow(dead_code)]
    fn set_current_tool(&self, tool_name: Option<&str>) {
        {
            let mut guard = self.current_tool.lock();
            *guard = tool_name.map(String::from);
        }
    }
}
