#![allow(dead_code)]
//! Mistral tool call parser.
//!
//! Supports two formats depending on tokenizer version:
//! - Pre-v11: `[TOOL_CALLS] [{"name": ..., "arguments": {...}}, ...]`
//! - v11+:    `[TOOL_CALLS]tool_name1{"arg": "val"}[TOOL_CALLS]tool_name2{"arg": "val"}`

use super::{ToolCall, ParseResult, ToolCallParserTrait, gen_call_id};

static BOT_TOKEN: &str = "[TOOL_CALLS]";

/// Parser for Mistral-format tool calls.
pub struct MistralToolCallParser;

impl MistralToolCallParser {
    pub fn new() -> Self { Self }
}

impl Default for MistralToolCallParser {
    fn default() -> Self { Self::new() }
}

impl ToolCallParserTrait for MistralToolCallParser {
    fn parse(&self, text: &str) -> ParseResult {
        if !text.contains(BOT_TOKEN) {
            return (Some(text.to_string()), None);
        }

        let parts: Vec<&str> = text.split(BOT_TOKEN).collect();
        if parts.len() < 2 {
            return (Some(text.to_string()), None);
        }

        let content = parts[0].trim();
        let raw_tool_calls: Vec<&str> = parts[1..].to_vec();

        if raw_tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        let first_raw = raw_tool_calls[0].trim();
        let is_pre_v11 = first_raw.starts_with('[') || first_raw.starts_with('{');

        let mut tool_calls = Vec::new();

        if !is_pre_v11 {
            // v11+ format: tool_name{args} after each [TOOL_CALLS]
            for raw in &raw_tool_calls {
                let raw = raw.trim();
                if raw.is_empty() || !raw.contains('{') {
                    continue;
                }
                let brace_idx = raw.find('{').unwrap();
                let tool_name = raw[..brace_idx].trim().to_string();
                let args_str = raw[brace_idx..].trim().to_string();

                // Validate JSON (keep as-is if parsing fails)
                let _ = serde_json::from_str::<serde_json::Value>(&args_str);

                tool_calls.push(ToolCall {
                    id: gen_call_id(),
                    name: tool_name,
                    arguments: args_str,
                });
            }
        } else {
            // Pre-v11 format: JSON array or single object
            let parsed = serde_json::from_str::<serde_json::Value>(first_raw);
            let items: Vec<serde_json::Value> = match parsed {
                Ok(serde_json::Value::Array(arr)) => arr,
                Ok(serde_json::Value::Object(_)) => vec![parsed.unwrap()],
                _ => {
                    // Fallback: try to extract JSON objects by scanning
                    extract_json_objects(first_raw)
                }
            };

            for item in &items {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    let args = item.get("arguments").unwrap_or(&serde_json::Value::Null);
                    let arguments = match args {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                            serde_json::to_string(args).unwrap_or_default()
                        }
                        _ => "{}".to_string(),
                    };
                    tool_calls.push(ToolCall {
                        id: gen_call_id(),
                        name: name.to_string(),
                        arguments,
                    });
                }
            }
        }

        if tool_calls.is_empty() {
            return (Some(text.to_string()), None);
        }

        let content_str = if content.is_empty() {
            None
        } else {
            Some(content.to_string())
        };
        (content_str, Some(tool_calls))
    }
}

fn extract_json_objects(text: &str) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    let decoder = json::RawDecoder;
    let mut pos = 0;
    let bytes = text.as_bytes();

    while pos < bytes.len() {
        if let Some((obj, end)) = decoder.decode(&text[pos..]) {
            results.push(obj);
            pos += end;
        } else {
            pos += 1;
        }
    }
    results
}

mod json {
    use serde_json::Value;

    /// Simple raw JSON decoder that finds top-level objects.
    pub struct RawDecoder;

    impl RawDecoder {
        pub fn decode(&self, text: &str) -> Option<(Value, usize)> {
            let bytes = text.as_bytes();
            if !bytes.starts_with(b"{") {
                // Skip to next '{'
                if let Some(pos) = text.find('{') {
                    return self.decode(&text[pos..]);
                }
                return None;
            }

            let mut depth = 0i32;
            let mut in_string = false;
            let mut escape_next = false;

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
                                if let Ok(obj) = serde_json::from_str::<Value>(json_str) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mistral_v11_single() {
        let parser = MistralToolCallParser::new();
        let text = "[TOOL_CALLS]get_weather{\"city\": \"NYC\"}";
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_mistral_pre_v11() {
        let parser = MistralToolCallParser::new();
        let text = "[TOOL_CALLS] [{\"name\": \"get_weather\", \"arguments\": {\"city\": \"NYC\"}}]";
        let (content, tool_calls) = parser.parse(text);
        assert!(content.is_none());
        let tc = tool_calls.unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "get_weather");
    }

    #[test]
    fn test_mistral_none() {
        let parser = MistralToolCallParser::new();
        let text = "Hello, no tool calls.";
        let (content, tool_calls) = parser.parse(text);
        assert_eq!(content, Some(text.to_string()));
        assert!(tool_calls.is_none());
    }
}
