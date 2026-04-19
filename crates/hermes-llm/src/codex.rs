#![allow(dead_code)]
//! OpenAI Codex Responses API adapter.
//!
//! Mirrors the Python Codex Responses API support in `run_agent.py:3709-4627`.
//! The Responses API is distinct from Chat Completions:
//! - Input format: list of items with `type` field (not `role`/`content` messages)
//! - `instructions` replaces `system` message
//! - `max_output_tokens` replaces `max_tokens`
//! - `store=false` required (no server-side persistence)
//! - Uses `responses.stream()` instead of `chat.completions.create()`

use bytes::Bytes;
use futures::Stream;
use reqwest::Client as HttpClient;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::error_classifier::{classify_api_error, ClassifiedError};

// ---------------------------------------------------------------------------
// Streaming events
// ---------------------------------------------------------------------------

/// Single event emitted from a Responses API streaming response.
#[derive(Debug, Clone)]
pub enum CodexStreamEvent {
    /// Text content delta (output_text.delta)
    TextDelta { delta: String },
    /// Reasoning text delta (reasoning.*delta)
    ReasoningDelta { delta: String },
    /// Function call detected during stream
    FunctionCall { call_id: String, name: String, arguments: String },
    /// Completed output item (response.output_item.done)
    OutputItemDone { item: Value },
    /// Terminal: response completed
    ResponseCompleted { response: Value },
    /// Terminal: response incomplete
    ResponseIncomplete { response: Value },
    /// Terminal: response failed
    ResponseFailed { response: Value },
    /// Raw event for anything else
    Raw { event_type: String, data: Value },
}

/// Stream wrapper that parses SSE from a reqwest body.
pub struct CodexResponseStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    buffer: String,
    finished: bool,
    /// Parsed events waiting to be emitted (avoids re-parsing on multi-event buffers).
    event_queue: VecDeque<CodexStreamEvent>,
}

impl CodexResponseStream {
    fn new(body: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(body),
            buffer: String::new(),
            finished: false,
            event_queue: VecDeque::new(),
        }
    }

    /// Parse buffered SSE lines into events.
    fn drain_buffer(&mut self) -> Vec<CodexStreamEvent> {
        let mut events = Vec::new();
        let mut pending_event: Option<String> = None;
        let mut pending_data: Option<String> = None;

        // Process complete SSE messages (separated by blank lines)
        let mut pos = 0;
        let text = &self.buffer;
        while pos < text.len() {
            // Find next blank line separator
            let mut end = pos;
            let mut found_separator = false;
            while end < text.len() {
                // Check for double newline (SSE message separator)
                if end + 1 < text.len() && &text[end..end + 2] == "\n\n" {
                    end += 2;
                    found_separator = true;
                    break;
                }
                if end + 1 < text.len() && &text[end..end + 2] == "\r\n" {
                    // Check if next is also \r\n
                    if end + 3 < text.len() && &text[end + 2..end + 4] == "\r\n" {
                        end += 4;
                        found_separator = true;
                        break;
                    }
                }
                end += 1;
            }

            // If we reached the end without finding a separator, the message
            // is incomplete — keep the tail in the buffer for next read.
            if !found_separator {
                break;
            }

            let chunk = &text[pos..end];
            pos = end;

            if chunk.trim().is_empty() {
                continue;
            }

            // Parse the SSE chunk
            for line in chunk.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("event: ") {
                    pending_event = Some(rest.to_string());
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    pending_data = Some(rest.to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    pending_data = Some(rest.to_string());
                }
            }

            // If we have both event and data, emit
            if let (Some(event_type), Some(data_str)) = (pending_event.take(), pending_data.take()) {
                if let Some(evt) = parse_sse_event(&event_type, &data_str) {
                    events.push(evt);
                }
            }
        }

        // Keep the unprocessed tail in the buffer
        self.buffer = self.buffer[pos..].to_string();
        events
    }
}

impl Stream for CodexResponseStream {
    type Item = CodexStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }

        // 1. Return any queued events first (avoids re-parsing)
        if let Some(evt) = self.event_queue.pop_front() {
            return Poll::Ready(Some(evt));
        }

        // 2. Try to drain buffered content
        let drained = self.drain_buffer();
        let mut iter = drained.into_iter();
        if let Some(first) = iter.next() {
            // Queue remaining events for subsequent polls
            self.event_queue.extend(iter);
            return Poll::Ready(Some(first));
        }

        // 3. Poll the inner stream for more bytes
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                let text = String::from_utf8_lossy(&b).to_string();
                self.buffer.push_str(&text);
                let drained = self.drain_buffer();
                let mut iter = drained.into_iter();
                if let Some(first) = iter.next() {
                    self.event_queue.extend(iter);
                    return Poll::Ready(Some(first));
                }
                // Not enough for a full SSE message yet — re-schedule
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Some(Err(e))) => {
                self.finished = true;
                let err_msg = e.to_string();
                Poll::Ready(Some(CodexStreamEvent::Raw {
                    event_type: "error".to_string(),
                    data: json!({ "error": err_msg }),
                }))
            }
            Poll::Ready(None) => {
                // Stream ended — drain any remaining
                self.finished = true;
                let drained = self.drain_buffer();
                let mut iter = drained.into_iter();
                if let Some(first) = iter.next() {
                    self.event_queue.extend(iter);
                    return Poll::Ready(Some(first));
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Parse an SSE event type + data string into a `CodexStreamEvent`.
fn parse_sse_event(event_type: &str, data_str: &str) -> Option<CodexStreamEvent> {
    // Skip [DONE] sentinel
    if data_str.trim() == "[DONE]" {
        return None;
    }

    let data: Value = match serde_json::from_str(data_str) {
        Ok(v) => v,
        Err(_) => {
            return Some(CodexStreamEvent::Raw {
                event_type: event_type.to_string(),
                data: json!({ "raw": data_str }),
            });
        }
    };

    match event_type {
        "response.output_text.delta" => {
            let delta = data.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
            if !delta.is_empty() {
                Some(CodexStreamEvent::TextDelta { delta })
            } else {
                None
            }
        }
        // Various reasoning delta event types
        t if t.contains("reasoning") && t.contains("delta") => {
            let delta = data.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
            if !delta.is_empty() {
                Some(CodexStreamEvent::ReasoningDelta { delta })
            } else {
                None
            }
        }
        "response.output_item.done" => {
            let item = data.get("item").cloned().unwrap_or(data.clone());
            // Check if it's a function_call
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string();
                Some(CodexStreamEvent::FunctionCall {
                    call_id,
                    name,
                    arguments,
                })
            } else {
                Some(CodexStreamEvent::OutputItemDone { item })
            }
        }
        "response.completed" => Some(CodexStreamEvent::ResponseCompleted {
            response: data.get("response").cloned().unwrap_or(data),
        }),
        "response.incomplete" => Some(CodexStreamEvent::ResponseIncomplete {
            response: data.get("response").cloned().unwrap_or(data),
        }),
        "response.failed" => Some(CodexStreamEvent::ResponseFailed {
            response: data.get("response").cloned().unwrap_or(data),
        }),
        "function_call" => {
            // Direct function_call event (some providers emit this)
            let call_id = data.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = data.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let arguments = data
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            Some(CodexStreamEvent::FunctionCall {
                call_id,
                name,
                arguments,
            })
        }
        _ => Some(CodexStreamEvent::Raw {
            event_type: event_type.to_string(),
            data,
        }),
    }
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Convert chat messages to Responses API input items.
///
/// Mirrors Python `_chat_messages_to_responses_input` (run_agent.py:3709).
/// - System messages are skipped
/// - User/assistant messages become `{"role": ..., "content": ...}`
/// - Assistant tool_calls become `{"type": "function_call", ...}`
/// - Tool responses become `{"type": "function_call_output", ...}`
/// - Encrypted reasoning items are replayed (without `id`, store=false)
/// - Image content blocks for vision are preserved
pub fn chat_to_responses_input(messages: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    let mut seen_item_ids: HashSet<String> = HashSet::new();

    for msg in messages {
        let Some(msg_obj) = msg.as_object() else { continue };
        let Some(role) = msg_obj.get("role").and_then(Value::as_str) else { continue };

        match role {
            // System messages become instructions — skip here
            "system" => {}

            "user" | "assistant" => {
                if role == "assistant" {
                    // Replay encrypted reasoning items from previous turns
                    // so the API can maintain coherent reasoning chains.
                    if let Some(reasoning_arr) = msg_obj.get("codex_reasoning_items").and_then(Value::as_array) {
                        for ri in reasoning_arr {
                            if let Some(ri_obj) = ri.as_object() {
                                if ri_obj.get("encrypted_content").is_some() {
                                    if let Some(item_id) = ri_obj.get("id").and_then(Value::as_str) {
                                        if seen_item_ids.contains(item_id) {
                                            continue;
                                        }
                                        seen_item_ids.insert(item_id.to_string());
                                    }
                                    // Strip the "id" field — with store=false the
                                    // Responses API cannot look up items by ID.
                                    let mut replay: serde_json::Map<String, Value> = serde_json::Map::new();
                                    for (k, v) in ri_obj {
                                        if k != "id" {
                                            replay.insert(k.clone(), v.clone());
                                        }
                                    }
                                    items.push(Value::Object(replay));
                                }
                            }
                        }
                    }
                }

                let content = msg_obj.get("content").and_then(Value::as_str);

                if role == "assistant" {
                    // Tool calls -> function_call items
                    if let Some(tool_calls) = msg_obj.get("tool_calls").and_then(Value::as_array) {
                        for tc in tool_calls {
                            let Some(tc_obj) = tc.as_object() else { continue };
                            let Some(fn_obj) = tc_obj.get("function").and_then(Value::as_object) else { continue };

                            let Some(fn_name) = fn_obj.get("name").and_then(Value::as_str) else { continue };
                            if fn_name.trim().is_empty() {
                                continue;
                            }

                            // Normalize call_id
                            let call_id = normalize_call_id(tc, &items.len());

                            // Normalize arguments to string
                            let arguments = normalize_tool_arguments(fn_obj.get("arguments"));

                            items.push(json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": fn_name,
                                "arguments": arguments,
                            }));
                        }
                    }

                    // Text content
                    if let Some(text) = content {
                        if !text.trim().is_empty() {
                            items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": text,
                            }));
                        } else if items.last().map(|i| {
                            // Check if last item was a reasoning item (has encrypted_content)
                            i.get("encrypted_content").is_some()
                        }) == Some(true) {
                            // Responses API requires a following item after each
                            // reasoning item. Emit empty assistant message.
                            items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": "",
                            }));
                        }
                    } else if items.last().map(|i| i.get("encrypted_content").is_some()) == Some(true) {
                        items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": "",
                        }));
                    }
                } else {
                    // User message — handle multimodal content
                    items.push(build_user_message(msg_obj));
                }
            }

            "tool" => {
                let raw_tool_call_id = msg_obj.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                let (call_id, _) = split_responses_tool_id(raw_tool_call_id);
                let call_id = if call_id.trim().is_empty() && !raw_tool_call_id.trim().is_empty() {
                    raw_tool_call_id.trim().to_string()
                } else {
                    call_id
                };
                if call_id.trim().is_empty() {
                    continue;
                }

                let output = msg_obj
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }

            _ => {}
        }
    }

    items
}

/// Build a user message item, handling both plain text and multimodal content.
fn build_user_message(msg_obj: &serde_json::Map<String, Value>) -> Value {
    let content = msg_obj.get("content");

    // Check for multimodal content array
    if let Some(content_arr) = content.and_then(Value::as_array) {
        let mut parts = Vec::new();
        for part in content_arr {
            if let Some(part_obj) = part.as_object() {
                match part_obj.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part_obj.get("text").and_then(Value::as_str) {
                            parts.push(json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                    }
                    Some("image_url") => {
                        if let Some(image_url) = part_obj.get("image_url") {
                            // Pass through image_url as-is (supports both URL and base64)
                            parts.push(json!({
                                "type": "input_image",
                                "image_url": image_url,
                            }));
                        }
                    }
                    _ => {
                        // Pass through unknown content types
                        parts.push(part.clone());
                    }
                }
            } else if let Some(text) = part.as_str() {
                parts.push(json!({
                    "type": "input_text",
                    "text": text,
                }));
            }
        }
        if parts.is_empty() {
            // Fallback to empty text
            parts.push(json!({ "type": "input_text", "text": "" }));
        }
        return json!({
            "type": "message",
            "role": "user",
            "content": parts,
        });
    }

    // Plain text content
    let text = content.and_then(Value::as_str).unwrap_or("");
    json!({
        "type": "message",
        "role": "user",
        "content": text,
    })
}

/// Normalize tool arguments to a JSON string.
fn normalize_tool_arguments(arguments: Option<&Value>) -> String {
    match arguments {
        Some(Value::Object(_)) | Some(Value::Array(_)) => {
            serde_json::to_string(arguments.unwrap()).unwrap_or_else(|_| "{}".to_string())
        }
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() { "{}".to_string() } else { s.to_string() }
        }
        Some(v) => {
            let s = v.to_string();
            if s.is_empty() { "{}".to_string() } else { s }
        }
        None => "{}".to_string(),
    }
}

/// Split a Responses API tool ID into (call_id, response_item_id).
///
/// Mirrors Python `_split_responses_tool_id`.
/// Format: `call_<suffix>` -> response_item_id = `fc_<suffix>`
fn split_responses_tool_id(tool_id: &str) -> (String, String) {
    let trimmed = tool_id.trim();
    if let Some(suffix) = trimmed.strip_prefix("call_") {
        let response_item_id = format!("fc_{suffix}");
        return (trimmed.to_string(), response_item_id);
    }
    (trimmed.to_string(), trimmed.to_string())
}

/// Normalize a call_id for function_call items.
///
/// Mirrors Python call_id normalization logic.
fn normalize_call_id(tc: &Value, item_index: &usize) -> String {
    let tc_obj = tc.as_object().unwrap();

    // Try explicit call_id
    if let Some(call_id) = tc_obj.get("call_id").and_then(Value::as_str) {
        if !call_id.trim().is_empty() {
            return call_id.trim().to_string();
        }
    }

    // Try id field with embedded response format
    if let Some(id) = tc_obj.get("id").and_then(Value::as_str) {
        let (call_id, response_item_id) = split_responses_tool_id(id);
        if !call_id.trim().is_empty() && call_id.starts_with("call_") {
            return call_id;
        }
        // If response_item_id starts with fc_, derive call_id
        if response_item_id.starts_with("fc_") && response_item_id.len() > "fc_".len() {
            return format!("call_{}", &response_item_id["fc_".len()..]);
        }
    }

    // Deterministic fallback
    let fn_name = tc_obj
        .get("function")
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let raw_args = tc_obj
        .get("function")
        .and_then(|v| v.get("arguments"))
        .map(|v| v.to_string())
        .unwrap_or_default();
    deterministic_call_id(fn_name, &raw_args, *item_index)
}

/// Generate a deterministic call_id from function name, arguments, and index.
fn deterministic_call_id(fn_name: &str, raw_args: &str, index: usize) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("{fn_name}:{raw_args}:{index}"));
    let digest = hasher.finalize();
    let hex = hex::encode(digest);
    format!("fc_{}", &hex[..24])
}

// ---------------------------------------------------------------------------
// Preflight / validation
// ---------------------------------------------------------------------------

/// Validated and normalized Codex Responses API parameters.
#[derive(Debug, Clone)]
pub struct CodexApiParams {
    pub model: String,
    pub instructions: String,
    pub input: Vec<Value>,
    pub tools: Option<Vec<Value>>,
    pub store: bool,
    pub reasoning: Option<Value>,
    pub include: Option<Vec<Value>>,
    pub max_output_tokens: Option<usize>,
    pub temperature: Option<f64>,
    pub tool_choice: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    pub prompt_cache_key: Option<String>,
    pub service_tier: Option<String>,
    pub stream: Option<bool>,
}

/// Validate and normalize Codex API parameters.
///
/// Mirrors Python `_preflight_codex_api_kwargs` (run_agent.py:3911).
pub fn preflight_codex_kwargs(
    api_kwargs: &Value,
    allow_stream: bool,
) -> anyhow::Result<CodexApiParams> {
    let obj = api_kwargs.as_object().ok_or_else(|| {
        anyhow::anyhow!("Codex Responses request must be a dict.")
    })?;

    // Check required fields
    let required = ["model", "instructions", "input"];
    let missing: Vec<_> = required
        .iter()
        .filter(|k| !obj.contains_key(**k))
        .collect();
    if !missing.is_empty() {
        let names: Vec<_> = missing.iter().map(|s| **s).collect();
        return Err(anyhow::anyhow!(
            "Codex Responses request missing required field(s): {}.",
            names.join(", ")
        ));
    }

    // Validate model
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Codex Responses request 'model' must be a non-empty string."))?;
    if model.trim().is_empty() {
        return Err(anyhow::anyhow!("Codex Responses request 'model' must be a non-empty string."));
    }
    let model = model.trim().to_string();

    // Validate instructions (default to empty string)
    let instructions = match obj.get("instructions") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) => v.to_string().trim().to_string(),
        None => String::new(),
    };

    // Normalize input
    let raw_input = obj.get("input").ok_or_else(|| {
        anyhow::anyhow!("Codex Responses request missing required field(s): input.")
    })?;
    let normalized_input = preflight_codex_input_items(raw_input)?;

    // Normalize tools
    let tools = obj.get("tools");
    let normalized_tools = if let Some(tools) = tools {
        let tools_arr = tools.as_array().ok_or_else(|| {
            anyhow::anyhow!("Codex Responses request 'tools' must be a list when provided.")
        })?;
        let mut normalized = Vec::new();
        for (idx, tool) in tools_arr.iter().enumerate() {
            let tool_obj = tool.as_object().ok_or_else(|| {
                anyhow::anyhow!("Codex Responses tools[{idx}] must be an object.")
            })?;
            if tool_obj.get("type").and_then(Value::as_str) != Some("function") {
                return Err(anyhow::anyhow!(
                    "Codex Responses tools[{idx}] has unsupported type {:?}.",
                    tool_obj.get("type")
                ));
            }
            let name = tool_obj.get("name").and_then(Value::as_str).ok_or_else(|| {
                anyhow::anyhow!("Codex Responses tools[{idx}] is missing a valid name.")
            })?;
            if name.trim().is_empty() {
                return Err(anyhow::anyhow!("Codex Responses tools[{idx}] is missing a valid name."));
            }
            let parameters = tool_obj.get("parameters").and_then(Value::as_object).ok_or_else(|| {
                anyhow::anyhow!("Codex Responses tools[{idx}] is missing valid parameters.")
            })?;

            let description = tool_obj
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let strict = tool_obj
                .get("strict")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            normalized.push(json!({
                "type": "function",
                "name": name.trim(),
                "description": description,
                "strict": strict,
                "parameters": parameters,
            }));
        }
        Some(normalized)
    } else {
        None
    };

    // Validate store (must be false)
    let store = obj.get("store").and_then(Value::as_bool).unwrap_or(false);
    if store {
        return Err(anyhow::anyhow!("Codex Responses contract requires 'store' to be false."));
    }

    // Build normalized params — include "stream" in allowed keys so we
    // can give a specific error message about it below.
    let allowed_keys: HashSet<&str> = [
        "model", "instructions", "input", "tools", "store",
        "reasoning", "include", "max_output_tokens", "temperature",
        "tool_choice", "parallel_tool_calls", "prompt_cache_key", "service_tier",
        "stream",
    ]
    .into_iter()
    .collect();

    let mut unexpected: Vec<_> = obj
        .keys()
        .filter(|k| !allowed_keys.contains(k.as_str()))
        .cloned()
        .collect();
    unexpected.sort();
    if !unexpected.is_empty() {
        return Err(anyhow::anyhow!(
            "Codex Responses request has unsupported field(s): {}.",
            unexpected.join(", ")
        ));
    }

    // Validate stream flag
    if let Some(stream_val) = obj.get("stream") {
        if allow_stream {
            if let Some(b) = stream_val.as_bool() {
                if !b {
                    return Err(anyhow::anyhow!("Codex Responses 'stream' must be true when set."));
                }
            } else {
                return Err(anyhow::anyhow!("Codex Responses 'stream' must be true when set."));
            }
        } else {
            return Err(anyhow::anyhow!("Codex Responses stream flag is only allowed in fallback streaming requests."));
        }
    }

    // Optional passthrough fields
    let reasoning = obj.get("reasoning").filter(|v| v.is_object()).cloned();
    let include = obj.get("include").and_then(Value::as_array).cloned();
    let service_tier = obj
        .get("service_tier")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());

    let max_output_tokens = obj
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .map(|v| v as usize);

    let temperature = obj.get("temperature").and_then(Value::as_f64);

    let tool_choice = obj.get("tool_choice").cloned();
    let parallel_tool_calls = obj.get("parallel_tool_calls").and_then(Value::as_bool);
    let prompt_cache_key = obj
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let stream = if allow_stream {
        obj.get("stream").and_then(Value::as_bool)
    } else {
        None
    };

    Ok(CodexApiParams {
        model,
        instructions,
        input: normalized_input,
        tools: normalized_tools,
        store: false,
        reasoning,
        include,
        max_output_tokens,
        temperature,
        tool_choice,
        parallel_tool_calls,
        prompt_cache_key,
        service_tier,
        stream,
    })
}

/// Validate and normalize Responses API input items.
///
/// Mirrors Python `_preflight_codex_input_items` (run_agent.py:3818).
fn preflight_codex_input_items(raw_items: &Value) -> anyhow::Result<Vec<Value>> {
    let arr = raw_items.as_array().ok_or_else(|| {
        anyhow::anyhow!("Codex Responses input must be a list of input items.")
    })?;

    let mut normalized = Vec::new();
    for (idx, item) in arr.iter().enumerate() {
        let item_obj = item.as_object().ok_or_else(|| {
            anyhow::anyhow!("Codex Responses input[{idx}] must be an object.")
        })?;

        let item_type = item_obj.get("type").and_then(Value::as_str);
        let role = item_obj.get("role").and_then(Value::as_str);

        match item_type {
            Some("function_call") => {
                let call_id = item_obj.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    anyhow::anyhow!("Codex Responses input[{idx}] function_call is missing call_id.")
                })?;
                if call_id.trim().is_empty() {
                    return Err(anyhow::anyhow!("Codex Responses input[{idx}] function_call is missing call_id."));
                }
                let name = item_obj.get("name").and_then(Value::as_str).ok_or_else(|| {
                    anyhow::anyhow!("Codex Responses input[{idx}] function_call is missing name.")
                })?;
                if name.trim().is_empty() {
                    return Err(anyhow::anyhow!("Codex Responses input[{idx}] function_call is missing name."));
                }
                let arguments = normalize_tool_arguments(item_obj.get("arguments"));

                normalized.push(json!({
                    "type": "function_call",
                    "call_id": call_id.trim(),
                    "name": name.trim(),
                    "arguments": arguments,
                }));
            }
            Some("function_call_output") => {
                let call_id = item_obj.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    anyhow::anyhow!("Codex Responses input[{idx}] function_call_output is missing call_id.")
                })?;
                if call_id.trim().is_empty() {
                    return Err(anyhow::anyhow!("Codex Responses input[{idx}] function_call_output is missing call_id."));
                }
                let output = item_obj
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                normalized.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id.trim(),
                    "output": output,
                }));
            }
            Some(t) => {
                return Err(anyhow::anyhow!(
                    "Codex Responses input[{idx}] has unsupported type {t:?}."
                ));
            }
            None => {
                // Role-based item (message)
                match role {
                    Some("user") | Some("assistant") => {
                        let content = match item_obj.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        normalized.push(json!({
                            "role": role.unwrap(),
                            "content": content,
                        }));
                    }
                    Some(r) => {
                        return Err(anyhow::anyhow!(
                            "Codex Responses input[{idx}] has unsupported item shape (type=null, role={r:?})."
                        ));
                    }
                    None => {
                        return Err(anyhow::anyhow!(
                            "Codex Responses input[{idx}] has unsupported item shape (type=null, role=null)."
                        ));
                    }
                }
            }
        }
    }

    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Build request body
// ---------------------------------------------------------------------------

/// Build a Responses API request body from validated params.
pub fn build_responses_body(params: &CodexApiParams) -> Value {
    let mut body = json!({
        "model": params.model,
        "instructions": params.instructions,
        "input": params.input,
        "store": false,
    });

    if let Some(ref tools) = params.tools {
        body["tools"] = json!(tools);
    }
    if let Some(ref reasoning) = params.reasoning {
        body["reasoning"] = reasoning.clone();
    }
    if let Some(ref include) = params.include {
        body["include"] = json!(include);
    }
    if let Some(max) = params.max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(temp) = params.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(ref tc) = params.tool_choice {
        body["tool_choice"] = tc.clone();
    }
    if let Some(ptc) = params.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(ptc);
    }
    if let Some(ref key) = params.prompt_cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    if let Some(ref tier) = params.service_tier {
        body["service_tier"] = json!(tier);
    }
    if params.stream == Some(true) {
        body["stream"] = json!(true);
    }

    body
}

/// Build a Responses API request body from individual parts (legacy helper).
fn build_responses_body_from_parts(
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: Option<&[Value]>,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
) -> Value {
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "store": false,
    });

    if let Some(t) = tools {
        let responses_tools: Vec<Value> = t.iter().map(|tool| {
            if let Some(function) = tool.get("function") {
                json!({
                    "type": "function",
                    "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": function.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                })
            } else {
                tool.clone()
            }
        }).collect();
        body["tools"] = json!(responses_tools);
    }

    if let Some(max) = max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }

    if let Some(temp) = temperature {
        body["temperature"] = json!(temp);
    }

    body
}

// ---------------------------------------------------------------------------
// API calls
// ---------------------------------------------------------------------------

/// Call OpenAI Responses API (non-streaming).
///
/// Mirrors Python `_run_codex_create` (non-streaming variant).
pub async fn call_responses_api(
    params: &CodexApiParams,
    base_url: &str,
    api_key: &str,
    timeout_secs: u64,
) -> Result<ResponsesApiResponse, ClassifiedError> {
    let body = build_responses_body(params);

    call_responses_api_raw(&body, base_url, api_key, timeout_secs, false).await
}

/// Call OpenAI Responses API (non-streaming) — legacy signature for backwards compatibility.
pub async fn call_responses_api_legacy(
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: Option<&[Value]>,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
    base_url: &str,
    api_key: &str,
    timeout_secs: u64,
) -> Result<ResponsesApiResponse, ClassifiedError> {
    let body = build_responses_body_from_parts(
        model, instructions, input, tools, max_output_tokens, temperature,
    );

    call_responses_api_raw(&body, base_url, api_key, timeout_secs, false).await
}

/// Raw Responses API call with a pre-built body.
async fn call_responses_api_raw(
    body: &Value,
    base_url: &str,
    api_key: &str,
    timeout_secs: u64,
    is_stream: bool,
) -> Result<ResponsesApiResponse, ClassifiedError> {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let client = HttpClient::builder()
        .user_agent("hermes-llm/0.1.0")
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| {
            classify_api_error(
                "openai_responses",
                "",
                None,
                &format!("Failed to build HTTP client: {e}"),
            )
        })?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "responses=v1")
        .json(body)
        .send()
        .await
        .map_err(|e| {
            classify_api_error(
                "openai_responses",
                body.get("model").and_then(Value::as_str).unwrap_or(""),
                None,
                &format!("Request failed: {e}"),
            )
        })?;

    let status = resp.status().as_u16();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if status >= 400 {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_api_error("openai_responses", &model, Some(status), &text));
    }

    if is_stream {
        // For streaming, we return a placeholder; the caller uses the stream directly.
        return Ok(ResponsesApiResponse {
            id: String::new(),
            model,
            status: "streaming".to_string(),
            output: Vec::new(),
            usage: None,
        });
    }

    let text = resp.text().await.unwrap_or_default();
    let response: ResponsesApiResponse = serde_json::from_str(&text).map_err(|e| {
        classify_api_error(
            "openai_responses",
            &model,
            Some(status),
            &format!("Failed to parse response: {e}"),
        )
    })?;

    Ok(response)
}

/// Call OpenAI Responses API with SSE streaming.
///
/// Returns a `CodexResponseStream` that yields `CodexStreamEvent`s.
/// Mirrors Python `_run_codex_stream` (run_agent.py:4508).
pub async fn call_codex_responses_stream(
    params: &CodexApiParams,
    base_url: &str,
    api_key: &str,
    timeout_secs: u64,
) -> Result<CodexResponseStream, ClassifiedError> {
    let mut stream_params = params.clone();
    stream_params.stream = Some(true);
    let body = build_responses_body(&stream_params);

    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let client = HttpClient::builder()
        .user_agent("hermes-llm/0.1.0")
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| {
            classify_api_error(
                "openai_responses",
                &params.model,
                None,
                &format!("Failed to build HTTP client: {e}"),
            )
        })?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "responses=v1")
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            classify_api_error(
                "openai_responses",
                &params.model,
                None,
                &format!("Request failed: {e}"),
            )
        })?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_api_error(
            "openai_responses",
            &params.model,
            Some(status),
            &text,
        ));
    }

    let stream = resp.bytes_stream();

    Ok(CodexResponseStream::new(stream))
}

// ---------------------------------------------------------------------------
// Response structures
// ---------------------------------------------------------------------------

/// Responses API response structure.
#[derive(Debug, Clone)]
pub struct ResponsesApiResponse {
    pub id: String,
    pub model: String,
    pub status: String,
    pub output: Vec<Value>,
    pub usage: Option<ResponsesApiUsage>,
}

impl<'de> serde::Deserialize<'de> for ResponsesApiResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let val = Value::deserialize(deserializer)?;
        let obj = val.as_object().ok_or_else(|| serde::de::Error::custom("expected object"))?;

        Ok(Self {
            id: obj.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            model: obj.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
            status: obj.get("status").and_then(Value::as_str).unwrap_or("completed").to_string(),
            output: obj.get("output").and_then(Value::as_array).cloned().unwrap_or_default(),
            usage: obj.get("usage").and_then(|u| serde_json::from_value(u.clone()).ok()),
        })
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ResponsesApiUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Extract text content from Responses API output.
pub fn extract_text_from_output(output: &[Value]) -> String {
    let mut parts = Vec::new();
    for item in output {
        if let Some(item_type) = item.get("type").and_then(Value::as_str) {
            match item_type {
                "message" => {
                    if let Some(content_arr) = item.get("content").and_then(Value::as_array) {
                        for c in content_arr {
                            if c.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(text) = c.get("text").and_then(Value::as_str) {
                                    parts.push(text);
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    // Tool call detected — caller will handle
                }
                _ => {}
            }
        }
    }
    parts.join("\n")
}

/// Extract tool calls from Responses API output.
pub fn extract_tool_calls_from_output(output: &[Value]) -> Vec<Value> {
    let mut tool_calls = Vec::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            tool_calls.push(json!({
                "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": item.get("arguments").cloned().unwrap_or(json!({})),
                }
            }));
        }
    }
    tool_calls
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_to_responses_basic() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "What is 2+2?"}),
            json!({"role": "assistant", "content": "It's 4."}),
        ];
        let input = chat_to_responses_input(&messages);
        // System is skipped, user and assistant become items
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
    }

    #[test]
    fn test_chat_to_responses_with_tool_calls() {
        let messages = vec![
            json!({"role": "user", "content": "Search for Rust"}),
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {
                    "name": "web_search", "arguments": {"query": "Rust"}
                }}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "Results here"}),
        ];
        let input = chat_to_responses_input(&messages);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "Results here");
    }

    #[test]
    fn test_extract_text_from_output() {
        let output = vec![
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello!"}]
            }),
        ];
        let text = extract_text_from_output(&output);
        assert_eq!(text, "Hello!");
    }

    #[test]
    fn test_extract_tool_calls_from_output() {
        let output = vec![
            json!({
                "type": "function_call",
                "call_id": "call_abc",
                "name": "terminal",
                "arguments": {"command": "ls"}
            }),
        ];
        let tool_calls = extract_tool_calls_from_output(&output);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["function"]["name"], "terminal");
    }

    #[test]
    fn test_build_responses_body() {
        let params = CodexApiParams {
            model: "o3".to_string(),
            instructions: "You are a coding assistant".to_string(),
            input: vec![json!({"type": "message", "role": "user", "content": "Hi"})],
            tools: None,
            store: false,
            reasoning: None,
            include: None,
            max_output_tokens: Some(1000),
            temperature: Some(1.0),
            tool_choice: None,
            parallel_tool_calls: None,
            prompt_cache_key: None,
            service_tier: None,
            stream: None,
        };
        let body = build_responses_body(&params);
        assert_eq!(body["model"], "o3");
        assert_eq!(body["instructions"], "You are a coding assistant");
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["temperature"], 1.0);
    }

    #[test]
    fn test_system_message_skipped() {
        let messages = vec![
            json!({"role": "system", "content": "System prompt"}),
            json!({"role": "system", "content": "Another system"}),
            json!({"role": "user", "content": "Hello"}),
        ];
        let input = chat_to_responses_input(&messages);
        // Only user message remains
        assert_eq!(input.len(), 1);
    }

    // ── New tests for expanded features ─────────────────────────────────

    #[test]
    fn test_user_message_with_image() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
            ]
        })];
        let input = chat_to_responses_input(&messages);
        assert_eq!(input.len(), 1);
        let content = &input[0]["content"];
        assert!(content.is_array());
        let arr = content.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "input_text");
        assert_eq!(arr[1]["type"], "input_image");
    }

    #[test]
    fn test_preflight_codex_kwargs_basic() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "You are helpful",
            "input": [
                {"role": "user", "content": "Hello"}
            ],
            "store": false
        });
        let params = preflight_codex_kwargs(&kwargs, false).unwrap();
        assert_eq!(params.model, "o3");
        assert_eq!(params.instructions, "You are helpful");
        assert_eq!(params.input.len(), 1);
        assert_eq!(params.store, false);
    }

    #[test]
    fn test_preflight_missing_required_field() {
        let kwargs = json!({
            "model": "o3",
            "input": []
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("missing required field"));
        assert!(err.to_string().contains("instructions"));
    }

    #[test]
    fn test_preflight_store_must_be_false() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [],
            "store": true
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("store"));
        assert!(err.to_string().contains("false"));
    }

    #[test]
    fn test_preflight_unexpected_fields() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [],
            "store": false,
            "unknown_field": "value"
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("unsupported field"));
        assert!(err.to_string().contains("unknown_field"));
    }

    #[test]
    fn test_preflight_with_tools() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"role": "user", "content": "hi"}],
            "store": false,
            "tools": [{
                "type": "function",
                "name": "search",
                "description": "Search the web",
                "parameters": {"type": "object", "properties": {}},
                "strict": true
            }]
        });
        let params = preflight_codex_kwargs(&kwargs, false).unwrap();
        assert!(params.tools.is_some());
        let tools = params.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[0]["strict"], true);
    }

    #[test]
    fn test_preflight_with_reasoning_and_max_tokens() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"role": "user", "content": "hi"}],
            "store": false,
            "reasoning": {"effort": "high"},
            "max_output_tokens": 2048,
            "temperature": 0.7
        });
        let params = preflight_codex_kwargs(&kwargs, false).unwrap();
        assert!(params.reasoning.is_some());
        assert_eq!(params.max_output_tokens, Some(2048));
        assert_eq!(params.temperature, Some(0.7));
    }

    #[test]
    fn test_preflight_codex_input_items_validation() {
        // Missing call_id in function_call
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"type": "function_call", "name": "foo"}],
            "store": false
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("missing call_id"));
    }

    #[test]
    fn test_preflight_codex_input_items_validation_name() {
        // Missing name in function_call
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"type": "function_call", "call_id": "c1"}],
            "store": false
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("missing name"));
    }

    #[test]
    fn test_preflight_codex_input_items_validation_output_call_id() {
        // Missing call_id in function_call_output
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"type": "function_call_output", "output": "result"}],
            "store": false
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("function_call_output is missing call_id"));
    }

    #[test]
    fn test_preflight_codex_input_items_unsupported_type() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"type": "unknown_type"}],
            "store": false
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("unsupported type"));
    }

    #[test]
    fn test_preflight_with_stream_allowed() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"role": "user", "content": "hi"}],
            "store": false,
            "stream": true
        });
        let params = preflight_codex_kwargs(&kwargs, true).unwrap();
        assert_eq!(params.stream, Some(true));
    }

    #[test]
    fn test_preflight_stream_not_allowed() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"role": "user", "content": "hi"}],
            "store": false,
            "stream": true
        });
        let err = preflight_codex_kwargs(&kwargs, false).unwrap_err();
        assert!(err.to_string().contains("stream flag is only allowed"));
    }

    #[test]
    fn test_preflight_with_optional_passthrough() {
        let kwargs = json!({
            "model": "o3",
            "instructions": "test",
            "input": [{"role": "user", "content": "hi"}],
            "store": false,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "prompt_cache_key": "my-key",
            "service_tier": "auto"
        });
        let params = preflight_codex_kwargs(&kwargs, false).unwrap();
        assert!(params.tool_choice.is_some());
        assert_eq!(params.parallel_tool_calls, Some(true));
        assert_eq!(params.prompt_cache_key, Some("my-key".to_string()));
        assert_eq!(params.service_tier, Some("auto".to_string()));
    }

    #[test]
    fn test_deterministic_call_id() {
        let id1 = deterministic_call_id("search", "{\"q\":\"rust\"}", 0);
        let id2 = deterministic_call_id("search", "{\"q\":\"rust\"}", 0);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("fc_"));
        assert_eq!(id1.len(), "fc_".len() + 24);

        // Different args produce different IDs
        let id3 = deterministic_call_id("search", "{\"q\":\"python\"}", 0);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_split_responses_tool_id() {
        let (call_id, response_id) = split_responses_tool_id("call_abc123");
        assert_eq!(call_id, "call_abc123");
        assert_eq!(response_id, "fc_abc123");

        let (call_id2, response_id2) = split_responses_tool_id("some_other_id");
        assert_eq!(call_id2, "some_other_id");
        assert_eq!(response_id2, "some_other_id");
    }

    #[test]
    fn test_normalize_tool_arguments_object() {
        let result = normalize_tool_arguments(Some(&json!({"key": "value"})));
        assert_eq!(result, r#"{"key":"value"}"#);
    }

    #[test]
    fn test_normalize_tool_arguments_string() {
        let result = normalize_tool_arguments(Some(&json!("{\"key\":\"value\"}")));
        assert_eq!(result, r#"{"key":"value"}"#);
    }

    #[test]
    fn test_normalize_tool_arguments_null() {
        let result = normalize_tool_arguments(None);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_normalize_tool_arguments_empty_string() {
        let result = normalize_tool_arguments(Some(&json!("")));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_codex_reasoning_items_replayed() {
        let messages = vec![
            json!({"role": "user", "content": "Solve this"}),
            json!({
                "role": "assistant",
                "content": "",
                "codex_reasoning_items": [
                    {
                        "id": "item_001",
                        "type": "reasoning",
                        "encrypted_content": "base64data=="
                    }
                ]
            }),
        ];
        let input = chat_to_responses_input(&messages);
        // user message + reasoning item (content is empty so no assistant message)
        assert!(input.len() >= 1);
        // Check reasoning item is present with "type": "reasoning"
        let reasoning_items: Vec<_> = input
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("reasoning"))
            .collect();
        assert_eq!(reasoning_items.len(), 1);
        // ID field must be stripped
        assert!(reasoning_items[0].get("id").is_none());
        assert!(reasoning_items[0].get("encrypted_content").is_some());
    }

    #[test]
    fn test_codex_reasoning_items_deduplicated() {
        let messages = vec![
            json!({"role": "user", "content": "Solve"}),
            json!({
                "role": "assistant",
                "content": "",
                "codex_reasoning_items": [
                    {"id": "item_001", "type": "reasoning", "encrypted_content": "data1"}
                ]
            }),
            json!({
                "role": "assistant",
                "content": "",
                "codex_reasoning_items": [
                    {"id": "item_001", "type": "reasoning", "encrypted_content": "data1"}
                ]
            }),
        ];
        let input = chat_to_responses_input(&messages);
        let reasoning_items: Vec<_> = input
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("reasoning"))
            .collect();
        // Same ID should only appear once
        assert_eq!(reasoning_items.len(), 1);
    }

    #[test]
    fn test_empty_assistant_message_after_reasoning() {
        // When assistant has reasoning items but no text content,
        // an empty assistant message must follow.
        let messages = vec![
            json!({"role": "user", "content": "Solve"}),
            json!({
                "role": "assistant",
                "codex_reasoning_items": [
                    {"id": "item_001", "type": "reasoning", "encrypted_content": "data"}
                ],
                "content": ""
            }),
        ];
        let input = chat_to_responses_input(&messages);
        // user message + reasoning item + empty assistant message
        let has_empty_assistant = input.iter().any(|i| {
            i.get("role").and_then(Value::as_str) == Some("assistant")
                && i.get("content").and_then(Value::as_str) == Some("")
        });
        assert!(has_empty_assistant, "expected empty assistant message after reasoning");
    }

    // ---------------------------------------------------------------------------
    // SSE boundary tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_drain_buffer_empty() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: String::new(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert!(events.is_empty());
        assert!(stream.buffer.is_empty());
    }

    #[test]
    fn test_drain_buffer_incomplete_no_blank_line() {
        // No blank line separator — buffer should be kept for next read
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.output_text.delta\ndata: {\"delta\": \"hi\"}".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert!(events.is_empty());
        assert_eq!(stream.buffer, "event: response.output_text.delta\ndata: {\"delta\": \"hi\"}");
    }

    #[test]
    fn test_drain_buffer_single_message() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.output_text.delta\ndata: {\"delta\": \"hello\"}\n\n".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CodexStreamEvent::TextDelta { delta } if delta == "hello"));
        assert!(stream.buffer.is_empty());
    }

    #[test]
    fn test_drain_buffer_multiple_messages() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: concat!(
                "event: response.output_text.delta\n",
                "data: {\"delta\": \"hello\"}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"delta\": \" world\"}\n\n",
            )
            .to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], CodexStreamEvent::TextDelta { delta } if delta == "hello"));
        assert!(matches!(&events[1], CodexStreamEvent::TextDelta { delta } if delta == " world"));
    }

    #[test]
    fn test_drain_buffer_crlf_separator() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.completed\r\ndata: {\"response\": {}}\r\n\r\n".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CodexStreamEvent::ResponseCompleted { .. }));
    }

    #[test]
    fn test_drain_buffer_done_sentinel_skipped() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.output_text.delta\ndata: [DONE]\n\n".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert!(events.is_empty(), "[DONE] sentinel should be skipped");
    }

    #[test]
    fn test_drain_buffer_malformed_json_emits_raw() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.output_text.delta\ndata: not-json-at-all\n\n".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CodexStreamEvent::Raw { event_type, .. } if event_type == "response.output_text.delta"));
    }

    #[test]
    fn test_drain_buffer_function_call_event() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: concat!(
                "event: response.output_item.done\n",
                "data: {\"item\": {\"type\": \"function_call\", \"call_id\": \"fc_1\", \"name\": \"search\", \"arguments\": \"{}\"}}\n\n",
            )
            .to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], CodexStreamEvent::FunctionCall { call_id, name, arguments } if call_id == "fc_1" && name == "search" && arguments == "{}")
        );
    }

    #[test]
    fn test_drain_buffer_reasoning_delta() {
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: concat!(
                "event: reasoning.content_text.delta\n",
                "data: {\"delta\": \"Let me think...\"}\n\n",
            )
            .to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], CodexStreamEvent::ReasoningDelta { delta } if delta == "Let me think...")
        );
    }

    #[test]
    fn test_drain_buffer_partial_then_complete() {
        // Simulate data arriving in two chunks
        let mut stream = CodexResponseStream {
            inner: Box::pin(futures::stream::empty()),
            buffer: "event: response.output_text.delta\ndata: {\"delta\": \"hel".to_string(),
            finished: false,
            event_queue: VecDeque::new(),
        };
        let events = stream.drain_buffer();
        assert!(events.is_empty(), "incomplete message should stay buffered");

        stream.buffer.push_str("lo\"}\n\n");
        let events = stream.drain_buffer();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CodexStreamEvent::TextDelta { delta } if delta == "hello"));
    }
}
