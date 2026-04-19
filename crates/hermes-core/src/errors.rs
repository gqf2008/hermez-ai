//! Unified error types for the Hermes Agent system.
//!
//! Mirrors Python's exception-based error handling with structured,
//! displayable errors that carry context and chain information.

use std::fmt;

use thiserror::Error;

/// Result alias for HermesError
pub type Result<T> = std::result::Result<T, HermesError>;

/// Error categories for error handling and retry logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// LLM API errors (rate limits, context overflow, invalid requests)
    ApiError,
    /// Authentication / credential errors
    AuthError,
    /// Tool execution errors
    ToolError,
    /// Dangerous command approval errors
    ApprovalError,
    /// Configuration errors
    ConfigError,
    /// Session / database errors
    SessionError,
    /// File system errors
    FileError,
    /// Terminal / subprocess errors
    TerminalError,
    /// Context compression errors
    CompressionError,
    /// Network errors
    NetworkError,
    /// Internal errors (bugs, unexpected states)
    InternalError,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorCategory::ApiError => write!(f, "API Error"),
            ErrorCategory::AuthError => write!(f, "Auth Error"),
            ErrorCategory::ToolError => write!(f, "Tool Error"),
            ErrorCategory::ApprovalError => write!(f, "Approval Error"),
            ErrorCategory::ConfigError => write!(f, "Config Error"),
            ErrorCategory::SessionError => write!(f, "Session Error"),
            ErrorCategory::FileError => write!(f, "File Error"),
            ErrorCategory::TerminalError => write!(f, "Terminal Error"),
            ErrorCategory::CompressionError => write!(f, "Compression Error"),
            ErrorCategory::NetworkError => write!(f, "Network Error"),
            ErrorCategory::InternalError => write!(f, "Internal Error"),
        }
    }
}

/// LLM API error details for retry / failover logic.
#[derive(Debug, Clone)]
pub struct ApiErrorDetails {
    /// HTTP status code if applicable
    pub status_code: Option<u16>,
    /// Provider name (e.g., "openrouter", "anthropic", "openai")
    pub provider: String,
    /// Model name that failed
    pub model: String,
    /// Whether this is retryable
    pub retryable: bool,
    /// Whether to rotate credentials
    pub rotate_credential: bool,
    /// Whether to fall back to next provider
    pub fallback_provider: bool,
}

/// The unified error type for all Hermes operations.
#[derive(Error, Debug)]
pub struct HermesError {
    /// Error category for classification
    pub category: ErrorCategory,
    /// Human-readable message
    pub message: String,
    /// Underlying cause
    pub source: Option<anyhow::Error>,
    /// API error details (if applicable)
    pub api_details: Option<ApiErrorDetails>,
}

impl HermesError {
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            source: None,
            api_details: None,
        }
    }

    pub fn with_source(category: ErrorCategory, message: impl Into<String>, source: anyhow::Error) -> Self {
        Self {
            category,
            message: message.into(),
            source: Some(source),
            api_details: None,
        }
    }

    pub fn api(category: ErrorCategory, message: impl Into<String>, details: ApiErrorDetails) -> Self {
        Self {
            category,
            message: message.into(),
            source: None,
            api_details: Some(details),
        }
    }

    /// Whether this error should trigger a retry
    pub fn is_retryable(&self) -> bool {
        self.api_details
            .as_ref()
            .map(|d| d.retryable)
            .unwrap_or(matches!(
                self.category,
                ErrorCategory::NetworkError | ErrorCategory::ApiError
            ))
    }

    /// Whether to rotate credentials before retry
    pub fn should_rotate(&self) -> bool {
        self.api_details
            .as_ref()
            .map(|d| d.rotate_credential)
            .unwrap_or(false)
    }

    /// Whether to fall back to the next provider
    pub fn should_fallback(&self) -> bool {
        self.api_details
            .as_ref()
            .map(|d| d.fallback_provider)
            .unwrap_or(matches!(self.category, ErrorCategory::ApiError))
    }
}

impl fmt::Display for HermesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.category, self.message)
    }
}

// Conversion from common error types

impl From<anyhow::Error> for HermesError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            category: ErrorCategory::InternalError,
            message: err.to_string(),
            source: Some(err),
            api_details: None,
        }
    }
}

impl From<std::io::Error> for HermesError {
    fn from(err: std::io::Error) -> Self {
        Self {
            category: ErrorCategory::InternalError,
            message: format!("IO error: {err}"),
            source: Some(err.into()),
            api_details: None,
        }
    }
}

impl From<serde_json::Error> for HermesError {
    fn from(err: serde_json::Error) -> Self {
        Self {
            category: ErrorCategory::InternalError,
            message: format!("JSON error: {err}"),
            source: Some(err.into()),
            api_details: None,
        }
    }
}

impl From<String> for HermesError {
    fn from(err: String) -> Self {
        Self::new(ErrorCategory::InternalError, err)
    }
}

impl From<&str> for HermesError {
    fn from(err: &str) -> Self {
        Self::new(ErrorCategory::InternalError, err)
    }
}
