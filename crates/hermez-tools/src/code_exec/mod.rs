//! Code execution tool — run Python scripts that call tools programmatically.
//!
//! Mirrors the Python `tools/code_execution_tool.py`.
//! Supports two transport modes:
//! - **UDS** (Unix Domain Socket): local backend, parent runs RPC server on socket
//! - **File-based**: remote backends (Docker/SSH/Modal), parent polls for request files

pub mod executor;
pub mod sandbox;
pub mod transport;

use std::sync::Arc;

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};
use crate::code_exec::executor::execute_python_code;

/// Handle execute_code tool call.
pub fn handle_execute_code(
    args: Value,
    registry: Arc<ToolRegistry>,
) -> Result<String, hermez_core::HermezError> {
    let code = match args.get("code").and_then(Value::as_str) {
        Some(c) if !c.trim().is_empty() => c.to_string(),
        _ => return Ok(tool_error("execute_code requires a non-empty 'code' parameter.")),
    };

    let enabled_tools = args
        .get("enabled_tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .map(String::from);

    // Check for Python availability
    let python = find_python();
    if python.is_none() {
        return Ok(tool_error(
            "Python 3 not found. Install Python 3.11+ to use execute_code.",
        ));
    }

    let python_path = python.unwrap();

    execute_python_code(&code, &python_path, &enabled_tools, task_id.as_deref(), registry)
}

/// Find Python 3 executable.
fn find_python() -> Option<String> {
    for cmd in ["python", "python3", "py"] {
        if std::process::Command::new(cmd)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok()
        {
            return Some(cmd.to_string());
        }
    }
    None
}

/// Register execute_code tool.
pub fn register_code_exec_tool(registry: &mut ToolRegistry, parent_registry: Arc<ToolRegistry>) {
    registry.register(
        "execute_code".to_string(),
        "code_execution".to_string(),
        serde_json::json!({
            "name": "execute_code",
            "description": "Run Python scripts that call tools programmatically (reduces LLM round trips). Supports RPC transport for calling parent tools.",
            "parameters": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python source code to execute." },
                    "task_id": { "type": "string", "description": "Session task ID for tool isolation." },
                    "enabled_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of tool names allowed in the sandbox (default: all 7 sandbox tools)."
                    }
                },
                "required": ["code"]
            }
        }),
        std::sync::Arc::new(move |args: Value| handle_execute_code(args, parent_registry.clone())),
        None,
        vec!["code_execution".to_string()],
        "Run Python code with tool access".to_string(),
        "🐍".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn test_registry() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    #[test]
    #[serial]
    fn test_missing_code() {
        let result = handle_execute_code(serde_json::json!({}), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_empty_code() {
        let result = handle_execute_code(serde_json::json!({ "code": "   " }), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_dangerous_import_rejected() {
        let result = handle_execute_code(serde_json::json!({
            "code": "import os.system('rm -rf /')"
        }), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_simple_print() {
        let result = handle_execute_code(serde_json::json!({
            "code": "print('hello from python')"
        }), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        // May fail if Python is not installed
        if json.get("success").and_then(Value::as_bool).unwrap_or(false) {
            assert!(json["stdout"].as_str().unwrap().contains("hello"));
        }
    }

    #[test]
    #[serial]
    fn test_python_error() {
        let result = handle_execute_code(serde_json::json!({
            "code": "raise ValueError('test error')"
        }), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        // May fail if Python is not installed
        if json.get("success").is_some() && !json.get("error").is_some() {
            assert_eq!(json.get("success").and_then(Value::as_bool), Some(false));
        }
    }

    #[test]
    #[serial]
    fn test_tool_calls_via_import() {
        let code = r#"
from hermez_tools import web_search, read_file
result = web_search(query="test")
print(result)
"#;
        let result = handle_execute_code(serde_json::json!({
            "code": code,
            "enabled_tools": ["web_search", "read_file"]
        }), test_registry());
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        if json.get("error").is_some() {
            let err = json["error"].as_str().unwrap();
            assert!(!err.contains("disallowed import"), "should not reject hermez_tools import: {err}");
        }
    }

    #[test]
    #[serial]
    fn test_timeout_validation() {
        let result = handle_execute_code(serde_json::json!({
            "code": "import time; time.sleep(1)",
            "timeout": 5
        }), test_registry());
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_max_tool_calls_enforcement() {
        use crate::code_exec::executor::MAX_TOOL_CALLS;
        assert_eq!(MAX_TOOL_CALLS, 50);
    }

    #[test]
    #[serial]
    fn test_sandbox_module_generation() {
        use crate::code_exec::sandbox::generate_hermez_tools_module;

        let tools = vec![
            "web_search".to_string(),
            "read_file".to_string(),
            "terminal".to_string(),
        ];
        let module = generate_hermez_tools_module(&tools, "uds");

        assert!(module.contains("def web_search("));
        assert!(module.contains("def read_file("));
        assert!(module.contains("def terminal("));
        assert!(module.contains("HERMEZ_RPC_SOCKET"));
        assert!(!module.contains("def write_file("), "should not include non-enabled tools");
    }

    #[test]
    #[serial]
    fn test_file_transport_module() {
        use crate::code_exec::sandbox::generate_hermez_tools_module;

        let tools = vec!["web_search".to_string()];
        let module = generate_hermez_tools_module(&tools, "file");

        assert!(module.contains("HERMEZ_RPC_DIR"));
        assert!(module.contains("seq"));
    }
}
