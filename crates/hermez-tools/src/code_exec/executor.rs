//! Code execution — spawn Python subprocess with RPC transport.

use std::process::{Command, Stdio};
use std::sync::Arc;

use hermez_core::errors::ErrorCategory;

use super::sandbox;
use super::transport;
use crate::registry::ToolRegistry;

/// Max stdout returned to LLM (50KB).
const MAX_STDOUT: usize = 50 * 1024;

/// Max stderr returned to LLM (10KB).
const MAX_STDERR: usize = 10 * 1024;

/// Max tool calls per code execution session.
pub const MAX_TOOL_CALLS: u64 = transport::MAX_TOOL_CALLS;

/// Execute Python code with RPC transport for tool calls.
///
/// On Unix: uses UDS transport with a background RPC server.
/// On Windows: falls back to file-based transport (no UDS support).
pub fn execute_python_code(
    code: &str,
    python: &str,
    enabled_tools: &[serde_json::Value],
    _task_id: Option<&str>,
    registry: Arc<ToolRegistry>,
) -> Result<String, hermez_core::HermezError> {
    // Security: basic validation
    for dangerous in [
        "import os.system",
        "import subprocess",
        "__import__('os')",
    ] {
        if code.contains(dangerous) {
            return Ok(crate::registry::tool_error(format!(
                "Code contains disallowed import: '{dangerous}'. Use the sandbox tools instead."
            )));
        }
    }

    // Determine enabled tool names
    let tool_names: Vec<String> = if enabled_tools.is_empty() {
        sandbox::SANDBOX_TOOLS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect()
    } else {
        enabled_tools
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    };

    // Choose transport mode
    #[cfg(unix)]
    return execute_with_uds(code, python, &tool_names, registry);

    #[cfg(not(unix))]
    return execute_with_file_rpc(code, python, &tool_names, registry);
}

/// Execute with UDS transport (Unix only).
#[cfg(unix)]
fn execute_with_uds(
    code: &str,
    python: &str,
    tool_names: &[String],
    registry: Arc<ToolRegistry>,
) -> Result<String, hermez_core::HermezError> {
    // Generate hermez_tools.py module
    let module_code = sandbox::generate_hermez_tools_module(tool_names, "uds");

    // Create UDS server
    let server = transport::UdsServer::new().map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to create UDS server: {e}"),
        )
    })?;

    let socket_path = server.socket_path_str();
    let server_running = server.running.clone();
    let _call_count = server.call_count.clone();

    // Wire the handler to the ToolRegistry dispatch
    let handler: transport::ToolHandler = Arc::new(move |tool_name: &str, args: serde_json::Value| {
        match registry.dispatch(tool_name, args) {
            Ok(result) => result,
            Err(e) => crate::registry::tool_error(e.to_string()),
        }
    });

    let _server_handle = transport::start_uds_server(server, tool_names.to_vec(), handler)
        .map_err(|e| {
            hermez_core::HermezError::new(
                ErrorCategory::ToolError,
                format!("Failed to start UDS server: {e}"),
            )
        })?;

    // Create a temp sandbox directory
    let sandbox_dir = std::env::temp_dir().join(format!(
        "hermez_sandbox_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&sandbox_dir).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to create sandbox directory: {e}"),
        )
    })?;

    // Write hermez_tools.py and script.py
    std::fs::write(sandbox_dir.join("hermez_tools.py"), &module_code).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to write hermez_tools.py: {e}"),
        )
    })?;

    std::fs::write(sandbox_dir.join("script.py"), code).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to write script.py: {e}"),
        )
    })?;

    // Build sanitized environment
    let mut child_env: Vec<(String, String)> = sandbox::sanitize_env().into_iter().collect();
    child_env.push(("HERMEZ_RPC_SOCKET".to_string(), socket_path.clone()));
    child_env.push(("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()));

    // Add sandbox dir to PYTHONPATH
    let pythonpath = sandbox_dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        child_env.push(("PYTHONPATH".to_string(), format!("{pythonpath}:{existing}")));
    } else {
        child_env.push(("PYTHONPATH".to_string(), pythonpath));
    }

    // Execute
    let output = Command::new(python)
        .arg("script.py")
        .current_dir(&sandbox_dir)
        .envs(child_env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            hermez_core::HermezError::new(ErrorCategory::ToolError, format!("Failed to run Python: {e}"))
        })?;

    // Shut down UDS server
    server_running.store(false, std::sync::atomic::Ordering::Relaxed);

    // Clean up
    let _ = std::fs::remove_dir_all(&sandbox_dir);

    // Process output
    process_output(&output)
}

/// Execute with file-based RPC transport (Windows fallback).
#[cfg(not(unix))]
fn execute_with_file_rpc(
    code: &str,
    python: &str,
    tool_names: &[String],
    registry: Arc<ToolRegistry>,
) -> Result<String, hermez_core::HermezError> {
    // Generate hermez_tools.py module
    let module_code = sandbox::generate_hermez_tools_module(tool_names, "file");

    // Create file RPC state
    let rpc = transport::FileRpc::new().map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to create RPC directory: {e}"),
        )
    })?;

    let rpc_dir = rpc.rpc_dir_str();

    // Wire the handler to the ToolRegistry dispatch
    let handler: transport::ToolHandler = Arc::new(move |tool_name: &str, args: serde_json::Value| {
        match registry.dispatch(tool_name, args) {
            Ok(result) => result,
            Err(e) => crate::registry::tool_error(e.to_string()),
        }
    });

    // Start the file RPC polling server
    let _server_handle = transport::start_file_rpc_poll(
        rpc,
        tool_names.to_vec(),
        handler,
    ).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to start file RPC server: {e}"),
        )
    })?;

    // Create a temp sandbox directory
    let sandbox_dir = std::env::temp_dir().join(format!(
        "hermez_sandbox_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&sandbox_dir).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to create sandbox directory: {e}"),
        )
    })?;

    // Write hermez_tools.py and script.py
    std::fs::write(sandbox_dir.join("hermez_tools.py"), &module_code).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to write hermez_tools.py: {e}"),
        )
    })?;

    std::fs::write(sandbox_dir.join("script.py"), code).map_err(|e| {
        hermez_core::HermezError::new(
            ErrorCategory::ToolError,
            format!("Failed to write script.py: {e}"),
        )
    })?;

    // Build sanitized environment
    let mut child_env: Vec<(String, String)> = sandbox::sanitize_env().into_iter().collect();
    child_env.push(("HERMEZ_RPC_DIR".to_string(), rpc_dir));
    child_env.push(("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()));

    // Add sandbox dir to PYTHONPATH
    let pythonpath = sandbox_dir.to_string_lossy().to_string();
    child_env.push(("PYTHONPATH".to_string(), pythonpath));

    // Execute
    let output = Command::new(python)
        .arg("script.py")
        .current_dir(&sandbox_dir)
        .envs(child_env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            hermez_core::HermezError::new(ErrorCategory::ToolError, format!("Failed to run Python: {e}"))
        })?;

    // Clean up
    let _ = std::fs::remove_dir_all(&sandbox_dir);

    // Process output
    process_output(&output)
}

/// Process subprocess output: truncate, strip ANSI, redact secrets.
fn process_output(output: &std::process::Output) -> Result<String, hermez_core::HermezError> {
    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Truncate
    if stdout.len() > MAX_STDOUT {
        stdout = format!(
            "{}\n... [{} bytes truncated]",
            &stdout[..MAX_STDOUT],
            stdout.len()
        );
    }
    if stderr.len() > MAX_STDERR {
        stderr = format!(
            "{}\n... [{} bytes truncated]",
            &stderr[..MAX_STDERR],
            stderr.len()
        );
    }

    // Strip ANSI
    stdout = crate::ansi_strip::strip_ansi(&stdout);
    stderr = crate::ansi_strip::strip_ansi(&stderr);

    let exit_code = output.status.code().unwrap_or(-1);

    Ok(serde_json::json!({
        "success": exit_code == 0,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_output_truncation() {
        let big_stdout = "x".repeat(MAX_STDOUT + 100);
        let small_stderr = "error".to_string();

        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: big_stdout.into_bytes(),
            stderr: small_stderr.into_bytes(),
        };

        let result = process_output(&output).unwrap();
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(json["stdout"].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn test_process_output_ansi_stripped() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"\x1b[31mred\x1b[0m".to_vec(),
            stderr: Vec::new(),
        };

        let result = process_output(&output).unwrap();
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["stdout"].as_str().unwrap(), "red");
    }

    #[test]
    fn test_dangerous_import_check() {
        // This test goes through the full path
        let registry = Arc::new(ToolRegistry::new());
        let result = execute_python_code(
            "import os.system('rm -rf /')",
            "python3",
            &[],
            None,
            registry,
        );
        let json: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }
}
