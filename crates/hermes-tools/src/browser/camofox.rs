//! Camofox REST API client.
//!
//! Mirrors the Python `tools/browser_camofox.py`.
//! Camofox is a local anti-detection browser (Firefox fork with C++
//! fingerprint spoofing) that exposes a REST API.

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

/// Camofox session tracking info.
#[derive(Debug, Clone)]
pub struct CamofoxSession {
    pub user_id: String,
    pub tab_id: Option<String>,
    pub session_key: String,
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

        Ok(body)
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
        // Clear tab tracking
        session_info.tab_id = None;
        // Call the API to close
        let _ = self.close_session(user_id).await;
        tracing::debug!("Camofox: dropped session for user {user_id}");
    }

    /// Close browser and drop session. Returns status message.
    /// Mirrors Python: `camofox_close`.
    pub async fn camofox_close(&self, session: &mut CamofoxSession) -> String {
        let user_id = session.user_id.clone();
        if let Some(ref tab_id) = session.tab_id {
            // Close tab first
            let _ = self.close_session(tab_id).await;
        }
        self.drop_session(&user_id, session).await;
        format!("Camofox session closed for user {user_id}")
    }

    /// Get screenshots as base64 images.
    /// Mirrors Python: `camofox_get_images`.
    pub async fn camofox_get_images(&self, tab_id: &str, user_id: &str) -> Result<String, String> {
        let bytes = self.screenshot(tab_id, user_id).await?;
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        Ok(format!("data:image/png;base64,{encoded}"))
    }

    /// Print session state for debugging.
    /// Mirrors Python: `camofox_console`.
    pub fn camofox_console(&self, session: &CamofoxSession) -> String {
        format!(
            "Camofox Session:\n\
             \tUser ID:    {}\n\
             \tTab ID:     {}\n\
             \tSession Key: {}\n\
             \tConfig URL:  {}",
            session.user_id,
            session.tab_id.as_deref().unwrap_or("(none)"),
            session.session_key,
            self.config.url,
        )
    }
}

/// Health check response.
#[derive(Debug)]
pub struct CamofoxHealth {
    pub vnc_port: Option<u16>,
    #[allow(dead_code)]
    pub raw: serde_json::Value,
}

const CAMOFOX_STATE_DIR_NAME: &str = "browser_auth";
const CAMOFOX_STATE_SUBDIR: &str = "camofox";

/// Return the profile-scoped root directory for Camofox persistence.
/// Mirrors Python `get_camofox_state_dir()`.
pub fn get_camofox_state_dir() -> std::path::PathBuf {
    hermes_core::get_hermes_home()
        .join(CAMOFOX_STATE_DIR_NAME)
        .join(CAMOFOX_STATE_SUBDIR)
}

/// Return the stable Hermes-managed Camofox identity for this profile.
///
/// The user identity is profile-scoped (same Hermes profile = same userId).
/// The session key is scoped to the logical browser task so newly created
/// tabs within the same profile reuse the same identity contract.
///
/// Mirrors Python `get_camofox_identity()`.
pub fn get_camofox_identity(task_id: Option<&str>) -> serde_json::Value {
    let scope_root = get_camofox_state_dir().to_string_lossy().to_string();
    let logical_scope = task_id.unwrap_or("default");

    let user_id = derive_user_id(Some(&scope_root));
    let session_key = derive_session_key(logical_scope, Some(&scope_root));

    serde_json::json!({
        "user_id": user_id,
        "session_key": session_key,
    })
}

/// Derive a deterministic user_id from a profile path (state directory).
///
/// Profile-scoped: same Hermes profile always yields the same user_id,
/// regardless of task_id. This ensures Camofox maps to the same persistent
/// browser profile directory across restarts.
///
/// **BREAKING CHANGE (2026-04-17)**: Previously derived from both profile
/// path *and* task_id, causing a new browser profile on every new task.
/// Now purely profile-scoped to match Python behaviour and enable session
/// persistence across restarts.
///
/// Mirrors Python `uuid.uuid5(NAMESPACE_URL, f"camofox-user:{scope_root}").hex[:10]`.
pub fn derive_user_id(profile_path: Option<&str>) -> String {
    use uuid::Uuid;

    let scope = profile_path.unwrap_or("default");
    let input = format!("camofox-user:{scope}");
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, input.as_bytes());
    let hex = uuid.as_simple().to_string();
    format!("hermes_{}", &hex[..10])
}

/// Derive a session key from task_id and profile path.
/// Mirrors Python `uuid.uuid5(NAMESPACE_URL, f"camofox-session:{scope_root}:{logical_scope}").hex[:16]`.
pub fn derive_session_key(task_id: &str, profile_path: Option<&str>) -> String {
    use uuid::Uuid;

    let scope = profile_path.unwrap_or("default");
    let input = format!("camofox-session:{scope}:{task_id}");
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, input.as_bytes());
    let hex = uuid.as_simple().to_string();
    format!("task_{}", &hex[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_user_id_deterministic() {
        let id1 = derive_user_id(None);
        let id2 = derive_user_id(None);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("hermes_"));
    }

    #[test]
    fn test_derive_session_key_deterministic() {
        let key1 = derive_session_key("task-1", None);
        let key2 = derive_session_key("task-1", None);
        assert_eq!(key1, key2);
        assert!(key1.starts_with("task_"));
    }

    #[test]
    fn test_derive_user_id_profile_scoped() {
        // user_id is profile-scoped: same profile = same id regardless of task
        let id1 = derive_user_id(Some("/tmp/test-profile"));
        let id2 = derive_user_id(Some("/tmp/test-profile"));
        assert_eq!(id1, id2, "same profile should yield same user_id");

        // Different profiles should yield different ids
        let id3 = derive_user_id(Some("/tmp/other-profile"));
        assert_ne!(id1, id3, "different profiles should yield different user_ids");
    }

    #[test]
    fn test_derive_session_key_task_scoped() {
        // session_key is task-scoped
        let key1 = derive_session_key("task-a", None);
        let key2 = derive_session_key("task-b", None);
        assert_ne!(key1, key2, "different tasks should yield different session_keys");
    }

    #[test]
    fn test_get_camofox_state_dir() {
        let dir = get_camofox_state_dir();
        let path = dir.to_string_lossy();
        assert!(path.contains("browser_auth"));
        assert!(path.contains("camofox"));
    }

    #[test]
    fn test_get_camofox_identity() {
        let identity = get_camofox_identity(Some("my-task"));
        let user_id = identity.get("user_id").and_then(|v| v.as_str()).unwrap();
        let session_key = identity.get("session_key").and_then(|v| v.as_str()).unwrap();

        assert!(user_id.starts_with("hermes_"));
        assert!(session_key.starts_with("task_"));

        // Same task should yield same identity
        let identity2 = get_camofox_identity(Some("my-task"));
        assert_eq!(identity, identity2);
    }

    #[test]
    fn test_config_from_env() {
        let config = CamofoxConfig::default();
        let client = CamofoxClient::new(config);
        assert!(client.is_configured()); // default URL is non-empty
    }

    #[test]
    fn test_camofox_console_format() {
        let session = CamofoxSession {
            user_id: "test_user".to_string(),
            tab_id: Some("tab_123".to_string()),
            session_key: "key_abc".to_string(),
        };
        let config = CamofoxConfig::default();
        let client = CamofoxClient::new(config);
        let output = client.camofox_console(&session);
        assert!(output.contains("test_user"));
        assert!(output.contains("tab_123"));
        assert!(output.contains("key_abc"));
    }

    #[test]
    fn test_drop_session_clears_tab() {
        let mut session = CamofoxSession {
            user_id: "drop_test".to_string(),
            tab_id: Some("tab_456".to_string()),
            session_key: "key_xyz".to_string(),
        };
        // Run async code on a tokio runtime
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = CamofoxConfig::default();
            let client = CamofoxClient::new(config);
            let user_id = session.user_id.clone();
            let _ = client.drop_session(&user_id, &mut session).await;
        });
        assert!(session.tab_id.is_none(), "drop_session should clear tab_id");
    }
}
