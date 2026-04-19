//! # Hermes LLM
#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//!
//! LLM client library with multi-provider routing, credential pooling,
//! and a 7-tier fallback chain.

pub(crate) mod anthropic;
pub mod auxiliary_client;
pub mod bedrock;
pub mod client;
pub(crate) mod codex;
pub mod credential_pool;
pub mod error_classifier;
pub mod model_metadata;
pub mod model_normalize;
pub mod models_dev;
pub(crate) mod pricing;
pub mod provider;
pub(crate) mod rate_limit;
pub mod reasoning;
pub(crate) mod retry;
pub mod runtime_provider;
pub(crate) mod token_estimate;
pub mod tool_call;

// Re-export key types for convenience.
pub use models_dev::{
    fetch_models_dev, get_model_capabilities, get_model_info, get_provider_info,
    list_agentic_models, list_provider_models, lookup_context, search_models_dev,
    ModelCapabilities, ModelInfo, ModelSearchResult, ProviderInfo,
};
