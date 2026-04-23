//! MCP server connection management using the `rmcp` crate.
//!
//! Handles stdio subprocess transport, tool discovery, and tool calling.
//! Bridges async rmcp API to sync tool handler closures via `tokio::runtime::Runtime`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParam, RawContent};
use rmcp::model::ResourceContents;
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
}

/// A connected MCP server handle.
pub struct McpServerHandle {
    pub config: ServerConfig,
    pub tool_count: usize,
    inner: Arc<parking_lot::Mutex<Option<McpInner>>>,
}

/// Async rmcp client peer.
struct McpInner {
    runtime: Runtime,
    peer: Arc<parking_lot::Mutex<Option<rmcp::Peer<RoleClient>>>>,
}

impl McpServerHandle {
    /// Connect to an MCP server via stdio transport.
    pub fn connect_stdio(config: ServerConfig) -> Result<Self, String> {
        // Build the command
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args);

        // Build safe environment (only pass explicitly set vars + PATH)
        let mut child_env: Vec<(String, String)> = Vec::new();
        for (key, value) in &config.env {
            child_env.push((key.clone(), value.clone()));
        }
        if let Ok(path) = std::env::var("PATH") {
            child_env.push(("PATH".to_string(), path));
        }
        cmd.envs(child_env);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let rt = Runtime::new().map_err(|e| format!("Failed to create runtime: {e}"))?;

        // Create the transport
        let transport = rmcp::transport::TokioChildProcess::new(&mut cmd)
            .map_err(|e| format!("Failed to create transport: {e}"))?;

        // Start the client and discover tools
        let peer = rt.block_on(async {
            let client = rmcp::serve_client((), transport)
                .await
                .map_err(|e| format!("Failed to connect: {e}"))?;

            // serve_client returns RunningService which derefs to Peer
            let peer: rmcp::Peer<RoleClient> = (*client).clone();
            let tools = peer.list_all_tools()
                .await
                .map_err(|e| format!("Failed to list tools: {e}"))?;

            let tool_count = tools.len();
            tracing::info!("Connected to MCP server, discovered {} tools", tool_count);

            Ok::<_, String>((peer, tool_count))
        })?;

        let (peer, tool_count) = peer;

        Ok(Self {
            config,
            tool_count,
            inner: Arc::new(parking_lot::Mutex::new(Some(McpInner {
                runtime: rt,
                peer: Arc::new(parking_lot::Mutex::new(Some(peer))),
            }))),
        })
    }

    /// List tools from the connected server.
    #[allow(clippy::await_holding_lock)]
    pub fn list_tools(&self) -> Result<Vec<Value>, String> {
        let inner = self.inner.lock();
        let inner = inner.as_ref().ok_or_else(|| "Server disconnected".to_string())?;

        let peer = inner.peer.clone();
        inner.runtime.block_on(async {
            let guard = peer.lock();
            let peer_ref = guard.as_ref().ok_or_else(|| "Peer lost".to_string())?;
            let tools = peer_ref.list_all_tools()
                .await
                .map_err(|e| format!("Failed to list tools: {e}"))?;

            let tool_list: Vec<Value> = tools
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();

            Ok(tool_list)
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

            let result = peer_ref.call_tool(CallToolRequestParam {
                name: std::borrow::Cow::Owned(tool_name_str),
                arguments: arguments_clone.as_object().cloned(),
            })
            .await
            .map_err(|e| format!("Tool call failed: {e}"))?;

            // Convert result content to JSON
            let content: Vec<Value> = result
                .content
                .into_iter()
                .map(|c| {
                    // Content = Annotated<RawContent>, Deref to RawContent
                    match &c.raw {
                        RawContent::Text(text) => {
                            serde_json::json!({ "type": "text", "text": text.text })
                        }
                        RawContent::Image(image) => {
                            serde_json::json!({ "type": "image", "data": image.data, "mime_type": image.mime_type })
                        }
                        RawContent::Resource(resource) => {
                            let (uri, mime_type) = match &resource.resource {
                                ResourceContents::TextResourceContents { uri, mime_type, .. } => {
                                    (uri.clone(), mime_type.clone())
                                }
                                ResourceContents::BlobResourceContents { uri, mime_type, .. } => {
                                    (uri.clone(), mime_type.clone())
                                }
                            };
                            serde_json::json!({ "type": "resource", "uri": uri, "mime_type": mime_type })
                        }
                    }
                })
                .collect();

            Ok(serde_json::json!({
                "content": content,
                "is_error": result.is_error.unwrap_or(false),
            }))
        })
    }

    /// Disconnect from the server.
    pub fn disconnect(&self) {
        let mut inner = self.inner.lock();
        if let Some(mcp_inner) = inner.take() {
            // Drop the peer, which will close the transport and kill the subprocess
            drop(mcp_inner);
        }
    }
}
