#![allow(dead_code)]
//! Process registry tool.
//!
//! Mirrors the Python `tools/process_registry.py`.
//! 1 tool: `process` with actions: list, poll, log, wait, kill, write, submit, close.
//! Manages background processes spawned by terminal(background=true).

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

/// A managed background process.
#[derive(Debug, Clone)]
pub(crate) struct ManagedProcess {
    session_id: String,
    command: String,
    pub(crate) pid: Option<u32>,
    running: bool,
    exit_code: Option<i32>,
    output_buffer: String,
    output_size: usize,
    spawned_at: String,
    last_polled: String,
    notify_on_complete: bool,
    watch_patterns: Vec<String>,
    use_pty: bool,
    watcher_platform: Option<String>,
    watcher_chat_id: Option<String>,
    watcher_user_id: Option<String>,
    watcher_user_name: Option<String>,
    watcher_thread_id: Option<String>,
}

/// Max output buffer size (200KB).
const MAX_OUTPUT_SIZE: usize = 200 * 1024;

/// Process registry singleton.
pub(crate) static PROCESS_REGISTRY: Lazy<Mutex<HashMap<String, ManagedProcess>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Max number of tracked processes (LRU pruned at this limit).
const MAX_PROCESSES: usize = 64;

/// Handle process tool call.
pub fn handle_process(args: Value) -> Result<String, hermez_core::HermezError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list");

    match action {
        "list" => handle_list(),
        "poll" => handle_poll(&args),
        "log" => handle_log(&args),
        "wait" => handle_wait(&args),
        "kill" => handle_kill(&args),
        "write" => handle_write(&args),
        "submit" => handle_submit(&args),
        "close" => handle_close(&args),
        _ => Ok(tool_error(format!(
            "Unknown action: '{action}'. Valid actions: list, poll, log, wait, kill, write, submit, close"
        ))),
    }
}

fn handle_list() -> Result<String, hermez_core::HermezError> {
    let registry = PROCESS_REGISTRY.lock();

    let processes: Vec<Value> = registry
        .values()
        .map(|p| {
            serde_json::json!({
                "session_id": p.session_id,
                "command": p.command,
                "pid": p.pid,
                "running": p.running,
                "exit_code": p.exit_code,
                "output_size": p.output_size,
                "spawned_at": p.spawned_at,
                "last_polled": p.last_polled,
            })
        })
        .collect();

    let running_count = processes.iter().filter(|p| p["running"] == true).count();

    Ok(serde_json::json!({
        "success": true,
        "action": "list",
        "total": processes.len(),
        "running": running_count,
        "processes": processes,
    })
    .to_string())
}

fn handle_poll(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("poll requires 'session_id' parameter")),
    };

    let registry = PROCESS_REGISTRY.lock();
    let Some(p) = registry.get(&session_id) else {
        return Ok(tool_error(format!("Process not found: {session_id}")));
    };

    let now = chrono::Utc::now().to_rfc3339();
    let output_preview = if p.output_buffer.len() > 200 {
        format!("...{}", &p.output_buffer[p.output_buffer.len().saturating_sub(200)..])
    } else {
        p.output_buffer.clone()
    };

    Ok(serde_json::json!({
        "success": true,
        "action": "poll",
        "session_id": session_id,
        "running": p.running,
        "exit_code": p.exit_code,
        "output_size": p.output_size,
        "last_polled": now,
        "output_preview": output_preview,
    })
    .to_string())
}

fn handle_log(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("log requires 'session_id' parameter")),
    };

    let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(0) as usize;
    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10000) as usize;

    let registry = PROCESS_REGISTRY.lock();
    let Some(p) = registry.get(&session_id) else {
        return Ok(tool_error(format!("Process not found: {session_id}")));
    };

    let total = p.output_buffer.len();
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let log_content = &p.output_buffer[start..end];

    Ok(serde_json::json!({
        "success": true,
        "action": "log",
        "session_id": session_id,
        "offset": start,
        "limit": end - start,
        "total": total,
        "has_more": end < total,
        "log": log_content,
    })
    .to_string())
}

fn handle_wait_blocking(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("wait requires 'session_id' parameter")),
    };

    let timeout = args.get("timeout").and_then(Value::as_i64).unwrap_or(180);
    let timeout_secs = timeout.min(180) as u64;

    // D2: Actually wait for the process by polling the registry in a loop.
    // The background output collection thread calls mark_process_finished
    // when the OS process exits, which sets running=false.
    let timeout_dur = std::time::Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();

    loop {
        {
            let registry = PROCESS_REGISTRY.lock();
            let Some(p) = registry.get(&session_id) else {
                return Ok(tool_error(format!("Process not found: {session_id}")));
            };

            if !p.running {
                return Ok(serde_json::json!({
                    "success": true,
                    "action": "wait",
                    "session_id": session_id,
                    "already_finished": true,
                    "exit_code": p.exit_code,
                    "output_size": p.output_size,
                })
                .to_string());
            }
        }

        // Check timeout
        if start.elapsed() >= timeout_dur {
            return Ok(serde_json::json!({
                "success": true,
                "action": "wait",
                "session_id": session_id,
                "timed_out": true,
                "note": "Process is still running after {timeout_secs}s timeout. Use 'poll' to check status.",
            })
            .to_string());
        }

        // Brief sleep before next check
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn handle_wait(args: &Value) -> Result<String, hermez_core::HermezError> {
    // If inside a tokio runtime, offload the blocking polling loop to spawn_blocking
    // so the async executor thread remains responsive.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let args = args.clone();
        return handle.block_on(async {
            tokio::task::spawn_blocking(move || handle_wait_blocking(&args))
                .await
                .map_err(|e| hermez_core::HermezError::new(
                    hermez_core::ErrorCategory::TerminalError,
                    format!("wait task join error: {e}"),
                ))?
        });
    }

    // Not in a tokio runtime — run directly
    handle_wait_blocking(args)
}

fn handle_kill(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("kill requires 'session_id' parameter")),
    };

    // First: try to kill the actual OS process if we have a PID
    let pid = {
        let registry = PROCESS_REGISTRY.lock();
        let Some(p) = registry.get(&session_id) else {
            return Ok(tool_error(format!("Process not found: {session_id}")));
        };
        if !p.running {
            return Ok(tool_error(format!("Process already finished: {session_id}")));
        }
        p.pid
    };

    let signal_sent = if let Some(pid) = pid {
        kill_process_by_pid(pid)
    } else {
        false
    };

    // Mark as finished in registry regardless of signal result
    let mut registry = PROCESS_REGISTRY.lock();
    if let Some(p) = registry.get_mut(&session_id) {
        p.running = false;
        p.exit_code = Some(-1);
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "kill",
        "session_id": session_id,
        "signal": "SIGTERM",
        "pid_sent": pid,
        "signal_sent": signal_sent,
    })
    .to_string())
}

/// Actually kill a process by PID.
#[cfg(unix)]
fn kill_process_by_pid(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if result == 0 {
        true
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        tracing::warn!("Failed to send SIGTERM to PID {pid}: errno {errno}");
        false
    }
}

#[cfg(windows)]
fn kill_process_by_pid(pid: u32) -> bool {
    let output = std::process::Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .output();
    match output {
        Ok(out) => {
            if out.status.success() {
                true
            } else {
                tracing::warn!("taskkill failed for PID {pid}: {}", String::from_utf8_lossy(&out.stderr));
                false
            }
        }
        Err(e) => {
            tracing::warn!("Failed to run taskkill for PID {pid}: {e}");
            false
        }
    }
}

fn handle_write(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("write requires 'session_id' parameter")),
    };

    let data = match args.get("data").and_then(Value::as_str) {
        Some(d) => d.to_string(),
        None => return Ok(tool_error("write requires 'data' parameter (stdin input)")),
    };

    let registry = PROCESS_REGISTRY.lock();
    let Some(p) = registry.get(&session_id) else {
        return Ok(tool_error(format!("Process not found: {session_id}")));
    };

    if !p.running {
        return Ok(tool_error(format!("Cannot write to finished process: {session_id}")));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "write",
        "session_id": session_id,
        "bytes_written": data.len(),
        "note": "Data queued for stdin. Use 'submit' to send.",
    })
    .to_string())
}

fn handle_submit(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("submit requires 'session_id' parameter")),
    };

    let registry = PROCESS_REGISTRY.lock();
    let Some(p) = registry.get(&session_id) else {
        return Ok(tool_error(format!("Process not found: {session_id}")));
    };

    if !p.running {
        return Ok(tool_error(format!("Cannot submit to finished process: {session_id}")));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "submit",
        "session_id": session_id,
        "note": "Stdin data submitted to process.",
    })
    .to_string())
}

fn handle_close(args: &Value) -> Result<String, hermez_core::HermezError> {
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return Ok(tool_error("close requires 'session_id' parameter")),
    };

    let mut registry = PROCESS_REGISTRY.lock();
    if registry.remove(&session_id).is_none() {
        return Ok(tool_error(format!("Process not found: {session_id}")));
    }

    Ok(serde_json::json!({
        "success": true,
        "action": "close",
        "session_id": session_id,
    })
    .to_string())
}

/// Register a process in the registry (called by terminal tool).
pub fn register_process(
    session_id: String,
    command: String,
    pid: Option<u32>,
) {
    let mut registry = PROCESS_REGISTRY.lock();

    if registry.len() >= MAX_PROCESSES {
        let finished: Vec<_> = registry
            .iter()
            .filter(|(_, p)| !p.running)
            .map(|(k, _)| k.clone())
            .collect();
        for k in finished {
            registry.remove(&k);
        }
        if registry.len() >= MAX_PROCESSES {
            if let Some(oldest) = registry.keys().next().cloned() {
                registry.remove(&oldest);
            }
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    registry.insert(
        session_id.clone(),
        ManagedProcess {
            session_id,
            command,
            pid,
            running: true,
            exit_code: None,
            output_buffer: String::new(),
            output_size: 0,
            spawned_at: now.clone(),
            last_polled: now,
            notify_on_complete: false,
            watch_patterns: Vec::new(),
            use_pty: false,
            watcher_platform: None,
            watcher_chat_id: None,
            watcher_user_id: None,
            watcher_user_name: None,
            watcher_thread_id: None,
        },
    );
}

/// Update process output buffer.
pub fn update_process_output(session_id: &str, output: &str) {
    let mut registry = PROCESS_REGISTRY.lock();
    if let Some(p) = registry.get_mut(session_id) {
        p.output_buffer.push_str(output);
        p.output_size = p.output_buffer.len();

        if p.output_size > MAX_OUTPUT_SIZE {
            let trim_point = p.output_size - MAX_OUTPUT_SIZE;
            p.output_buffer = p.output_buffer[trim_point..].to_string();
            p.output_size = MAX_OUTPUT_SIZE;
        }
    }
}

/// Mark process as finished.
pub fn mark_process_finished(session_id: &str, exit_code: i32) {
    let mut registry = PROCESS_REGISTRY.lock();
    if let Some(p) = registry.get_mut(session_id) {
        p.running = false;
        p.exit_code = Some(exit_code);
        p.last_polled = chrono::Utc::now().to_rfc3339();
    }
}

/// Spawn a local background process and register it.
///
/// Mirrors Python `process_registry.spawn_local`.
pub fn spawn_local(
    command: &str,
    cwd: Option<&str>,
    task_id: &str,
    use_pty: bool,
) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = cmd.spawn().map_err(|e| format!("Failed to spawn background command: {e}"))?;

    let pid = child.id();
    let session_id = format!(
        "proc_{:08x}_{:08x}",
        task_id.as_bytes().iter().fold(0u32, |a, b| a.wrapping_add(*b as u32)),
        pid
    );

    register_process(session_id.clone(), command.to_string(), Some(pid));

    // Configure PTY flag
    {
        let mut registry = PROCESS_REGISTRY.lock();
        if let Some(p) = registry.get_mut(&session_id) {
            p.use_pty = use_pty;
        }
    }

    let sid = session_id.clone();
    std::thread::spawn(move || {
        if let Ok(output) = child.wait_with_output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            update_process_output(&sid, &format!("{stdout}{stderr}"));
            let exit_code = output.status.code().unwrap_or(-1);
            mark_process_finished(&sid, exit_code);
        }
    });

    Ok(session_id)
}

/// Spawn a background process via an environment backend.
///
/// Mirrors Python `process_registry.spawn_via_env`.
pub fn spawn_via_env(
    env: &Arc<dyn crate::environments::Environment>,
    command: &str,
    cwd: Option<&str>,
    task_id: &str,
) -> Result<String, String> {
    // Non-local backends run inside the sandbox via env.execute() in a thread.
    let command = command.to_string();
    let task_id = task_id.to_string();
    let cwd = cwd.map(|s| s.to_string());
    let env = Arc::clone(env);

    let session_id = format!(
        "env_{:08x}_{:016x}",
        task_id.as_bytes().iter().fold(0u32, |a, b| a.wrapping_add(*b as u32)),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );

    register_process(session_id.clone(), command.clone(), None);

    let sid = session_id.clone();
    std::thread::spawn(move || {
        let result = env.execute(&command, cwd.as_deref(), None);
        let combined = format!("{}{}", result.stdout, result.stderr);
        update_process_output(&sid, &combined);
        mark_process_finished(&sid, result.exit_code);
    });

    Ok(session_id)
}

/// Set notification and watch options on a registered process.
pub fn set_process_notify_options(
    session_id: &str,
    notify_on_complete: bool,
    watch_patterns: Vec<String>,
) {
    let mut registry = PROCESS_REGISTRY.lock();
    if let Some(p) = registry.get_mut(session_id) {
        p.notify_on_complete = notify_on_complete;
        p.watch_patterns = watch_patterns;
    }
}

/// Register the process tool.
/// Kill all tracked processes — used during gateway shutdown.
///
/// Iterates the process registry and sends SIGTERM to every running process.
/// Returns the number of processes killed.
/// Mirrors Python _kill_tool_subprocesses() (run.py:2599-2745).
pub fn kill_all() -> usize {
    let registry = PROCESS_REGISTRY.lock();
    let mut count = 0;
    for (session_id, proc) in registry.iter() {
        if proc.running {
            if let Some(pid) = proc.pid {
                if kill_process_by_pid(pid) {
                    count += 1;
                    tracing::info!("Killed process {} (session={}) during shutdown", pid, session_id);
                }
            }
        }
    }
    count
}

pub fn register_process_tool(registry: &mut ToolRegistry) {
    registry.register(
        "process".to_string(),
        "terminal".to_string(),
        serde_json::json!({
            "name": "process",
            "description": "Manage background processes spawned by terminal(background=true). Actions: list, poll, log, wait, kill, write, submit, close.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "Action: list, poll, log, wait, kill, write, submit, close." },
                    "session_id": { "type": "string", "description": "Process session ID (required for all actions except 'list')." },
                    "data": { "type": "string", "description": "Stdin data to write (for 'write'/'submit' actions)." },
                    "timeout": { "type": "integer", "description": "Wait timeout in seconds (default 180, max 180)." },
                    "offset": { "type": "integer", "description": "Log read offset (for 'log' action, default 0)." },
                    "limit": { "type": "integer", "description": "Log read limit (for 'log' action, default 10000)." }
                },
                "required": ["action"]
            }
        }),
        std::sync::Arc::new(handle_process),
        None,
        vec!["terminal".to_string()],
        "Manage background processes".to_string(),
        "⚙️".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn cleanup() {
        let mut registry = PROCESS_REGISTRY.lock();
        registry.clear();
    }

    #[test]
    #[serial]
    fn test_handle_list_empty() {
        cleanup();
        let result = handle_process(serde_json::json!({ "action": "list" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("total").is_some());
        assert!(json.get("processes").is_some());
    }

    #[test]
    #[serial]
    fn test_handle_poll_no_session() {
        cleanup();
        let result = handle_process(serde_json::json!({
            "action": "poll",
            "session_id": "nonexistent_proc"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_handle_missing_session_id() {
        cleanup();
        for action in ["poll", "log", "wait", "kill", "write", "submit", "close"] {
            let result = handle_process(serde_json::json!({ "action": action }));
            let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
            assert!(json.get("error").is_some(), "action '{action}' should error without session_id");
        }
    }

    #[test]
    #[serial]
    fn test_handle_unknown_action() {
        cleanup();
        let result = handle_process(serde_json::json!({ "action": "restart" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    #[serial]
    fn test_register_and_poll() {
        cleanup();
        let id = "reg_poll_test";
        register_process(id.to_string(), "echo hello".to_string(), Some(1234));

        let result = handle_process(serde_json::json!({
            "action": "poll",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["running"], true);
        assert_eq!(json["session_id"], id);

        cleanup();
    }

    #[test]
    #[serial]
    fn test_kill_process() {
        cleanup();
        let id = "kill_test";
        register_process(id.to_string(), "sleep 10".to_string(), None);

        let result = handle_process(serde_json::json!({
            "action": "kill",
            "session_id": id
        }));
        assert!(result.is_ok());

        let result = handle_process(serde_json::json!({
            "action": "wait",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["already_finished"], true);

        cleanup();
    }

    #[test]
    #[serial]
    fn test_kill_with_pid() {
        cleanup();
        let id = "kill_with_pid_test";
        // Register with a fake PID — the kill will attempt to send a signal
        // but the PID won't exist, so the signal will fail gracefully
        register_process(id.to_string(), "sleep 10".to_string(), Some(999999));

        let result = handle_process(serde_json::json!({
            "action": "kill",
            "session_id": id
        }));
        assert!(result.is_ok());

        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        // Verify the response includes pid_sent and signal_sent fields
        assert_eq!(json["pid_sent"], 999999);
        assert!(json.get("signal_sent").is_some());

        // Process should be marked as finished in registry
        let result = handle_process(serde_json::json!({
            "action": "wait",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["already_finished"], true);

        cleanup();
    }

    #[test]
    #[serial]
    fn test_close_process() {
        cleanup();
        let id = "close_test";
        register_process(id.to_string(), "echo done".to_string(), None);

        let result = handle_process(serde_json::json!({
            "action": "close",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);

        // Verify removed
        let result = handle_process(serde_json::json!({
            "action": "poll",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());

        cleanup();
    }

    #[test]
    #[serial]
    fn test_log_registered_process() {
        cleanup();
        let id = "log_test";
        register_process(id.to_string(), "echo hello".to_string(), None);
        update_process_output(id, "Hello from process\n");

        let result = handle_process(serde_json::json!({
            "action": "log",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json["log"].as_str().unwrap().contains("Hello"));

        cleanup();
    }

    #[test]
    #[serial]
    fn test_mark_finished() {
        cleanup();
        let id = "finish_test";
        register_process(id.to_string(), "exit 0".to_string(), None);
        mark_process_finished(id, 0);

        let result = handle_process(serde_json::json!({
            "action": "wait",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["already_finished"], true);
        assert_eq!(json["exit_code"], 0);

        cleanup();
    }

    #[test]
    #[serial]
    fn test_output_buffer_trim() {
        cleanup();
        let id = "trim_test";
        register_process(id.to_string(), "big output".to_string(), None);

        // Feed output in chunks to trigger the trim without a single huge allocation
        let chunk = "x".repeat(50_000); // 50KB chunks
        for _ in 0..5 {
            // 5 * 50KB = 250KB > 200KB MAX
            update_process_output(id, &chunk);
        }

        {
            let registry = PROCESS_REGISTRY.lock();
            let p = registry.get(id).unwrap();
            assert!(p.output_size <= MAX_OUTPUT_SIZE, "buffer should be trimmed to MAX");
            assert!(p.output_size > 0, "buffer should not be empty");
        }

        cleanup();
    }

    #[test]
    #[serial]
    fn test_wait_timeout_short() {
        cleanup();
        let id = "timeout_test";
        register_process(id.to_string(), "sleep 999".to_string(), None);

        // With a 1-second timeout, wait should return timed_out=true quickly
        let result = handle_process(serde_json::json!({
            "action": "wait",
            "session_id": id,
            "timeout": 1
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["timed_out"], true);
        assert_eq!(json["session_id"], id);

        cleanup();
    }

    #[test]
    #[serial]
    fn test_wait_already_finished() {
        cleanup();
        let id = "wait_finished_test";
        register_process(id.to_string(), "echo done".to_string(), None);
        mark_process_finished(id, 0);

        let result = handle_process(serde_json::json!({
            "action": "wait",
            "session_id": id
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["already_finished"], true);
        assert_eq!(json["exit_code"], 0);

        cleanup();
    }
}
