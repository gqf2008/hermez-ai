#![allow(dead_code)]
//! Generic OAuth 2.0 flows: device code, authorization code, token refresh.
//!
//! Mirrors Python `auth.py` OAuth helpers:
//! - Device code flow (RFC 8628) — GitHub, Nous Portal
//! - Authorization code flow with PKCE — Google, Discord
//! - Token refresh with automatic expiry tracking

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::oauth_server::{CallbackResult, CallbackServer, redirect_url};
use crate::oauth_store::{store_credential, StoredCredential};

// ─── Errors ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum OAuthError {
    HttpError(String),
    JsonError(String),
    DeviceFlowFailed(String),
    DeviceFlowTimeout,
    AccessDenied,
    ExpiredToken,
    MissingConfig(String),
    StateMismatch,
    MissingCode,
    BrowserOpenFailed,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpError(msg) => write!(f, "HTTP error: {msg}"),
            Self::JsonError(msg) => write!(f, "JSON error: {msg}"),
            Self::DeviceFlowFailed(msg) => write!(f, "Device flow failed: {msg}"),
            Self::DeviceFlowTimeout => write!(f, "Device flow timed out"),
            Self::AccessDenied => write!(f, "Access denied by user"),
            Self::ExpiredToken => write!(f, "Token expired"),
            Self::MissingConfig(msg) => write!(f, "Missing config: {msg}"),
            Self::StateMismatch => write!(f, "OAuth state mismatch"),
            Self::MissingCode => write!(f, "Authorization code missing"),
            Self::BrowserOpenFailed => write!(f, "Failed to open browser"),
        }
    }
}

impl std::error::Error for OAuthError {}

// ─── Device Code Flow ───────────────────────────────────────────────────────

/// Response from the device code endpoint.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: Option<String>,
    #[serde(default)]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

/// Response from the access token poll endpoint.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceTokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub interval: u64,
}

/// Run the OAuth device code flow.
///
/// 1. Request device code from `device_code_url`
/// 2. Print instructions for the user
/// 3. Poll `token_url` until authorized or timeout
/// 4. Store tokens securely
///
/// `client_id` and `scope` are sent as form data.
pub async fn device_code_login(
    provider: &str,
    device_code_url: &str,
    token_url: &str,
    client_id: &str,
    scope: &str,
    timeout: Duration,
    no_browser: bool,
) -> Result<(), OAuthError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| OAuthError::HttpError(e.to_string()))?;

    // Step 1: request device code
    let params = [
        ("client_id", client_id),
        ("scope", scope),
    ];

    let device: DeviceCodeResponse = client
        .post(device_code_url)
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|e| OAuthError::HttpError(e.to_string()))?
        .json()
        .await
        .map_err(|e| OAuthError::JsonError(e.to_string()))?;

    if device.device_code.is_empty() || device.user_code.is_empty() {
        return Err(OAuthError::DeviceFlowFailed(
            "Provider did not return a device code".into(),
        ));
    }

    let verification_uri = device
        .verification_uri
        .clone()
        .unwrap_or_else(|| "https://github.com/login/device".to_string());

    println!();
    println!("  Open this URL in your browser: {}", verification_uri);
    println!("  Enter this code: {}", device.user_code);
    println!();

    // Try opening browser automatically (unless disabled)
    if !no_browser {
        let _ = try_open_browser(&format!("{}?user_code={}", verification_uri, device.user_code));
    }

    print!("  Waiting for authorization...");
    let _ = std::io::stdout().flush();

    // Step 2: poll for token
    let deadline = Instant::now() + timeout;
    let mut interval = device.interval.max(5);

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let poll_params = [
            ("client_id", client_id),
            ("device_code", &device.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];

        let resp_result = client
            .post(token_url)
            .header("Accept", "application/json")
            .form(&poll_params)
            .send()
            .await;

        let token_resp: DeviceTokenResponse = match resp_result {
            Ok(resp) => resp.json().await.map_err(|e| OAuthError::JsonError(e.to_string()))?,
            Err(_) => {
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
        };

        if let Some(token) = token_resp.access_token {
            println!(" ✓");

            let expires_at = token_resp.expires_in.map(|secs| {
                chrono::Utc::now() + chrono::Duration::seconds(secs as i64)
            });

            let cred = StoredCredential {
                provider: provider.to_string(),
                access_token: token,
                refresh_token: token_resp.refresh_token,
                expires_at,
                token_type: token_resp.token_type,
                scope: token_resp.scope,
            };
            store_credential(&cred).map_err(|e| OAuthError::JsonError(e.to_string()))?;

            println!("  {} Logged in to {}.", console::Style::new().green().apply_to("✓"), provider);
            return Ok(());
        }

        match token_resp.error.as_deref() {
            Some("authorization_pending") => {
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
            Some("slow_down") => {
                interval += 5;
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
            Some("expired_token") => {
                println!();
                return Err(OAuthError::ExpiredToken);
            }
            Some("access_denied") => {
                println!();
                return Err(OAuthError::AccessDenied);
            }
            Some(error) => {
                println!();
                return Err(OAuthError::DeviceFlowFailed(error.to_string()));
            }
            None => {
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
        }
    }

    println!();
    Err(OAuthError::DeviceFlowTimeout)
}

// ─── Authorization Code Flow (with local callback server) ───────────────────

/// Exchange an authorization code for tokens.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CodeTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
}

/// Run an authorization-code OAuth flow with a local callback server.
///
/// 1. Generate PKCE verifier + state
/// 2. Print authorization URL for the user
/// 3. Start local callback server
/// 4. Exchange code for tokens
/// 5. Store tokens securely
pub async fn authorization_code_login(
    provider: &str,
    authorize_url_template: &str,
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    scope: &str,
    timeout: Duration,
    no_browser: bool,
) -> Result<(), OAuthError> {
    let state = crate::oauth_server::generate_state();

    // Start callback server and get the actual bound port
    let server = CallbackServer::start(&state)
        .await
        .map_err(|e| OAuthError::HttpError(e.to_string()))?;
    let redirect = redirect_url(server.port);

    let auth_url = authorize_url_template
        .replace("{client_id}", client_id)
        .replace("{redirect_uri}", &urlencode(&redirect))
        .replace("{scope}", &urlencode(scope))
        .replace("{state}", &state);

    println!();
    println!("  Open this URL in your browser:");
    println!("  {}", auth_url);
    println!();

    // Try opening browser (unless disabled)
    if !no_browser {
        let _ = try_open_browser(&auth_url);
    }

    print!("  Waiting for authorization...");
    let _ = std::io::stdout().flush();

    let callback: CallbackResult = server.wait(timeout)
        .await
        .map_err(|e| OAuthError::HttpError(e.to_string()))?;

    if callback.code.starts_with("error:") {
        if callback.code == "error:state_mismatch" {
            return Err(OAuthError::StateMismatch);
        }
        if callback.code == "error:missing_code" {
            return Err(OAuthError::MissingCode);
        }
        return Err(OAuthError::AccessDenied);
    }

    // Exchange code for token
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| OAuthError::HttpError(e.to_string()))?;

    let mut params = HashMap::new();
    params.insert("grant_type", "authorization_code");
    params.insert("code", &callback.code);
    params.insert("redirect_uri", &redirect);
    params.insert("client_id", client_id);
    if let Some(secret) = client_secret {
        params.insert("client_secret", secret);
    }

    let token_resp: CodeTokenResponse = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| OAuthError::HttpError(e.to_string()))?
        .json()
        .await
        .map_err(|e| OAuthError::JsonError(e.to_string()))?;

    let expires_at = token_resp.expires_in.map(|secs| {
        chrono::Utc::now() + chrono::Duration::seconds(secs as i64)
    });

    let cred = StoredCredential {
        provider: provider.to_string(),
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        expires_at,
        token_type: token_resp.token_type,
        scope: token_resp.scope,
    };
    store_credential(&cred).map_err(|e| OAuthError::JsonError(e.to_string()))?;

    println!(" ✓");
    println!("  {} Logged in to {}.", console::Style::new().green().apply_to("✓"), provider);

    Ok(())
}

// ─── Token Refresh ──────────────────────────────────────────────────────────

/// Refresh an access token using a refresh token.
///
/// On success, updates the secure store with the new credentials.
pub async fn refresh_access_token(
    provider: &str,
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<String, OAuthError> {
    let stored = crate::oauth_store::retrieve_credential(provider)
        .map_err(|_| OAuthError::MissingConfig("No stored refresh token".into()))?;

    let refresh_token = stored
        .refresh_token
        .ok_or_else(|| OAuthError::MissingConfig("No refresh token available".into()))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| OAuthError::HttpError(e.to_string()))?;

    let mut params = HashMap::new();
    params.insert("grant_type", "refresh_token");
    params.insert("refresh_token", &refresh_token);
    params.insert("client_id", client_id);
    if let Some(secret) = client_secret {
        params.insert("client_secret", secret);
    }

    let token_resp: CodeTokenResponse = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| OAuthError::HttpError(e.to_string()))?
        .json()
        .await
        .map_err(|e| OAuthError::JsonError(e.to_string()))?;

    let expires_at = token_resp.expires_in.map(|secs| {
        chrono::Utc::now() + chrono::Duration::seconds(secs as i64)
    });

    let new_cred = StoredCredential {
        provider: provider.to_string(),
        access_token: token_resp.access_token.clone(),
        refresh_token: token_resp.refresh_token.or_else(|| Some(refresh_token.clone())),
        expires_at,
        token_type: token_resp.token_type,
        scope: token_resp.scope,
    };
    store_credential(&new_cred).map_err(|e| OAuthError::JsonError(e.to_string()))?;

    Ok(token_resp.access_token)
}

/// Check whether a token needs refreshing (within 2 min of expiry).
pub fn needs_refresh(cred: &StoredCredential) -> bool {
    match cred.expires_at {
        None => false,
        Some(expiry) => {
            let now = chrono::Utc::now();
            let skew = chrono::Duration::seconds(120);
            expiry - skew <= now
        }
    }
}

/// Resolve an access token, refreshing if necessary.
pub async fn resolve_access_token(
    provider: &str,
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<String, OAuthError> {
    let cred = crate::oauth_store::retrieve_credential(provider)
        .map_err(|_| OAuthError::MissingConfig("No stored credentials".into()))?;

    if needs_refresh(&cred) {
        refresh_access_token(provider, token_url, client_id, client_secret).await
    } else {
        Ok(cred.access_token)
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn try_open_browser(url: &str) -> Result<(), std::io::Error> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        if std::process::Command::new("xdg-open").arg(url).spawn().is_err() {
            let _ = std::process::Command::new("xdg-open").arg(url).output();
        }
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_refresh_no_expiry() {
        let cred = StoredCredential {
            provider: "test".into(),
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            token_type: None,
            scope: None,
        };
        assert!(!needs_refresh(&cred));
    }

    #[test]
    fn test_needs_refresh_future() {
        let cred = StoredCredential {
            provider: "test".into(),
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            token_type: None,
            scope: None,
        };
        assert!(!needs_refresh(&cred));
    }

    #[test]
    fn test_needs_refresh_past() {
        let cred = StoredCredential {
            provider: "test".into(),
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
            token_type: None,
            scope: None,
        };
        assert!(needs_refresh(&cred));
    }
}
