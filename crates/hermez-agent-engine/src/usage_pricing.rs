#![allow(dead_code)]
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Status of the cost calculation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostStatus {
    Actual,
    Estimated,
    Included,
    Unknown,
}

impl CostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CostStatus::Actual => "actual",
            CostStatus::Estimated => "estimated",
            CostStatus::Included => "included",
            CostStatus::Unknown => "unknown",
        }
    }
}

/// Source of pricing data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostSource {
    ProviderCostApi,
    ProviderGenerationApi,
    ProviderModelsApi,
    OfficialDocsSnapshot,
    UserOverride,
    CustomContract,
    None,
}

impl CostSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CostSource::ProviderCostApi => "provider_cost_api",
            CostSource::ProviderGenerationApi => "provider_generation_api",
            CostSource::ProviderModelsApi => "provider_models_api",
            CostSource::OfficialDocsSnapshot => "official_docs_snapshot",
            CostSource::UserOverride => "user_override",
            CostSource::CustomContract => "custom_contract",
            CostSource::None => "none",
        }
    }
}

/// Pricing entry for a model. Cache costs are optional since not all
/// providers support or publish cache pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingEntry {
    pub input_cost_per_million: Option<f64>,
    pub output_cost_per_million: Option<f64>,
    pub cache_read_cost_per_million: Option<f64>,
    pub cache_write_cost_per_million: Option<f64>,
    pub request_cost: Option<f64>,
    pub source: CostSource,
    pub source_url: Option<String>,
    pub pricing_version: Option<String>,
}

/// Canonical usage bucket — normalized across all API response shapes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanonicalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub request_count: u64,
}

impl CanonicalUsage {
    /// Total prompt tokens (input + cache components).
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// Total tokens across the request.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output_tokens
    }
}

/// Billing route resolved from model name + provider + base_url.
#[derive(Debug, Clone)]
pub struct BillingRoute {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub billing_mode: String,
}

/// Result of a cost calculation.
#[derive(Debug, Clone)]
pub struct CostResult {
    pub amount_usd: Option<f64>,
    pub status: CostStatus,
    pub source: CostSource,
    pub label: String,
    pub pricing_version: Option<String>,
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Official docs snapshot pricing table — matches Python _OFFICIAL_DOCS_PRICING
// ---------------------------------------------------------------------------

static OFFICIAL_DOCS_PRICING: Lazy<HashMap<(&'static str, &'static str), PricingEntry>> =
    Lazy::new(|| {
        let mut m: HashMap<(&'static str, &'static str), PricingEntry> = HashMap::new();

        // ----- Anthropic -----
        m.insert(
            ("anthropic", "claude-opus-4-20250514"),
            PricingEntry {
                input_cost_per_million: Some(15.00),
                output_cost_per_million: Some(75.00),
                cache_read_cost_per_million: Some(1.50),
                cache_write_cost_per_million: Some(18.75),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-prompt-caching-2026-03-16".into()),
            },
        );
        m.insert(
            ("anthropic", "claude-sonnet-4-20250514"),
            PricingEntry {
                input_cost_per_million: Some(3.00),
                output_cost_per_million: Some(15.00),
                cache_read_cost_per_million: Some(0.30),
                cache_write_cost_per_million: Some(3.75),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-prompt-caching-2026-03-16".into()),
            },
        );
        m.insert(
            ("anthropic", "claude-3-5-sonnet-20241022"),
            PricingEntry {
                input_cost_per_million: Some(3.00),
                output_cost_per_million: Some(15.00),
                cache_read_cost_per_million: Some(0.30),
                cache_write_cost_per_million: Some(3.75),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("anthropic", "claude-3-5-haiku-20241022"),
            PricingEntry {
                input_cost_per_million: Some(0.80),
                output_cost_per_million: Some(4.00),
                cache_read_cost_per_million: Some(0.08),
                cache_write_cost_per_million: Some(1.00),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("anthropic", "claude-3-opus-20240229"),
            PricingEntry {
                input_cost_per_million: Some(15.00),
                output_cost_per_million: Some(75.00),
                cache_read_cost_per_million: Some(1.50),
                cache_write_cost_per_million: Some(18.75),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("anthropic", "claude-3-haiku-20240307"),
            PricingEntry {
                input_cost_per_million: Some(0.25),
                output_cost_per_million: Some(1.25),
                cache_read_cost_per_million: Some(0.03),
                cache_write_cost_per_million: Some(0.30),
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some(
                    "https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching".into(),
                ),
                pricing_version: Some("anthropic-pricing-2026-03-16".into()),
            },
        );

        // ----- OpenAI -----
        m.insert(
            ("openai", "gpt-4o"),
            PricingEntry {
                input_cost_per_million: Some(2.50),
                output_cost_per_million: Some(10.00),
                cache_read_cost_per_million: Some(1.25),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "gpt-4o-mini"),
            PricingEntry {
                input_cost_per_million: Some(0.15),
                output_cost_per_million: Some(0.60),
                cache_read_cost_per_million: Some(0.075),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "gpt-4.1"),
            PricingEntry {
                input_cost_per_million: Some(2.00),
                output_cost_per_million: Some(8.00),
                cache_read_cost_per_million: Some(0.50),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "gpt-4.1-mini"),
            PricingEntry {
                input_cost_per_million: Some(0.40),
                output_cost_per_million: Some(1.60),
                cache_read_cost_per_million: Some(0.10),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "gpt-4.1-nano"),
            PricingEntry {
                input_cost_per_million: Some(0.10),
                output_cost_per_million: Some(0.40),
                cache_read_cost_per_million: Some(0.025),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "o3"),
            PricingEntry {
                input_cost_per_million: Some(10.00),
                output_cost_per_million: Some(40.00),
                cache_read_cost_per_million: Some(2.50),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("openai", "o3-mini"),
            PricingEntry {
                input_cost_per_million: Some(1.10),
                output_cost_per_million: Some(4.40),
                cache_read_cost_per_million: Some(0.55),
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://openai.com/api/pricing/".into()),
                pricing_version: Some("openai-pricing-2026-03-16".into()),
            },
        );

        // ----- DeepSeek -----
        m.insert(
            ("deepseek", "deepseek-chat"),
            PricingEntry {
                input_cost_per_million: Some(0.14),
                output_cost_per_million: Some(0.28),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://api-docs.deepseek.com/quick_start/pricing".into()),
                pricing_version: Some("deepseek-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("deepseek", "deepseek-reasoner"),
            PricingEntry {
                input_cost_per_million: Some(0.55),
                output_cost_per_million: Some(2.19),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://api-docs.deepseek.com/quick_start/pricing".into()),
                pricing_version: Some("deepseek-pricing-2026-03-16".into()),
            },
        );

        // ----- Google Gemini -----
        m.insert(
            ("google", "gemini-2.5-pro"),
            PricingEntry {
                input_cost_per_million: Some(1.25),
                output_cost_per_million: Some(10.00),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://ai.google.dev/pricing".into()),
                pricing_version: Some("google-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("google", "gemini-2.5-flash"),
            PricingEntry {
                input_cost_per_million: Some(0.15),
                output_cost_per_million: Some(0.60),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://ai.google.dev/pricing".into()),
                pricing_version: Some("google-pricing-2026-03-16".into()),
            },
        );
        m.insert(
            ("google", "gemini-2.0-flash"),
            PricingEntry {
                input_cost_per_million: Some(0.10),
                output_cost_per_million: Some(0.40),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://ai.google.dev/pricing".into()),
                pricing_version: Some("google-pricing-2026-03-16".into()),
            },
        );

        // ----- AWS Bedrock -----
        m.insert(
            ("bedrock", "anthropic.claude-opus-4-6"),
            PricingEntry {
                input_cost_per_million: Some(15.00),
                output_cost_per_million: Some(75.00),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "anthropic.claude-sonnet-4-6"),
            PricingEntry {
                input_cost_per_million: Some(3.00),
                output_cost_per_million: Some(15.00),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "anthropic.claude-sonnet-4-5"),
            PricingEntry {
                input_cost_per_million: Some(3.00),
                output_cost_per_million: Some(15.00),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "anthropic.claude-haiku-4-5"),
            PricingEntry {
                input_cost_per_million: Some(0.80),
                output_cost_per_million: Some(4.00),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "amazon.nova-pro"),
            PricingEntry {
                input_cost_per_million: Some(0.80),
                output_cost_per_million: Some(3.20),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "amazon.nova-lite"),
            PricingEntry {
                input_cost_per_million: Some(0.06),
                output_cost_per_million: Some(0.24),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );
        m.insert(
            ("bedrock", "amazon.nova-micro"),
            PricingEntry {
                input_cost_per_million: Some(0.035),
                output_cost_per_million: Some(0.14),
                cache_read_cost_per_million: None,
                cache_write_cost_per_million: None,
                request_cost: None,
                source: CostSource::OfficialDocsSnapshot,
                source_url: Some("https://aws.amazon.com/bedrock/pricing/".into()),
                pricing_version: Some("bedrock-pricing-2026-04".into()),
            },
        );

        m
    });

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve a billing route from model name, provider, and base URL.
/// Mirrors Python `resolve_billing_route`.
pub fn resolve_billing_route(
    model_name: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> BillingRoute {
    let provider_name = provider.unwrap_or("").trim().to_lowercase();
    let base = base_url.unwrap_or("").trim().to_lowercase();
    let model = model_name.trim();

    // Infer provider from model prefix if not given
    let (provider_name, model) = if provider_name.is_empty() && model.contains('/') {
        if let Some(pos) = model.find('/') {
            let inferred = &model[..pos];
            if matches!(inferred, "anthropic" | "openai" | "google") {
                let bare = &model[pos + 1..];
                (inferred.to_string(), bare.to_string())
            } else {
                (String::new(), model.to_string())
            }
        } else {
            (String::new(), model.to_string())
        }
    } else {
        (provider_name, model.to_string())
    };

    let billing_mode = match provider_name.as_str() {
        "openai-codex" => {
            return BillingRoute {
                provider: "openai-codex".into(),
                model,
                base_url: base_url.unwrap_or("").into(),
                billing_mode: "subscription_included".into(),
            };
        }
        "openrouter" => {
            if base.contains("openrouter.ai") || provider_name == "openrouter" {
                "official_models_api"
            } else {
                ""
            }
        }
        "anthropic" | "openai" => "official_docs_snapshot",
        "custom" | "local" => {
            if base.contains("localhost") {
                "unknown"
            } else {
                ""
            }
        }
        _ => {
            if base.contains("localhost") {
                "unknown"
            } else {
                ""
            }
        }
    };

    let billing_mode = if billing_mode.is_empty() {
        "unknown"
    } else {
        billing_mode
    }
    .to_string();

    // Strip provider prefix for the model in the route
    let bare_model = if let Some(pos) = model.rfind('/') {
        model[pos + 1..].to_string()
    } else {
        model
    };

    BillingRoute {
        provider: provider_name,
        model: bare_model,
        base_url: base_url.unwrap_or("").into(),
        billing_mode,
    }
}

/// Look up pricing from the official docs snapshot table.
pub fn lookup_official_docs_pricing(route: &BillingRoute) -> Option<PricingEntry> {
    OFFICIAL_DOCS_PRICING
        .get(&(route.provider.as_str(), route.model.to_lowercase().as_str()))
        .cloned()
}

/// Create a zero-cost pricing entry for subscription-included routes.
fn zero_pricing_entry() -> PricingEntry {
    PricingEntry {
        input_cost_per_million: Some(0.0),
        output_cost_per_million: Some(0.0),
        cache_read_cost_per_million: Some(0.0),
        cache_write_cost_per_million: Some(0.0),
        request_cost: None,
        source: CostSource::None,
        source_url: None,
        pricing_version: Some("included-route".into()),
    }
}

/// Get pricing entry for a model + provider + base_url combination.
/// Mirrors Python `get_pricing_entry`.
pub fn get_pricing_entry(
    model_name: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> Option<PricingEntry> {
    let route = resolve_billing_route(model_name, provider, base_url);

    if route.billing_mode == "subscription_included" {
        return Some(zero_pricing_entry());
    }

    // OpenRouter pricing would come from their models API — not implemented here.
    if route.provider == "openrouter" {
        return None;
    }

    // Try official docs snapshot
    lookup_official_docs_pricing(&route)
}

/// Normalize raw API response usage into canonical token buckets.
/// Handles three API shapes:
/// - Anthropic: input_tokens/output_tokens/cache_read_input_tokens/cache_creation_input_tokens
/// - Codex Responses: input_tokens includes cache tokens
/// - OpenAI Chat Completions: prompt_tokens includes cache tokens
pub fn normalize_usage(
    usage: &serde_json::Value,
    provider: &str,
    api_mode: Option<&str>,
) -> CanonicalUsage {
    let provider_name = provider.trim().to_lowercase();
    let mode = api_mode.map(|s| s.trim().to_lowercase()).unwrap_or_default();

    if mode == "anthropic_messages" || provider_name == "anthropic" {
        let input_tokens = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_write_tokens = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let reasoning_tokens = usage
            .get("reasoning_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        CanonicalUsage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            request_count: 1,
        }
    } else if mode == "codex_responses" {
        let input_total = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_write_tokens = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_creation_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let input_tokens = input_total
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_write_tokens);
        CanonicalUsage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens: 0,
            request_count: 1,
        }
    } else {
        // Default: OpenAI Chat Completions shape
        let prompt_total = usage
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_write_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let input_tokens = prompt_total
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_write_tokens);
        let reasoning_tokens = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .or_else(|| {
                usage
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
            })
            .unwrap_or(0);
        CanonicalUsage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            request_count: 1,
        }
    }
}

/// Estimate the usage cost for a given model and normalized usage.
/// Mirrors Python `estimate_usage_cost`.
pub fn estimate_usage_cost(
    model_name: &str,
    usage: &CanonicalUsage,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> CostResult {
    let route = resolve_billing_route(model_name, provider, base_url);

    if route.billing_mode == "subscription_included" {
        return CostResult {
            amount_usd: Some(0.0),
            status: CostStatus::Included,
            source: CostSource::None,
            label: "included".into(),
            pricing_version: Some("included-route".into()),
            notes: Vec::new(),
        };
    }

    let entry = match get_pricing_entry(model_name, provider, base_url) {
        Some(e) => e,
        None => {
            return CostResult {
                amount_usd: None,
                status: CostStatus::Unknown,
                source: CostSource::None,
                label: "n/a".into(),
                pricing_version: None,
                notes: Vec::new(),
            }
        }
    };

    let mut notes = Vec::new();

    // If we have tokens but no pricing for that token type, return unknown
    if usage.input_tokens > 0 && entry.input_cost_per_million.is_none() {
        return CostResult {
            amount_usd: None,
            status: CostStatus::Unknown,
            source: entry.source,
            label: "n/a".into(),
            pricing_version: entry.pricing_version,
            notes: Vec::new(),
        };
    }
    if usage.output_tokens > 0 && entry.output_cost_per_million.is_none() {
        return CostResult {
            amount_usd: None,
            status: CostStatus::Unknown,
            source: entry.source,
            label: "n/a".into(),
            pricing_version: entry.pricing_version,
            notes: Vec::new(),
        };
    }
    if usage.cache_read_tokens > 0 && entry.cache_read_cost_per_million.is_none() {
        return CostResult {
            amount_usd: None,
            status: CostStatus::Unknown,
            source: entry.source,
            label: "n/a".into(),
            pricing_version: entry.pricing_version.clone(),
            notes: vec!["cache-read pricing unavailable for route".into()],
        };
    }
    if usage.cache_write_tokens > 0 && entry.cache_write_cost_per_million.is_none() {
        return CostResult {
            amount_usd: None,
            status: CostStatus::Unknown,
            source: entry.source,
            label: "n/a".into(),
            pricing_version: entry.pricing_version.clone(),
            notes: vec!["cache-write pricing unavailable for route".into()],
        };
    }

    let mut amount = 0.0_f64;

    if let Some(price) = entry.input_cost_per_million {
        amount += usage.input_tokens as f64 * price / 1_000_000.0;
    }
    if let Some(price) = entry.output_cost_per_million {
        amount += usage.output_tokens as f64 * price / 1_000_000.0;
    }
    if let Some(price) = entry.cache_read_cost_per_million {
        amount += usage.cache_read_tokens as f64 * price / 1_000_000.0;
    }
    if let Some(price) = entry.cache_write_cost_per_million {
        amount += usage.cache_write_tokens as f64 * price / 1_000_000.0;
    }
    if let Some(price) = entry.request_cost {
        amount += usage.request_count as f64 * price;
    }

    let status = if entry.source == CostSource::None && amount == 0.0 {
        CostStatus::Included
    } else {
        CostStatus::Estimated
    };

    let label = format!("~${amount:.2}");

    if route.provider == "openrouter" {
        notes.push("OpenRouter cost is estimated from the models API until reconciled.".into());
    }

    CostResult {
        amount_usd: Some(amount),
        status,
        source: entry.source,
        label,
        pricing_version: entry.pricing_version,
        notes,
    }
}

/// Check whether we have pricing data for this model + route.
/// Mirrors Python `has_known_pricing`.
pub fn has_known_pricing(
    model_name: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> bool {
    let route = resolve_billing_route(model_name, provider, base_url);
    if route.billing_mode == "subscription_included" {
        return true;
    }
    get_pricing_entry(model_name, provider, base_url).is_some()
}

/// Format a duration in seconds as a compact human-readable string.
/// Mirrors Python `format_duration_compact`.
pub fn format_duration_compact(seconds: f64) -> String {
    if seconds < 60.0 {
        return format!("{}s", seconds as u64);
    }
    let minutes = seconds / 60.0;
    if minutes < 60.0 {
        return format!("{}m", minutes as u64);
    }
    let hours = minutes / 60.0;
    if hours < 24.0 {
        let remaining_min = (minutes as u64) % 60;
        if remaining_min > 0 {
            return format!("{}h {}m", hours as u64, remaining_min);
        }
        return format!("{}h", hours as u64);
    }
    let days = hours / 24.0;
    format!("{:.1}d", days)
}

/// Format a token count as a compact human-readable string.
/// Mirrors Python `format_token_count_compact`.
pub fn format_token_count_compact(value: i64) -> String {
    let abs_value = value.unsigned_abs();
    if abs_value < 1_000 {
        return value.to_string();
    }

    let sign = if value < 0 { "-" } else { "" };

    let units: [(u64, &str); 3] = [
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "K"),
    ];

    for &(threshold, suffix) in &units {
        if abs_value >= threshold {
            let scaled = abs_value as f64 / threshold as f64;
            let text = if scaled < 10.0 {
                format!("{:.2}", scaled)
            } else if scaled < 100.0 {
                format!("{:.1}", scaled)
            } else {
                format!("{:.0}", scaled)
            };
            // Strip trailing zeros
            let text = if text.contains('.') {
                let t = text.trim_end_matches('0').trim_end_matches('.');
                t.to_string()
            } else {
                text
            };
            return format!("{}{}{}", sign, text, suffix);
        }
    }

    // Fallback: shouldn't happen given the abs_value < 1_000 check above
    value.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_usage tests ---

    #[test]
    fn test_normalize_anthropic() {
        let json = serde_json::json!({
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 300,
        });
        let u = normalize_usage(&json, "anthropic", None);
        assert_eq!(u.input_tokens, 1000);
        assert_eq!(u.output_tokens, 500);
        assert_eq!(u.cache_read_tokens, 200);
        assert_eq!(u.cache_write_tokens, 300);
    }

    #[test]
    fn test_normalize_openai() {
        let json = serde_json::json!({
            "prompt_tokens": 1000,
            "completion_tokens": 500,
            "prompt_tokens_details": {"cached_tokens": 200},
            "completion_tokens_details": {"reasoning_tokens": 100},
        });
        let u = normalize_usage(&json, "openai", None);
        assert_eq!(u.input_tokens, 800);
        assert_eq!(u.output_tokens, 500);
        assert_eq!(u.cache_read_tokens, 200);
        assert_eq!(u.cache_write_tokens, 0);
        assert_eq!(u.reasoning_tokens, 100);
    }

    #[test]
    fn test_normalize_codex_responses() {
        let json = serde_json::json!({
            "input_tokens": 1500,
            "output_tokens": 300,
            "input_tokens_details": {"cached_tokens": 500},
        });
        let u = normalize_usage(&json, "openai", Some("codex_responses"));
        assert_eq!(u.input_tokens, 1000);
        assert_eq!(u.output_tokens, 300);
        assert_eq!(u.cache_read_tokens, 500);
    }

    #[test]
    fn test_normalize_null_usage() {
        let json = serde_json::json!({});
        let u = normalize_usage(&json, "unknown", None);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
    }

    // --- resolve_billing_route tests ---

    #[test]
    fn test_route_anthropic() {
        let r = resolve_billing_route("claude-sonnet-4-20250514", Some("anthropic"), None);
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.billing_mode, "official_docs_snapshot");
    }

    #[test]
    fn test_route_openrouter() {
        let r = resolve_billing_route(
            "anthropic/claude-sonnet-4-20250514",
            Some("openrouter"),
            Some("https://openrouter.ai/api/v1"),
        );
        assert_eq!(r.provider, "openrouter");
        assert_eq!(r.billing_mode, "official_models_api");
    }

    #[test]
    fn test_route_codex_subscription() {
        let r = resolve_billing_route("gpt-4o", Some("openai-codex"), None);
        assert_eq!(r.billing_mode, "subscription_included");
    }

    #[test]
    fn test_route_infer_provider_from_model() {
        let r = resolve_billing_route("anthropic/claude-sonnet-4", None, None);
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.model, "claude-sonnet-4");
    }

    #[test]
    fn test_route_custom_localhost() {
        let r = resolve_billing_route("my-model", Some("custom"), Some("http://localhost:8080"));
        assert_eq!(r.billing_mode, "unknown");
    }

    // --- get_pricing_entry tests ---

    #[test]
    fn test_pricing_anthropic_opus4() {
        let e = get_pricing_entry("claude-opus-4-20250514", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(15.0));
        assert_eq!(e.output_cost_per_million, Some(75.0));
        assert_eq!(e.cache_read_cost_per_million, Some(1.50));
        assert_eq!(e.cache_write_cost_per_million, Some(18.75));
    }

    #[test]
    fn test_pricing_anthropic_sonnet4() {
        let e = get_pricing_entry("claude-sonnet-4-20250514", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(3.0));
        assert_eq!(e.output_cost_per_million, Some(15.0));
        assert_eq!(e.cache_read_cost_per_million, Some(0.30));
        assert_eq!(e.cache_write_cost_per_million, Some(3.75));
    }

    #[test]
    fn test_pricing_anthropic_35_sonnet() {
        let e = get_pricing_entry("claude-3-5-sonnet-20241022", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(3.0));
        assert_eq!(e.cache_write_cost_per_million, Some(3.75));
    }

    #[test]
    fn test_pricing_anthropic_35_haiku() {
        let e = get_pricing_entry("claude-3-5-haiku-20241022", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.80));
        assert_eq!(e.output_cost_per_million, Some(4.0));
    }

    #[test]
    fn test_pricing_anthropic_opus3() {
        let e = get_pricing_entry("claude-3-opus-20240229", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(15.0));
        assert_eq!(e.output_cost_per_million, Some(75.0));
    }

    #[test]
    fn test_pricing_anthropic_haiku3() {
        let e = get_pricing_entry("claude-3-haiku-20240307", Some("anthropic"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.25));
        assert_eq!(e.output_cost_per_million, Some(1.25));
    }

    #[test]
    fn test_pricing_openai_gpt4o() {
        let e = get_pricing_entry("gpt-4o", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(2.50));
        assert_eq!(e.output_cost_per_million, Some(10.0));
        assert_eq!(e.cache_read_cost_per_million, Some(1.25));
        assert!(e.cache_write_cost_per_million.is_none());
    }

    #[test]
    fn test_pricing_openai_gpt4o_mini() {
        let e = get_pricing_entry("gpt-4o-mini", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.15));
        assert_eq!(e.output_cost_per_million, Some(0.60));
    }

    #[test]
    fn test_pricing_openai_gpt41() {
        let e = get_pricing_entry("gpt-4.1", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(2.0));
        assert_eq!(e.output_cost_per_million, Some(8.0));
    }

    #[test]
    fn test_pricing_openai_gpt41_mini() {
        let e = get_pricing_entry("gpt-4.1-mini", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.40));
        assert_eq!(e.output_cost_per_million, Some(1.60));
    }

    #[test]
    fn test_pricing_openai_gpt41_nano() {
        let e = get_pricing_entry("gpt-4.1-nano", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.10));
        assert_eq!(e.output_cost_per_million, Some(0.40));
    }

    #[test]
    fn test_pricing_openai_o3() {
        let e = get_pricing_entry("o3", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(10.0));
        assert_eq!(e.output_cost_per_million, Some(40.0));
    }

    #[test]
    fn test_pricing_openai_o3_mini() {
        let e = get_pricing_entry("o3-mini", Some("openai"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(1.10));
        assert_eq!(e.output_cost_per_million, Some(4.40));
    }

    #[test]
    fn test_pricing_deepseek_chat() {
        let e = get_pricing_entry("deepseek-chat", Some("deepseek"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.14));
        assert_eq!(e.output_cost_per_million, Some(0.28));
    }

    #[test]
    fn test_pricing_deepseek_reasoner() {
        let e = get_pricing_entry("deepseek-reasoner", Some("deepseek"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.55));
        assert_eq!(e.output_cost_per_million, Some(2.19));
    }

    #[test]
    fn test_pricing_google_gemini_25_pro() {
        let e = get_pricing_entry("gemini-2.5-pro", Some("google"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(1.25));
        assert_eq!(e.output_cost_per_million, Some(10.0));
    }

    #[test]
    fn test_pricing_google_gemini_25_flash() {
        let e = get_pricing_entry("gemini-2.5-flash", Some("google"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.15));
        assert_eq!(e.output_cost_per_million, Some(0.60));
    }

    #[test]
    fn test_pricing_google_gemini_20_flash() {
        let e = get_pricing_entry("gemini-2.0-flash", Some("google"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.10));
        assert_eq!(e.output_cost_per_million, Some(0.40));
    }

    #[test]
    fn test_pricing_bedrock_opus46() {
        let e = get_pricing_entry("anthropic.claude-opus-4-6", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(15.0));
        assert_eq!(e.output_cost_per_million, Some(75.0));
    }

    #[test]
    fn test_pricing_bedrock_sonnet46() {
        let e = get_pricing_entry("anthropic.claude-sonnet-4-6", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(3.0));
        assert_eq!(e.output_cost_per_million, Some(15.0));
    }

    #[test]
    fn test_pricing_bedrock_sonnet45() {
        let e = get_pricing_entry("anthropic.claude-sonnet-4-5", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(3.0));
        assert_eq!(e.output_cost_per_million, Some(15.0));
    }

    #[test]
    fn test_pricing_bedrock_haiku45() {
        let e = get_pricing_entry("anthropic.claude-haiku-4-5", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.80));
        assert_eq!(e.output_cost_per_million, Some(4.0));
    }

    #[test]
    fn test_pricing_bedrock_nova_pro() {
        let e = get_pricing_entry("amazon.nova-pro", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.80));
        assert_eq!(e.output_cost_per_million, Some(3.20));
    }

    #[test]
    fn test_pricing_bedrock_nova_lite() {
        let e = get_pricing_entry("amazon.nova-lite", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.06));
        assert_eq!(e.output_cost_per_million, Some(0.24));
    }

    #[test]
    fn test_pricing_bedrock_nova_micro() {
        let e = get_pricing_entry("amazon.nova-micro", Some("bedrock"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.035));
        assert_eq!(e.output_cost_per_million, Some(0.14));
    }

    #[test]
    fn test_pricing_unknown_model() {
        assert!(get_pricing_entry("totally-unknown-model", None, None).is_none());
    }

    #[test]
    fn test_pricing_subscription_included() {
        let e = get_pricing_entry("gpt-4o", Some("openai-codex"), None).unwrap();
        assert_eq!(e.input_cost_per_million, Some(0.0));
        assert_eq!(e.source, CostSource::None);
    }

    // --- estimate_usage_cost tests ---

    #[test]
    fn test_cost_claude_sonnet4() {
        let usage = CanonicalUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            request_count: 1,
        };
        let cost =
            estimate_usage_cost("claude-sonnet-4-20250514", &usage, Some("anthropic"), None);
        assert_eq!(cost.status, CostStatus::Estimated);
        // 3.00 * 1M/1M + 15.0 * 0.5M/1M = 3.0 + 7.5 = 10.5
        assert!((cost.amount_usd.unwrap() - 10.5).abs() < 0.001);
    }

    #[test]
    fn test_cost_claude_sonnet4_with_cache() {
        let usage = CanonicalUsage {
            input_tokens: 500_000,
            output_tokens: 200_000,
            cache_read_tokens: 500_000,
            cache_write_tokens: 500_000,
            reasoning_tokens: 0,
            request_count: 1,
        };
        let cost =
            estimate_usage_cost("claude-sonnet-4-20250514", &usage, Some("anthropic"), None);
        // 3.0*0.5 + 15.0*0.2 + 0.30*0.5 + 3.75*0.5 = 1.5 + 3.0 + 0.15 + 1.875 = 6.525
        assert!((cost.amount_usd.unwrap() - 6.525).abs() < 0.001);
    }

    #[test]
    fn test_cost_gpt4o() {
        let usage = CanonicalUsage {
            input_tokens: 2_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            request_count: 1,
        };
        let cost = estimate_usage_cost("gpt-4o", &usage, Some("openai"), None);
        // 2.50*2 + 10.0*1 = 5.0 + 10.0 = 15.0
        assert!((cost.amount_usd.unwrap() - 15.0).abs() < 0.001);
    }

    #[test]
    fn test_cost_deepseek() {
        let usage = CanonicalUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        let cost = estimate_usage_cost("deepseek-chat", &usage, Some("deepseek"), None);
        // 0.14 + 0.28 = 0.42
        assert!((cost.amount_usd.unwrap() - 0.42).abs() < 0.001);
    }

    #[test]
    fn test_cost_subscription_included() {
        let usage = CanonicalUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            ..Default::default()
        };
        let cost = estimate_usage_cost("gpt-4o", &usage, Some("openai-codex"), None);
        assert_eq!(cost.status, CostStatus::Included);
        assert_eq!(cost.label, "included");
    }

    #[test]
    fn test_cost_unknown_model() {
        let usage = CanonicalUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let cost = estimate_usage_cost("nonexistent", &usage, Some("custom"), None);
        assert_eq!(cost.status, CostStatus::Unknown);
        assert!(cost.amount_usd.is_none());
    }

    #[test]
    fn test_cost_cache_unavailable_returns_unknown() {
        // OpenAI models don't have cache_write pricing
        let usage = CanonicalUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 100,
            ..Default::default()
        };
        let cost = estimate_usage_cost("gpt-4o", &usage, Some("openai"), None);
        assert_eq!(cost.status, CostStatus::Unknown);
        assert!(cost.amount_usd.is_none());
    }

    // --- has_known_pricing tests ---

    #[test]
    fn test_has_pricing_known() {
        assert!(has_known_pricing("claude-sonnet-4-20250514", Some("anthropic"), None));
        assert!(has_known_pricing("gpt-4o", Some("openai"), None));
        assert!(has_known_pricing("deepseek-chat", Some("deepseek"), None));
        assert!(has_known_pricing("gemini-2.5-pro", Some("google"), None));
        assert!(has_known_pricing("amazon.nova-pro", Some("bedrock"), None));
    }

    #[test]
    fn test_has_pricing_unknown() {
        assert!(!has_known_pricing("unknown-model-xyz", None, None));
    }

    #[test]
    fn test_has_pricing_subscription_included() {
        assert!(has_known_pricing("gpt-4o", Some("openai-codex"), None));
    }

    // --- format_duration_compact tests ---

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration_compact(45.0), "45s");
        assert_eq!(format_duration_compact(59.0), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration_compact(135.0), "2m");
        assert_eq!(format_duration_compact(3599.0), "59m");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration_compact(3600.0), "1h");
        assert_eq!(format_duration_compact(5000.0), "1h 23m");
        assert_eq!(format_duration_compact(7200.0), "2h");
    }

    #[test]
    fn test_format_duration_days() {
        assert_eq!(format_duration_compact(86400.0), "1.0d");
        assert_eq!(format_duration_compact(172800.0), "2.0d");
    }

    // --- format_token_count_compact tests ---

    #[test]
    fn test_format_token_small() {
        assert_eq!(format_token_count_compact(0), "0");
        assert_eq!(format_token_count_compact(500), "500");
        assert_eq!(format_token_count_compact(999), "999");
    }

    #[test]
    fn test_format_token_k() {
        assert_eq!(format_token_count_compact(1_000), "1K");
        assert_eq!(format_token_count_compact(1_500), "1.5K");
        assert_eq!(format_token_count_compact(450_000), "450K");
        // 999999 < 1M threshold, so it falls through to K (matches Python behavior)
        assert_eq!(format_token_count_compact(999_999), "1000K");
    }

    #[test]
    fn test_format_token_m() {
        assert_eq!(format_token_count_compact(1_000_000), "1M");
        assert_eq!(format_token_count_compact(1_200_000), "1.2M");
        assert_eq!(format_token_count_compact(9_500_000), "9.5M");
    }

    #[test]
    fn test_format_token_b() {
        assert_eq!(format_token_count_compact(1_000_000_000), "1B");
        assert_eq!(format_token_count_compact(1_234_567_890), "1.23B");
    }

    #[test]
    fn test_format_token_negative() {
        assert_eq!(format_token_count_compact(-1_500), "-1.5K");
        assert_eq!(format_token_count_compact(-2_000_000), "-2M");
    }

    // --- CanonicalUsage helpers ---

    #[test]
    fn test_canonical_usage_prompt_tokens() {
        let u = CanonicalUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 50,
            cache_write_tokens: 30,
            ..Default::default()
        };
        assert_eq!(u.prompt_tokens(), 180);
        assert_eq!(u.total_tokens(), 380);
    }

    // --- billing route model stripping ---

    #[test]
    fn test_route_strips_provider_prefix() {
        let r = resolve_billing_route(
            "anthropic/claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
        );
        assert_eq!(r.model, "claude-sonnet-4-20250514");
    }
}
