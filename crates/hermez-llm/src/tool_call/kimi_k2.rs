#![allow(dead_code)]
//! Kimi K2 tool call parser.
//!
//! Format:
//!     <|tool_calls_section_begin|>
//!     <|tool_call_begin|>function_id:0<|tool_call_argument_begin|>{"arg": "val"}<|tool_call_end|>
//!     <|tool_calls_section_end|>

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait};

static START_TOKENS: &[&str] = &[
    "<|tool_calls_section_begin|>",
    "<|tool_call_section_begin|>",
];

static PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"<\|tool_call_begin\|>\s*(?P<tool_call_id>[^<]+:\d+)\s*<\|tool_call_argument_begin\|>\s*(?P<function_arguments>.*?)\s*<\|tool_call_end\|>"
    ).unwrap()
});

/// Parser for Kimi K2 tool calls.
pub struct KimiK2ToolCallParser;

impl KimiK2ToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for KimiK2ToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for KimiK2ToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        let has_start = START_TOKENS.iter().any(|t| text.contains(t));
        if !has_start {
            return (Some(text.to_string()), None);
        }

        let matches: Vec<_> = PATTERN.captures_iter(text).collect();
        if matches.is_empty() {
            return (Some(text.to_string()), None);
        }

        let mut tool_calls = Vec::new();
        for cap in &matches {
            let function_id = cap.name("tool_call_id").map(|m| m.as_str().trim()).unwrap_or("");
            let func_args = cap.name("function_arguments").map(|m| m.as_str().trim()).unwrap_or("");
            if function_id.is_empty() { continue; }

            // Extract function name from "functions.get_weather:0" -> "get_weather"
            let function_name = function_id
                .split(':')
                .next()
                .unwrap_or("")
                .split('.')
                .next_back()
                .unwrap_or(function_id);

            tool_calls.push(ToolCall {
                id: function_id.to_string(),
                name: function_name.to_string(),
                arguments: func_args.to_string(),
            });
        }

        if tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        // Find earliest start token
        let earliest = START_TOKENS
            .iter()
            .filter_map(|t| text.find(t))
            .min()
            .unwrap_or(0);
        let content = text[..earliest].trim().to_string();
        let content = if content.is_empty() { None } else { Some(content) };
        (content, Some(tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kimi_k2_single() {
        let parser = KimiK2ToolCallParser::new();
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>functions.get_weather:0<|tool_call_argument_begin|>{\"city\": \"NYC\"}<|tool_call_end|><|tool_calls_section_end|>";
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
        assert_eq!(tc[0].id, "functions.get_weather:0");
    }

    #[test]
    fn test_kimi_k2_none() {
        let parser = KimiK2ToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
