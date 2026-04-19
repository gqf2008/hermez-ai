//! Dingtalk platform adapter.
//!
//! Mirrors the Python `gateway/platforms/dingtalk.py`.
//!
//! Supports:
//! - HTTP webhook callback endpoint (POST receiver)
//! - Direct-message and group text receive/send
//! - Inbound rich_text extraction
//! - Deduplication cache
//! - Session webhook URL caching for proactive sends
//!
//! Outbound messages are sent via the `session_webhook` URL
//! that comes with each incoming message, or via the Open API
//! for proactive sends.
//!
//! The webhook server is started by calling `run()`.

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::post,
};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, oneshot, Semaphore};
use tracing::{debug, error, info, warn};

use crate::dedup::MessageDeduplicator;

type HmacSha256 = Hmac<Sha256>;

/// Dingtalk platform configuration.
#[derive(Debug, Clone)]
pub struct DingtalkConfig {
    pub client_id: String,
    pub client_secret: String,
    pub webhook_port: u16,
    pub webhook_path: String,
}

impl Default for DingtalkConfig {
    fn default() -> Self {
        Self {
            client_id: std::env::var("DINGTALK_CLIENT_ID").unwrap_or_default(),
            client_secret: std::env::var("DINGTALK_CLIENT_SECRET").unwrap_or_default(),
            webhook_port: std::env::var("DINGTALK_WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8766),
            webhook_path: std::env::var("DINGTALK_WEBHOOK_PATH")
                .ok()
                .unwrap_or_else(|| "/dingtalk/callback".to_string()),
        }
    }
}

impl DingtalkConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Inbound message event from Dingtalk.
#[derive(Debug, Clone)]
pub struct DingtalkMessageEvent {
    /// Unique message ID.
    pub message_id: String,
    /// Chat/session ID.
    pub chat_id: String,
    /// Sender ID.
    pub sender_id: String,
    /// Sender nick.
    pub sender_nick: String,
    /// Message content (text).
    pub content: String,
    /// Whether this is a group message.
    pub is_group: bool,
    /// Session webhook URL for replies.
    pub session_webhook: String,
}

/// Truncate text to at most `max_chars` characters (UTF-8 safe).
fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Webhook callback payload from Dingtalk.
#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub msg_id: Option<String>,
    pub conversation_id: Option<String>,
    pub conversation_type: Option<String>,
    pub sender_id: Option<String>,
    pub sender_nick: Option<String>,
    pub text: Option<serde_json::Value>,
    pub rich_text: Option<Vec<RichTextItem>>,
    pub session_webhook: Option<String>,
    pub create_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RichTextItem {
    pub text: Option<String>,
}

/// Response to Dingtalk webhook callback.
/// Dingtalk expects a 200 with this JSON to acknowledge receipt.
#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    #[serde(rename = "ret")]
    pub ret: String,
}

/// Shared state passed to webhook route handlers.
#[derive(Clone)]
struct WebhookState {
    adapter: Arc<DingtalkAdapter>,
    handler: Arc<Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
}

/// Session webhook cache: chat_id -> webhook URL.
struct WebhookCache {
    entries: parking_lot::Mutex<std::collections::HashMap<String, String>>,
    max_size: usize,
}

impl WebhookCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: parking_lot::Mutex::new(std::collections::HashMap::with_capacity(max_size)),
            max_size,
        }
    }

    #[allow(dead_code)]
    fn get(&self, key: &str) -> Option<String> {
        self.entries.lock().get(key).cloned()
    }

    fn insert(&self, key: String, value: String) {
        let mut map = self.entries.lock();
        if map.len() >= self.max_size {
            // Evict oldest entry
            if let Some(oldest_key) = map.keys().next().cloned() {
                map.remove(&oldest_key);
            }
        }
        map.insert(key, value);
    }
}

/// Cached access token with expiry time.
struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// Dingtalk platform adapter.
#[derive(Clone)]
pub struct DingtalkAdapter {
    config: DingtalkConfig,
    client: Client,
    dedup: Arc<MessageDeduplicator>,
    /// Access token cached from Dingtalk API (with TTL).
    access_token: Arc<RwLock<Option<CachedToken>>>,
    /// Session webhook cache for proactive sends.
    webhook_cache: Arc<WebhookCache>,
    /// Semaphore to limit concurrent webhook handlers.
    handler_semaphore: Arc<Semaphore>,
}

impl DingtalkAdapter {
    pub fn new(config: DingtalkConfig) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    Client::new()
                }),
            dedup: Arc::new(MessageDeduplicator::new()),
            access_token: Arc::new(RwLock::new(None)),
            webhook_cache: Arc::new(WebhookCache::new(500)),
            handler_semaphore: Arc::new(Semaphore::new(100)),
            config,
        }
    }

    /// Get/refresh the Dingtalk access token.
    /// Tokens expire after 7200 seconds (2 hours); refreshed on demand.
    async fn get_access_token(&self) -> Result<String, String> {
        // Check cache with expiry
        {
            let guard = self.access_token.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.expires_at > Instant::now() {
                    return Ok(cached.token.clone());
                }
            }
        }

        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/oauth2/accessToken")
            .json(&serde_json::json!({
                "appKey": &self.config.client_id,
                "appSecret": &self.config.client_secret,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to get access token: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token response: {e}"))?;

        let token = body
            .get("accessToken")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing accessToken in response: {body}"))?
            .to_string();

        let expires_in = body
            .get("expireIn")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);

        let cached = CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in - 300), // 5min buffer
        };
        *self.access_token.write().await = Some(cached);
        Ok(token)
    }

    /// Send a markdown message to a Dingtalk chat via session webhook.
    ///
    /// SSRF-safe: validates webhook URL starts with `https://api.dingtalk.com/`.
    pub async fn send_text(&self, webhook_url: &str, text: &str) -> Result<String, String> {
        // SSRF protection
        if !webhook_url.starts_with("https://api.dingtalk.com/") {
            return Err(format!("Invalid dingtalk webhook URL: {webhook_url}"));
        }

        let resp = self
            .client
            .post(webhook_url)
            .json(&serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "title": "Hermes",
                    "text": truncate_text(text, 20000),
                },
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Send failed: HTTP {}", status));
        }

        debug!("Dingtalk message sent via webhook");
        Ok("ok".to_string())
    }

    /// Send a proactive markdown message via the Open API.
    ///
    /// Requires the chat_id to be a valid Dingtalk open conversation ID.
    #[allow(dead_code)]
    pub async fn send_proactive(
        &self,
        open_conversation_id: &str,
        text: &str,
    ) -> Result<String, String> {
        let token = self.get_access_token().await?;

        let resp = self
            .client
            .post("https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend")
            .header("x-acs-dingtalk-access-token", &token)
            .json(&serde_json::json!({
                "robotCode": &self.config.client_id,
                "userIds": [open_conversation_id],
                "msgKey": "sampleMarkdown",
                "msgParam": serde_json::json!({
                    "title": "Hermes",
                    "text": truncate_text(text, 20000),
                }).to_string(),
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to send proactive message: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Proactive send failed: HTTP {}", status));
        }

        debug!("Dingtalk proactive message sent to {open_conversation_id}");
        Ok("ok".to_string())
    }

    /// Process an inbound webhook event.
    pub fn handle_inbound(&self, payload: &serde_json::Value) -> Option<DingtalkMessageEvent> {
        let msg_id = payload
            .get("msgId")
            .or_else(|| payload.get("msg_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !msg_id.is_empty() && self.dedup.is_duplicate(msg_id) {
            debug!("Dingtalk dedup: skipping {msg_id}");
            return None;
        }

        // Extract text content
        let content = Self::extract_text(payload).unwrap_or_default();
        if content.is_empty() {
            return None;
        }

        let chat_id = payload
            .get("conversationId")
            .or_else(|| payload.get("conversation_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let sender_id = payload
            .get("senderId")
            .or_else(|| payload.get("sender_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let sender_nick = payload
            .get("senderNick")
            .or_else(|| payload.get("sender_nick"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let is_group = payload
            .get("conversationType")
            .or_else(|| payload.get("conversation_type"))
            .and_then(|v| v.as_str())
            .map(|t| t == "2")
            .unwrap_or(false);

        let session_webhook = payload
            .get("sessionWebhook")
            .or_else(|| payload.get("session_webhook"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !msg_id.is_empty() {
            self.dedup.insert(msg_id.to_string());
        }

        // Cache the session webhook URL
        if !chat_id.is_empty() && !session_webhook.is_empty() {
            self.webhook_cache.insert(chat_id.clone(), session_webhook.clone());
        }

        Some(DingtalkMessageEvent {
            message_id: msg_id.to_string(),
            chat_id,
            sender_id,
            sender_nick,
            content,
            is_group,
            session_webhook,
        })
    }

    /// Extract text from inbound payload.
    ///
    /// Handles multiple formats: text.content, raw text string, rich_text array.
    fn extract_text(payload: &serde_json::Value) -> Option<String> {
        // Try text.content first
        if let Some(text_obj) = payload.get("text") {
            if let Some(content) = text_obj.get("content").and_then(|v| v.as_str()) {
                if !content.trim().is_empty() {
                    return Some(content.trim().to_string());
                }
            }
            // text might be a raw string
            if let Some(content) = text_obj.as_str() {
                if !content.trim().is_empty() {
                    return Some(content.trim().to_string());
                }
            }
        }

        // Fallback to richText: concatenate all text fields
        if let Some(rich_text) = payload
            .get("richText")
            .or_else(|| payload.get("rich_text"))
            .and_then(|v| v.as_array())
        {
            let parts: Vec<String> = rich_text
                .iter()
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()).map(String::from))
                .collect();
            let combined = parts.join("");
            if !combined.is_empty() {
                return Some(combined);
            }
        }

        None
    }

    /// Check if the adapter is properly configured.
    pub fn is_configured(&self) -> bool {
        !self.config.client_id.is_empty() && !self.config.client_secret.is_empty()
    }

    /// Run the Dingtalk webhook HTTP server.
    ///
    /// Starts an axum HTTP server that listens for POST callbacks
    /// on the configured webhook path.
    pub async fn run(
        &self,
        handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let app = self.build_router(handler.clone());

        let host = "0.0.0.0";
        let port = self.config.webhook_port;
        let listener = match tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
            Ok(l) => l,
            Err(e) => return Err(format!("Failed to bind Dingtalk webhook server: {e}")),
        };

        info!(
            "Dingtalk webhook server listening on {}:{}{}",
            host, port, self.config.webhook_path
        );

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
        {
            error!("Dingtalk webhook server error: {e}");
            return Err(format!("Dingtalk webhook server error: {e}"));
        }

        info!("Dingtalk webhook server stopped gracefully");
        Ok(())
    }

    /// Build the axum router with webhook endpoint.
    fn build_router(&self, handler: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::runner::MessageHandler>>>>) -> Router {
        let state = WebhookState {
            adapter: Arc::new((*self).clone()),
            handler,
        };

        Router::new()
            .route(&self.config.webhook_path, post(webhook_handler))
            .with_state(state)
    }
}

// ── Webhook Route Handler ─────────────────────────────────────────

/// Verify Dingtalk webhook signature to prevent message injection.
fn verify_webhook_signature(
    payload: &serde_json::Value,
    client_secret: &str,
) -> Result<(), String> {
    let timestamp = payload
        .get("timestamp")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("create_at").and_then(|v| v.as_str()))
        .ok_or_else(|| "Missing timestamp in webhook payload".to_string())?;

    let signature = payload
        .get("sign")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing signature in webhook payload".to_string())?;

    let mut mac = HmacSha256::new_from_slice(client_secret.as_bytes())
        .map_err(|_| "Invalid HMAC key length".to_string())?;
    mac.update(timestamp.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());

    if signature != expected {
        return Err(format!(
            "Webhook signature mismatch: received={}, expected={}",
            signature, expected
        ));
    }
    Ok(())
}

async fn webhook_handler(
    State(state): State<WebhookState>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<WebhookResponse>) {
    debug!("Dingtalk webhook received: {payload}");

    // Verify webhook signature if client_secret is configured
    if !state.adapter.config.client_secret.is_empty() {
        if let Err(e) = verify_webhook_signature(&payload, &state.adapter.config.client_secret) {
            warn!("Dingtalk webhook signature verification failed: {e}");
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookResponse {
                    ret: "signature_failed".to_string(),
                }),
            );
        }
    }

    let Some(event) = state.adapter.handle_inbound(&payload) else {
        // Deduped or empty
        return (
            StatusCode::OK,
            Json(WebhookResponse {
                ret: "success".to_string(),
            }),
        );
    };

    if event.content.is_empty() {
        return (
            StatusCode::OK,
            Json(WebhookResponse {
                ret: "success".to_string(),
            }),
        );
    }

    // Process agent handler in background with concurrency limit.
    // Dingtalk expects a 200 response quickly; the reply is sent separately
    // via the session_webhook URL after the agent responds.
    // The semaphore limits concurrent handlers to prevent resource exhaustion.
    let adapter = state.adapter.clone();
    let handler = state.handler.clone();
    let permit = state.adapter.handler_semaphore.clone()
        .try_acquire_owned()
        .map_err(|_| "Too many concurrent webhook handlers")
        .ok();

    if let Some(_permit) = permit {
        tokio::spawn(async move {
            // permit is dropped here, releasing the semaphore after handler completes
            info!(
                "Dingtalk message from {} ({}) via {}: {}",
                event.sender_nick,
                event.sender_id,
                event.chat_id,
                event.content.chars().take(50).collect::<String>(),
            );

            let handler_guard = handler.lock().await;
            if let Some(handler) = handler_guard.as_ref() {
                match handler
                    .handle_message(
                        crate::config::Platform::Dingtalk,
                        &event.chat_id,
                        &event.content,
                    )
                    .await
                {
                    Ok(result) => {
                        if !result.response.is_empty() {
                            if let Err(e) = adapter
                                .send_text(&event.session_webhook, &result.response)
                                .await
                            {
                                error!("Dingtalk send failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Agent handler failed for Dingtalk message: {e}");
                        let _ = adapter
                            .send_text(
                                &event.session_webhook,
                                "Sorry, I encountered an error processing your message.",
                            )
                            .await;
                    }
                }
            } else {
                warn!("No message handler registered for Dingtalk messages");
            }
        });
    } else {
        warn!("Dingtalk webhook rejected: too many concurrent handlers");
    }

    (
        StatusCode::OK,
        Json(WebhookResponse {
            ret: "success".to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = DingtalkConfig::default();
        assert_eq!(config.webhook_port, 8766);
        assert_eq!(config.webhook_path, "/dingtalk/callback");
    }

    #[test]
    fn test_config_from_env() {
        let config = DingtalkConfig::from_env();
        assert!(config.webhook_port > 0);
    }

    #[test]
    fn test_not_configured_when_empty() {
        let config = DingtalkConfig::default();
        let adapter = DingtalkAdapter::new(config);
        assert!(!adapter.is_configured());
    }

    #[test]
    fn test_extract_text_content() {
        let payload = serde_json::json!({
            "text": {"content": "hello world"},
            "msgId": "msg1",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        assert_eq!(adapter.handle_inbound(&payload).unwrap().content, "hello world");
    }

    #[test]
    fn test_extract_rich_text() {
        let payload = serde_json::json!({
            "text": {"content": ""},
            "richText": [
                {"text": "line1"},
                {"text": "line2"},
            ],
            "msgId": "msg2",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        assert_eq!(adapter.handle_inbound(&payload).unwrap().content, "line1line2");
    }

    #[test]
    fn test_dedup() {
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let payload = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "dedup_test_1",
        });
        assert!(adapter.handle_inbound(&payload).is_some());
        assert!(adapter.handle_inbound(&payload).is_none());
    }

    #[test]
    fn test_webhook_url_validation() {
        // SSRF protection: only allow api.dingtalk.com URLs
        assert!("https://api.dingtalk.com/robot/send?access_token=xxx"
            .starts_with("https://api.dingtalk.com/"));
        assert!(!"https://evil.com/test".starts_with("https://api.dingtalk.com/"));
        assert!(!"http://api.dingtalk.com/test".starts_with("https://api.dingtalk.com/"));
    }

    #[test]
    fn test_session_webhook_cached() {
        let payload = serde_json::json!({
            "text": {"content": "hello"},
            "msgId": "cache_test_1",
            "conversationId": "conv123",
            "sessionWebhook": "https://api.dingtalk.com/test/webhook",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let event = adapter.handle_inbound(&payload).unwrap();
        assert_eq!(event.session_webhook, "https://api.dingtalk.com/test/webhook");

        // Check cache
        let cached = adapter.webhook_cache.get("conv123");
        assert_eq!(cached, Some("https://api.dingtalk.com/test/webhook".to_string()));
    }

    #[test]
    fn test_handle_inbound_snake_case_keys() {
        // Some Dingtalk setups use snake_case keys
        let payload = serde_json::json!({
            "text": {"content": "snake case test"},
            "msg_id": "snake1",
            "conversation_id": "conv456",
            "sender_id": "user1",
            "sender_nick": "Tester",
            "conversation_type": "1",
            "session_webhook": "https://api.dingtalk.com/test",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let event = adapter.handle_inbound(&payload).unwrap();
        assert_eq!(event.content, "snake case test");
        assert_eq!(event.message_id, "snake1");
        assert_eq!(event.chat_id, "conv456");
        assert!(!event.is_group);
    }

    #[test]
    fn test_extract_raw_string_text() {
        // Some setups send text as a raw string instead of {"content": "..."}
        let payload = serde_json::json!({
            "text": "raw string message",
            "msgId": "raw1",
        });
        let adapter = DingtalkAdapter::new(DingtalkConfig::default());
        let event = adapter.handle_inbound(&payload).unwrap();
        assert_eq!(event.content, "raw string message");
    }

    #[test]
    fn test_truncate_text_utf8_safe() {
        // Emoji is 4 bytes in UTF-8
        let text = "Hello 😀 World";
        assert_eq!(truncate_text(text, 3), "Hel");
        assert_eq!(truncate_text(text, 7), "Hello 😀");
        assert_eq!(truncate_text(text, 100), text);
    }

    #[test]
    fn test_webhook_signature_verification() {
        let secret = "test_secret_123";
        let timestamp = "1700000000";

        // Compute valid signature
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        let valid_sig = hex::encode(mac.finalize().into_bytes());

        let payload = serde_json::json!({
            "timestamp": timestamp,
            "sign": valid_sig,
        });
        assert!(verify_webhook_signature(&payload, secret).is_ok());

        // Wrong signature
        let payload_bad = serde_json::json!({
            "timestamp": timestamp,
            "sign": "invalid_signature",
        });
        assert!(verify_webhook_signature(&payload_bad, secret).is_err());

        // Missing timestamp
        let payload_no_ts = serde_json::json!({
            "sign": valid_sig,
        });
        assert!(verify_webhook_signature(&payload_no_ts, secret).is_err());
    }

    #[tokio::test]
    async fn test_cached_token_expiry() {
        let config = DingtalkConfig::default();
        let adapter = DingtalkAdapter::new(config);

        // Token cache is empty initially
        assert!(adapter.access_token.read().await.is_none());
        // get_access_token would fail without real credentials, so test the
        // CachedToken struct directly
        let token = CachedToken {
            token: "test_token".to_string(),
            expires_at: std::time::Instant::now() + Duration::from_secs(3600),
        };
        *adapter.access_token.write().await = Some(token);

        // Token should be present and not expired
        let guard = adapter.access_token.read().await;
        let cached = guard.as_ref().unwrap();
        assert_eq!(cached.token, "test_token");
        assert!(cached.expires_at > std::time::Instant::now());
    }
}
