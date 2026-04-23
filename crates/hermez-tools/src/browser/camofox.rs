//! Camofox REST API client.
//!
//! Mirrors the Python `tools/browser_camofox.py`.
//! Camofox is a local anti-detection browser (Firefox fork with C++
//! fingerprint spoofing) that exposes a REST API.

use std::collections::HashMap;

use tokio::sync::Mutex;

use super::camofox_state;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default timeout per HTTP request (seconds).
const DEFAULT_TIMEOUT: u64 = 30;

/// Camofox client configuration.
#[derive(Debug, Clone)]
pub struct CamofoxConfig {
    /// REST API base URL (e.g. `http://localhost:9377`).
    pub url: String,
}

impl Default for CamofoxConfig {
    fn default() -> Self {
        Self {
            url: std::env::var("CAMOFOX_URL")
                .unwrap_or_else(|_| "http://localhost:9377".to_string()),
        }
    }
}

/// Return the configured Camofox server URL, or empty string.
pub fn get_camofox_url() -> String {
    std::env::var("CAMOFOX_URL").unwrap_or_default().trim_end_matches('/').to_string()
}

/// True when Camofox backend is configured and no CDP override is active.
///
/// When the user has explicitly connected to a live Chrome instance via
/// `BROWSER_CDP_URL`, the CDP connection takes priority over Camofox.
pub fn is_camofox_mode() -> bool {
    if std::env::var("BROWSER_CDP_URL").map(|s| !s.trim().is_empty()).unwrap_or(false) {
        return false;
    }
    !get_camofox_url().is_empty()
}

/// Camofox session tracking info.
#[derive(Debug, Clone)]
pub struct CamofoxSession {
    pub user_id: String,
    pub tab_id: Option<String>,
    pub session_key: String,
    pub managed: bool,
}

/// Camofox REST API client.
pub struct CamofoxClient {
    config: CamofoxConfig,
    client: reqwest::Client,
}

impl CamofoxClient {
    pub fn new(config: CamofoxConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(CamofoxConfig::default())
    }

    /// Check if Camofox is available.
    pub fn is_configured(&self) -> bool {
        !self.config.url.is_empty()
    }

    /// Build the base URL.
    fn base(&self) -> &str {
        &self.config.url
    }

    /// Create a new tab. Returns `tabId`.
    pub async fn create_tab(&self, user_id: &str) -> Result<String, String> {
        let url = format!("{}/tabs", self.base());
        let resp = self.client.post(&url)
            .json(&serde_json::json!({ "userId": user_id }))
            .send()
            .await
            .map_err(|e| format!("Camofox: create tab failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: create tab failed ({status}): {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Camofox: parse error: {e}"))?;

        data.get("tabId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Camofox: no tabId in response: {body}"))
    }

    /// Create a new tab with session key and initial URL.
    pub async fn create_tab_with_url(
        &self,
        user_id: &str,
        session_key: &str,
        url: &str,
    ) -> Result<String, String> {
        let endpoint = format!("{}/tabs", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({
                "userId": user_id,
                "sessionKey": session_key,
                "url": url,
            }))
            .send()
            .await
            .map_err(|e| format!("Camofox: create tab failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: create tab failed ({status}): {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Camofox: parse error: {e}"))?;

        data.get("tabId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Camofox: no tabId in response: {body}"))
    }

    /// Navigate a tab to a URL.
    pub async fn navigate(&self, tab_id: &str, url: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/navigate", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id, "url": url }))
            .send()
            .await
            .map_err(|e| format!("Camofox: navigate failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: navigate failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Get accessibility tree snapshot.
    pub async fn snapshot(&self, tab_id: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/snapshot?userId={user_id}", self.base());
        let resp = self.client.get(&endpoint)
            .send()
            .await
            .map_err(|e| format!("Camofox: snapshot failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: snapshot failed ({status}): {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Camofox: parse error: {e}"))?;

        Ok(data.get("snapshot").and_then(|v| v.as_str()).unwrap_or(&body).to_string())
    }

    /// Click element by ref.
    pub async fn click(&self, tab_id: &str, ref_id: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/click", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id, "ref": ref_id }))
            .send()
            .await
            .map_err(|e| format!("Camofox: click failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: click failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Type text into element.
    pub async fn type_text(&self, tab_id: &str, ref_id: &str, text: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/type", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id, "ref": ref_id, "text": text }))
            .send()
            .await
            .map_err(|e| format!("Camofox: type failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: type failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Scroll page.
    pub async fn scroll(&self, tab_id: &str, direction: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/scroll", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id, "direction": direction }))
            .send()
            .await
            .map_err(|e| format!("Camofox: scroll failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: scroll failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Go back in history.
    pub async fn back(&self, tab_id: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/back", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id }))
            .send()
            .await
            .map_err(|e| format!("Camofox: back failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: back failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Press keyboard key.
    pub async fn press(&self, tab_id: &str, key: &str, user_id: &str) -> Result<String, String> {
        let endpoint = format!("{}/tabs/{tab_id}/press", self.base());
        let resp = self.client.post(&endpoint)
            .json(&serde_json::json!({ "userId": user_id, "key": key }))
            .send()
            .await
            .map_err(|e| format!("Camofox: press failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: press failed ({status}): {body}"));
        }

        Ok(body)
    }

    /// Take a screenshot.
    pub async fn screenshot(&self, tab_id: &str, user_id: &str) -> Result<Vec<u8>, String> {
        let endpoint = format!("{}/tabs/{tab_id}/screenshot?userId={user_id}", self.base());
        let resp = self.client.get(&endpoint)
            .send()
            .await
            .map_err(|e| format!("Camofox: screenshot failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Camofox: screenshot failed ({status}): {body}"));
        }

        resp.bytes().await
            .map(|b| b.to_vec())
            .map_err(|e| format!("Camofox: read screenshot failed: {e}"))
    }

    /// Health check — returns VNC port if available.
    pub async fn health(&self) -> Result<CamofoxHealth, String> {
        let url = format!("{}/health", self.base());
        let resp = self.client.get(&url)
            .send()
            .await
            .map_err(|e| format!("Camofox: health check failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Camofox: read health failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Camofox: health check failed ({status}): {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Camofox: parse error: {e}"))?;

        Ok(CamofoxHealth {
            vnc_port: data.get("vncPort").and_then(|v| v.as_u64()).map(|v| v as u16),
            raw: data,
        })
    }

    /// Close session (soft cleanup).
    pub async fn close_session(&self, user_id: &str) -> bool {
        let url = format!("{}/sessions/{user_id}", self.base());
        let resp = self.client.delete(&url).send().await;
        match resp {
            Ok(r) => r.status().is_success() || r.status().as_u16() == 404,
            Err(e) => {
                tracing::warn!("Camofox: close session failed: {e}");
                false
            }
        }
    }

    /// Drop session — clear in-memory state and close the browser session.
    /// Mirrors Python: `_drop_session` + API close.
    pub async fn drop_session(&self, user_id: &str, session_info: &mut CamofoxSession) {
        session_info.tab_id = None;
        let _ = self.close_session(user_id).await;
        tracing::debug!("Camofox: dropped session for user {user_id}");
    }

    /// Close browser and drop session. Returns status message.
    /// Mirrors Python: `camofox_close`.
    pub async fn camofox_close(&self, session: &mut CamofoxSession) -> String {
        let user_id = session.user_id.clone();
        if let Some(ref tab_id) = session.tab_id {
            let _ = self.close_session(tab_id).await;
        }
        self.drop_session(&user_id, session).await;
        format!("Camofox session closed for user {user_id}")
    }
}

/// Health check response.
#[derive(Debug)]
pub struct CamofoxHealth {
    pub vnc_port: Option<u16>,
    #[allow(dead_code)]
    pub raw: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Session management (tokio::sync::Mutex)
// ---------------------------------------------------------------------------

static CAMOFOX_SESSIONS: std::sync::LazyLock<Mutex<HashMap<String, CamofoxSession>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Check if managed persistence is enabled for Camofox.
/// Mirrors Python `_managed_persistence_enabled`.
fn managed_persistence_enabled() -> bool {
    std::env::var("CAMOFOX_MANAGED_PERSISTENCE")
        .map(|v| hermez_core::coerce_bool(&v))
        .unwrap_or(false)
}

/// Get or create a Camofox session for the given task.
/// Mirrors Python `_get_session`.
pub async fn get_session_entry(task_id: Option<&str>) -> CamofoxSession {
    let task_id = task_id.unwrap_or("default");
    let mut sessions = CAMOFOX_SESSIONS.lock().await;
    if let Some(entry) = sessions.get(task_id) {
        return entry.clone();
    }
    let entry = if managed_persistence_enabled() {
        let identity = camofox_state::get_camofox_identity(Some(task_id));
        let user_id = identity
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let session_key = identity
            .get("session_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        CamofoxSession {
            user_id,
            tab_id: None,
            session_key,
            managed: true,
        }
    } else {
        let uuid_str = uuid::Uuid::new_v4().as_simple().to_string();
        CamofoxSession {
            user_id: format!("hermez_{}", &uuid_str[..10]),
            tab_id: None,
            session_key: format!("task_{}", &task_id[..task_id.len().min(16)]),
            managed: false,
        }
    };
    sessions.insert(task_id.to_string(), entry.clone());
    entry
}

/// Remove and return session info.
/// Mirrors Python `_drop_session`.
pub async fn drop_session_entry(task_id: Option<&str>) -> Option<CamofoxSession> {
    let task_id = task_id.unwrap_or("default");
    CAMOFOX_SESSIONS.lock().await.remove(task_id)
}

/// Ensure a tab exists for the session, creating one if needed.
/// Mirrors Python `_ensure_tab`.
pub async fn ensure_tab(
    client: &CamofoxClient,
    task_id: Option<&str>,
    url: &str,
) -> Result<CamofoxSession, String> {
    let mut entry = get_session_entry(task_id).await;
    if entry.tab_id.is_some() {
        return Ok(entry);
    }
    let tab_id = client
        .create_tab_with_url(&entry.user_id, &entry.session_key, url)
        .await?;
    entry.tab_id = Some(tab_id);
    CAMOFOX_SESSIONS
        .lock()
        .await
        .insert(task_id.unwrap_or("default").to_string(), entry.clone());
    Ok(entry)
}

/// Soft cleanup — release in-memory session without destroying server-side context.
/// Mirrors Python `camofox_soft_cleanup`.
pub async fn camofox_soft_cleanup(task_id: Option<&str>) -> bool {
    if managed_persistence_enabled() {
        drop_session_entry(task_id).await;
        tracing::debug!("Camofox soft cleanup for task {:?} (managed persistence)", task_id);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// High-level helpers
// ---------------------------------------------------------------------------

/// Take a screenshot and analyze it with vision AI via Camofox.
/// Mirrors Python `camofox_vision`.
pub async fn camofox_vision(
    client: &CamofoxClient,
    tab_id: &str,
    user_id: &str,
    question: &str,
    annotate: bool,
) -> Result<serde_json::Value, String> {
    let screenshot = client.screenshot(tab_id, user_id).await?;
    let img_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &screenshot);

    let mut annotation_context = String::new();
    if annotate {
        match client.snapshot(tab_id, user_id).await {
            Ok(snapshot) => {
                let truncated = if snapshot.len() > 3000 {
                    format!("{}...", &snapshot[..3000])
                } else {
                    snapshot
                };
                annotation_context = format!(
                    "\n\nAccessibility tree (element refs for interaction):\n{truncated}"
                );
            }
            Err(e) => tracing::debug!("Annotation snapshot failed: {e}"),
        }
    }

    let vision_prompt = format!(
        "Analyze this browser screenshot and answer: {question}{annotation_context}"
    );

    let messages = vec![serde_json::json!({
        "role": "user",
        "content": [
            {"type": "text", "text": vision_prompt},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{img_b64}")}},
        ],
    })];

    let vision_timeout = std::env::var("CAMOFOX_VISION_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    let response = hermez_llm::auxiliary_client::async_call_llm(
        Some("vision"),
        None,
        None,
        None,
        None,
        messages,
        None,
        None,
        None,
        Some(vision_timeout),
        None,
    )
    .await;

    let analysis = match response {
        Ok(resp) => resp.content.trim().to_string(),
        Err(e) => {
            tracing::warn!("Vision LLM call failed: {e}");
            return Err(format!("Vision analysis failed: {e}"));
        }
    };

    Ok(serde_json::json!({
        "success": true,
        "analysis": analysis,
    }))
}

/// Get images on the current page via Camofox.
/// Mirrors Python `camofox_get_images`.
pub async fn camofox_get_images(
    client: &CamofoxClient,
    tab_id: &str,
    user_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let snapshot = client.snapshot(tab_id, user_id).await?;
    let lines: Vec<&str> = snapshot.lines().collect();
    let mut images = Vec::new();

    let re_alt = regex::Regex::new(r#"img\s+"([^"]*)""#).unwrap();
    let re_url = regex::Regex::new(r#"/url:\s*(\S+)"#).unwrap();

    for i in 0..lines.len() {
        let stripped = lines[i].trim();
        if stripped.starts_with("- img ") || stripped.starts_with("img ") {
            let alt = re_alt
                .captures(stripped)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or("");

            let mut src = "";
            if i + 1 < lines.len() {
                let next = lines[i + 1].trim();
                if let Some(cap) = re_url.captures(next) {
                    src = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                }
            }

            if !alt.is_empty() || !src.is_empty() {
                images.push(serde_json::json!({
                    "src": src,
                    "alt": alt,
                }));
            }
        }
    }

    Ok(images)
}

/// Get console output — limited support in Camofox.
/// Mirrors Python `camofox_console`.
pub fn camofox_console() -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "console_messages": [],
        "js_errors": [],
        "total_messages": 0,
        "total_errors": 0,
        "note": "Console log capture is not available with the Camofox backend. \
                Use browser_snapshot or browser_vision to inspect page state.",
    })
}

// Re-export derive helpers for backward compatibility with existing callers.
#[allow(unused_imports)]
pub use camofox_state::derive_user_id;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env() {
        let config = CamofoxConfig::default();
        let client = CamofoxClient::new(config);
        assert!(client.is_configured());
    }

    #[test]
    fn test_is_camofox_mode_without_cdp() {
        // Can't easily test env var interactions, but verify the function compiles
        let _ = is_camofox_mode();
    }

    #[test]
    fn test_camofox_console_format() {
        let output = camofox_console();
        assert!(output.get("note").is_some());
        assert_eq!(
            output.get("total_messages").and_then(|v| v.as_u64()),
            Some(0)
        );
    }

    #[test]
    fn test_derive_user_id_via_reexport() {
        let id1 = derive_user_id(None);
        let id2 = derive_user_id(None);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("hermez_"));
    }

    #[test]
    fn test_managed_persistence_default() {
        // Default should be false unless env var is set
        assert!(!managed_persistence_enabled());
    }
}
