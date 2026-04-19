//! Error classification for API errors.
#![allow(clippy::result_large_err)]
//!
//! Classifies HTTP and transport errors into actionable categories with
//! hints for retry, fallback, compression, or credential rotation.
//! Mirrors the Python `error_classifier.py` (classify_api_error).

use std::collections::HashMap;
use std::fmt;
use serde::Serialize;
use serde_json::Value;

/// Reasons for API failure, mapped to specific actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FailoverReason {
    Auth,
    AuthPermanent,
    Billing,
    RateLimit,
    Overloaded,
    ServerError,
    Timeout,
    ContextOverflow,
    PayloadTooLarge,
    ModelNotFound,
    FormatError,
    ThinkingSignature,
    LongContextTier,
    Unknown,
}

impl std::fmt::Display for FailoverReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailoverReason::Auth => write!(f, "auth"),
            FailoverReason::AuthPermanent => write!(f, "auth_permanent"),
            FailoverReason::Billing => write!(f, "billing"),
            FailoverReason::RateLimit => write!(f, "rate_limit"),
            FailoverReason::Overloaded => write!(f, "overloaded"),
            FailoverReason::ServerError => write!(f, "server_error"),
            FailoverReason::Timeout => write!(f, "timeout"),
            FailoverReason::ContextOverflow => write!(f, "context_overflow"),
            FailoverReason::PayloadTooLarge => write!(f, "payload_too_large"),
            FailoverReason::ModelNotFound => write!(f, "model_not_found"),
            FailoverReason::FormatError => write!(f, "format_error"),
            FailoverReason::ThinkingSignature => write!(f, "thinking_signature"),
            FailoverReason::LongContextTier => write!(f, "long_context_tier"),
            FailoverReason::Unknown => write!(f, "unknown"),
        }
    }
}

/// Action hints computed during classification.
#[derive(Debug, Clone, Copy)]
struct ActionHints {
    retryable: bool,
    should_compress: bool,
    should_rotate_credential: bool,
    should_fallback: bool,
}

/// Classified API error with actionable hints.
#[derive(Debug, Clone, Serialize)]
pub struct ClassifiedError {
    pub reason: FailoverReason,
    pub status_code: Option<u16>,
    pub provider: String,
    pub model: String,
    pub message: String,
    pub error_context: HashMap<String, Value>,
    pub retryable: bool,
    pub should_compress: bool,
    pub should_rotate_credential: bool,
    pub should_fallback: bool,
}

impl fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {} ({}/{})", self.reason, self.message, self.provider, self.model)
    }
}

/// Parameters for context-aware classification of 400 errors and
/// server-disconnect heuristics.
#[derive(Debug, Clone, Default)]
pub struct ErrorContextParams {
    /// Approximate token count of the current conversation.
    pub approx_tokens: usize,
    /// Maximum context length for the current model.
    pub context_length: usize,
    /// Number of messages in the current session.
    pub num_messages: usize,
}

/// Classify an API error into an actionable category.
///
/// 7-step classification pipeline:
/// 1. Provider-specific patterns (thinking signature, long-context tier)
/// 2. HTTP status code classification with message-aware refinement
/// 3. Error code classification (from structured body)
/// 4. Message pattern matching (billing, rate_limit, context_overflow, auth)
/// 5. Server disconnect + large session -> context overflow
/// 6. Transport/timeout heuristics
/// 7. Fallback: Unknown (retryable with backoff)
///
/// Use `classify_api_error_with_context` for context-aware classification
/// (400 refinement, disconnect heuristics).
pub fn classify_api_error(
    provider: &str,
    model: &str,
    status_code: Option<u16>,
    message: &str,
) -> ClassifiedError {
    classify_api_error_with_context(
        provider, model, status_code, message,
        &ErrorContextParams::default(),
    )
}

/// Classify an API error with context-aware parameters for 400 refinement
/// and server-disconnect heuristics.
pub fn classify_api_error_with_context(
    provider: &str,
    model: &str,
    status_code: Option<u16>,
    message: &str,
    ctx: &ErrorContextParams,
) -> ClassifiedError {
    // Build combined error message from all available sources
    let error_msg = build_error_message(message);
    let ml = error_msg.to_lowercase();
    let provider_lower = provider.to_lowercase();
    let model_lower = model.to_lowercase();

    // Step 1: Provider-specific patterns
    if is_thinking_signature(status_code, &ml) {
        return classified(
            FailoverReason::ThinkingSignature, status_code, provider, model, message,
            HashMap::new(),
            ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: true },
        );
    }
    if is_long_context_tier(status_code, &ml) {
        return classified(
            FailoverReason::LongContextTier, status_code, provider, model, message,
            HashMap::new(),
            ActionHints { retryable: true, should_compress: true, should_rotate_credential: false, should_fallback: true },
        );
    }

    // Step 2: HTTP status code classification
    if let Some(code) = status_code {
        if let Some(result) = classify_by_status(
            code, &ml, &provider_lower, &model_lower, ctx,
        ) {
            return classified(
                result.0, status_code, provider, model, message,
                HashMap::new(), result.1,
            );
        }
    }

    // Step 4: Message pattern matching
    if let Some(result) = classify_by_message(&ml, ctx) {
        return classified(
            result.0, status_code, provider, model, message,
            HashMap::new(), result.1,
        );
    }

    // Step 5: Server disconnect + large session -> context overflow
    if is_server_disconnect(&ml) && status_code.is_none() {
        let is_large = is_large_session(ctx);
        let reason = if is_large {
            FailoverReason::ContextOverflow
        } else {
            FailoverReason::Timeout
        };
        return classified(
            reason, status_code, provider, model, message,
            HashMap::new(),
            ActionHints { retryable: true, should_compress: is_large, should_rotate_credential: false, should_fallback: false },
        );
    }

    // Step 6: Transport/timeout
    if ml.contains("timeout") || ml.contains("timed out") {
        return classified(
            FailoverReason::Timeout, status_code, provider, model, message,
            HashMap::new(),
            ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: false },
        );
    }
    if ml.contains("disconnect") || ml.contains("connection") {
        return classified(
            FailoverReason::ServerError, status_code, provider, model, message,
            HashMap::new(),
            ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: false },
        );
    }

    // Step 7: Fallback
    classified(
        FailoverReason::Unknown, status_code, provider, model, message,
        HashMap::new(),
        ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: false },
    )
}

/// Build a combined error message string from the raw message and any
/// structured body data. Mirrors Python's combination of str(error) +
/// body message + metadata.raw message.
fn build_error_message(raw_message: &str) -> String {
    // For now, use the raw message directly. When integrating with the
    // retry loop, extract_error_body should be called separately and
    // its results passed in. The raw message is the primary signal.
    raw_message.to_string()
}

// ── Provider-specific pattern checks ─────────────────────────────────────

/// Anthropic thinking block signature invalid (400).
/// Not gated on provider -- OpenRouter proxies Anthropic errors.
fn is_thinking_signature(status_code: Option<u16>, msg: &str) -> bool {
    status_code == Some(400) && msg.contains("thinking") && msg.contains("signature")
}

/// Anthropic long-context tier gate (429 "extra usage" + "long context").
fn is_long_context_tier(status_code: Option<u16>, msg: &str) -> bool {
    status_code == Some(429) && msg.contains("extra usage") && msg.contains("long context")
}

// ── Status code classification ───────────────────────────────────────────

const H_AUTH: ActionHints = ActionHints { retryable: false, should_compress: false, should_rotate_credential: true, should_fallback: true };
const H_AUTH_PERMANENT: ActionHints = ActionHints { retryable: false, should_compress: false, should_rotate_credential: false, should_fallback: true };
const H_BILLING: ActionHints = ActionHints { retryable: false, should_compress: false, should_rotate_credential: true, should_fallback: true };
const H_RATE_LIMIT: ActionHints = ActionHints { retryable: true, should_compress: false, should_rotate_credential: true, should_fallback: true };
const H_OVERLOADED: ActionHints = ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: false };
const H_SERVER_ERROR: ActionHints = ActionHints { retryable: true, should_compress: false, should_rotate_credential: false, should_fallback: false };
const H_CONTEXT_OVERFLOW: ActionHints = ActionHints { retryable: true, should_compress: true, should_rotate_credential: false, should_fallback: false };
const H_PAYLOAD_TOO_LARGE: ActionHints = ActionHints { retryable: true, should_compress: true, should_rotate_credential: false, should_fallback: false };
const H_MODEL_NOT_FOUND: ActionHints = ActionHints { retryable: false, should_compress: false, should_rotate_credential: false, should_fallback: true };
const H_FORMAT_ERROR: ActionHints = ActionHints { retryable: false, should_compress: false, should_rotate_credential: false, should_fallback: true };

/// Returns (reason, action_hints) or None if status code not recognized.
fn classify_by_status(
    code: u16,
    msg: &str,
    provider: &str,
    model: &str,
    ctx: &ErrorContextParams,
) -> Option<(FailoverReason, ActionHints)> {
    match code {
        401 => Some((FailoverReason::Auth, H_AUTH)),

        403 => {
            // OpenRouter 403 "key limit exceeded" / "spending limit" is billing
            if msg.contains("key limit exceeded") || msg.contains("spending limit") {
                Some((FailoverReason::Billing, H_BILLING))
            } else {
                Some((FailoverReason::Auth, H_AUTH_PERMANENT))
            }
        }

        402 => Some(classify_402(msg)),

        404 => Some((FailoverReason::ModelNotFound, H_MODEL_NOT_FOUND)),

        413 => Some((FailoverReason::PayloadTooLarge, H_PAYLOAD_TOO_LARGE)),

        429 => {
            // Long-context tier already checked in Step 1
            Some((FailoverReason::RateLimit, H_RATE_LIMIT))
        }

        400 => Some(classify_400(msg, provider, model, ctx)),

        500 | 502 => Some((FailoverReason::ServerError, H_SERVER_ERROR)),

        503 | 529 => Some((FailoverReason::Overloaded, H_OVERLOADED)),

        // Other 4xx -- non-retryable format error
        c if (400..500).contains(&c) => Some((FailoverReason::FormatError, H_FORMAT_ERROR)),

        // Other 5xx -- retryable server error
        c if (500..600).contains(&c) => Some((FailoverReason::ServerError, H_SERVER_ERROR)),

        _ => None,
    }
}

/// Disambiguate 402: billing exhaustion vs transient usage limit.
fn classify_402(msg: &str) -> (FailoverReason, ActionHints) {
    let has_usage_limit = has_any_pattern(msg, USAGE_LIMIT_PATTERNS);
    let has_transient = has_any_pattern(msg, USAGE_LIMIT_TRANSIENT_SIGNALS);

    if has_usage_limit && has_transient {
        // Transient quota -- treat as rate limit
        (FailoverReason::RateLimit, H_RATE_LIMIT)
    } else {
        // Confirmed billing exhaustion
        (FailoverReason::Billing, H_BILLING)
    }
}

/// Classify 400 Bad Request -- context overflow, model not found, rate limit,
/// billing, or generic format error. Considers session size heuristics.
fn classify_400(
    msg: &str,
    _provider: &str,
    _model: &str,
    ctx: &ErrorContextParams,
) -> (FailoverReason, ActionHints) {
    // Context overflow from message patterns
    if has_any_pattern(msg, CONTEXT_OVERFLOW_PATTERNS) {
        return (FailoverReason::ContextOverflow, H_CONTEXT_OVERFLOW);
    }

    // Model-not-found as 400 (e.g. OpenRouter)
    if has_any_pattern(msg, MODEL_NOT_FOUND_PATTERNS) {
        return (FailoverReason::ModelNotFound, H_MODEL_NOT_FOUND);
    }

    // Rate limit / billing as 400 instead of 429/402
    if has_any_pattern(msg, RATE_LIMIT_PATTERNS) {
        return (FailoverReason::RateLimit, H_RATE_LIMIT);
    }
    if has_any_pattern(msg, BILLING_PATTERNS) {
        return (FailoverReason::Billing, H_BILLING);
    }

    // Generic 400 + large session -> probable context overflow
    // Anthropic sometimes returns a bare "Error" when context is too large
    if is_generic_400_message(msg) && is_large_session(ctx) {
        return (FailoverReason::ContextOverflow, H_CONTEXT_OVERFLOW);
    }

    // Non-retryable format error
    (FailoverReason::FormatError, H_FORMAT_ERROR)
}

/// Check if the 400 error body is generic (short message or "error").
fn is_generic_400_message(msg: &str) -> bool {
    msg.len() < 30 || msg == "error" || msg.is_empty()
}

/// Check if the current session is large enough that a generic 400 is
/// likely context overflow rather than a format issue.
fn is_large_session(ctx: &ErrorContextParams) -> bool {
    let threshold_tokens = (ctx.context_length as f64 * 0.4) as usize;
    ctx.approx_tokens > threshold_tokens
        || ctx.approx_tokens > 80_000
        || ctx.num_messages > 80
}

// ── Message pattern classification ───────────────────────────────────────

fn classify_by_message(
    msg: &str,
    _ctx: &ErrorContextParams,
) -> Option<(FailoverReason, ActionHints)> {
    // Payload-too-large patterns (from message text when no status_code)
    if has_any_pattern(msg, PAYLOAD_TOO_LARGE_PATTERNS) {
        return Some((FailoverReason::PayloadTooLarge, H_PAYLOAD_TOO_LARGE));
    }

    // Context overflow patterns -- checked before usage-limit to avoid
    // "max input token limit exceeded" matching "limit exceeded" first.
    if has_any_pattern(msg, CONTEXT_OVERFLOW_PATTERNS) {
        return Some((FailoverReason::ContextOverflow, H_CONTEXT_OVERFLOW));
    }

    // Usage-limit patterns with disambiguation (same logic as 402)
    if has_any_pattern(msg, USAGE_LIMIT_PATTERNS) {
        let has_transient = has_any_pattern(msg, USAGE_LIMIT_TRANSIENT_SIGNALS);
        if has_transient {
            return Some((FailoverReason::RateLimit, H_RATE_LIMIT));
        }
        return Some((FailoverReason::Billing, H_BILLING));
    }

    // Billing patterns
    if has_any_pattern(msg, BILLING_PATTERNS) {
        return Some((FailoverReason::Billing, H_BILLING));
    }

    // Rate limit patterns
    if has_any_pattern(msg, RATE_LIMIT_PATTERNS) {
        return Some((FailoverReason::RateLimit, H_RATE_LIMIT));
    }

    // Auth patterns
    if has_any_pattern(msg, AUTH_PATTERNS) {
        return Some((FailoverReason::Auth, H_AUTH));
    }

    // Model not found patterns
    if has_any_pattern(msg, MODEL_NOT_FOUND_PATTERNS) {
        return Some((FailoverReason::ModelNotFound, H_MODEL_NOT_FOUND));
    }

    None
}

// ── Server disconnect check ─────────────────────────────────────────────

fn is_server_disconnect(msg: &str) -> bool {
    has_any_pattern(msg, SERVER_DISCONNECT_PATTERNS)
}

// ── Pattern matching helper ─────────────────────────────────────────────

fn has_any_pattern(msg: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| msg.contains(p))
}

// ── Constructor ──────────────────────────────────────────────────────────

fn classified(
    reason: FailoverReason,
    status_code: Option<u16>,
    provider: &str,
    model: &str,
    message: &str,
    error_context: HashMap<String, Value>,
    hints: ActionHints,
) -> ClassifiedError {
    ClassifiedError {
        reason,
        status_code,
        provider: provider.to_string(),
        model: model.to_string(),
        message: message.to_string(),
        error_context,
        retryable: hints.retryable,
        should_compress: hints.should_compress,
        should_rotate_credential: hints.should_rotate_credential,
        should_fallback: hints.should_fallback,
    }
}

// ── Pattern lists (expanded to match Python) ─────────────────────────────

/// Patterns that indicate billing exhaustion (not transient rate limit).
const BILLING_PATTERNS: &[&str] = &[
    "insufficient credits",
    "insufficient_quota",
    "credit balance",
    "credits have been exhausted",
    "top up your credits",
    "payment required",
    "billing hard limit",
    "exceeded your current quota",
    "account is deactivated",
    "plan does not include",
];

/// Patterns that indicate rate limiting (transient, will resolve).
const RATE_LIMIT_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "too many requests",
    "throttl",
    "requests per minute",
    "tokens per minute",
    "requests per day",
    "try again in",
    "please retry after",
    "resource_exhausted",
    // Alibaba/DashScope throttling
    "rate increased too quickly",
    // AWS Bedrock throttling
    "throttlingexception",
    "too many concurrent requests",
    "servicequotaexceededexception",
];

/// Usage-limit patterns that need disambiguation (could be billing OR rate_limit).
const USAGE_LIMIT_PATTERNS: &[&str] = &[
    "usage limit",
    "quota",
    "limit exceeded",
    "key limit exceeded",
];

/// Patterns confirming usage limit is transient (not billing).
const USAGE_LIMIT_TRANSIENT_SIGNALS: &[&str] = &[
    "try again",
    "retry",
    "resets at",
    "reset in",
    "wait",
    "requests remaining",
    "periodic",
    "window",
];

/// Payload-too-large patterns (from message text when no status_code).
const PAYLOAD_TOO_LARGE_PATTERNS: &[&str] = &[
    "request entity too large",
    "payload too large",
    "error code: 413",
];

/// Context overflow patterns (expanded to match Python).
const CONTEXT_OVERFLOW_PATTERNS: &[&str] = &[
    "context length",
    "context size",
    "maximum context",
    "token limit",
    "too many tokens",
    "reduce the length",
    "exceeds the limit",
    "context window",
    "prompt is too long",
    "prompt exceeds max length",
    "max_tokens",
    "maximum number of tokens",
    // vLLM / local inference server patterns
    "exceeds the max_model_len",
    "max_model_len",
    "prompt length",
    "input is too long",
    "maximum model length",
    // Ollama patterns
    "context length exceeded",
    "truncating input",
    // llama.cpp / llama-server patterns
    "slot context",
    "n_ctx_slot",
    // Chinese error messages
    "超过最大长度",
    "上下文长度",
    // AWS Bedrock Converse API patterns
    "max input token",
    "input token",
    "exceeds the maximum number of input tokens",
];

/// Model not found patterns.
const MODEL_NOT_FOUND_PATTERNS: &[&str] = &[
    "is not a valid model",
    "invalid model",
    "model not found",
    "model_not_found",
    "does not exist",
    "no such model",
    "unknown model",
    "unsupported model",
];

/// Auth patterns (non-status-code signals).
const AUTH_PATTERNS: &[&str] = &[
    "invalid api key",
    "invalid_api_key",
    "authentication",
    "unauthorized",
    "forbidden",
    "invalid token",
    "token expired",
    "token revoked",
    "access denied",
];

/// Server disconnect patterns (transport-level, no status code).
const SERVER_DISCONNECT_PATTERNS: &[&str] = &[
    "server disconnected",
    "peer closed connection",
    "connection reset by peer",
    "connection was closed",
    "network connection lost",
    "unexpected eof",
    "incomplete chunked read",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_401() {
        let e = classify_api_error("openrouter", "gpt-4", Some(401), "Invalid API key");
        assert_eq!(e.reason, FailoverReason::Auth);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
        assert!(!e.retryable);
    }

    #[test]
    fn test_billing_402() {
        let e = classify_api_error("openrouter", "gpt-4", Some(402), "Billing: insufficient credits");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_rate_limit_429() {
        let e = classify_api_error("openai", "gpt-4", Some(429), "Rate limit exceeded");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.retryable);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_context_overflow() {
        let e = classify_api_error("anthropic", "claude-3", Some(400), "prompt too long, exceeds context length");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
        assert!(e.should_compress);
    }

    #[test]
    fn test_server_500() {
        let e = classify_api_error("openrouter", "gpt-4", Some(500), "Internal server error");
        assert_eq!(e.reason, FailoverReason::ServerError);
        assert!(e.retryable);
    }

    #[test]
    fn test_timeout() {
        let e = classify_api_error("custom", "llama-3", None, "Request timed out");
        assert_eq!(e.reason, FailoverReason::Timeout);
        assert!(e.retryable);
    }

    #[test]
    fn test_unknown_retryable() {
        let e = classify_api_error("unknown", "model", None, "Something weird happened");
        assert_eq!(e.reason, FailoverReason::Unknown);
        assert!(e.retryable);
    }

    #[test]
    fn test_402_transient() {
        let e = classify_api_error("openrouter", "model", Some(402), "Usage limit exceeded, please try again later");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.retryable);
    }

    #[test]
    fn test_thinking_signature() {
        let e = classify_api_error("anthropic", "claude-3", Some(400), "thinking signature invalid");
        assert_eq!(e.reason, FailoverReason::ThinkingSignature);
        assert!(e.should_fallback);
    }

    // ── New tests for expanded features ─────────────────────────────────

    #[test]
    fn test_403_billing_openrouter_spending_limit() {
        let e = classify_api_error("openrouter", "gpt-4", Some(403), "key limit exceeded");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_403_billing_spending_limit_explicit() {
        let e = classify_api_error("openrouter", "gpt-4", Some(403), "spending limit reached");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_403_generic_auth() {
        let e = classify_api_error("openai", "gpt-4", Some(403), "forbidden");
        assert_eq!(e.reason, FailoverReason::Auth);
        assert!(!e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_429_credential_rotation() {
        let e = classify_api_error("openai", "gpt-4", Some(429), "Rate limit exceeded");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_long_context_tier() {
        let e = classify_api_error("anthropic", "claude-3", Some(429),
            "extra usage charged for long context, tier limit exceeded");
        assert_eq!(e.reason, FailoverReason::LongContextTier);
        assert!(e.retryable);
        assert!(e.should_compress);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_400_model_not_found() {
        let e = classify_api_error("openrouter", "fake-model", Some(400),
            "is not a valid model");
        assert_eq!(e.reason, FailoverReason::ModelNotFound);
        assert!(e.should_fallback);
        assert!(!e.retryable);
    }

    #[test]
    fn test_400_rate_limit_disguised() {
        let e = classify_api_error("dashscope", "qwen-max", Some(400),
            "rate limit exceeded, please try again later");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_400_billing_disguised() {
        let e = classify_api_error("custom", "model", Some(400),
            "insufficient credits");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_400_generic_with_large_session_is_context_overflow() {
        let ctx = ErrorContextParams {
            approx_tokens: 100_000,
            context_length: 200_000,
            num_messages: 100,
        };
        let e = classify_api_error_with_context(
            "anthropic", "claude-3", Some(400), "Error", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
        assert!(e.should_compress);
    }

    #[test]
    fn test_400_generic_with_small_session_is_format_error() {
        let ctx = ErrorContextParams {
            approx_tokens: 1_000,
            context_length: 200_000,
            num_messages: 5,
        };
        let e = classify_api_error_with_context(
            "anthropic", "claude-3", Some(400), "Error", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::FormatError);
        assert!(!e.retryable);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_server_disconnect_large_session_context_overflow() {
        let ctx = ErrorContextParams {
            approx_tokens: 150_000,
            context_length: 200_000,
            num_messages: 250,
        };
        let e = classify_api_error_with_context(
            "openai", "gpt-4", None, "server disconnected unexpectedly", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
        assert!(e.should_compress);
    }

    #[test]
    fn test_server_disconnect_small_session_timeout() {
        let ctx = ErrorContextParams {
            approx_tokens: 2_000,
            context_length: 200_000,
            num_messages: 10,
        };
        let e = classify_api_error_with_context(
            "openai", "gpt-4", None, "server disconnected", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::Timeout);
        assert!(e.retryable);
        assert!(!e.should_compress);
    }

    #[test]
    fn test_context_overflow_vllm_patterns() {
        let e = classify_api_error("local", "llama-3", None,
            "prompt length exceeds the max_model_len");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
        assert!(e.should_compress);
    }

    #[test]
    fn test_context_overflow_chinese_messages() {
        let e = classify_api_error("custom", "qwen", None,
            "超过最大长度，请减少输入");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_context_overflow_ollama() {
        let e = classify_api_error("ollama", "llama3", None,
            "context length exceeded, truncating input");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_rate_limit_aws_bedrock() {
        let e = classify_api_error("aws", "anthropic.claude", None,
            "ThrottlingException: too many concurrent requests");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_rate_limit_dashscope() {
        let e = classify_api_error("dashscope", "qwen-max", None,
            "rate increased too quickly, please retry");
        assert_eq!(e.reason, FailoverReason::RateLimit);
    }

    #[test]
    fn test_billing_from_message_no_status() {
        let e = classify_api_error("custom", "model", None,
            "insufficient credits, account balance is zero");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_usage_limit_transient_without_signal_is_billing() {
        let e = classify_api_error("custom", "model", None,
            "usage limit exceeded");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(!e.retryable);
    }

    #[test]
    fn test_usage_limit_transient_with_signal_is_rate_limit() {
        let e = classify_api_error("custom", "model", None,
            "usage limit exceeded, resets at midnight");
        assert_eq!(e.reason, FailoverReason::RateLimit);
        assert!(e.retryable);
    }

    #[test]
    fn test_payload_too_large_from_message() {
        let e = classify_api_error("proxy", "model", None,
            "request entity too large");
        assert_eq!(e.reason, FailoverReason::PayloadTooLarge);
        assert!(e.should_compress);
    }

    #[test]
    fn test_payload_too_large_413() {
        let e = classify_api_error("proxy", "model", Some(413),
            "payload too large");
        assert_eq!(e.reason, FailoverReason::PayloadTooLarge);
        assert!(e.retryable);
        assert!(e.should_compress);
    }

    #[test]
    fn test_auth_from_message_no_status() {
        let e = classify_api_error("custom", "model", None,
            "invalid api key provided");
        assert_eq!(e.reason, FailoverReason::Auth);
        assert!(!e.retryable);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_model_not_found_from_message() {
        let e = classify_api_error("openrouter", "fake-model", None,
            "model not found: fake-model");
        assert_eq!(e.reason, FailoverReason::ModelNotFound);
        assert!(e.should_fallback);
        assert!(!e.retryable);
    }

    #[test]
    fn test_503_overloaded() {
        let e = classify_api_error("openai", "gpt-4", Some(503), "Service overloaded");
        assert_eq!(e.reason, FailoverReason::Overloaded);
        assert!(e.retryable);
    }

    #[test]
    fn test_529_overloaded() {
        let e = classify_api_error("custom", "model", Some(529), "Site is overloaded");
        assert_eq!(e.reason, FailoverReason::Overloaded);
        assert!(e.retryable);
    }

    #[test]
    fn test_other_4xx_format_error() {
        let e = classify_api_error("custom", "model", Some(422), "Unprocessable entity");
        assert_eq!(e.reason, FailoverReason::FormatError);
        assert!(!e.retryable);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_other_5xx_server_error() {
        let e = classify_api_error("custom", "model", Some(504), "Gateway timeout");
        assert_eq!(e.reason, FailoverReason::ServerError);
        assert!(e.retryable);
    }

    #[test]
    fn test_error_context_field_empty_by_default() {
        let e = classify_api_error("openai", "gpt-4", Some(401), "Invalid API key");
        assert!(e.error_context.is_empty());
    }

    #[test]
    fn test_402_billing_exhaustion_no_transient() {
        let e = classify_api_error("openai", "gpt-4", Some(402),
            "insufficient credits, please top up");
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(!e.retryable);
        assert!(e.should_rotate_credential);
        assert!(e.should_fallback);
    }

    #[test]
    fn test_402_quota_exceeded_no_transient_signal() {
        let e = classify_api_error("openai", "gpt-4", Some(402),
            "quota exceeded");
        // "quota" matches USAGE_LIMIT_PATTERNS but no transient signal
        assert_eq!(e.reason, FailoverReason::Billing);
        assert!(!e.retryable);
    }

    #[test]
    fn test_context_overflow_llamacpp_patterns() {
        let e = classify_api_error("local", "llama", None,
            "slot context: 8192 tokens, prompt 9000 tokens");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_context_overflow_bedrock_patterns() {
        let e = classify_api_error("aws", "anthropic.claude", None,
            "input exceeds the maximum number of input tokens");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_context_overflow_bedrock_max_input_token() {
        let e = classify_api_error("aws", "anthropic.claude", None,
            "max input token limit exceeded");
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_400_context_overflow_threshold_heuristic_80k() {
        // approx_tokens > 80000 should trigger context overflow
        let ctx = ErrorContextParams {
            approx_tokens: 85_000,
            context_length: 200_000,
            num_messages: 10,
        };
        let e = classify_api_error_with_context(
            "anthropic", "claude-3", Some(400), "Error", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_400_context_overflow_threshold_heuristic_num_messages() {
        // num_messages > 80 should trigger context overflow
        let ctx = ErrorContextParams {
            approx_tokens: 5_000,
            context_length: 200_000,
            num_messages: 100,
        };
        let e = classify_api_error_with_context(
            "anthropic", "claude-3", Some(400), "Error", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }

    #[test]
    fn test_400_context_overflow_threshold_40_percent() {
        // approx_tokens > context_length * 0.4
        let ctx = ErrorContextParams {
            approx_tokens: 81_000,
            context_length: 200_000,
            num_messages: 10,
        };
        let e = classify_api_error_with_context(
            "anthropic", "claude-3", Some(400), "Error", &ctx,
        );
        assert_eq!(e.reason, FailoverReason::ContextOverflow);
    }
}
