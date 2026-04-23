#![allow(dead_code)]
//! Hermes / Qwen tool call parser.
//!
//! Based on VLLM's Hermes2ProToolParser.
//!
//! Format uses special Unicode tags:
//! Opening: U+1F50D U+1F4A1 (magnifying glass + light bulb)
//! Closing: U+1F50D U+1F4A1 U+1F4A1
//! Content between tags is JSON with "name" and "arguments" keys.

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id};

// Opening tag bytes (in UTF-8): 0xF0 0x9F 0x94 0x8D 0xF0 0x9F 0x92 0xA1
// We match via escaped Unicode in regex
static PATTERN: Lazy<Regex> = Lazy::new(|| {
    // Match both closed (\1) and unclosed (\2) variants
    let open = "\u{1f50d}\u{1f4a1}";
    let close = "\u{1f50d}\u{1f4a1}\u{1f4a1}";
    // We need to build the pattern dynamically
    Regex::new(&format!(
        r"{}\s*(.*?)\s*{}|{}\s*(.*)",
        regex::escape(open),
        regex::escape(close),
        regex::escape(open)
    ))
    .unwrap()
});

static OPEN_TAG: Lazy<String> = Lazy::new(|| "\u{1f50d}\u{1f4a1}".to_string());

/// Parser for Hermes and Qwen 2.5 tool calls.
pub struct HermesToolCallParser;

impl HermesToolCallParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HermesToolCallParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallParserTrait for HermesToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        // Quick check for opening tag
        if !text.contains(&*OPEN_TAG) {
            return (Some(text.to_string()), None);
        }

        let matches: Vec<_> = PATTERN.captures_iter(text).collect();
        if matches.is_empty() {
            return (Some(text.to_string()), None);
        }

        let mut tool_calls = Vec::new();
        for cap in &matches {
            let raw_json = cap.get(1).or_else(|| cap.get(2)).map(|m| m.as_str());
            let Some(json_str) = raw_json else { continue };
            let trimmed = json_str.trim();
            if trimmed.is_empty() {
                continue;
            }

            let Ok(data) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            let Some(name) = data.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let args = data
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let arguments = serde_json::to_string(&args).unwrap_or_default();

            tool_calls.push(ToolCall {
                id: gen_call_id(),
                name: name.to_string(),
                arguments,
            });
        }

        if tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        let first_idx = text.find(&*OPEN_TAG).unwrap();
        let content = text[..first_idx].trim().to_string();
        let content = if content.is_empty() { None } else { Some(content) };
        (content, Some(tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_tool_calls() {
        let parser = HermesToolCallParser::new();
        let text = "Hello, I can help with that!";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
