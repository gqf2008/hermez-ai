//! Browser CDP Supervisor — dialog detection and auto-response.
//!
//! Connects to a browser's Chrome DevTools Protocol (CDP) WebSocket endpoint
//! and listens for `Page.javascriptDialogOpening` events. When a dialog
//! appears (alert, confirm, prompt, beforeunload), the supervisor
//! auto-responds based on the configured `DialogPolicy` instead of
//! letting the browser hang waiting for user input.
//!
//! Mirrors Python `browser_supervisor.SupervisorRegistry` (agent-browser).

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tracing;

/// Per-dialog-type response policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DialogPolicy {
    /// Auto-dismiss all dialogs (return false / send empty string for prompt).
    AutoDismiss,
    /// Auto-accept all dialogs (return true for confirm, empty for prompt).
    AutoAccept,
}

impl Default for DialogPolicy {
    fn default() -> Self {
        Self::AutoDismiss
    }
}

/// Handle to a running CDP supervisor task.
struct SupervisorHandle {
    /// Send a shutdown signal to the supervisor.
    shutdown_tx: oneshot::Sender<()>,
    /// Session name for logging.
    session_name: String,
}

/// Global registry of active CDP supervisors.
static SUPERVISOR_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, SupervisorHandle>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Start a CDP supervisor for a browser session.
///
/// Spawns a background task that connects to the CDP WebSocket at `cdp_url`
/// and monitors for dialog events. Auto-responds based on `policy`.
///
/// Returns the session name on success (used to stop the supervisor later).
pub fn start_supervisor(
    session_name: &str,
    cdp_url: &str,
    policy: DialogPolicy,
) -> Result<String, String> {
    let url = cdp_url.to_string();
    let name = session_name.to_string();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // Register before spawning to avoid race with stop_supervisor
    let mut registry = SUPERVISOR_REGISTRY.lock();
    if registry.contains_key(&name) {
        return Ok(name.clone()); // Already supervised
    }
    registry.insert(
        name.clone(),
        SupervisorHandle {
            shutdown_tx,
            session_name: name.clone(),
        },
    );
    drop(registry);

    let name_clone = name.clone();
    tokio::spawn(async move {
        if let Err(e) = run_supervisor(&name_clone, &url, policy, shutdown_rx).await {
            tracing::warn!("CDP supervisor for '{}' exited: {}", name_clone, e);
        }
        SUPERVISOR_REGISTRY.lock().remove(&name_clone);
    });

    tracing::info!("CDP supervisor started for session '{}'", name);
    Ok(name)
}

/// Stop a CDP supervisor for a browser session.
pub fn stop_supervisor(session_name: &str) {
    if let Some(handle) = SUPERVISOR_REGISTRY.lock().remove(session_name) {
        let _ = handle.shutdown_tx.send(());
        tracing::info!("CDP supervisor stopped for session '{}'", session_name);
    }
}

/// Stop all active CDP supervisors.
pub fn stop_all_supervisors() {
    let handles: Vec<_> = SUPERVISOR_REGISTRY.lock().drain().map(|(_, h)| h).collect();
    for handle in handles {
        let _ = handle.shutdown_tx.send(());
    }
}

/// Core supervisor event loop.
async fn run_supervisor(
    session_name: &str,
    cdp_url: &str,
    policy: DialogPolicy,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    // Connect to CDP WebSocket
    let (ws_stream, _) = connect_async(cdp_url)
        .await
        .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;

    let (mut write, mut read) = ws_stream.split();
    let mut msg_id: u64 = 1;

    // Enable Page domain to receive dialog events
    let enable_cmd = serde_json::json!({
        "id": msg_id,
        "method": "Page.enable",
        "params": {}
    });
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            enable_cmd.to_string(),
        ))
        .await
        .map_err(|e| format!("CDP send Page.enable failed: {e}"))?;
    msg_id += 1;

    // Enable Runtime domain for beforeunload handling
    let runtime_cmd = serde_json::json!({
        "id": msg_id,
        "method": "Runtime.enable",
        "params": {}
    });
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            runtime_cmd.to_string(),
        ))
        .await
        .map_err(|e| format!("CDP send Runtime.enable failed: {e}"))?;

    tracing::debug!(
        "CDP supervisor '{}': Page and Runtime domains enabled",
        session_name
    );

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                tracing::debug!("CDP supervisor '{}': shutdown requested", session_name);
                return Ok(());
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(event) = serde_json::from_str::<Value>(&text) {
                            let method = event.get("method").and_then(Value::as_str).unwrap_or("");
                            match method {
                                "Page.javascriptDialogOpening" => {
                                    let params = event.get("params").unwrap_or(&Value::Null);
                                    let dialog_type = params.get("type").and_then(Value::as_str).unwrap_or("alert");
                                    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                                    let default_prompt = params.get("defaultPrompt").and_then(Value::as_str).unwrap_or("");

                                    let (accept, prompt_text) = match dialog_type {
                                        "beforeunload" => (true, String::new()), // Always allow navigation
                                        "alert" => (false, String::new()),       // Just dismiss
                                        "confirm" => match policy {
                                            DialogPolicy::AutoAccept => (true, String::new()),
                                            DialogPolicy::AutoDismiss => (false, String::new()),
                                        },
                                        "prompt" => match policy {
                                            DialogPolicy::AutoAccept => (true, default_prompt.to_string()),
                                            DialogPolicy::AutoDismiss => (true, String::new()), // Send empty string
                                        },
                                        _ => (false, String::new()),
                                    };

                                    let response = serde_json::json!({
                                        "id": msg_id,
                                        "method": "Page.handleJavaScriptDialog",
                                        "params": {
                                            "accept": accept,
                                            "promptText": prompt_text
                                        }
                                    });
                                    msg_id += 1;

                                    let action = if accept { "accepted" } else { "dismissed" };
                                    tracing::debug!(
                                        "CDP supervisor '{}': {} dialog (type={}, message='{}')",
                                        session_name, action, dialog_type,
                                        &message[..message.len().min(100)]
                                    );

                                    if let Err(e) = write.send(
                                        tokio_tungstenite::tungstenite::Message::Text(response.to_string())
                                    ).await {
                                        tracing::warn!("CDP supervisor '{}': send failed: {}", session_name, e);
                                        return Err(format!("WebSocket send error: {e}"));
                                    }
                                }
                                _ => {
                                    // Ignore other CDP events
                                }
                            }
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                        return Ok(()); // Normal browser close
                    }
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    None => {
                        return Ok(()); // Stream ended
                    }
                    _ => {} // Ignore binary/ping/pong
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dialog_policy_default_is_dismiss() {
        assert_eq!(DialogPolicy::default(), DialogPolicy::AutoDismiss);
    }

    #[test]
    fn test_supervisor_registry_start_empty() {
        assert!(SUPERVISOR_REGISTRY.lock().is_empty());
    }
}
