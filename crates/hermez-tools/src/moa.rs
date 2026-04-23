#![allow(dead_code)]
//! Mixture-of-Agents (MoA) tool.
//!
//! Mirrors the Python `tools/mixture_of_agents_tool.py`.
//! Routes a hard problem through multiple frontier LLMs in parallel,
//! then uses an aggregator model to synthesize a high-quality response.
//!
//! Architecture:
//! 1. Layer 1: Multiple reference models generate diverse responses in parallel
//! 2. Layer 2: Aggregator model synthesizes the best elements into final response

use serde::Serialize;
use serde_json::Value;

use hermez_llm::client::{call_llm, LlmRequest, LlmResponse};
use hermez_llm::error_classifier::ClassifiedError;

use crate::registry::{tool_error, ToolRegistry};

/// Default reference models for generating diverse responses.
const REFERENCE_MODELS: &[&str] = &[
    "anthropic/claude-opus-4-6",
    "google/gemini-3-pro-preview",
    "openai/gpt-5.4-pro",
    "deepseek/deepseek-v3.2",
];

/// Aggregator model for synthesizing reference responses.
const AGGREGATOR_MODEL: &str = "anthropic/claude-opus-4-6";

/// Sampling temperatures.
const REFERENCE_TEMPERATURE: f64 = 0.6;
const AGGREGATOR_TEMPERATURE: f64 = 0.4;

/// Minimum successful reference models needed to proceed.
const MIN_SUCCESSFUL_REFERENCES: usize = 1;

/// Max retries per reference model.
const MAX_RETRIES: usize = 3;

/// System prompt for the aggregator model.
const AGGREGATOR_SYSTEM_PROMPT: &str = "\
You have been provided with a set of responses from various open-source models \
to the latest user query. Your task is to synthesize these responses into a \
single, high-quality response. It is crucial to critically evaluate the \
information provided in these responses, recognizing that some of it may be \
biased or incorrect. Your response should not simply replicate the given \
answers but should offer a refined, accurate, and comprehensive reply to the \
instruction. Ensure your response is well-structured, coherent, and adheres \
to the highest standards of accuracy and reliability.\n\n\
Responses from models:";

/// Result of a single reference model call.
#[derive(Debug, Clone, Serialize)]
struct ReferenceResult {
    model: String,
    content: String,
    success: bool,
}

/// Full MoA result.
#[derive(Debug, Serialize)]
struct MoAResult {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    models_used: ModelsUsed,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    reference_count: usize,
    failed_count: usize,
}

#[derive(Debug, Serialize)]
struct ModelsUsed {
    reference_models: Vec<String>,
    aggregator_model: String,
}

/// Run a single reference model with retry logic (async).
async fn run_reference_model(
    model: &str,
    user_prompt: &str,
    temperature: f64,
) -> ReferenceResult {
    for attempt in 0..MAX_RETRIES {
        let request = LlmRequest {
            model: model.to_string(),
            messages: vec![serde_json::json!({
                "role": "user",
                "content": user_prompt,
            })],
            tools: None,
            temperature: Some(temperature),
            max_tokens: Some(32000),
            base_url: None,
            api_key: None,
            timeout_secs: Some(120),
            provider_preferences: None,
            api_mode: None,
        };

        match call_llm(request).await {
            Ok(resp) => {
                if let Some(content) = resp.content {
                    if !content.is_empty() {
                        tracing::info!("MoA reference {} responded ({} chars)", model, content.len());
                        return ReferenceResult {
                            model: model.to_string(),
                            content,
                            success: true,
                        };
                    }
                    tracing::warn!(
                        "MoA reference {} returned empty content (attempt {}/{})",
                        model, attempt + 1, MAX_RETRIES
                    );
                } else {
                    tracing::warn!(
                        "MoA reference {} returned no content (attempt {}/{})",
                        model, attempt + 1, MAX_RETRIES
                    );
                }
            }
            Err(ref e) => {
                let err_str = e.to_string();
                tracing::warn!(
                    "MoA reference {} error (attempt {}): {}",
                    model, attempt + 1, err_str
                );
            }
        }

        // Exponential backoff: 2s, 4s, 8s
        if attempt < MAX_RETRIES - 1 {
            tokio::time::sleep(std::time::Duration::from_secs(
                2u64.pow((attempt + 1) as u32),
            ))
            .await;
        }
    }

    ReferenceResult {
        model: model.to_string(),
        content: format!("{model} failed after {MAX_RETRIES} attempts"),
        success: false,
    }
}

/// Run the aggregator model to synthesize the final response.
async fn run_aggregator(
    system_prompt: &str,
    user_prompt: &str,
    temperature: f64,
) -> Result<LlmResponse, ClassifiedError> {
    let request = LlmRequest {
        model: AGGREGATOR_MODEL.to_string(),
        messages: vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": user_prompt}),
        ],
        tools: None,
        temperature: Some(temperature),
        max_tokens: Some(32000),
        base_url: None,
        api_key: None,
        timeout_secs: Some(120),
        provider_preferences: None,
        api_mode: None,
    };

    call_llm(request).await
}

/// Construct the aggregator prompt with enumerated responses.
fn construct_aggregator_prompt(system_prompt: &str, responses: &[String]) -> String {
    let response_text = responses
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}", i + 1, r))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{system_prompt}\n\n{response_text}")
}

/// Run the full MoA pipeline (async).
async fn run_moa_async(
    user_prompt: &str,
    ref_models: Vec<String>,
    agg_model: String,
) -> String {
    tracing::info!(
        "MoA: querying {} reference models in parallel, then aggregating",
        ref_models.len()
    );

    // Layer 1: Run reference models in parallel
    let mut join_set = tokio::task::JoinSet::new();
    for model in &ref_models {
        let model = model.clone();
        let prompt = user_prompt.to_string();
        join_set.spawn(async move {
            run_reference_model(&model, &prompt, REFERENCE_TEMPERATURE).await
        });
    }

    // Collect results
    let mut results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(r) => results.push(r),
            Err(e) => {
                tracing::error!("MoA reference task panicked: {e}");
            }
        }
    }

    // Separate successful and failed responses
    let successful_responses: Vec<String> = results
        .iter()
        .filter(|r| r.success)
        .map(|r| r.content.clone())
        .collect();
    let failed_models: Vec<String> = results
        .iter()
        .filter(|r| !r.success)
        .map(|r| r.model.clone())
        .collect();

    let successful_count = successful_responses.len();
    let failed_count = failed_models.len();

    tracing::info!(
        "MoA reference results: {} successful, {} failed",
        successful_count, failed_count
    );

    // Check if we have enough successful responses
    if successful_count < MIN_SUCCESSFUL_REFERENCES {
        return serde_json::json!({
            "success": false,
            "response": "MoA processing failed. Please try again or use a single model for this query.",
            "models_used": {
                "reference_models": ref_models,
                "aggregator_model": agg_model,
            },
            "error": format!(
                "Insufficient successful reference models ({successful_count}/{}). Need at least {MIN_SUCCESSFUL_REFERENCES}.",
                ref_models.len()
            ),
        })
        .to_string();
    }

    // Layer 2: Aggregate responses
    tracing::info!("MoA: aggregating {} responses", successful_count);
    let aggregator_prompt = construct_aggregator_prompt(
        AGGREGATOR_SYSTEM_PROMPT,
        &successful_responses,
    );

    match run_aggregator(&aggregator_prompt, user_prompt, AGGREGATOR_TEMPERATURE).await {
        Ok(resp) => {
            let final_response = resp.content.unwrap_or_default();
            tracing::info!("MoA aggregation complete ({} chars)", final_response.len());

            serde_json::to_string_pretty(&MoAResult {
                success: true,
                response: Some(final_response),
                models_used: ModelsUsed {
                    reference_models: ref_models,
                    aggregator_model: agg_model,
                },
                error: None,
                reference_count: successful_count,
                failed_count,
            })
            .unwrap_or_else(|_| "MoA processing failed: serialization error".to_string())
        }
        Err(e) => {
            tracing::error!("MoA aggregation failed: {e}");
            serde_json::to_string_pretty(&MoAResult {
                success: false,
                response: Some("MoA processing failed. Please try again or use a single model for this query.".to_string()),
                models_used: ModelsUsed {
                    reference_models: ref_models,
                    aggregator_model: agg_model,
                },
                error: Some(format!("Aggregator model failed: {e}")),
                reference_count: successful_count,
                failed_count,
            })
            .unwrap_or_else(|_| "MoA processing failed: serialization error".to_string())
        }
    }
}

/// Handle mixture_of_agents tool call.
pub fn handle_moa(args: Value) -> Result<String, hermez_core::HermezError> {
    let user_prompt = args
        .get("user_prompt")
        .and_then(Value::as_str)
        .unwrap_or("");

    if user_prompt.is_empty() {
        return Ok(tool_error(
            "mixture_of_agents requires a 'user_prompt' parameter — the complex query to solve.",
        ));
    }

    let ref_models: Vec<String> = args
        .get("reference_models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| REFERENCE_MODELS.iter().map(|s| s.to_string()).collect());

    let agg_model = args
        .get("aggregator_model")
        .and_then(Value::as_str)
        .unwrap_or(AGGREGATOR_MODEL)
        .to_string();

    // Validate minimum requirements
    if ref_models.len() < MIN_SUCCESSFUL_REFERENCES {
        return Ok(tool_error(format!(
            "Need at least {MIN_SUCCESSFUL_REFERENCES} reference model, got {}",
            ref_models.len()
        )));
    }

    // Run the async MoA pipeline from sync context
    let user_prompt = user_prompt.to_string();
    let rt = tokio::runtime::Handle::current();

    Ok(rt.block_on(async {
        run_moa_async(&user_prompt, ref_models, agg_model).await
    }))
}

/// Register mixture_of_agents tool.
pub fn register_moa_tool(registry: &mut ToolRegistry) {
    registry.register(
        "mixture_of_agents".to_string(),
        "moa".to_string(),
        serde_json::json!({
            "name": "mixture_of_agents",
            "description": "Route a hard problem through multiple frontier LLMs collaboratively. Makes several API calls (reference models + aggregator) with maximum reasoning effort — use sparingly for genuinely difficult problems. Best for: complex math, advanced algorithms, multi-step analytical reasoning, problems benefiting from diverse perspectives.",
            "parameters": {
                "type": "object",
                "properties": {
                    "user_prompt": {
                        "type": "string",
                        "description": "The complex query or problem to solve using multiple AI models."
                    },
                    "reference_models": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Custom reference models to use (defaults to claude-opus-4-6, gemini-3-pro, gpt-5.4-pro, deepseek-v3.2)."
                    },
                    "aggregator_model": {
                        "type": "string",
                        "description": "Custom aggregator model for synthesis (default: claude-opus-4-6)."
                    }
                },
                "required": ["user_prompt"]
            }
        }),
        std::sync::Arc::new(handle_moa),
        None,
        vec!["OPENROUTER_API_KEY".to_string()],
        "Multi-model collaborative reasoning for hard problems".to_string(),
        "\u{1F9E0}".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_construct_aggregator_prompt() {
        let prompt = construct_aggregator_prompt(
            "Base prompt",
            &["Response 1".to_string(), "Response 2".to_string()],
        );
        assert!(prompt.contains("Base prompt"));
        assert!(prompt.contains("1. Response 1"));
        assert!(prompt.contains("2. Response 2"));
    }

    #[test]
    fn test_construct_aggregator_prompt_empty() {
        let prompt = construct_aggregator_prompt("Base", &[]);
        assert!(prompt.contains("Base"));
    }

    #[test]
    fn test_empty_prompt() {
        let result = handle_moa(serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_moa_result_serialization() {
        let result = MoAResult {
            success: true,
            response: Some("Test response".to_string()),
            models_used: ModelsUsed {
                reference_models: vec!["model1".to_string()],
                aggregator_model: "agg".to_string(),
            },
            error: None,
            reference_count: 1,
            failed_count: 0,
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        assert!(json.contains("model1"));
        assert!(json.contains("Test response"));
        assert!(json.contains("true"));
    }

    #[test]
    fn test_moa_result_serialization_failure() {
        let result = MoAResult {
            success: false,
            response: Some("Failed".to_string()),
            models_used: ModelsUsed {
                reference_models: vec![],
                aggregator_model: "agg".to_string(),
            },
            error: Some("Test error".to_string()),
            reference_count: 0,
            failed_count: 3,
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        assert!(json.contains("false"));
        assert!(json.contains("Test error"));
        assert!(json.contains("3"));
    }

    #[test]
    fn test_reference_models_default() {
        assert_eq!(REFERENCE_MODELS.len(), 4);
        assert!(REFERENCE_MODELS.iter().any(|m| m.contains("claude")));
        assert!(REFERENCE_MODELS.iter().any(|m| m.contains("gemini")));
        assert!(REFERENCE_MODELS.iter().any(|m| m.contains("gpt")));
        assert!(REFERENCE_MODELS.iter().any(|m| m.contains("deepseek")));
    }

    #[test]
    fn test_min_successful_references() {
        assert_eq!(MIN_SUCCESSFUL_REFERENCES, 1);
    }
}
