//! MCP OAuth 2.1 PKCE support for authenticated MCP servers.
//!
//! Implements the authorization-code flow with Proof Key for Code Exchange (PKCE)
//! for MCP servers that require authentication. Tokens are persisted to disk
//! for reuse across sessions.
//!
//! Mirrors Python `MCPOAuthManager` in tools/mcp_tool.py.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// OAuth configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthConfig {
    /// OAuth authorization endpoint URL.
    pub authorization_url: String,
    /// OAuth token endpoint URL.
    pub token_url: String,
    /// OAuth client ID.
    pub client_id: String,
    /// OAuth client secret (optional for public clients).
    pub client_secret: Option<String>,
    /// Local redirect port for the callback server.
    pub redirect_port: u16,
    /// Space-separated list of OAuth scopes to request.
    pub scopes: Option<String>,
}

/// Stored OAuth token state for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokenState {
    /// Access token for API requests.
    pub access_token: String,
    /// Refresh token for obtaining new access tokens.
    pub refresh_token: Option<String>,
    /// Token type (e.g., "Bearer").
    pub token_type: String,
    /// When the access token expires (epoch seconds).
    pub expires_at: Option<u64>,
    /// When this state was last updated.
    pub updated_at: String,
}

/// Generate a PKCE code verifier and challenge.
fn generate_pkce_pair() -> (String, String) {
    let verifier_bytes: [u8; 32] = rand::random();
    let verifier = base64_url_encode(&verifier_bytes);
    // SHA-256(verifier) using ring-compatible approach
    let challenge = {
        use sha2::Digest;
        let mut h = sha2::Sha256::default();
        h.update(verifier.as_bytes());
        base64_url_encode(&h.finalize())
    };
    (verifier, challenge)
}

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Start a one-shot HTTP server on localhost:port, wait for the OAuth callback,
/// extract the authorization code, and return it.
async fn listen_for_code(port: u16) -> Result<String, String> {
    use tokio::net::TcpListener;
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).await
        .map_err(|e| format!("Cannot bind to {addr}: {e}"))?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                if let Ok(n) = tokio::io::AsyncReadExt::read(&mut (&mut stream), &mut buf).await {
                    let request = String::from_utf8_lossy(&buf[..n]);
                    if let Some(code) = request.split("code=").nth(1)
                        .and_then(|s| s.split('&').next())
                        .and_then(|s| s.split_whitespace().next())
                    {
                        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nAuthorization complete. You may close this window.";
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                        let _ = tx.send(code.to_string());
                        break;
                    }
                }
            }
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(120), rx).await
        .map_err(|_| "OAuth authorization timed out (120s)".to_string())?
        .map_err(|_| "OAuth server error".to_string())
}

/// Manages OAuth authentication for MCP servers.
pub struct McpOAuthManager {
    /// Per-server OAuth configuration.
    configs: HashMap<String, McpOAuthConfig>,
    /// Per-server token state.
    tokens: HashMap<String, OAuthTokenState>,
    /// Storage directory for persisted tokens.
    storage_dir: PathBuf,
}

impl McpOAuthManager {
    /// Create a new OAuth manager.
    pub fn new() -> Self {
        let storage_dir = hermez_core::get_hermez_home().join("mcp-oauth");
        let _ = std::fs::create_dir_all(&storage_dir);
        Self {
            configs: HashMap::new(),
            tokens: HashMap::new(),
            storage_dir,
        }
    }

    /// Register an OAuth configuration for a server.
    pub fn register(&mut self, server_name: &str, config: McpOAuthConfig) {
        self.configs.insert(server_name.to_string(), config);
        // Try to load persisted token
        if let Some(token) = self.load_token(server_name) {
            self.tokens.insert(server_name.to_string(), token);
        }
    }

    /// Get a valid access token for a server (refreshing if needed).
    pub async fn get_access_token(&mut self, server_name: &str) -> Option<String> {
        let token = self.tokens.get(server_name)?;

        // Check if token is expired
        if let Some(expires_at) = token.expires_at {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if now >= expires_at.saturating_sub(60) {
                // Token expired — try refresh
                return self.refresh_token(server_name).await;
            }
        }

        Some(token.access_token.clone())
    }

    /// Start PKCE authorization flow: generate code verifier, open browser,
    /// start local redirect server, exchange code for token.
    pub async fn authorize(&mut self, server_name: &str) -> Result<(), String> {
        let config = self.configs.get(server_name)
            .ok_or_else(|| format!("No OAuth config for '{server_name}'"))?;
        let (verifier, challenge) = generate_pkce_pair();

        // Build authorization URL
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri=http://localhost:{}/callback&code_challenge={}&code_challenge_method=S256&state={}",
            config.authorization_url, config.client_id, config.redirect_port, challenge,
            &verifier[..8]
        );

        // Open browser for authorization
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::warn!("Failed to open browser for OAuth: {e}");
            tracing::info!("Please open this URL manually: {auth_url}");
        }

        // Start local redirect server
        let redirect_port = config.redirect_port;
        let code = listen_for_code(redirect_port).await?;

        // Exchange code for token
        let client = reqwest::Client::new();
        let mut params = HashMap::new();
        params.insert("grant_type", "authorization_code");
        params.insert("code", &code);
        params.insert("code_verifier", &verifier);
        params.insert("client_id", &config.client_id);
        let redirect_uri = format!("http://localhost:{redirect_port}/callback");
        params.insert("redirect_uri", &redirect_uri);
        if let Some(ref secret) = config.client_secret {
            params.insert("client_secret", secret);
        }

        let resp = client.post(&config.token_url).form(&params).send().await
            .map_err(|e| format!("Token request failed: {e}"))?;
        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("Token response parse failed: {e}"))?;

        let access_token = body.get("access_token").and_then(|v| v.as_str())
            .ok_or("No access_token in response")?.to_string();
        let refresh_token = body.get("refresh_token").and_then(|v| v.as_str()).map(String::from);
        let expires_in = body.get("expires_in").and_then(|v| v.as_u64());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_secs();

        let token = OAuthTokenState {
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_at: expires_in.map(|e| now + e),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        self.tokens.insert(server_name.to_string(), token.clone());
        self.save_token(server_name, &token);
        Ok(())
    }

    /// Refresh an expired access token.
    async fn refresh_token(&mut self, server_name: &str) -> Option<String> {
        let config = self.configs.get(server_name)?;
        let token = self.tokens.get(server_name)?;
        let refresh_token = token.refresh_token.as_ref()?;

        let client = reqwest::Client::new();
        let mut params = HashMap::new();
        params.insert("grant_type", "refresh_token");
        params.insert("refresh_token", refresh_token);
        params.insert("client_id", &config.client_id);
        if let Some(ref secret) = config.client_secret {
            params.insert("client_secret", secret);
        }

        let resp = client
            .post(&config.token_url)
            .form(&params)
            .send()
            .await
            .ok()?;

        let body: serde_json::Value = resp.json().await.ok()?;
        let access_token = body.get("access_token")?.as_str()?.to_string();
        let new_refresh = body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| token.refresh_token.clone());
        let expires_in = body.get("expires_in").and_then(|v| v.as_u64());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let new_token = OAuthTokenState {
            access_token: access_token.clone(),
            refresh_token: new_refresh,
            token_type: body
                .get("token_type")
                .and_then(|v| v.as_str())
                .unwrap_or("Bearer")
                .to_string(),
            expires_at: expires_in.map(|e| now + e),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };

        self.tokens.insert(server_name.to_string(), new_token.clone());
        self.save_token(server_name, &new_token);

        Some(access_token)
    }

    /// Load a persisted token from disk.
    fn load_token(&self, server_name: &str) -> Option<OAuthTokenState> {
        let path = self.token_path(server_name);
        let data = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Persist a token to disk.
    fn save_token(&self, server_name: &str, token: &OAuthTokenState) {
        let path = self.token_path(server_name);
        if let Ok(data) = serde_json::to_string_pretty(token) {
            let _ = std::fs::write(&path, data);
        }
    }

    /// Get the disk path for a server's token file.
    fn token_path(&self, server_name: &str) -> PathBuf {
        let safe_name = server_name.replace(['/', '\\', ' '], "_");
        self.storage_dir.join(format!("{safe_name}.json"))
    }
}

impl Default for McpOAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_manager_new() {
        let mgr = McpOAuthManager::new();
        assert!(mgr.configs.is_empty());
        assert!(mgr.tokens.is_empty());
    }

    #[test]
    fn test_register_config() {
        let mut mgr = McpOAuthManager::new();
        let config = McpOAuthConfig {
            authorization_url: "https://example.com/auth".to_string(),
            token_url: "https://example.com/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
            redirect_port: 8080,
            scopes: None,
        };
        mgr.register("test-server", config);
        assert!(mgr.configs.contains_key("test-server"));
    }

    #[test]
    fn test_token_path() {
        let mgr = McpOAuthManager::new();
        let path = mgr.token_path("my server");
        assert!(path.to_string_lossy().contains("my_server"));
    }
}
