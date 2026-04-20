//! GitHub Copilot ACP client adapter.
//!
//! Mirrors Python `agent/copilot_acp_client.py`.
//!
//! Opens a short-lived ACP session via `copilot --acp --stdio`, sends the
//! formatted conversation as a single prompt, collects text chunks, and
//! converts the result back into the minimal shape Hermes expects from an
//! OpenAI-compatible client.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

const ACP_MARKER_BASE_URL: &str = "acp://copilot";
const DEFAULT_TIMEOUT_SECONDS: u64 = 900;

/// Resolve the Copilot CLI command.
fn resolve_command() -> String {
    std::env::var("HERMES_COPILOT_ACP_COMMAND")
        .or_else(|_| std::env::var("COPILOT_CLI_PATH"))
        .unwrap_or_else(|_| "copilot".into())
}

/// Resolve extra arguments for the Copilot CLI.
fn resolve_args() -> Vec<String> {
    if let Ok(raw) = std::env::var("HERMES_COPILOT_ACP_ARGS") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.split_whitespace().map(|s| s.to_string()).collect();
        }
    }
    vec!["--acp".into(), "--stdio".into()]
}

/// JSON-RPC request.
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: Value,
}

/// JSON-RPC response.
#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// ACP server → client notification.
#[derive(Deserialize, Debug)]
struct AcpNotification {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

/// Copilot ACP client.
pub struct CopilotAcpClient {
    command: String,
    args: Vec<String>,
    timeout_seconds: u64,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for CopilotAcpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotAcpClient {
    pub fn new() -> Self {
        Self {
            command: resolve_command(),
            args: resolve_args(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Start the Copilot ACP subprocess.
    async fn start_process(&self) -> anyhow::Result<(Child, ChildStdin, BufReader<ChildStdout>)> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let reader = BufReader::new(stdout);
        Ok((child, stdin, reader))
    }

    /// Run a prompt through Copilot ACP and return the response text.
    pub async fn run_prompt(
        &self,
        prompt_text: &str,
        tools: Option<&[Value]>,
    ) -> anyhow::Result<String> {
        let (mut child, mut stdin, mut reader) = self.start_process().await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        // Spawn stdout reader
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let _ = tx_clone.send(trimmed.to_string());
                        }
                    }
                    Err(e) => {
                        tracing::error!("ACP stdout read error: {}", e);
                        break;
                    }
                }
            }
        });

        // Initialize ACP session
        let init_id = self.next_request_id();
        let init_req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: init_id,
            method: "initialize".into(),
            params: serde_json::json!({
                "protocolVersion": "2024-11-15",
                "capabilities": {},
                "clientInfo": { "name": "hermes", "version": env!("CARGO_PKG_VERSION") },
            }),
        };
        self.send_request(&mut stdin, &init_req).await?;

        // Wait for initialize response
        let _ = self.wait_for_response(init_id, &mut rx, self.timeout_seconds).await?;

        // Send the prompt as a tools/call request
        let call_id = self.next_request_id();
        let mut params = serde_json::json!({
            "prompt": prompt_text,
        });
        if let Some(tools) = tools {
            params["tools"] = serde_json::json!(tools);
        }

        let call_req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: call_id,
            method: "tools/call".into(),
            params,
        };
        self.send_request(&mut stdin, &call_req).await?;

        // Collect response
        let result = self
            .wait_for_response(call_id, &mut rx, self.timeout_seconds)
            .await;

        // Clean up
        let _ = stdin.shutdown().await;
        let _ = child.wait().await;

        match result {
            Ok(val) => Ok(extract_text(&val)),
            Err(e) => Err(e),
        }
    }

    async fn send_request(
        &self,
        stdin: &mut ChildStdin,
        req: &JsonRpcRequest,
    ) -> anyhow::Result<()> {
        let json = serde_json::to_string(req)?;
        stdin.write_all(json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn wait_for_response(
        &self,
        expected_id: u64,
        rx: &mut mpsc::UnboundedReceiver<String>,
        timeout_secs: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Duration::from_secs(timeout_secs);

        loop {
            let line = timeout(deadline, rx.recv())
                .await
                .map_err(|_| anyhow::anyhow!("ACP response timeout"))?
                .ok_or_else(|| anyhow::anyhow!("ACP channel closed"))?;

            // Try parsing as JSON-RPC response
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&line) {
                if resp.id.as_ref().and_then(|v| v.as_u64()) == Some(expected_id) {
                    if let Some(err) = resp.error {
                        return Err(anyhow::anyhow!(
                            "ACP error {}: {}",
                            err.code,
                            err.message
                        ));
                    }
                    return Ok(resp.result.unwrap_or(Value::Null));
                }
            }

            // Try parsing as notification (ignore)
            if serde_json::from_str::<AcpNotification>(&line).is_ok() {
                continue;
            }

            // Unexpected line — log and continue waiting
            tracing::debug!("ACP unexpected line: {}", line);
        }
    }
}

/// Extract text from an ACP response value.
fn extract_text(value: &Value) -> String {
    // Try common ACP response shapes
    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        let mut text = String::new();
        for item in content {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
            }
        }
        return text;
    }

    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    if let Some(result) = value.get("result").and_then(|v| v.as_str()) {
        return result.to_string();
    }

    // Fallback: return the JSON as a string
    value.to_string()
}

/// Build a prompt from OpenAI-style messages.
pub fn format_messages_as_prompt(messages: &[Value], _model: Option<&str>) -> String {
    let mut parts = Vec::new();
    parts.push(
        "You are being used as the active ACP agent backend for Hermes. \
         Use ACP capabilities to complete tasks."
            .into(),
    );

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        parts.push(format!("[{}] {}", role, content));
    }

    parts.join("\n\n")
}

/// Parse `<tool_call>{...}</tool_call>` blocks from ACP text output.
pub fn extract_tool_calls(text: &str) -> (Vec<Value>, String) {
    let re = regex::Regex::new(r"<tool_call>\s*(\{.*?\})\s*</tool_call>").unwrap();
    let mut tool_calls = Vec::new();
    let mut last_end = 0;
    let mut plain_text = String::new();

    for cap in re.captures_iter(text) {
        let mat = cap.get(0).unwrap();
        plain_text.push_str(&text[last_end..mat.start()]);
        last_end = mat.end();

        let json_str = cap.get(1).unwrap().as_str();
        if let Ok(val) = serde_json::from_str::<Value>(json_str) {
            tool_calls.push(val);
        }
    }
    plain_text.push_str(&text[last_end..]);

    (tool_calls, plain_text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_messages_as_prompt() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "You are helpful."}),
            serde_json::json!({"role": "user", "content": "Hello"}),
        ];
        let prompt = format_messages_as_prompt(&messages, None);
        assert!(prompt.contains("[system]"));
        assert!(prompt.contains("[user]"));
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_extract_tool_calls() {
        let text = r#"I'll search for you.
<tool_call>{"id":"call_1","type":"function","function":{"name":"web_search","arguments":"{\"query\":\"rust\"}"}}</tool_call>
Done."#;
        let (calls, plain) = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert!(plain.contains("I'll search for you."));
        assert!(plain.contains("Done."));
    }
}
