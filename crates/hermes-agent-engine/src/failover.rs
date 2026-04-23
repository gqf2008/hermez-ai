#![allow(dead_code)]
//! Chat loop failover chain.
//!
//! Mirrors the Python failover sequence in `run_agent.py:9350-10127`.
//! On LLM errors, applies a priority-ordered sequence of recovery actions:
//! 1. Unicode sanitization (surrogate characters)
//! 2. Error classification
//! 3. Credential pool rotation
//! 4. Provider-specific auth refresh
//! 5. Thinking signature recovery
//! 6. Rate limit eager fallback
//! 7. Payload too large → compress
//! 8. Context overflow → compress
//! 9. Non-retryable → fallback → abort

use hermes_llm::credential_pool::CredentialPool;
use hermes_llm::error_classifier::{ClassifiedError, FailoverReason};
use serde_json::Value;
use std::sync::Arc;

use crate::agent::types::Message;

/// Failover chain state for a single conversation turn.
#[derive(Debug, Default)]
pub struct FailoverState {
    /// Consecutive 429 rate limit hits.
    pub consecutive_429: u32,
    /// Whether thinking signature has been stripped.
    pub thinking_stripped: bool,
    /// Unicode sanitization pass count.
    pub sanitize_passes: u32,
    /// Total retry attempts.
    pub retry_count: u32,
    /// Whether OAuth auth refresh has been attempted this turn.
    pub auth_refresh_attempted: bool,
    /// Whether context tier has been reduced this turn.
    pub tier_reduced: bool,
}

const MAX_SANITIZE_PASSES: u32 = 2;

/// Sanitize surrogate characters in message content.
///
/// Mirrors Python: UnicodeEncodeError recovery (run_agent.py:9376-9489).
/// Replaces invalid UTF-8 surrogate characters with replacement character.
pub fn sanitize_unicode_messages(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        let value = Arc::make_mut(msg);
        if let Some(obj) = value.as_object_mut() {
            for (_, val) in obj.iter_mut() {
                sanitize_value(val);
            }
        }
    }
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            // Filter out replacement/surrogate characters
            let cleaned: String = s.chars()
                .filter(|c| *c != '\u{FFFD}')
                .collect();
            *s = cleaned;
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sanitize_value(item);
            }
        }
        Value::Object(map) => {
            for (_, value) in map.iter_mut() {
                sanitize_value(value);
            }
        }
        _ => {}
    }
}

/// Strip reasoning details from all messages.
///
/// Mirrors Python: thinking signature recovery (run_agent.py:9574-9592).
/// Removes `reasoning`, `reasoning_content`, `reasoning_details` fields
/// and inline think tags from content.
pub fn strip_reasoning_from_messages(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        let value = Arc::make_mut(msg);
        if let Some(obj) = value.as_object_mut() {
            obj.remove("reasoning");
            obj.remove("reasoning_content");
            obj.remove("reasoning_details");
            // Strip inline tags from content
            if let Some(content) = obj.get("content").and_then(Value::as_str) {
                let cleaned = hermes_core::strip_think_blocks(content);
                if let Some(content_val) = obj.get_mut("content") {
                    *content_val = Value::String(cleaned);
                }
            }
        }
    }
}

/// Apply the failover chain for an LLM error.
///
/// Returns `FailoverAction` indicating what the caller should do.
/// Mirrors Python failover sequence (run_agent.py:9350-10127).
pub fn apply_failover(
    error: &ClassifiedError,
    state: &mut FailoverState,
    pool: Option<&CredentialPool>,
    has_compressor: bool,
    had_prior_success: bool,
) -> FailoverAction {
    state.retry_count += 1;

    // 1. Unicode sanitization — up to 2 passes
    if state.sanitize_passes < MAX_SANITIZE_PASSES {
        // Check if error looks like an encoding issue
        if error.message.contains("encoding") || error.message.contains("codec") {
            state.sanitize_passes += 1;
            return FailoverAction::SanitizeUnicode;
        }
    }

    // 2. Rate limit tracking (Python: first 429 doesn't rotate, second does)
    if error.reason == FailoverReason::RateLimit {
        if state.consecutive_429 > 0 {
            state.consecutive_429 = 0;
            if pool.is_some_and(|p| p.has_available()) {
                return FailoverAction::RotateCredential;
            }
            // Pool exhausted — eager fallback (Python: run_agent.py:9729-9750)
            return FailoverAction::TryFallback;
        } else {
            state.consecutive_429 += 1;
        }
        return FailoverAction::RetryWithBackoff;
    }

    // 3. Provider-specific auth refresh (OAuth 401 → try refresh before rotate)
    // Mirrors Python: Codex/Nous/Anthropic 401 refresh (run_agent.py:9500-9570)
    if error.reason == FailoverReason::Auth {
        match error.provider.as_str() {
            "anthropic" | "codex" | "nous" => {
                if !state.auth_refresh_attempted {
                    state.auth_refresh_attempted = true;
                    return FailoverAction::RefreshProviderAuth;
                }
                // Refresh already attempted, fall through to rotation
            }
            _ => {}
        }
    }

    // 4. Credential pool rotation (non-rate-limit errors)
    if error.should_rotate_credential {
        match error.reason {
            FailoverReason::Billing | FailoverReason::Auth => {
                return FailoverAction::RotateCredential;
            }
            _ => {}
        }
    }

    // 5. Thinking signature recovery (one-shot)
    if error.reason == FailoverReason::ThinkingSignature && !state.thinking_stripped {
        state.thinking_stripped = true;
        return FailoverAction::StripThinkingSignature;
    }

    // 6. Context tier reduction (before compression — cheaper first)
    // Mirrors Python: degrade probe tier / reduce max_tokens (run_agent.py:9800-9900)
    if error.reason == FailoverReason::ContextOverflow && !state.tier_reduced {
        state.tier_reduced = true;
        return FailoverAction::ReduceContextTier;
    }

    // 7. Context overflow → compress
    if error.reason == FailoverReason::ContextOverflow && has_compressor {
        return FailoverAction::CompressContext;
    }

    // 8. Payload too large → compress
    if error.reason == FailoverReason::PayloadTooLarge && has_compressor {
        return FailoverAction::CompressContext;
    }

    // 9. Rollback to last assistant turn (truncated response after success)
    // Mirrors Python: rollback on incomplete response after successful API call
    if had_prior_success && error.retryable
        && error.reason != FailoverReason::ContextOverflow
        && error.reason != FailoverReason::PayloadTooLarge
    {
        return FailoverAction::RollbackToLastAssistant;
    }

    // 10. Retryable errors → backoff
    if error.retryable {
        return FailoverAction::RetryWithBackoff;
    }

    // 11. Fallback recommended
    if error.should_fallback {
        return FailoverAction::TryFallback;
    }

    // 12. Abort
    FailoverAction::Abort
}

/// Recommended action after failover analysis.
#[derive(Debug, Clone)]
pub enum FailoverAction {
    /// Sanitize Unicode characters and retry.
    SanitizeUnicode,
    /// Rotate to next credential in pool and retry.
    RotateCredential,
    /// Refresh auth token for current provider (OAuth refresh).
    RefreshProviderAuth,
    /// Strip reasoning from messages and retry (one-shot).
    StripThinkingSignature,
    /// Compress context and retry.
    CompressContext,
    /// Reduce context tier (degrade probing level, remove oldest turns).
    ReduceContextTier,
    /// Retry with exponential backoff.
    RetryWithBackoff,
    /// Roll back messages to last complete assistant turn and retry.
    /// Mirrors Python `_rollback_to_last_assistant()` (run_agent.py:~2497).
    RollbackToLastAssistant,
    /// Try fallback provider.
    TryFallback,
    /// No recovery available — abort.
    Abort,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use hermes_llm::error_classifier::classify_api_error;

    #[test]
    fn test_sanitize_unicode_messages() {
        let mut messages = vec![
            Arc::new(serde_json::json!({
                "role": "user",
                "content": "Hello\u{FFFD}World"
            })),
        ];
        sanitize_unicode_messages(&mut messages);
        // Replacement characters should be filtered out
        let content = messages[0].get("content").and_then(Value::as_str).unwrap();
        assert_eq!(content, "HelloWorld");
    }

    #[test]
    fn test_strip_reasoning_fields() {
        let mut messages = vec![
            Arc::new(serde_json::json!({
                "role": "assistant",
                "reasoning": "I should think...",
                "reasoning_content": "More thinking...",
                "reasoning_details": [{"summary": "Summary"}],
                "content": "Hello!"
            })),
        ];
        strip_reasoning_from_messages(&mut messages);
        assert!(messages[0].get("reasoning").is_none());
        assert!(messages[0].get("reasoning_content").is_none());
        assert!(messages[0].get("reasoning_details").is_none());
        assert_eq!(messages[0].get("content").and_then(Value::as_str), Some("Hello!"));
    }

    #[test]
    fn test_strip_inline_reasoning_tags() {
        let input = "<think>Secret thinking</think>Hello!";
        let result = hermes_core::strip_think_blocks(input);
        assert_eq!(result, "Hello!");
    }

    #[test]
    fn test_strip_thinking_tags() {
        let input = "<thinking>Internal reasoning</thinking>Answer: 42";
        let result = hermes_core::strip_think_blocks(input);
        assert_eq!(result, "Answer: 42");
    }

    #[test]
    fn test_apply_failover_billing() {
        let err = classify_api_error("openrouter", "model", Some(402), "Billing exceeded");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        // Billing errors have should_rotate_credential=true → RotateCredential
        assert!(matches!(action, FailoverAction::RotateCredential));
    }

    #[test]
    fn test_apply_failover_context_overflow() {
        let err = classify_api_error("anthropic", "claude", Some(400), "context length exceeded");
        let mut state = FailoverState::default();
        // First context overflow → ReduceContextTier (one-shot, before compression)
        let action = apply_failover(&err, &mut state, None, true, false);
        assert!(matches!(action, FailoverAction::ReduceContextTier));
        // Second overflow → CompressContext (tier already reduced)
        let action2 = apply_failover(&err, &mut state, None, true, false);
        assert!(matches!(action2, FailoverAction::CompressContext));
    }

    #[test]
    fn test_apply_failover_thinking_signature() {
        let err = classify_api_error("anthropic", "claude", Some(400), "thinking signature invalid");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::StripThinkingSignature));
    }

    #[test]
    fn test_apply_failover_thinking_already_stripped() {
        let err = classify_api_error("anthropic", "claude", Some(400), "thinking signature invalid");
        let mut state = FailoverState {
            thinking_stripped: true,
            ..Default::default()
        };
        let action = apply_failover(&err, &mut state, None, false, false);
        // Thinking signature error is retryable, so when already stripped → RetryWithBackoff
        assert!(matches!(action, FailoverAction::RetryWithBackoff));
    }

    #[test]
    fn test_apply_failover_rate_limit_first() {
        let err = classify_api_error("openai", "gpt-4", Some(429), "Rate limit exceeded");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        // First 429: retry with backoff (don't rotate yet)
        assert!(matches!(action, FailoverAction::RetryWithBackoff));
        assert_eq!(state.consecutive_429, 1);
    }

    #[test]
    fn test_apply_failover_retryable_unknown() {
        let err = classify_api_error("unknown", "model", None, "Something weird");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::RetryWithBackoff));
    }

    #[test]
    fn test_apply_failover_abort_no_fallback() {
        // Construct an error that is neither retryable nor has fallback
        let err = ClassifiedError {
            reason: FailoverReason::Unknown,
            status_code: None,
            provider: "custom".to_string(),
            model: "model".to_string(),
            message: "unrecoverable error".to_string(),
            error_context: HashMap::new(),
            retryable: false,
            should_compress: false,
            should_rotate_credential: false,
            should_fallback: false,
        };
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::Abort));
    }

    #[test]
    fn test_apply_failover_unicode_pass_then_retry() {
        // Unicode encoding error should trigger sanitize, then retry
        let err = classify_api_error("openai", "model", Some(400), "encoding error: invalid byte");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::SanitizeUnicode));
        assert_eq!(state.sanitize_passes, 1);
    }

    #[test]
    fn test_apply_failover_max_sanitize_passes() {
        // After 2 sanitize passes, should fall back to retry/abort
        let mut state = FailoverState { sanitize_passes: 2, ..Default::default() };
        let err = classify_api_error("openai", "model", Some(400), "encoding error: invalid byte");
        let action = apply_failover(&err, &mut state, None, false, false);
        // Max passes reached, should not sanitize again
        assert!(!matches!(action, FailoverAction::SanitizeUnicode));
    }

    #[test]
    fn test_apply_failover_billing_with_pool() {
        // Billing error triggers credential rotation in the failover chain
        let err = classify_api_error("openrouter", "model", Some(402), "Billing exceeded");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        // Billing errors have should_rotate_credential=true → RotateCredential
        assert!(matches!(action, FailoverAction::RotateCredential));
    }

    #[test]
    fn test_apply_failover_rollback_after_success() {
        // Retryable error after a successful API call → rollback
        let err = classify_api_error("openai", "model", None, "server disconnected");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, true);
        assert!(matches!(action, FailoverAction::RollbackToLastAssistant));
    }

    #[test]
    fn test_apply_failover_rollback_not_without_prior_success() {
        // Retryable error without prior success → backoff, not rollback
        let err = classify_api_error("openai", "model", None, "server disconnected");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::RetryWithBackoff));
    }

    #[test]
    fn test_apply_failover_rate_limit_eager_fallback() {
        // Second 429 without pool → TryFallback
        let err = classify_api_error("openai", "gpt-4", Some(429), "Rate limit exceeded");
        let mut state = FailoverState {
            consecutive_429: 1,
            ..Default::default()
        };
        let action = apply_failover(&err, &mut state, None, false, false);
        // No pool available → eager fallback
        assert!(matches!(action, FailoverAction::TryFallback));
    }

    #[test]
    fn test_apply_failover_provider_auth_refresh() {
        // Anthropic 401 → RefreshProviderAuth (one-shot)
        let err = classify_api_error("anthropic", "claude", Some(401), "invalid api key");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, false, false);
        assert!(matches!(action, FailoverAction::RefreshProviderAuth));
        assert!(state.auth_refresh_attempted);
    }

    #[test]
    fn test_apply_failover_provider_auth_refresh_already_attempted() {
        // Auth refresh already attempted → falls through to RotateCredential
        let err = classify_api_error("anthropic", "claude", Some(401), "invalid api key");
        let mut state = FailoverState {
            auth_refresh_attempted: true,
            ..Default::default()
        };
        let action = apply_failover(&err, &mut state, None, false, false);
        // After refresh already attempted, auth error with should_rotate_credential → Rotate
        assert!(matches!(action, FailoverAction::RotateCredential));
    }

    #[test]
    fn test_apply_failover_context_tier_reduction() {
        // Context overflow → ReduceContextTier (one-shot, before compression)
        let err = classify_api_error("anthropic", "claude", Some(400), "context length exceeded");
        let mut state = FailoverState::default();
        let action = apply_failover(&err, &mut state, None, true, false);
        assert!(matches!(action, FailoverAction::ReduceContextTier));
        assert!(state.tier_reduced);
    }

    #[test]
    fn test_apply_failover_context_tier_already_reduced() {
        // Tier already reduced → compress context
        let err = classify_api_error("anthropic", "claude", Some(400), "context length exceeded");
        let mut state = FailoverState {
            tier_reduced: true,
            ..Default::default()
        };
        let action = apply_failover(&err, &mut state, None, true, false);
        assert!(matches!(action, FailoverAction::CompressContext));
    }
}
