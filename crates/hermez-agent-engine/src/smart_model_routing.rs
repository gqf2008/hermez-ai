//! Helpers for optional cheap-vs-strong model routing.
//!
//! Conservative by design: if the message has signs of code/tool/debugging/
//! long-form work, keep the primary model.
//!
//! Mirrors the Python `agent/smart_model_routing.py`.

/// Keywords that indicate a complex request that should NOT be routed to a cheap model.
const COMPLEX_KEYWORDS: &[&str] = &[
    "debug", "debugging", "implement", "implementation", "refactor", "patch",
    "traceback", "stacktrace", "exception", "error", "analyze", "analysis",
    "investigate", "architecture", "design", "compare", "benchmark", "optimize",
    "optimise", "review", "terminal", "shell", "tool", "tools", "pytest",
    "test", "tests", "plan", "planning", "delegate", "subagent", "cron",
    "docker", "kubernetes",
];

/// Compiled URL regex for detection.
fn url_re() -> &'static regex::Regex {
    use once_cell::sync::Lazy;
    static URL_RE: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"https?://|www\.").unwrap()
    });
    &URL_RE
}

/// Compiled complex keywords regex.
fn complex_keywords_re() -> &'static regex::Regex {
    use once_cell::sync::Lazy;
    static CKW_RE: Lazy<regex::Regex> = Lazy::new(|| {
        let pattern = COMPLEX_KEYWORDS
            .iter()
            .map(|k| regex::escape(k))
            .collect::<Vec<_>>()
            .join("|");
        regex::Regex::new(&format!(r"(?i)\b(?:{})\b", pattern)).unwrap()
    });
    &CKW_RE
}

/// Configuration for smart model routing.
#[derive(Debug, Clone, Default)]
pub struct RoutingConfig {
    /// Whether smart routing is enabled.
    pub enabled: bool,
    /// Cheap model provider (e.g. "openai").
    pub provider: Option<String>,
    /// Cheap model name (e.g. "gpt-4o-mini").
    pub model: Option<String>,
    /// Max characters for a "simple" message.
    pub max_simple_chars: usize,
    /// Max words for a "simple" message.
    pub max_simple_words: usize,
    /// Optional explicit API key.
    pub api_key: Option<String>,
    /// Optional base URL.
    pub base_url: Option<String>,
}

/// Result of routing a turn to a model.
#[derive(Debug, Clone)]
pub struct TurnRoute {
    /// The model to use for this turn.
    pub model: String,
    /// Whether this was routed to the cheap model.
    pub is_cheap_route: bool,
    /// Optional label describing the routing decision.
    pub label: Option<String>,
}

/// Return the configured cheap-model route when a message looks simple.
///
/// Conservative by design: if the message has signs of code/tool/debugging/
/// long-form work, keep the primary model.
pub fn choose_cheap_model_route(user_message: &str, config: &RoutingConfig) -> Option<RoutingConfig> {
    if !config.enabled {
        return None;
    }

    let provider = config.provider.as_ref()?.trim().to_lowercase();
    let model = config.model.as_ref()?.trim();
    if provider.is_empty() || model.is_empty() {
        return None;
    }

    let text = user_message.trim();
    if text.is_empty() {
        return None;
    }

    let max_chars = config.max_simple_chars.max(1);
    let max_words = config.max_simple_words.max(1);

    if text.len() > max_chars {
        return None;
    }
    if text.split_whitespace().count() > max_words {
        return None;
    }
    if text.chars().filter(|&c| c == '\n').count() > 1 {
        return None;
    }
    if text.contains("```") || text.contains('`') {
        return None;
    }
    if url_re().is_match(text) {
        return None;
    }

    // Check complex keywords
    if complex_keywords_re().is_match(&text.to_lowercase()) {
        return None;
    }

    Some(RoutingConfig {
        enabled: true,
        provider: Some(provider),
        model: Some(model.to_string()),
        max_simple_chars: config.max_simple_chars,
        max_simple_words: config.max_simple_words,
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
    })
}

/// Parse a routing config from a JSON value.
pub fn parse_routing_config(value: &serde_json::Value) -> RoutingConfig {
    let Some(obj) = value.as_object() else {
        return RoutingConfig::default();
    };

    let enabled = obj.get("enabled").is_some_and(|v| {
        v.as_bool().unwrap_or(false)
            || v.as_str().is_some_and(|s| matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"))
    });

    let cheap_model = obj.get("cheap_model").and_then(|v| v.as_object());

    let max_simple_chars = obj.get("max_simple_chars").and_then(|v| v.as_u64()).unwrap_or(160) as usize;
    let max_simple_words = obj.get("max_simple_words").and_then(|v| v.as_u64()).unwrap_or(28) as usize;

    RoutingConfig {
        enabled,
        provider: cheap_model.and_then(|cm| cm.get("provider").and_then(|v| v.as_str())).map(|s| s.to_string()),
        model: cheap_model.and_then(|cm| cm.get("model").and_then(|v| v.as_str())).map(|s| s.to_string()),
        max_simple_chars,
        max_simple_words,
        api_key: None,
        base_url: cheap_model.and_then(|cm| cm.get("base_url").and_then(|v| v.as_str())).map(|s| s.to_string()),
    }
}

/// Resolve the effective model for one turn.
///
/// Returns a `TurnRoute` with the effective model and routing metadata.
pub fn resolve_turn_route(
    user_message: &str,
    routing_config: &RoutingConfig,
    primary_model: &str,
    primary_provider: &str,
) -> TurnRoute {
    let route = choose_cheap_model_route(user_message, routing_config);

    match route {
        Some(route) => {
            let model = route.model.clone().unwrap_or_else(|| primary_model.to_string());
            let model_label = route.model.as_deref().unwrap_or(primary_model);
            let provider_label = route.provider.as_deref().unwrap_or(primary_provider);
            TurnRoute {
                model,
                is_cheap_route: true,
                label: Some(format!("smart route → {} ({})", model_label, provider_label)),
            }
        }
        None => TurnRoute {
            model: primary_model.to_string(),
            is_cheap_route: false,
            label: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(enabled: bool, provider: &str, model: &str) -> RoutingConfig {
        RoutingConfig {
            enabled,
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            max_simple_chars: 160,
            max_simple_words: 28,
            api_key: None,
            base_url: None,
        }
    }

    #[test]
    fn test_disabled_routes_to_primary() {
        let cfg = make_config(false, "openai", "gpt-4o-mini");
        assert!(choose_cheap_model_route("hello", &cfg).is_none());
    }

    #[test]
    fn test_simple_message_routes_to_cheap() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        let result = choose_cheap_model_route("what is 2+2?", &cfg);
        assert!(result.is_some());
    }

    #[test]
    fn test_long_message_stays_on_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        let long_msg = "a".repeat(200);
        assert!(choose_cheap_model_route(&long_msg, &cfg).is_none());
    }

    #[test]
    fn test_code_message_stays_on_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        assert!(choose_cheap_model_route("use `println!`", &cfg).is_none());
    }

    #[test]
    fn test_multiline_stays_on_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        assert!(choose_cheap_model_route("line1\nline2\nline3", &cfg).is_none());
    }

    #[test]
    fn test_url_stays_on_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        assert!(choose_cheap_model_route("check https://example.com", &cfg).is_none());
    }

    #[test]
    fn test_complex_keyword_stays_on_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        assert!(choose_cheap_model_route("debug this issue", &cfg).is_none());
        assert!(choose_cheap_model_route("run pytest", &cfg).is_none());
    }

    #[test]
    fn test_resolve_turn_route_cheap() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        let result = resolve_turn_route("hi", &cfg, "claude-opus", "anthropic");
        assert!(result.is_cheap_route);
        assert!(result.label.is_some());
    }

    #[test]
    fn test_resolve_turn_route_primary() {
        let cfg = make_config(true, "openai", "gpt-4o-mini");
        let result = resolve_turn_route("debug this ```code```", &cfg, "claude-opus", "anthropic");
        assert!(!result.is_cheap_route);
        assert_eq!(result.model, "claude-opus");
        assert!(result.label.is_none());
    }

    #[test]
    fn test_parse_routing_config() {
        let val = serde_json::json!({
            "enabled": true,
            "cheap_model": {
                "provider": "openai",
                "model": "gpt-4o-mini",
                "max_tokens": 4096
            },
            "max_simple_chars": 200,
            "max_simple_words": 30
        });
        let cfg = parse_routing_config(&val);
        assert!(cfg.enabled);
        assert_eq!(cfg.provider, Some("openai".to_string()));
        assert_eq!(cfg.model, Some("gpt-4o-mini".to_string()));
        assert_eq!(cfg.max_simple_chars, 200);
        assert_eq!(cfg.max_simple_words, 30);
    }
}
