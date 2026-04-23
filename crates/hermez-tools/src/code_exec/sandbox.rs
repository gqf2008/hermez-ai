//! Sandbox module generation — generates `hermez_tools.py` stub module.
//!
//! The generated module is shipped to the child Python process and provides
//! thin wrapper functions for each allowed tool. Calls are dispatched through
//! either UDS (local) or file-based RPC (remote).

/// Allowed sandbox tools.
pub const SANDBOX_TOOLS: &[(&str, &[(&str, &str)])] = &[
    (
        "web_search",
        &[("query", "str"), ("limit", "int = 5")],
    ),
    (
        "web_extract",
        &[("url", "str")],
    ),
    (
        "read_file",
        &[("path", "str")],
    ),
    (
        "write_file",
        &[("path", "str"), ("content", "str")],
    ),
    (
        "search_files",
        &[("pattern", "str"), ("path", "str = '.'")],
    ),
    (
        "patch",
        &[("path", "str"), ("patch", "str")],
    ),
    (
        "terminal",
        &[("command", "str"), ("timeout", "int = 60")],
    ),
];

/// Parameters forbidden for terminal tool in sandbox (parent manages these).
const FORBIDDEN_TERMINAL_PARAMS: &[&str] = &[
    "background", "check_interval", "pty", "notify_on_complete",
];

/// Generate the `hermez_tools.py` module source.
pub fn generate_hermez_tools_module(enabled_tools: &[String], transport: &str) -> String {
    let mut module = String::new();

    // Module header
    module.push_str("# Auto-generated hermez_tools module for code execution sandbox.\n");
    module.push_str("# Do not edit manually.\n\n");
    module.push_str("import json\n");
    module.push_str("import os\n");
    module.push_str("import time\n");

    if transport == "uds" {
        module.push_str("import socket\n\n");
    } else {
        module.push_str("import glob\n");
        module.push_str("import base64\n\n");
    }

    // JSON parse helper with double-decode
    module.push_str(
        r#"def _json_parse(text):
    """Parse JSON, handling double-encoded results."""
    try:
        result = json.loads(text)
        if isinstance(result, str):
            try:
                return json.loads(result)
            except (json.JSONDecodeError, TypeError):
                return result
        return result
    except (json.JSONDecodeError, TypeError):
        return text

""#,
    );

    // Transport-specific _call function
    if transport == "uds" {
        module.push_str(
            r#"_uds_cache = {}

def _connect():
    """Create UDS connection with caching."""
    sock_path = os.environ.get("HERMEZ_RPC_SOCKET")
    if not sock_path:
        raise RuntimeError("HERMEZ_RPC_SOCKET not set")
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(300)
    conn.connect(sock_path)
    return conn

def _call(tool_name, args):
    """Call a tool via UDS transport."""
    conn = _connect()
    try:
        request = json.dumps({"tool": tool_name, "args": args}) + "\n"
        conn.sendall(request.encode("utf-8"))

        # Read response until newline
        buf = b""
        while b"\n" not in buf:
            chunk = conn.recv(65536)
            if not chunk:
                raise RuntimeError("Connection closed before response")
            buf += chunk

        return _json_parse(buf.decode("utf-8").strip())
    finally:
        try:
            conn.close()
        except Exception:
            pass

"#,
        );
    } else {
        module.push_str(
            r#"_seq = 0

def _call(tool_name, args):
    """Call a tool via file-based RPC transport."""
    global _seq
    rpc_dir = os.environ.get("HERMEZ_RPC_DIR")
    if not rpc_dir:
        raise RuntimeError("HERMEZ_RPC_DIR not set")

    _seq += 1
    seq = f"{_seq:06d}"
    req_file = os.path.join(rpc_dir, f"req_{seq}")
    req_tmp = f"{req_file}.tmp"
    res_file = os.path.join(rpc_dir, f"res_{seq}")

    # Write request atomically
    request = json.dumps({"tool": tool_name, "args": args, "seq": _seq})
    with open(req_tmp, "w") as f:
        f.write(request)
    os.rename(req_tmp, req_file)

    # Poll for response with adaptive backoff
    deadline = time.time() + 300
    delay = 0.05
    while time.time() < deadline:
        if os.path.exists(res_file):
            with open(res_file, "r") as f:
                response = f.read()
            os.unlink(res_file)
            return _json_parse(response)
        time.sleep(delay)
        delay = min(delay * 1.2, 0.25)

    raise RuntimeError(f"RPC timeout waiting for response to req_{seq}")

"#,
        );
    }

    // Generate wrapper functions for each enabled tool
    for &(tool_name, params) in SANDBOX_TOOLS {
        if !enabled_tools.iter().any(|t| t == tool_name) {
            continue;
        }

        // Build function signature
        let param_list: Vec<String> = params
            .iter()
            .map(|(name, ty)| format!("{name}: {ty}"))
            .collect();

        module.push_str(&format!(
            "def {tool_name}({}):\n",
            param_list.join(", ")
        ));

        // Build args dict
        let param_names: Vec<&str> = params.iter().map(|(name, _)| *name).collect();
        module.push_str(&format!(
            "    \"\"\"Call the {tool_name} tool.\"\"\"\n"
        ));
        module.push_str(&format!(
            "    args = {{{}}}\n",
            param_names
                .iter()
                .map(|n| format!("\"{n}\": {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));

        // Filter forbidden terminal params
        if tool_name == "terminal" {
            for &param in FORBIDDEN_TERMINAL_PARAMS {
                module.push_str(&format!(
                    "    args.pop(\"{param}\", None)\n"
                ));
            }
        }

        module.push_str(&format!("    return _call(\"{tool_name}\", args)\n\n"));
    }

    module
}

/// Sanitize environment variables for the child process.
/// Strips secret-like env vars, keeps safe prefixes.
pub fn sanitize_env() -> std::collections::HashMap<String, String> {
    let safe_prefixes = &[
        "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "SHELL", "TMPDIR",
        "XDG_", "PYTHON", "LOGNAME", "HOSTNAME", "PWD",
    ];

    std::env::vars()
        .filter(|(key, _)| {
            // Block secret-like env vars
            let upper = key.to_uppercase();
            if upper.contains("KEY")
                || upper.contains("TOKEN")
                || upper.contains("SECRET")
                || upper.contains("PASSWORD")
                || upper.contains("API_KEY")
                || upper.contains("PRIVATE")
            {
                return false;
            }

            // Allow safe prefixes
            safe_prefixes.iter().any(|prefix| upper.starts_with(*prefix))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_module_uds() {
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
        assert!(!module.contains("def write_file("));
    }

    #[test]
    fn test_generate_module_file() {
        let tools = vec!["web_search".to_string()];
        let module = generate_hermez_tools_module(&tools, "file");

        assert!(module.contains("HERMEZ_RPC_DIR"));
        assert!(module.contains("seq"));
        assert!(module.contains("_seq += 1"));
    }

    #[test]
    fn test_generate_module_all_tools() {
        let tools: Vec<String> = SANDBOX_TOOLS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        let module = generate_hermez_tools_module(&tools, "uds");

        for (name, _) in SANDBOX_TOOLS {
            assert!(
                module.contains(&format!("def {name}(")),
                "missing function: {name}"
            );
        }
    }

    #[test]
    fn test_generate_module_empty_tools() {
        let module = generate_hermez_tools_module(&[], "uds");
        assert!(module.contains("_call"));
        assert!(!module.contains("def web_search("));
    }

    #[test]
    fn test_sanitize_env() {
        let env = sanitize_env();

        // Should not contain secret-like vars
        for key in env.keys() {
            let upper = key.to_uppercase();
            assert!(
                !upper.contains("KEY") && !upper.contains("TOKEN") && !upper.contains("SECRET"),
                "should have stripped secret-like env var: {key}"
            );
        }
    }

    #[test]
    fn test_terminal_forbidden_params_filtered() {
        let tools = vec!["terminal".to_string()];
        let module = generate_hermez_tools_module(&tools, "uds");

        assert!(module.contains("args.pop(\"background\", None)"));
        assert!(module.contains("args.pop(\"pty\", None)"));
    }
}
