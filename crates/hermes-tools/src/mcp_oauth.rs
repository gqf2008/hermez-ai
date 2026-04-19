#![allow(dead_code)]
//! MCP OAuth 2.1 Client Support (stub).
//!
//! Implements the browser-based OAuth 2.1 authorization code flow with PKCE
//! for MCP servers that require OAuth authentication instead of static bearer
//! tokens.
//!
//! The full Python implementation uses the MCP SDK's `OAuthClientProvider`
//! which handles discovery, dynamic client registration, PKCE, token exchange,
//! refresh, and step-up authorization. This Rust module provides the glue
//! structure for future integration.
//!
//! Configuration in config.yaml:
//! ```yaml
//! mcp_servers:
//!   my_server:
//!     url: "https://mcp.example.com/mcp"
//!     auth: oauth
//!     oauth:
//!       client_id: "pre-registered-id"  # skip dynamic registration
//!       client_secret: "secret"          # confidential clients only
//!       scope: "read write"              # default: server-provided
//!       redirect_port: 0                 # 0 = auto-pick free port
//!       client_name: "My Custom Client"  # default: "Hermes Agent"
//! ```

use std::path::PathBuf;

use hermes_core::hermes_home::get_hermes_home;

/// OAuth configuration for an MCP server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpOAuthConfig {
    /// Pre-registered client ID (skip dynamic registration).
    pub client_id: Option<String>,
    /// Pre-registered client secret (confidential clients only).
    pub client_secret: Option<String>,
    /// OAuth scopes to request.
    pub scope: Option<String>,
    /// Port for the local callback server (0 = auto).
    pub redirect_port: Option<u16>,
    /// Custom client name for dynamic registration.
    pub client_name: Option<String>,
    /// Request timeout in seconds.
    pub timeout: Option<u64>,
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            scope: None,
            redirect_port: Some(0),
            client_name: Some("Hermes Agent".to_string()),
            timeout: Some(300),
        }
    }
}

/// Return the directory for MCP OAuth token files.
///
/// Layout: `HERMES_HOME/mcp-tokens/`
fn get_token_dir() -> PathBuf {
    get_hermes_home().join("mcp-tokens")
}

/// Sanitize a server name for use as a filename.
fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(128)
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Path to the token file for a server.
fn token_path(server_name: &str) -> PathBuf {
    let name = safe_filename(server_name);
    get_token_dir().join(format!("{name}.json"))
}

/// Path to the client info file for a server.
fn client_info_path(server_name: &str) -> PathBuf {
    let name = safe_filename(server_name);
    get_token_dir().join(format!("{name}.client.json"))
}

/// Delete stored OAuth tokens and client info for a server.
pub fn remove_oauth_tokens(server_name: &str) {
    let _ = std::fs::remove_file(token_path(server_name));
    let _ = std::fs::remove_file(client_info_path(server_name));
}

/// Check if we have cached tokens for a server.
pub fn has_cached_tokens(server_name: &str) -> bool {
    token_path(server_name).exists()
}

/// Find an available TCP port on localhost.
pub fn find_free_port() -> Option<u16> {
    use std::net::TcpListener;
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// Check if the environment looks interactive (has a TTY).
pub fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Build the redirect URI for the OAuth callback.
pub fn build_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_filename() {
        assert_eq!(safe_filename("my_server"), "my_server");
        assert_eq!(safe_filename("my/server:8080"), "my_server_8080");
    }

    #[test]
    fn test_build_redirect_uri() {
        let uri = build_redirect_uri(8080);
        assert_eq!(uri, "http://127.0.0.1:8080/callback");
    }

    #[test]
    fn test_find_free_port() {
        let port = find_free_port();
        assert!(port.is_some());
        assert!(port.unwrap() > 0);
    }
}
