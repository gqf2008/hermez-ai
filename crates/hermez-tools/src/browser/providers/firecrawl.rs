//! Firecrawl cloud browser provider.
//!
//! Mirrors the Python `tools/browser_providers/firecrawl.py`.
//! Simple provider with minimal config.

use std::collections::HashMap;

use super::{CloudBrowserProvider, CloudSession, session_name};

/// Firecrawl configuration.
#[derive(Debug, Clone)]
pub struct FirecrawlConfig {
    pub api_key: String,
    pub base_url: String,
    /// Browser session TTL in seconds.
    pub ttl: u64,
}

impl Default for FirecrawlConfig {
    fn default() -> Self {
        Self {
            api_key: std::env::var("FIRECRAWL_API_KEY").unwrap_or_default(),
            base_url: std::env::var("FIRECRAWL_API_URL")
                .unwrap_or_else(|_| "https://api.firecrawl.dev".to_string()),
            ttl: std::env::var("FIRECRAWL_BROWSER_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        }
    }
}

/// Firecrawl cloud provider.
pub struct FirecrawlProvider {
    config: FirecrawlConfig,
    client: reqwest::Client,
}

impl FirecrawlProvider {
    pub fn new(config: FirecrawlConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(FirecrawlConfig::default())
    }
}

#[async_trait::async_trait]
impl CloudBrowserProvider for FirecrawlProvider {
    fn provider_name(&self) -> &str {
        "Firecrawl"
    }

    fn is_configured(&self) -> bool {
        !self.config.api_key.is_empty()
    }

    async fn create_session(&self, task_id: &str) -> Result<CloudSession, String> {
        let url = format!("{}/v2/browser", self.config.base_url);

        let resp = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "ttl": self.config.ttl }))
            .send()
            .await
            .map_err(|e| format!("Firecrawl: request failed: {e}"))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| format!("Firecrawl: read response failed: {e}"))?;

        if !status.is_success() {
            return Err(format!("Firecrawl: API error {status}: {body}"));
        }

        let data: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Firecrawl: parse error: {e}"))?;

        let session_id = data.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Firecrawl: no session ID in response: {body}"))?;

        let cdp_url = data.get("cdpUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut features = HashMap::new();
        features.insert("ttl".to_string(), true);

        Ok(CloudSession {
            session_name: session_name(task_id),
            provider_session_id: session_id.to_string(),
            cdp_url,
            features,
        })
    }

    async fn close_session(&self, session_id: &str) -> bool {
        let url = format!("{}/v2/browser/{session_id}", self.config.base_url);

        let resp = self.client.delete(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .send()
            .await;

        match resp {
            Ok(r) => r.status().is_success() || r.status().as_u16() == 404,
            Err(e) => {
                tracing::warn!("Firecrawl: close session failed: {e}");
                false
            }
        }
    }

    async fn emergency_cleanup(&self, session_id: &str) {
        if !self.close_session(session_id).await {
            tracing::warn!("Firecrawl: emergency cleanup failed for session {session_id}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env_not_configured() {
        let config = FirecrawlConfig::default();
        let provider = FirecrawlProvider::new(config);
        assert!(!provider.is_configured());
    }

    #[test]
    fn test_config_defaults() {
        let config = FirecrawlConfig::default();
        assert_eq!(config.base_url, "https://api.firecrawl.dev");
        assert_eq!(config.ttl, 300);
    }

    #[test]
    fn test_provider_name() {
        let config = FirecrawlConfig::default();
        let provider = FirecrawlProvider::new(config);
        assert_eq!(provider.provider_name(), "Firecrawl");
    }
}
