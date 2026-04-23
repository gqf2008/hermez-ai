//! ACP (Agent Client Protocol) — JSON-RPC types and transport.
//!
//! Implements the agent-client-protocol JSON-RPC 2.0 over stdio.
//! Mirrors the Python `acp` SDK types used by `acp_adapter/server.py`.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ---- JSON-RPC 2.0 core types ------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub error: JsonRpcErrorDetail,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcErrorDetail {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
}

// ---- ACP schema types -------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<SessionForkCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionResumeCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionForkCapabilities {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListCapabilities {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResumeCapabilities {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMethodAgent {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    pub protocol_version: i64,
    pub agent_info: Implementation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<AgentCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<Vec<AuthMethodAgent>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticateResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSessionResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSessionResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionModelResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionModeResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetConfigOptionResponse {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<u64>,
}

// Content block types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { image_url: String },
    Resource { resource: ResourceContent },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// Session update types (server → client notifications)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableCommandsUpdate {
    #[serde(rename = "sessionUpdate")]
    pub session_update: String,
    #[serde(rename = "availableCommands")]
    pub available_commands: Vec<AvailableCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<UnstructuredCommandInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstructuredCommandInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

// Tool call ACP event types
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallStart {
    #[serde(rename = "sessionUpdate")]
    pub session_update: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "title")]
    pub title: String,
    #[serde(rename = "kind")]
    pub kind: String,
    #[serde(rename = "rawInput")]
    pub raw_input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallProgress {
    #[serde(rename = "sessionUpdate")]
    pub session_update: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "content")]
    pub content: Vec<ToolCallContent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ACP session update envelope
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessage {
        message: Vec<TextContent>,
    },
    AgentThought {
        thought: String,
    },
    ToolCallStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        title: String,
        kind: String,
        #[serde(rename = "rawInput")]
        raw_input: serde_json::Value,
    },
    ToolCallContent {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: Vec<ToolCallContent>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        available_commands: Vec<AvailableCommand>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

// ---- Protocol constants -----------------------------------------------------

pub const PROTOCOL_VERSION: i64 = 1;
pub const AGENT_NAME: &str = "hermez-agent";
