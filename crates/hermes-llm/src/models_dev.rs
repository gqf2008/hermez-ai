//! Models.dev registry integration — primary database for providers and models.
//!
//! Fetches from `https://models.dev/api.json` — a community-maintained database
//! of 4000+ models across 109+ providers.
//!
//! Mirrors the Python `agent/models_dev.py`.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::Mutex as AsyncMutex;

// =========================================================================
// Constants
// =========================================================================

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_CACHE_TTL: u64 = 3600; // 1 hour in-memory (seconds)

/// Hermes provider names → models.dev provider IDs.
static PROVIDER_TO_MODELS_DEV: Lazy<HashMap<&str, &str>> = Lazy::new(|| {
    HashMap::from([
        ("openrouter", "openrouter"),
        ("anthropic", "anthropic"),
        ("openai", "openai"),
        ("openai-codex", "openai"),
        ("zai", "zai"),
        ("kimi-coding", "kimi-for-coding"),
        ("minimax", "minimax"),
        ("minimax-cn", "minimax-cn"),
        ("deepseek", "deepseek"),
        ("alibaba", "alibaba"),
        ("qwen-oauth", "alibaba"),
        ("copilot", "github-copilot"),
        ("ai-gateway", "vercel"),
        ("opencode-zen", "opencode"),
        ("opencode-go", "opencode-go"),
        ("kilocode", "kilo"),
        ("fireworks", "fireworks-ai"),
        ("huggingface", "huggingface"),
        ("gemini", "google"),
        ("google", "google"),
        ("xai", "xai"),
        ("xiaomi", "xiaomi"),
        ("nvidia", "nvidia"),
        ("groq", "groq"),
        ("mistral", "mistral"),
        ("togetherai", "togetherai"),
        ("perplexity", "perplexity"),
        ("cohere", "cohere"),
    ])
});

/// Reverse mapping: models.dev ID → Hermes provider ID.
#[allow(dead_code)]
static MODELS_DEV_TO_PROVIDER: Lazy<HashMap<&str, &str>> = Lazy::new(|| {
    PROVIDER_TO_MODELS_DEV
        .iter()
        .map(|(k, v)| (*v, *k))
        .collect()
});

// =========================================================================
// Data types
// =========================================================================

/// Full metadata for a single model from models.dev.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub family: String,
    pub provider_id: String,

    // Capabilities
    pub reasoning: bool,
    pub tool_call: bool,
    pub attachment: bool,
    pub temperature: bool,
    pub structured_output: bool,
    pub open_weights: bool,

    // Modalities
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,

    // Limits
    pub context_window: u64,
    pub max_output: u64,
    pub max_input: Option<u64>,

    // Cost (per million tokens, USD)
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,

    // Metadata
    pub knowledge_cutoff: String,
    pub release_date: String,
    pub status: String,
}

impl ModelInfo {
    pub fn has_cost_data(&self) -> bool {
        self.cost_input > 0.0 || self.cost_output > 0.0
    }

    pub fn supports_vision(&self) -> bool {
        self.attachment || self.input_modalities.iter().any(|m| m == "image")
    }

    pub fn supports_pdf(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "pdf")
    }

    pub fn supports_audio_input(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "audio")
    }

    /// Human-readable cost string, e.g. '$3.00/M in, $15.00/M out'.
    pub fn format_cost(&self) -> String {
        if !self.has_cost_data() {
            return "unknown".to_string();
        }
        let mut parts = vec![
            format!("${:.2}/M in", self.cost_input),
            format!("${:.2}/M out", self.cost_output),
        ];
        if let Some(cr) = self.cost_cache_read {
            parts.push(format!("cache read ${:.2}/M", cr));
        }
        parts.join(", ")
    }

    /// Human-readable capabilities string.
    pub fn format_capabilities(&self) -> String {
        let mut caps = Vec::new();
        if self.reasoning {
            caps.push("reasoning");
        }
        if self.tool_call {
            caps.push("tools");
        }
        if self.supports_vision() {
            caps.push("vision");
        }
        if self.supports_pdf() {
            caps.push("PDF");
        }
        if self.supports_audio_input() {
            caps.push("audio");
        }
        if self.structured_output {
            caps.push("structured output");
        }
        if self.open_weights {
            caps.push("open weights");
        }
        if caps.is_empty() {
            "basic".to_string()
        } else {
            caps.join(", ")
        }
    }
}

/// Full metadata for a provider from models.dev.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,
    pub api: String,
    pub doc: String,
    pub model_count: usize,
}

/// Structured capability metadata for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub model_family: String,
}

/// Fuzzy search result.
#[derive(Debug, Clone)]
pub struct ModelSearchResult {
    pub provider: String,
    pub model_id: String,
    pub entry: serde_json::Value,
}

// =========================================================================
// Caching
// =========================================================================

struct DevCache {
    data: serde_json::Value,
    cached_at: std::time::Instant,
}

static MODELS_DEV_INMEM: AsyncMutex<Option<DevCache>> = AsyncMutex::const_new(None);

/// Disk cache path: `~/.hermes/models_dev_cache.json`.
fn get_disk_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".hermes").join("models_dev_cache.json"))
}

fn load_disk_cache() -> Option<serde_json::Value> {
    let path = get_disk_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_disk_cache(data: &serde_json::Value) {
    let Some(path) = get_disk_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(content) = serde_json::to_string(data) {
        let _ = std::fs::write(&tmp, content);
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Fetch models.dev registry. In-memory cache (1hr) + disk fallback.
pub async fn fetch_models_dev(force_refresh: bool) -> serde_json::Value {
    // Check in-memory cache
    {
        let guard = MODELS_DEV_INMEM.lock().await;
        if let Some(cache) = guard.as_ref() {
            if !force_refresh && cache.cached_at.elapsed().as_secs() < MODELS_DEV_CACHE_TTL {
                return cache.data.clone();
            }
        }
    }

    // Try network fetch
    if let Ok(response) = reqwest::get(MODELS_DEV_URL).await {
        if response.status().is_success() {
            if let Ok(data) = response.json::<serde_json::Value>().await {
                if let Some(obj) = data.as_object() {
                    if !obj.is_empty() {
                        save_disk_cache(&data);
                        let mut guard = MODELS_DEV_INMEM.lock().await;
                        *guard = Some(DevCache {
                            data: data.clone(),
                            cached_at: std::time::Instant::now(),
                        });
                        return data;
                    }
                }
            }
        }
    }

    // Fall back to in-memory (short TTL) or disk cache
    {
        let mut guard = MODELS_DEV_INMEM.lock().await;
        if let Some(ref cache) = *guard {
            if cache.cached_at.elapsed().as_secs() < 300 {
                return cache.data.clone();
            }
        }
        if guard.is_none() {
            if let Some(disk_data) = load_disk_cache() {
                *guard = Some(DevCache {
                    data: disk_data.clone(),
                    cached_at: std::time::Instant::now(),
                });
                return disk_data;
            }
        }
        // Return whatever is in memory even if stale
        if let Some(ref cache) = *guard {
            return cache.data.clone();
        }
    }

    serde_json::Value::Object(Default::default())
}

// =========================================================================
// Lookup helpers
// =========================================================================

/// Extract `limit.context` from a models.dev model entry.
fn extract_context(entry: &serde_json::Value) -> Option<u64> {
    entry
        .get("limit")
        .and_then(|l| l.get("context"))
        .and_then(|c| c.as_u64())
        .filter(|&v| v > 0)
}

/// Resolve a Hermes provider ID to its models.dev provider models dict.
fn get_provider_models<'a>(
    provider: &str,
    data: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    let mdev_id = PROVIDER_TO_MODELS_DEV.get(provider)?;
    let provider_data = data.get(*mdev_id)?;
    provider_data.get("models")
}

/// Find a model entry by exact match, then case-insensitive fallback.
fn find_model_entry<'a>(
    models: &'a serde_json::Value,
    model: &str,
) -> Option<(String, &'a serde_json::Value)> {
    let models_obj = models.as_object()?;

    // Exact match
    if let Some(entry) = models_obj.get(model) {
        if entry.is_object() {
            return Some((model.to_string(), entry));
        }
    }

    // Case-insensitive match
    let model_lower = model.to_lowercase();
    for (mid, mdata) in models_obj {
        if mid.to_lowercase() == model_lower && mdata.is_object() {
            return Some((mid.clone(), mdata));
        }
    }

    None
}

/// Look up context_length for a provider+model combo in models.dev.
pub async fn lookup_context(provider: &str, model: &str) -> Option<u64> {
    let data = fetch_models_dev(false).await;
    let models = get_provider_models(provider, &data)?;
    let (_, entry) = find_model_entry(models, model)?;
    extract_context(entry)
}

// =========================================================================
// Capability queries
// =========================================================================

/// Noise patterns for filtering non-agentic models.
static NOISE_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"-(preview|exp)-\d{2,4}([-_]|$)").unwrap());

fn is_noise_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.contains("-tts")
        || lower.contains("embedding")
        || lower.contains("live-")
        || lower.contains("-image")
        || lower.contains("-image-preview")
        || lower.contains("-customtools")
        || NOISE_RE.is_match(model_id)
}

/// Look up full capability metadata from models.dev cache.
pub async fn get_model_capabilities(provider: &str, model: &str) -> Option<ModelCapabilities> {
    let data = fetch_models_dev(false).await;
    let models = get_provider_models(provider, &data)?;
    let (_, entry) = find_model_entry(models, model)?;

    let supports_tools = entry
        .get("tool_call")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Vision: check `attachment` flag and `modalities.input` for "image"
    let input_mods: Vec<String> = entry
        .get("modalities")
        .and_then(|m| m.get("input"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let supports_vision = entry
        .get("attachment")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || input_mods.iter().any(|m| m == "image");

    let supports_reasoning = entry
        .get("reasoning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let limit_obj = entry.get("limit").and_then(|l| l.as_object());
    let context_window = limit_obj
        .and_then(|l| l.get("context"))
        .and_then(|c| c.as_u64())
        .filter(|&v| v > 0)
        .unwrap_or(200_000);

    let max_output_tokens = limit_obj
        .and_then(|l| l.get("output"))
        .and_then(|o| o.as_u64())
        .filter(|&v| v > 0)
        .unwrap_or(8192);

    let model_family = entry
        .get("family")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Some(ModelCapabilities {
        supports_tools,
        supports_vision,
        supports_reasoning,
        context_window,
        max_output_tokens,
        model_family,
    })
}

/// Return all model IDs for a provider from models.dev.
pub async fn list_provider_models(provider: &str) -> Vec<String> {
    let data = fetch_models_dev(false).await;
    let Some(models) = get_provider_models(provider, &data) else {
        return Vec::new();
    };
    let Some(obj) = models.as_object() else {
        return Vec::new();
    };
    obj.keys().cloned().collect()
}

/// Return model IDs suitable for agentic use from models.dev.
/// Filters for tool_call=True and excludes noise (TTS, embedding, etc.).
pub async fn list_agentic_models(provider: &str) -> Vec<String> {
    let data = fetch_models_dev(false).await;
    let Some(models) = get_provider_models(provider, &data) else {
        return Vec::new();
    };
    let Some(obj) = models.as_object() else {
        return Vec::new();
    };

    obj.iter()
        .filter(|(mid, entry)| {
            entry.get("tool_call").and_then(|v| v.as_bool()) == Some(true) && !is_noise_model(mid)
        })
        .map(|(mid, _)| mid.clone())
        .collect()
}

// =========================================================================
// Search
// =========================================================================

/// Fuzzy search across models.dev catalog.
pub async fn search_models_dev(
    query: &str,
    provider: Option<&str>,
    limit: usize,
) -> Vec<ModelSearchResult> {
    let data = fetch_models_dev(false).await;

    // Build candidates: (hermes_provider, model_id, entry)
    let candidates: Vec<(String, String, serde_json::Value)> = if let Some(p) = provider {
        let mdev_id = PROVIDER_TO_MODELS_DEV.get(p);
        if let Some(&mdev_id) = mdev_id {
            if let Some(pdata) = data
                .get(mdev_id)
                .and_then(|v| v.get("models"))
                .and_then(|v| v.as_object())
            {
                pdata
                    .iter()
                    .map(|(mid, mdata)| (p.to_string(), mid.clone(), mdata.clone()))
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        let mut cands = Vec::new();
        for (hermes_prov, mdev_prov) in PROVIDER_TO_MODELS_DEV.iter() {
            if let Some(pdata) = data
                .get(*mdev_prov)
                .and_then(|v| v.get("models"))
                .and_then(|v| v.as_object())
            {
                for (mid, mdata) in pdata {
                    cands.push((hermes_prov.to_string(), mid.clone(), mdata.clone()));
                }
            }
        }
        cands
    };

    if candidates.is_empty() {
        return Vec::new();
    }

    let query_lower = query.to_lowercase();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut results = Vec::new();

    // Substring matches first (more intuitive than pure edit-distance)
    for (prov, mid, mdata) in &candidates {
        if mid.to_lowercase().contains(&query_lower) {
            let key = (prov.clone(), mid.clone());
            if seen.insert(key) {
                results.push(ModelSearchResult {
                    provider: prov.clone(),
                    model_id: mid.clone(),
                    entry: mdata.clone(),
                });
                if results.len() >= limit {
                    return results;
                }
            }
        }
    }

    // Fuzzy matches for remaining
    let mut remaining: Vec<_> = candidates
        .iter()
        .filter(|(_, mid, _)| !mid.to_lowercase().contains(&query_lower))
        .collect();

    // Score by longest common substring
    remaining.sort_by(|a, b| {
        let score_a = fuzzy_score(&query_lower, &a.1.to_lowercase());
        let score_b = fuzzy_score(&query_lower, &b.1.to_lowercase());
        score_b.cmp(&score_a)
    });

    for (prov, mid, mdata) in remaining {
        let key = (prov.clone(), mid.clone());
        if seen.insert(key) {
            results.push(ModelSearchResult {
                provider: prov.clone(),
                model_id: mid.clone(),
                entry: mdata.clone(),
            });
            if results.len() >= limit {
                return results;
            }
        }
    }

    results
}

/// Simple fuzzy score: length of longest common substring.
fn fuzzy_score(query: &str, target: &str) -> usize {
    if query.is_empty() || target.is_empty() {
        return 0;
    }
    let q_bytes = query.as_bytes();
    let t_bytes = target.as_bytes();
    let mut max_len: u16 = 0;
    let mut dp = vec![0u16; t_bytes.len() + 1];

    for i in 1..=q_bytes.len() {
        let mut prev: u16 = 0;
        for j in 1..=t_bytes.len() {
            let temp = dp[j];
            if q_bytes[i - 1] == t_bytes[j - 1] {
                dp[j] = prev + 1;
                if dp[j] > max_len {
                    max_len = dp[j];
                }
            } else {
                dp[j] = 0;
            }
            prev = temp;
        }
    }
    max_len as usize
}

// =========================================================================
// Rich dataclass constructors
// =========================================================================

fn parse_model_info(model_id: &str, raw: &serde_json::Value, provider_id: &str) -> ModelInfo {
    let limit = raw
        .get("limit")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let cost = raw
        .get("cost")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let modalities = raw
        .get("modalities")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let input_mods: Vec<String> = modalities
        .get("input")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let output_mods: Vec<String> = modalities
        .get("output")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let ctx = limit
        .get("context")
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
        .unwrap_or(0);
    let out = limit
        .get("output")
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
        .unwrap_or(0);
    let inp = limit
        .get("input")
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0);

    ModelInfo {
        id: model_id.to_string(),
        name: raw
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(model_id)
            .to_string(),
        family: raw
            .get("family")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        provider_id: provider_id.to_string(),
        reasoning: raw.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(false),
        tool_call: raw.get("tool_call").and_then(|v| v.as_bool()).unwrap_or(false),
        attachment: raw.get("attachment").and_then(|v| v.as_bool()).unwrap_or(false),
        temperature: raw
            .get("temperature")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        structured_output: raw
            .get("structured_output")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        open_weights: raw
            .get("open_weights")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        input_modalities: input_mods,
        output_modalities: output_mods,
        context_window: ctx,
        max_output: out,
        max_input: inp,
        cost_input: cost
            .get("input")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        cost_output: cost
            .get("output")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        cost_cache_read: cost.get("cache_read").and_then(|v| v.as_f64()),
        cost_cache_write: cost.get("cache_write").and_then(|v| v.as_f64()),
        knowledge_cutoff: raw
            .get("knowledge")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        release_date: raw
            .get("release_date")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        status: raw
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

fn parse_provider_info(
    provider_id: &str,
    raw: &serde_json::Value,
) -> ProviderInfo {
    let env: Vec<String> = raw
        .get("env")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let model_count = raw
        .get("models")
        .and_then(|v| v.as_object())
        .map(|m| m.len())
        .unwrap_or(0);

    ProviderInfo {
        id: provider_id.to_string(),
        name: raw
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(provider_id)
            .to_string(),
        env,
        api: raw
            .get("api")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        doc: raw
            .get("doc")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        model_count,
    }
}

/// Get full provider metadata from models.dev.
pub async fn get_provider_info(provider_id: &str) -> Option<ProviderInfo> {
    let mdev_id = PROVIDER_TO_MODELS_DEV
        .get(provider_id)
        .copied()
        .unwrap_or(provider_id);

    let data = fetch_models_dev(false).await;
    let raw = data.get(mdev_id)?;
    if !raw.is_object() {
        return None;
    }
    Some(parse_provider_info(mdev_id, raw))
}

/// Get full model metadata from models.dev.
pub async fn get_model_info(
    provider_id: &str,
    model_id: &str,
) -> Option<ModelInfo> {
    let mdev_id = PROVIDER_TO_MODELS_DEV
        .get(provider_id)
        .copied()
        .unwrap_or(provider_id);

    let data = fetch_models_dev(false).await;
    let pdata = data.get(mdev_id)?;
    let models = pdata.get("models")?;
    let models_obj = models.as_object()?;

    // Exact match
    if let Some(raw) = models_obj.get(model_id) {
        if raw.is_object() {
            return Some(parse_model_info(model_id, raw, mdev_id));
        }
    }

    // Case-insensitive fallback
    let model_lower = model_id.to_lowercase();
    for (mid, mdata) in models_obj {
        if mid.to_lowercase() == model_lower && mdata.is_object() {
            return Some(parse_model_info(mid, mdata, mdev_id));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_mapping_valid() {
        // Every forward mapping should resolve through the reverse (for unique mappings)
        // and every reverse mapping should point back to a valid forward target.
        for (mdev, hermes) in MODELS_DEV_TO_PROVIDER.iter() {
            let fwd = PROVIDER_TO_MODELS_DEV.get(hermes);
            assert_eq!(fwd, Some(mdev), "reverse {} → {} has no matching forward", mdev, hermes);
        }
    }

    #[test]
    fn test_is_noise_model() {
        assert!(is_noise_model("some-model-tts-v2"));
        assert!(is_noise_model("embedding-model"));
        assert!(is_noise_model("model-preview-2024"));
        assert!(is_noise_model("model-image-v2"));
        assert!(!is_noise_model("claude-sonnet-4-5"));
        assert!(!is_noise_model("gpt-4o"));
    }

    #[test]
    fn test_fuzzy_score() {
        assert_eq!(fuzzy_score("claude", "claude-sonnet-4-5"), 6);
        assert_eq!(fuzzy_score("gpt", "gpt-4o"), 3);
        assert_eq!(fuzzy_score("xyz", "claude-sonnet"), 0);
    }

    #[test]
    fn test_extract_context() {
        let entry = serde_json::json!({
            "limit": {"context": 200000, "output": 8192}
        });
        assert_eq!(extract_context(&entry), Some(200000));

        let zero_ctx = serde_json::json!({
            "limit": {"context": 0}
        });
        assert_eq!(extract_context(&zero_ctx), None);

        let no_limit = serde_json::json!({"name": "test"});
        assert_eq!(extract_context(&no_limit), None);
    }

    #[test]
    fn test_model_info_methods() {
        let info = ModelInfo {
            id: "claude-sonnet-4-5".to_string(),
            name: "Claude Sonnet 4.5".to_string(),
            family: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            reasoning: true,
            tool_call: true,
            attachment: true,
            temperature: true,
            structured_output: true,
            open_weights: false,
            input_modalities: vec!["text".to_string(), "image".to_string()],
            output_modalities: vec!["text".to_string()],
            context_window: 200000,
            max_output: 8192,
            max_input: None,
            cost_input: 3.0,
            cost_output: 15.0,
            cost_cache_read: Some(0.3),
            cost_cache_write: Some(3.75),
            knowledge_cutoff: "2025-08-01".to_string(),
            release_date: "2025-08-01".to_string(),
            status: "".to_string(),
        };

        assert!(info.has_cost_data());
        assert!(info.supports_vision());
        assert!(!info.supports_pdf());
        assert!(!info.supports_audio_input());
        assert_eq!(
            info.format_capabilities(),
            "reasoning, tools, vision, structured output"
        );
        assert!(info.format_cost().contains("$3.00"));
    }
}
