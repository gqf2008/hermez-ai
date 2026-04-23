//! MCP client tool — Model Context Protocol integration.
//!
//! Mirrors the Python `tools/mcp_tool.py`.
//! Uses the `rmcp` crate for stdio transport to MCP servers.
//! Supports connecting to external MCP servers, listing their tools, and calling them.

pub mod security;
pub mod server;

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::registry::{tool_error, ToolRegistry};
use crate::mcp_client::server::{McpServerHandle, ServerConfig};

/// Global registry of connected MCP servers.
static MCP_SERVERS: Lazy<Mutex<HashMap<String, McpServerHandle>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Handle MCP client tool call.
pub fn handle_mcp_client(args: Value) -> Result<String, hermez_core::HermezError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list_servers");

    match action {
        "list_servers" => mcp_list_servers(),
        "connect" => mcp_connect(&args),
        "disconnect" => mcp_disconnect(&args),
        "list_tools" => mcp_list_tools(&args),
        "call_tool" => mcp_call_tool(&args),
        _ => Ok(tool_error(format!(
            "Unknown MCP action: '{action}'. Valid actions: list_servers, connect, disconnect, list_tools, call_tool"
        ))),
    }
}

fn mcp_list_servers() -> Result<String, hermez_core::HermezError> {
    let servers = MCP_SERVERS.lock().unwrap();
    let server_list: Vec<Value> = servers
        .iter()
        .map(|(name, handle)| {
            serde_json::json!({
                "name": name,
                "transport": handle.config.transport,
                "enabled": handle.config.enabled,
                "status": "connected",
                "tool_count": handle.tool_count,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "action": "list_servers",
        "servers": server_list,
        "total": server_list.len(),
    })
    .to_string())
}

fn mcp_connect(args: &Value) -> Result<String, hermez_core::HermezError> {
    let name = match args.get("server").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("mcp_connect requires 'server' parameter.")),
    };

    let command = match args.get("command").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => return Ok(tool_error("mcp_connect requires 'command' parameter.")),
    };

    let cmd_args = args
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let env: HashMap<String, String> = args
        .get("env")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let config = ServerConfig {
        name: name.clone(),
        transport: "stdio".to_string(),
        enabled: true,
        command,
        args: cmd_args,
        env,
        timeout: args.get("timeout").and_then(Value::as_u64).unwrap_or(120),
    };

    let mut servers = MCP_SERVERS.lock().unwrap();
    if servers.contains_key(&name) {
        return Ok(serde_json::json!({
            "success": true,
            "action": "connect",
            "server": name,
            "status": "already_connected",
        })
        .to_string());
    }

    match McpServerHandle::connect_stdio(config) {
        Ok(handle) => {
            let tool_count = handle.tool_count;
            servers.insert(name.clone(), handle);
            Ok(serde_json::json!({
                "success": true,
                "action": "connect",
                "server": name,
                "status": "connected",
                "tools_discovered": tool_count,
            })
            .to_string())
        }
        Err(e) => Ok(tool_error(format!("Failed to connect to MCP server '{name}': {e}"))),
    }
}

fn mcp_disconnect(args: &Value) -> Result<String, hermez_core::HermezError> {
    let server = match args.get("server").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("mcp_disconnect requires 'server' parameter.")),
    };

    let mut servers = MCP_SERVERS.lock().unwrap();
    if let Some(handle) = servers.remove(&server) {
        handle.disconnect();
        Ok(serde_json::json!({
            "success": true,
            "action": "disconnect",
            "server": server,
        })
        .to_string())
    } else {
        Ok(tool_error(format!("Server not connected: {server}")))
    }
}

fn mcp_list_tools(args: &Value) -> Result<String, hermez_core::HermezError> {
    let server = match args.get("server").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("mcp_list_tools requires 'server' parameter.")),
    };

    let servers = MCP_SERVERS.lock().unwrap();
    let handle = match servers.get(&server) {
        Some(h) => h,
        None => return Ok(tool_error(format!("Server not connected: {server}. Use mcp_connect first."))),
    };

    match handle.list_tools() {
        Ok(tools) => Ok(serde_json::json!({
            "success": true,
            "action": "list_tools",
            "server": server,
            "tools": tools,
            "total": tools.len(),
        })
        .to_string()),
        Err(e) => Ok(tool_error(format!("Failed to list tools for server '{server}': {e}"))),
    }
}

fn mcp_call_tool(args: &Value) -> Result<String, hermez_core::HermezError> {
    let server = match args.get("server").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("mcp_call_tool requires 'server' parameter.")),
    };

    let tool = match args.get("tool").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => return Ok(tool_error("mcp_call_tool requires 'tool' parameter.")),
    };

    let arguments = args
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    let servers = MCP_SERVERS.lock().unwrap();
    let handle = match servers.get(&server) {
        Some(h) => h,
        None => return Ok(tool_error(format!("Server not connected: {server}. Use mcp_connect first."))),
    };

    match handle.call_tool(&tool, &arguments) {
        Ok(result) => Ok(serde_json::json!({
            "success": true,
            "action": "call_tool",
            "server": server,
            "tool": tool,
            "result": result,
        })
        .to_string()),
        Err(e) => Ok(tool_error(format!("Tool call failed on server '{server}': {e}"))),
    }
}

/// Register MCP client tool.
pub fn register_mcp_client_tool(registry: &mut ToolRegistry) {
    registry.register(
        "mcp_client".to_string(),
        "mcp".to_string(),
        serde_json::json!({
            "name": "mcp_client",
            "description": "Model Context Protocol client. Connect to external MCP servers and use their tools. Actions: list_servers, connect, disconnect, list_tools, call_tool.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "MCP action to perform." },
                    "server": { "type": "string", "description": "MCP server name." },
                    "command": { "type": "string", "description": "Command to run for stdio server." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for the command." },
                    "env": { "type": "object", "description": "Environment variables for the subprocess." },
                    "timeout": { "type": "integer", "description": "Tool call timeout in seconds." },
                    "tool": { "type": "string", "description": "Tool name to call on the server." },
                    "arguments": { "type": "object", "description": "Arguments for the tool call." }
                },
                "required": ["action"]
            }
        }),
        std::sync::Arc::new(handle_mcp_client),
        None,
        vec!["mcp".to_string()],
        "Model Context Protocol client".to_string(),
        "🔌".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_servers() {
        let result = handle_mcp_client(serde_json::json!({ "action": "list_servers" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert!(json["servers"].as_array().is_some());
    }

    #[test]
    fn test_connect_missing_server() {
        let result = handle_mcp_client(serde_json::json!({ "action": "connect" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_disconnect_missing_server() {
        let result = handle_mcp_client(serde_json::json!({
            "action": "disconnect",
            "server": "nonexistent"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_list_tools_missing_server() {
        let result = handle_mcp_client(serde_json::json!({ "action": "list_tools" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_call_tool_missing_params() {
        let result = handle_mcp_client(serde_json::json!({ "action": "call_tool" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_unknown_action() {
        let result = handle_mcp_client(serde_json::json!({ "action": "restart" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }
}
