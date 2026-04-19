#![allow(dead_code)]
//! Qwen3-Coder tool call parser.
//!
//! Uses XML-style nested tags:
//!     function=function_name
//!     parameter=param_name value /parameter
//!     /function

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id, try_convert_value};

static FUNCTION_PREFIX: &str = "function=";

static FUNCTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"function=([^>/\s]+)>(.*?)</function").unwrap()
});

static PARAMETER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"parameter=([^>/\s]+)>(.*?)</parameter").unwrap()
});

/// Parser for Qwen3-Coder XML-format tool calls.
pub struct Qwen3CoderToolCallParser;

impl Qwen3CoderToolCallParser {
    pub fn new() -> Self { Self }

    fn parse_function_call(&self, func_str: &str) -> Option<ToolCall> {
        let cap = FUNCTION_RE.captures(func_str)?;
        let func_name = cap.get(1)?.as_str();
        let params_str = cap.get(2)?.as_str();

        let mut param_dict = serde_json::Map::new();
        for param_cap in PARAMETER_RE.captures_iter(params_str) {
            let param_name = param_cap.get(1)?.as_str().trim();
            let param_value = param_cap.get(2)?.as_str().trim();
            param_dict.insert(param_name.to_string(), try_convert_value(param_value));
        }

        Some(ToolCall {
            id: gen_call_id(),
            name: func_name.to_string(),
            arguments: serde_json::to_string(&serde_json::Value::Object(param_dict)).unwrap_or_default(),
        })
    }
}

impl Default for Qwen3CoderToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for Qwen3CoderToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains(FUNCTION_PREFIX) {
            return (Some(text.to_string()), None);
        }

        let mut tool_calls = Vec::new();
        for cap in FUNCTION_RE.captures_iter(text) {
            let full = cap.get(0).unwrap().as_str();
            if let Some(tc) = self.parse_function_call(full) {
                tool_calls.push(tc);
            }
        }

        if tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        let first_tc_start = text.find(FUNCTION_PREFIX).unwrap_or(0);
        let content = if first_tc_start > 0 {
            let c = text[..first_tc_start].trim().to_string();
            if c.is_empty() { None } else { Some(c) }
        } else {
            None
        };
        (content, Some(tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qwen3_coder_single() {
        let parser = Qwen3CoderToolCallParser::new();
        let text = "function=get_weather>parameter=city>\"NYC\"</parameter</function";
        let (_content, tool_calls) = parser.parse(text);
        // content may or may not be None depending on whether text precedes the function tag
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_qwen3_coder_none() {
        let parser = Qwen3CoderToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
