#![allow(dead_code)]
//! GLM 4.7 tool call parser.
//!
//! Same as GLM 4.5 but with slightly different regex patterns.

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id, try_convert_value};

static START_TOKEN: &str = "＜＜";

static FUNC_CALL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?s)＜＜.*?＞＞").unwrap()
});

static FUNC_DETAIL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?s)＜＜([^\n]*)\n(.*)＞＞").unwrap()
});

static FUNC_ARG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<arg_key>(.*?)</arg_key>(?:\n|\s)*<arg_value>(.*?)</arg_value>").unwrap()
});

/// Parser for GLM 4.7 tool calls.
pub struct Glm47ToolCallParser;

impl Glm47ToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for Glm47ToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for Glm47ToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains(START_TOKEN) {
            return (Some(text.to_string()), None);
        }

        let matched: Vec<_> = FUNC_CALL_RE.find_iter(text).map(|m| m.as_str().to_string()).collect();
        if matched.is_empty() {
            return (Some(text.to_string()), None);
        }

        let mut tool_calls = Vec::new();
        for match_text in &matched {
            let detail = FUNC_DETAIL_RE.captures(match_text);
            let Some(detail) = detail else { continue; };

            let func_name = detail.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let func_args_raw = detail.get(2).map(|m| m.as_str()).unwrap_or("");

            let pairs = FUNC_ARG_RE.captures_iter(func_args_raw);
            let mut arg_dict = serde_json::Map::new();
            for cap in pairs {
                let key = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let value = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                arg_dict.insert(key.to_string(), try_convert_value(value));
            }

            let arguments = serde_json::to_string(&serde_json::Value::Object(arg_dict)).unwrap_or_default();

            tool_calls.push(ToolCall {
                id: gen_call_id(),
                name: func_name.to_string(),
                arguments,
            });
        }

        if tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        let first_idx = text.find(START_TOKEN).unwrap();
        let content = text[..first_idx].trim().to_string();
        let content = if content.is_empty() { None } else { Some(content) };
        (content, Some(tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glm47_single() {
        let parser = Glm47ToolCallParser::new();
        let text = r#"＜＜get_weather
<arg_key>city</arg_key><arg_value>"NYC"</arg_value>＞＞"#;
        let (_content, tool_calls) = parser.parse(text);
        // content starts with tag, may be None or empty
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_glm47_none() {
        let parser = Glm47ToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
