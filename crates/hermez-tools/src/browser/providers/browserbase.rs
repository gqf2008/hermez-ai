//! Browserbase cloud browser provider.
//!
//! Mirrors the Python `tools/browser_providers/browserbase.py`.
//! Uses the Browserbase REST API to create/manage cloud browser sessions.

use std::collections::HashMap;

use super::{CloudBrowserProvider, CloudSession, session_name};

/// Browserbase session configuration.
#[derive(Debug, Clone)]
pub struct BrowserbaseConfig {
    pub api_key: String,
    pub project_id: String,
    pub base_url: String,
    pub use_proxies: bool,
    pub keep_alive: bool,
    pub advanced_stealth: bool,
    pub session_timeout_ms: Option<u64>,
}

impl Default for BrowserbaseConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("BROWSERBASE_API_KEY").unwrap_or_default(),
            project_id: std::env::var("BROWSERBASE_PROJECT_ID").unwrap_or_default(),
            base_url: std::env::var("BROWSERBASE_BASE_URL")
                .unwrap_or_else(|_| "https://api.browserbase.com".to_string()),
            use_proxies: std::env::var("BROWSERBASE_PROXIES")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            keep_alive: std::env::var("BROWSERBASE_KEEP_ALIVE")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            advanced_stealth: std::env::var("BROWSERBASE_ADVANCED_STEALTH")
                .map(|v| hermez_core::coerce_bool(&v))
                .unwrap_or(false),
            session_timeout_ms: std::env::var("BROWSERBASE_SESSION_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok()),
        }
    }
}

/// Browserbase cloud provider.
pub struct BrowserbaseProvider {
    config: BrowserbaseConfig,
    client: reqwest::Client,
}

impl BrowserbaseProvider {
    pub fn new(config: BrowserbaseConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(BrowserbaseConfig::default())
    }

    async fn create_session_inner(&self, config: &BrowserbaseConfig) -> Result<CloudSession, String> {
        let url = format!("{}/v1/sessions", config.base_url);

        let mut session_config = serde_json::json!({
            "projectId": config.project_id,
        });

        if config.keep_alive {
            session_config["keepAlive"] = serde_json::Value::Bool(true);
        }
        if config.use_proxies {
            session_config["proxies"] = serde_json::Value::Bool(true);
        }
        if let Some(timeout) = config.session_timeout_ms {
            session_config["timeout"] = serde_json::Value::Number(serde_json::Number::from(timeout));
        }
        if config.advanced_stealth {
            session_config["browserSettings"] = serde_json::json!({
                "advancedStealth": true
            });
        }

        let resp = self.client.post(&url)
            .header("X-BB-API-Key", &config.api_key)
            .header("Content-Type", "application/json")
            .json(&session_config)
            .send()
            .await
            .map_err(|e| format!("Browserbase: request failed: {e}"))?;

        // 402 fallback: if proxies/keepAlive require paid plan, retry without
        if resp.status().as_u16() == 402 && (config.use_proxies || config.keep_alive) {
            let mut fallback = config.clone();
            fallback.use_proxies = false;
            fallback.keep_alive = false;
            return Box::pin(self.create_session_inner(&fallback)).await;
        }

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Browserbase: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Browserbase: API error {status}: {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Browserbase: parse error: {e}"))?;

        let session_id = data.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Browserbase: no session ID in response: {body}"))?;

        let cdp_url = data.get("connectUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut features = HashMap::new();
        features.insert("proxies".to_string(), config.use_proxies);
        features.insert("keep_alive".to_string(), config.keep_alive);
        features.insert("advanced_stealth".to_string(), config.advanced_stealth);

        Ok(CloudSession {
            session_name: session_name(""), // caller overrides
            provider_session_id: session_id.to_string(),
            cdp_url,
            features,
        })
    }
}

#[async_trait::async_trait]
impl CloudBrowserProvider for BrowserbaseProvider {
    fn provider_name(&self) -> &str {
        "Browserbase"
    }

    fn is_configured(&self) -> bool {
        !self.config.api_key.is_empty() && !self.config.project_id.is_empty()
    }

    async fn create_session(&self, task_id: &str) -> Result<CloudSession, String> {
        let mut session = self.create_session_inner(&self.config).await?;
        session.session_name = session_name(task_id);
        Ok(session)
    }

    async fn close_session(&self, session_id: &str) -> bool {
        let url = format!("{}/v1/sessions/{session_id}", self.config.base_url);
        let resp = self.client.post(&url)
            .header("X-BB-API-Key", &self.config.api_key)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "projectId": self.config.project_id, "status": "REQUEST_RELEASE" }))
            .send()
            .await;

        match resp {
            Ok(r) => r.status().is_success(),
            Err(e) => {
                tracing::warn!("Browserbase: close session failed: {e}");
                false
            }
        }
    }

    async fn emergency_cleanup(&self, session_id: &str) {
        if !self.close_session(session_id).await {
            tracing::warn!("Browserbase: emergency cleanup failed for session {session_id}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env_not_configured() {
        let config = BrowserbaseConfig::default();
        let provider = BrowserbaseProvider::new(config);
        // Without env vars, should not be configured
        assert!(!provider.is_configured());
    }

    #[test]
    fn test_config_defaults() {
        let config = BrowserbaseConfig::default();
        assert!(config.use_proxies);
        assert!(config.keep_alive);
        assert!(!config.advanced_stealth);
        assert_eq!(config.base_url, "https://api.browserbase.com");
    }

    #[test]
    fn test_session_name() {
        let name = session_name("test-123");
        assert_eq!(name, "hermez-test-123");
    }
}
