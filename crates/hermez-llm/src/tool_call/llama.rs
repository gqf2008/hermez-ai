#![allow(dead_code)]
//! Llama 3.x / 4 tool call parser.
//!
//! Format: JSON objects with "name" and "arguments" (or "parameters") keys.
//! May be preceded by <|python_tag|> token.

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id};

static BOT_TOKEN: &str = "<|python_tag|>";

/// Parser for Llama 3.x / 4 JSON-format tool calls.
pub struct LlamaToolCallParser;

impl LlamaToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for LlamaToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for LlamaToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains(BOT_TOKEN) && !text.contains('{') {
            return (Some(text.to_string()), None);
        }

        try_json_objects(text)
    }
}

/// Find a balanced JSON object starting at position 0.
fn find_json_object(text: &str) -> Option<(serde_json::Value, usize)> {
    if !text.starts_with('{') { return None; }
    
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    let bytes = text.as_bytes();
    
    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape_next = true,
            b'"' => in_string = !in_string,
            _ if !in_string => match b {
                b'{' | b'[' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let json_str = &text[..=i];
                        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(json_str) {
                            return Some((obj, i + 1));
                        }
                    }
                }
                b']' => depth -= 1,
                _ => {}
            },
            _ => {}
        }
    }
    None
}

fn try_json_objects(text: &str) -> ParseResult {
    let mut tool_calls = Vec::new();
    let mut pos = 0;
    
    while pos < text.len() {
        let remaining = &text[pos..];
        if let Some(brace_pos) = remaining.find('{') {
            let start = pos + brace_pos;
            if let Some((obj, json_len)) = find_json_object(&text[start..]) {
                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                    if let Some(args) = obj.get("arguments").or_else(|| obj.get("parameters")) {
                        let arguments = match args {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).unwrap_or_default(),
                        };
                        tool_calls.push(ToolCall {
                            id: gen_call_id(),
                            name: name.to_string(),
                            arguments,
                        });
                    }
                }
                pos = start + json_len;
            } else {
                pos = start + 1;
            }
        } else {
            break;
        }
    }
    
    if tool_calls.is_empty() {
        return (Some(text.to_string()), None);
    }

    let first_tc_start = text.find('{').unwrap_or(0);
    let bot_start = text.find(BOT_TOKEN).unwrap_or(text.len());
    let content_start = first_tc_start.min(bot_start);
    let content = if content_start > 0 {
        let c = text[..content_start].trim().to_string();
        if c.is_empty() { None } else { Some(c) }
    } else {
        None
    };
    (content, Some(tool_calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llama_single() {
        let parser = LlamaToolCallParser::new();
        let text = "<|python_tag|>{\"name\": \"get_weather\", \"arguments\": {\"city\": \"NYC\"}}";
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_llama_none() {
        let parser = LlamaToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
