//! Browser automation tools.
//!
//! Mirrors the Python `tools/browser_tool.py` + `browser_camofox.py`.
//! Supports three backends:
//! 1. **Camofox** — local anti-detection browser (REST API)
//! 2. **Cloud** — Browserbase, BrowserUse, Firecrawl (CDP via agent-browser)
//! 3. **Local** — agent-browser CLI (session-based)
//! 4. **Direct CDP** — user-supplied CDP endpoint

pub mod camofox;
pub mod providers;
pub mod resolver;
pub mod session;

use std::sync::Arc;

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

use self::resolver::BrowserBackend;
use self::session::BrowserSessionManager;

/// Global session manager — shared across all browser tool calls.
static SESSION_MANAGER: std::sync::LazyLock<Arc<BrowserSessionManager>> =
    std::sync::LazyLock::new(|| Arc::new(BrowserSessionManager::new()));

/// Resolve the browser backend (cached per process).
static RESOLVED_BACKEND: std::sync::LazyLock<BrowserBackend> =
    std::sync::LazyLock::new(|| resolver::resolve_backend(None));

/// Get the task_id from args, or use a default.
fn get_task_id(args: &Value) -> String {
    args.get("task_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string()
}

/// Run an async closure on the current tokio runtime.
/// Returns an error instead of panicking if the runtime cannot be created.
fn block_on_browser<F>(f: F) -> Result<String, hermes_core::HermesError>
where
    F: std::future::Future<Output = Result<String, String>>,
{
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return Err(hermes_core::HermesError::new(
                hermes_core::ErrorCategory::ToolError,
                "browser tool: no tokio runtime available".to_string(),
            ));
        }
    };
    match handle.block_on(f) {
        Ok(result) => Ok(result),
        Err(e) => Ok(tool_error(&e)),
    }
}

/// Handle browser tool call (dispatcher).
pub fn handle_browser_tool(args: Value) -> Result<String, hermes_core::HermesError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("navigate");

    handle_browser(action, &args)
}

/// Route to the appropriate browser action.
pub fn handle_browser(action: &str, args: &Value) -> Result<String, hermes_core::HermesError> {
    match action {
        "navigate" => browser_navigate(args),
        "snapshot" => browser_snapshot(args),
        "click" => browser_click(args),
        "type" => browser_type(args),
        "scroll" => browser_scroll(args),
        "back" => browser_back(args),
        "press" => browser_press(args),
        "get_images" => browser_get_images(args),
        "vision" => browser_vision(args),
        "console" => browser_console(args),
        _ => Ok(tool_error(format!(
            "Unknown browser action: '{action}'. Valid actions: navigate, snapshot, click, type, scroll, back, press, get_images, vision, console"
        ))),
    }
}

/// Build the agent-browser command args based on backend.
fn build_browser_args(extra_args: &[&str]) -> Vec<String> {
    let mut args = Vec::new();

    match &*RESOLVED_BACKEND {
        BrowserBackend::DirectCdp(cdp_url) => {
            args.push("--cdp".to_string());
            args.push(cdp_url.clone());
        }
        BrowserBackend::Cloud(_) => {
            // Cloud mode: session is created lazily by the navigate handler
            // For now, pass through with session name
        }
        BrowserBackend::Local => {
            // Local mode: use session name
        }
        BrowserBackend::Camofox => {
            // Camofox uses REST API, not agent-browser CLI
        }
    }

    for arg in extra_args {
        args.push(arg.to_string());
    }

    args
}

/// Run an `agent-browser` command and return (stdout, stderr, success).
fn run_agent_browser(args: &[&str]) -> Result<(String, String, bool), String> {
    let output = std::process::Command::new("agent-browser")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("agent-browser not available: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();

    Ok((stdout, stderr, success))
}

// --- Browser actions ---

fn browser_navigate(args: &Value) -> Result<String, hermes_core::HermesError> {
    let url = match args.get("url").and_then(Value::as_str) {
        Some(u) => u.to_string(),
        None => return Ok(tool_error("browser_navigate requires a 'url' parameter.")),
    };

    let task_id = get_task_id(args);

    // Camofox backend
    if matches!(&*RESOLVED_BACKEND, BrowserBackend::Camofox) {
        return browser_navigate_camofox(&url, &task_id);
    }

    // Cloud backend: create session if needed
    if let BrowserBackend::Cloud(provider) = &*RESOLVED_BACKEND {
        return browser_navigate_cloud(&url, &task_id, provider);
    }

    // Direct CDP or Local: use agent-browser CLI
    let cmd_args = build_browser_args(&["open", &url]);
    let str_args: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();

    match run_agent_browser(&str_args) {
        Ok((stdout, stderr, success)) => {
            if success {
                SESSION_MANAGER.register_local(&task_id, &format!("local-{task_id}"));
                Ok(serde_json::json!({
                    "success": true,
                    "action": "navigate",
                    "url": url,
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Navigate failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

/// Camofox navigate: create tab + navigate.
async fn camofox_navigate_async(url: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let user_id = camofox::derive_user_id(None);
    let tab_id = client.create_tab(&user_id).await?;
    let result = client.navigate(&tab_id, &url, &user_id).await?;

    SESSION_MANAGER.register_camofox(&task_id, &user_id, &tab_id);

    Ok(serde_json::json!({
        "success": true,
        "action": "navigate",
        "url": url,
        "result": result.trim(),
    })
    .to_string())
}

fn browser_navigate_camofox(url: &str, task_id: &str) -> Result<String, hermes_core::HermesError> {
    // Camofox requires async I/O; use a blocking runtime for simplicity
    let url = url.to_string();
    let task_id = task_id.to_string();
    block_on_browser(camofox_navigate_async(url, task_id))
}

/// Cloud navigate: create session + run agent-browser with --cdp.
async fn cloud_navigate_async(
    url: String,
    task_id: String,
    provider: Arc<dyn providers::CloudBrowserProvider>,
) -> Result<String, String> {
    let session = provider.create_session(&task_id).await?;
    let cdp_url = session.cdp_url.clone().ok_or_else(|| {
        format!("{}: no CDP URL returned", provider.provider_name())
    })?;

    SESSION_MANAGER.register_cloud(&task_id, &session);

    let output = tokio::process::Command::new("agent-browser")
        .arg("--cdp")
        .arg(&cdp_url)
        .arg("open")
        .arg(&url)
        .output()
        .await
        .map_err(|e| format!("agent-browser not available: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(serde_json::json!({
            "success": true,
            "action": "navigate",
            "url": url,
            "provider": provider.provider_name(),
            "result": stdout.trim(),
        })
        .to_string())
    } else {
        Err(format!("Navigate failed: {stderr}"))
    }
}

fn browser_navigate_cloud(
    url: &str,
    task_id: &str,
    provider: &Arc<dyn providers::CloudBrowserProvider>,
) -> Result<String, hermes_core::HermesError> {
    let url = url.to_string();
    let task_id = task_id.to_string();
    let provider = provider.clone();

    block_on_browser(cloud_navigate_async(url, task_id, provider))
}

fn browser_snapshot(args: &Value) -> Result<String, hermes_core::HermesError> {
    let task_id = get_task_id(args);
    let interactive = args.get("full").and_then(Value::as_bool).unwrap_or(false);

    // Camofox snapshot
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_snapshot_camofox(&task_id);
    }

    let cmd_args = if interactive {
        vec!["snapshot", "-i"]
    } else {
        vec!["snapshot"]
    };

    match run_agent_browser(&cmd_args) {
        Ok((stdout, stderr, success)) => {
            if success {
                let ref_count = stdout.matches("ref=").count();
                let line_count = stdout.lines().count();
                Ok(serde_json::json!({
                    "success": true,
                    "action": "snapshot",
                    "snapshot": stdout,
                    "elements": ref_count,
                    "lines": line_count,
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Snapshot failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

/// Camofox snapshot via accessibility tree.
async fn camofox_snapshot_async(task_id: String, _full: bool) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let snapshot = client.snapshot(&tab_id, &user_id).await?;
    let ref_count = snapshot.matches("ref=").count();
    let line_count = snapshot.lines().count();

    Ok(serde_json::json!({
        "success": true,
        "action": "snapshot",
        "snapshot": snapshot,
        "elements": ref_count,
        "lines": line_count,
    })
    .to_string())
}

fn browser_snapshot_camofox(task_id: &str) -> Result<String, hermes_core::HermesError> {
    let task_id = task_id.to_string();
    block_on_browser(camofox_snapshot_async(task_id, false))
}

fn browser_click(args: &Value) -> Result<String, hermes_core::HermesError> {
    let ref_id = match args.get("ref").and_then(Value::as_str) {
        Some(r) => r.to_string(),
        None => return Ok(tool_error("browser_click requires a 'ref' parameter (e.g. '@e5').")),
    };

    let task_id = get_task_id(args);

    // Camofox click
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_click_camofox(&ref_id, &task_id);
    }

    match run_agent_browser(&["click", &ref_id]) {
        Ok((stdout, stderr, success)) => {
            if success {
                Ok(serde_json::json!({
                    "success": true,
                    "action": "click",
                    "ref": ref_id,
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Click failed on '{ref_id}': {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_click_async(ref_id: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let result = client.click(&tab_id, &ref_id, &user_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "action": "click",
        "ref": ref_id,
        "result": result.trim(),
    })
    .to_string())
}

fn browser_click_camofox(ref_id: &str, task_id: &str) -> Result<String, hermes_core::HermesError> {
    let ref_id = ref_id.to_string();
    let task_id = task_id.to_string();
    block_on_browser(camofox_click_async(ref_id, task_id))
}

fn browser_type(args: &Value) -> Result<String, hermes_core::HermesError> {
    let ref_id = match args.get("ref").and_then(Value::as_str) {
        Some(r) => r.to_string(),
        None => return Ok(tool_error("browser_type requires a 'ref' parameter.")),
    };
    let text = match args.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => return Ok(tool_error("browser_type requires a 'text' parameter.")),
    };

    let task_id = get_task_id(args);

    // Camofox type
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_type_camofox(&ref_id, &text, &task_id);
    }

    match run_agent_browser(&["type", &ref_id, &text]) {
        Ok((stdout, stderr, success)) => {
            if success {
                Ok(serde_json::json!({
                    "success": true,
                    "action": "type",
                    "ref": ref_id,
                    "text": text,
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Type failed on '{ref_id}': {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_type_async(ref_id: String, text: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let result = client.type_text(&tab_id, &ref_id, &text, &user_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "action": "type",
        "ref": ref_id,
        "text": text,
        "result": result.trim(),
    })
    .to_string())
}

fn browser_type_camofox(ref_id: &str, text: &str, task_id: &str) -> Result<String, hermes_core::HermesError> {
    let ref_id = ref_id.to_string();
    let text = text.to_string();
    let task_id = task_id.to_string();
    block_on_browser(camofox_type_async(ref_id, text, task_id))
}

fn browser_scroll(args: &Value) -> Result<String, hermes_core::HermesError> {
    let direction = args.get("direction").and_then(Value::as_str).unwrap_or("down");

    let task_id = get_task_id(args);

    // Camofox scroll
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_scroll_camofox(direction, &task_id);
    }

    match run_agent_browser(&["scroll", direction]) {
        Ok((stdout, stderr, success)) => {
            if success {
                Ok(serde_json::json!({
                    "success": true,
                    "action": "scroll",
                    "direction": direction,
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Scroll failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_scroll_async(direction: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let result = client.scroll(&tab_id, &direction, &user_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "action": "scroll",
        "direction": direction,
        "result": result.trim(),
    })
    .to_string())
}

fn browser_scroll_camofox(direction: &str, task_id: &str) -> Result<String, hermes_core::HermesError> {
    let direction = direction.to_string();
    let task_id = task_id.to_string();
    block_on_browser(camofox_scroll_async(direction, task_id))
}

fn browser_back(_args: &Value) -> Result<String, hermes_core::HermesError> {
    let task_id = get_task_id(_args);

    // Camofox back
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_back_camofox(&task_id);
    }

    match run_agent_browser(&["back"]) {
        Ok((stdout, stderr, success)) => {
            if success {
                Ok(serde_json::json!({
                    "success": true,
                    "action": "back",
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Back failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_back_async(task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let result = client.back(&tab_id, &user_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "action": "back",
        "result": result.trim(),
    })
    .to_string())
}

fn browser_back_camofox(task_id: &str) -> Result<String, hermes_core::HermesError> {
    let task_id = task_id.to_string();
    block_on_browser(camofox_back_async(task_id))
}

fn browser_press(args: &Value) -> Result<String, hermes_core::HermesError> {
    let key = match args.get("key").and_then(Value::as_str) {
        Some(k) => k.to_string(),
        None => return Ok(tool_error("browser_press requires a 'key' parameter (e.g. 'Enter', 'Tab', 'Escape').")),
    };

    let task_id = get_task_id(args);

    // Camofox press
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_press_camofox(&key, &task_id);
    }

    match run_agent_browser(&["press", &key]) {
        Ok((stdout, stderr, success)) => {
            if success {
                Ok(serde_json::json!({
                    "success": true,
                    "action": "press",
                    "key": key,
                    "result": stdout.trim(),
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Press failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_press_async(key: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let result = client.press(&tab_id, &key, &user_id).await?;
    Ok(serde_json::json!({
        "success": true,
        "action": "press",
        "key": key,
        "result": result.trim(),
    })
    .to_string())
}

fn browser_press_camofox(key: &str, task_id: &str) -> Result<String, hermes_core::HermesError> {
    let key = key.to_string();
    let task_id = task_id.to_string();
    block_on_browser(camofox_press_async(key, task_id))
}

fn browser_get_images(_args: &Value) -> Result<String, hermes_core::HermesError> {
    match run_agent_browser(&["eval", "Array.from(document.querySelectorAll('img')).map(i => ({src: i.src, alt: i.alt, width: i.width, height: i.height}))"]) {
        Ok((stdout, stderr, success)) => {
            if success {
                let images: Value = serde_json::from_str(stdout.trim())
                    .unwrap_or(Value::Array(Vec::new()));
                let count = images.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(serde_json::json!({
                    "success": true,
                    "action": "get_images",
                    "images": images,
                    "count": count,
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Get images failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

fn browser_vision(args: &Value) -> Result<String, hermes_core::HermesError> {
    let question = match args.get("question").and_then(Value::as_str) {
        Some(q) => q.to_string(),
        None => return Ok(tool_error("browser_vision requires a 'question' parameter.")),
    };

    let temp_dir = std::env::temp_dir().join(format!(
        "hermes_browser_{}.png",
        std::process::id()
    ));
    let temp_path = temp_dir.to_string_lossy().to_string();

    // Camofox vision: screenshot via REST API
    if let BrowserBackend::Camofox = &*RESOLVED_BACKEND {
        return browser_vision_camofox(&question, &temp_path);
    }

    match run_agent_browser(&["screenshot", &temp_path]) {
        Ok((_stdout, stderr, success)) => {
            if success {
                let screenshot_data = std::fs::read(&temp_path)
                    .ok()
                    .map(|bytes| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes));

                let _ = std::fs::remove_file(&temp_path);

                Ok(serde_json::json!({
                    "success": true,
                    "action": "vision",
                    "question": question,
                    "screenshot": screenshot_data,
                    "note": "Screenshot captured. Use a vision model to analyze the image with the provided question.",
                })
                .to_string())
            } else {
                Ok(tool_error(format!("Screenshot failed: {stderr}")))
            }
        }
        Err(e) => Ok(tool_error(&e)),
    }
}

async fn camofox_vision_async(question: String, task_id: String) -> Result<String, String> {
    let client = camofox::CamofoxClient::from_env();
    let info = SESSION_MANAGER.get_session(&task_id)
        .ok_or_else(|| "No active Camofox session. Navigate to a URL first.".to_string())?;

    let user_id = info.camofox_user_id.ok_or("Missing user_id")?;
    let tab_id = info.camofox_tab_id.ok_or("Missing tab_id")?;

    let screenshot = client.screenshot(&tab_id, &user_id).await?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &screenshot);

    Ok(serde_json::json!({
        "success": true,
        "action": "vision",
        "question": question,
        "screenshot": encoded,
        "note": "Screenshot captured via Camofox. Use a vision model to analyze the image.",
    })
    .to_string())
}

fn browser_vision_camofox(question: &str, _temp_path: &str) -> Result<String, hermes_core::HermesError> {
    let question = question.to_string();
    let task_id = get_task_id(&serde_json::json!({}));
    block_on_browser(camofox_vision_async(question, task_id))
}

fn browser_console(args: &Value) -> Result<String, hermes_core::HermesError> {
    let expression = args.get("expression").and_then(Value::as_str);

    if let Some(expr) = expression {
        match run_agent_browser(&["eval", expr]) {
            Ok((stdout, stderr, success)) => {
                if success {
                    Ok(serde_json::json!({
                        "success": true,
                        "action": "console",
                        "expression": expr,
                        "result": stdout.trim(),
                    })
                    .to_string())
                } else {
                    Ok(tool_error(format!("Eval failed: {stderr}")))
                }
            }
            Err(e) => Ok(tool_error(&e)),
        }
    } else {
        match run_agent_browser(&["eval", "JSON.stringify({ errors: (window.__hermesConsole || []), logs: [] })"]) {
            Ok((stdout, _stderr, success)) => {
                if success {
                    Ok(serde_json::json!({
                        "success": true,
                        "action": "console",
                        "console_output": stdout.trim(),
                        "note": "Console output captured. No persistent console log available without page reload; use 'expression' to evaluate JS.",
                    })
                    .to_string())
                } else {
                    Ok(tool_error("Failed to read console output."))
                }
            }
            Err(e) => Ok(tool_error(&e)),
        }
    }
}

// --- Tool Registration ---

/// Register all browser tools.
pub fn register_browser_tools(registry: &mut ToolRegistry) {
    register_browser_navigate(registry);
    register_browser_snapshot(registry);
    register_browser_click(registry);
    register_browser_type(registry);
    register_browser_scroll(registry);
    register_browser_back(registry);
    register_browser_press(registry);
    register_browser_get_images(registry);
    register_browser_vision(registry);
    register_browser_console(registry);
}

fn register_browser_navigate(registry: &mut ToolRegistry) {
    registry.register(
        "browser_navigate".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_navigate",
            "description": "Navigate the browser to a URL. Must be called before other browser actions.",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to navigate to." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                },
                "required": ["url"]
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Navigate browser to URL".to_string(),
        "🌐".to_string(),
        None,
    );
}

fn register_browser_snapshot(registry: &mut ToolRegistry) {
    registry.register(
        "browser_snapshot".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_snapshot",
            "description": "Get an accessibility tree snapshot of the current page.",
            "parameters": {
                "type": "object",
                "properties": {
                    "full": { "type": "boolean", "description": "Get full snapshot (default false)." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                }
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Get page accessibility tree".to_string(),
        "📄".to_string(),
        None,
    );
}

fn register_browser_click(registry: &mut ToolRegistry) {
    registry.register(
        "browser_click".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_click",
            "description": "Click an element by its ref ID (e.g. '@e5') from a snapshot.",
            "parameters": {
                "type": "object",
                "properties": {
                    "ref": { "type": "string", "description": "Element ref ID (e.g. '@e5')." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                },
                "required": ["ref"]
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Click element by ref".to_string(),
        "👆".to_string(),
        None,
    );
}

fn register_browser_type(registry: &mut ToolRegistry) {
    registry.register(
        "browser_type".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_type",
            "description": "Type text into an input field by ref ID.",
            "parameters": {
                "type": "object",
                "properties": {
                    "ref": { "type": "string", "description": "Element ref ID." },
                    "text": { "type": "string", "description": "Text to type." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                },
                "required": ["ref", "text"]
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Type text into input".to_string(),
        "⌨️".to_string(),
        None,
    );
}

fn register_browser_scroll(registry: &mut ToolRegistry) {
    registry.register(
        "browser_scroll".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_scroll",
            "description": "Scroll the page up or down.",
            "parameters": {
                "type": "object",
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down"], "description": "Scroll direction." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                }
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Scroll page".to_string(),
        "📜".to_string(),
        None,
    );
}

fn register_browser_back(registry: &mut ToolRegistry) {
    registry.register(
        "browser_back".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_back",
            "description": "Navigate back in browser history.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                }
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Go back in history".to_string(),
        "⬅️".to_string(),
        None,
    );
}

fn register_browser_press(registry: &mut ToolRegistry) {
    registry.register(
        "browser_press".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_press",
            "description": "Press a keyboard key (Enter, Tab, Escape, etc.).",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Key to press (e.g. 'Enter', 'Tab', 'Escape')." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                },
                "required": ["key"]
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Press keyboard key".to_string(),
        "🔘".to_string(),
        None,
    );
}

fn register_browser_get_images(registry: &mut ToolRegistry) {
    registry.register(
        "browser_get_images".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_get_images",
            "description": "List images on the current page with URLs and alt text.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                }
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "List page images".to_string(),
        "🖼️".to_string(),
        None,
    );
}

fn register_browser_vision(registry: &mut ToolRegistry) {
    registry.register(
        "browser_vision".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_vision",
            "description": "Take a screenshot and analyze with vision AI (for CAPTCHAs, visual verification).",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "Question to ask about the screenshot." },
                    "annotate": { "type": "boolean", "description": "Add annotations to the screenshot (default false)." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                },
                "required": ["question"]
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "Screenshot + vision analysis".to_string(),
        "👁️".to_string(),
        None,
    );
}

fn register_browser_console(registry: &mut ToolRegistry) {
    registry.register(
        "browser_console".to_string(),
        "browser".to_string(),
        serde_json::json!({
            "name": "browser_console",
            "description": "Get JS console output or evaluate a JS expression.",
            "parameters": {
                "type": "object",
                "properties": {
                    "clear": { "type": "boolean", "description": "Clear console before reading." },
                    "expression": { "type": "string", "description": "JS expression to evaluate." },
                    "task_id": { "type": "string", "description": "Task identifier for session isolation." }
                }
            }
        }),
        std::sync::Arc::new(handle_browser_tool),
        None,
        vec!["browser".to_string()],
        "JS console / evaluate".to_string(),
        "🔧".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_navigate_missing_url() {
        let result = handle_browser("navigate", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_click_missing_ref() {
        let result = handle_browser("click", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_type_missing_params() {
        let result = handle_browser("type", &serde_json::json!({ "ref": "@e1" }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_press_missing_key() {
        let result = handle_browser("press", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_vision_missing_question() {
        let result = handle_browser("vision", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_unknown_action() {
        let result = handle_browser("restart", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_dispatcher_default_action() {
        let result = handle_browser_tool(serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_scroll_default_direction() {
        let result = handle_browser("scroll", &serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.is_object());
    }

    #[test]
    fn test_resolve_backend_default() {
        let backend = resolver::resolve_backend(None);
        assert!(matches!(backend, BrowserBackend::Local));
    }
}
