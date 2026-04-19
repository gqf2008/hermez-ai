#![allow(dead_code)]
//! OAuth callback server for authorization-code flows.
//!
//! Spins up a temporary HTTP server on localhost, captures the authorization
//! code from the provider's redirect, and verifies the `state` parameter to
//! mitigate CSRF.
//!
//! Mirrors Python `auth.py` local callback server logic.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Result of a successful OAuth callback.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

/// Errors that can occur while running the callback server.
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackError {
    BindError(String),
    ShutdownError(String),
    StateMismatch,
    MissingCode,
    UserDenied,
    Timeout,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindError(msg) => write!(f, "Failed to bind callback server: {msg}"),
            Self::ShutdownError(msg) => write!(f, "Server shutdown error: {msg}"),
            Self::StateMismatch => write!(f, "OAuth state mismatch (possible CSRF attack)"),
            Self::MissingCode => write!(f, "Authorization code missing in callback"),
            Self::UserDenied => write!(f, "Authorization denied by user"),
            Self::Timeout => write!(f, "Timed out waiting for OAuth callback"),
        }
    }
}

impl std::error::Error for CallbackError {}

/// A temporary OAuth callback server listening on a local port.
///
/// Use [`CallbackServer::start`] to bind to a free port, then
/// [`CallbackServer::wait`] to block until the browser redirects back.
pub struct CallbackServer {
    /// The local port the server is listening on.
    pub port: u16,
    rx: mpsc::Receiver<CallbackResult>,
    shutdown_tx: oneshot::Sender<()>,
}

impl CallbackServer {
    /// Bind a temporary HTTP server to a free localhost port.
    ///
    /// Returns immediately with the bound port; call [`Self::wait`] to block
    /// for the OAuth redirect.
    pub async fn start(expected_state: &str) -> Result<Self, CallbackError> {
        let (code_tx, code_rx) = mpsc::channel::<CallbackResult>(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let state = expected_state.to_string();
        let code_tx = Arc::new(tokio::sync::Mutex::new(Some(code_tx)));

        let app = axum::Router::new()
            .route("/callback", axum::routing::get(handle_callback))
            .route("/", axum::routing::get(handle_callback))
            .with_state(AppState {
                expected_state: state,
                code_tx,
            });

        // Bind to any free localhost port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| CallbackError::BindError(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| CallbackError::BindError(e.to_string()))?;
        let port = local_addr.port();

        tracing::info!("OAuth callback server listening on http://{local_addr}/callback");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Ok(Self {
            port,
            rx: code_rx,
            shutdown_tx,
        })
    }

    /// Block until the browser redirects back with an authorization code
    /// or the timeout expires.
    pub async fn wait(mut self, timeout: std::time::Duration) -> Result<CallbackResult, CallbackError> {
        let result = tokio::time::timeout(timeout, self.rx.recv()).await;

        // Signal shutdown regardless of result
        let _ = self.shutdown_tx.send(());

        match result {
            Ok(Some(res)) => Ok(res),
            Ok(None) => Err(CallbackError::ShutdownError(
                "Channel closed".into(),
            )),
            Err(_) => Err(CallbackError::Timeout),
        }
    }
}

/// Return the redirect URL to register with the OAuth provider.
///
/// Uses `127.0.0.1` so the provider doesn't need to resolve `localhost`.
pub fn redirect_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

// ---------------------------------------------------------------------------
// Internal axum handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    expected_state: String,
    code_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<CallbackResult>>>>,
}

async fn handle_callback(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Html<String> {
    // Check for error from provider (user denied, etc.)
    if let Some(error) = params.get("error") {
        let msg = params.get("error_description").map(String::as_str).unwrap_or(error);
        let html = error_page(msg);

        let mut tx = state.code_tx.lock().await;
        if let Some(sender) = tx.take() {
            let _ = sender.send(CallbackResult {
                code: format!("error:{error}"),
                state: params.get("state").cloned(),
            }).await;
        }
        return axum::response::Html(html);
    }

    // Verify state
    let received_state = params.get("state").cloned().unwrap_or_default();
    if received_state != state.expected_state {
        let html = error_page("Invalid state parameter. Possible CSRF attack.");
        let mut tx = state.code_tx.lock().await;
        if let Some(sender) = tx.take() {
            let _ = sender.send(CallbackResult {
                code: "error:state_mismatch".into(),
                state: Some(received_state),
            }).await;
        }
        return axum::response::Html(html);
    }

    // Extract code
    let Some(code) = params.get("code") else {
        let html = error_page("Authorization code not found in callback.");
        let mut tx = state.code_tx.lock().await;
        if let Some(sender) = tx.take() {
            let _ = sender.send(CallbackResult {
                code: "error:missing_code".into(),
                state: Some(received_state),
            }).await;
        }
        return axum::response::Html(html);
    };

    let html = success_page();

    let mut tx = state.code_tx.lock().await;
    if let Some(sender) = tx.take() {
        let _ = sender.send(CallbackResult {
            code: code.clone(),
            state: Some(received_state),
        }).await;
    }

    axum::response::Html(html)
}

fn success_page() -> String {
    r#"<!DOCTYPE html>
<html>
<head><title>Hermes Login</title><style>
body{font-family:sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#0d1117;color:#c9d1d9}
.container{text-align:center}
h1{color:#3fb950}
</style></head>
<body><div class="container">
<h1>✓ Authorization successful</h1>
<p>You can close this tab and return to the terminal.</p>
</div></body>
</html>
"#
    .to_string()
}

fn error_page(msg: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><title>Hermes Login</title><style>
body{{font-family:sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#0d1117;color:#c9d1d9}}
.container{{text-align:center}}
h1{{color:#f85149}}
</style></head>
<body><div class="container">
<h1>✗ Authorization failed</h1>
<p>{msg}</p>
<p>Please return to the terminal and try again.</p>
</div></body>
</html>
"#
    )
}

/// Generate a random OAuth state parameter for CSRF protection.
pub fn generate_state() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redirect_url_format() {
        let url = redirect_url(8765);
        assert_eq!(url, "http://127.0.0.1:8765/callback");
    }

    #[test]
    fn test_generate_state_not_empty() {
        let state = generate_state();
        assert!(!state.is_empty());
        assert_eq!(state.len(), 43); // 32 bytes base64url-no-pad
    }

    #[test]
    fn test_success_page_contains_ok() {
        let html = success_page();
        assert!(html.contains("successful"));
    }

    #[test]
    fn test_error_page_contains_message() {
        let html = error_page("Something went wrong");
        assert!(html.contains("Something went wrong"));
        assert!(html.contains("failed"));
    }
}
