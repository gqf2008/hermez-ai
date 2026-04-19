#![allow(dead_code)]
//! Generic webhook platform adapter.
//!
//! Runs an axum HTTP server that receives webhook POSTs from external
//! services (GitHub, GitLab, JIRA, Stripe, etc.), validates HMAC signatures,
//! transforms payloads into agent prompts, and routes responses back to the
//! source or to another configured platform.
//!
//! Mirrors Python `gateway/platforms/webhook.py`.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio::sync::oneshot;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, error, info, warn};

use crate::config::Platform;
use crate::runner::MessageHandler;
use crate::session::SessionStore;

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8644;
const INSECURE_NO_AUTH: &str = "INSECURE_NO_AUTH";
const DYNAMIC_ROUTES_FILENAME: &str = "webhook_subscriptions.json";
const DEFAULT_RATE_LIMIT: usize = 30; // per minute
const DEFAULT_MAX_BODY_BYTES: usize = 1_048_576; // 1MB
const IDEMPOTENCY_TTL_SECS: u64 = 3600; // 1 hour

/// Webhook platform configuration.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub host: String,
    pub port: u16,
    pub global_secret: String,
    pub rate_limit: usize,
    pub max_body_bytes: usize,
    pub static_routes: HashMap<String, WebhookRouteConfig>,
}

/// Configuration for a single webhook route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookRouteConfig {
    pub events: Vec<String>,
    pub secret: Option<String>,
    pub prompt: String,
    pub skills: Vec<String>,
    pub deliver: String,
    #[serde(default)]
    pub deliver_extra: HashMap<String, serde_json::Value>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        let mut static_routes = HashMap::new();
        // Load routes from config.yaml extra if present
        if let Ok(routes_json) = std::env::var("WEBHOOK_ROUTES") {
            if let Ok(routes) = serde_json::from_str::<HashMap<String, WebhookRouteConfig>>(&routes_json) {
                static_routes = routes;
            }
        }
        Self {
            host: std::env::var("WEBHOOK_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            port: std::env::var("WEBHOOK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            global_secret: std::env::var("WEBHOOK_SECRET").unwrap_or_default(),
            rate_limit: std::env::var("WEBHOOK_RATE_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_RATE_LIMIT),
            max_body_bytes: std::env::var("WEBHOOK_MAX_BODY_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MAX_BODY_BYTES),
            static_routes,
        }
    }
}

impl WebhookConfig {
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Inbound webhook event (after prompt rendering).
#[derive(Debug, Clone)]
pub struct WebhookMessageEvent {
    pub route_name: String,
    pub event_type: String,
    pub delivery_id: String,
    pub chat_id: String,
    pub prompt: String,
    pub payload: serde_json::Value,
    pub deliver: String,
    pub deliver_extra: HashMap<String, serde_json::Value>,
}

/// Delivery info stored per chat_id for response routing.
#[derive(Debug, Clone)]
struct DeliveryInfo {
    deliver: String,
    deliver_extra: HashMap<String, serde_json::Value>,
}

/// Cross-platform delivery trait for routing responses to other platforms.
#[async_trait::async_trait]
pub trait CrossPlatformDelivery: Send + Sync {
    async fn send_to_platform(&self, platform: Platform, chat_id: &str, content: &str) -> Result<(), String>;
}

/// Shared state for the webhook HTTP server.
struct WebhookState {
    config: WebhookConfig,
    routes: Arc<RwLock<HashMap<String, WebhookRouteConfig>>>,
    rate_counts: Arc<RwLock<HashMap<String, VecDeque<f64>>>>,
    seen_deliveries: Arc<RwLock<HashMap<String, f64>>>,
    delivery_info: Arc<RwLock<HashMap<String, DeliveryInfo>>>,
    delivery_info_created: Arc<RwLock<HashMap<String, f64>>>,
    handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
    session_store: Arc<SessionStore>,
    cross_platform: Arc<RwLock<Option<Arc<dyn CrossPlatformDelivery>>>>,
}

/// Generic webhook receiver that triggers agent runs from HTTP POSTs.
#[derive(Clone)]
pub struct WebhookAdapter {
    config: WebhookConfig,
    routes: Arc<RwLock<HashMap<String, WebhookRouteConfig>>>,
    dynamic_routes_mtime: Arc<RwLock<f64>>,
    rate_counts: Arc<RwLock<HashMap<String, VecDeque<f64>>>>,
    seen_deliveries: Arc<RwLock<HashMap<String, f64>>>,
    delivery_info: Arc<RwLock<HashMap<String, DeliveryInfo>>>,
    delivery_info_created: Arc<RwLock<HashMap<String, f64>>>,
    cross_platform: Arc<RwLock<Option<Arc<dyn CrossPlatformDelivery>>>>,
}

impl WebhookAdapter {
    pub fn new(config: WebhookConfig) -> Self {
        let routes = config.static_routes.clone();
        Self {
            config,
            routes: Arc::new(RwLock::new(routes)),
            dynamic_routes_mtime: Arc::new(RwLock::new(0.0)),
            rate_counts: Arc::new(RwLock::new(HashMap::new())),
            seen_deliveries: Arc::new(RwLock::new(HashMap::new())),
            delivery_info: Arc::new(RwLock::new(HashMap::new())),
            delivery_info_created: Arc::new(RwLock::new(HashMap::new())),
            cross_platform: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the cross-platform delivery handler.
    pub async fn set_cross_platform_delivery(&self, delivery: Arc<dyn CrossPlatformDelivery>) {
        *self.cross_platform.write().await = Some(delivery);
    }

    /// Validate routes at startup.
    pub async fn validate_routes(&self) -> Result<(), String> {
        let routes = self.routes.read().await;
        for (name, route) in routes.iter() {
            let secret = route.secret.as_ref().unwrap_or(&self.config.global_secret);
            if secret.is_empty() {
                return Err(format!(
                    "[webhook] Route '{}' has no HMAC secret. \
                     Set 'secret' on the route or globally. \
                     For testing without auth, set secret to '{}'.",
                    name, INSECURE_NO_AUTH
                ));
            }
        }
        Ok(())
    }

    /// Run the webhook HTTP server.
    pub async fn run(
        &self,
        handler: Arc<Mutex<Option<Arc<dyn MessageHandler>>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        running_sessions: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        busy_ack_ts: Arc<parking_lot::Mutex<HashMap<String, f64>>>,
        session_store: Arc<SessionStore>,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), String> {
        // Reload dynamic routes before starting
        self.reload_dynamic_routes().await;

        let state = WebhookState {
            config: self.config.clone(),
            routes: self.routes.clone(),
            rate_counts: self.rate_counts.clone(),
            seen_deliveries: self.seen_deliveries.clone(),
            delivery_info: self.delivery_info.clone(),
            delivery_info_created: self.delivery_info_created.clone(),
            handler,
            running,
            running_sessions,
            busy_ack_ts,
            session_store,
            cross_platform: self.cross_platform.clone(),
        };

        let app = Router::new()
            .route("/health", get(handle_health))
            .route("/webhooks/:route_name", post(handle_webhook))
            .layer(RequestBodyLimitLayer::new(self.config.max_body_bytes))
            .with_state(Arc::new(state));

        let listener = tokio::net::TcpListener::bind((self.config.host.as_str(), self.config.port))
            .await
            .map_err(|e| format!("Failed to bind webhook server on {}:{}: {}", self.config.host, self.config.port, e))?;

        let route_names: Vec<String> = {
            let routes = self.routes.read().await;
            routes.keys().cloned().collect()
        };
        info!(
            "[webhook] Listening on {}:{} — routes: {}",
            self.config.host,
            self.config.port,
            if route_names.is_empty() {
                "(none configured)".to_string()
            } else {
                route_names.join(", ")
            }
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("[webhook] Shutting down");
            })
            .await
            .map_err(|e| format!("Webhook server error: {e}"))
    }

    /// Send a response for the given chat_id using stored delivery info.
    pub async fn send(&self, chat_id: &str, content: &str) -> Result<(), String> {
        let delivery = {
            let info = self.delivery_info.read().await;
            info.get(chat_id).cloned()
        };

        let Some(delivery) = delivery else {
            info!("[webhook] Response for {}: {}", chat_id, &content[..content.len().min(200)]);
            return Ok(());
        };

        match delivery.deliver.as_str() {
            "log" => {
                info!("[webhook] Response for {}: {}", chat_id, &content[..content.len().min(200)]);
                Ok(())
            }
            "github_comment" => {
                self.deliver_github_comment(content, &delivery.deliver_extra).await
            }
            platform => {
                // Cross-platform delivery
                let cross = self.cross_platform.read().await;
                if let Some(sender) = cross.as_ref() {
                    let Some(platform_enum) = Platform::from_str(platform) else {
                        return Err(format!("Unknown deliver type: {}", platform));
                    };
                    let chat_id_override = delivery
                        .deliver_extra
                        .get("chat_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(chat_id);
                    sender.send_to_platform(platform_enum, chat_id_override, content).await
                } else {
                    warn!("[webhook] Cross-platform delivery not available for {}", platform);
                    Err(format!("Cross-platform delivery not available for {}", platform))
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Dynamic route reloading
    // ------------------------------------------------------------------

    async fn reload_dynamic_routes(&self) {
        let subs_path = hermes_core::get_hermes_home().join(DYNAMIC_ROUTES_FILENAME);
        if !subs_path.exists() {
            let has_dynamic = {
                let routes = self.routes.read().await;
                let static_keys: Vec<String> = self.config.static_routes.keys().cloned().collect();
                routes.keys().any(|k| !static_keys.contains(k))
            };
            if has_dynamic {
                let mut routes = self.routes.write().await;
                routes.retain(|k, _| self.config.static_routes.contains_key(k));
            }
            return;
        }

        let mtime = match subs_path.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            Err(_) => return,
        };

        {
            let last_mtime = *self.dynamic_routes_mtime.read().await;
            if mtime <= last_mtime {
                return;
            }
        }

        let content = match tokio::fs::read_to_string(&subs_path).await {
            Ok(c) => c,
            Err(e) => {
                error!("[webhook] Failed to read dynamic routes: {}", e);
                return;
            }
        };

        let data: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                error!("[webhook] Failed to parse dynamic routes: {}", e);
                return;
            }
        };

        let mut dynamic_routes = HashMap::new();
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                if self.config.static_routes.contains_key(k) {
                    continue; // Static routes take precedence
                }
                if let Ok(route) = serde_json::from_value::<WebhookRouteConfig>(v.clone()) {
                    dynamic_routes.insert(k.clone(), route);
                }
            }
        }

        {
            let mut routes = self.routes.write().await;
            // Remove old dynamic routes
            routes.retain(|k, _| self.config.static_routes.contains_key(k));
            // Add new dynamic routes
            for (k, v) in dynamic_routes {
                routes.insert(k, v);
            }
        }

        *self.dynamic_routes_mtime.write().await = mtime;
        info!(
            "[webhook] Reloaded dynamic route(s): {}",
            {
                let routes = self.routes.read().await;
                
                routes.len() - self.config.static_routes.len()
            }
        );
    }

    // ------------------------------------------------------------------
    // Response delivery
    // ------------------------------------------------------------------

    async fn deliver_github_comment(
        &self,
        content: &str,
        extra: &HashMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        let repo = extra
            .get("repo")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let pr_number = extra
            .get("pr_number")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if repo.is_empty() || pr_number.is_empty() {
            return Err("github_comment delivery missing repo or pr_number".to_string());
        }

        let output = tokio::process::Command::new("gh")
            .args([
                "pr", "comment", pr_number,
                "--repo", repo,
                "--body", content,
            ])
            .output()
            .await
            .map_err(|e| format!("Failed to run gh CLI: {}", e))?;

        if output.status.success() {
            info!("[webhook] Posted comment on {}#{}", repo, pr_number);
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("[webhook] gh pr comment failed: {}", stderr);
            Err(format!("gh pr comment failed: {}", stderr))
        }
    }
}

// ------------------------------------------------------------------
// HTTP handlers
// ------------------------------------------------------------------

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok", "platform": "webhook"}))
}

async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    Path(route_name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // Hot-reload dynamic subscriptions (mtime-gated, cheap)
    // Note: we can't easily call reload_dynamic_routes here since it's on adapter,
    // but the adapter reloads at startup. For runtime reload, we'd need a shared method.
    // Skipping per-request reload for simplicity; routes reload on restart.

    let routes = state.routes.read().await;
    let route_config = match routes.get(&route_name) {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Unknown route: {}", route_name)})),
            );
        }
    };
    drop(routes);

    // Auth-before-body: check size
    if body.len() > state.config.max_body_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "Payload too large"})),
        );
    }

    // Rate limiting
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    {
        let mut counts = state.rate_counts.write().await;
        let window = counts.entry(route_name.clone()).or_default();
        window.retain(|&t| now - t < 60.0);
        if window.len() >= state.config.rate_limit {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "Rate limit exceeded"})),
            );
        }
        window.push_back(now);
    }

    // Validate HMAC signature
    let secret = route_config.secret.as_ref().unwrap_or(&state.config.global_secret);
    if !secret.is_empty() && secret != INSECURE_NO_AUTH
        && !validate_signature(&headers, &body, secret) {
            warn!("[webhook] Invalid signature for route {}", route_name);
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }

    // Parse payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Cannot parse body"})),
            );
        }
    };

    // Check event type filter
    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("X-GitLab-Event").and_then(|v| v.to_str().ok()))
        .or_else(|| payload.get("event_type").and_then(|v| v.as_str()))
        .unwrap_or("unknown")
        .to_string();

    if !route_config.events.is_empty() && !route_config.events.contains(&"*".to_string())
        && !route_config.events.contains(&event_type) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "ignored",
                    "event": event_type
                })),
            );
        }

    // Render prompt
    let prompt = render_prompt(&route_config.prompt, &payload, &event_type, &route_name);

    // Build delivery ID
    let delivery_id = headers
        .get("X-GitHub-Delivery")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("X-Request-ID").and_then(|v| v.to_str().ok()))
        .unwrap_or(&format!("{}", now as u64 * 1000))
        .to_string();

    // Idempotency check
    {
        let mut seen = state.seen_deliveries.write().await;
        seen.retain(|_, &mut t| now - t < IDEMPOTENCY_TTL_SECS as f64);
        if seen.contains_key(&delivery_id) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "duplicate",
                    "delivery_id": delivery_id
                })),
            );
        }
        seen.insert(delivery_id.clone(), now);
    }

    let session_chat_id = format!("webhook:{}:{}", route_name, delivery_id);

    // Store delivery info
    let deliver_extra = render_delivery_extra(&route_config.deliver_extra, &payload);
    let delivery_info = DeliveryInfo {
        deliver: route_config.deliver.clone(),
        deliver_extra,
    };
    {
        let mut info = state.delivery_info.write().await;
        info.insert(session_chat_id.clone(), delivery_info);
    }
    {
        let mut created = state.delivery_info_created.write().await;
        created.insert(session_chat_id.clone(), now);
    }
    prune_delivery_info(&state.delivery_info, &state.delivery_info_created, now).await;

    info!(
        "[webhook] {} event={} route={} prompt_len={} delivery={}",
        "POST",
        event_type,
        route_name,
        prompt.len(),
        delivery_id,
    );

    // Process in background, return 202 immediately
    let handler = state.handler.clone();
    let running = state.running.clone();
    let running_sessions = state.running_sessions.clone();
    let busy_ack_ts = state.busy_ack_ts.clone();
    let session_store = state.session_store.clone();
    let chat_id = session_chat_id.clone();
    let deliver_type = route_config.deliver.clone();
    let deliver_extra = route_config.deliver_extra.clone();
    let cross_platform = state.cross_platform.clone();

    tokio::spawn(async move {
        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let handler_guard = handler.lock().await;
        let Some(handler_ref) = handler_guard.as_ref().cloned() else {
            warn!("No message handler registered for webhook messages");
            return;
        };
        drop(handler_guard);

        // Busy session handling
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let busy_elapsed_min: Option<f64> = {
            let sessions = running_sessions.lock();
            sessions.get(&chat_id).map(|&start_ts| {
                let elapsed_secs = now - start_ts;
                elapsed_secs / 60.0
            })
        };

        if let Some(elapsed_min) = busy_elapsed_min {
            let should_ack = {
                let mut ack_map = busy_ack_ts.lock();
                let last_ack = ack_map.get(&chat_id).copied().unwrap_or(0.0);
                if now - last_ack < 30.0 {
                    false
                } else {
                    ack_map.insert(chat_id.clone(), now);
                    true
                }
            };
            if should_ack {
                handler_ref.interrupt(&chat_id, &prompt);
                info!("Session {}: busy — agent interrupted after {:.1} min", chat_id, elapsed_min);
                // For webhook, busy ack goes to log or configured deliver
                let busy_msg = format!(
                    "Still processing your previous message ({:.0}m elapsed). \
                     Please wait for my response before sending another prompt.",
                    elapsed_min
                );
                let _ = deliver_response(&chat_id, &busy_msg, &deliver_type, &deliver_extra, &cross_platform).await;
            }
            return;
        }

        {
            let mut sessions = running_sessions.lock();
            sessions.insert(chat_id.clone(), now);
        }

        match handler_ref.handle_message(Platform::Webhook, &chat_id, &prompt, None).await {
            Ok(result) => {
                running_sessions.lock().remove(&chat_id);
                busy_ack_ts.lock().remove(&chat_id);

                if result.compression_exhausted {
                    let session_key = format!("webhook:{}", chat_id);
                    session_store.reset_session(&session_key);
                    let _ = deliver_response(
                        &chat_id,
                        "Session reset: conversation context grew too large. Starting fresh.",
                        &deliver_type,
                        &deliver_extra,
                        &cross_platform,
                    ).await;
                }
                if !result.response.is_empty() {
                    let _ = deliver_response(&chat_id, &result.response, &deliver_type, &deliver_extra, &cross_platform).await;
                }
            }
            Err(e) => {
                running_sessions.lock().remove(&chat_id);
                busy_ack_ts.lock().remove(&chat_id);
                error!("Agent handler failed for webhook message: {}", e);
                let _ = deliver_response(
                    &chat_id,
                    "Sorry, I encountered an error processing your message.",
                    &deliver_type,
                    &deliver_extra,
                    &cross_platform,
                ).await;
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "accepted",
            "route": route_name,
            "event": event_type,
            "delivery_id": delivery_id,
        })),
    )
}

/// Deliver a response for a webhook session.
async fn deliver_response(
    chat_id: &str,
    content: &str,
    deliver_type: &str,
    deliver_extra: &HashMap<String, serde_json::Value>,
    cross_platform: &Arc<RwLock<Option<Arc<dyn CrossPlatformDelivery>>>>,
) {
    match deliver_type {
        "log" | "" => {
            info!("[webhook] Response for {}: {}", chat_id, &content[..content.len().min(200)]);
        }
        "github_comment" => {
            let repo = deliver_extra.get("repo").and_then(|v| v.as_str()).unwrap_or("");
            let pr_number = deliver_extra.get("pr_number").and_then(|v| v.as_str()).unwrap_or("");
            if repo.is_empty() || pr_number.is_empty() {
                error!("[webhook] github_comment delivery missing repo or pr_number");
                return;
            }
            match tokio::process::Command::new("gh")
                .args(["pr", "comment", pr_number, "--repo", repo, "--body", content])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    info!("[webhook] Posted comment on {}#{}", repo, pr_number);
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    error!("[webhook] gh pr comment failed: {}", stderr);
                }
                Err(e) => {
                    error!("[webhook] Failed to run gh CLI: {}", e);
                }
            }
        }
        platform => {
            let cross = cross_platform.read().await;
            if let Some(sender) = cross.as_ref() {
                let Some(platform_enum) = Platform::from_str(platform) else {
                    warn!("[webhook] Unknown deliver type: {}", platform);
                    return;
                };
                let chat_id_override = deliver_extra
                    .get("chat_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(chat_id);
                if let Err(e) = sender.send_to_platform(platform_enum, chat_id_override, content).await {
                    error!("[webhook] Cross-platform delivery failed: {}", e);
                }
            } else {
                warn!("[webhook] Cross-platform delivery not available for {}", platform);
            }
        }
    }
}

// ------------------------------------------------------------------
// Signature validation
// ------------------------------------------------------------------

/// Constant-time string comparison to prevent timing side-channels.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= x ^ y;
    }
    result == 0
}

fn validate_signature(headers: &HeaderMap, body: &[u8], secret: &str) -> bool {
    // GitHub: X-Hub-Signature-256 = sha256=<hex>
    if let Some(gh_sig) = headers.get("X-Hub-Signature-256").and_then(|v| v.to_str().ok()) {
        let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(body);
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        return constant_time_eq(&expected, gh_sig);
    }

    // GitLab: X-Gitlab-Token = <plain secret>
    if let Some(gl_token) = headers.get("X-Gitlab-Token").and_then(|v| v.to_str().ok()) {
        return constant_time_eq(gl_token, secret);
    }

    // Generic: X-Webhook-Signature = <hex HMAC-SHA256>
    if let Some(generic_sig) = headers.get("X-Webhook-Signature").and_then(|v| v.to_str().ok()) {
        let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(body);
        let expected = hex::encode(mac.finalize().into_bytes());
        return constant_time_eq(&expected, generic_sig);
    }

    debug!("[webhook] Secret configured but no signature header found");
    false
}

// ------------------------------------------------------------------
// Prompt rendering
// ------------------------------------------------------------------

fn render_prompt(template: &str, payload: &serde_json::Value, event_type: &str, route_name: &str) -> String {
    if template.is_empty() {
        let truncated = serde_json::to_string_pretty(payload).unwrap_or_default();
        let truncated = if truncated.len() > 4000 {
            format!("{}...", &truncated[..4000])
        } else {
            truncated
        };
        return format!(
            "Webhook event '{}' on route '{}':\n\n```json\n{}\n```",
            event_type, route_name, truncated
        );
    }

    static TEMPLATE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TEMPLATE_RE.get_or_init(|| regex::Regex::new(r"\{([a-zA-Z0-9_\.]+)\}").unwrap());
    re.replace_all(template, |caps: &regex::Captures| {
        let key = &caps[1];
        if key == "__raw__" {
            let raw = serde_json::to_string_pretty(payload).unwrap_or_default();
            return if raw.len() > 4000 {
                format!("{}...", &raw[..4000])
            } else {
                raw
            };
        }
        let mut value: &serde_json::Value = payload;
        for part in key.split('.') {
            if let Some(v) = value.get(part) {
                value = v;
            } else {
                return format!("{{{}}}", key);
            }
        }
        match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => {
                let json = serde_json::to_string_pretty(value).unwrap_or_default();
                if json.len() > 2000 {
                    format!("{}...", &json[..2000])
                } else {
                    json
                }
            }
        }
    })
    .to_string()
}

fn render_delivery_extra(
    extra: &HashMap<String, serde_json::Value>,
    payload: &serde_json::Value,
) -> HashMap<String, serde_json::Value> {
    let mut rendered = HashMap::new();
    for (key, value) in extra {
        if let Some(s) = value.as_str() {
            rendered.insert(key.clone(), serde_json::Value::String(render_prompt(s, payload, "", "")));
        } else {
            rendered.insert(key.clone(), value.clone());
        }
    }
    rendered
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

async fn prune_delivery_info(
    delivery_info: &Arc<RwLock<HashMap<String, DeliveryInfo>>>,
    delivery_info_created: &Arc<RwLock<HashMap<String, f64>>>,
    now: f64,
) {
    let cutoff = now - IDEMPOTENCY_TTL_SECS as f64;
    let created = delivery_info_created.write().await;
    let stale: Vec<String> = created
        .iter()
        .filter(|(_, &t)| t < cutoff)
        .map(|(k, _)| k.clone())
        .collect();
    drop(created);

    let mut info = delivery_info.write().await;
    let mut created = delivery_info_created.write().await;
    for k in stale {
        info.remove(&k);
        created.remove(&k);
    }
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = WebhookConfig::default();
        assert_eq!(config.host, DEFAULT_HOST);
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.rate_limit, DEFAULT_RATE_LIMIT);
        assert_eq!(config.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn test_render_prompt_empty_template() {
        let payload = serde_json::json!({"action": "opened"});
        let result = render_prompt("", &payload, "push", "github");
        assert!(result.contains("push"));
        assert!(result.contains("github"));
        assert!(result.contains("opened"));
    }

    #[test]
    fn test_render_prompt_dot_notation() {
        let payload = serde_json::json!({
            "pull_request": {
                "title": "Fix bug",
                "number": 42
            }
        });
        let result = render_prompt("PR: {pull_request.title} (#{pull_request.number})", &payload, "", "");
        assert_eq!(result, "PR: Fix bug (#42)");
    }

    #[test]
    fn test_render_prompt_missing_key() {
        let payload = serde_json::json!({"foo": "bar"});
        let result = render_prompt("{missing.key}", &payload, "", "");
        assert_eq!(result, "{missing.key}");
    }

    #[test]
    fn test_render_prompt_raw_token() {
        let payload = serde_json::json!({"foo": "bar"});
        let result = render_prompt("{__raw__}", &payload, "", "");
        assert!(result.contains("\"foo\": \"bar\""));
    }

    #[test]
    fn test_render_delivery_extra() {
        let payload = serde_json::json!({"pr": {"number": "42"}});
        let mut extra = HashMap::new();
        extra.insert("pr_number".to_string(), serde_json::json!("{pr.number}"));
        extra.insert("static".to_string(), serde_json::json!("value"));

        let rendered = render_delivery_extra(&extra, &payload);
        assert_eq!(rendered.get("pr_number").unwrap().as_str().unwrap(), "42");
        assert_eq!(rendered.get("static").unwrap().as_str().unwrap(), "value");
    }

    #[test]
    fn test_github_signature_validation() {
        let secret = "mysecret";
        let body = b"test payload";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let mut headers = HeaderMap::new();
        headers.insert("X-Hub-Signature-256", expected.parse().unwrap());

        assert!(validate_signature(&headers, body, secret));
    }

    #[test]
    fn test_gitlab_token_validation() {
        let secret = "mytoken";
        let body = b"test payload";
        let mut headers = HeaderMap::new();
        headers.insert("X-Gitlab-Token", secret.parse().unwrap());

        assert!(validate_signature(&headers, body, secret));
    }

    #[test]
    fn test_generic_signature_validation() {
        let secret = "mysecret";
        let body = b"test payload";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let expected = hex::encode(mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert("X-Webhook-Signature", expected.parse().unwrap());

        assert!(validate_signature(&headers, body, secret));
    }

    #[test]
    fn test_signature_no_header_fails() {
        let secret = "mysecret";
        let body = b"test payload";
        let headers = HeaderMap::new();
        assert!(!validate_signature(&headers, body, secret));
    }

    #[test]
    fn test_insecure_no_auth_bypass() {
        // INSECURE_NO_AUTH should be handled at call site; signature validation still runs
        let mut headers = HeaderMap::new();
        headers.insert("X-Hub-Signature-256", "sha256=bad".parse().unwrap());
        assert!(!validate_signature(&headers, b"body", INSECURE_NO_AUTH));
    }

    #[test]
    fn test_rate_limiting_window() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let adapter = WebhookAdapter::new(WebhookConfig::default());
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();

            // Add rate counts
            {
                let mut counts = adapter.rate_counts.write().await;
                counts.insert("test".to_string(), VecDeque::from([now - 10.0, now - 5.0, now]));
            }

            // Should have 3 entries
            {
                let counts = adapter.rate_counts.read().await;
                assert_eq!(counts.get("test").unwrap().len(), 3);
            }
        });
    }

    #[test]
    fn test_idempotency_cache() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let adapter = WebhookAdapter::new(WebhookConfig::default());
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();

            {
                let mut seen = adapter.seen_deliveries.write().await;
                seen.insert("delivery_1".to_string(), now);
                seen.insert("delivery_2".to_string(), now - IDEMPOTENCY_TTL_SECS as f64 - 1.0);
            }

            // Prune old entries
            {
                let mut seen = adapter.seen_deliveries.write().await;
                seen.retain(|_, &mut t| now - t < IDEMPOTENCY_TTL_SECS as f64);
            }

            let seen = adapter.seen_deliveries.read().await;
            assert!(seen.contains_key("delivery_1"));
            assert!(!seen.contains_key("delivery_2"));
        });
    }
}
