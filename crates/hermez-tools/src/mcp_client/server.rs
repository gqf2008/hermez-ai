//! MCP server connection management using the `rmcp` crate.
//!
//! Handles stdio subprocess transport, tool discovery, and tool calling.
//! Bridges async rmcp API to sync tool handler closures via `tokio::runtime::Runtime`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{CallToolRequestParams, RawContent, SamplingMessage, SamplingMessageContent};
use rmcp::model::ResourceContents;
use rmcp::ErrorData as McpError;
use rmcp::RoleClient;
use serde_json::Value;
use tokio::runtime::Runtime;

/// Configuration for an MCP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub transport: String,
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub timeout: u64,
    pub strip_auth_on_redirect: bool,
}

/// Open a log file for MCP server stderr output.
fn open_mcp_stderr_log(server_name: &str) -> Result<std::fs::File, String> {
    let home = hermez_core::get_hermez_home();
    let log_dir = home.join("logs");
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| format!("Failed to create log dir: {e}"))?;
    let safe_name = server_name.replace(['/', '\\', ' '], "_");
    let log_path = log_dir.join(format!("mcp-stderr-{safe_name}.log"));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("Failed to open MCP stderr log at {}: {e}", log_path.display()))
}

/// MCP Sampling handler — responds to server-initiated LLM requests.
/// Mirrors Python `SamplingHandler` in tools/mcp_tool.py.
struct SamplingClientHandler;

impl ClientHandler for SamplingClientHandler {
    fn create_message(
        &self,
        params: rmcp::model::CreateMessageRequestParams,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = std::result::Result<rmcp::model::CreateMessageResult, McpError>> + Send {
        async move {
            let messages: Vec<Value> = params.messages.into_iter().map(|m| {
                let role = match m.role {
                    rmcp::model::Role::User => "user",
                    rmcp::model::Role::Assistant => "assistant",
                };
                let text = m.content.into_vec().iter()
                    .find_map(|c| if let SamplingMessageContent::Text(ref t) = c { Some(t.text.clone()) } else { None })
                    .unwrap_or_default();
                serde_json::json!({"role": role, "content": text})
            }).collect();

            let result = hermez_llm::auxiliary_client::call_llm(
                Some("mcp_sampling"), None, None, None, None, messages,
                params.temperature.map(|t| t as f64), None, None, None, None,
            );

            match result {
                Ok(resp) => {
                    let msg = SamplingMessage::new(
                        rmcp::model::Role::Assistant,
                        SamplingMessageContent::text(resp.content),
                    );
                    let mut result = rmcp::model::CreateMessageResult::new(msg, resp.model);
                    if let Some(reason) = resp.finish_reason {
                        result = result.with_stop_reason(reason);
                    }
                    Ok(result)
                }
                Err(e) => Err(McpError {
                    code: rmcp::model::ErrorCode::INTERNAL_ERROR,
                    message: format!("Sampling LLM call failed: {e}").into(),
                    data: None,
                }),
            }
        }
    }
}

/// A connected MCP server handle.
pub struct McpServerHandle {
    pub config: ServerConfig,
    pub tool_count: usize,
    inner: Arc<parking_lot::Mutex<Option<McpInner>>>,
    cached_tools: parking_lot::Mutex<Option<Vec<Value>>>,
}

/// Async rmcp client peer.
struct McpInner {
    runtime: Arc<Runtime>,
    peer: Arc<parking_lot::Mutex<Option<rmcp::Peer<RoleClient>>>>,
}

impl McpServerHandle {
    /// Connect to an MCP server via stdio transport with exponential backoff.
    pub fn connect_stdio(config: ServerConfig) -> Result<Self, String> {
        let mut child_env: Vec<(String, String)> = Vec::new();
        for (key, value) in &config.env {
            child_env.push((key.clone(), value.clone()));
        }
        if let Ok(path) = std::env::var("PATH") {
            child_env.push(("PATH".to_string(), path));
        }

        let rt = Arc::new(Runtime::new().map_err(|e| format!("Failed to create runtime: {e}"))?);
        let max_attempts = 3;
        let base_delay_ms = 1000u64;
        let max_delay_ms = 30_000u64;
        let mut last_err = String::new();
        let name = config.name.clone();

        for attempt in 0..max_attempts {
            if attempt > 0 {
                let delay = (base_delay_ms * (1u64 << (attempt - 1))).min(max_delay_ms);
                tracing::warn!("MCP reconnect {}/{} for '{}' after {}ms", attempt + 1, max_attempts, name, delay);
                rt.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(delay)).await });
            }

            let mut fresh_cmd = tokio::process::Command::new(&config.command);
            fresh_cmd.args(&config.args);
            fresh_cmd.envs(child_env.clone());
            fresh_cmd.stdin(std::process::Stdio::piped());
            fresh_cmd.stdout(std::process::Stdio::piped());
            let stderr_file = match open_mcp_stderr_log(&name) {
                Ok(f) => f,
                Err(e) => { last_err = format!("Failed to open stderr log: {e}"); continue; }
            };
            fresh_cmd.stderr(stderr_file);
            fresh_cmd.kill_on_drop(true);

            let transport = match rmcp::transport::TokioChildProcess::new(fresh_cmd) {
                Ok(t) => t,
                Err(e) => { last_err = format!("Failed to create transport: {e}"); continue; }
            };

            match rt.block_on(async {
                let client = rmcp::serve_client(SamplingClientHandler, transport).await
                    .map_err(|e| format!("serve_client: {e}"))?;
                let peer: rmcp::Peer<RoleClient> = (*client).clone();
                let tools = peer.list_all_tools().await
                    .map_err(|e| format!("list_tools: {e}"))?;
                Ok::<_, String>((peer, tools))
            }) {
                Ok((peer, tools)) => {
                    tracing::info!("MCP '{}': {} tools (attempt {})", name, tools.len(), attempt + 1);
                    return Ok(Self { config, tool_count: tools.len(),
                        inner: Arc::new(parking_lot::Mutex::new(Some(McpInner { runtime: rt,
                            peer: Arc::new(parking_lot::Mutex::new(Some(peer))) }))),
                        cached_tools: parking_lot::Mutex::new(None) });
                }
                Err(e) => { last_err = format!("Failed to connect: {e}"); }
            }
        }
        Err(format!("MCP '{}' failed after {} attempts: {}", name, max_attempts, last_err))
    }

    /// Connect via SSE/HTTP transport.
    /// Requires reqwest >= 0.13 (workspace currently on 0.12 - gate until upgrade).
    #[cfg(feature = "mcp-sse")]
    pub fn connect_sse(config: ServerConfig) -> Result<Self, String> {
        let url: std::sync::Arc<str> = config.command.clone().into();
        let name = config.name.clone();
        let rt = Arc::new(Runtime::new().map_err(|e| format!("Failed to create runtime: {e}"))?);
        let rt_clone = rt.clone();

        rt.block_on(async move {
            use rmcp::transport::streamable_http_client::{
                StreamableHttpClientWorker, StreamableHttpClientTransport,
            };
            let worker = StreamableHttpClientWorker::<reqwest::Client>::new_simple(url);
            let transport = StreamableHttpClientTransport::new(worker);
            let client = rmcp::serve_client(SamplingClientHandler, transport).await
                .map_err(|e| format!("MCP SSE '{}': {e}", name))?;
            let peer: rmcp::Peer<RoleClient> = (*client).clone();
            let tools = peer.list_all_tools().await
                .map_err(|e| format!("List tools '{}': {e}", name))?;
            let tool_list: Vec<Value> = tools.iter().map(|t| serde_json::json!({
                "name": t.name, "description": t.description, "input_schema": t.input_schema
            })).collect();
            tracing::info!("MCP SSE '{}': {} tools", name, tool_list.len());
            Ok(Self { config, tool_count: tool_list.len(),
                inner: Arc::new(parking_lot::Mutex::new(Some(McpInner { runtime: rt_clone,
                    peer: Arc::new(parking_lot::Mutex::new(Some(peer))) }))),
                cached_tools: parking_lot::Mutex::new(Some(tool_list)) })
        })
    }

    /// List tools from the connected server.
    #[allow(clippy::await_holding_lock)]
    pub fn list_tools(&self) -> Result<Vec<Value>, String> {
        if let Some(ref cached) = *self.cached_tools.lock() {
            return Ok(cached.clone());
        }
        let inner = self.inner.lock();
        let inner = inner.as_ref().ok_or_else(|| "Server disconnected".to_string())?;
        let peer = inner.peer.clone();
        inner.runtime.block_on(async {
            let guard = peer.lock();
            let peer_ref = guard.as_ref().ok_or_else(|| "Peer lost".to_string())?;
            let tools = peer_ref.list_all_tools().await.map_err(|e| format!("Failed to list tools: {e}"))?;
            let result: Vec<Value> = tools.iter().map(|t| serde_json::json!({
                "name": t.name, "description": t.description, "input_schema": t.input_schema
            })).collect();
            *self.cached_tools.lock() = Some(result.clone());
            Ok(result)
        })
    }

    /// Call a tool on the connected server.
    #[allow(clippy::await_holding_lock)]
    pub fn call_tool(&self, tool_name: &str, arguments: &Value) -> Result<Value, String> {
        let inner = self.inner.lock();
        let inner = inner.as_ref().ok_or_else(|| "Server disconnected".to_string())?;
        let peer = inner.peer.clone();
        let tool_name_str = tool_name.to_string();
        let arguments_clone = arguments.clone();
        inner.runtime.block_on(async {
            let guard = peer.lock();
            let peer_ref = guard.as_ref().ok_or_else(|| "Peer lost".to_string())?;
            let params = CallToolRequestParams::new(tool_name_str)
                .with_arguments(arguments_clone.as_object().cloned().unwrap_or_default());
            let result = peer_ref.call_tool(params).await
                .map_err(|e| format!("Tool call failed: {e}"))?;
            let content: Vec<Value> = result.content.into_iter().map(|c| match &c.raw {
                RawContent::Text(text) => serde_json::json!({"type":"text","text":text.text}),
                RawContent::Image(image) => serde_json::json!({"type":"image","data":image.data,"mime_type":image.mime_type}),
                RawContent::Resource(resource) => {
                    let (uri, mime_type) = match &resource.resource {
                        ResourceContents::TextResourceContents { uri, mime_type, .. } => (uri.clone(), mime_type.clone()),
                        ResourceContents::BlobResourceContents { uri, mime_type, .. } => (uri.clone(), mime_type.clone()),
                    };
                    serde_json::json!({"type":"resource","uri":uri,"mime_type":mime_type})
                }
                RawContent::Audio(_) => serde_json::json!({"type":"audio","data":""}),
                RawContent::ResourceLink(link) => serde_json::json!({"type":"resource_link","uri":link.uri}),
            }).collect();
            Ok(serde_json::json!({"content": content, "is_error": result.is_error.unwrap_or(false)}))
        })
    }

    /// Disconnect from the server.
    pub fn disconnect(&self) {
        let mut inner = self.inner.lock();
        if let Some(mcp_inner) = inner.take() {
            drop(mcp_inner);
        }
    }
}
