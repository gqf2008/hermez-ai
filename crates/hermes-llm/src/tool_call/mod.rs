//! Model-specific tool call parsers.
//!
//! Mirrors the Python `environments/tool_call_parsers/`.
//! Each parser extracts structured tool calls from raw model output text
//! that doesn't follow the standard OpenAI format.
//!
//! Used when the model is served via a VLLM server that returns raw text
//! without pre-parsing tool calls.

pub(crate) mod deepseek_v3;
pub(crate) mod deepseek_v3_1;
pub(crate) mod glm45;
pub(crate) mod glm47;
pub(crate) mod kimi_k2;
pub(crate) mod llama;
pub(crate) mod longcat;
pub(crate) mod mistral;
pub(crate) mod qwen;
pub(crate) mod qwen3_coder;

use std::collections::HashMap;

/// Parsed tool call result.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Unique call ID.
    pub id: String,
    /// Tool/function name.
    pub name: String,
    /// Arguments as a JSON string.
    pub arguments: String,
}

/// Parse result: (content, tool_calls).
/// content = text with tool call markup stripped (or None if all tool calls)
/// tool_calls = Some(vec) if tool calls found, None otherwise
pub type ParseResult = (Option<String>, Option<Vec<ToolCall>>);

/// Try to convert a string value to a native JSON type.
/// Handles null, numbers, booleans, JSON objects/arrays, falls back to string.
fn try_convert_value(value: &str) -> serde_json::Value {
    let stripped = value.trim();

    // Handle null
    if stripped.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }

    // Try JSON first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        return v;
    }

    // Return as string
    serde_json::Value::String(stripped.to_string())
}

/// Generate a tool call ID.
fn gen_call_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{n:08x}")
}

/// Parser registry.
type ParserFactory = Box<dyn Fn() -> Box<dyn ToolCallParserTrait> + Send + Sync>;

static PARSER_REGISTRY: std::sync::LazyLock<HashMap<&'static str, ParserFactory>> =
    std::sync::LazyLock::new(|| {
    let mut m: HashMap<&'static str, ParserFactory> = HashMap::new();
    m.insert("hermes", Box::new(|| Box::new(qwen::HermesToolCallParser::new())));
    m.insert("qwen", Box::new(|| Box::new(qwen::HermesToolCallParser::new())));
    m.insert("longcat", Box::new(|| Box::new(longcat::LongcatToolCallParser::new())));
    m.insert("deepseek_v3", Box::new(|| Box::new(deepseek_v3::DeepSeekV3ToolCallParser::new())));
    m.insert("deepseek_v3_1", Box::new(|| Box::new(deepseek_v3_1::DeepSeekV31ToolCallParser::new())));
    m.insert("deepseek_v31", Box::new(|| Box::new(deepseek_v3_1::DeepSeekV31ToolCallParser::new())));
    m.insert("kimi_k2", Box::new(|| Box::new(kimi_k2::KimiK2ToolCallParser::new())));
    m.insert("glm45", Box::new(|| Box::new(glm45::Glm45ToolCallParser::new())));
    m.insert("glm47", Box::new(|| Box::new(glm47::Glm47ToolCallParser::new())));
    m.insert("llama3_json", Box::new(|| Box::new(llama::LlamaToolCallParser::new())));
    m.insert("llama4_json", Box::new(|| Box::new(llama::LlamaToolCallParser::new())));
    m.insert("qwen3_coder", Box::new(|| Box::new(qwen3_coder::Qwen3CoderToolCallParser::new())));
    m.insert("mistral", Box::new(|| Box::new(mistral::MistralToolCallParser::new())));
    m
});

/// Trait for tool call parsers (object-safe for dynamic dispatch).
pub trait ToolCallParserTrait: Send + Sync {
    fn parse(&self, text: &str) -> ParseResult;
}

/// Get a parser by name.
pub fn get_parser(name: &str) -> Option<Box<dyn ToolCallParserTrait>> {
    PARSER_REGISTRY.get(name).map(|f| f())
}

/// List all registered parser names.
pub fn list_parsers() -> Vec<&'static str> {
    let mut names: Vec<_> = PARSER_REGISTRY.keys().copied().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_convert_value() {
        assert_eq!(try_convert_value("null"), serde_json::Value::Null);
        assert_eq!(try_convert_value("42"), serde_json::Value::Number(42.into()));
        assert_eq!(
            try_convert_value("true"),
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            try_convert_value(r#""hello""#),
            serde_json::Value::String("hello".to_string())
        );
        assert_eq!(
            try_convert_value("just_a_string"),
            serde_json::Value::String("just_a_string".to_string())
        );
    }

    #[test]
    fn test_gen_call_id_unique() {
        let id1 = gen_call_id();
        let id2 = gen_call_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_get_parser_known_names() {
        assert!(get_parser("hermes").is_some());
        assert!(get_parser("qwen").is_some());
        assert!(get_parser("deepseek_v3").is_some());
        assert!(get_parser("kimi_k2").is_some());
        assert!(get_parser("glm45").is_some());
        assert!(get_parser("llama3_json").is_some());
        assert!(get_parser("qwen3_coder").is_some());
    }

    #[test]
    fn test_get_parser_unknown() {
        assert!(get_parser("unknown_model").is_none());
    }

    #[test]
    fn test_list_parsers() {
        let names = list_parsers();
        assert!(names.contains(&"hermes"));
        assert!(names.contains(&"deepseek_v3"));
        assert!(names.contains(&"deepseek_v3_1"));
        assert!(names.contains(&"kimi_k2"));
        assert!(names.contains(&"glm45"));
        assert!(names.contains(&"glm47"));
        assert!(names.contains(&"llama3_json"));
        assert!(names.contains(&"llama4_json"));
        assert!(names.contains(&"qwen"));
        assert!(names.contains(&"qwen3_coder"));
        assert!(names.contains(&"longcat"));
    }
}
