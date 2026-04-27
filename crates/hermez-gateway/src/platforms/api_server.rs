//! API Server adapter — OpenAI-compatible HTTP API for Hermez Agent.
//!
//! Mirrors the Python `gateway/platforms/api_server.py`.
//! Hosts an HTTP server with OpenAI Chat Completions endpoints so that
//! any OpenAI-compatible frontend (Open WebUI, LobeChat, ChatBox, etc.)
//! can connect to Hermez Agent.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Json, Sse},
    routing::{delete, get, post},
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot};
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, error, info};

use crate::config::Platform;
use crate::runner::MessageHandler;

/// API Server configuration.
#[derive(Debug, Clone)]
pub struct ApiServerConfig {
    pub port: u16,
    pub host: String,
    pub api_key: String,
    /// Model name advertised in /v1/models and responses.
    /// Mirrors Python `extra.model_name` / `API_SERVER_MODEL_NAME`.
    pub model_name: String,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            port: std::env::var("API_SERVER_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8642),
            host: std::env::var("API_SERVER_HOST")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            api_key: std::env::var("API_SERVER_KEY").unwrap_or_default(),
            model_name: std::env::var("API_SERVER_MODEL_NAME")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "hermez-agent".to_string()),
        }
    }
}

impl ApiServerConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Shared state passed to route handlers via axum State.
#[derive(Clone)]
pub struct ApiServerState {
    pub handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    pub api_key: String,
    /// Model name advertised in responses and /v1/models.
    pub model_name: String,
}

/// OpenAI-style chat completion request.
#[derive(Debug, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Previous response ID for multi-turn session continuity.
    /// Mirrors Python PR 5cbb45d9 — reuses the stored session_id
    /// so the dashboard groups all turns under one session.
    #[serde(default)]
    pub previous_response_id: Option<String>,
}

/// OpenAI-style message.
#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    #[serde(deserialize_with = "deserialize_content")]
    pub content: String,
}

/// OpenAI Responses API request.
/// Mirrors Python PR handling of `input`, `instructions`, `previous_response_id`,
/// `conversation`, `conversation_history`, `store`, `truncation`.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResponsesRequest {
    pub input: serde_json::Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub conversation: Option<String>,
    #[serde(default)]
    pub conversation_history: Option<Vec<HistoryMessageInput>>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub truncation: Option<String>,
}

/// Input message for Responses API (simpler than chat completions Message).
#[derive(Debug, Deserialize, Serialize)]
pub struct HistoryMessageInput {
    pub role: String,
    #[serde(deserialize_with = "deserialize_content")]
    pub content: String,
}

/// Deserialize content that may be a string or an array of content parts.
/// Mirrors Python `_normalize_chat_content` — flattens typed content parts
/// into a plain string so the agent pipeline (which expects strings) works.
/// Enforces depth, size, and length limits to prevent abuse.
fn deserialize_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    // Limits mirroring Python: 64 KB text cap, 1000 items, depth 10.
    const MAX_TEXT_LENGTH: usize = 65_536;
    const MAX_ITEMS: usize = 1_000;
    struct ContentVisitor;
    impl<'de> Visitor<'de> for ContentVisitor {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or an array of content parts")
        }
        fn visit_str<E>(self, v: &str) -> Result<String, E>
        where
            E: de::Error,
        {
            if v.len() > MAX_TEXT_LENGTH {
                Ok(v.chars().take(MAX_TEXT_LENGTH).collect())
            } else {
                Ok(v.to_string())
            }
        }
        fn visit_seq<A>(self, mut seq: A) -> Result<String, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut parts: Vec<String> = Vec::new();
            let mut total_len: usize = 0;
            let limit = std::cmp::min(seq.size_hint().unwrap_or(MAX_ITEMS), MAX_ITEMS);
            for _ in 0..limit {
                if let Some(part) = seq.next_element::<serde_json::Value>()? {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str().map(String::from)) {
                        let capped = if text.len() > MAX_TEXT_LENGTH {
                            text.chars().take(MAX_TEXT_LENGTH).collect()
                        } else {
                            text
                        };
                        total_len += capped.len();
                        parts.push(capped);
                    } else if let Some(text) = part.as_str() {
                        let capped = if text.len() > MAX_TEXT_LENGTH {
                            text.chars().take(MAX_TEXT_LENGTH).collect()
                        } else {
                            text.to_string()
                        };
                        total_len += capped.len();
                        parts.push(capped);
                    }
                    if total_len >= MAX_TEXT_LENGTH {
                        break;
                    }
                }
            }
            Ok(parts.join("\n"))
        }
    }
    deserializer.deserialize_any(ContentVisitor)
}

/// OpenAI-style chat completion response (non-streaming).
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Choice {
    pub index: usize,
    pub message: ResponseMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

/// OpenAI-style error envelope. Mirrors Python `_openai_error`.
#[derive(Debug, Serialize)]
pub struct OpenAIError {
    pub error: OpenAIErrorInner,
}

#[derive(Debug, Serialize)]
pub struct OpenAIErrorInner {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// Helper to construct OpenAI error responses.
fn openai_error(message: &str, code: Option<&str>, error_type: Option<&str>) -> Json<OpenAIError> {
    Json(OpenAIError {
        error: OpenAIErrorInner {
            message: message.to_string(),
            code: code.map(String::from),
            error_type: error_type.map(String::from),
            param: None,
        },
    })
}

/// Result type for API handlers with structured error responses.
type ApiResult<T> = Result<T, (StatusCode, Json<OpenAIError>)>;

fn unauthorized_error() -> (StatusCode, Json<OpenAIError>) {
    (StatusCode::UNAUTHORIZED, openai_error("Invalid API key", Some("invalid_api_key"), Some("invalid_request_error")))
}

fn bad_request_error(message: &str) -> (StatusCode, Json<OpenAIError>) {
    (StatusCode::BAD_REQUEST, openai_error(message, None, Some("invalid_request_error")))
}

fn not_found_error(message: &str) -> (StatusCode, Json<OpenAIError>) {
    (StatusCode::NOT_FOUND, openai_error(message, Some("resource_missing"), Some("not_found")))
}

fn service_unavailable_error() -> (StatusCode, Json<OpenAIError>) {
    (StatusCode::SERVICE_UNAVAILABLE, openai_error("No message handler registered", Some("service_unavailable"), Some("server_error")))
}

fn internal_server_error(message: &str) -> (StatusCode, Json<OpenAIError>) {
    (StatusCode::INTERNAL_SERVER_ERROR, openai_error(message, None, Some("server_error")))
}

/// Build output items from agent result messages.
///
/// Walks `messages` and emits:
/// - `function_call` items for each tool_call on assistant messages
/// - `function_call_output` items for each tool-role message
/// - a final `message` item with the assistant's text reply
///
/// Mirrors Python `_extract_output_items`.
fn extract_output_items(response: &str, messages: &[serde_json::Value]) -> Vec<OutputItem> {
    let mut items = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let func = tc.get("function").and_then(|v| v.as_object());
                    let name = func.and_then(|m| m.get("name").and_then(|v| v.as_str())).unwrap_or("").to_string();
                    let arguments = func.and_then(|m| m.get("arguments").and_then(|v| v.as_str())).unwrap_or("").to_string();
                    let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    items.push(OutputItem {
                        item_type: "function_call".to_string(),
                        role: String::new(),
                        content: None,
                        call_id: Some(call_id),
                        name: Some(name),
                        arguments: Some(arguments),
                        output: None,
                    });
                }
            }
        } else if role == "tool" {
            let call_id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            items.push(OutputItem {
                item_type: "function_call_output".to_string(),
                role: String::new(),
                content: None,
                call_id: Some(call_id),
                name: None,
                arguments: None,
                output: Some(content),
            });
        }
    }

    // Final assistant message
    items.push(OutputItem {
        item_type: "message".to_string(),
        role: "assistant".to_string(),
        content: Some(vec![ContentPart {
            part_type: "output_text".to_string(),
            text: Some(response.to_string()),
        }]),
        call_id: None,
        name: None,
        arguments: None,
        output: None,
    });

    items
}

/// Response store entry — holds full response object for Responses API
/// chaining / GET retrieval. Mirrors Python ResponseStore class.
#[derive(Debug, Clone)]
pub struct ResponseStoreEntry {
    /// Full response data (for GET /v1/responses/{id})
    pub response_data: Option<ResponseData>,
    /// Conversation history with tool calls (for previous_response_id chaining)
    pub conversation_history: Vec<HistoryMessage>,
    /// Ephemeral system instructions (carried forward on chain)
    pub instructions: Option<String>,
    /// Session ID for dashboard grouping
    pub session_id: String,
    /// Conversation name (optional, for name-based chaining)
    pub conversation: Option<String>,
}

/// A message in conversation history.
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// In-memory response store. Mirrors Python's SQLite-backed ResponseStore.
/// Holds full response objects for Responses API stateful chaining.
static RESPONSE_STORE: LazyLock<parking_lot::Mutex<ResponseStore>> =
    LazyLock::new(|| parking_lot::Mutex::new(ResponseStore::default()));

/// Maximum entries before LRU eviction.
const RESPONSE_STORE_MAX: usize = 100;

/// Server start time for uptime tracking.
static START_TIME: LazyLock<std::time::Instant> =
    LazyLock::new(std::time::Instant::now);

/// SQLite-free in-memory response store with LRU eviction.
#[derive(Default)]
struct ResponseStore {
    entries: HashMap<String, ResponseStoreEntry>,
    /// Insertion order for LRU eviction (oldest first).
    order: Vec<String>,
    /// Conversation name -> response_id mapping.
    conversations: HashMap<String, String>,
}

impl ResponseStore {
    fn get(&self, response_id: &str) -> Option<&ResponseStoreEntry> {
        self.entries.get(response_id)
    }

    fn put(&mut self, response_id: String, entry: ResponseStoreEntry) {
        // If conversation name provided, map it
        if let Some(ref conv) = entry.conversation {
            self.conversations.insert(conv.clone(), response_id.clone());
        }
        self.entries.insert(response_id.clone(), entry);
        self.order.push(response_id);
        // LRU eviction — clean up conversation mappings to avoid stale lookups
        while self.entries.len() > RESPONSE_STORE_MAX {
            if let Some(oldest) = self.order.first().cloned() {
                if let Some(entry) = self.entries.remove(&oldest) {
                    if let Some(ref conv) = entry.conversation {
                        if let Some(existing) = self.conversations.get(conv) {
                            if existing == &oldest {
                                self.conversations.remove(conv);
                            }
                        }
                    }
                }
                self.order.remove(0);
            } else {
                break;
            }
        }
    }

    fn delete(&mut self, response_id: &str) -> bool {
        if let Some(entry) = self.entries.remove(response_id) {
            self.order.retain(|id| id != response_id);
            // Clean up conversation mapping to avoid stale lookups
            if let Some(ref conv) = entry.conversation {
                if let Some(existing) = self.conversations.get(conv) {
                    if existing == response_id {
                        self.conversations.remove(conv);
                    }
                }
            }
            true
        } else {
            false
        }
    }

    fn get_conversation(&self, name: &str) -> Option<&String> {
        self.conversations.get(name)
    }

    fn set_conversation(&mut self, name: String, response_id: String) {
        self.conversations.insert(name, response_id);
    }
}

// ── Idempotency Cache ───────────────────────────────────────────

/// Maximum entries before LRU eviction.
const IDEM_MAX_ITEMS: usize = 1000;
/// TTL in seconds for idempotency cache entries.
const IDEM_TTL_SECS: u64 = 300;

/// In-memory idempotency cache with TTL and LRU eviction.
/// Mirrors Python `_IdempotencyCache`.
#[derive(Default)]
struct IdempotencyCache {
    store: HashMap<String, IdempotencyEntry>,
    /// Insertion order for LRU eviction (oldest first).
    order: std::collections::VecDeque<String>,
}

struct IdempotencyEntry {
    response: serde_json::Value,
    fingerprint: String,
    timestamp: std::time::Instant,
}

impl IdempotencyCache {
    fn purge(&mut self) {
        let now = std::time::Instant::now();
        let ttl = Duration::from_secs(IDEM_TTL_SECS);
        // Remove expired
        self.store.retain(|_, v| now.duration_since(v.timestamp) <= ttl);
        // Remove expired from order
        while let Some(front) = self.order.front() {
            if self.store.contains_key(front) {
                break;
            }
            self.order.pop_front();
        }
        // LRU eviction — keep only max items
        while self.store.len() > IDEM_MAX_ITEMS {
            if let Some(k) = self.order.pop_front() {
                self.store.remove(&k);
            } else {
                break;
            }
        }
    }

    fn get(&mut self, key: &str, fingerprint: &str) -> Option<serde_json::Value> {
        self.purge();
        self.store.get(key).and_then(|entry| {
            if entry.fingerprint == fingerprint {
                Some(entry.response.clone())
            } else {
                None
            }
        })
    }

    fn set(&mut self, key: String, fingerprint: String, response: serde_json::Value) {
        // Remove expired entries first
        let now = std::time::Instant::now();
        let ttl = Duration::from_secs(IDEM_TTL_SECS);
        self.store.retain(|_, v| now.duration_since(v.timestamp) <= ttl);
        // Clean up order
        while let Some(front) = self.order.front() {
            if self.store.contains_key(front) {
                break;
            }
            self.order.pop_front();
        }

        self.order.push_back(key.clone());
        self.store.insert(key, IdempotencyEntry {
            response,
            fingerprint,
            timestamp: std::time::Instant::now(),
        });

        // LRU eviction — keep only max items
        while self.store.len() > IDEM_MAX_ITEMS {
            if let Some(k) = self.order.pop_front() {
                self.store.remove(&k);
            } else {
                break;
            }
        }
    }
}

static IDEMPOTENCY_CACHE: LazyLock<parking_lot::Mutex<IdempotencyCache>> =
    LazyLock::new(|| parking_lot::Mutex::new(IdempotencyCache::default()));

/// Compute a SHA-256 fingerprint of selected request fields.
fn make_request_fingerprint(data: &serde_json::Value, keys: &[&str]) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    for &key in keys {
        if let Some(v) = data.get(key) {
            hasher.update(key.as_bytes());
            hasher.update(serde_json::to_string(v).unwrap_or_default().as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

// ── Run Streams ─────────────────────────────────────────────────

/// Maximum concurrent runs.
const MAX_CONCURRENT_RUNS: usize = 20;

/// A single run event (structured lifecycle event for SSE streaming).
#[derive(Debug, Clone, Serialize)]
pub struct RunEvent {
    pub event: String,
    pub run_id: String,
    pub timestamp: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<RunUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
}

/// Run start response (202 Accepted).
#[derive(Debug, Clone, Serialize)]
pub struct RunStartResponse {
    pub run_id: String,
    pub status: String,
}

/// In-memory store for run event streams.
/// Each run has a tokio broadcast channel for SSE events.
static RUN_STREAMS: LazyLock<Arc<Mutex<RunStreamStore>>> =
    LazyLock::new(|| Arc::new(Mutex::new(RunStreamStore::default())));

#[derive(Default)]
struct RunStreamStore {
    /// run_id -> (event sender, created_at timestamp)
    senders: HashMap<String, (tokio::sync::broadcast::Sender<RunEvent>, f64)>,
}

impl RunStreamStore {
    fn insert(&mut self, run_id: String, tx: tokio::sync::broadcast::Sender<RunEvent>, created_at: f64) {
        self.senders.insert(run_id, (tx, created_at));
    }

    fn get_sender(&self, run_id: &str) -> Option<tokio::sync::broadcast::Sender<RunEvent>> {
        self.senders.get(run_id).map(|(tx, _)| tx.clone())
    }

    fn remove(&mut self, run_id: &str) {
        self.senders.remove(run_id);
    }

    fn len(&self) -> usize {
        self.senders.len()
    }
}

/// OpenAI Responses API response data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseData {
    pub id: String,
    pub object: String,
    pub status: String,
    pub created_at: i64,
    pub model: String,
    pub output: Vec<OutputItem>,
    pub usage: ResponseUsage,
}

/// SSE event data for streaming Responses API.
/// Each variant corresponds to an OpenAI Responses SSE event type.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResponsesSseEvent {
    /// `response.created` — initial envelope
    ResponseCreated {
        #[serde(rename = "type")]
        event_type: String,
        response: SseResponseEnvelope,
        sequence_number: usize,
    },
    /// `response.output_text.delta` — text chunk
    OutputTextDelta {
        #[serde(rename = "type")]
        event_type: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        delta: String,
        logprobs: Vec<()>,
        sequence_number: usize,
    },
    /// `response.output_text.done` — text complete
    OutputTextDone {
        #[serde(rename = "type")]
        event_type: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        text: String,
        logprobs: Vec<()>,
        sequence_number: usize,
    },
    /// `response.output_item.added` — new output item
    OutputItemAdded {
        #[serde(rename = "type")]
        event_type: String,
        output_index: usize,
        item: SseOutputItem,
        sequence_number: usize,
    },
    /// `response.output_item.done` — output item complete
    OutputItemDone {
        #[serde(rename = "type")]
        event_type: String,
        output_index: usize,
        item: SseOutputItem,
        sequence_number: usize,
    },
    /// `response.completed` — terminal event
    ResponseCompleted {
        #[serde(rename = "type")]
        event_type: String,
        response: SseResponseEnvelope,
        sequence_number: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct SseResponseEnvelope {
    pub id: String,
    pub object: String,
    pub status: String,
    pub created_at: i64,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<SseOutputItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SseOutputItem {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentPart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Plain string output for function_call_output items (matches Python).
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentPart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// Plain string output for function_call_output items (matches Python).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
}

/// OpenAI-style model list response.
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
}

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
}

/// Detailed health check response — returns gateway state, platforms,
/// PID, and uptime for cross-container dashboard probing.
/// Mirrors Python _handle_health_detailed.
#[derive(Debug, Serialize)]
pub struct DetailedHealthResponse {
    pub status: String,
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_state: Option<String>,
    pub platforms: Vec<String>,
    pub active_agents: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub pid: u32,
    pub uptime_seconds: u64,
}

/// API Server adapter — holds config, builds the HTTP router.
pub struct ApiServerAdapter {
    pub config: ApiServerConfig,
}

impl ApiServerAdapter {
    pub fn new(config: ApiServerConfig) -> Self {
        Self { config }
    }

    /// Build the axum router with all API endpoints.
    pub fn build_router(&self, state: ApiServerState) -> Router {
        let cors = CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods([axum::http::Method::GET, axum::http::Method::POST, axum::http::Method::DELETE, axum::http::Method::OPTIONS])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderName::from_static("idempotency-key"),
            ]);

        // Security headers middleware — mirrors Python _SECURITY_HEADERS
        let security_headers = tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
        let referrer_policy = tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            axum::http::header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("no-referrer"),
        );

        Router::new()
            .route("/health", get(health_handler))
            .route("/health/detailed", get(health_detailed_handler))
            .route("/v1/health", get(health_handler))
            .route("/v1/models", get(models_handler))
            .route("/v1/chat/completions", post(chat_completions_handler))
            .route("/v1/responses", post(responses_handler))
            .route("/v1/responses/{response_id}", get(get_response_handler))
            .route("/v1/responses/{response_id}", delete(delete_response_handler))
            .route("/v1/runs", post(runs_handler))
            .route("/v1/runs/{run_id}/events", get(run_events_handler))
            .layer(cors)
            .layer(RequestBodyLimitLayer::new(1024 * 1024)) // 1MB max request body
            .layer(security_headers)
            .layer(referrer_policy)
            .with_state(state)
    }

    /// Run the HTTP server with graceful shutdown.
    /// Returns a oneshot sender that should be triggered to stop the server.
    pub async fn run(
        &self,
        state: ApiServerState,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let app = self.build_router(state);
        let listener = match tokio::net::TcpListener::bind(format!("{}:{}", self.config.host, self.config.port)).await {
            Ok(l) => l,
            Err(e) => return Err(format!("Failed to bind API server: {e}")),
        };
        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("API Server listening on http://{addr}");

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
        {
            error!("API server error: {e}");
            return Err(format!("API server error: {e}"));
        }
        info!("API Server stopped gracefully");
        Ok(())
    }
}

// ── Route Handlers ──────────────────────────────────────────────

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

/// GET /health/detailed — rich status for cross-container dashboard probing.
/// Mirrors Python _handle_health_detailed.
async fn health_detailed_handler(
    State(state): State<ApiServerState>,
) -> Json<DetailedHealthResponse> {
    let uptime = START_TIME.elapsed().as_secs();
    let pid = std::process::id();

    // Count stored responses as active agents proxy
    let active_agents = RESPONSE_STORE.lock().entries.len();

    Json(DetailedHealthResponse {
        status: "ok".to_string(),
        platform: state.model_name.clone(),
        gateway_state: Some("running".to_string()),
        platforms: vec!["api_server".to_string()],
        active_agents,
        exit_reason: None,
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
        pid,
        uptime_seconds: uptime,
    })
}

async fn models_handler(
    State(state): State<ApiServerState>,
) -> Json<ModelsResponse> {
    Json(ModelsResponse {
        object: "list".to_string(),
        data: vec![ModelInfo {
            id: state.model_name.clone(),
            object: "model".to_string(),
            created: chrono::Utc::now().timestamp(),
            owned_by: "nous-research".to_string(),
        }],
    })
}

/// SSE chunk response for streaming mode.
/// Mirrors OpenAI's `chat.completion.chunk` format.
#[derive(Debug, Serialize)]
pub struct StreamChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
}

#[derive(Debug, Serialize)]
pub struct StreamChoice {
    pub index: usize,
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StreamDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

async fn chat_completions_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> ApiResult<SseOrJson> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    // Idempotency: extract key + fingerprint early, reuse for both get and set
    let idem_key_and_fp = if !request.stream {
        headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()).map(|key| {
            let request_json = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
            let fp = make_request_fingerprint(&request_json, &["model", "messages", "tools", "tool_choice", "stream"]);
            (key.to_string(), fp)
        })
    } else {
        None
    };

    // Cache hit — return cached response without running agent
    if let Some((ref key, ref fp)) = idem_key_and_fp {
        let mut cache = IDEMPOTENCY_CACHE.lock();
        if let Some(cached) = cache.get(key, fp) {
            debug!("Idempotency cache hit for chat completions, key={key}");
            if let Ok(resp) = serde_json::from_value::<ChatCompletionResponse>(cached) {
                let session_id = resp.id.clone();
                return Ok(SseOrJson::Json(Json(resp), session_id));
            }
        }
    }

    // Extract the last user message
    let user_message = request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if user_message.is_empty() {
        return Err(bad_request_error("No user message found"));
    }

    // Determine session ID — chain from previous_response_id if available.
    // Priority: explicit session_id > stored session_id from previous response > fresh UUID.
    let stored_session_id = if let Some(ref prev_id) = request.previous_response_id {
        RESPONSE_STORE.lock().get(prev_id).map(|e| e.session_id.clone())
    } else {
        None
    };

    let session_id = request
        .session_id
        .clone()
        .or(stored_session_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Call the agent handler
    let handler_guard = state.handler.lock().await;
    let Some(handler) = handler_guard.as_ref() else {
        return Err(service_unavailable_error());
    };

    let result = handler
        .handle_message(
            Platform::ApiServer,
            &session_id,
            &user_message,
            None,
        )
        .await
        .map_err(|e| {
            error!("Agent handler failed for API request: {e}");
            internal_server_error(&format!("Agent handler error: {e}"))
        })?;

    let response = result.response;

    // Extract token usage from handler result
    let usage = result.usage.map(|u| Usage {
        prompt_tokens: u.prompt_tokens as usize,
        completion_tokens: u.completion_tokens as usize,
        total_tokens: u.total_tokens as usize,
    }).unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    let model = request.model.clone().unwrap_or_else(|| state.model_name.clone());
    let chat_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let created = chrono::Utc::now().timestamp();

    // Store session_id so subsequent requests with previous_response_id
    // can reuse the same session (multi-turn continuity).
    {
        let mut store = RESPONSE_STORE.lock();
        store.put(chat_id.clone(), ResponseStoreEntry {
            response_data: None,
            conversation_history: vec![],
            instructions: None,
            session_id: session_id.clone(),
            conversation: None,
        });
    }

    if request.stream {
        // Streaming mode: emit SSE events
        Ok(SseOrJson::Sse(build_sse_stream(chat_id, model, created, response), session_id))
    } else {
        // Non-streaming mode
        let resp = ChatCompletionResponse {
            id: chat_id,
            object: "chat.completion".to_string(),
            created,
            model,
            choices: vec![Choice {
                index: 0,
                message: ResponseMessage {
                    role: "assistant".to_string(),
                    content: response,
                },
                finish_reason: "stop".to_string(),
            }],
            usage,
        };

        // Cache response for idempotency
        if let Some((ref key, ref fp)) = idem_key_and_fp {
            if let Ok(json) = serde_json::to_value(&resp) {
                let mut cache = IDEMPOTENCY_CACHE.lock();
                cache.set(key.clone(), fp.clone(), json);
            }
        }

        Ok(SseOrJson::Json(Json(resp), session_id))
    }
}

// ── Responses API Handlers ──────────────────────────────────────

/// Union type for responses: batch JSON or streaming SSE.
enum ResponsesResponse {
    Json(Json<ResponseData>),
    Sse(Sse<SseResponsesStreamType>),
}

#[async_trait::async_trait]
impl axum::response::IntoResponse for ResponsesResponse {
    fn into_response(self) -> axum::response::Response {
        match self {
            ResponsesResponse::Json(json) => json.into_response(),
            ResponsesResponse::Sse(sse) => sse.into_response(),
        }
    }
}

/// POST /v1/responses — OpenAI Responses API format.
/// Stateful via previous_response_id; supports conversation naming,
/// explicit conversation_history, store flag, truncation, and streaming.
/// Mirrors Python _handle_responses (lines 1393–1645).
async fn responses_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> ApiResult<ResponsesResponse> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    // Normalize input to message list
    let input_messages = normalize_responses_input(&request.input)
        .map_err(|e| bad_request_error(&e))?;

    // conversation and previous_response_id are mutually exclusive
    if request.conversation.is_some() && request.previous_response_id.is_some() {
        return Err(bad_request_error("Cannot use both 'conversation' and 'previous_response_id'"));
    }

    // Resolve conversation name to latest response_id
    let mut prev_id = request.previous_response_id.clone();
    if let Some(ref conv) = request.conversation {
        let store = RESPONSE_STORE.lock();
        if let Some(resp_id) = store.get_conversation(conv) {
            prev_id = Some(resp_id.clone());
        }
        // No error if conversation doesn't exist — it's a new conversation
    }

    // Accept explicit conversation_history from request body.
    // Precedence: conversation_history > previous_response_id.
    let mut conversation_history: Vec<HistoryMessage> = Vec::new();
    let mut stored_session_id: Option<String> = None;
    let mut stored_instructions: Option<String> = None;

    if let Some(ref raw_history) = request.conversation_history {
        for entry in raw_history {
            conversation_history.push(HistoryMessage {
                role: entry.role.clone(),
                content: entry.content.clone(),
            });
        }
    } else if let Some(ref prev_resp_id) = prev_id {
        let store = RESPONSE_STORE.lock();
        if let Some(stored) = store.get(prev_resp_id) {
            conversation_history = stored.conversation_history.clone();
            stored_session_id = Some(stored.session_id.clone());
            stored_instructions = stored.instructions.clone();
        } else {
            return Err(not_found_error(&format!("Previous response not found: {}", prev_resp_id)));
        }
    }

    // Carry forward instructions if not provided
    let instructions = request.instructions.clone().or(stored_instructions);

    // Append all but last input message to history
    let all_but_last: Vec<_> = input_messages.iter().take(input_messages.len().saturating_sub(1)).cloned().collect();
    for msg in all_but_last {
        conversation_history.push(msg);
    }

    // Last input message is the user_message
    let user_message = input_messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if user_message.is_empty() {
        return Err(bad_request_error("No user message found in input"));
    }

    // Truncation support: auto-truncate to last 100 messages
    if request.truncation.as_deref() == Some("auto") && conversation_history.len() > 100 {
        conversation_history = conversation_history.split_off(conversation_history.len() - 100);
    }

    // Reuse session from previous_response_id chain
    let session_id = stored_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let store_response = request.store.unwrap_or(true);

    // Idempotency: extract key + fingerprint early, reuse for both get and set
    let idem_key_and_fp = if !request.stream.unwrap_or(false) {
        headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()).map(|key| {
            let request_json = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
            let fp = make_request_fingerprint(&request_json, &["input", "instructions", "previous_response_id", "conversation", "model", "tools"]);
            (key.to_string(), fp)
        })
    } else {
        None
    };

    // Cache hit — return cached response without running agent
    if let Some((ref key, ref fp)) = idem_key_and_fp {
        let mut cache = IDEMPOTENCY_CACHE.lock();
        if let Some(cached) = cache.get(key, fp) {
            debug!("Idempotency cache hit for responses, key={key}");
            if let Ok(resp) = serde_json::from_value::<ResponseData>(cached) {
                return Ok(ResponsesResponse::Json(Json(resp)));
            }
        }
    }

    // Call the agent handler
    let handler_guard = state.handler.lock().await;
    let Some(handler) = handler_guard.as_ref() else {
        return Err(service_unavailable_error());
    };

    let result = handler
        .handle_message(
            Platform::ApiServer,
            &session_id,
            &user_message,
            None,
        )
        .await
        .map_err(|e| {
            error!("Agent handler failed for responses request: {e}");
            internal_server_error(&format!("Agent handler error: {e}"))
        })?;

    let response_text = result.response.clone();
    let model = request.model.clone().unwrap_or_else(|| state.model_name.clone());
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple().to_string().chars().take(28).collect::<String>());
    let created_at = chrono::Utc::now().timestamp();

    // Build output items from agent messages (tool_calls + tool outputs + final message)
    let output_items = extract_output_items(&response_text, &result.messages);

    let response_data = ResponseData {
        id: response_id.clone(),
        object: "response".to_string(),
        status: "completed".to_string(),
        created_at,
        model,
        output: output_items.clone(),
        usage: ResponseUsage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
        },
    };

    // Store for future chaining if requested
    if store_response {
        let mut store = RESPONSE_STORE.lock();
        // Build full history: existing history + user message + assistant response
        let mut full_history = conversation_history;
        full_history.push(HistoryMessage {
            role: "user".to_string(),
            content: user_message.clone(),
        });
        full_history.push(HistoryMessage {
            role: "assistant".to_string(),
            content: response_text,
        });

        store.put(response_id.clone(), ResponseStoreEntry {
            response_data: Some(response_data.clone()),
            conversation_history: full_history,
            instructions,
            session_id,
            conversation: request.conversation.clone(),
        });

        // Update conversation mapping
        if let Some(ref conv) = request.conversation {
            store.set_conversation(conv.clone(), response_id);
        }
    }

    // Cache response for idempotency (non-streaming only)
    if let Some((ref key, ref fp)) = idem_key_and_fp {
        if let Ok(json) = serde_json::to_value(&response_data) {
            let mut cache = IDEMPOTENCY_CACHE.lock();
            cache.set(key.clone(), fp.clone(), json);
        }
    }

    // Check for streaming mode
    if request.stream.unwrap_or(false) {
        let stream_result = responses_stream_handler(
            state.clone(), &state.api_key, request,
        ).await?;
        return Ok(ResponsesResponse::Sse(stream_result));
    }

    Ok(ResponsesResponse::Json(Json(response_data)))
}

/// POST /v1/responses — streaming variant.
/// When `stream: true`, emits spec-compliant SSE events with proper
/// event types (response.created, output_text.delta, output_item.added/done,
/// response.completed). Tool call events require agent engine streaming
/// callbacks — currently deferred (TODO: wire through agent stream_delta_callback).
async fn responses_stream_handler(
    state: ApiServerState,
    _api_key: &str,
    request: ResponsesRequest,
) -> ApiResult<Sse<SseResponsesStreamType>> {
    // Reuse the same setup logic as batch mode
    let input_messages = normalize_responses_input(&request.input)
        .map_err(|e| bad_request_error(&e))?;

    if request.conversation.is_some() && request.previous_response_id.is_some() {
        return Err(bad_request_error("Cannot use both 'conversation' and 'previous_response_id'"));
    }

    let mut prev_id = request.previous_response_id.clone();
    if let Some(ref conv) = request.conversation {
        let store = RESPONSE_STORE.lock();
        if let Some(resp_id) = store.get_conversation(conv) {
            prev_id = Some(resp_id.clone());
        }
    }

    let mut conversation_history: Vec<HistoryMessage> = Vec::new();
    let mut stored_session_id: Option<String> = None;
    let mut stored_instructions: Option<String> = None;

    if let Some(ref raw_history) = request.conversation_history {
        for entry in raw_history {
            conversation_history.push(HistoryMessage {
                role: entry.role.clone(),
                content: entry.content.clone(),
            });
        }
    } else if let Some(ref prev_resp_id) = prev_id {
        let store = RESPONSE_STORE.lock();
        if let Some(stored) = store.get(prev_resp_id) {
            conversation_history = stored.conversation_history.clone();
            stored_session_id = Some(stored.session_id.clone());
            stored_instructions = stored.instructions.clone();
        } else {
            return Err(not_found_error(&format!("Previous response not found: {}", prev_resp_id)));
        }
    }

    let instructions = request.instructions.or(stored_instructions);

    let all_but_last: Vec<_> = input_messages.iter().take(input_messages.len().saturating_sub(1)).cloned().collect();
    for msg in all_but_last {
        conversation_history.push(msg);
    }

    let user_message = input_messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if user_message.is_empty() {
        return Err(bad_request_error("No user message found in input"));
    }

    if request.truncation.as_deref() == Some("auto") && conversation_history.len() > 100 {
        conversation_history = conversation_history.split_off(conversation_history.len() - 100);
    }

    let session_id = stored_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let store_response = request.store.unwrap_or(true);
    let model = request.model.clone().unwrap_or_else(|| state.model_name.clone());
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple().to_string().chars().take(28).collect::<String>());
    let created_at = chrono::Utc::now().timestamp();

    // Shared buffer + notify for real-time text deltas during LLM generation.
    // Notify wakes the SSE builder only when new data arrives (no polling).
    let streamed_deltas: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let delta_notify: std::sync::Arc<tokio::sync::Notify> =
        std::sync::Arc::new(tokio::sync::Notify::new());

    // Spawn agent run in background task with streaming support.
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<crate::runner::HandlerResult, String>>();
    let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let handler = state.handler.clone();
    let sess_id = session_id.clone();
    let user_msg_spawn = user_message.clone();
    tokio::spawn(async move {
        let handler_guard = handler.lock().await;
        let result = match handler_guard.as_ref() {
            Some(h) => h.handle_message_streaming(
                Platform::ApiServer, &sess_id, &user_msg_spawn, None, stream_tx
            ).await,
            None => Err("No message handler registered".to_string()),
        };
        let _ = tx.send(result);
    });

    // Drain stream deltas into the shared buffer for SSE consumption.
    let deltas_for_drain = streamed_deltas.clone();
    let notify_for_drain = delta_notify.clone();
    tokio::spawn(async move {
        let mut rx = stream_rx;
        while let Some(delta) = rx.recv().await {
            if !delta.is_empty() {
                deltas_for_drain.lock().unwrap().push(delta);
                notify_for_drain.notify_one();
            }
        }
    });

    // Build SSE stream with real-time delta support from shared buffer + notify.
    let stream = build_responses_sse_stream(
        rx, Some((streamed_deltas, delta_notify)), response_id, model, created_at, session_id,
        conversation_history, user_message, instructions,
        store_response, request.conversation.clone(),
    );

    Ok(Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text(": keepalive"),
        ))
}

/// Stream type for responses SSE.
type SseResponsesStreamType = Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send>>;

/// Build an SSE stream for Responses API streaming.
/// Emits: response.created → output_item.added (message) → output_text.delta (chunked)
/// → output_text.done → output_item.done (message) → response.completed
fn build_responses_sse_stream(
    mut agent_rx: tokio::sync::oneshot::Receiver<Result<crate::runner::HandlerResult, String>>,
    streamed_deltas: Option<(std::sync::Arc<std::sync::Mutex<Vec<String>>>, std::sync::Arc<tokio::sync::Notify>)>,
    response_id: String,
    model: String,
    created_at: i64,
    session_id: String,
    conversation_history: Vec<HistoryMessage>,
    user_message: String,
    instructions: Option<String>,
    store_response: bool,
    conversation: Option<String>,
) -> SseResponsesStreamType {
    Box::pin(async_stream::stream! {
        let mut seq: usize = 0;

        // Helper: emit an SSE event
        fn make_event(data: impl serde::Serialize) -> axum::response::sse::Event {
            axum::response::sse::Event::default()
                .json_data(&data)
                .unwrap_or_else(|e| axum::response::sse::Event::default().data(format!("{{\"error\":\"sse serialization: {e}\"}}")))
        }

        // response.created
        let envelope = SseResponseEnvelope {
            id: response_id.clone(),
            object: "response".to_string(),
            status: "in_progress".to_string(),
            created_at,
            model: model.clone(),
            output: Some(vec![]),
            usage: None,
        };
        yield Ok(make_event(ResponsesSseEvent::ResponseCreated {
            event_type: "response.created".to_string(),
            response: envelope,
            sequence_number: seq,
        }));
        seq += 1;

        // output_item.added (message)
        let message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple().to_string().chars().take(24).collect::<String>());
        let msg_item = SseOutputItem {
            id: message_item_id.clone(),
            item_type: "message".to_string(),
            status: "in_progress".to_string(),
            role: Some("assistant".to_string()),
            content: Some(vec![]),
            name: None,
            call_id: None,
            arguments: None,
            output: None,
        };
        let msg_output_index: usize = 0;
        yield Ok(make_event(ResponsesSseEvent::OutputItemAdded {
            event_type: "response.output_item.added".to_string(),
            output_index: msg_output_index,
            item: msg_item,
            sequence_number: seq,
        }));
        seq += 1;

        // Emit real-time text deltas while agent runs.
        // Event-driven via notify (no polling): drainer task wakes us when data arrives.
        let mut streamed_text = String::new();
        let mut emitted_count = 0usize;
        let agent_result;
        let (delta_buf, delta_notify) = match streamed_deltas {
            Some(ref pair) => (Some(pair.0.clone()), Some(pair.1.clone())),
            None => (None, None),
        };
        loop {
            if let Some(ref notify) = delta_notify {
                tokio::select! {
                    res = &mut agent_rx => {
                        agent_result = res;
                        break;
                    }
                    // Wake on new data OR every 1s (catches missed notifies from race)
                    _ = tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified()) => {
                        if let Some(ref deltas) = delta_buf {
                            let pending: Vec<String> = {
                                let guard = deltas.lock().unwrap();
                                guard[emitted_count..].to_vec()
                            };
                            for delta in pending {
                                emitted_count += 1;
                                if !delta.is_empty() {
                                    streamed_text.push_str(&delta);
                                    let ci: usize = 0;
                                    yield Ok(make_event(ResponsesSseEvent::OutputTextDelta {
                                        event_type: "response.output_text.delta".to_string(),
                                        item_id: message_item_id.clone(),
                                        output_index: msg_output_index,
                                        content_index: ci,
                                        delta,
                                        logprobs: vec![],
                                        sequence_number: seq,
                                    }));
                                    seq += 1;
                                }
                            }
                        }
                    }
                }
            } else {
                // No streaming — just wait for agent
                agent_result = agent_rx.await;
                break;
            }
        }
        let (response_text, result_messages) = match agent_result {
            Ok(Ok(result)) => (result.response.clone(), result.messages.clone()),
            Ok(Err(_e)) => {
                let error_envelope = SseResponseEnvelope {
                    id: response_id.clone(),
                    object: "response".to_string(),
                    status: "failed".to_string(),
                    created_at,
                    model: model.clone(),
                    output: None,
                    usage: None,
                };
                yield Ok(make_event(ResponsesSseEvent::ResponseCompleted {
                    event_type: "response.failed".to_string(),
                    response: error_envelope,
                    sequence_number: seq,
                }));
                return;
            }
            Err(_) => {
                return;
            }
        };

        // Emit remaining text if no streaming deltas were received.
        // When streamed_text already has content, deltas were emitted in real-time.
        if streamed_text.is_empty() {
            let mut chars = response_text.chars().peekable();
            while chars.peek().is_some() {
                let text: String = chars.by_ref().take(3).collect();
                if text.is_empty() { break; }
                yield Ok(make_event(ResponsesSseEvent::OutputTextDelta {
                    event_type: "response.output_text.delta".to_string(),
                    item_id: message_item_id.clone(),
                    output_index: msg_output_index,
                    content_index: 0,
                    delta: text,
                    logprobs: vec![],
                    sequence_number: seq,
                }));
                seq += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        // output_text.done
        yield Ok(make_event(ResponsesSseEvent::OutputTextDone {
            event_type: "response.output_text.done".to_string(),
            item_id: message_item_id.clone(),
            output_index: msg_output_index,
            content_index: 0,
            text: response_text.clone(),
            logprobs: vec![],
            sequence_number: seq,
        }));
        seq += 1;

        // output_item.done (message)
        let msg_done = SseOutputItem {
            id: message_item_id.clone(),
            item_type: "message".to_string(),
            status: "completed".to_string(),
            role: Some("assistant".to_string()),
            content: Some(vec![ContentPart {
                part_type: "output_text".to_string(),
                text: Some(response_text.clone()),
            }]),
            name: None,
            call_id: None,
            arguments: None,
            output: None,
        };
        yield Ok(make_event(ResponsesSseEvent::OutputItemDone {
            event_type: "response.output_item.done".to_string(),
            output_index: msg_output_index,
            item: msg_done,
            sequence_number: seq,
        }));
        seq += 1;

        // response.completed
        let completed_envelope = SseResponseEnvelope {
            id: response_id.clone(),
            object: "response".to_string(),
            status: "completed".to_string(),
            created_at,
            model: model.clone(),
            output: Some(vec![SseOutputItem {
                id: message_item_id,
                item_type: "message".to_string(),
                status: "completed".to_string(),
                role: Some("assistant".to_string()),
                content: Some(vec![ContentPart {
                    part_type: "output_text".to_string(),
                    text: Some(response_text.clone()),
                }]),
                name: None,
                call_id: None,
                arguments: None,
                output: None,
            }]),
            usage: Some(ResponseUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
            }),
        };
        yield Ok(make_event(ResponsesSseEvent::ResponseCompleted {
            event_type: "response.completed".to_string(),
            response: completed_envelope,
            sequence_number: seq,
        }));

        // Store for future chaining (same as batch mode)
        if store_response {
            let mut store = RESPONSE_STORE.lock();
            let output_items = extract_output_items(&response_text, &result_messages);

            let mut full_history = conversation_history;
            full_history.push(HistoryMessage {
                role: "user".to_string(),
                content: user_message,
            });
            full_history.push(HistoryMessage {
                role: "assistant".to_string(),
                content: response_text,
            });

            let final_response_data = ResponseData {
                id: response_id.clone(),
                object: "response".to_string(),
                status: "completed".to_string(),
                created_at,
                model,
                output: output_items,
                usage: ResponseUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                },
            };

            let conv_name = conversation.clone();
            store.put(response_id.clone(), ResponseStoreEntry {
                response_data: Some(final_response_data),
                conversation_history: full_history,
                instructions,
                session_id,
                conversation,
            });

            if let Some(conv) = conv_name {
                store.set_conversation(conv, response_id);
            }
        }
    })
}

/// GET /v1/responses/{response_id} — retrieve a stored response.
async fn get_response_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> ApiResult<Json<ResponseData>> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    let store = RESPONSE_STORE.lock();
    let Some(entry) = store.get(&response_id) else {
        return Err(not_found_error(&format!("Response not found: {}", response_id)));
    };

    let Some(ref data) = entry.response_data else {
        return Err(not_found_error(&format!("Response data not available for: {}", response_id)));
    };

    Ok(Json(data.clone()))
}

/// DELETE /v1/responses/{response_id} — delete a stored response.
async fn delete_response_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> ApiResult<Json<DeleteResponseResult>> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    let deleted = RESPONSE_STORE.lock().delete(&response_id);
    if !deleted {
        return Err(not_found_error(&format!("Response not found: {}", response_id)));
    }

    Ok(Json(DeleteResponseResult {
        id: response_id,
        object: "response".to_string(),
        deleted: true,
    }))
}

#[derive(Debug, Serialize)]
pub struct DeleteResponseResult {
    pub id: String,
    pub object: String,
    pub deleted: bool,
}

// ── Runs API Handlers ──────────────────────────────────────────

/// POST /v1/runs — start an agent run, return run_id immediately (202).
/// The actual run runs in a background tokio task. Events are streamed
/// via GET /v1/runs/{run_id}/events.
/// Mirrors Python _handle_runs.
async fn runs_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> ApiResult<(StatusCode, Json<RunStartResponse>)> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    // Enforce concurrency limit
    {
        let store = RUN_STREAMS.lock().await;
        if store.len() >= MAX_CONCURRENT_RUNS {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                openai_error(&format!("Too many concurrent runs (max {})", MAX_CONCURRENT_RUNS), Some("rate_limit_exceeded"), Some("server_error")),
            ));
        }
    }

    let raw_input = request.get("input").cloned().unwrap_or(serde_json::Value::Null);
    if raw_input.is_null() {
        return Err(bad_request_error("Missing 'input' field"));
    }

    // Normalize input to extract user_message and conversation_history
    let (user_message, conversation_history, instructions, session_id, previous_response_id) = {
        let user_message = if let Some(s) = raw_input.as_str() {
            s.to_string()
        } else if let Some(arr) = raw_input.as_array() {
            arr.last()
                .and_then(|m| m.get("content")).map(|c| extract_content_from_value(Some(c)))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let conversation_history: Vec<HistoryMessage> = Vec::new();
        let instructions = request.get("instructions").and_then(|v| v.as_str()).map(String::from);
        let session_id = request.get("session_id").and_then(|v| v.as_str()).map(String::from);
        let previous_response_id = request.get("previous_response_id").and_then(|v| v.as_str()).map(String::from);

        (user_message, conversation_history, instructions, session_id, previous_response_id)
    };

    if user_message.is_empty() {
        return Err(bad_request_error("No user message found in input"));
    }

    let run_id = format!("run_{}", uuid::Uuid::new_v4().simple());

    // Create broadcast channel for run events
    let (tx, _rx) = tokio::sync::broadcast::channel::<RunEvent>(256);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    {
        let mut store = RUN_STREAMS.lock().await;
        store.insert(run_id.clone(), tx.clone(), now);
    }

    // Spawn the run in a background task
    let state_clone = state.clone();
    let run_id_spawn = run_id.clone();
    let final_session_id = session_id.unwrap_or_else(|| run_id.clone());
    tokio::spawn(async move {
        run_and_close(
            state_clone,
            run_id_spawn,
            tx.clone(),
            user_message,
            conversation_history,
            instructions,
            final_session_id,
            previous_response_id,
        ).await;
    });

    Ok((StatusCode::ACCEPTED, Json(RunStartResponse {
        run_id,
        status: "started".to_string(),
    })))
}

/// Background task: run the agent, emit events, then close the channel.
async fn run_and_close(
    state: ApiServerState,
    run_id: String,
    tx: tokio::sync::broadcast::Sender<RunEvent>,
    user_message: String,
    conversation_history: Vec<HistoryMessage>,
    _instructions: Option<String>,
    session_id: String,
    previous_response_id: Option<String>,
) {
    use std::time::SystemTime;

    let mut sequence_number: usize = 0;

    // Emit run.started
    let started_event = RunEvent {
        event: "run.started".to_string(),
        run_id: run_id.clone(),
        timestamp: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64(),
        delta: None,
        output: None,
        error: None,
        usage: None,
        sequence_number: Some(sequence_number),
    };
    let _ = tx.send(started_event);
    sequence_number += 1;

    // Resolve previous_response_id for conversation chaining.
    // NOTE: Wiring resolved history into the handler pipeline requires
    // extending `MessageHandler::handle_message` to accept an optional
    // session resume / conversation_history parameter. The resolved
    // session_id is computed below but not yet plumbed through.
    let _previous_resolved = if conversation_history.is_empty() {
        previous_response_id.as_ref().and_then(|prev_id| {
            let store = RESPONSE_STORE.lock();
            store.get(prev_id).map(|e| e.session_id.clone())
        })
    } else {
        None
    };

    let handler_guard = state.handler.lock().await;
    let result = if let Some(handler) = handler_guard.as_ref() {
        handler.handle_message(Platform::ApiServer, &session_id, &user_message, None).await
    } else {
        Err("No message handler registered".to_string())
    };

    match result {
        Ok(handler_result) => {
            // Emit message.delta events
            let response_text = handler_result.response;
            let delta_event = RunEvent {
                event: "message.delta".to_string(),
                run_id: run_id.clone(),
                timestamp: SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
                delta: Some(response_text.clone()),
                output: None,
                error: None,
                usage: None,
                sequence_number: Some(sequence_number),
            };
            let _ = tx.send(delta_event);
            sequence_number += 1;

            // Emit run.completed
            let completed_event = RunEvent {
                event: "run.completed".to_string(),
                run_id: run_id.clone(),
                timestamp: SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
                delta: None,
                output: Some(response_text),
                error: None,
                usage: Some(RunUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                }),
                sequence_number: Some(sequence_number),
            };
            let _ = tx.send(completed_event);
        }
        Err(e) => {
            let failed_event = RunEvent {
                event: "run.failed".to_string(),
                run_id: run_id.clone(),
                timestamp: SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
                delta: None,
                output: None,
                error: Some(e),
                usage: None,
                sequence_number: Some(sequence_number),
            };
            let _ = tx.send(failed_event);
        }
    }

    // Cleanup run stream
    RUN_STREAMS.lock().await.remove(&run_id);
}

/// GET /v1/runs/{run_id}/events — SSE stream of structured agent lifecycle events.
/// Mirrors Python _handle_run_events.
async fn run_events_handler(
    State(state): State<ApiServerState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> ApiResult<Sse<SseRunStreamType>> {
    // Bearer token auth
    if !state.api_key.is_empty() {
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != format!("Bearer {}", state.api_key) {
            return Err(unauthorized_error());
        }
    }

    // Wait for run to be registered (race condition window)
    let tx = {
        let mut attempts = 0;
        loop {
            let store = RUN_STREAMS.lock().await;
            if let Some(sender) = store.get_sender(&run_id) {
                break Some(sender);
            }
            drop(store);
            attempts += 1;
            if attempts >= 20 {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let Some(tx) = tx else {
        return Err(not_found_error(&format!("Run not found: {}", run_id)));
    };

    let stream = build_run_sse_stream(tx, run_id);
    Ok(Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text(": keepalive"),
        ))
}

/// Stream type for run SSE responses.
type SseRunStreamType = Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send>>;

/// Build an SSE stream from a run event broadcast channel.
fn build_run_sse_stream(
    tx: tokio::sync::broadcast::Sender<RunEvent>,
    _run_id: String,
) -> SseRunStreamType {
    Box::pin(async_stream::stream! {
        let mut rx = tx.subscribe();
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(event)) => {
                    // Check if this is a terminal event
                    let is_terminal = matches!(event.event.as_str(), "run.completed" | "run.failed");
                    let event_json = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                    let sse_event = axum::response::sse::Event::default()
                        .json_data(&event_json)
                        .unwrap_or_else(|e| axum::response::sse::Event::default().data(format!("{{\"error\":\"sse serialization: {e}\"}}")));
                    yield Ok::<_, std::convert::Infallible>(sse_event);

                    if is_terminal {
                        // Send final comment and close
                        let done_event = axum::response::sse::Event::default()
                            .data(": stream closed");
                        yield Ok(done_event);
                        break;
                    }
                }
                Ok(Err(_)) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Timeout — send keepalive
                    let keepalive = axum::response::sse::Event::default()
                        .data(": keepalive");
                    yield Ok(keepalive);
                }
            }
        }
    })
}

/// Normalize Responses API input into a list of history messages.
/// Accepts: string, or array of message objects / strings.
fn normalize_responses_input(
    input: &serde_json::Value,
) -> Result<Vec<HistoryMessage>, String> {
    if let Some(s) = input.as_str() {
        return Ok(vec![HistoryMessage {
            role: "user".to_string(),
            content: s.to_string(),
        }]);
    }

    if let Some(arr) = input.as_array() {
        let mut messages = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                if !s.is_empty() {
                    messages.push(HistoryMessage {
                        role: "user".to_string(),
                        content: s.to_string(),
                    });
                }
            } else if let Some(obj) = item.as_object() {
                let role = obj
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("user")
                    .to_string();
                let content = extract_content_from_value(obj.get("content"));
                if !content.is_empty() {
                    messages.push(HistoryMessage { role, content });
                }
            }
        }
        return Ok(messages);
    }

    Err("'input' must be a string or array".to_string())
}

/// Extract content text from a JSON value (handles string or content part objects).
fn extract_content_from_value(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else { return String::new() };

    if let Some(s) = value.as_str() {
        return s.to_string();
    }

    // Try to extract from content part objects
    if let Some(obj) = value.as_object() {
        // Direct text field
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            return text.to_string();
        }
    }

    // Try as array of content parts
    if let Some(arr) = value.as_array() {
        let mut parts: Vec<String> = Vec::new();
        for part in arr {
            if let Some(s) = part.as_str() {
                parts.push(s.to_string());
            } else if let Some(obj) = part.as_object() {
                if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
        }
        return parts.join("\n");
    }

    String::new()
}

/// Stream type for SSE responses.
type SseStreamType = Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send>>;

/// Union type for streaming or non-streaming response.
enum SseOrJson {
    Sse(Sse<SseStreamType>, String),
    Json(Json<ChatCompletionResponse>, String),
}

#[async_trait::async_trait]
impl axum::response::IntoResponse for SseOrJson {
    fn into_response(self) -> axum::response::Response {
        match self {
            SseOrJson::Sse(sse, session_id) => {
                let mut resp = sse.into_response();
                if let Ok(val) = axum::http::HeaderValue::from_str(&session_id) {
                    resp.headers_mut().insert("X-Hermez-Session-Id", val);
                }
                resp
            }
            SseOrJson::Json(json, session_id) => {
                let mut resp = json.into_response();
                if let Ok(val) = axum::http::HeaderValue::from_str(&session_id) {
                    resp.headers_mut().insert("X-Hermez-Session-Id", val);
                }
                resp
            }
        }
    }
}

/// Build an SSE stream from a complete response text.
/// Splits the text into character-level chunks and emits them as delta events.
fn build_sse_stream(
    chat_id: String,
    model: String,
    created: i64,
    response: String,
) -> Sse<SseStreamType> {
    let stream: SseStreamType = Box::pin(async_stream::stream! {
        // First event: role announcement
        let role_chunk = StreamChunk {
            id: chat_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![StreamChoice {
                index: 0,
                delta: StreamDelta {
                    role: Some("assistant".to_string()),
                    content: None,
                },
                finish_reason: None,
            }],
        };
        let event = axum::response::sse::Event::default()
            .json_data(&role_chunk)
            .unwrap_or_else(|e| axum::response::sse::Event::default().data(format!("{{\"error\":\"sse serialization: {e}\"}}")));
        yield Ok::<_, std::convert::Infallible>(event);

        // Split response into character chunks (3 chars per chunk for pacing)
        let mut chars = response.chars().peekable();
        while chars.peek().is_some() {
            let text: String = chars.by_ref().take(3).collect();
            if text.is_empty() {
                break;
            }
            let content_chunk = StreamChunk {
                id: chat_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: StreamDelta {
                        role: None,
                        content: Some(text),
                    },
                    finish_reason: None,
                }],
            };
            let event = axum::response::sse::Event::default()
                .json_data(&content_chunk)
                .unwrap_or_else(|e| axum::response::sse::Event::default().data(format!("{{\"error\":\"sse serialization: {e}\"}}")));
            yield Ok(event);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Final event: finish reason
        let finish_chunk = StreamChunk {
            id: chat_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![StreamChoice {
                index: 0,
                delta: StreamDelta {
                    role: None,
                    content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
        };
        let event = axum::response::sse::Event::default()
            .json_data(&finish_chunk)
            .unwrap_or_else(|e| axum::response::sse::Event::default().data(format!("{{\"error\":\"sse serialization: {e}\"}}")));
        yield Ok(event);

        // Send [DONE] marker
        let done_event = axum::response::sse::Event::default()
            .data("[DONE]");
        yield Ok(done_event);
    });

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text(": ping"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the global RESPONSE_STORE to prevent
    /// concurrent-test interference (cargo test runs tests in parallel).
    static RESPONSE_STORE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_config_defaults() {
        let config = ApiServerConfig::default();
        assert_eq!(config.port, 8642);
        assert_eq!(config.host, "127.0.0.1");
        assert!(config.api_key.is_empty());
    }

    #[test]
    fn test_config_from_env() {
        let config = ApiServerConfig::from_env();
        assert!(config.port > 0);
    }

    #[test]
    fn test_health_response() {
        let resp = HealthResponse { status: "ok".to_string() };
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn test_models_response() {
        let resp = ModelsResponse {
            object: "list".to_string(),
            data: vec![ModelInfo {
                id: "hermez-agent".to_string(),
                object: "model".to_string(),
                created: 0,
                owned_by: "nous-research".to_string(),
            }],
        };
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].id, "hermez-agent");
    }

    #[test]
    fn test_stream_chunk_serializes_cleanly() {
        // Ensure StreamChunk serializes to valid JSON without extra wrapping
        let chunk = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "hermez-agent".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: StreamDelta {
                    role: Some("assistant".to_string()),
                    content: Some("Hello".to_string()),
                },
                finish_reason: None,
            }],
        };

        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Verify structure matches OpenAI format
        assert_eq!(parsed["id"], "test-id");
        assert_eq!(parsed["object"], "chat.completion.chunk");
        assert_eq!(parsed["model"], "hermez-agent");
        assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");

        // The JSON should NOT contain double-wrapped "data: " prefix
        assert!(!json.contains("data: data:"));
    }

    #[test]
    fn test_response_store_session_chain() {
        let _guard = RESPONSE_STORE_TEST_LOCK.lock();

        // Simulate: request 1 has no session_id, gets a UUID.
        // Request 2 uses previous_response_id from response 1,
        // and should inherit the same session_id.

        // Clear store first
        RESPONSE_STORE.lock().entries.clear();
        RESPONSE_STORE.lock().order.clear();
        RESPONSE_STORE.lock().conversations.clear();

        // Simulate first turn
        let response_id_1 = "chatcmpl-first".to_string();
        let session_id_1 = "session-abc".to_string();
        RESPONSE_STORE.lock().put(response_id_1.clone(), ResponseStoreEntry {
            response_data: None,
            conversation_history: vec![],
            instructions: None,
            session_id: session_id_1.clone(),
            conversation: None,
        });

        // Simulate second turn with previous_response_id
        let store = RESPONSE_STORE.lock();
        let stored = store.get(&response_id_1).map(|e| e.session_id.clone());
        assert_eq!(stored, Some(session_id_1));

        // Without chain, new request should fall back to default
        let no_chain = store.get("nonexistent").map(|e| e.session_id.clone());
        assert_eq!(no_chain, None);
    }

    #[test]
    fn test_response_store_trim_on_overflow() {
        let _guard = RESPONSE_STORE_TEST_LOCK.lock();
        RESPONSE_STORE.lock().entries.clear();
        RESPONSE_STORE.lock().order.clear();
        RESPONSE_STORE.lock().conversations.clear();

        // Insert RESPONSE_STORE_MAX + 1 entries
        for i in 0..=RESPONSE_STORE_MAX {
            RESPONSE_STORE.lock().put(
                format!("resp-{i}"),
                ResponseStoreEntry {
                    response_data: None,
                    conversation_history: vec![],
                    instructions: None,
                    session_id: format!("session-{i}"),
                    conversation: None,
                },
            );
        }

        // Next insert should trigger LRU eviction
        {
            RESPONSE_STORE.lock().put(
                "resp-new".to_string(),
                ResponseStoreEntry {
                    response_data: None,
                    conversation_history: vec![],
                    instructions: None,
                    session_id: "session-new".to_string(),
                    conversation: None,
                },
            );
        }

        // Store should have exactly RESPONSE_STORE_MAX entries
        let store = RESPONSE_STORE.lock();
        assert_eq!(store.entries.len(), RESPONSE_STORE_MAX);
        // The new entry should be present
        assert!(store.entries.contains_key("resp-new"));
        // The oldest entry ("resp-0") should have been evicted
        assert!(!store.entries.contains_key("resp-0"));
    }

    #[test]
    fn test_response_store_conversation_mapping() {
        let _guard = RESPONSE_STORE_TEST_LOCK.lock();
        RESPONSE_STORE.lock().entries.clear();
        RESPONSE_STORE.lock().order.clear();
        RESPONSE_STORE.lock().conversations.clear();

        // Store with conversation name
        RESPONSE_STORE.lock().put(
            "resp-1".to_string(),
            ResponseStoreEntry {
                response_data: None,
                conversation_history: vec![],
                instructions: None,
                session_id: "session-1".to_string(),
                conversation: Some("my-chat".to_string()),
            },
        );

        // Lookup by conversation name
        let store = RESPONSE_STORE.lock();
        let resp_id = store.get_conversation("my-chat");
        assert_eq!(resp_id, Some(&"resp-1".to_string()));

        // Unknown conversation
        assert!(store.get_conversation("unknown").is_none());
    }

    #[test]
    fn test_response_store_delete() {
        let _guard = RESPONSE_STORE_TEST_LOCK.lock();
        RESPONSE_STORE.lock().entries.clear();
        RESPONSE_STORE.lock().order.clear();
        RESPONSE_STORE.lock().conversations.clear();

        RESPONSE_STORE.lock().put(
            "resp-to-delete".to_string(),
            ResponseStoreEntry {
                response_data: None,
                conversation_history: vec![],
                instructions: None,
                session_id: "session-x".to_string(),
                conversation: None,
            },
        );

        assert!(RESPONSE_STORE.lock().delete("resp-to-delete"));
        assert!(!RESPONSE_STORE.lock().delete("resp-to-delete")); // already gone
        assert!(RESPONSE_STORE.lock().get("resp-to-delete").is_none());
    }

    #[test]
    fn test_responses_input_normalization() {
        // String input
        let input = serde_json::json!("Hello");
        let msgs = normalize_responses_input(&input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "Hello");

        // Array of strings
        let input = serde_json::json!(["Hello", "World"]);
        let msgs = normalize_responses_input(&input).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Hello");
        assert_eq!(msgs[1].content, "World");

        // Array of message objects
        let input = serde_json::json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hey"}
        ]);
        let msgs = normalize_responses_input(&input).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn test_response_data_serializes() {
        let data = ResponseData {
            id: "resp_test".to_string(),
            object: "response".to_string(),
            status: "completed".to_string(),
            created_at: 0,
            model: "hermez-agent".to_string(),
            output: vec![OutputItem {
                item_type: "message".to_string(),
                role: "assistant".to_string(),
                content: Some(vec![ContentPart {
                    part_type: "output_text".to_string(),
                    text: Some("Hello world".to_string()),
                }]),
                call_id: None,
                name: None,
                arguments: None,
                output: None,
            }],
            usage: ResponseUsage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
            },
        };

        let json = serde_json::to_string(&data).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], "resp_test");
        assert_eq!(parsed["object"], "response");
        assert_eq!(parsed["output"][0]["role"], "assistant");
        assert_eq!(parsed["output"][0]["content"][0]["text"], "Hello world");
    }

    #[test]
    fn test_idempotency_cache_set_get() {
        let mut cache = IdempotencyCache::default();
        let resp = serde_json::json!({"id": "resp_1"});
        cache.set("key-1".to_string(), "fp-abc".to_string(), resp.clone());
        let got = cache.get("key-1", "fp-abc").unwrap();
        assert_eq!(got["id"], "resp_1");
    }

    #[test]
    fn test_idempotency_cache_fingerprint_mismatch() {
        let mut cache = IdempotencyCache::default();
        let resp = serde_json::json!({"id": "resp_1"});
        cache.set("key-1".to_string(), "fp-abc".to_string(), resp.clone());
        // Same key but different fingerprint → miss
        assert!(cache.get("key-1", "fp-xyz").is_none());
    }

    #[test]
    fn test_idempotency_cache_lru_eviction() {
        let mut cache = IdempotencyCache::default();
        // Override max for test — manually insert enough to trigger overflow
        for i in 0..IDEM_MAX_ITEMS + 10 {
            let key = format!("key-{i}");
            cache.set(key, format!("fp-{i}"), serde_json::json!({"i": i}));
        }
        assert!(cache.store.len() <= IDEM_MAX_ITEMS);
    }

    #[test]
    fn test_idempotency_cache_ttl_expiry() {
        use std::time::Instant;
        let mut cache = IdempotencyCache::default();
        cache.set("ttl-key".to_string(), "fp-ttl".to_string(), serde_json::json!({"ok": true}));

        // Manually backdate the entry to simulate expiry
        if let Some(entry) = cache.store.get_mut("ttl-key") {
            entry.timestamp = Instant::now() - Duration::from_secs(IDEM_TTL_SECS + 1);
        }
        // Now get() should purge and return None
        assert!(cache.get("ttl-key", "fp-ttl").is_none());
    }

    #[test]
    fn test_make_request_fingerprint_deterministic() {
        let data = serde_json::json!({"model": "hermez", "messages": [{"role": "user", "content": "hi"}]});
        let fp1 = make_request_fingerprint(&data, &["model", "messages"]);
        let fp2 = make_request_fingerprint(&data, &["model", "messages"]);
        assert_eq!(fp1, fp2); // deterministic
    }

    #[test]
    fn test_make_request_fingerprint_different_keys_different_hash() {
        let data = serde_json::json!({"model": "hermez", "stream": true});
        let fp1 = make_request_fingerprint(&data, &["model"]);
        let fp2 = make_request_fingerprint(&data, &["model", "stream"]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_extract_output_items_no_tool_calls() {
        let messages: Vec<serde_json::Value> = vec![];
        let items = extract_output_items("Hello", &messages);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_type, "message");
        assert_eq!(items[0].content.as_ref().unwrap()[0].text.as_deref(), Some("Hello"));
    }

    #[test]
    fn test_extract_output_items_with_tool_calls() {
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "tc-1",
                    "function": {"name": "get_weather", "arguments": "{\"city\": \"NYC\"}"}
                }]
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "tc-1",
                "content": "Sunny, 72°F"
            }),
        ];
        let items = extract_output_items("It's sunny!", &messages);
        assert_eq!(items.len(), 3); // function_call + function_call_output + message

        // function_call
        assert_eq!(items[0].item_type, "function_call");
        assert_eq!(items[0].call_id.as_deref(), Some("tc-1"));
        assert_eq!(items[0].name.as_deref(), Some("get_weather"));
        assert_eq!(items[0].arguments.as_deref(), Some("{\"city\": \"NYC\"}"));

        // function_call_output
        assert_eq!(items[1].item_type, "function_call_output");
        assert_eq!(items[1].call_id.as_deref(), Some("tc-1"));
        assert_eq!(items[1].output.as_deref(), Some("Sunny, 72°F"));

        // final message
        assert_eq!(items[2].item_type, "message");
        assert_eq!(items[2].role, "assistant");
    }
}
