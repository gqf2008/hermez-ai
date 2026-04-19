#![allow(dead_code)]
//! Longcat tool call parser.
//!
//! Same as Hermes but with <longcat_tool_call> tags.

use once_cell::sync::Lazy;
use regex::Regex;

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id};

static PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<longcat_tool_call>\s*(.*?)\s*</longcat_tool_call>|<longcat_tool_call>\s*(.*)").unwrap()
});

/// Parser for Longcat Flash Chat tool calls.
pub struct LongcatToolCallParser;

impl LongcatToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for LongcatToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for LongcatToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains("<longcat_tool_call>") {
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
            if trimmed.is_empty() { continue; }

            let Ok(data) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
            let Some(name) = data.get("name").and_then(|v| v.as_str()) else { continue };
            let args = data.get("arguments").cloned().unwrap_or(serde_json::Value::Object(Default::default()));
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

        let first_idx = text.find("<longcat_tool_call>").unwrap();
        let content = text[..first_idx].trim().to_string();
        let content = if content.is_empty() { None } else { Some(content) };
        (content, Some(tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_longcat_single() {
        let parser = LongcatToolCallParser::new();
        let text = r#"<longcat_tool_call>{"name": "search", "arguments": {"q": "test"}}</longcat_tool_call>"#;
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "search");
    }

    #[test]
    fn test_longcat_none() {
        let parser = LongcatToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
