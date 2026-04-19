#![allow(dead_code)]
//! DeepSeek V3.1 tool call parser.
//!
//! Similar to V3 but simpler format:
//!     <|tool_call_begin|>function_name<|tool_sep|>arguments<|tool_call_end|>

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id};

static START_TOKEN: &str = "<|tool_calls_begin|>";

static PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"<\|tool_call_begin\|>(?P<function_name>.*?)<\|tool_sep\|>(?P<function_arguments>.*?)<\|tool_call_end\|>"
    ).unwrap()
});

/// Parser for DeepSeek V3.1 tool calls.
pub struct DeepSeekV31ToolCallParser;

impl DeepSeekV31ToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for DeepSeekV31ToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for DeepSeekV31ToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains(START_TOKEN) {
            return (Some(text.to_string()), None);
        }

        let matches: Vec<_> = PATTERN.captures_iter(text).collect();
        if matches.is_empty() {
            return (Some(text.to_string()), None);
        }

        let mut tool_calls = Vec::new();
        for cap in &matches {
            let func_name = cap.name("function_name").map(|m| m.as_str().trim()).unwrap_or("");
            let func_args = cap.name("function_arguments").map(|m| m.as_str().trim()).unwrap_or("");
            if func_name.is_empty() { continue; }

            tool_calls.push(ToolCall {
                id: gen_call_id(),
                name: func_name.to_string(),
                arguments: func_args.to_string(),
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
    fn test_deepseek_v31_single() {
        let parser = DeepSeekV31ToolCallParser::new();
        let text = "<|tool_calls_begin|><|tool_call_begin|>get_weather<|tool_sep|>{\"city\": \"NYC\"}<|tool_call_end|><|tool_calls_end|>";
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_deepseek_v31_none() {
        let parser = DeepSeekV31ToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
