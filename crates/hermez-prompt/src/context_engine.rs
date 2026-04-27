//! Pluggable context engine abstraction.
//!
//! Mirrors Python `agent/context_engine.py`.
//!
//! A context engine controls how conversation context is managed when
//! approaching the model's token limit. The built-in `ContextCompressor`
//! is the default implementation. Third-party engines can replace it
//! via the plugin system or config.
//!
//! Selection is config-driven: `context.engine` in config.yaml.
//! Default is `"compressor"` (the built-in). Only one engine is active.

use serde_json::Value;

/// Trait for pluggable context engines.
///
/// The engine is responsible for:
///   - Deciding when compaction should fire
///   - Performing compaction (summarization, DAG construction, etc.)
///   - Optionally exposing tools the agent can call
///   - Tracking token usage from API responses
///
/// Lifecycle:
///   1. Engine is instantiated and registered (plugin register() or default)
///   2. `on_session_start()` called when a conversation begins
///   3. `update_from_response()` called after each API response with usage data
///   4. `should_compress()` checked after each turn
///   5. `compress()` called when `should_compress()` returns true
///   6. `on_session_end()` called at real session boundaries
pub trait ContextEngine: Send + Sync {
    /// Short identifier (e.g. 'compressor', 'lcm').
    fn name(&self) -> &str;

    /// Called when a new conversation session begins.
    fn on_session_start(&mut self);

    /// Called when the session state is reset (e.g. /new, /clear).
    fn on_session_reset(&mut self);

    /// Update token usage from the latest API response.
    fn update_from_response(&mut self, prompt_tokens: usize, completion_tokens: usize);

    /// Check whether the context should be compressed now.
    fn should_compress(&self, prompt_tokens: Option<usize>) -> bool;

    /// Compress the message list and return the compacted version.
    ///
    /// `current_tokens` is the estimated token count of the messages.
    /// `focus_topic` is an optional hint for targeted compression.
    fn compress(
        &mut self,
        messages: &[Value],
        current_tokens: Option<usize>,
        focus_topic: Option<&str>,
    ) -> Vec<Value>;

    /// Called at real session boundaries (CLI exit, /reset, gateway expiry).
    fn on_session_end(&mut self);

    /// Return the compression threshold in tokens.
    ///
    /// Used by `AIAgent` to emit context pressure warnings before
    /// the threshold is actually exceeded.
    fn threshold_tokens(&self) -> usize;

    /// Recalculate budgets when the model changes (e.g., /model, fallback).
    fn update_model(&mut self, model: &str, context_length: Option<usize>) {
        let _ = (model, context_length);
    }
}

/// Factory for creating context engines by name.
pub fn create_engine(name: &str, config: Option<crate::CompressorConfig>) -> Option<Box<dyn ContextEngine>> {
    match name {
        "compressor" | "default" | "" => {
            let cfg = config.unwrap_or_default();
            Some(Box::new(crate::ContextCompressor::new(cfg)))
        }
        // Future: "lcm", "dag", etc.
        _ => None,
    }
}

/// List available engine names.
pub fn available_engines() -> &'static [&'static str] {
    &["compressor"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_engines() {
        let engines = available_engines();
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0], "compressor");
    }

    #[test]
    fn test_create_engine_compressor() {
        let engine = create_engine("compressor", None);
        assert!(engine.is_some());
        assert_eq!(engine.unwrap().name(), "compressor");
    }

    #[test]
    fn test_create_engine_default() {
        let engine = create_engine("default", None);
        assert!(engine.is_some());
        assert_eq!(engine.unwrap().name(), "compressor");
    }

    #[test]
    fn test_create_engine_empty_string() {
        let engine = create_engine("", None);
        assert!(engine.is_some());
        assert_eq!(engine.unwrap().name(), "compressor");
    }

    #[test]
    fn test_create_engine_unknown_returns_none() {
        let engine = create_engine("lcm", None);
        assert!(engine.is_none());
    }
}
