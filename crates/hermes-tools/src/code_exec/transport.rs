//! RPC transport layer for code execution sandbox.
//!
//! Two modes:
//! - **UDS**: Unix Domain Socket server, parent listens, child connects
//! - **File-based**: Parent polls a shared directory for request files

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::registry::tool_error;

/// Max tool calls per code execution session.
pub const MAX_TOOL_CALLS: u64 = 50;

/// UDS RPC server state.
#[cfg(unix)]
pub struct UdsServer {
    /// Path to the Unix socket.
    pub socket_path: PathBuf,
    /// Whether the server is running.
    pub running: Arc<AtomicBool>,
    /// Tool call counter.
    pub call_count: Arc<AtomicU64>,
}

#[cfg(unix)]
impl UdsServer {
    /// Create a new UDS server with a unique socket path.
    pub fn new() -> std::io::Result<Self> {
        let socket_path = std::env::temp_dir().join(format!(
            "hermes_rpc_{}.sock",
            std::process::id()
        ));

        Ok(Self {
            socket_path,
            running: Arc::new(AtomicBool::new(false)),
            call_count: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Get the socket path as a string.
    pub fn socket_path_str(&self) -> String {
        self.socket_path.to_string_lossy().to_string()
    }
}

#[cfg(unix)]
impl Drop for UdsServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// File-based RPC state.
pub struct FileRpc {
    /// Shared directory for request/response files.
    pub rpc_dir: PathBuf,
    /// Whether the poll loop is running.
    pub running: Arc<AtomicBool>,
    /// Tool call counter.
    pub call_count: Arc<AtomicU64>,
}

impl FileRpc {
    /// Create a new file-based RPC state with a temp directory.
    pub fn new() -> std::io::Result<Self> {
        let rpc_dir = std::env::temp_dir().join(format!(
            "hermes_rpc_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&rpc_dir)?;

        Ok(Self {
            rpc_dir,
            running: Arc::new(AtomicBool::new(false)),
            call_count: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Get the RPC directory as a string.
    pub fn rpc_dir_str(&self) -> String {
        self.rpc_dir.to_string_lossy().to_string()
    }
}

impl Drop for FileRpc {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&self.rpc_dir);
    }
}

/// Parse a single RPC request from JSON.
pub fn parse_request(json_str: &str) -> Result<(String, Value), String> {
    let value: Value = serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {e}"))?;

    let tool = value
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing 'tool' field in request".to_string())?
        .to_string();

    let args = value
        .get("args")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    Ok((tool, args))
}

/// Format an RPC response.
pub fn format_response(result: &str) -> String {
    format!("{result}\n")
}

/// Check if a tool call is within the allow-list.
pub fn is_tool_allowed(tool: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|t| t == tool)
}

/// Strip forbidden terminal params from a tool call.
pub fn strip_terminal_params(args: &mut Value) {
    if let Value::Object(map) = args {
        for param in ["background", "check_interval", "pty", "notify_on_complete"] {
            map.remove(param);
        }
    }
}

/// Tool call handler type — dispatched by the RPC server.
pub type ToolHandler = Arc<dyn Fn(&str, Value) -> String + Send + Sync>;

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;

    /// Start the UDS RPC server loop in a background thread.
    ///
    /// Listens on the socket and dispatches tool calls through the handler.
    /// Returns a handle to shut down the server.
    pub fn start_uds_server(
        server: UdsServer,
        allowed_tools: Vec<String>,
        handler: ToolHandler,
    ) -> std::io::Result<thread::JoinHandle<()>> {
        // Remove stale socket
        let _ = std::fs::remove_file(&server.socket_path);

        let listener = UnixListener::bind(&server.socket_path)?;
        listener.set_nonblocking(true)?;

        let running = server.running.clone();
        let call_count = server.call_count.clone();

        running.store(true, Ordering::Relaxed);

        let handle = thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let count = handle_uds_connection(
                            &mut stream,
                            &allowed_tools,
                            &handler,
                            &call_count,
                        );
                        if let Err(e) = count {
                            tracing::error!("UDS connection error: {}", e);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Handle a single UDS connection.
    fn handle_uds_connection(
        stream: &mut UnixStream,
        allowed_tools: &[String],
        handler: &ToolHandler,
        call_count: &AtomicU64,
    ) -> std::io::Result<()> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(300)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;

        let reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream.try_clone()?;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            // Check tool call limit
            let count = call_count.fetch_add(1, Ordering::Relaxed);
            if count >= MAX_TOOL_CALLS {
                let resp = tool_error("Maximum tool call limit (50) exceeded");
                writeln!(writer, "{}", resp)?;
                break;
            }

            match parse_request(&line) {
                Ok((tool, mut args)) => {
                    if !is_tool_allowed(&tool, allowed_tools) {
                        let resp = tool_error(format!("Tool '{tool}' is not allowed in sandbox"));
                        writeln!(writer, "{}", resp)?;
                        continue;
                    }

                    // Strip forbidden terminal params
                    if tool == "terminal" {
                        strip_terminal_params(&mut args);
                    }

                    let result = handler(&tool, args);
                    writeln!(writer, "{}", format_response(&result))?;
                }
                Err(e) => {
                    let resp = tool_error(format!("Invalid request: {e}"));
                    writeln!(writer, "{}", resp)?;
                }
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
pub use unix_impl::start_uds_server;

/// Poll loop for file-based RPC.
///
/// Runs in a background thread, polling for request files and dispatching
/// tool calls. Used for remote backends (Docker, SSH, Modal).
pub fn start_file_rpc_poll(
    rpc: FileRpc,
    allowed_tools: Vec<String>,
    handler: ToolHandler,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let running = rpc.running.clone();
    let call_count = rpc.call_count.clone();
    let rpc_dir = rpc.rpc_dir.clone();

    running.store(true, Ordering::Relaxed);

    let handle = std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            // Find request files
            let pattern = rpc_dir.join("req_*");
            let pattern_str = pattern.to_string_lossy();

            // Use glob to find matching files
            let entries: Vec<_> = match glob::glob(&pattern_str) {
                Ok(paths) => paths.filter_map(|p| p.ok()).collect(),
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
            };

            for req_path in entries {
                // Skip .tmp files (partial writes)
                if req_path.extension().is_some_and(|e| e == "tmp") {
                    continue;
                }

                let req_str = match std::fs::read_to_string(&req_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                // Check tool call limit
                let count = call_count.fetch_add(1, Ordering::Relaxed);
                if count >= MAX_TOOL_CALLS {
                    let resp = tool_error("Maximum tool call limit (50) exceeded");
                    let seq = extract_seq(&req_str);
                    let res_path = rpc_dir.join(format!("res_{seq:06}"));
                    let _ = std::fs::write(&res_path, format_response(&resp));
                    let _ = std::fs::remove_file(&req_path);
                    continue;
                }

                match parse_request(&req_str) {
                    Ok((tool, mut args)) => {
                        if !is_tool_allowed(&tool, &allowed_tools) {
                            let resp =
                                tool_error(format!("Tool '{tool}' is not allowed in sandbox"));
                            let seq = extract_seq(&req_str);
                            let res_path = rpc_dir.join(format!("res_{seq:06}"));
                            let _ = std::fs::write(&res_path, format_response(&resp));
                            let _ = std::fs::remove_file(&req_path);
                            continue;
                        }

                        if tool == "terminal" {
                            strip_terminal_params(&mut args);
                        }

                        let result = handler(&tool, args);
                        let seq = extract_seq(&req_str);
                        let res_path = rpc_dir.join(format!("res_{seq:06}"));
                        let _ = std::fs::write(&res_path, format_response(&result));
                        let _ = std::fs::remove_file(&req_path);
                    }
                    Err(_) => {
                        // Invalid request — just delete it
                        let _ = std::fs::remove_file(&req_path);
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    Ok(handle)
}

/// Extract sequence number from a request string.
fn extract_seq(req_str: &str) -> u64 {
    serde_json::from_str::<Value>(req_str)
        .ok()
        .and_then(|v| v.get("seq").and_then(Value::as_u64))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_request_valid() {
        let json = r#"{"tool": "web_search", "args": {"query": "test", "limit": 5}}"#;
        let (tool, args) = parse_request(json).unwrap();
        assert_eq!(tool, "web_search");
        assert_eq!(args["query"], "test");
        assert_eq!(args["limit"], 5);
    }

    #[test]
    fn test_parse_request_missing_tool() {
        let json = r#"{"args": {}}"#;
        assert!(parse_request(json).is_err());
    }

    #[test]
    fn test_parse_request_invalid_json() {
        assert!(parse_request("not json").is_err());
    }

    #[test]
    fn test_format_response() {
        let result = r#"{"success": true}"#;
        let formatted = format_response(result);
        assert_eq!(formatted, r#"{"success": true}
"#);
    }

    #[test]
    fn test_is_tool_allowed() {
        let allowed = vec!["web_search".to_string(), "read_file".to_string()];
        assert!(is_tool_allowed("web_search", &allowed));
        assert!(!is_tool_allowed("nonexistent", &allowed));
    }

    #[test]
    fn test_strip_terminal_params() {
        let mut args = serde_json::json!({
            "command": "ls",
            "background": true,
            "pty": true,
            "timeout": 30
        });
        strip_terminal_params(&mut args);
        assert!(args.get("background").is_none());
        assert!(args.get("pty").is_none());
        assert!(args.get("timeout").is_some());
    }

    #[test]
    fn test_extract_seq() {
        let req = r#"{"tool": "test", "args": {}, "seq": 42}"#;
        assert_eq!(extract_seq(req), 42);
    }

    #[test]
    fn test_extract_seq_missing() {
        let req = r#"{"tool": "test", "args": {}}"#;
        assert_eq!(extract_seq(req), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_uds_server_creation() {
        let server = UdsServer::new().unwrap();
        assert!(server.socket_path_str().contains("hermes_rpc_"));
    }

    #[test]
    fn test_file_rpc_creation() {
        let rpc = FileRpc::new().unwrap();
        assert!(rpc.rpc_dir_str().contains("hermes_rpc_"));
    }
}
