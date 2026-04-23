//! Browser Use cloud browser provider.
//!
//! Mirrors the Python `tools/browser_providers/browser_use.py`.
//! Supports both direct API key mode and managed Nous gateway mode.

use std::collections::HashMap;

use super::{CloudBrowserProvider, CloudSession, session_name};

/// BrowserUse configuration.
#[derive(Debug, Clone)]
pub struct BrowserUseConfig {
    /// Direct API key mode.
    pub api_key: Option<String>,
    /// Managed mode gateway URL (Nous gateway).
    pub managed_gateway_url: Option<String>,
    /// Managed mode auth token.
    pub managed_token: Option<String>,
    /// Base URL for direct mode.
    pub base_url: String,
    /// Session timeout in seconds (managed mode).
    pub session_timeout: u64,
}

impl Default for BrowserUseConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("BROWSER_USE_API_KEY").ok(),
            managed_gateway_url: None, // resolved at runtime via auth store
            managed_token: None,
            base_url: "https://api.browser-use.com/api/v3".to_string(),
            session_timeout: 5,
        }
    }
}

/// Browser Use cloud provider.
pub struct BrowserUseProvider {
    config: BrowserUseConfig,
    client: reqwest::Client,
}

impl BrowserUseProvider {
    pub fn new(config: BrowserUseConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(BrowserUseConfig::default())
    }

    fn base_url(&self) -> &str {
        if let Some(gateway) = &self.config.managed_gateway_url {
            gateway
        } else {
            &self.config.base_url
        }
    }

    fn is_managed(&self) -> bool {
        self.config.managed_gateway_url.is_some() && self.config.managed_token.is_some()
    }
}

#[async_trait::async_trait]
impl CloudBrowserProvider for BrowserUseProvider {
    fn provider_name(&self) -> &str {
        "Browser Use"
    }

    fn is_configured(&self) -> bool {
        self.config.api_key.is_some() || self.is_managed()
    }

    async fn create_session(&self, task_id: &str) -> Result<CloudSession, String> {
        let url = format!("{}/browsers", self.base_url());

        let mut req = self.client.post(&url);

        // Headers
        if let Some(ref api_key) = self.config.api_key {
            req = req.header("X-Browser-Use-API-Key", api_key);
        } else if let Some(ref token) = self.config.managed_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        // Idempotency key for managed mode
        if self.is_managed() {
            let idempotency_key = format!("browser-use-session-create:{task_id}");
            req = req.header("X-Idempotency-Key", &idempotency_key);
        }

        // Request body
        let body = if self.is_managed() {
            serde_json::json!({
                "timeout": self.config.session_timeout,
                "proxyCountryCode": "us"
            })
        } else {
            serde_json::json!({})
        };

        let resp = req
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("BrowserUse: request failed: {e}"))?;

        let status = resp.status();
        let headers = resp.headers().clone();
        let body_text = resp.text().await
            .map_err(|e| format!("BrowserUse: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("BrowserUse: API error {status}: {body_text}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body_text)
            .map_err(|e| format!("BrowserUse: parse error: {e}"))?;

        let session_id = data.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("BrowserUse: no session ID in response: {body_text}"))?;

        let cdp_url = data.get("cdpUrl")
            .or_else(|| data.get("connectUrl"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Managed mode: extract external_call_id from response header
        let _external_call_id = headers.get("x-external-call-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let mut features = HashMap::new();
        features.insert("managed".to_string(), self.is_managed());

        Ok(CloudSession {
            session_name: session_name(task_id),
            provider_session_id: session_id.to_string(),
            cdp_url,
            features,
        })
    }

    async fn close_session(&self, session_id: &str) -> bool {
        let url = format!("{}/browsers/{session_id}", self.base_url());

        let mut req = self.client.patch(&url);
        if let Some(ref api_key) = self.config.api_key {
            req = req.header("X-Browser-Use-API-Key", api_key);
        } else if let Some(ref token) = self.config.managed_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let resp = req
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "action": "stop" }))
            .send()
            .await;

        match resp {
            Ok(r) => r.status().is_success(),
            Err(e) => {
                tracing::warn!("BrowserUse: close session failed: {e}");
                false
            }
        }
    }

    async fn emergency_cleanup(&self, session_id: &str) {
        if !self.close_session(session_id).await {
            tracing::warn!("BrowserUse: emergency cleanup failed for session {session_id}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env_not_configured() {
        let config = BrowserUseConfig::default();
        let provider = BrowserUseProvider::new(config);
        // Without env vars or managed config, should not be configured
        assert!(!provider.is_configured());
    }

    #[test]
    fn test_managed_detection() {
        let config = BrowserUseConfig {
            managed_gateway_url: Some("http://localhost:8080".to_string()),
            managed_token: Some("test-token".to_string()),
            ..Default::default()
        };
        let provider = BrowserUseProvider::new(config);
        assert!(provider.is_managed());
        assert!(provider.is_configured());
    }

    #[test]
    fn test_direct_mode_configured() {
        let config = BrowserUseConfig {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };
        let provider = BrowserUseProvider::new(config);
        assert!(!provider.is_managed());
        assert!(provider.is_configured());
    }

    #[test]
    fn test_provider_name() {
        let config = BrowserUseConfig::default();
        let provider = BrowserUseProvider::new(config);
        assert_eq!(provider.provider_name(), "Browser Use");
    }
}
