#![allow(dead_code)]
//! Pricing extraction from API responses.
//!
//! Parses nested JSON for prompt/completion/request/cache pricing fields
//! from provider `/models` endpoint responses.
//! Mirrors the Python `_extract_pricing` function.

use serde::{Deserialize, Serialize};

/// Per-model pricing information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsagePricing {
    /// Cost per million input (prompt) tokens in USD.
    pub prompt_per_million: f64,
    /// Cost per million output (completion) tokens in USD.
    pub completion_per_million: f64,
    /// Cost per million tokens for cached reads (Anthropic).
    pub cache_read_per_million: Option<f64>,
    /// Cost per million tokens for cache writes (Anthropic).
    pub cache_write_per_million: Option<f64>,
    /// Cost per request (fixed, rare).
    pub per_request: Option<f64>,
}

impl UsagePricing {
    /// Estimate cost for a given token count.
    pub fn estimate_cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        let input_cost = (input_tokens as f64 / 1_000_000.0) * self.prompt_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * self.completion_per_million;
        input_cost + output_cost
    }

    /// Whether any pricing fields are set.
    pub fn is_set(&self) -> bool {
        self.prompt_per_million > 0.0 || self.completion_per_million > 0.0
    }
}

/// Extract pricing information from a provider API response.
///
/// Walks nested dicts looking for common field names:
/// `prompt_token_cost_usd`, `completion_token_cost_usd`,
/// `input_cost_per_token`, `output_cost_per_token`,
/// `cache_read_cost_per_token`, `cache_write_cost_per_token`, etc.
pub fn extract_pricing(response: &serde_json::Value) -> Option<UsagePricing> {
    let obj = response.as_object()?;

    // Direct fields
    let mut pricing = UsagePricing::default();

    // OpenRouter style: nested under `pricing`
    if let Some(pricing_obj) = obj.get("pricing").and_then(|v| v.as_object()) {
        pricing.prompt_per_million = find_pricing_field(pricing_obj, &["input_cost_per_token", "prompt_token_cost_usd"])
            .map(|v| v * 1_000_000.0)
            .unwrap_or(0.0);
        pricing.completion_per_million = find_pricing_field(pricing_obj, &["output_cost_per_token", "completion_token_cost_usd"])
            .map(|v| v * 1_000_000.0)
            .unwrap_or(0.0);
        pricing.cache_read_per_million = find_pricing_field(pricing_obj, &["cache_read_cost_per_token", "cache_read_input_tokens_cost_per_token"])
            .map(|v| v * 1_000_000.0);
        pricing.cache_write_per_million = find_pricing_field(pricing_obj, &["cache_write_cost_per_token", "cache_creation_input_tokens_cost_per_token"])
            .map(|v| v * 1_000_000.0);
        pricing.per_request = find_pricing_field(pricing_obj, &["per_request_cost_usd"]);
    }

    // Top-level fields (some providers)
    if pricing.prompt_per_million == 0.0 {
        pricing.prompt_per_million = find_pricing_field_in_obj(obj, &["input_cost_per_token", "prompt_token_cost_usd"])
            .map(|v| v * 1_000_000.0)
            .unwrap_or(0.0);
    }
    if pricing.completion_per_million == 0.0 {
        pricing.completion_per_million = find_pricing_field_in_obj(obj, &["output_cost_per_token", "completion_token_cost_usd"])
            .map(|v| v * 1_000_000.0)
            .unwrap_or(0.0);
    }

    if pricing.is_set() {
        Some(pricing)
    } else {
        None
    }
}

fn find_pricing_field(obj: &serde_json::Map<String, serde_json::Value>, candidates: &[&str]) -> Option<f64> {
    for &name in candidates {
        if let Some(v) = obj.get(name).and_then(|v| v.as_f64()) {
            return Some(v);
        }
    }
    None
}

fn find_pricing_field_in_obj(obj: &serde_json::Map<String, serde_json::Value>, candidates: &[&str]) -> Option<f64> {
    for &name in candidates {
        if let Some(v) = obj.get(name).and_then(|v| v.as_f64()) {
            return Some(v);
        }
    }
    None
}

/// Default pricing for known model families (fallback when API doesn't provide pricing).
/// Values are approximate and per-million tokens.
pub fn default_pricing(model: &str) -> UsagePricing {
    let ml = model.to_lowercase();
    if ml.contains("gpt-4o") {
        UsagePricing {
            prompt_per_million: 2.50,
            completion_per_million: 10.00,
            ..Default::default()
        }
    } else if ml.contains("gpt-4") {
        UsagePricing {
            prompt_per_million: 10.00,
            completion_per_million: 30.00,
            ..Default::default()
        }
    } else if ml.contains("gpt-3.5") {
        UsagePricing {
            prompt_per_million: 0.50,
            completion_per_million: 1.50,
            ..Default::default()
        }
    } else if ml.contains("claude-3-5-sonnet") || ml.contains("claude-3-sonnet") {
        UsagePricing {
            prompt_per_million: 3.00,
            completion_per_million: 15.00,
            cache_read_per_million: Some(0.30),
            cache_write_per_million: Some(3.75),
            per_request: None,
        }
    } else if ml.contains("claude-3-opus") || ml.contains("claude-opus") {
        UsagePricing {
            prompt_per_million: 15.00,
            completion_per_million: 75.00,
            cache_read_per_million: Some(1.50),
            cache_write_per_million: Some(18.75),
            per_request: None,
        }
    } else if ml.contains("claude-3-haiku") || ml.contains("claude-haiku") {
        UsagePricing {
            prompt_per_million: 0.25,
            completion_per_million: 1.25,
            cache_read_per_million: Some(0.03),
            cache_write_per_million: Some(0.30),
            per_request: None,
        }
    } else {
        UsagePricing::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_pricing_openrouter() {
        let response = serde_json::json!({
            "pricing": {
                "input_cost_per_token": 0.0000025,
                "output_cost_per_token": 0.000010,
                "cache_read_input_tokens_cost_per_token": 0.0000003,
                "cache_creation_input_tokens_cost_per_token": 0.00000375,
            }
        });
        let pricing = extract_pricing(&response).unwrap();
        assert!((pricing.prompt_per_million - 2.5).abs() < 0.01);
        assert!((pricing.completion_per_million - 10.0).abs() < 0.01);
        assert!(pricing.cache_read_per_million.unwrap() > 0.0);
    }

    #[test]
    fn test_extract_pricing_none() {
        let response = serde_json::json!({});
        assert!(extract_pricing(&response).is_none());
    }

    #[test]
    fn test_default_pricing_gpt4o() {
        let pricing = default_pricing("gpt-4o-mini");
        assert!((pricing.prompt_per_million - 2.50).abs() < 0.01);
    }

    #[test]
    fn test_default_pricing_claude() {
        let pricing = default_pricing("claude-3-5-sonnet-20241022");
        assert!((pricing.prompt_per_million - 3.00).abs() < 0.01);
        assert!(pricing.cache_read_per_million.is_some());
    }

    #[test]
    fn test_default_pricing_unknown() {
        let pricing = default_pricing("some-unknown-model");
        assert!(!pricing.is_set());
    }

    #[test]
    fn test_estimate_cost() {
        let pricing = UsagePricing {
            prompt_per_million: 2.50,
            completion_per_million: 10.00,
            ..Default::default()
        };
        let cost = pricing.estimate_cost(1_000_000, 500_000);
        assert!((cost - 7.50).abs() < 0.01);
    }
}
