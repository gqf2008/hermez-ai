#![allow(dead_code)]
//! Terminal tool — command execution with foreground and background modes.
//!
//! Mirrors the Python `tools/terminal_tool.py`.
//! Supports multiple backends: local, Docker, SSH, Modal, Singularity, Daytona.
//! Integrates with `process_reg` for background process tracking.
//! Environment selection via `HERMES_TERMINAL_BACKEND` env var or config.
//!
//! Features:
//! - PTY mode for interactive CLI tools (local + SSH)
//! - Approval system (tirith + dangerous command guards)
//! - Sudo handling with SUDO_PASSWORD env and cached session password
//! - Exit code context for common CLI tools (grep, diff, curl, etc.)

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use parking_lot::RwLock;
use regex::Regex;
use serde_json::Value;

use crate::environments::{
    create_environment, Environment, EnvConfig, ProcessResult,
};
use crate::process_reg::{
    set_process_notify_options, spawn_local, spawn_via_env,
};
use crate::registry::{tool_error, ToolRegistry};

use hermes_core::config::HermesConfig;

// ---------------------------------------------------------------------------
// Global environment cache — mirrors Python `_active_environments`
// ---------------------------------------------------------------------------

/// Cached environment with last-activity timestamp for idle cleanup.
struct CachedEnv {
    env: Arc<dyn Environment>,
    last_activity: Instant,
    env_type: String,
}

/// Global environment cache keyed by task_id.
static ACTIVE_ENVIRONMENTS: Lazy<RwLock<std::collections::HashMap<String, CachedEnv>>> =
    Lazy::new(|| RwLock::new(std::collections::HashMap::new()));

/// Inactivity timeout before an environment is cleaned up (seconds).
const ENV_IDLE_TIMEOUT: u64 = 1800; // 30 minutes

/// Max foreground timeout (seconds).
const FOREGROUND_MAX_TIMEOUT: u64 = 600;

/// Default foreground timeout (seconds).
const FOREGROUND_DEFAULT_TIMEOUT: u64 = 60;

/// Max output size returned to LLM (50KB).
const MAX_OUTPUT_RETURN: usize = 50 * 1024;

/// Truncate ratio: 40% head + 60% tail.
const HEAD_RATIO: f64 = 0.4;

// ---------------------------------------------------------------------------
// Workdir validation
// ---------------------------------------------------------------------------

fn is_valid_workdir(workdir: &str) -> bool {
    !workdir.is_empty()
        && workdir
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '-' | '_' | ' ' | ':' | '~' | '+' | '@' | '=' | ','))
        && !workdir.contains("..")
        && !workdir.contains('|')
        && !workdir.contains(';')
        && !workdir.contains('&')
        && !workdir.contains('$')
        && !workdir.contains('`')
}

// ---------------------------------------------------------------------------
// Output processing
// ---------------------------------------------------------------------------

fn truncate_output(output: &str) -> String {
    let bytes = output.len();
    if bytes <= MAX_OUTPUT_RETURN {
        return output.to_string();
    }

    let head_len = (bytes as f64 * HEAD_RATIO) as usize;
    let tail_len = bytes - head_len;

    let head_end = output
        .char_indices()
        .take_while(|(i, _)| *i <= head_len)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(head_len.min(bytes));

    let tail_start = output
        .char_indices()
        .find(|(i, _)| *i >= bytes - tail_len)
        .map(|(i, _)| i)
        .unwrap_or(head_end);

    if tail_start <= head_end {
        return format!(
            "{}\n... [{} bytes truncated]",
            &output[..head_end.min(bytes)],
            bytes
        );
    }

    format!(
        "{}\n... [{} bytes truncated] ...\n{}",
        &output[..head_end],
        bytes,
        &output[tail_start..]
    )
}

fn redact_secrets(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in ["sk-", "ghp_", "xoxb-", "Bearer "] {
        while let Some(pos) = result.find(pattern) {
            let end = pos + pattern.len() + 10;
            let end = end.min(result.len());
            result.replace_range(pos..end, "[REDACTED]");
            if !result.contains(pattern) {
                break;
            }
        }
    }
    result
}

fn format_process_result(result: &ProcessResult) -> String {
    let mut combined = format!("{}{}", result.stdout, result.stderr);
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&format!("[Process exited with code {}]", result.exit_code));
    let stripped = crate::ansi_strip::strip_ansi(&combined);
    let truncated = truncate_output(&stripped);
    redact_secrets(&truncated)
}

// ---------------------------------------------------------------------------
// Exit code context for common CLI tools
// ---------------------------------------------------------------------------

/// Return a human-readable note when a non-zero exit code is non-erroneous.
///
/// Returns `None` when the exit code is 0 or genuinely signals an error.
/// The note is appended to the tool result so the model doesn't waste
/// turns investigating expected exit codes.
fn interpret_exit_code(command: &str, exit_code: i32) -> Option<String> {
    if exit_code == 0 {
        return None;
    }

    // Extract the last command in a pipeline/chain.
    let segments: Vec<&str> = command.split(['|', ';', '&']).collect();
    let last_segment = segments.last().unwrap_or(&command).trim();

    // Get base command name, skipping env var assignments.
    let words: Vec<&str> = last_segment.split_whitespace().collect();
    let mut base_cmd = "";
    for w in &words {
        if w.contains('=') && !w.starts_with('-') {
            continue;
        }
        base_cmd = w.split('/').next_back().unwrap_or(w);
        break;
    }

    if base_cmd.is_empty() {
        return None;
    }

    match base_cmd {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => {
            if exit_code == 1 {
                return Some("No matches found (not an error)".to_string());
            }
        }
        "diff" | "colordiff" => {
            if exit_code == 1 {
                return Some("Files differ (expected, not an error)".to_string());
            }
        }
        "find" => {
            if exit_code == 1 {
                return Some(
                    "Some directories were inaccessible (partial results may still be valid)"
                        .to_string(),
                );
            }
        }
        "test" | "[" => {
            if exit_code == 1 {
                return Some("Condition evaluated to false (expected, not an error)".to_string());
            }
        }
        "curl" => match exit_code {
            6 => return Some("Could not resolve host".to_string()),
            7 => return Some("Failed to connect to host".to_string()),
            22 => return Some(
                "HTTP response code indicated error (e.g. 404, 500)".to_string(),
            ),
            28 => return Some("Operation timed out".to_string()),
            _ => {}
        },
        "git" => {
            if exit_code == 1 {
                return Some(
                    "Non-zero exit (often normal — e.g. 'git diff' returns 1 when files differ)"
                        .to_string(),
                );
            }
        }
        _ => {}
    }

    None
}

// ---------------------------------------------------------------------------
// Sudo handling
// ---------------------------------------------------------------------------

/// Session-cached sudo password (persists until process exits).
static CACHED_SUDO_PASSWORD: Lazy<std::sync::Mutex<String>> =
    Lazy::new(|| std::sync::Mutex::new(String::new()));

/// Return True when PTY mode would break stdin-driven commands.
fn command_requires_pipe_stdin(command: &str) -> bool {
    let normalized: String = command.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.starts_with("gh auth login") && normalized.contains("--with-token")
}

/// Read one shell token, preserving quotes/escapes, starting at *start*.
fn read_shell_token(command: &str, start: usize) -> (String, usize) {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut i = start;
    let mut result = String::new();

    while i < n {
        let ch = chars[i];
        if ch.is_whitespace() || ch == ';' || ch == '|' || ch == '&' || ch == '(' || ch == ')' {
            break;
        }
        if ch == '\'' {
            i += 1;
            while i < n && chars[i] != '\'' {
                result.push(chars[i]);
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            i += 1;
            while i < n {
                let inner = chars[i];
                if inner == '\\' && i + 1 < n {
                    result.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if inner == '"' {
                    i += 1;
                    break;
                }
                result.push(inner);
                i += 1;
            }
            continue;
        }
        if ch == '\\' && i + 1 < n {
            result.push(chars[i + 1]);
            i += 2;
            continue;
        }
        result.push(ch);
        i += 1;
    }

    (result, i)
}

/// Return true when token is a leading shell environment assignment.
fn looks_like_env_assignment(token: &str) -> bool {
    if !token.contains('=') || token.starts_with('=') {
        return false;
    }
    let name: &str = token.split('=').next().unwrap_or("");
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap().is_match(name)
}

/// Rewrite only real unquoted sudo command words.
fn rewrite_real_sudo_invocations(command: &str) -> (String, bool) {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0;
    let mut command_start = true;
    let mut found = false;

    while i < n {
        let ch = chars[i];

        if ch.is_whitespace() {
            out.push(ch);
            if ch == '\n' {
                command_start = true;
            }
            i += 1;
            continue;
        }

        if ch == '#' && command_start {
            let comment_start = i;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            out.push_str(&command[comment_start..i]);
            continue;
        }

        if i + 1 < n
            && ((chars[i] == '&' && chars[i + 1] == '&')
                || (chars[i] == '|' && chars[i + 1] == '|')
                || (chars[i] == ';' && chars[i + 1] == ';'))
        {
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
            command_start = true;
            continue;
        }

        if ch == ';' || ch == '|' || ch == '&' || ch == '(' {
            out.push(ch);
            i += 1;
            command_start = true;
            continue;
        }

        if ch == ')' {
            out.push(ch);
            i += 1;
            command_start = false;
            continue;
        }

        let (token, next_i) = read_shell_token(command, i);
        if command_start && token == "sudo" {
            out.push_str("sudo -S -p ''");
            found = true;
        } else {
            out.push_str(&token);
        }

        command_start = command_start && looks_like_env_assignment(&token);
        i = next_i;
    }

    (out, found)
}

/// Transform sudo commands to use `-S` flag if SUDO_PASSWORD is available.
///
/// Returns `(transformed_command, sudo_stdin)` where `sudo_stdin` is the
/// password with a trailing newline that the caller must prepend to the
/// process's stdin stream.
fn transform_sudo_command(command: &str) -> (String, Option<String>) {
    let (transformed, has_real_sudo) = rewrite_real_sudo_invocations(command);
    if !has_real_sudo {
        return (command.to_string(), None);
    }

    let has_configured_password = std::env::var("SUDO_PASSWORD").is_ok();
    let mut cached = CACHED_SUDO_PASSWORD.lock().unwrap();
    let sudo_password = if has_configured_password {
        std::env::var("SUDO_PASSWORD").unwrap_or_default()
    } else if !cached.is_empty() {
        cached.clone()
    } else if std::env::var("HERMES_INTERACTIVE").is_ok() {
        let password = prompt_for_sudo_password(45);
        if !password.is_empty() {
            *cached = password.clone();
        }
        password
    } else {
        String::new()
    };
    drop(cached);

    if has_configured_password || !sudo_password.is_empty() {
        return (transformed, Some(sudo_password + "\n"));
    }

    (command.to_string(), None)
}

/// Prompt user for sudo password with timeout.
///
/// Returns the password if entered, or empty string if skipped/error.
fn prompt_for_sudo_password(timeout_seconds: u64) -> String {
    use std::io::Write;

    // Only works in interactive mode
    if std::env::var("HERMES_INTERACTIVE").is_err() {
        return String::new();
    }

    eprintln!();
    eprintln!("┌{}┐", "─".repeat(58));
    eprintln!("│  🔐 SUDO PASSWORD REQUIRED{}│", " ".repeat(30));
    eprintln!("├{}┤", "─".repeat(58));
    eprintln!("│  Enter password below (input is hidden), or:            │");
    eprintln!("│    • Press Enter to skip (command fails gracefully)     │");
    let timeout_label = format!("{timeout_seconds}s to auto-skip");
    eprintln!("│    • Wait {}{}│", timeout_label, " ".repeat(27usize.saturating_sub(timeout_seconds.to_string().len())));
    eprintln!("└{}┘", "─".repeat(58));
    eprintln!();
    eprint!("  Password (hidden): ");
    let _ = std::io::stderr().flush();

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe {
            let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty");
            let fd = match tty {
                Ok(ref f) => f.as_raw_fd(),
                Err(_) => return String::new(),
            };

            let mut old_attrs: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut old_attrs) != 0 {
                return String::new();
            }
            let mut new_attrs = old_attrs;
            new_attrs.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &new_attrs) != 0 {
                return String::new();
            }

            let mut result = String::new();
            let start = Instant::now();
            let mut buf = [0u8; 1];
            loop {
                if start.elapsed().as_secs() >= timeout_seconds {
                    break;
                }
                // Use poll with timeout to avoid blocking forever
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ret = libc::poll(&mut pfd, 1, 100);
                if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
                    let n = libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), 1);
                    if n <= 0 {
                        break;
                    }
                    if buf[0] == b'\n' || buf[0] == b'\r' {
                        break;
                    }
                    result.push(buf[0] as char);
                }
            }

            let _ = libc::tcsetattr(fd, libc::TCSAFLUSH, &old_attrs);
            eprintln!();
            if !result.is_empty() {
                eprintln!("  ✓ Password received (cached for this session)");
            } else {
                eprintln!("  ⏭ Skipped - continuing without sudo");
            }
            eprintln!();
            result
        }
    }

    #[cfg(not(unix))]
    {
        // Non-Unix: read from stdin without echo control
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        input.trim_end().to_string()
    }
}

/// Check for sudo failure and add helpful message for messaging contexts.
fn handle_sudo_failure(output: &str, _env_type: &str) -> String {
    let is_gateway = std::env::var("HERMES_GATEWAY_SESSION").is_ok();
    if !is_gateway {
        return output.to_string();
    }

    let failures = [
        "sudo: a password is required",
        "sudo: no tty present",
        "sudo: a terminal is required",
    ];

    for failure in &failures {
        if output.contains(failure) {
            let home = hermes_core::get_hermes_home();
            return format!(
                "{}\n\n💡 Tip: To enable sudo over messaging, add SUDO_PASSWORD to {}/.env on the agent machine.",
                output,
                home.display()
            );
        }
    }

    output.to_string()
}

// ---------------------------------------------------------------------------
// Approval system (tirith + dangerous command guards)
// ---------------------------------------------------------------------------

/// Result of checking all approval guards.
#[derive(Debug)]
struct GuardResult {
    approved: bool,
    status: String,
    message: Option<String>,
    description: Option<String>,
    pattern_key: Option<String>,
    user_approved: bool,
    smart_approved: bool,
}

/// Check all guards: tirith security scan + dangerous command detection + approval mode.
///
/// Mirrors Python `tools.approval.check_all_command_guards`.
fn check_all_guards(command: &str, env_type: &str) -> GuardResult {
    let gateway_ask = std::env::var("HERMES_GATEWAY_ASK_MODE").is_ok();

    // Sandboxed environments are more permissive.
    let is_sandboxed = matches!(env_type, "docker" | "modal" | "daytona" | "singularity" | "ssh");
    if is_sandboxed {
        let trimmed = command.trim();
        // Allow rm -rf against a specific path, but not the root filesystem.
        if trimmed.starts_with("rm -rf ") && !trimmed.ends_with(" /") {
            return GuardResult {
                approved: true,
                status: "approved".to_string(),
                message: Some("approved in sandboxed environment".to_string()),
                description: None,
                pattern_key: None,
                user_approved: false,
                smart_approved: false,
            };
        }
    }

    // 1. Tirith security scan
    if crate::tirith::is_tirith_installed() {
        match crate::tirith::check_command_security(command) {
            Ok(tirith_result) => {
                if tirith_result.action == "block" {
                    let msg = format!(
                        "Tirith security check blocked this command: {}",
                        tirith_result.summary
                    );
                    if gateway_ask {
                        return GuardResult {
                            approved: false,
                            status: "approval_required".to_string(),
                            message: Some(msg),
                            description: Some(
                                "Blocked by tirith security scanner".to_string(),
                            ),
                            pattern_key: Some("tirith_block".to_string()),
                            user_approved: false,
                            smart_approved: false,
                        };
                    }
                    return GuardResult {
                        approved: false,
                        status: "blocked".to_string(),
                        message: Some(msg),
                        description: Some(
                            "Blocked by tirith security scanner".to_string(),
                        ),
                        pattern_key: Some("tirith_block".to_string()),
                        user_approved: false,
                        smart_approved: false,
                    };
                }
            }
            Err(e) => {
                tracing::warn!("Tirith check failed: {e}");
            }
        }
    }

    // 2. Approval mode
    let mode_str = std::env::var("HERMES_APPROVAL_MODE")
        .unwrap_or_else(|_| "smart".to_string());
    let mode = crate::approval::ApprovalMode::parse(&mode_str)
        .unwrap_or(crate::approval::ApprovalMode::Smart);

    let session_id = std::env::var("HERMES_SESSION_ID").ok();
    let eval = crate::approval::evaluate_command(command, mode, session_id.as_deref());

    if eval.approved {
        let smart_approved = mode == crate::approval::ApprovalMode::Smart
            && (eval.reason.as_ref().map(|r| r.contains("allowlist")).unwrap_or(false)
                || eval.reason.as_ref().map(|r| r.contains("safe")).unwrap_or(false));
        return GuardResult {
            approved: true,
            status: "approved".to_string(),
            message: eval.reason.clone(),
            description: None,
            pattern_key: None,
            user_approved: false,
            smart_approved,
        };
    }

    // Not approved
    if eval.dangerous || mode == crate::approval::ApprovalMode::Manual {
        let desc = eval
            .reason
            .clone()
            .unwrap_or_else(|| "command flagged".to_string());
        if gateway_ask {
            return GuardResult {
                approved: false,
                status: "approval_required".to_string(),
                message: eval.reason.clone(),
                description: Some(desc.clone()),
                pattern_key: Some("dangerous_command".to_string()),
                user_approved: false,
                smart_approved: false,
            };
        }
        return GuardResult {
            approved: false,
            status: "blocked".to_string(),
            message: eval.reason.clone(),
            description: Some(desc),
            pattern_key: Some("dangerous_command".to_string()),
            user_approved: false,
            smart_approved: false,
        };
    }

    GuardResult {
        approved: false,
        status: "blocked".to_string(),
        message: eval.reason.clone(),
        description: Some("Command requires approval".to_string()),
        pattern_key: Some("manual_approval".to_string()),
        user_approved: false,
        smart_approved: false,
    }
}

// ---------------------------------------------------------------------------
// Environment config resolution
// ---------------------------------------------------------------------------

/// Resolve terminal backend configuration from config + env vars.
fn resolve_env_config(override_type: Option<&str>) -> EnvConfig {
    // Check env var first (TERMINAL_ENV or HERMES_TERMINAL_BACKEND)
    let env_type = override_type
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HERMES_TERMINAL_BACKEND").ok())
        .or_else(|| std::env::var("TERMINAL_ENV").ok())
        .unwrap_or_else(|| {
            // Fall back to config file
            HermesConfig::load()
                .ok()
                .map(|c| c.terminal.backend)
                .unwrap_or_else(|| "local".to_string())
        });

    let terminal_cfg = HermesConfig::load()
        .ok()
        .map(|c| c.terminal);

    let cwd = terminal_cfg
        .as_ref()
        .and_then(|t| t.cwd.clone())
        .map(|p| p.to_string_lossy().to_string());

    let docker_image = terminal_cfg
        .as_ref()
        .and_then(|t| t.docker_image.clone())
        .unwrap_or_else(|| "ubuntu:22.04".to_string());

    let ssh_host = terminal_cfg.as_ref().and_then(|t| t.ssh_host.clone());
    let ssh_user = terminal_cfg.as_ref().and_then(|t| t.ssh_user.clone());
    let ssh_port = terminal_cfg.as_ref().map(|_| 22u16);

    EnvConfig {
        env_type,
        cwd,
        image: Some(docker_image),
        ssh_host,
        ssh_user,
        ssh_port,
    }
}

// ---------------------------------------------------------------------------
// Environment cache management
// ---------------------------------------------------------------------------

/// Get or create an environment for the given task_id.
fn get_or_create_env(task_id: &str, env_config: &EnvConfig) -> (Arc<dyn Environment>, String) {
    // Check cache first
    {
        let mut cache = ACTIVE_ENVIRONMENTS.write();
        if let Some(cached) = cache.get_mut(task_id) {
            cached.last_activity = Instant::now();
            return (Arc::clone(&cached.env), cached.env_type.clone());
        }
    }

    // Create new environment
    let env = create_environment(env_config);
    let env_type = env.env_type().to_string();
    let env_arc = env;

    let mut cache = ACTIVE_ENVIRONMENTS.write();
    cache.insert(
        task_id.to_string(),
        CachedEnv {
            env: Arc::clone(&env_arc),
            last_activity: Instant::now(),
            env_type: env_type.clone(),
        },
    );

    (env_arc, env_type)
}

/// Clean up idle environments.
fn cleanup_idle_envs() {
    let mut cache = ACTIVE_ENVIRONMENTS.write();
    let idle_cutoff = Instant::now() - Duration::from_secs(ENV_IDLE_TIMEOUT);
    cache.retain(|task_id, cached| {
        if cached.last_activity < idle_cutoff {
            tracing::debug!("Cleaning up idle environment for task {task_id}");
            false
        } else {
            true
        }
    });
}

/// Check total active environment count.
fn active_env_count() -> usize {
    ACTIVE_ENVIRONMENTS.read().len()
}

// ---------------------------------------------------------------------------
// Foreground execution
// ---------------------------------------------------------------------------

fn execute_foreground_via_env(
    env: Arc<dyn Environment>,
    command: &str,
    timeout: u64,
    workdir: Option<&str>,
    use_pty: bool,
) -> Result<String, String> {
    let result = if use_pty {
        env.execute_pty(command, workdir, Some(timeout))
    } else {
        env.execute(command, workdir, Some(timeout))
    };
    let output = format_process_result(&result);

    if result.exit_code != 0 && result.stdout.is_empty() && result.stderr.is_empty()
        && result.exit_code == -1 {
            return Err("Command execution failed (environment unavailable)".to_string());
        }

    Ok(output)
}

/// Build a shell command appropriate for the platform.
#[cfg(unix)]
fn build_shell_cmd(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", command]);
    cmd
}

#[cfg(windows)]
fn build_shell_cmd(command: &str) -> Command {
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", command]);
    cmd
}

fn execute_foreground_local_blocking(
    command: &str,
    timeout: u64,
    workdir: Option<&str>,
    stdin_data: Option<&str>,
) -> Result<String, String> {
    let start = Instant::now();

    let mut cmd = build_shell_cmd(command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::piped());

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn command: {e}"))?;

    // Write stdin data if provided (e.g., sudo password)
    if let Some(data) = stdin_data {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data.as_bytes());
            // stdin is dropped here, closing the pipe
        }
    } else {
        // Drop stdin immediately so the child sees EOF
        drop(child.stdin.take());
    }

    let result = loop {
        if start.elapsed() > Duration::from_secs(timeout) {
            let _ = child.kill();
            break Err(format!(
                "Command timed out after {timeout}s. Increase timeout or run with background=true."
            ));
        }

        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.code().unwrap_or(-1)),
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => break Err(format!("Error waiting for process: {e}")),
        }
    };

    let exit_code = result?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read output: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut combined = format!("{stdout}{stderr}");
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&format!("[Process exited with code {exit_code}]\n"));

    let stripped = crate::ansi_strip::strip_ansi(&combined);
    let truncated = truncate_output(&stripped);
    Ok(redact_secrets(&truncated))
}

fn execute_foreground_local(
    command: &str,
    timeout: u64,
    workdir: Option<&str>,
    stdin_data: Option<&str>,
) -> Result<String, String> {
    // If we're inside a tokio runtime, offload the blocking work to spawn_blocking
    // so the async executor thread remains responsive.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let command = command.to_string();
        let workdir = workdir.map(|s| s.to_string());
        let stdin_data = stdin_data.map(|s| s.to_string());
        return handle.block_on(async {
            tokio::task::spawn_blocking(move || {
                execute_foreground_local_blocking(
                    &command,
                    timeout,
                    workdir.as_deref(),
                    stdin_data.as_deref(),
                )
            })
            .await
            .map_err(|e| format!("Task join error: {e}"))?
        });
    }

    // Not in a tokio runtime — run directly (e.g., synchronous test or non-async caller)
    execute_foreground_local_blocking(command, timeout, workdir, stdin_data)
}

// ---------------------------------------------------------------------------
// Background execution
// ---------------------------------------------------------------------------

fn execute_background(
    command: &str,
    workdir: Option<&str>,
    notify_on_complete: bool,
    watch_patterns: Vec<String>,
    use_pty: bool,
) -> String {
    let session_id = match spawn_local(command, workdir, "default", use_pty) {
        Ok(id) => id,
        Err(e) => return tool_error(format!("Failed to spawn background command: {e}")),
    };

    let pid = {
        let registry = crate::process_reg::PROCESS_REGISTRY.lock();
        registry.get(&session_id).and_then(|p| p.pid)
    };

    set_process_notify_options(&session_id, notify_on_complete, watch_patterns.clone());

    let mut result = serde_json::json!({
        "success": true,
        "action": "background",
        "session_id": session_id,
        "pid": pid,
        "command": command,
        "note": "Process started in background. Use 'process' tool with session_id to poll status.",
    });

    if notify_on_complete {
        result["notify_on_complete"] = serde_json::json!(true);
    }
    if !watch_patterns.is_empty() {
        result["watch_patterns"] = serde_json::json!(watch_patterns);
    }

    result.to_string()
}

// ---------------------------------------------------------------------------
// Cleanup thread
// ---------------------------------------------------------------------------

fn start_cleanup_thread() {
    static CLEANUP_STARTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if CLEANUP_STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return; // Already running
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(300)); // Check every 5 min
        cleanup_idle_envs();
    });
}

// ---------------------------------------------------------------------------
// Main handler
// ---------------------------------------------------------------------------

/// Handle terminal tool call.
pub fn handle_terminal(args: Value) -> Result<String, hermes_core::HermesError> {
    let command = match args.get("command").and_then(Value::as_str) {
        Some(c) if !c.trim().is_empty() => c.to_string(),
        _ => return Ok(tool_error("Terminal tool requires a non-empty 'command' parameter.")),
    };

    let background = args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(FOREGROUND_DEFAULT_TIMEOUT);

    let workdir = args
        .get("workdir")
        .and_then(Value::as_str)
        .map(String::from);

    let pty = args.get("pty").and_then(Value::as_bool).unwrap_or(false);

    let notify_on_complete = args
        .get("notify_on_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let watch_patterns: Vec<String> = args
        .get("watch_patterns")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let env_type_override = args
        .get("env_type")
        .or_else(|| args.get("backend"))
        .and_then(Value::as_str);

    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();

    // Validate workdir
    if let Some(ref dir) = workdir {
        if !is_valid_workdir(dir) {
            return Ok(tool_error(format!(
                "Invalid workdir '{dir}'. Workdir must be a safe path without shell metacharacters."
            )));
        }
    }

    // Start cleanup thread (idempotent)
    start_cleanup_thread();

    // Resolve environment config
    let env_config = resolve_env_config(env_type_override);
    let env_type = env_config.env_type.clone();

    // Background mode
    if background {
        let mut result_json = serde_json::Map::new();
        result_json.insert("success".to_string(), serde_json::json!(true));
        result_json.insert("action".to_string(), serde_json::json!("background"));

        let effective_cwd = workdir.as_deref().or(env_config.cwd.as_deref());

        if env_type == "local" {
            let bg_result = execute_background(
                &command,
                effective_cwd,
                notify_on_complete,
                watch_patterns,
                pty,
            );
            return Ok(bg_result);
        } else {
            // Non-local backend: spawn via environment
            let (env, _actual_type) = get_or_create_env(&task_id, &env_config);
            match spawn_via_env(&env, &command, effective_cwd, &task_id) {
                Ok(session_id) => {
                    result_json.insert("session_id".to_string(), serde_json::json!(session_id));
                    result_json.insert(
                        "note".to_string(),
                        serde_json::json!("Background process started in remote environment. Use 'process' tool to poll status."),
                    );
                    if notify_on_complete {
                        result_json.insert("notify_on_complete".to_string(), serde_json::json!(true));
                        set_process_notify_options(&session_id, true, watch_patterns.clone());
                    }
                    if !watch_patterns.is_empty() {
                        result_json.insert("watch_patterns".to_string(), serde_json::json!(watch_patterns));
                    }
                    return Ok(serde_json::Value::Object(result_json).to_string());
                }
                Err(e) => {
                    return Ok(tool_error(format!("Failed to start background process: {e}")));
                }
            }
        }
    }

    // Cap foreground timeout
    let timeout = timeout.min(FOREGROUND_MAX_TIMEOUT);

    // Reject timeout above max (nudge to background)
    if args.get("timeout").and_then(Value::as_u64).unwrap_or(0) > FOREGROUND_MAX_TIMEOUT {
        return Ok(tool_error(
            format!("Foreground timeout {}s exceeds the maximum of {}s. Use background=true with notify_on_complete=true for long-running commands.",
                timeout, FOREGROUND_MAX_TIMEOUT)
        ));
    }

    // Pre-exec security checks (tirith + dangerous command detection)
    // Skip check if force=true (user has confirmed they want to run it)
    let mut approval_note: Option<String> = None;
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    if !force {
        let approval = check_all_guards(&command, &env_type);
        if !approval.approved {
            if approval.status == "approval_required" {
                return Ok(serde_json::json!({
                    "output": "",
                    "exit_code": -1,
                    "error": approval.message.unwrap_or_else(|| "Waiting for user approval".to_string()),
                    "status": "approval_required",
                    "command": command,
                    "description": approval.description.unwrap_or_else(|| "command flagged".to_string()),
                    "pattern_key": approval.pattern_key.unwrap_or_default(),
                }).to_string());
            }
            let desc = approval
                .description
                .unwrap_or_else(|| "command flagged".to_string());
            let fallback_msg = format!(
                "Command denied: {desc}. Use the approval prompt to allow it, or rephrase the command."
            );
            return Ok(serde_json::json!({
                "output": "",
                "exit_code": -1,
                "error": approval.message.unwrap_or(fallback_msg),
                "status": "blocked"
            }).to_string());
        }
        if approval.user_approved {
            let desc = approval
                .description
                .unwrap_or_else(|| "flagged as dangerous".to_string());
            approval_note = Some(format!(
                "Command required approval ({desc}) and was approved by the user."
            ));
        } else if approval.smart_approved {
            let desc = approval
                .description
                .unwrap_or_else(|| "flagged as dangerous".to_string());
            approval_note = Some(format!(
                "Command was flagged ({desc}) and auto-approved by smart approval."
            ));
        }
    }

    // Sudo handling: transform command and get password for stdin
    let (command, sudo_stdin) = transform_sudo_command(&command);

    // PTY handling
    let mut pty_disabled_reason: Option<String> = None;
    let effective_pty = if pty && command_requires_pipe_stdin(&command) {
        pty_disabled_reason = Some(
            "PTY disabled for this command because it expects piped stdin/EOF \
             (for example gh auth login --with-token). For local background \
             processes, call process(action='close') after writing so it receives \
             EOF."
                .to_string(),
        );
        false
    } else {
        pty
    };

    // For local backend, use direct local execution
    if env_type == "local" {
        let result = if effective_pty {
            // PTY mode: use environment abstraction's PTY support
            let env = crate::environments::LocalEnvironment::new();
            let process_result =
                env.execute_pty(&command, workdir.as_deref(), Some(timeout));
            let mut combined = format!("{}{}", process_result.stdout, process_result.stderr);
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&format!(
                "[Process exited with code {}]\n",
                process_result.exit_code
            ));
            let stripped = crate::ansi_strip::strip_ansi(&combined);
            let truncated = truncate_output(&stripped);
            Ok(redact_secrets(&truncated))
        } else {
            execute_foreground_local(&command, timeout, workdir.as_deref(), sudo_stdin.as_deref())
        };

        match result {
            Ok(mut output) => {
                // Add helpful message for sudo failures in messaging context
                output = handle_sudo_failure(&output, &env_type);

                let exit_code = extract_exit_code_from_output(&output).unwrap_or(0);
                let exit_note = interpret_exit_code(&command, exit_code);

                let mut json = serde_json::json!({
                    "success": true,
                    "output": output,
                    "exit_code": exit_code,
                    "env_type": "local",
                });
                if let Some(note) = approval_note {
                    json["approval"] = serde_json::json!(note);
                }
                if let Some(note) = exit_note {
                    json["exit_code_meaning"] = serde_json::json!(note);
                }
                if let Some(reason) = pty_disabled_reason {
                    json["pty_note"] = serde_json::json!(reason);
                }
                Ok(json.to_string())
            }
            Err(e) => Ok(tool_error(redact_secrets(&e))),
        }
    } else {
        // Non-local backend: use environment abstraction
        let (env, actual_type) = get_or_create_env(&task_id, &env_config);

        match execute_foreground_via_env(
            env,
            &command,
            timeout,
            workdir.as_deref(),
            effective_pty,
        ) {
            Ok(mut output) => {
                // Add helpful message for sudo failures in messaging context
                output = handle_sudo_failure(&output, &env_type);

                let exit_code = extract_exit_code_from_output(&output).unwrap_or(0);
                let exit_note = interpret_exit_code(&command, exit_code);

                let mut json = serde_json::json!({
                    "success": true,
                    "output": output,
                    "exit_code": exit_code,
                    "env_type": actual_type,
                    "active_envs": active_env_count(),
                });
                if let Some(note) = approval_note {
                    json["approval"] = serde_json::json!(note);
                }
                if let Some(note) = exit_note {
                    json["exit_code_meaning"] = serde_json::json!(note);
                }
                if let Some(reason) = pty_disabled_reason {
                    json["pty_note"] = serde_json::json!(reason);
                }
                Ok(json.to_string())
            }
            Err(e) => Ok(tool_error(redact_secrets(&e))),
        }
    }
}

/// Extract exit code from the formatted output line `[Process exited with code N]`.
fn extract_exit_code_from_output(output: &str) -> Option<i32> {
    output
        .lines()
        .filter_map(|line| {
            let prefix = "[Process exited with code ";
            line.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|s| s.parse::<i32>().ok())
        })
        .next_back()
}

/// Register terminal tool.
pub fn register_terminal_tool(registry: &mut ToolRegistry) {
    registry.register(
        "terminal".to_string(),
        "terminal".to_string(),
        serde_json::json!({
            "name": "terminal",
            "description": "Execute shell commands. Use background=true for long-running processes. Optional: env_type='docker|ssh|modal|singularity|daytona' for remote execution. PTY mode for interactive CLI tools (local + SSH).",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute." },
                    "background": { "type": "boolean", "description": "Run in background with process tracking (default false)." },
                    "timeout": { "type": "integer", "description": "Max seconds to wait (default 60, max 600 for foreground)." },
                    "workdir": { "type": "string", "description": "Working directory override." },
                    "pty": { "type": "boolean", "description": "Run in pseudo-terminal (PTY) mode for interactive CLI tools like Codex, Claude Code, or Python REPL. Only works with local and SSH backends. Default: false.", "default": false },
                    "notify_on_complete": { "type": "boolean", "description": "When true (and background=true), auto-notify when the process finishes." },
                    "watch_patterns": { "type": "array", "items": { "type": "string" }, "description": "Strings to watch for in background process output. Fires a notification on first match per pattern." },
                    "force": { "type": "boolean", "description": "Skip dangerous command check (default false)." },
                    "task_id": { "type": "string", "description": "Task identifier for environment isolation (reuses sandbox/container)." },
                    "env_type": { "type": "string", "enum": ["local", "docker", "ssh", "modal", "singularity", "daytona"], "description": "Terminal backend override." },
                },
                "required": ["command"]
            }
        }),
        std::sync::Arc::new(handle_terminal),
        None,
        vec!["terminal".to_string()],
        "Execute shell commands".to_string(),
        "💻".to_string(),
        None,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workdir_validation() {
        assert!(is_valid_workdir("/home/user/project"), "unix path should be valid");
        let win_path = "C:\\Users\\test";
        assert!(is_valid_workdir(win_path), "windows path '{win_path}' should be valid");
        assert!(is_valid_workdir("./src"));
        assert!(!is_valid_workdir("../etc/passwd"));
        assert!(!is_valid_workdir("/tmp; rm -rf /"));
        assert!(!is_valid_workdir("/tmp|cat /etc/passwd"));
        assert!(!is_valid_workdir(""));
        assert!(!is_valid_workdir("$(whoami)"));
        assert!(!is_valid_workdir("/tmp`id`"));
    }

    #[test]
    fn test_truncate_output_small() {
        let input = "short output";
        assert_eq!(truncate_output(input), "short output");
    }

    #[test]
    fn test_truncate_output_large() {
        let head = "H".repeat(40_000);
        let tail = "T".repeat(40_000);
        let input = format!("{head}MIDDLE{tail}");
        let result = truncate_output(&input);
        assert!(result.contains("truncated"), "should have truncation marker");
        assert!(result.starts_with("HHHHH"), "should start with head");
        assert!(result.len() < input.len(), "should be shorter than input");
    }

    #[test]
    fn test_redact_secrets() {
        let input = "error with sk-abc1234567890abcdef key";
        let output = redact_secrets(input);
        assert!(!output.contains("sk-abc1234567890"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn test_handle_missing_command() {
        let result = handle_terminal(serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_handle_empty_command() {
        let result = handle_terminal(serde_json::json!({ "command": "   " }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_foreground_echo() {
        // Force local backend so the test works regardless of user config.
        let saved = std::env::var("HERMES_TERMINAL_BACKEND").ok();
        std::env::set_var("HERMES_TERMINAL_BACKEND", "local");

        let result = handle_terminal(serde_json::json!({
            "command": "echo hello"
        }));

        // Restore env
        if let Some(v) = saved {
            std::env::set_var("HERMES_TERMINAL_BACKEND", v);
        } else {
            std::env::remove_var("HERMES_TERMINAL_BACKEND");
        }

        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("success").is_some());
        let output = json.get("output").and_then(Value::as_str).unwrap_or("");
        assert!(output.contains("hello") || json.get("error").is_some(), "output: {output}");
    }

    #[test]
    fn test_background_starts() {
        let result = handle_terminal(serde_json::json!({
            "command": "sleep 5",
            "background": true
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        if json.get("success").and_then(Value::as_bool).unwrap_or(false) {
            assert!(json.get("session_id").is_some());
            assert!(json.get("pid").is_some());
        }
    }

    #[test]
    fn test_invalid_workdir_rejected() {
        let result = handle_terminal(serde_json::json!({
            "command": "echo test",
            "workdir": "/tmp; malicious"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_env_config_resolution() {
        // Without env vars, defaults to "local"
        let config = resolve_env_config(None);
        // May be "local" or whatever is in config file
        assert!(!config.env_type.is_empty());
    }

    #[test]
    fn test_env_config_override() {
        let config = resolve_env_config(Some("docker"));
        assert_eq!(config.env_type, "docker");
    }

    #[test]
    fn test_cleanup_idle_envs() {
        // Should not panic even with empty cache
        cleanup_idle_envs();
        assert_eq!(active_env_count(), 0);
    }

    #[test]
    fn test_env_type_in_response() {
        let result = handle_terminal(serde_json::json!({
            "command": "echo test"
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        // Local backend includes env_type in response
        if json.get("success").and_then(Value::as_bool).unwrap_or(false) {
            // Either env_type field or just success
            assert!(
                json.get("env_type").is_some() || json.get("output").is_some(),
                "response should have env_type or output"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Exit code context tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_interpret_exit_code_grep_no_matches() {
        assert_eq!(
            interpret_exit_code("grep foo file.txt", 1),
            Some("No matches found (not an error)".to_string())
        );
    }

    #[test]
    fn test_interpret_exit_code_diff() {
        assert_eq!(
            interpret_exit_code("diff a.txt b.txt", 1),
            Some("Files differ (expected, not an error)".to_string())
        );
    }

    #[test]
    fn test_interpret_exit_code_curl_timeout() {
        assert_eq!(
            interpret_exit_code("curl https://example.com", 28),
            Some("Operation timed out".to_string())
        );
    }

    #[test]
    fn test_interpret_exit_code_zero() {
        assert_eq!(interpret_exit_code("ls", 0), None);
    }

    #[test]
    fn test_interpret_exit_code_unknown() {
        assert_eq!(interpret_exit_code("ls", 1), None);
    }

    // -----------------------------------------------------------------------
    // Sudo handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_rewrite_real_sudo_invocations() {
        let (out, found) = rewrite_real_sudo_invocations("sudo apt-get update");
        assert!(found);
        assert!(out.contains("sudo -S -p ''"));
        assert!(!out.contains("sudo apt-get"));
    }

    #[test]
    fn test_rewrite_no_sudo() {
        let (out, found) = rewrite_real_sudo_invocations("apt-get update");
        assert!(!found);
        assert_eq!(out, "apt-get update");
    }

    #[test]
    fn test_transform_sudo_command_no_password() {
        std::env::remove_var("SUDO_PASSWORD");
        let cached = CACHED_SUDO_PASSWORD.lock().unwrap();
        // If no password available and not interactive, returns unchanged
        drop(cached);
        let (cmd, stdin) = transform_sudo_command("sudo ls");
        // Without SUDO_PASSWORD and not interactive, returns as-is with None stdin
        assert_eq!(cmd, "sudo ls");
        assert!(stdin.is_none());
    }

    #[test]
    fn test_transform_sudo_command_with_env_password() {
        std::env::set_var("SUDO_PASSWORD", "secret123");
        let (cmd, stdin) = transform_sudo_command("sudo ls");
        std::env::remove_var("SUDO_PASSWORD");
        assert!(cmd.contains("sudo -S -p ''"));
        assert_eq!(stdin, Some("secret123\n".to_string()));
    }

    #[test]
    fn test_command_requires_pipe_stdin() {
        assert!(command_requires_pipe_stdin("gh auth login --with-token"));
        assert!(!command_requires_pipe_stdin("gh auth login"));
        assert!(!command_requires_pipe_stdin("ls -la"));
    }

    #[test]
    fn test_looks_like_env_assignment() {
        assert!(looks_like_env_assignment("FOO=bar"));
        assert!(!looks_like_env_assignment("echo"));
        assert!(!looks_like_env_assignment("=foo"));
    }

    // -----------------------------------------------------------------------
    // Approval guard tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_all_guards_safe_command() {
        let result = check_all_guards("ls -la", "local");
        assert!(result.approved);
        assert_eq!(result.status, "approved");
    }

    #[test]
    fn test_check_all_guards_dangerous_local() {
        let result = check_all_guards("rm -rf /", "local");
        assert!(!result.approved);
        assert_eq!(result.status, "blocked");
        assert!(result.description.is_some());
    }

    #[test]
    fn test_check_all_guards_sandboxed_permissive() {
        // Sandboxed environments are more permissive
        let result = check_all_guards("rm -rf /some/path", "docker");
        // Should be approved in docker because it's not rm -rf / exactly
        assert!(result.approved);
    }

    #[test]
    fn test_check_all_guards_gateway_ask_mode() {
        std::env::set_var("HERMES_GATEWAY_ASK_MODE", "1");
        let result = check_all_guards("rm -rf /", "local");
        std::env::remove_var("HERMES_GATEWAY_ASK_MODE");
        assert!(!result.approved);
        assert_eq!(result.status, "approval_required");
    }

    #[test]
    fn test_extract_exit_code_from_output() {
        assert_eq!(
            extract_exit_code_from_output("hello\n[Process exited with code 42]"),
            Some(42)
        );
        assert_eq!(
            extract_exit_code_from_output("no exit code here"),
            None
        );
    }
}
