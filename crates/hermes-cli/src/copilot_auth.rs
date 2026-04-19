//! GitHub Copilot authentication utilities.
//!
//! Implements the OAuth device code flow used by the Copilot CLI and handles
//! token validation/exchange for the Copilot API.
//!
//! Mirrors the Python `hermes_cli/copilot_auth.py`.
//!
//! Token type support (per GitHub docs):
//! - `gho_`          OAuth token           ✓ (default via copilot login)
//! - `github_pat_`   Fine-grained PAT      ✓ (needs Copilot Requests permission)
//! - `ghu_`          GitHub App token      ✓ (via environment variable)
//! - `ghp_`          Classic PAT           ✗ NOT SUPPORTED
//!
//! Credential search order (matching Copilot CLI behaviour):
//! 1. `COPILOT_GITHUB_TOKEN` env var
//! 2. `GH_TOKEN` env var
//! 3. `GITHUB_TOKEN` env var
//! 4. `gh auth token` CLI fallback

use std::collections::HashMap;

#[cfg(test)]
use serial_test::serial;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

// OAuth device code flow constants (same client ID as opencode/Copilot CLI)
const COPILOT_OAUTH_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const CLASSIC_PAT_PREFIX: &str = "ghp_";
#[allow(dead_code)]
const SUPPORTED_PREFIXES: &[&str] = &["gho_", "github_pat_", "ghu_"];

/// Env var search order (matches Copilot CLI).
const COPILOT_ENV_VARS: &[&str] = &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

const DEVICE_CODE_POLL_INTERVAL: u64 = 5;
const DEVICE_CODE_POLL_SAFETY_MARGIN: u64 = 3;

/// Errors that can occur during Copilot authentication.
#[derive(Debug, Clone, PartialEq)]
pub enum CopilotAuthError {
    /// Classic PAT is not supported.
    ClassicPatUnsupported,
    /// Token validation failed with a message.
    InvalidToken(String),
    /// No token could be resolved.
    NoTokenFound,
    /// OAuth device flow failed.
    DeviceFlowFailed(String),
    /// OAuth device flow timed out.
    DeviceFlowTimeout,
    /// Authorization was denied by the user.
    AccessDenied,
    /// Device code expired.
    ExpiredToken,
    /// HTTP request error.
    HttpError(String),
}

impl std::fmt::Display for CopilotAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClassicPatUnsupported => write!(
                f,
                "Classic Personal Access Tokens (ghp_*) are not supported by the Copilot API. \
                 Use one of:\n  → `copilot login` or `hermes model` to authenticate via OAuth\n  \
                 → A fine-grained PAT (github_pat_*) with Copilot Requests permission\n  \
                 → `gh auth login` with the default device code flow (produces gho_* tokens)"
            ),
            Self::InvalidToken(msg) => write!(f, "Invalid token: {}", msg),
            Self::NoTokenFound => write!(f, "No Copilot token found"),
            Self::DeviceFlowFailed(msg) => write!(f, "Device flow failed: {}", msg),
            Self::DeviceFlowTimeout => write!(f, "Timed out waiting for authorization"),
            Self::AccessDenied => write!(f, "Authorization was denied"),
            Self::ExpiredToken => write!(f, "Device code expired"),
            Self::HttpError(msg) => write!(f, "HTTP error: {}", msg),
        }
    }
}

impl std::error::Error for CopilotAuthError {}

/// Validate that a token is usable with the Copilot API.
///
/// Returns `Ok(())` if valid, or `Err(CopilotAuthError)` with details.
pub fn validate_copilot_token(token: &str) -> Result<(), CopilotAuthError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(CopilotAuthError::InvalidToken("Empty token".to_string()));
    }

    if token.starts_with(CLASSIC_PAT_PREFIX) {
        return Err(CopilotAuthError::ClassicPatUnsupported);
    }

    Ok(())
}

/// Resolve a GitHub token suitable for Copilot API use.
///
/// Returns `Some((token, source))` where `source` describes where the token came from,
/// or `None` if no token is available.
///
/// # Errors
/// Returns an error if a classic PAT is found (to warn the user).
pub fn resolve_copilot_token() -> Result<(String, &'static str), CopilotAuthError> {
    // 1. Check env vars in priority order
    for env_var in COPILOT_ENV_VARS {
        if let Ok(val) = std::env::var(env_var) {
            let val = val.trim();
            if !val.is_empty() {
                if let Err(e) = validate_copilot_token(val) {
                    tracing::warn!(
                        "Token from {} is not supported: {}",
                        env_var,
                        e
                    );
                    continue;
                }
                return Ok((val.to_string(), env_var));
            }
        }
    }

    // 2. Fall back to `gh auth token`
    if let Some(token) = try_gh_cli_token() {
        validate_copilot_token(&token)?;
        return Ok((token, "gh auth token"));
    }

    Err(CopilotAuthError::NoTokenFound)
}

/// Return candidate `gh` binary paths, including common installs.
fn gh_cli_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    if let Ok(exe) = which::which("gh") {
        if let Some(s) = exe.to_str() {
            candidates.push(s.to_string());
        }
    }

    for candidate in [
        "/opt/homebrew/bin/gh",
        "/usr/local/bin/gh",
    ] {
        if candidates.contains(&candidate.to_string()) {
            continue;
        }
        if Path::new(candidate).is_file() {
            candidates.push(candidate.to_string());
        }
    }

    // ~/.local/bin/gh
    if let Some(home) = dirs::home_dir() {
        let local_gh = home.join(".local").join("bin").join("gh");
        if let Some(s) = local_gh.to_str() {
            if local_gh.is_file() && !candidates.contains(&s.to_string()) {
                candidates.push(s.to_string());
            }
        }
    }

    candidates
}

/// Return a token from `gh auth token` when the GitHub CLI is available.
///
/// When `COPILOT_GH_HOST` is set, passes `--hostname` so gh returns the
/// correct host's token. Strips `GITHUB_TOKEN` / `GH_TOKEN` from the subprocess
/// environment so `gh` reads from its own credential store (hosts.yml) instead
/// of just echoing the env var back.
fn try_gh_cli_token() -> Option<String> {
    let hostname = std::env::var("COPILOT_GH_HOST").unwrap_or_default();

    // Build a clean env so gh doesn't short-circuit on GITHUB_TOKEN / GH_TOKEN
    let clean_env: HashMap<String, String> = std::env::vars()
        .filter(|(k, _)| k != "GITHUB_TOKEN" && k != "GH_TOKEN")
        .collect();

    for gh_path in gh_cli_candidates() {
        let mut cmd = std::process::Command::new(&gh_path);
        cmd.arg("auth").arg("token");
        if !hostname.is_empty() {
            cmd.arg("--hostname").arg(&hostname);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(&clean_env);

        match cmd.output() {
            Ok(output) => {
                if output.status.success() {
                    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !token.is_empty() {
                        return Some(token);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("gh CLI token lookup failed ({}): {}", gh_path, e);
            }
        }
    }

    None
}

// ─── OAuth Device Code Flow ────────────────────────────────────────────────

/// Device code response from GitHub.
#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: Option<String>,
    interval: Option<u64>,
}

/// Access token poll response from GitHub.
#[derive(Debug, Clone, serde::Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

/// Run the GitHub OAuth device code flow for Copilot.
///
/// Prints instructions for the user, polls for completion, and returns
/// the OAuth access token on success.
///
/// This replicates the flow used by opencode and the Copilot CLI.
pub async fn copilot_device_code_login(
    host: &str,
    timeout: Duration,
) -> Result<String, CopilotAuthError> {
    let domain = host.trim_end_matches('/');
    let device_code_url = format!("https://{}/login/device/code", domain);
    let access_token_url = format!("https://{}/login/oauth/access_token", domain);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| CopilotAuthError::HttpError(e.to_string()))?;

    // Step 1: Request device code
    let params = [
        ("client_id", COPILOT_OAUTH_CLIENT_ID),
        ("scope", "read:user"),
    ];

    let device_data: DeviceCodeResponse = client
        .post(&device_code_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "HermesAgent/1.0")
        .form(&params)
        .send()
        .await
        .map_err(|e| CopilotAuthError::HttpError(e.to_string()))?
        .json()
        .await
        .map_err(|e| CopilotAuthError::HttpError(e.to_string()))?;

    let verification_uri = device_data
        .verification_uri
        .unwrap_or_else(|| "https://github.com/login/device".to_string());
    let user_code = device_data.user_code;
    let device_code = device_data.device_code;
    let mut interval = device_data.interval.unwrap_or(DEVICE_CODE_POLL_INTERVAL).max(1);

    if device_code.is_empty() || user_code.is_empty() {
        return Err(CopilotAuthError::DeviceFlowFailed(
            "GitHub did not return a device code".to_string(),
        ));
    }

    // Step 2: Show instructions
    println!();
    println!("  Open this URL in your browser: {}", verification_uri);
    println!("  Enter this code: {}", user_code);
    println!();
    print!("  Waiting for authorization...");
    let _ = std::io::stdout().flush();

    // Step 3: Poll for completion
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval + DEVICE_CODE_POLL_SAFETY_MARGIN)).await;

        let poll_params = [
            ("client_id", COPILOT_OAUTH_CLIENT_ID),
            ("device_code", device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];

        let resp_result = client
            .post(&access_token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", "HermesAgent/1.0")
            .form(&poll_params)
            .send()
            .await;

        let result: Result<AccessTokenResponse, reqwest::Error> = match resp_result {
            Ok(resp) => resp.json().await,
            Err(_) => {
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
        };

        match result {
            Ok(token_resp) => {
                if let Some(token) = token_resp.access_token {
                    println!(" ✓");
                    return Ok(token);
                }

                match token_resp.error.as_deref() {
                    Some("authorization_pending") => {
                        print!(".");
                        let _ = std::io::stdout().flush();
                        continue;
                    }
                    Some("slow_down") => {
                        // RFC 8628: add 5 seconds to polling interval
                        interval = token_resp.interval.unwrap_or(interval + 5);
                        print!(".");
                        let _ = std::io::stdout().flush();
                        continue;
                    }
                    Some("expired_token") => {
                        println!();
                        return Err(CopilotAuthError::ExpiredToken);
                    }
                    Some("access_denied") => {
                        println!();
                        return Err(CopilotAuthError::AccessDenied);
                    }
                    Some(error) => {
                        println!();
                        return Err(CopilotAuthError::DeviceFlowFailed(error.to_string()));
                    }
                    None => {
                        print!(".");
                        let _ = std::io::stdout().flush();
                        continue;
                    }
                }
            }
            Err(_) => {
                print!(".");
                let _ = std::io::stdout().flush();
                continue;
            }
        }
    }

    println!();
    Err(CopilotAuthError::DeviceFlowTimeout)
}

// ─── Copilot API Headers ───────────────────────────────────────────────────

/// Build the standard headers for Copilot API requests.
///
/// Replicates the header set used by opencode and the Copilot CLI.
pub fn copilot_request_headers(
    is_agent_turn: bool,
    is_vision: bool,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "Editor-Version".to_string(),
        "vscode/1.104.1".to_string(),
    );
    headers.insert(
        "User-Agent".to_string(),
        "HermesAgent/1.0".to_string(),
    );
    headers.insert(
        "Copilot-Integration-Id".to_string(),
        "vscode-chat".to_string(),
    );
    headers.insert(
        "Openai-Intent".to_string(),
        "conversation-edits".to_string(),
    );
    headers.insert(
        "x-initiator".to_string(),
        if is_agent_turn { "agent" } else { "user" }.to_string(),
    );

    if is_vision {
        headers.insert(
            "Copilot-Vision-Request".to_string(),
            "true".to_string(),
        );
    }

    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial]
    fn test_validate_empty_token() {
        assert!(matches!(
            validate_copilot_token(""),
            Err(CopilotAuthError::InvalidToken(_))
        ));
    }

    #[test]
    #[serial]
    fn test_validate_classic_pat_rejected() {
        assert_eq!(
            validate_copilot_token("ghp_abc123"),
            Err(CopilotAuthError::ClassicPatUnsupported)
        );
    }

    #[test]
    #[serial]
    fn test_validate_oauth_token_ok() {
        assert!(validate_copilot_token("gho_abc123").is_ok());
    }

    #[test]
    #[serial]
    fn test_validate_fine_grained_pat_ok() {
        assert!(validate_copilot_token("github_pat_abc123").is_ok());
    }

    #[test]
    #[serial]
    fn test_validate_app_token_ok() {
        assert!(validate_copilot_token("ghu_abc123").is_ok());
    }

    #[test]
    #[serial]
    fn test_resolve_no_env_no_gh() {
        // Temporarily clear env vars so gh CLI fallback is the only source.
        let saved: Vec<(String, Option<String>)> = COPILOT_ENV_VARS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), Some(v))))
            .collect();
        for k in COPILOT_ENV_VARS {
            std::env::remove_var(k);
        }

        let result = resolve_copilot_token();
        // If gh CLI is available and authenticated, it may return a valid token
        // even without env vars. Accept either outcome.
        match result {
            Ok((token, source)) => {
                assert!(!token.is_empty());
                // Source may be "gh auth token" or an env var if another test
                // set it concurrently (env is process-global).
                assert!(
                    source == "gh auth token"
                        || source == "COPILOT_GITHUB_TOKEN"
                        || source == "GH_TOKEN"
                        || source == "GITHUB_TOKEN",
                    "unexpected source: {source}"
                );
            }
            Err(e) => {
                assert!(matches!(e, CopilotAuthError::NoTokenFound));
            }
        }

        // Restore
        for (k, v) in saved {
            if let Some(val) = v {
                std::env::set_var(k, val);
            }
        }
    }

    #[test]
    #[serial]
    fn test_resolve_from_env_var() {
        // Clear all copilot env vars first to avoid priority conflicts.
        let saved: Vec<(String, Option<String>)> = COPILOT_ENV_VARS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), Some(v))))
            .collect();
        for k in COPILOT_ENV_VARS {
            std::env::remove_var(k);
        }

        std::env::set_var("GH_TOKEN", "gho_test_token_123");
        let result = resolve_copilot_token();
        assert!(result.is_ok());
        let (token, source) = result.unwrap();
        assert_eq!(token, "gho_test_token_123");
        assert_eq!(source, "GH_TOKEN");

        std::env::remove_var("GH_TOKEN");
        // Restore any previously saved vars
        for (k, v) in saved {
            if let Some(val) = v {
                std::env::set_var(k, val);
            }
        }
    }

    #[test]
    #[serial]
    fn test_copilot_request_headers() {
        let headers = copilot_request_headers(true, false);
        assert_eq!(headers.get("Editor-Version"), Some(&"vscode/1.104.1".to_string()));
        assert_eq!(headers.get("x-initiator"), Some(&"agent".to_string()));
        assert!(!headers.contains_key("Copilot-Vision-Request"));

        let headers_vision = copilot_request_headers(false, true);
        assert_eq!(headers_vision.get("x-initiator"), Some(&"user".to_string()));
        assert_eq!(
            headers_vision.get("Copilot-Vision-Request"),
            Some(&"true".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_error_display() {
        let err = CopilotAuthError::DeviceFlowTimeout;
        assert!(err.to_string().contains("Timed out"));
    }
}
