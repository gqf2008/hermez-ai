//! ACP Client — JSON-RPC client for connecting to an ACP server.
//!
//! Provides:
//! - Stdio transport (spawn server process, communicate over stdin/stdout)
//! - TCP transport (connect to remote ACP server)
//! - High-level methods: initialize, newSession, prompt, etc.
//!
//! Used for end-to-end testing and multi-agent orchestration.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;

use crate::protocol::*;

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// A transport for sending/receiving JSON-RPC messages.
#[async_trait::async_trait]
pub trait AcpTransport: Send + Sync {
    /// Send a JSON-RPC request and wait for the response.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, AcpClientError>;
    /// Subscribe to server notifications.
    async fn notifications(&mut self) -> Result<mpsc::UnboundedReceiver<Value>, AcpClientError>;
    /// Close the transport.
    async fn close(&mut self);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during ACP client operation.
#[derive(Debug, thiserror::Error)]
pub enum AcpClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Server error: {0}")]
    Server(String),

    #[error("Request timeout")]
    Timeout,

    #[error("Transport closed")]
    TransportClosed,

    #[error("Invalid response: {0}")]
    InvalidResponse(String),
}

// ---------------------------------------------------------------------------
// Stdio transport
// ---------------------------------------------------------------------------

/// Spawn a local ACP server process and communicate over stdin/stdout.
pub struct StdioTransport {
    stdin: ChildStdin,
    stdout_reader: BufReader<ChildStdout>,
    request_id: AtomicU64,
    response_rx: mpsc::UnboundedReceiver<(u64, Result<Value, AcpClientError>)>,
    _child: tokio::process::Child,
}

impl StdioTransport {
    /// Spawn a new ACP server process and create a stdio transport.
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, AcpClientError> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpClientError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "failed to open stdin",
            )))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpClientError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "failed to open stdout",
            )))?;

        let stdout_reader = BufReader::new(stdout);
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        let transport = Self {
            stdin,
            stdout_reader,
            request_id: AtomicU64::new(1),
            response_rx,
            _child: child,
        };

        // Start reader task
        // Note: in a real implementation we'd spawn a task here to read lines
        // and route responses by request ID. For simplicity in this MVP we
        // read synchronously per-request.

        Ok(transport)
    }

    /// Read a single line from stdout.
    async fn read_line(&mut self) -> Result<String, AcpClientError> {
        let mut line = String::new();
        let n = self.stdout_reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(AcpClientError::TransportClosed);
        }
        Ok(line)
    }
}

#[async_trait::async_trait]
impl AcpTransport for StdioTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, AcpClientError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&req)?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        // Read response lines until we find one with matching id
        loop {
            let line = self.read_line().await?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // Skip non-JSON lines (e.g., log output)
            };

            // Check if it's a notification (no id)
            if value.get("id").is_none() {
                continue;
            }

            // Check id match
            if let Some(resp_id) = value.get("id").and_then(|v| v.as_u64()) {
                if resp_id == id {
                    if let Some(error) = value.get("error") {
                        let msg = error
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string();
                        return Err(AcpClientError::Server(msg));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    async fn notifications(&mut self) -> Result<mpsc::UnboundedReceiver<Value>, AcpClientError> {
        // For MVP, notifications are not streamed separately.
        // In production, spawn a background task to read all lines and route
        // notifications to this channel.
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx; // silence unused warning in MVP
        Ok(rx)
    }

    async fn close(&mut self) {
        let _ = self.stdin.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// TCP transport
// ---------------------------------------------------------------------------

/// Connect to a remote ACP server over TCP.
pub struct TcpTransport {
    stream: tokio::net::TcpStream,
    request_id: AtomicU64,
    read_buf: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: tokio::net::tcp::OwnedWriteHalf,
}

impl TcpTransport {
    /// Connect to an ACP server at the given address.
    pub async fn connect(addr: &str) -> Result<Self, AcpClientError> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let (read_half, write_half) = stream.into_split();
        let read_buf = BufReader::new(read_half);

        Ok(Self {
            stream: tokio::net::TcpStream::connect(addr).await?, // reconnect for storage
            request_id: AtomicU64::new(1),
            read_buf,
            write_half,
        })
    }

    async fn read_line(&mut self) -> Result<String, AcpClientError> {
        let mut line = String::new();
        let n = self.read_buf.read_line(&mut line).await?;
        if n == 0 {
            return Err(AcpClientError::TransportClosed);
        }
        Ok(line)
    }
}

#[async_trait::async_trait]
impl AcpTransport for TcpTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, AcpClientError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&req)?;
        self.write_half.write_all(line.as_bytes()).await?;
        self.write_half.write_all(b"\n").await?;
        self.write_half.flush().await?;

        loop {
            let line = self.read_line().await?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if value.get("id").is_none() {
                continue;
            }

            if let Some(resp_id) = value.get("id").and_then(|v| v.as_u64()) {
                if resp_id == id {
                    if let Some(error) = value.get("error") {
                        let msg = error
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string();
                        return Err(AcpClientError::Server(msg));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    async fn notifications(&mut self) -> Result<mpsc::UnboundedReceiver<Value>, AcpClientError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx;
        Ok(rx)
    }

    async fn close(&mut self) {
        let _ = self.write_half.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// High-level client
// ---------------------------------------------------------------------------

/// High-level ACP client with typed method wrappers.
pub struct AcpClient<T: AcpTransport> {
    transport: T,
}

impl<T: AcpTransport> AcpClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Initialize the connection with the server.
    pub async fn initialize(
        &mut self,
        client_info: Implementation,
    ) -> Result<InitializeResponse, AcpClientError> {
        let params = serde_json::json!({
            "clientInfo": {
                "name": client_info.name,
                "version": client_info.version,
            }
        });
        let result = self.transport.request("initialize", params).await?;
        let resp: InitializeResponse = serde_json::from_value(result)?;
        Ok(resp)
    }

    /// Create a new session.
    pub async fn new_session(&mut self, cwd: &str) -> Result<NewSessionResponse, AcpClientError> {
        let params = serde_json::json!({ "cwd": cwd });
        let result = self.transport.request("newSession", params).await?;
        let resp: NewSessionResponse = serde_json::from_value(result)?;
        Ok(resp)
    }

    /// Send a prompt to a session.
    pub async fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<PromptResponse, AcpClientError> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        });
        let result = self.transport.request("prompt", params).await?;
        let resp: PromptResponse = serde_json::from_value(result)?;
        Ok(resp)
    }

    /// List active sessions.
    pub async fn list_sessions(&mut self) -> Result<ListSessionsResponse, AcpClientError> {
        let result = self.transport.request("listSessions", Value::Object(Default::default())).await?;
        let resp: ListSessionsResponse = serde_json::from_value(result)?;
        Ok(resp)
    }

    /// Cancel a session.
    pub async fn cancel(&mut self, session_id: &str) -> Result<(), AcpClientError> {
        let params = serde_json::json!({ "sessionId": session_id });
        self.transport.request("cancel", params).await?;
        Ok(())
    }

    /// Fork a session.
    pub async fn fork_session(
        &mut self,
        session_id: &str,
        cwd: &str,
    ) -> Result<ForkSessionResponse, AcpClientError> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "cwd": cwd,
        });
        let result = self.transport.request("forkSession", params).await?;
        let resp: ForkSessionResponse = serde_json::from_value(result)?;
        Ok(resp)
    }

    /// Close the transport.
    pub async fn close(&mut self) {
        self.transport.close().await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock transport for testing the client without a real server.
    struct MockTransport {
        responses: std::collections::VecDeque<Result<Value, AcpClientError>>,
    }

    #[async_trait::async_trait]
    impl AcpTransport for MockTransport {
        async fn request(&mut self, _method: &str, _params: Value) -> Result<Value, AcpClientError> {
            self.responses.pop_front().unwrap_or(Err(AcpClientError::TransportClosed))
        }

        async fn notifications(&mut self) -> Result<mpsc::UnboundedReceiver<Value>, AcpClientError> {
            let (_, rx) = mpsc::unbounded_channel();
            Ok(rx)
        }

        async fn close(&mut self) {}
    }

    #[tokio::test]
    async fn test_client_initialize() {
        let transport = MockTransport {
            responses: std::collections::VecDeque::from([Ok(serde_json::json!({
                "protocol_version": 1,
                "agent_info": { "name": "hermes-agent", "version": "0.1.0" },
            }))]),
        };
        let mut client = AcpClient::new(transport);
        let resp = client
            .initialize(Implementation {
                name: "test-client".into(),
                version: "1.0.0".into(),
            })
            .await
            .unwrap();
        assert_eq!(resp.protocol_version, 1);
        assert_eq!(resp.agent_info.name, "hermes-agent");
    }

    #[tokio::test]
    async fn test_client_new_session() {
        let transport = MockTransport {
            responses: std::collections::VecDeque::from([Ok(serde_json::json!({
                "session_id": "sess-123",
            }))]),
        };
        let mut client = AcpClient::new(transport);
        let resp = client.new_session("/tmp").await.unwrap();
        assert_eq!(resp.session_id, "sess-123");
    }

    #[tokio::test]
    async fn test_client_prompt() {
        let transport = MockTransport {
            responses: std::collections::VecDeque::from([Ok(serde_json::json!({
                "stop_reason": "end_turn",
            }))]),
        };
        let mut client = AcpClient::new(transport);
        let resp = client.prompt("sess-123", "hello").await.unwrap();
        assert_eq!(resp.stop_reason, Some("end_turn".to_string()));
    }

    #[tokio::test]
    async fn test_client_server_error() {
        let transport = MockTransport {
            responses: std::collections::VecDeque::from([Err(AcpClientError::Server(
                "session not found".into(),
            ))]),
        };
        let mut client = AcpClient::new(transport);
        let err = client.new_session("/tmp").await.unwrap_err();
        assert!(err.to_string().contains("session not found"));
    }
}
